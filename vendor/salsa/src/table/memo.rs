//! Memos as columns (docs/v2/plan/lanes/db/speed-salsa.md, "Memos as columns").
//!
//! Every salsa struct ingredient has one [`MemoTableTypes`], shared by all its slots, and in it one
//! [`Column`] per tracked function keyed on the struct. A slot's [`MemoTable`] is only its row
//! number. A column holds, for each row, a pointer to the row's current memo, and the memos
//! themselves in a slab of the memo type. Both are blaze-mem regions (docs/v2/plan/design/
//! memory.md): address space reserved when the column is made, committed as it fills, never
//! moved, so a row's pointer is at `8 * row` and a slot at `size * slot`. The slab fills from a
//! bump counter and from the slots freed at the last revision boundary.
//!
//! So a memo costs its own size in the slab (no heap allocation and no allocator rounding of its
//! own) and an 8-byte row pointer in its function's column; a function with no memo for a range
//! of rows allocates nothing for it. Before, a slot held a 16-byte table pointing at an array of
//! one 8-byte pointer per function keyed on the struct, allocated for all of them once one memo
//! existed, and each memo was its own `Box`.
//!
//! Lifetimes are as before. A memo a reader found stays valid for the revision: a replaced or
//! removed memo is *retired*, not dropped, and retired memos are dropped, and their slots made
//! free, only by [`MemoTableTypes::reclaim`], which runs with `&mut Zalsa` at a new revision when
//! no memo can be borrowed. The free list is only written there, so taking a free slot during a
//! revision is one `fetch_add` on an index into a list nobody else changes: no ABA, no lock.
//!
//! Each slab slot is, at every point, in exactly one of four states: the current memo of one
//! row, retired, free (dropped, waiting for reuse), or being written by the task that allocated
//! it. Debug builds check the count of each against the slab's length at every reclaim and drop.

use std::alloc::Layout;
use std::any::{Any, TypeId};
use std::cell::UnsafeCell;
use std::fmt::Debug;
use std::mem;
use std::ptr::{self, NonNull};

use crate::DatabaseKeyIndex;
use crate::function::ErasedMemo;
use blaze_mem::{Region, Tag};

use crate::sync::Mutex;
use crate::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use crate::zalsa::MemoIngredientIndex;
use crate::zalsa::Zalsa;

/// Adds the registered concrete memo type's vtable without dereferencing the pointer.
///
/// Dereferencing the result requires a live, aligned allocation of the same concrete type.
// dyn: one vtable per memo column, used when a slot is cleared, at reclaim and in reports, never on a hit
pub(crate) type ToDynMemo = fn(NonNull<DummyMemo>) -> NonNull<dyn Memo>;

/// The "memo table" of a salsa struct slot: its row in each of its ingredient's memo columns.
/// Every tracked function must take a salsa struct as its first argument and memos are attached
/// to those salsa structs as auxiliary data.
pub struct MemoTable {
    row: u32,
}

#[cfg(not(feature = "shuttle"))]
const _: [(); mem::size_of::<MemoTable>()] = [(); 4];

impl MemoTable {
    /// Create a `MemoTable` with a fresh row in `types`' columns.
    ///
    /// # Safety
    ///
    /// The created memo table must only be accessed with the same `MemoTableTypes`.
    pub unsafe fn new(types: &MemoTableTypes) -> Self {
        Self {
            row: types.next_row(),
        }
    }

    /// Reset any memos in the table.
    ///
    /// The memos must have been taken (`take_memos`) or dropped (`MemoTableWithTypesMut::drop`)
    /// before calling this function; that empties the slot's rows, so the slot keeps its row
    /// numbers for whatever it holds next and there is nothing left to do here.
    pub fn reset(&mut self) {}
}

pub trait Memo: Any + Send + Sync {
    fn has_value(&self) -> bool;

    /// Removes the outputs that were created when this query ran. This includes
    /// tracked structs and specified queries.
    fn remove_outputs(&self, zalsa: &Zalsa, executor: DatabaseKeyIndex);

    /// Returns memory usage information about the memoized value.
    #[cfg(feature = "salsa_unstable")]
    fn memory_usage(&self) -> crate::database::MemoInfo;
}

/// Type metadata for one memo column.
///
/// All fields describe the column's concrete memo type, and the column stores only that type.
#[derive(Clone, Copy, Debug)]
pub struct MemoEntryType {
    /// The `type_id` of the erased memo type `M`
    type_id: TypeId,

    /// A type-coercion function for the erased memo type `M`.
    to_dyn_fn: ToDynMemo,

    /// The layout of `M`, the stride of the column's slab.
    layout: Layout,

    /// `M`'s name, for the column's errors.
    type_name: &'static str,
}

impl MemoEntryType {
    #[inline]
    pub fn of<M: Memo>() -> Self {
        Self {
            type_id: TypeId::of::<M>(),
            to_dyn_fn: Self::to_dyn_fn::<M>(),
            layout: Layout::new::<M>(),
            type_name: std::any::type_name::<M>(),
        }
    }

    pub(crate) const fn to_dyn_fn<M: Memo>() -> ToDynMemo {
        // dyn: one vtable per memo column, used when a slot is cleared, at reclaim and in reports, never on a hit
        fn to_dyn<M: Memo>(memo: NonNull<DummyMemo>) -> NonNull<dyn Memo> {
            let memo: NonNull<M> = memo.cast();
            memo
        }

        to_dyn::<M>
    }
}

/// Pointee marker for erased memo pointers; never instantiated or dereferenced.
#[derive(Debug)]
pub(crate) struct DummyMemo;

impl Memo for DummyMemo {
    fn has_value(&self) -> bool {
        unreachable!("DummyMemo is never stored in a memo table")
    }

    fn remove_outputs(&self, _zalsa: &Zalsa, _executor: DatabaseKeyIndex) {}

    #[cfg(feature = "salsa_unstable")]
    fn memory_usage(&self) -> crate::database::MemoInfo {
        crate::database::MemoInfo {
            debug_name: "dummy",
            output: crate::database::SlotInfo {
                debug_name: "dummy",
                size_of_metadata: 0,
                size_of_fields: 0,
                heap_size_of_fields: None,
                // alloc: the edge: memory-usage reports, run with `&mut` outside builds
                memos: Vec::new(),
            },
        }
    }
}

impl Drop for DummyMemo {
    fn drop(&mut self) {
        unreachable!("should never get here")
    }
}

/// Rows a struct ingredient can hand out. Each column reserves an 8-byte row pointer for every
/// one: 2 GB of address space, committed only as far as rows hold memos.
const MAX_ROWS: usize = 1 << 28;

/// Memos one column can hold at once, current, retired or free.
const MAX_MEMOS: usize = 1 << 24;

/// The most address space one column's slab reserves, for very large memo types.
const MAX_SLAB_BYTES: usize = 1 << 34;

const ROW_BYTES: usize = mem::size_of::<RowPtr>();

/// A row's pointer to its current memo. A plain atomic, not `crate::sync`'s: rows live in
/// kernel-zeroed pages of a [`Region`], and all-zero bits are a null `AtomicPtr`, which
/// shuttle's atomics (objects of their own) are not.
type RowPtr = std::sync::atomic::AtomicPtr<DummyMemo>;

/// A memo pointer that the column owns: the current memo of a row, a retired one, or a free slot.
#[derive(Clone, Copy, PartialEq, Eq)]
struct MemoPtr(NonNull<DummyMemo>);

// SAFETY: A `MemoPtr` points into a column's slab, at a memo (which is `Send + Sync`, a bound of
// `Memo`) or at uninitialized memory; moving or sharing the pointer between threads gives no
// access of its own, every access goes through the column's protocol.
unsafe impl Send for MemoPtr {}
// SAFETY: As above.
unsafe impl Sync for MemoPtr {}

/// One tracked function's memos for every row of a struct ingredient, in two reserved regions
/// of blaze-mem (docs/v2/plan/design/memory.md): addresses fixed when the database is made,
/// pages committed as they fill, never moved, so a row's pointer and a memo's place are
/// computed from their numbers with no directory to load.
///
/// `repr(C)` and line-aligned so a hit's row region (its base and committed length) and the
/// type check share the column's first cache line (a `const` assertion below).
#[repr(C, align(64))]
struct Column {
    /// Row `r`'s pointer at `r * 8`, null until a memo is stored; zeroed by the kernel when
    /// committed, and committed (`Region::ensure`) by the first `insert` at or past it.
    rows: Region,

    /// `type_.type_id`, beside `rows` for the hit's type check.
    type_id: TypeId,

    type_: MemoEntryType,

    /// The memos: slot `i` at `i * type_.layout.size()`.
    slab: Region,

    /// Slab slots ever handed out by the bump counter.
    slab_len: AtomicUsize,

    /// Slab slots the reservation holds.
    slab_max: usize,

    /// Free slots, written only by `reclaim` (exclusive) and read by `alloc` while shared.
    free: UnsafeCell<Vec<MemoPtr>>,

    /// How many of `free` have been taken this revision; may run past its length.
    free_taken: AtomicUsize,

    /// Memos replaced or removed this revision, dropped at the next `reclaim`.
    retired: Mutex<Vec<MemoPtr>>,
}

#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(mem::offset_of!(Column, rows) == 0);
    assert!(mem::offset_of!(Column, type_id) + mem::size_of::<TypeId>() <= 64);
};

// SAFETY: `free` is only written by `reclaim` and `Drop`, which have exclusive access to the
// column (their contracts), and only read through `&self` otherwise; the regions are `Sync`
// and their contents atomic or owned by the protocol above; everything else is atomic, locked,
// or immutable after construction, and the memos are `Send + Sync`.
unsafe impl Sync for Column {}
// SAFETY: The column owns its memos, which are `Send`, and its regions.
unsafe impl Send for Column {}

impl Column {
    fn new(type_: MemoEntryType) -> Column {
        let size = type_.layout.size();
        assert!(size > 0, "salsa: memos are never zero-sized");
        debug_assert_eq!(size % type_.layout.align(), 0);
        assert!(
            type_.layout.align() <= blaze_mem::page_size(),
            "salsa: the memo type `{}` is aligned past a page",
            type_.type_name
        );
        let slab_max = MAX_MEMOS.min(MAX_SLAB_BYTES / size);
        let reserve = |bytes: usize, what: &str| {
            Region::reserve(bytes, Tag::Query).unwrap_or_else(|err| {
                panic!("salsa: reserving the {what} of `{}`'s memo column: {err}", type_.type_name)
            })
        };
        Column {
            rows: reserve(MAX_ROWS * ROW_BYTES, "rows"),
            type_id: type_.type_id,
            type_,
            slab: reserve(slab_max * size, "slab"),
            slab_len: AtomicUsize::new(0),
            slab_max,
            // alloc: `Vec::new` allocates nothing; a column's free and retired lists grow once and are reused every revision
            free: UnsafeCell::new(Vec::new()),
            free_taken: AtomicUsize::new(0),
            // alloc: `Vec::new` allocates nothing; a column's free and retired lists grow once and are reused every revision
            retired: Mutex::new(Vec::new()),
        }
    }

    /// The row's pointer, if its page is committed.
    #[inline]
    fn row(&self, row: u32) -> Option<&RowPtr> {
        let at = row as usize * ROW_BYTES;
        if at + ROW_BYTES > self.rows.committed() {
            return None;
        }
        // SAFETY: `[at, at + 8)` is committed, so readable and writable for the region's life,
        // which is the column's; it holds zero or a pointer `insert` stored, both valid
        // `AtomicPtr`s; and it is 8-aligned, the base being page-aligned.
        Some(unsafe { self.rows.at(at).cast::<RowPtr>().as_ref() })
    }

    /// The row's pointer, committing its page.
    #[inline]
    fn row_or_init(&self, row: u32) -> &RowPtr {
        debug_assert!((row as usize) < MAX_ROWS);
        let end = (row as usize + 1) * ROW_BYTES;
        self.rows.ensure(end).unwrap_or_else(|err| {
            panic!("salsa: committing a row of `{}`'s memo column: {err}", self.type_.type_name)
        });
        self.row(row).expect("salsa: a row is committed once `ensure` returns")
    }

    /// A slab slot for one memo, uninitialized and owned by the caller until it is published.
    fn alloc(&self) -> NonNull<DummyMemo> {
        let taken = self.free_taken.fetch_add(1, Ordering::Relaxed);
        // SAFETY: `free` is written only by `reclaim` and `Drop`, with exclusive access, so it is
        // not written while we read it. Each index of it is handed out once, by the `fetch_add`.
        if let Some(&MemoPtr(slot)) = unsafe { &*self.free.get() }.get(taken) {
            return slot;
        }
        let index = self.slab_len.fetch_add(1, Ordering::Relaxed);
        if index >= self.slab_max {
            self.slab_len.fetch_sub(1, Ordering::Relaxed);
            panic!(
                "salsa: `{}`'s memo column holds more than {} memos (current, retired and free)",
                self.type_.type_name, self.slab_max
            );
        }
        let size = self.type_.layout.size();
        self.slab.ensure((index + 1) * size).unwrap_or_else(|err| {
            panic!("salsa: committing a slot of `{}`'s memo column: {err}", self.type_.type_name)
        });
        let slot = self.slab.at(index * size).cast::<DummyMemo>();
        debug_assert!(self.owns(slot));
        debug_assert_eq!(slot.as_ptr() as usize % self.type_.layout.align(), 0);
        slot
    }

    /// Whether `memo` is the start of a slot this column's slab has handed out.
    fn owns(&self, memo: NonNull<DummyMemo>) -> bool {
        let start = self.slab.base().as_ptr() as usize;
        let size = self.type_.layout.size();
        let Some(offset) = (memo.as_ptr() as usize).checked_sub(start) else {
            return false;
        };
        offset % size == 0 && offset / size < self.slab_len.load(Ordering::Relaxed)
    }

    /// Takes the row's memo out and retires it, returning it: it stays valid until `reclaim`.
    fn retire_row(&self, row: u32) -> Option<NonNull<DummyMemo>> {
        let memo = NonNull::new(self.row(row)?.swap(ptr::null_mut(), Ordering::AcqRel))?;
        self.retire(memo);
        Some(memo)
    }

    fn retire(&self, memo: NonNull<DummyMemo>) {
        debug_assert!(self.owns(memo), "salsa: a retired memo lies in its column's slab");
        self.retired.lock().push(MemoPtr(memo));
    }

    /// The memos the rows point at.
    fn current(&self) -> impl Iterator<Item = NonNull<DummyMemo>> + '_ {
        let rows = (self.rows.committed() / ROW_BYTES) as u32;
        (0..rows).filter_map(|row| NonNull::new(self.row(row)?.load(Ordering::Acquire)))
    }

    /// Checks that every slab slot is in exactly one state (module docs), with no task writing.
    #[cfg(debug_assertions)]
    fn check_accounting(&self, retired: &[MemoPtr]) {
        // SAFETY: Called only from `reclaim` and `Drop`, which have exclusive access.
        let free = unsafe { &*self.free.get() };
        let taken = self.free_taken.load(Ordering::Relaxed).min(free.len());
        let current = self.current().count();
        assert_eq!(
            current + retired.len() + (free.len() - taken),
            self.slab_len.load(Ordering::Relaxed),
            "salsa: every memo slab slot is a row's memo, retired or free"
        );
    }

    /// Drops the retired memos and makes their slots free.
    ///
    /// # Safety
    ///
    /// No memo of this column is borrowed, and no other thread accesses the column.
    unsafe fn reclaim(&self) {
        let retired = mem::take(&mut *self.retired.lock());
        #[cfg(debug_assertions)]
        self.check_accounting(&retired);
        // SAFETY: Exclusive access (our contract).
        let free = unsafe { &mut *self.free.get() };
        let taken = self.free_taken.swap(0, Ordering::Relaxed).min(free.len());
        free.drain(..taken);
        free.reserve(retired.len());
        for MemoPtr(memo) in retired {
            debug_assert!(self.owns(memo));
            // SAFETY: A retired memo is initialized, of the column's type, owned by the retired
            // list alone (it left its row when retired), and not borrowed (our contract).
            unsafe { ptr::drop_in_place((self.type_.to_dyn_fn)(memo).as_ptr()) };
            free.push(MemoPtr(memo));
        }
        #[cfg(debug_assertions)]
        self.check_accounting(&[]);
    }
}

impl Drop for Column {
    fn drop(&mut self) {
        let retired = mem::take(&mut *self.retired.lock());
        #[cfg(debug_assertions)]
        self.check_accounting(&retired);
        let current = self.current();
        let retired = retired.into_iter().map(|MemoPtr(memo)| memo);
        for memo in current.chain(retired) {
            // SAFETY: Each current or retired memo is initialized, of the column's type, owned by
            // the column in one place only, and with `&mut self` not borrowed. Dropping one
            // doesn't touch the rows the iterator reads.
            unsafe { ptr::drop_in_place((self.type_.to_dyn_fn)(memo).as_ptr()) };
        }
        // The free slots hold no memo; dropping the regions gives their pages back.
    }
}

/// A salsa struct ingredient's memo columns, shared by all its slots.
pub struct MemoTableTypes {
    columns: Vec<Column>,
    /// Rows handed out to slots.
    rows: AtomicU32,
}

impl Default for MemoTableTypes {
    fn default() -> Self {
        MemoTableTypes {
            // alloc: `Vec::new` allocates nothing; columns are added at setup
            columns: Vec::new(),
            rows: AtomicU32::new(0),
        }
    }
}

impl MemoTableTypes {
    pub(crate) fn set(
        &mut self,
        memo_ingredient_index: MemoIngredientIndex,
        memo_type: MemoEntryType,
    ) {
        self.columns
            .insert(memo_ingredient_index.as_usize(), Column::new(memo_type));
    }

    pub fn len(&self) -> usize {
        self.columns.len()
    }

    /// Whether memos of type `M` are registered at `memo_ingredient_index`.
    pub(crate) fn holds<M: Memo>(&self, memo_ingredient_index: MemoIngredientIndex) -> bool {
        self.columns
            .get(memo_ingredient_index.as_usize())
            .is_some_and(|column| column.type_.type_id == TypeId::of::<M>())
    }

    fn next_row(&self) -> u32 {
        let row = self.rows.fetch_add(1, Ordering::Relaxed);
        assert!(
            (row as usize) < MAX_ROWS,
            "salsa: a struct ingredient made more than {MAX_ROWS} slots, its memo columns' reservation"
        );
        row
    }

    /// The columns' address, for a page to keep beside its `Arc` of these types
    /// ([`MemoColumns::attach`]).
    pub(crate) fn columns(&self) -> MemoColumns {
        MemoColumns(NonNull::from(&self.columns[..]))
    }

    /// Drops the memos retired since the last reclaim and makes their slots free for reuse.
    ///
    /// # Safety
    ///
    /// No memo of these columns is borrowed, and no other thread accesses them: the caller has
    /// `&mut Zalsa`, through which every memo access goes.
    pub(crate) unsafe fn reclaim(&self) {
        for column in &self.columns {
            // SAFETY: Our contract.
            unsafe { column.reclaim() };
        }
    }

    /// # Safety
    ///
    /// The types table must be the correct one of `memos`.
    #[inline]
    pub(crate) unsafe fn attach_memos<'a>(
        &'a self,
        memos: &'a MemoTable,
    ) -> MemoTableWithTypes<'a> {
        debug_assert!(memos.row < self.rows.load(Ordering::Relaxed));
        MemoTableWithTypes {
            columns: &self.columns,
            memos,
        }
    }

    /// # Safety
    ///
    /// The types table must be the correct one of `memos`.
    #[inline]
    pub(crate) unsafe fn attach_memos_mut<'a>(
        &'a self,
        memos: &'a mut MemoTable,
    ) -> MemoTableWithTypesMut<'a> {
        debug_assert!(memos.row < self.rows.load(Ordering::Relaxed));
        MemoTableWithTypesMut {
            columns: &self.columns,
            memos,
        }
    }
}

/// The column at `memo_ingredient_index`, checked to hold memos of type `M`.
#[inline]
fn column_of<M: Memo>(columns: &[Column], memo_ingredient_index: MemoIngredientIndex) -> &Column {
    match columns.get(memo_ingredient_index.as_usize()) {
        Some(column) if column.type_.type_id == TypeId::of::<M>() => column,
        _ => type_assert_failed(memo_ingredient_index),
    }
}

/// A [`MemoTableTypes`]' columns, kept by each table page beside its `Arc` of the types so a
/// memo lookup reads the columns from the page's own line instead of going through the `Arc`.
#[derive(Clone, Copy)]
pub(crate) struct MemoColumns(NonNull<[Column]>);

impl MemoColumns {
    pub(crate) fn as_ptr(self) -> *const () {
        self.0.as_ptr().cast::<()>()
    }

    /// # Safety
    ///
    /// The `MemoTableTypes` these columns came from is alive for `'a`, unchanged since
    /// [`MemoTableTypes::columns`] (it can only change through `Arc::get_mut`, so holding a clone
    /// of its `Arc` is enough), and is the correct one of `memos`.
    #[inline]
    pub(crate) unsafe fn attach<'a>(self, memos: &'a MemoTable) -> MemoTableWithTypes<'a> {
        MemoTableWithTypes {
            // SAFETY: Our contract: the columns are alive, unmoved and shared for `'a`.
            columns: unsafe { self.0.as_ref() },
            memos,
        }
    }
}

pub struct MemoTableWithTypes<'a> {
    columns: &'a [Column],
    memos: &'a MemoTable,
}

/// A memo table slot whose observed allocations remain valid for shared access for `'a`.
pub(crate) struct MemoSlot<'a> {
    table: MemoTableWithTypes<'a>,
    memo_ingredient_index: MemoIngredientIndex,
}

impl<'a> MemoSlot<'a> {
    /// Creates a memo slot with an allocation-lifetime guarantee.
    ///
    /// # Safety
    ///
    /// Any allocation observed in this slot must remain at the same address and valid for shared
    /// access for `'a`.
    #[inline]
    pub(crate) unsafe fn new(
        table: MemoTableWithTypes<'a>,
        memo_ingredient_index: MemoIngredientIndex,
    ) -> Self {
        Self {
            table,
            memo_ingredient_index,
        }
    }

    /// Loads the erased memo currently stored in this slot.
    #[inline]
    pub(crate) fn get_erased(&self) -> Option<ErasedMemo<'a>> {
        // SAFETY: Guaranteed by the caller of `MemoSlot::new`.
        unsafe { self.table.get_erased(self.memo_ingredient_index) }
    }
}

impl<'a> MemoTableWithTypes<'a> {
    /// Stores `memo` as the row's memo and returns where it lives; the memo it replaces, if any,
    /// is retired and stays valid until the next reclaim.
    pub(crate) fn insert<M: Memo>(
        self,
        memo_ingredient_index: MemoIngredientIndex,
        memo: M,
    ) -> NonNull<M> {
        let column = column_of::<M>(self.columns, memo_ingredient_index);
        let slot = column.alloc().cast::<M>();
        // SAFETY: `alloc` gave a slot of `M`'s layout (the column's type, checked above) that
        // nothing else uses until we publish it.
        unsafe { slot.as_ptr().write(memo) };
        // AcqRel: release publishes the memo to readers of the row; acquire orders the old memo's
        // writes before we retire it.
        let old = column
            .row_or_init(self.memos.row)
            .swap(slot.cast::<DummyMemo>().as_ptr(), Ordering::AcqRel);
        if let Some(old) = NonNull::new(old) {
            debug_assert_ne!(old, slot.cast::<DummyMemo>());
            column.retire(old);
        }
        slot
    }

    /// Returns a pointer to the memo at the given index, if one has been inserted.
    #[inline]
    pub(crate) fn get<M: Memo>(
        self,
        memo_ingredient_index: MemoIngredientIndex,
    ) -> Option<NonNull<M>> {
        let column = self.columns.get(memo_ingredient_index.as_usize())?;
        if column.type_.type_id != TypeId::of::<M>() {
            type_assert_failed(memo_ingredient_index);
        }
        // Acquire: pairs with the release in `insert`, so the memo's writes are visible.
        NonNull::new(column.row(self.memos.row)?.load(Ordering::Acquire)).map(NonNull::cast)
    }

    /// Returns a type-erased view with the column's type metadata.
    ///
    /// # Safety
    ///
    /// Any allocation observed in the table entry must remain at the same address and valid for
    /// shared access for `'a`.
    #[inline]
    unsafe fn get_erased(
        &self,
        memo_ingredient_index: MemoIngredientIndex,
    ) -> Option<ErasedMemo<'a>> {
        let column = self.columns.get(memo_ingredient_index.as_usize())?;
        let memo = NonNull::new(column.row(self.memos.row)?.load(Ordering::Acquire))?;

        // SAFETY: `insert` type-checks and release-publishes a pointer to a slab slot holding a
        // memo of the column's type, with the provenance of its slab page; the acquire load
        // observes its initialization. The caller guarantees that it remains valid for `'a`.
        Some(unsafe {
            ErasedMemo::from_raw_parts(memo, column.type_.to_dyn_fn, column.type_.type_id)
        })
    }

    /// Returns memory usage information about the memos in this table.
    ///
    /// # Safety
    ///
    /// The caller must ensure exclusive access to the database's storage for the
    /// duration of this call, so memos cannot be replaced or freed during traversal.
    #[cfg(feature = "salsa_unstable")]
    pub(crate) unsafe fn memory_usage(&self) -> Vec<crate::database::MemoInfo> {
        // alloc: the edge: memory-usage reports, run with `&mut` outside builds
        let mut memory_usage = Vec::new();
        for column in self.columns {
            let Some(memo) = column
                .row(self.memos.row)
                .and_then(|row| NonNull::new(row.load(Ordering::Acquire)))
            else {
                continue;
            };

            // SAFETY: The column's memos are of its type (`insert` checks it). The caller
            // guarantees exclusive database access, so the memo remains valid while we measure it.
            // alloc: the edge: memory-usage reports, run with `&mut` outside builds
            let dyn_memo: &dyn Memo = unsafe { (column.type_.to_dyn_fn)(memo).as_ref() };
            memory_usage.push(dyn_memo.memory_usage());
        }

        memory_usage
    }
}

pub(crate) struct MemoTableWithTypesMut<'a> {
    columns: &'a [Column],
    memos: &'a mut MemoTable,
}

impl MemoTableWithTypesMut<'_> {
    /// Calls `f` on the memo at `memo_ingredient_index`.
    ///
    /// If the memo is not present, `f` is not called.
    pub(crate) fn map_memo<M: Memo>(
        self,
        memo_ingredient_index: MemoIngredientIndex,
        f: impl FnOnce(&mut M),
    ) {
        let column = column_of::<M>(self.columns, memo_ingredient_index);
        let Some(row) = column.row(self.memos.row) else {
            return;
        };
        let Some(memo) = NonNull::new(row.load(Ordering::Acquire)) else {
            return;
        };

        // SAFETY: The column holds memos of type `M` (checked above), and `&mut MemoTable` gives
        // exclusive access to the slot's memos.
        f(unsafe { memo.cast::<M>().as_mut() });
    }

    /// Retires every memo of the slot; they are dropped at the next reclaim.
    ///
    /// Note that calling this multiple times is safe, an empty row is skipped.
    ///
    /// # Safety
    ///
    /// The caller needs to make sure that the slot's memos are no longer reachable through it by
    /// anyone else; borrows of their contents may remain until the next reclaim.
    #[inline]
    pub unsafe fn drop(&mut self) {
        for column in self.columns {
            column.retire_row(self.memos.row);
        }
    }

    /// Retires every memo of the slot, calling `f` on each; they stay valid until the next
    /// reclaim, so `f` may use them.
    ///
    /// # Safety
    ///
    /// As for [`MemoTableWithTypesMut::drop`].
    // dyn: clearing a slot (a struct deleted or reused), once per memo it held
    pub(crate) unsafe fn take_memos(&mut self, mut f: impl FnMut(MemoIngredientIndex, &dyn Memo)) {
        for (index, column) in self.columns.iter().enumerate() {
            let Some(memo) = column.retire_row(self.memos.row) else {
                continue;
            };
            // SAFETY: The memo is of the column's type, and retired memos stay valid until the
            // next reclaim, which needs exclusive access to the whole database.
            // dyn: clearing a slot (a struct deleted or reused), once per memo it held
            let memo: &dyn Memo = unsafe { (column.type_.to_dyn_fn)(memo).as_ref() };
            f(MemoIngredientIndex::from_usize(index), memo);
        }
    }
}

/// This function is explicitly outlined to avoid debug machinery in the hot-path.
#[cold]
#[inline(never)]
fn type_assert_failed(memo_ingredient_index: MemoIngredientIndex) -> ! {
    panic!("inconsistent type-id for `{memo_ingredient_index:?}`")
}

impl std::fmt::Debug for MemoTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoTable").field("row", &self.row).finish()
    }
}

#[cfg(test)]
#[cfg(not(feature = "shuttle"))]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{MAX_ROWS, Memo, MemoEntryType, MemoTable, MemoTableTypes, ROW_BYTES};
    use crate::DatabaseKeyIndex;
    use crate::zalsa::{MemoIngredientIndex, Zalsa};

    /// A memo that counts its drops.
    struct Counted {
        value: u64,
        drops: Arc<AtomicUsize>,
    }

    impl Drop for Counted {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::Relaxed);
        }
    }

    impl Memo for Counted {
        fn has_value(&self) -> bool {
            true
        }

        fn remove_outputs(&self, _zalsa: &Zalsa, _executor: DatabaseKeyIndex) {}

        #[cfg(feature = "salsa_unstable")]
        fn memory_usage(&self) -> crate::database::MemoInfo {
            crate::database::MemoInfo {
                debug_name: "counted",
                output: crate::database::SlotInfo {
                    debug_name: "counted",
                    size_of_metadata: 0,
                    size_of_fields: 0,
                    heap_size_of_fields: None,
                    memos: Vec::new(),
                },
            }
        }
    }

    /// A second memo type, for the type check.
    struct Other(#[allow(dead_code)] u8);

    impl Memo for Other {
        fn has_value(&self) -> bool {
            true
        }

        fn remove_outputs(&self, _zalsa: &Zalsa, _executor: DatabaseKeyIndex) {}

        #[cfg(feature = "salsa_unstable")]
        fn memory_usage(&self) -> crate::database::MemoInfo {
            unreachable!()
        }
    }

    fn types(columns: usize) -> MemoTableTypes {
        let mut types = MemoTableTypes::default();
        for index in 0..columns {
            types.set(
                MemoIngredientIndex::from_usize(index),
                MemoEntryType::of::<Counted>(),
            );
        }
        types
    }

    fn counted(value: u64, drops: &Arc<AtomicUsize>) -> Counted {
        Counted {
            value,
            drops: drops.clone(),
        }
    }

    const FIRST: MemoIngredientIndex = MemoIngredientIndex::from_usize(0);
    const SECOND: MemoIngredientIndex = MemoIngredientIndex::from_usize(1);

    #[test]
    fn rows_and_slots_sit_at_fixed_offsets() {
        let drops = Arc::new(AtomicUsize::new(0));
        let types = types(1);
        let column = &types.columns[0];
        assert!(column.row(5000).is_none(), "nothing is committed before a memo is stored");
        // SAFETY: The tables are used with `types` only.
        let tables: Vec<MemoTable> = (0..5001).map(|_| unsafe { MemoTable::new(&types) }).collect();
        // SAFETY: `types` is the tables' types table.
        let (first, last) = unsafe {
            (
                types.attach_memos(&tables[0]).insert(FIRST, counted(0, &drops)),
                types.attach_memos(&tables[5000]).insert(FIRST, counted(5000, &drops)),
            )
        };
        let base = column.rows.base().as_ptr() as usize;
        let row = |row: u32| column.row(row).expect("committed") as *const _ as usize;
        assert_eq!(row(0), base);
        assert_eq!(row(5000), base + 5000 * ROW_BYTES);
        let stride = column.type_.layout.size();
        assert_eq!(first.as_ptr() as usize, column.slab.base().as_ptr() as usize);
        assert_eq!(last.as_ptr() as usize - first.as_ptr() as usize, stride);
        assert!(column.owns(first.cast()) && column.owns(last.cast()));
        assert!(!column.owns(
            std::ptr::NonNull::new((last.as_ptr() as usize + stride) as *mut _).unwrap()
        ));
        assert!(MAX_ROWS * ROW_BYTES <= column.rows.reserved());
        drop(types);
        assert_eq!(drops.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn rows_keep_their_own_memos_per_column() {
        let drops = Arc::new(AtomicUsize::new(0));
        let types = types(2);
        // SAFETY: The tables are used with `types` only.
        let (a, b) = unsafe { (MemoTable::new(&types), MemoTable::new(&types)) };
        assert_ne!(a.row, b.row);
        // SAFETY: `types` is the tables' types table.
        unsafe {
            types.attach_memos(&a).insert(FIRST, counted(1, &drops));
            types.attach_memos(&b).insert(SECOND, counted(2, &drops));
            let get = |table: &MemoTable, index| {
                types
                    .attach_memos(table)
                    .get::<Counted>(index)
                    .map(|memo| memo.as_ref().value)
            };
            assert_eq!(get(&a, FIRST), Some(1));
            assert_eq!(get(&a, SECOND), None);
            assert_eq!(get(&b, FIRST), None);
            assert_eq!(get(&b, SECOND), Some(2));
        }
        drop(types);
        assert_eq!(drops.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn replaced_memos_live_until_reclaim_and_their_slots_are_reused() {
        let drops = Arc::new(AtomicUsize::new(0));
        let types = types(1);
        // SAFETY: The table is used with `types` only.
        let table = unsafe { MemoTable::new(&types) };
        // SAFETY: `types` is the table's types table; `reclaim` runs with no memo borrowed.
        unsafe {
            let first = types.attach_memos(&table).insert(FIRST, counted(1, &drops));
            let second = types.attach_memos(&table).insert(FIRST, counted(2, &drops));
            assert_eq!(first.as_ref().value, 1, "a replaced memo stays readable");
            assert_eq!(drops.load(Ordering::Relaxed), 0);
            types.reclaim();
            assert_eq!(drops.load(Ordering::Relaxed), 1);
            let third = types.attach_memos(&table).insert(FIRST, counted(3, &drops));
            assert_eq!(third.cast::<u8>(), first.cast::<u8>(), "the freed slot is reused");
            assert_eq!(second.as_ref().value, 2);
            types.reclaim();
            assert_eq!(drops.load(Ordering::Relaxed), 2);
            assert_eq!(
                types
                    .attach_memos(&table)
                    .get::<Counted>(FIRST)
                    .map(|memo| memo.as_ref().value),
                Some(3)
            );
        }
        drop(types);
        assert_eq!(drops.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn taking_memos_retires_them() {
        let drops = Arc::new(AtomicUsize::new(0));
        let types = types(2);
        // SAFETY: The table is used with `types` only.
        let mut table = unsafe { MemoTable::new(&types) };
        // SAFETY: `types` is the table's types table; nothing else reaches the slot's memos.
        unsafe {
            types.attach_memos(&table).insert(FIRST, counted(1, &drops));
            types.attach_memos(&table).insert(SECOND, counted(2, &drops));
            let mut seen = Vec::new();
            types
                .attach_memos_mut(&mut table)
                .take_memos(|index, memo| seen.push((index, memo.has_value())));
            assert_eq!(seen, vec![(FIRST, true), (SECOND, true)]);
            assert_eq!(drops.load(Ordering::Relaxed), 0);
            assert!(types.attach_memos(&table).get::<Counted>(FIRST).is_none());
            types.attach_memos_mut(&mut table).drop();
            types.reclaim();
        }
        assert_eq!(drops.load(Ordering::Relaxed), 2);
        table.reset();
    }

    #[test]
    fn map_memo_changes_the_memo_in_place() {
        let drops = Arc::new(AtomicUsize::new(0));
        let types = types(1);
        // SAFETY: The table is used with `types` only.
        let mut table = unsafe { MemoTable::new(&types) };
        // SAFETY: `types` is the table's types table.
        unsafe {
            types.attach_memos(&table).insert(FIRST, counted(1, &drops));
            types
                .attach_memos_mut(&mut table)
                .map_memo(FIRST, |memo: &mut Counted| memo.value = 7);
            assert_eq!(
                types
                    .attach_memos(&table)
                    .get::<Counted>(FIRST)
                    .map(|memo| memo.as_ref().value),
                Some(7)
            );
        }
    }

    #[test]
    #[should_panic(expected = "inconsistent type-id")]
    fn the_wrong_memo_type_panics() {
        let types = types(1);
        // SAFETY: The table is used with `types` only.
        let table = unsafe { MemoTable::new(&types) };
        // SAFETY: `types` is the table's types table.
        unsafe { types.attach_memos(&table).insert(FIRST, Other(0)) };
    }

    /// Many tasks insert into their own rows and replace memos at once; after reclaim, every
    /// row reads its last memo and every other memo was dropped once.
    #[test]
    fn racing_inserts_keep_every_row() {
        let drops = Arc::new(AtomicUsize::new(0));
        let types = types(1);
        // SAFETY: The tables are used with `types` only.
        let tables: Vec<MemoTable> = (0..4000).map(|_| unsafe { MemoTable::new(&types) }).collect();
        for round in 0..3u64 {
            std::thread::scope(|scope| {
                for chunk in tables.chunks(500) {
                    let (types, drops) = (&types, &drops);
                    scope.spawn(move || {
                        for (i, table) in chunk.iter().enumerate() {
                            // SAFETY: `types` is the table's types table.
                            unsafe {
                                types
                                    .attach_memos(table)
                                    .insert(FIRST, counted(round * 10_000 + i as u64, drops))
                            };
                        }
                    });
                }
            });
            // SAFETY: The threads are joined; no memo is borrowed.
            unsafe { types.reclaim() };
        }
        for chunk in tables.chunks(500) {
            for (i, table) in chunk.iter().enumerate() {
                // SAFETY: `types` is the table's types table, and the memo stays valid while
                // `types` lives and nothing reclaims.
                let value = unsafe {
                    types
                        .attach_memos(table)
                        .get::<Counted>(FIRST)
                        .map(|memo| memo.as_ref().value)
                };
                assert_eq!(value, Some(20_000 + i as u64));
            }
        }
        assert_eq!(drops.load(Ordering::Relaxed), 8000);
        drop(types);
        assert_eq!(drops.load(Ordering::Relaxed), 12000);
    }
}

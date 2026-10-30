use std::alloc::Layout;
use std::any::{Any, TypeId};
use std::cell::UnsafeCell;
use std::marker::PhantomData;
use std::mem::{self, MaybeUninit};
use std::ptr::{self, NonNull};
use std::slice;

use blaze_mem::{Column, Region, Tag};
use memo::MemoTable;
use rustc_hash::FxHashMap;

use crate::sync::atomic::{AtomicUsize, Ordering};
use crate::sync::{Arc, Mutex};
use crate::table::memo::{MemoColumns, MemoTableTypes, MemoTableWithTypes, MemoTableWithTypesMut};
use crate::zalsa::MemoIngredientIndex;
use crate::{Id, IngredientIndex, Revision};

pub(crate) mod memo;
mod page_cache;
pub(crate) mod scan;

pub(crate) use page_cache::PageCache;

pub(crate) const PAGE_LEN_BITS: usize = 7;
const PAGE_LEN_MASK: usize = PAGE_LEN - 1;
const PAGE_LEN: usize = 1 << PAGE_LEN_BITS;
pub(crate) const MAX_PAGES: usize = Id::MAX_USIZE / PAGE_LEN;

/// A typed [`Page`] view.
pub(crate) struct PageView<'p, T: Slot>(&'p Page, PhantomData<&'p T>);

pub struct Table {
    /// Declared before the arenas, so dropped first: a page's drop drops its slots in place,
    /// in an arena.
    pages: Column<Page>,
    /// Map from ingredient to non-full pages that are up for grabs
    non_full_pages: Mutex<FxHashMap<IngredientIndex, Vec<PageIndex>>>,
    /// The slots of pages of a type tagged `Query`: inputs and tracked structs.
    query_slots: PageArena,
    /// The slots of pages of a type tagged `Intern`: interned values.
    intern_slots: PageArena,
}

/// The most address space one arena of slot pages reserves: 64 GB, committed only as pages are
/// made.
const SLOT_BYTES: usize = 1 << 36;

/// The slot pages of one tag, in one reserved region: bumped by one compare-and-swap per page
/// (per 128 slots), never moved, and freed with the table.
struct PageArena {
    region: Region,
    used: AtomicUsize,
}

impl PageArena {
    fn new(tag: Tag) -> PageArena {
        PageArena {
            region: Region::reserve_huge(SLOT_BYTES, tag).unwrap_or_else(|err| {
                panic!("salsa: reserving the table's `{tag}` slot pages: {err}")
            }),
            used: AtomicUsize::new(0),
        }
    }

    /// Committed, uninitialized room for one page of `layout`, the caller's until the arena
    /// drops.
    fn alloc(&self, layout: Layout) -> NonNull<()> {
        assert!(
            layout.align() <= blaze_mem::page_size(),
            "salsa: a slot type is aligned past a page"
        );
        let mut used = self.used.load(Ordering::Relaxed);
        let end = loop {
            let start = used.next_multiple_of(layout.align());
            let end = start + layout.size();
            match self
                .used
                .compare_exchange_weak(used, end, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => break end,
                Err(now) => used = now,
            }
        };
        self.region.ensure(end).unwrap_or_else(|err| {
            panic!(
                "salsa: committing a page of `{}` slots: {err}",
                self.region.tag()
            )
        });
        self.region.at(end - layout.size()).cast::<()>()
    }
}

/// The arena for pages of `T`, by its tag.
fn arena_of<'a, T: Slot>(query: &'a PageArena, intern: &'a PageArena) -> &'a PageArena {
    match T::MEM_TAG {
        Tag::Query => query,
        Tag::Intern => intern,
        other => panic!("salsa: table slots are tagged `Query` or `Intern`, not `{other}`"),
    }
}

#[cold]
#[inline(never)]
fn page_missing(page: usize) -> ! {
    panic!("salsa: page `{page}` is not allocated")
}

/// # Safety
///
/// Implementors of this trait need to make sure that their type is unique with respect to
/// their owning ingredient as the allocation strategy relies on this.
pub unsafe trait Slot: Any + Send + Sync {
    /// Access the [`MemoTable`][] for this slot.
    ///
    /// # Safety condition
    ///
    /// The current revision MUST be the current revision of the database containing this slot.
    unsafe fn memos(slot: *const Self, current_revision: Revision) -> *const MemoTable;

    /// Access the memo table without validating or locking the slot.
    ///
    /// # Safety
    ///
    /// `slot` must be initialized and remain alive without reuse or destruction of its memo
    /// storage for the entire lifetime of any references obtained through the returned pointer.
    unsafe fn memos_unchecked(slot: *const Self) -> *const MemoTable;

    /// Mutably access the [`MemoTable`] for this slot.
    fn memos_mut(&mut self) -> &mut MemoTable;

    /// The subsystem the table's pages of these slots count against: `Query`, or `Intern` for
    /// interned values.
    const MEM_TAG: Tag = Tag::Query;
}

/// [Slot::memos]
type SlotMemosFnErased = unsafe fn(*const (), current_revision: Revision) -> *const MemoTable;
/// [Slot::memos]
type SlotMemosFn<T> = unsafe fn(*const T, current_revision: Revision) -> *const MemoTable;
/// [Slot::memos_mut]
type SlotMemosMutFnErased = unsafe fn(*mut ()) -> *mut MemoTable;
/// [Slot::memos_mut]
type SlotMemosMutFn<T> = fn(&mut T) -> &mut MemoTable;

struct SlotVTable {
    layout: Layout,
    /// [`Slot`] methods
    memos: SlotMemosFnErased,
    memos_mut: SlotMemosMutFnErased,
    /// The type name of what is stored as entries in data.
    type_name: fn() -> &'static str,
    /// Drops the page's slots in place; their memory is the arena's, freed with the table.
    /// SAFETY: The caller is required to supply the page's data pointer (a `PageData<T>` in a
    /// [`PageArena`]), and the correct initialized length, memo types and the page's first memo
    /// row, once.
    drop_impl:
        unsafe fn(data: *mut (), initialized: usize, memo_types: &MemoTableTypes, first_row: u32),
}

impl SlotVTable {
    const fn of<T: Slot>() -> &'static Self {
        const {
            &Self {
                drop_impl: |data, initialized, memo_types, first_row| {
                    let data = data.cast::<PageDataEntry<T>>();
                    for i in 0..initialized {
                        // SAFETY: The caller provides the page's data, its initialized length and
                        // its first row, so entry `i` is in the page and holds an initialized `T`
                        // that nothing else drops, and its row is `first_row + i`.
                        unsafe {
                            let item = (*data.add(i)).get().cast::<T>();
                            memo_types
                                .attach_memos_mut(first_row + i as u32, (*item).memos_mut())
                                .drop();
                            ptr::drop_in_place(item);
                        }
                    }
                },
                layout: Layout::new::<T>(),
                type_name: std::any::type_name::<T>,
                // SAFETY: The signatures are ABI-compatible.
                memos: unsafe { mem::transmute::<SlotMemosFn<T>, SlotMemosFnErased>(T::memos) },
                // SAFETY: The signatures are ABI-compatible.
                memos_mut: unsafe {
                    mem::transmute::<SlotMemosMutFn<T>, SlotMemosMutFnErased>(T::memos_mut)
                },
            }
        }
    }
}

type PageDataEntry<T> = UnsafeCell<MaybeUninit<T>>;
type PageData<T> = [PageDataEntry<T>; PAGE_LEN];

struct Page {
    /// The ingredient for elements on this page.
    ingredient: IngredientIndex,

    /// Number of elements of `data` that are initialized.
    allocated: AtomicUsize,

    /// The potentially uninitialized data of this page, a `PageData<SlotType>` in its tag's
    /// [`PageArena`]. As we initialize new entries, we increment `allocated`.
    data: NonNull<()>,

    /// A vtable for the slot type stored in this page.
    slot_vtable: &'static SlotVTable,
    /// The type id of what is stored as entries in data.
    // FIXME: Move this into SlotVTable once const stable
    slot_type_id: TypeId,

    /// `memo_types`' columns, read by memo lookups from this line instead of through the `Arc`.
    memo_columns: MemoColumns,

    /// The memo row of slot 0; slot `s`'s is `memo_rows + s` (`MemoTableTypes::next_page_rows`).
    memo_rows: u32,

    memo_types: Arc<MemoTableTypes>,
}

// SAFETY: `Page` is `Send` as we make sure to only ever store `Slot` types in it which
// requires `Send`.`
unsafe impl Send for Page /* where for<M: Memo> M: Send */ {}
// SAFETY: `Page` is `Sync` as we make sure to only ever store `Slot` types in it which
// requires `Sync`.`
unsafe impl Sync for Page /* where for<M: Memo> M: Sync */ {}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PageIndex(usize);

impl PageIndex {
    #[inline]
    pub(crate) fn new(idx: usize) -> Self {
        debug_assert!(idx < MAX_PAGES);
        Self(idx)
    }

    #[allow(dead_code)]
    pub fn as_usize(&self) -> usize {
        self.0
    }
}

#[derive(Copy, Clone, Debug)]
pub struct SlotIndex(usize);

impl SlotIndex {
    #[inline]
    fn new(idx: usize) -> Self {
        debug_assert!(idx < PAGE_LEN);
        Self(idx)
    }
    pub(crate) fn as_usize(&self) -> usize {
        self.0
    }
}

impl Default for Table {
    fn default() -> Self {
        Self {
            pages: Column::new(MAX_PAGES, Tag::Query)
                .unwrap_or_else(|err| panic!("salsa: reserving the table's page list: {err}")),
            non_full_pages: Default::default(),
            query_slots: PageArena::new(Tag::Query),
            intern_slots: PageArena::new(Tag::Intern),
        }
    }
}

impl Table {
    #[inline]
    fn page_at(&self, page: usize) -> &Page {
        match self.pages.get(page as u32) {
            Some(page) => page,
            None => page_missing(page),
        }
    }

    fn arena<T: Slot>(&self) -> &PageArena {
        arena_of::<T>(&self.query_slots, &self.intern_slots)
    }

    fn push(&self, page: Page) -> usize {
        match self.pages.push(page) {
            Ok(index) => index as usize,
            Err(err) => panic!("salsa: adding a page to the table: {err}"),
        }
    }

    /// Returns the [`IngredientIndex`] for an [`Id`].
    #[inline]
    pub fn ingredient_index(&self, id: Id) -> IngredientIndex {
        let (page_idx, _) = split_id(id);
        self.page_at(page_idx.0).ingredient
    }

    /// Get a reference to the data for `id`, which must have been allocated from this table with type `T`.
    ///
    /// # Panics
    ///
    /// If `id` is out of bounds or the does not have the type `T`.
    pub(crate) fn get<T: Slot>(&self, id: Id) -> &T {
        let (page, slot) = split_id(id);
        let page_ref = self.page::<T>(page);
        &page_ref.data()[slot.0]
    }

    /// Get a raw pointer to the data for `id`, which must have been allocated from this table.
    ///
    /// # Panics
    ///
    /// If `id` is out of bounds or the does not have the type `T`.
    ///
    /// # Safety
    ///
    /// See [`Page::get_raw`][].
    pub(crate) fn get_raw<T: Slot>(&self, id: Id) -> *mut T {
        let (page, slot) = split_id(id);
        let page_ref = self.page::<T>(page);
        page_ref.page_data()[slot.0].get().cast::<T>()
    }

    /// Returns the number of pages that have been allocated.
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// The ingredient whose slots page `page` holds, or `None` for a placeholder page.
    pub(crate) fn page_owner(&self, page: usize) -> Option<IngredientIndex> {
        let page = self.page_at(page);
        (page.slot_type_id != TypeId::of::<DummySlot>()).then_some(page.ingredient)
    }

    #[cfg(feature = "salsa_unstable")]
    pub(crate) fn page_capacity(&self) -> usize {
        PAGE_LEN
    }

    #[cfg(feature = "salsa_unstable")]
    pub(crate) fn page_infos(&self) -> FxHashMap<IngredientIndex, crate::database::PageInfo> {
        let mut page_fills: FxHashMap<IngredientIndex, Vec<usize>> = FxHashMap::default();

        for (_, page) in self.pages.iter() {
            let fill = page.allocated.load(Ordering::Acquire);
            if fill > 0 {
                page_fills.entry(page.ingredient).or_default().push(fill);
            }
        }

        page_fills
            .into_iter()
            .filter_map(|(ingredient, fills)| {
                crate::database::PageInfo::from_page_fills(PAGE_LEN, fills)
                    .map(|page_info| (ingredient, page_info))
            })
            // alloc: the edge: memory-usage reports, run with `&mut` outside builds
            .collect()
    }

    /// Gets a reference to the page which has slots of type `T`
    ///
    /// # Panics
    ///
    /// If `page` is out of bounds or the type `T` is incorrect.
    #[inline]
    pub(crate) fn page<T: Slot>(&self, page: PageIndex) -> PageView<'_, T> {
        self.page_at(page.0).assert_type::<T>()
    }

    /// Force initialize the page at the given index.
    ///
    /// If the page at the provided index was created using `push_uninit_page`, it
    /// will be initialized using the provided ingredient data.
    ///
    /// Otherwise, the page will be allocated.
    ///
    /// # Panics
    ///
    /// If `page` is out of bounds or the type `T` is incorrect.
    #[inline]
    #[allow(dead_code)]
    pub(crate) fn force_page<T: Slot>(
        &mut self,
        page_idx: PageIndex,
        ingredient: IngredientIndex,
        memo_types: &Arc<MemoTableTypes>,
    ) {
        let page = self.pages.get_mut(page_idx.0 as u32);

        match page {
            Some(page) => {
                // Initialize the page if was created using `push_uninit_page`.
                if page.slot_type_id == TypeId::of::<DummySlot>() {
                    let arena = arena_of::<T>(&self.query_slots, &self.intern_slots);
                    // alloc: one `Arc` count per page; the page's slots are in its tag's arena
                    *page = Page::new::<T>(ingredient, memo_types.clone(), arena);
                }

                // Ensure the page has the correct type.
                page.assert_type::<T>();
            }

            None => {
                // Create dummy pages until we reach the page we want.
                while self.page_count() < page_idx.as_usize() {
                    // We make sure not to claim any intermediary pages for ourselves, as they may
                    // be required by a different ingredient when it is deserialized.
                    self.push_uninit_page();
                }

                // alloc: one page and one `Arc` count per 128 slots, when a page fills
                let allocated_idx = self.push_page::<T>(ingredient, memo_types.clone());
                assert_eq!(
                    allocated_idx, page_idx,
                    "allocated index does not match requested index"
                );
            }
        };
    }

    /// Allocate a new page for the given ingredient and with slots of type `T`
    #[inline]
    pub(crate) fn push_page<T: Slot>(
        &self,
        ingredient: IngredientIndex,
        memo_types: Arc<MemoTableTypes>,
    ) -> PageIndex {
        PageIndex::new(self.push(Page::new::<T>(ingredient, memo_types, self.arena::<T>())))
    }

    /// Allocate an uninitialized page.
    #[inline]
    #[allow(dead_code)]
    pub(crate) fn push_uninit_page(&self) -> PageIndex {
        // Note that `DummySlot` is a ZST, so the memory wasted by any pages of ingredients
        // that were not serialized should be negligible.
        PageIndex::new(self.push(Page::new::<DummySlot>(
            IngredientIndex::new(0),
            // alloc: one `Arc` per placeholder page, when persistence restores a database
            Arc::new(MemoTableTypes::default()),
            self.arena::<DummySlot>(),
        )))
    }

    /// Get the memo table associated with `id` for the concrete type `T`.
    ///
    /// # Safety
    ///
    /// The parameter `current_revision` must be the current revision of the database
    /// owning this table.
    ///
    /// # Panics
    ///
    /// If `page` is out of bounds or the type `T` is incorrect.
    pub unsafe fn memos<T: Slot>(
        &self,
        id: Id,
        current_revision: Revision,
    ) -> MemoTableWithTypes<'_> {
        let (page, slot) = split_id(id);
        let page = self.page_at(page.0).assert_type::<T>();
        let slot_data = &page.data()[slot.0];

        // Validates the slot (a tracked struct's read lock); the row doesn't wait for it.
        // SAFETY: The caller is required to pass the `current_revision`.
        let _ = unsafe { T::memos(slot_data, current_revision) };

        // SAFETY: `slot` is an allocated slot of this page (`data` checked it).
        unsafe { page.0.attach_memos(slot) }
    }

    /// Get the memo table associated with `id`.
    ///
    /// Unlike `Table::memos`, this does not require a concrete type, and instead uses dynamic
    /// dispatch.
    ///
    /// # Safety
    ///
    /// The parameter `current_revision` must be the current revision of the owner of database
    /// owning this table.
    pub unsafe fn dyn_memos(&self, id: Id, current_revision: Revision) -> MemoTableWithTypes<'_> {
        let (page, slot) = split_id(id);
        let page = self.page_at(page.0);
        // SAFETY: We supply a proper slot pointer and the caller is required to pass the `current_revision`.
        let _ = unsafe { (page.slot_vtable.memos)(page.get(slot), current_revision) };
        // SAFETY: `slot` is an allocated slot of this page (`Page::get` checked it).
        unsafe { page.attach_memos(slot) }
    }

    /// Get the memo table without validating or locking its slot.
    ///
    /// # Safety
    ///
    /// The slot must remain alive without reuse or destruction of its memo storage for the
    /// entire lifetime of the returned table and any references obtained from it.
    pub(crate) unsafe fn dyn_memos_unchecked(&self, id: Id) -> MemoTableWithTypes<'_> {
        let (page, slot) = split_id(id);
        let page = self.page_at(page.0);
        page.assert_allocated(slot);
        // SAFETY: `slot` is an allocated slot of this page, checked above; the caller prevents
        // its reuse and destruction.
        unsafe { page.attach_memos(slot) }
    }

    /// The memo row of the slot `id`, in its ingredient's memo columns.
    ///
    /// # Panics
    ///
    /// If `id` is not an allocated slot.
    pub(crate) fn memo_row(&self, id: Id) -> u32 {
        let (page, slot) = split_id(id);
        let page = self.page_at(page.0);
        page.assert_allocated(slot);
        page.memo_row(slot)
    }

    /// Get the memo table associated with `id`
    pub(crate) fn memos_mut(&mut self, id: Id) -> MemoTableWithTypesMut<'_> {
        let (page, slot) = split_id(id);
        let page_index = page.0;
        let page = self
            .pages
            .get_mut(page_index as u32)
            .unwrap_or_else(|| panic!("index `{page_index}` is uninitialized"));
        let row = page.memo_row(slot);
        // SAFETY: We supply a proper slot pointer and the caller is required to pass the `current_revision`.
        let memos = unsafe { &mut *(page.slot_vtable.memos_mut)(page.get(slot)) };
        // SAFETY: `row` is the row of the slot `memos` belongs to, of this page's ingredient.
        unsafe { page.memo_types.attach_memos_mut(row, memos) }
    }

    /// Calls `f` with the memo table of every allocated slot on the pages that hold memos of type
    /// `M` at the index `memo_index` returns for the page's ingredient.
    pub(crate) fn for_each_memo_table_mut<M: memo::Memo>(
        &mut self,
        memo_index: impl Fn(IngredientIndex) -> Option<MemoIngredientIndex>,
        mut f: impl FnMut(MemoTableWithTypesMut<'_>, MemoIngredientIndex),
    ) {
        for page_index in 0..self.pages.len() {
            let Some(page) = self.pages.get_mut(page_index as u32) else {
                continue;
            };
            let Some(memo_ingredient_index) = memo_index(page.ingredient) else {
                continue;
            };
            if !page.memo_types.holds::<M>(memo_ingredient_index) {
                continue;
            }
            for slot in 0..*page.allocated.get_mut() {
                let slot = SlotIndex::new(slot);
                let row = page.memo_row(slot);
                let slot = page.get(slot);
                // SAFETY: `slot` points to an initialized slot of this page, whose vtable matches
                // its slot type, and `&mut self` makes this the only access to it.
                let memos = unsafe { &mut *(page.slot_vtable.memos_mut)(slot) };
                // SAFETY: `row` is the row of the slot `memos` belongs to, of this page's
                // ingredient.
                let memos = unsafe { page.memo_types.attach_memos_mut(row, memos) };
                f(memos, memo_ingredient_index);
            }
        }
    }

    pub(crate) fn slots_of<T: Slot>(&self) -> impl Iterator<Item = (Id, &T)> + '_ {
        ErasedSlots {
            pages: self.pages.iter(),
            current_page: None,
            slot_type_id: TypeId::of::<T>(),
        }
        .map(|(id, slot)| {
            // SAFETY: `ErasedSlots` only visits pages whose `TypeId` matches `T` and only returns
            // pointers to initialized slots.
            (id, unsafe { &*slot.cast::<T>() })
        })
    }

    #[cold]
    #[inline(never)]
    pub(crate) fn fetch_or_push_page<T: Slot>(
        &self,
        ingredient: IngredientIndex,
        memo_types: impl FnOnce() -> Arc<MemoTableTypes>,
    ) -> PageIndex {
        if let Some(page) = self.take_non_full_page(ingredient) {
            return page;
        }

        self.push_page::<T>(ingredient, memo_types())
    }

    fn take_non_full_page(&self, ingredient: IngredientIndex) -> Option<PageIndex> {
        self.non_full_pages
            .lock()
            .get_mut(&ingredient)
            .and_then(Vec::pop)
    }

    pub(crate) fn record_unfilled_page(&self, ingredient: IngredientIndex, page: PageIndex) {
        self.non_full_pages
            .lock()
            .entry(ingredient)
            .or_default()
            .push(page);
    }
}

struct ErasedSlots<'db, P> {
    pages: P,
    current_page: Option<(PageIndex, &'db Page, std::ops::Range<usize>)>,
    slot_type_id: TypeId,
}

impl<'db, P: Iterator<Item = (u32, &'db Page)>> Iterator for ErasedSlots<'db, P> {
    type Item = (Id, *mut ());

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some((page_index, page, slots)) = &mut self.current_page
                && let Some(slot_index) = slots.next()
            {
                let slot_index = SlotIndex::new(slot_index);
                let id = make_id(*page_index, slot_index);

                // SAFETY: `slot_index` is below the initialized length captured when the page
                // became current, so the resulting pointer is within the page allocation.
                let slot = unsafe {
                    page.data
                        .as_ptr()
                        .byte_add(slot_index.0 * page.slot_vtable.layout.size())
                };

                return Some((id, slot));
            }

            self.current_page = self.pages.find_map(|(page_index, page)| {
                (page.slot_type_id == self.slot_type_id).then(|| {
                    (
                        PageIndex::new(page_index as usize),
                        page,
                        0..page.allocated.load(Ordering::Acquire),
                    )
                })
            });
            self.current_page.as_ref()?;
        }
    }
}

impl<'db, T: Slot> PageView<'db, T> {
    #[inline]
    fn page_data(&self) -> &'db [PageDataEntry<T>] {
        let len = self.0.allocated.load(Ordering::Acquire);
        // SAFETY: `len` is the initialized length of the page
        unsafe { slice::from_raw_parts(self.0.data.cast::<PageDataEntry<T>>().as_ptr(), len) }
    }

    #[inline]
    fn data(&self) -> &'db [T] {
        let len = self.0.allocated.load(Ordering::Acquire);
        // SAFETY: `len` is the initialized length of the page
        unsafe { slice::from_raw_parts(self.0.data.cast::<T>().as_ptr(), len) }
    }

    /// Allocate a value in this page.
    ///
    /// # Safety
    ///
    /// The caller must be the unique writer to this page, i.e. `allocate` cannot be called
    /// concurrently by multiple threads. Concurrent readers however, are fine.
    #[inline]
    pub(crate) unsafe fn allocate<V>(&self, page: PageIndex, value: V) -> Result<(Id, &'db T), V>
    where
        V: FnOnce(Id) -> T,
    {
        let index = self.0.allocated.load(Ordering::Acquire);
        if index >= PAGE_LEN {
            return Err(value);
        }

        // Initialize entry `index`
        let id = make_id(page, SlotIndex::new(index));
        let data = self.0.data.cast::<PageDataEntry<T>>();

        // SAFETY: `index` is also guaranteed to be in bounds as per the check above.
        let entry = unsafe { &*data.as_ptr().add(index) };

        // SAFETY: The caller guarantees we are the unique writer, and readers will not attempt to
        // access this index until we have updated the length.
        unsafe { (*entry.get()).write(value(id)) };

        // SAFETY: We just initialized the value above.
        let value = unsafe { (*entry.get()).assume_init_ref() };

        // Update the length now that we have initialized the value.
        self.0.allocated.store(index + 1, Ordering::Release);

        Ok((id, value))
    }
}

impl Page {
    /// The memos of slot `slot`, at its row: computed from this page's line, not read from the
    /// slot.
    ///
    /// # Safety
    ///
    /// `slot` is an allocated slot of this page.
    #[inline]
    unsafe fn attach_memos(&self, slot: SlotIndex) -> MemoTableWithTypes<'_> {
        debug_assert!(slot.0 < self.allocated.load(Ordering::Relaxed));
        debug_assert!(std::ptr::eq(
            self.memo_columns.as_ptr(),
            self.memo_types.columns().as_ptr()
        ));
        // SAFETY: `memo_columns` came from `memo_types`, which this page keeps alive; it can't
        // change while shared (`Arc::get_mut`), and the row is of a slot of this page, so of
        // its ingredient (our contract).
        unsafe { self.memo_columns.attach(self.memo_row(slot)) }
    }

    /// Slot `slot`'s memo row.
    #[inline]
    fn memo_row(&self, slot: SlotIndex) -> u32 {
        debug_assert!(slot.0 < PAGE_LEN);
        let row = self.memo_rows + slot.0 as u32;
        #[cfg(debug_assertions)]
        debug_assert!(row < self.memo_types.rows_handed_out());
        row
    }

    #[inline]
    fn new<T: Slot>(
        ingredient: IngredientIndex,
        memo_types: Arc<MemoTableTypes>,
        arena: &PageArena,
    ) -> Self {
        Self {
            ingredient,
            memo_columns: memo_types.columns(),
            memo_rows: memo_types.next_page_rows(),
            memo_types,
            slot_vtable: SlotVTable::of::<T>(),
            slot_type_id: TypeId::of::<T>(),
            allocated: AtomicUsize::new(0),
            data: arena.alloc(Layout::new::<PageData<T>>()),
        }
    }

    /// Retrieves the pointer for the given slot.
    ///
    /// # Panics
    ///
    /// If slot is out of bounds
    fn get(&self, slot: SlotIndex) -> *mut () {
        self.assert_allocated(slot);

        // SAFETY: We have checked that the resulting pointer will be within bounds.
        unsafe {
            self.data
                .as_ptr()
                .byte_add(slot.0 * self.slot_vtable.layout.size())
        }
    }

    #[inline]
    fn assert_allocated(&self, slot: SlotIndex) {
        let len = self.allocated.load(Ordering::Acquire);
        assert!(
            slot.0 < len,
            "out of bounds access `{slot:?}` (maximum slot `{len}`)"
        );
    }

    #[inline]
    fn assert_type<T: Slot>(&self) -> PageView<'_, T> {
        if self.slot_type_id != TypeId::of::<T>() {
            type_assert_failed::<T>(self);
        }

        PageView(self, PhantomData)
    }
}

/// This function is explicitly outlined to avoid debug machinery in the hot-path.
#[cold]
#[inline(never)]
fn type_assert_failed<T: 'static>(page: &Page) -> ! {
    panic!(
        "page has slot type `{:?}` but `{:?}` was expected",
        (page.slot_vtable.type_name)(),
        std::any::type_name::<T>(),
    )
}

impl Drop for Page {
    fn drop(&mut self) {
        let len = *self.allocated.get_mut();
        // SAFETY: We supply the data pointer, the initialized length, and the page's own memo
        // types and first row.
        unsafe {
            (self.slot_vtable.drop_impl)(self.data.as_ptr(), len, &self.memo_types, self.memo_rows)
        };
    }
}

/// A placeholder type representing the slots of an uninitialized `Page`.
struct DummySlot;

// SAFETY: The `DummySlot type is private.
unsafe impl Slot for DummySlot {
    unsafe fn memos(_: *const Self, _: Revision) -> *const MemoTable {
        unreachable!()
    }

    unsafe fn memos_unchecked(_: *const Self) -> *const MemoTable {
        unreachable!()
    }

    fn memos_mut(&mut self) -> &mut MemoTable {
        unreachable!()
    }
}

fn make_id(page: PageIndex, slot: SlotIndex) -> Id {
    let page = page.0 as u32;
    let slot = slot.0 as u32;
    // SAFETY: `slot` is guaranteed to be small enough that the resulting Id won't be bigger than `Id::MAX_U32`
    unsafe { Id::from_index((page << PAGE_LEN_BITS) | slot) }
}

#[inline]
pub fn split_id(id: Id) -> (PageIndex, SlotIndex) {
    let index = id.index() as usize;
    let slot = index & PAGE_LEN_MASK;
    let page = index >> PAGE_LEN_BITS;
    (PageIndex::new(page), SlotIndex::new(slot))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_keep_their_order_and_their_arena() {
        let table = Table::default();
        let types = Arc::new(MemoTableTypes::default());
        let first = table.push_page::<DummySlot>(IngredientIndex::new(3), types.clone());
        let second = table.push_page::<DummySlot>(IngredientIndex::new(4), types);
        assert_eq!((first.as_usize(), second.as_usize()), (0, 1));
        assert_eq!(table.page_count(), 2);
        assert_eq!(
            table.ingredient_index(make_id(second, SlotIndex::new(0))),
            IngredientIndex::new(4)
        );
        assert_eq!(table.query_slots.region.tag(), Tag::Query);
        assert_eq!(table.intern_slots.region.tag(), Tag::Intern);
    }
}

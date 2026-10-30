//! The tree's nodes, stored once and read from any thread without a lock
//! (docs/v2/plan/design/concurrency.md, docs/v2/plan/design/data.md). The crate's one unsafe
//! module (docs/v2/plan/design/unsafe.md): everything it offers is safe to call.
//!
//! - A [`Table`] is one generation of nodes: append-only columns indexed by a node's `u32`, in
//!   chunks that never move, and an open-addressed index from content ID to node, probed by the
//!   ID's own bits (never hashed again) and grown by adding a level.
//! - Each column publishes its length with a release store after writing the values below it,
//!   and every read is checked against the length it loads with acquire: a read past what is
//!   written panics, it never reads uninitialized memory. A value is pushed before anything
//!   that names it (a node before its index place, entries and children before their
//!   directory), so a node reached through the index or a directory reads whole.
//! - Only the [`Writer`] pushes, through `&mut self`, and there is one per [`Shared`] (made
//!   with it, never cloned); a collection fills a [`Building`] it alone holds. A reader reaches
//!   a table only through a [`Snapshot`], which is in a task ([`blaze_reclaim::enter`]) for as
//!   long as anything it lends lives; a replaced table is freed once every such task has ended.
//! - Names are interned once for the tree's life, as spans of one byte column.
//!
//! Why unsafe: the columns are read while the writer appends, with no lock (one acquire load
//! and a compare per read). The safe version, a `RwLock<Vec<_>>` per column or a table copied
//! per commit, costs every read a shared write (the lock's count) or every commit a copy of the
//! tree.
//!
//! Costs: a node is 32 bytes of ID plus an 8-byte [`Shape`], and 4 bytes of index place at
//! most half full; a directory entry is 9 bytes (name, node, mode byte) in three columns; a
//! file's bytes are one `Arc<[u8]>` shared with every reader. Finding a root is one probe of
//! about 1.5 places; a path component is a binary search over the directory's name column.

use std::{
    marker::PhantomData,
    mem::{MaybeUninit, needs_drop},
    ptr,
    sync::Arc,
};

use blaze_ids::ContentId;
use blaze_reclaim::{Guard, Limbo};

use crate::{
    Mode,
    sync::{
        Mutex, PoisonError,
        atomic::{AtomicPtr, AtomicU32, AtomicUsize, Ordering},
    },
};

/// Chunk `k` of a column holds `FIRST << k` values from offset `FIRST * (2^k - 1)`, so an
/// offset's chunk is arithmetic and 25 chunks cover every `u32`.
const FIRST: usize = 1 << 8;
const CHUNKS: usize = 25;

fn chunk_of(at: usize) -> (usize, usize) {
    let biased = at / FIRST + 1;
    let chunk = (usize::BITS - 1 - biased.leading_zeros()) as usize;
    (chunk, at - FIRST * ((1 << chunk) - 1))
}

const fn chunk_start(chunk: usize) -> usize {
    FIRST * ((1 << chunk) - 1)
}

/// The first offset at or after `cursor` where `len` values fit in one chunk, so they can be
/// read as one slice. Skipping to the next chunk wastes at most that chunk's tail.
fn run(cursor: u32, len: u32) -> u32 {
    let mut start = cursor as usize;
    loop {
        let (chunk, place) = chunk_of(start);
        if place + len as usize <= FIRST << chunk {
            assert!(start + len as usize <= u32::MAX as usize, "a column of 2^32 values");
            return start as u32;
        }
        start = chunk_start(chunk + 1);
    }
}

/// On a 128-byte line of its own (the pair arm64 and x86 prefetch together), so the writer's
/// length stores do not take the line holding the chunk pointers from readers.
#[repr(align(128))]
struct Padded<T>(T);

/// An append-only column whose values never move: chunks are allocated as it grows and kept
/// until it is dropped. Readers see the prefix below `len`. Its values are `Send` and `Sync`:
/// pushed on the writer's thread through `&self`, read on any, dropped on whichever drops it.
struct Column<T: Send + Sync> {
    len: Padded<AtomicU32>,
    chunks: [AtomicPtr<T>; CHUNKS],
    of: PhantomData<T>,
}

impl<T: Send + Sync> Column<T> {
    fn new() -> Column<T> {
        Column {
            len: Padded(AtomicU32::new(0)),
            chunks: std::array::from_fn(|_| AtomicPtr::new(ptr::null_mut())),
            of: PhantomData,
        }
    }

    /// How many values are written and readable.
    fn len(&self) -> u32 {
        self.len.0.load(Ordering::Acquire)
    }

    /// The value at `at`; panics past what is written.
    fn get(&self, at: u32) -> &T {
        let len = self.len();
        assert!(at < len, "value {at} of a column of {len}");
        let (chunk, place) = chunk_of(at as usize);
        let base = self.chunks[chunk].load(Ordering::Acquire);
        // SAFETY: `at < len`, and `len` was stored with release after `at`'s chunk was
        // allocated (a block of `FIRST << chunk` values, which `place` is below) and its value
        // written: the offset stays in that block.
        let value = unsafe { base.add(place) };
        // SAFETY: as above the value is initialized; it is never written again and its chunk
        // is freed only when the column drops, which `&self` outlives.
        unsafe { &*value }
    }

    /// The chunk, allocated if it is not yet.
    fn chunk(&self, chunk: usize) -> *mut T {
        let at = self.chunks[chunk].load(Ordering::Acquire);
        if !at.is_null() {
            return at;
        }
        let fresh = Box::into_raw(Box::<[T]>::new_uninit_slice(FIRST << chunk)).cast::<T>();
        self.chunks[chunk].store(fresh, Ordering::Release);
        fresh
    }

    /// Writes `value` at the end and publishes it; returns its offset.
    ///
    /// # Safety
    ///
    /// The caller is the column's one writer: no other thread pushes to it at the same time.
    /// Otherwise two pushes write the same slot and one value is lost or torn.
    unsafe fn push(&self, value: T) -> u32 {
        let at = self.len.0.load(Ordering::Relaxed);
        assert!(at < u32::MAX, "a column of 2^32 values");
        let (chunk, place) = chunk_of(at as usize);
        let base = self.chunk(chunk);
        // SAFETY: `base` is a block of `FIRST << chunk` values and `place` is below that.
        let slot = unsafe { base.add(place) };
        // SAFETY: the slot is at `len`, past what any reader may read, and the caller is the
        // only writer, so nothing else touches it.
        unsafe { slot.write(value) };
        self.len.0.store(at + 1, Ordering::Release);
        at
    }
}

impl<T: Copy + Send + Sync> Column<T> {
    /// The `len` values from `start`; panics past what is written or across chunks.
    fn slice(&self, start: u32, len: u32) -> &[T] {
        if len == 0 {
            return &[];
        }
        let written = self.len();
        assert!(
            start.checked_add(len).is_some_and(|end| end <= written),
            "values {start}..+{len} of a column of {written}"
        );
        let (chunk, place) = chunk_of(start as usize);
        assert!(place + len as usize <= FIRST << chunk, "a run across chunks");
        let base = self.chunks[chunk].load(Ordering::Acquire);
        // SAFETY: the run is within one chunk, as checked, and its chunk is allocated (it was
        // before `written` was stored).
        let first = unsafe { base.add(place) };
        // SAFETY: the `len` values from `first` are in one block, written before `written`
        // was stored with release and never written again; the block lives as long as `&self`.
        unsafe { std::slice::from_raw_parts(first, len as usize) }
    }

    /// Pushes the `len` values of `values` in one chunk, padding with `filler` up to where
    /// they fit; returns where they start.
    ///
    /// # Safety
    ///
    /// As [`Column::push`].
    unsafe fn push_run(&self, len: u32, filler: T, values: impl IntoIterator<Item = T>) -> u32 {
        let start = run(self.len.0.load(Ordering::Relaxed), len);
        while self.len.0.load(Ordering::Relaxed) < start {
            // SAFETY: the caller is the one writer.
            unsafe { self.push(filler) };
        }
        let mut pushed = 0;
        for value in values {
            // SAFETY: as above.
            unsafe { self.push(value) };
            pushed += 1;
        }
        assert_eq!(pushed, len, "a run of the length it said");
        start
    }
}

impl<T: Send + Sync> Drop for Column<T> {
    fn drop(&mut self) {
        let len = *self.len.0.get_mut() as usize;
        for (chunk, base) in self.chunks.iter_mut().enumerate() {
            let base = *base.get_mut();
            if base.is_null() {
                continue;
            }
            let size = FIRST << chunk;
            if needs_drop::<T>() {
                let written = len.saturating_sub(chunk_start(chunk)).min(size);
                // SAFETY: chunks fill in order, so the first `written` values of this one are
                // the column's values below `len`: initialized, and dropped once, here.
                unsafe { ptr::drop_in_place(ptr::slice_from_raw_parts_mut(base, written)) };
            }
            let all = ptr::slice_from_raw_parts_mut(base.cast::<MaybeUninit<T>>(), size);
            // SAFETY: `chunk` allocated `base` as a box of exactly `size` values, and nothing
            // reads it any more (`&mut self`).
            drop(unsafe { Box::from_raw(all) });
        }
    }
}

/// A node's place in the columns. 8 bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub(crate) struct Shape {
    /// A directory's first entry; a file's place in `blobs`.
    pub(crate) start: u32,
    /// A directory's number of entries; [`FILE`] for a file.
    pub(crate) len: u32,
}

const _: () = assert!(size_of::<Shape>() == 8);

pub(crate) const FILE: u32 = u32::MAX;

impl Shape {
    pub(crate) fn is_dir(self) -> bool {
        self.len != FILE
    }
}

/// An entry's mode byte: a file's [`Mode`], or [`DIR`].
pub(crate) const DIR: u8 = 3;

pub(crate) const fn mode_byte(mode: Mode) -> u8 {
    match mode {
        Mode::Regular => 0,
        Mode::Executable => 1,
        Mode::Symlink => 2,
    }
}

/// The mode an entry's byte gives: [`Mode::Regular`] for a directory, as the tree says.
pub(crate) const fn mode_of(byte: u8) -> Mode {
    match byte {
        1 => Mode::Executable,
        2 => Mode::Symlink,
        _ => Mode::Regular,
    }
}

/// A directory's entries, sorted by name bytes: three columns of one range.
#[derive(Clone, Copy)]
pub(crate) struct Entries<'a> {
    pub(crate) names: &'a [u32],
    pub(crate) nodes: &'a [u32],
    pub(crate) modes: &'a [u8],
}

impl Entries<'_> {
    pub(crate) fn len(&self) -> usize {
        self.names.len()
    }

    pub(crate) fn is_dir(&self, at: usize) -> bool {
        self.modes[at] == DIR
    }

    /// The entry named `name`, by binary search of the name column.
    pub(crate) fn find(&self, names: &Names, name: &[u8]) -> Option<usize> {
        self.names.binary_search_by(|&it| names.get(it).cmp(name)).ok()
    }
}

/// Every name, interned once: a span of one byte column each.
pub(crate) struct Names {
    spans: Column<Span>,
    bytes: Column<u8>,
}

/// 8 bytes.
#[derive(Clone, Copy)]
#[repr(C)]
struct Span {
    start: u32,
    len: u32,
}

const _: () = assert!(size_of::<Span>() == 8);

impl Names {
    fn new() -> Names {
        Names { spans: Column::new(), bytes: Column::new() }
    }

    /// How many names there are.
    pub(crate) fn len(&self) -> u32 {
        self.spans.len()
    }

    /// The bytes of `name`; panics for a name not interned.
    pub(crate) fn get(&self, name: u32) -> &[u8] {
        let span = *self.spans.get(name);
        self.bytes.slice(span.start, span.len)
    }

    /// `name` as the `&str` it was interned from.
    pub(crate) fn str(&self, name: u32) -> &str {
        let bytes = self.get(name);
        debug_assert!(std::str::from_utf8(bytes).is_ok());
        // SAFETY: spans are pushed only by `intern`, each right after the bytes of a `&str`,
        // and cover exactly them; pushed values never change. So these bytes are valid UTF-8.
        unsafe { std::str::from_utf8_unchecked(bytes) }
    }

    /// Interns `name`, new; returns its number.
    ///
    /// # Safety
    ///
    /// As [`Column::push`]: the caller is the one writer.
    unsafe fn intern(&self, name: &str) -> u32 {
        let len = u32::try_from(name.len()).expect("a name under 4 GiB");
        // SAFETY: the caller is the one writer.
        let start = unsafe { self.bytes.push_run(len, 0, name.bytes()) };
        // SAFETY: as above.
        unsafe { self.spans.push(Span { start, len }) }
    }
}

/// Level `k` of the index has `PLACES << k` places; a place holds a node plus one, 0 empty.
const PLACES: usize = 64;
const LEVELS: usize = 28;

struct Index {
    level: AtomicUsize,
    levels: [AtomicPtr<AtomicU32>; LEVELS],
}

fn places(len: usize) -> *mut AtomicU32 {
    Box::into_raw((0..len).map(|_| AtomicU32::new(0)).collect::<Box<[AtomicU32]>>()).cast()
}

/// Where `id`'s probe starts: its own first 64 bits, uniform as a BLAKE3 hash is.
fn home(id: &ContentId) -> usize {
    let mut word = [0; 8];
    word.copy_from_slice(&id.0[..8]);
    u64::from_le_bytes(word) as usize
}

impl Index {
    /// An index whose first level holds `count` nodes at most half full.
    fn new(count: usize) -> Index {
        let mut level = 0;
        while (count + 1) * 2 > PLACES << level {
            level += 1;
        }
        let index = Index {
            level: AtomicUsize::new(level),
            levels: std::array::from_fn(|_| AtomicPtr::new(ptr::null_mut())),
        };
        index.levels[level].store(places(PLACES << level), Ordering::Release);
        index
    }

    fn places(&self, level: usize) -> &[AtomicU32] {
        let at = self.levels[level].load(Ordering::Acquire);
        assert!(!at.is_null(), "index level {level} is not allocated");
        // SAFETY: a level is allocated by `places` as `PLACES << level` atomics before
        // anything names it, never moved, and freed only when the index drops, which `&self`
        // outlives.
        unsafe { std::slice::from_raw_parts(at, PLACES << level) }
    }
}

impl Drop for Index {
    fn drop(&mut self) {
        for (level, at) in self.levels.iter_mut().enumerate() {
            let at = *at.get_mut();
            if !at.is_null() {
                let all = ptr::slice_from_raw_parts_mut(at, PLACES << level);
                // SAFETY: allocated by `places` with exactly this many places; nothing reads
                // it any more (`&mut self`).
                drop(unsafe { Box::from_raw(all) });
            }
        }
    }
}

/// One generation of nodes.
pub(crate) struct Table {
    ids: Column<ContentId>,
    shapes: Column<Shape>,
    blobs: Column<Arc<[u8]>>,
    entry_names: Column<u32>,
    entry_nodes: Column<u32>,
    entry_modes: Column<u8>,
    index: Index,
}

impl Table {
    /// A table whose index holds `count` nodes without growing.
    fn new(count: usize) -> Table {
        Table {
            ids: Column::new(),
            shapes: Column::new(),
            blobs: Column::new(),
            entry_names: Column::new(),
            entry_nodes: Column::new(),
            entry_modes: Column::new(),
            index: Index::new(count),
        }
    }

    /// How many nodes it holds.
    pub(crate) fn len(&self) -> u32 {
        self.ids.len()
    }

    /// The node whose content ID is `id`.
    pub(crate) fn find(&self, id: &ContentId) -> Option<u32> {
        self.probe(id).ok()
    }

    /// The node whose content ID is `id`; else its free place in the level in use.
    fn probe(&self, id: &ContentId) -> Result<u32, usize> {
        let places = self.index.places(self.index.level.load(Ordering::Acquire));
        Self::probe_in(&self.ids, places, id)
    }

    fn probe_in(
        ids: &Column<ContentId>,
        places: &[AtomicU32],
        id: &ContentId,
    ) -> Result<u32, usize> {
        let mask = places.len() - 1;
        let mut at = home(id) & mask;
        loop {
            match places[at].load(Ordering::Acquire) {
                0 => return Err(at),
                plus_one if ids.get(plus_one - 1) == id => return Ok(plus_one - 1),
                _ => at = (at + 1) & mask,
            }
        }
    }

    pub(crate) fn id(&self, node: u32) -> &ContentId {
        self.ids.get(node)
    }

    pub(crate) fn shape(&self, node: u32) -> Shape {
        *self.shapes.get(node)
    }

    /// The entries of a directory's shape; panics for a file's.
    pub(crate) fn entries(&self, shape: Shape) -> Entries<'_> {
        assert!(shape.is_dir(), "a file has no entries");
        Entries {
            names: self.entry_names.slice(shape.start, shape.len),
            nodes: self.entry_nodes.slice(shape.start, shape.len),
            modes: self.entry_modes.slice(shape.start, shape.len),
        }
    }

    /// The bytes of a file's shape; panics for a directory's.
    pub(crate) fn blob(&self, shape: Shape) -> &Arc<[u8]> {
        assert!(!shape.is_dir(), "a directory has no bytes");
        self.blobs.get(shape.start)
    }

    /// The node of the file `id` with `bytes`, added if it is new.
    ///
    /// # Safety
    ///
    /// As [`Column::push`]: the caller is the table's one writer.
    unsafe fn add_file(&self, id: ContentId, bytes: Arc<[u8]>) -> u32 {
        let place = match self.probe(&id) {
            Ok(node) => return node,
            Err(place) => place,
        };
        // SAFETY: the caller is the one writer.
        let blob = unsafe { self.blobs.push(bytes) };
        // SAFETY: as above.
        unsafe { self.add(id, Shape { start: blob, len: FILE }, place) }
    }

    /// The node of the directory `id` with `entries` (name, node, mode byte), added if it is
    /// new.
    ///
    /// # Safety
    ///
    /// As [`Column::push`].
    unsafe fn add_dir(&self, id: ContentId, entries: &[(u32, u32, u8)]) -> u32 {
        let place = match self.probe(&id) {
            Ok(node) => return node,
            Err(place) => place,
        };
        let len = u32::try_from(entries.len()).ok().filter(|&it| it != FILE);
        let len = len.expect("fewer than 2^32 - 1 entries");
        // SAFETY: the caller is the one writer.
        let start = unsafe { self.entry_names.push_run(len, 0, entries.iter().map(|it| it.0)) };
        // SAFETY: as above.
        let nodes = unsafe { self.entry_nodes.push_run(len, 0, entries.iter().map(|it| it.1)) };
        // SAFETY: as above.
        let modes = unsafe { self.entry_modes.push_run(len, 0, entries.iter().map(|it| it.2)) };
        assert!(start == nodes && start == modes, "the entry columns are pushed together");
        // SAFETY: as above.
        unsafe { self.add(id, Shape { start, len }, place) }
    }

    /// Adds a node whose blob or entries are pushed, then indexes it at `place` (from
    /// [`Table::probe`]), which publishes it.
    ///
    /// # Safety
    ///
    /// As [`Column::push`].
    unsafe fn add(&self, id: ContentId, shape: Shape, place: usize) -> u32 {
        // SAFETY: the caller is the one writer.
        let node = unsafe { self.shapes.push(shape) };
        // SAFETY: as above.
        let at = unsafe { self.ids.push(id) };
        assert_eq!(node, at, "the node columns are pushed together");
        self.index(&id, node, place);
        node
    }

    /// Stores `node` at `place` of the level in use, after growing the index if `node` would
    /// fill it past half. The one writer's.
    fn index(&self, id: &ContentId, node: u32, place: usize) {
        let mut level = self.index.level.load(Ordering::Relaxed);
        let mut place = place;
        if (node as usize + 1) * 2 > PLACES << level {
            level = self.grow(level);
            let Err(free) = Self::probe_in(&self.ids, self.index.places(level), id) else {
                unreachable!("a node indexed twice")
            };
            place = free;
        }
        self.index.places(level)[place].store(node + 1, Ordering::Release);
    }

    /// Copies every place into a level twice the size and makes it the one in use. A reader
    /// probing the old level still finds every node indexed before; the old level is freed
    /// with the table.
    fn grow(&self, level: usize) -> usize {
        assert!(level + 1 < LEVELS, "an index of 2^32 nodes");
        let next = level + 1;
        self.index.levels[next].store(places(PLACES << next), Ordering::Release);
        let new = self.index.places(next);
        let mask = new.len() - 1;
        for place in self.index.places(level) {
            let plus_one = place.load(Ordering::Relaxed);
            if plus_one == 0 {
                continue;
            }
            let mut at = home(self.ids.get(plus_one - 1)) & mask;
            while new[at].load(Ordering::Relaxed) != 0 {
                at = (at + 1) & mask;
            }
            new[at].store(plus_one, Ordering::Relaxed);
        }
        self.index.level.store(next, Ordering::Release);
        next
    }
}

/// What the tree and every reader share. Made only by [`Writer::new`].
pub(crate) struct Shared {
    table: AtomicPtr<Table>,
    names: Names,
    /// The tables replaced by collections, until no reader's task can be in them. Locked by
    /// the writer alone (readers never touch it), so never contended.
    retired: Mutex<Limbo>,
}

impl Shared {
    /// The tree as it is now, for reading from this thread.
    pub(crate) fn read(&self) -> Snapshot<'_> {
        let task = blaze_reclaim::enter();
        let table = self.table.load(Ordering::Acquire);
        // SAFETY: `table` is the published `Box`. A replaced one is freed only after a grace
        // started once it was unlinked has passed, which waits for this task (entered before
        // the load) to end; the `Snapshot` holds the task, and lends the table only for as
        // long as it lives.
        let table = unsafe { &*table };
        Snapshot { table, names: &self.names, _task: task }
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        let table = *self.table.get_mut();
        // SAFETY: the last owner: no reader or writer is left, so nobody reads the published
        // `Box` (the limbo drops the retired tables with it, for the same reason).
        drop(unsafe { Box::from_raw(table) });
    }
}

/// The tree as it was when [`Shared::read`] was called. Not `Send`: its task is the thread's.
pub(crate) struct Snapshot<'a> {
    table: &'a Table,
    names: &'a Names,
    _task: Guard,
}

impl Snapshot<'_> {
    pub(crate) fn table(&self) -> &Table {
        self.table
    }

    pub(crate) fn names(&self) -> &Names {
        self.names
    }
}

/// The one writer of a [`Shared`]: made with it and never cloned, so every push goes through
/// its `&mut self`.
pub(crate) struct Writer {
    shared: Arc<Shared>,
}

impl Writer {
    pub(crate) fn new() -> Writer {
        let table = Box::into_raw(Box::new(Table::new(0)));
        let shared = Shared {
            table: AtomicPtr::new(table),
            names: Names::new(),
            retired: Mutex::new(Limbo::new()),
        };
        Writer { shared: Arc::new(shared) }
    }

    /// A handle for readers.
    pub(crate) fn shared(&self) -> Arc<Shared> {
        self.shared.clone()
    }

    /// The table in use.
    pub(crate) fn table(&self) -> &Table {
        let table = self.shared.table.load(Ordering::Acquire);
        // SAFETY: only `replace`, through `&mut self`, unlinks the published table, so it
        // outlives this borrow of `self`.
        unsafe { &*table }
    }

    pub(crate) fn names(&self) -> &Names {
        &self.shared.names
    }

    /// Interns `name`, which is not interned yet; returns its number.
    pub(crate) fn intern(&mut self, name: &str) -> u32 {
        // SAFETY: `&mut self`: the one writer.
        unsafe { self.shared.names.intern(name) }
    }

    /// The node of the file `id` with `bytes`, added if it is new.
    pub(crate) fn file(&mut self, id: ContentId, bytes: Arc<[u8]>) -> u32 {
        // SAFETY: `&mut self`: the one writer.
        unsafe { self.table().add_file(id, bytes) }
    }

    /// The node of the directory `id` with `entries` (name, node, mode byte), added if new.
    pub(crate) fn dir(&mut self, id: ContentId, entries: &[(u32, u32, u8)]) -> u32 {
        // SAFETY: `&mut self`: the one writer.
        unsafe { self.table().add_dir(id, entries) }
    }

    /// Publishes `next` in place of the table in use, retiring that one; returns how many
    /// retired tables are not freed yet.
    pub(crate) fn replace(&mut self, next: Building) -> usize {
        let next = Box::into_raw(Box::new(next.0));
        let old = self.shared.table.swap(next, Ordering::AcqRel);
        // SAFETY: `old` was the published `Box`, now unlinked; the limbo becomes its only
        // owner and frees it once every task that could have loaded it has ended.
        let old = unsafe { Box::from_raw(old) };
        let mut retired = self.shared.retired.lock().unwrap_or_else(PoisonError::into_inner);
        retired.retire(old);
        retired.collect();
        retired.len()
    }

    /// Frees each retired table no reader's task can be in; returns how many are left.
    pub(crate) fn reclaim(&mut self) -> usize {
        let mut retired = self.shared.retired.lock().unwrap_or_else(PoisonError::into_inner);
        retired.collect();
        retired.len()
    }
}

/// The next generation, filled by a collection before [`Writer::replace`] publishes it.
pub(crate) struct Building(Table);

impl Building {
    /// A table for `count` nodes.
    pub(crate) fn new(count: usize) -> Building {
        Building(Table::new(count))
    }

    pub(crate) fn file(&mut self, id: ContentId, bytes: Arc<[u8]>) -> u32 {
        // SAFETY: `&mut self` on a table nobody else has yet.
        unsafe { self.0.add_file(id, bytes) }
    }

    pub(crate) fn dir(&mut self, id: ContentId, entries: &[(u32, u32, u8)]) -> u32 {
        // SAFETY: as above.
        unsafe { self.0.add_dir(id, entries) }
    }
}

#[cfg(test)]
mod tests;

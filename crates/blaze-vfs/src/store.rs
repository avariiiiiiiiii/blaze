//! The tree's nodes, stored once and read from any thread without a lock
//! (docs/v2/plan/design/concurrency.md, docs/v2/plan/design/data.md).
//!
//! - A [`Table`] is one generation of nodes: append-only columns indexed by a node's `u32`, in
//!   chunks that never move, and an open-addressed index from content ID to node, probed by the
//!   ID's own bits (never hashed again) and grown by adding a level. The
//!   [`Tree`](crate::tree::Tree) is its one writer. A value is written before anything that
//!   names it is published with a release store (a node before its index place, children
//!   before their directory), so a reader that reached a node through acquire loads sees all of
//!   it, and everything under it.
//! - A collection builds the next generation and publishes it with one release store
//!   ([`Shared::replace`]); the old one is freed once no reader's task can be in it
//!   ([`blaze_reclaim`]).
//! - Names are interned once for the tree's life, as spans of one byte column.
//!
//! Costs: a node is 32 bytes of ID plus an 8-byte [`Shape`], and 4 bytes of index place at
//! most half full; a directory entry is 9 bytes (name, node, mode byte) in three columns; a
//! file's bytes are one `Arc<[u8]>` shared with every reader. Finding a root is one probe of
//! about 1.5 places; a path component is a binary search over the directory's name column.

use std::{
    marker::PhantomData,
    mem::{MaybeUninit, needs_drop},
    ptr,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicPtr, AtomicU32, AtomicUsize, Ordering},
    },
};

use blaze_ids::ContentId;
use blaze_reclaim::Limbo;

use crate::Mode;

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
pub(crate) fn run(cursor: u32, len: u32) -> u32 {
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

/// An append-only column whose values never move: chunks are allocated as it grows and kept
/// until it is dropped.
pub(crate) struct Chunked<T> {
    chunks: [AtomicPtr<T>; CHUNKS],
    /// How many values were written from offset 0, for a column whose values need dropping
    /// (such a column is written without gaps).
    written: AtomicUsize,
    of: PhantomData<T>,
}

impl<T> Chunked<T> {
    pub(crate) fn new() -> Chunked<T> {
        Chunked {
            chunks: std::array::from_fn(|_| AtomicPtr::new(ptr::null_mut())),
            written: AtomicUsize::new(0),
            of: PhantomData,
        }
    }

    /// The chunk, allocated if it is not yet. The writer's only.
    fn chunk(&self, chunk: usize) -> *mut T {
        let at = self.chunks[chunk].load(Ordering::Acquire);
        if !at.is_null() {
            return at;
        }
        let fresh = Box::into_raw(Box::<[T]>::new_uninit_slice(FIRST << chunk)).cast::<T>();
        self.chunks[chunk].store(fresh, Ordering::Release);
        fresh
    }

    /// Writes `value` at `at`.
    ///
    /// # Safety
    ///
    /// Only the column's one writer calls it, at an offset nothing published names yet; a
    /// column whose values need dropping is written at `0, 1, 2, ...` in turn.
    pub(crate) unsafe fn write(&self, at: u32, value: T) {
        let (chunk, place) = chunk_of(at as usize);
        // SAFETY: the chunk holds `FIRST << chunk` values, `place` is below that, and no reader
        // reads `at` before something naming it is published.
        unsafe { self.chunk(chunk).add(place).write(value) };
        if needs_drop::<T>() {
            self.written.store(at as usize + 1, Ordering::Relaxed);
        }
    }

    /// The value at `at`.
    ///
    /// # Safety
    ///
    /// `at` was written by this thread, or before a release store this thread's acquire load
    /// saw; and the column is alive for as long as the reference is held.
    pub(crate) unsafe fn get(&self, at: u32) -> &T {
        let (chunk, place) = chunk_of(at as usize);
        // SAFETY: as the caller promises, the place is written and its chunk allocated.
        unsafe { &*self.chunks[chunk].load(Ordering::Acquire).add(place) }
    }

    /// The `len` values from `start`.
    ///
    /// # Safety
    ///
    /// As [`Chunked::get`] for each of them, and [`run`] placed them in one chunk.
    pub(crate) unsafe fn slice(&self, start: u32, len: u32) -> &[T] {
        if len == 0 {
            return &[];
        }
        let (chunk, place) = chunk_of(start as usize);
        // SAFETY: as the caller promises, the run is written and within one chunk.
        unsafe {
            std::slice::from_raw_parts(self.chunks[chunk].load(Ordering::Acquire).add(place), len as usize)
        }
    }
}

impl<T: Copy> Chunked<T> {
    /// Writes `values` from `start`, which [`run`] placed in one chunk.
    ///
    /// # Safety
    ///
    /// As [`Chunked::write`] for each offset.
    pub(crate) unsafe fn write_run(&self, start: u32, values: &[T]) {
        if values.is_empty() {
            return;
        }
        let (chunk, place) = chunk_of(start as usize);
        debug_assert!(place + values.len() <= FIRST << chunk, "a run across chunks");
        // SAFETY: the run is within one chunk and nothing published names it yet.
        unsafe { ptr::copy_nonoverlapping(values.as_ptr(), self.chunk(chunk).add(place), values.len()) };
    }
}

impl<T> Drop for Chunked<T> {
    fn drop(&mut self) {
        if needs_drop::<T>() {
            for at in 0..*self.written.get_mut() {
                let (chunk, place) = chunk_of(at);
                // SAFETY: every offset below `written` holds a value, dropped once here.
                unsafe { ptr::drop_in_place(self.chunks[chunk].get_mut().add(place)) };
            }
        }
        for (chunk, at) in self.chunks.iter_mut().enumerate() {
            let at = *at.get_mut();
            if !at.is_null() {
                let all = ptr::slice_from_raw_parts_mut(at.cast::<MaybeUninit<T>>(), FIRST << chunk);
                // SAFETY: allocated by `chunk` as a box of exactly this many values.
                drop(unsafe { Box::from_raw(all) });
            }
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

impl<'a> Entries<'a> {
    pub(crate) fn len(&self) -> usize {
        self.names.len()
    }

    pub(crate) fn is_dir(&self, at: usize) -> bool {
        self.modes[at] == DIR
    }

    /// The entry named `name`, by binary search of the name column.
    pub(crate) fn find(&self, names: &Names, name: &[u8]) -> Option<usize> {
        // SAFETY: an entry's name was written before its directory was published.
        self.names.binary_search_by(|&it| unsafe { names.get(it) }.cmp(name)).ok()
    }
}

/// Every name, interned once: a span of one byte column each.
pub(crate) struct Names {
    pub(crate) spans: Chunked<Span>,
    pub(crate) bytes: Chunked<u8>,
}

/// 8 bytes.
#[derive(Clone, Copy)]
#[repr(C)]
pub(crate) struct Span {
    pub(crate) start: u32,
    pub(crate) len: u32,
}

const _: () = assert!(size_of::<Span>() == 8);

impl Names {
    /// The bytes of `name`.
    ///
    /// # Safety
    ///
    /// `name` was interned by this thread, or before a release store this thread's acquire
    /// load saw.
    pub(crate) unsafe fn get(&self, name: u32) -> &[u8] {
        // SAFETY: as the caller promises; a name's bytes are written before its span.
        unsafe {
            let span = *self.spans.get(name);
            self.bytes.slice(span.start, span.len)
        }
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
        // SAFETY: a level is allocated before `level` names it, and freed only with the index.
        unsafe { std::slice::from_raw_parts(self.levels[level].load(Ordering::Acquire), PLACES << level) }
    }
}

impl Drop for Index {
    fn drop(&mut self) {
        for (level, at) in self.levels.iter_mut().enumerate() {
            let at = *at.get_mut();
            if !at.is_null() {
                // SAFETY: allocated by `places` with exactly this many places.
                drop(unsafe { Box::from_raw(ptr::slice_from_raw_parts_mut(at, PLACES << level)) });
            }
        }
    }
}

/// One generation of nodes.
pub(crate) struct Table {
    pub(crate) ids: Chunked<ContentId>,
    pub(crate) shapes: Chunked<Shape>,
    pub(crate) blobs: Chunked<Arc<[u8]>>,
    pub(crate) entry_names: Chunked<u32>,
    pub(crate) entry_nodes: Chunked<u32>,
    pub(crate) entry_modes: Chunked<u8>,
    index: Index,
}

impl Table {
    /// A table whose index holds `count` nodes without growing.
    pub(crate) fn new(count: usize) -> Table {
        Table {
            ids: Chunked::new(),
            shapes: Chunked::new(),
            blobs: Chunked::new(),
            entry_names: Chunked::new(),
            entry_nodes: Chunked::new(),
            entry_modes: Chunked::new(),
            index: Index::new(count),
        }
    }

    /// The node whose content ID is `id`; else the place to index it at in the level in use.
    pub(crate) fn find(&self, id: &ContentId) -> Result<u32, usize> {
        Self::probe(&self.ids, self.index.places(self.index.level.load(Ordering::Acquire)), id)
    }

    fn probe(ids: &Chunked<ContentId>, places: &[AtomicU32], id: &ContentId) -> Result<u32, usize> {
        let mask = places.len() - 1;
        let mut at = home(id) & mask;
        loop {
            match places[at].load(Ordering::Acquire) {
                0 => return Err(at),
                plus_one => {
                    // SAFETY: a node is written before its place is stored.
                    if unsafe { ids.get(plus_one - 1) } == id {
                        return Ok(plus_one - 1);
                    }
                }
            }
            at = (at + 1) & mask;
        }
    }

    /// Indexes `node`, written already, at the `place` [`Table::find`] gave for its ID; `count`
    /// is how many nodes the index holds. The writer's only.
    pub(crate) fn index(&self, id: &ContentId, node: u32, place: usize, count: usize) {
        let mut level = self.index.level.load(Ordering::Relaxed);
        let mut place = place;
        if (count + 1) * 2 > PLACES << level {
            level = self.grow(level);
            let Err(free) = Self::probe(&self.ids, self.index.places(level), id) else {
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
            // SAFETY: indexed nodes are written.
            let mut at = home(unsafe { self.ids.get(plus_one - 1) }) & mask;
            while new[at].load(Ordering::Relaxed) != 0 {
                at = (at + 1) & mask;
            }
            new[at].store(plus_one, Ordering::Relaxed);
        }
        self.index.level.store(next, Ordering::Release);
        next
    }

    /// # Safety
    ///
    /// `node` was reached through this table (its index or a directory's entries), or written
    /// by this thread.
    pub(crate) unsafe fn id(&self, node: u32) -> &ContentId {
        // SAFETY: as the caller promises.
        unsafe { self.ids.get(node) }
    }

    /// # Safety
    ///
    /// As [`Table::id`].
    pub(crate) unsafe fn shape(&self, node: u32) -> Shape {
        // SAFETY: as the caller promises.
        unsafe { *self.shapes.get(node) }
    }

    /// The entries of a directory's shape.
    ///
    /// # Safety
    ///
    /// `shape` is a directory's, read as [`Table::shape`] allows.
    pub(crate) unsafe fn entries(&self, shape: Shape) -> Entries<'_> {
        // SAFETY: a directory's entries are written, in one run, before it is.
        unsafe {
            Entries {
                names: self.entry_names.slice(shape.start, shape.len),
                nodes: self.entry_nodes.slice(shape.start, shape.len),
                modes: self.entry_modes.slice(shape.start, shape.len),
            }
        }
    }

    /// The bytes of a file's shape.
    ///
    /// # Safety
    ///
    /// `shape` is a file's, read as [`Table::shape`] allows.
    pub(crate) unsafe fn blob(&self, shape: Shape) -> &Arc<[u8]> {
        // SAFETY: a file's bytes are written before it is.
        unsafe { self.blobs.get(shape.start) }
    }
}

/// What the tree and every reader share.
pub(crate) struct Shared {
    table: AtomicPtr<Table>,
    pub(crate) names: Names,
    /// The tables replaced by collections, until no reader's task can be in them. Locked by
    /// the tree alone (readers never touch it), so never contended.
    retired: Mutex<Limbo>,
}

impl Shared {
    pub(crate) fn new(table: Table) -> Shared {
        Shared {
            table: AtomicPtr::new(Box::into_raw(Box::new(table))),
            names: Names { spans: Chunked::new(), bytes: Chunked::new() },
            retired: Mutex::new(Limbo::new()),
        }
    }

    /// The table in use.
    ///
    /// # Safety
    ///
    /// The caller is the writer (which alone replaces it, through `&mut`), or is in a task
    /// ([`blaze_reclaim::enter`]) for as long as it holds the reference.
    pub(crate) unsafe fn table(&self) -> &Table {
        // SAFETY: as the caller promises: a replaced table is freed only after the grace
        // started when it was unlinked has passed, so after every task that could have loaded
        // it has ended.
        unsafe { &*self.table.load(Ordering::Acquire) }
    }

    /// Publishes `next` in place of the table in use, retiring that one. Returns how many
    /// retired tables are not freed yet. The writer's only.
    pub(crate) fn replace(&self, next: Table) -> usize {
        let old = self.table.swap(Box::into_raw(Box::new(next)), Ordering::AcqRel);
        // SAFETY: `old` was the published `Box`, now unlinked; the limbo is its only owner.
        let old = unsafe { Box::from_raw(old) };
        let mut retired = self.retired.lock().unwrap_or_else(PoisonError::into_inner);
        retired.retire(old);
        retired.collect();
        retired.len()
    }

    /// Frees each retired table no reader's task can be in; returns how many are left.
    pub(crate) fn reclaim(&self) -> usize {
        let mut retired = self.retired.lock().unwrap_or_else(PoisonError::into_inner);
        retired.collect();
        retired.len()
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        // SAFETY: the last owner: no reader handle is left, so nobody is reading (the limbo
        // drops the retired tables with it, for the same reason).
        drop(unsafe { Box::from_raw(*self.table.get_mut()) });
    }
}

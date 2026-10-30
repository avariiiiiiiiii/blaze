//! The store under every interner: values in append-only arenas sharded by hash.
//!
//! A value is written once into a chunk of its shard's arena and never moves or is freed: the
//! stores are statics, so a pointer to a value is good for the life of the process. Memory is
//! bounded by the database rebuilding what it keeps (docs/v2/db.md), not by counting references.
//!
//! A shard's table maps a hash to the values with it. Finding a value takes no lock; interning
//! one takes its shard's lock only to look again and insert. A table grows by copying into one
//! twice its size, published once full; the tables it outgrew are kept, as a reader may be
//! probing one, so a shard's tables cost at most twice its last.

use std::{
    alloc::{self, Layout},
    ptr::{self, NonNull},
    sync::{
        Mutex, MutexGuard, PoisonError,
        atomic::{AtomicPtr, AtomicU64, Ordering},
    },
};

const SHARD_BITS: u32 = 6;
const SHARDS: usize = 1 << SHARD_BITS;
const FIRST_TABLE: usize = 16;

const CHUNK_ALIGN: usize = 64;
const FIRST_CHUNK: usize = 1024;
const LAST_CHUNK: usize = 256 * 1024;

pub(crate) struct Store {
    shards: Box<[Shard]>,
}

struct Shard {
    /// The table in use, null until the first insert. Only the holder of `state` stores it.
    table: AtomicPtr<Table>,
    state: Mutex<State>,
}

struct State {
    /// The values in the shard.
    len: usize,
    bump: Bump,
}

struct Table {
    slots: Box<[Slot]>,
}

/// Empty while `value` is null. Written once, by the holder of the shard's lock: `hash` first,
/// then `value` with `Release`, so a reader that loads a non-null `value` with `Acquire` sees
/// `hash` and the value's bytes.
struct Slot {
    hash: AtomicU64,
    value: AtomicPtr<u8>,
}

impl Table {
    fn new(len: usize) -> Table {
        debug_assert!(len.is_power_of_two());
        Table {
            slots: (0..len)
                .map(|_| Slot { hash: AtomicU64::new(0), value: AtomicPtr::new(ptr::null_mut()) })
                .collect(),
        }
    }

    /// The value with `hash` that `matches`, probing from the hash's home until an empty slot.
    fn find(&self, hash: u64, mut matches: impl FnMut(NonNull<u8>) -> bool) -> Option<NonNull<u8>> {
        let mask = self.slots.len() - 1;
        let mut at = hash as usize & mask;
        loop {
            let slot = &self.slots[at];
            let value = NonNull::new(slot.value.load(Ordering::Acquire))?;
            if slot.hash.load(Ordering::Relaxed) == hash && matches(value) {
                return Some(value);
            }
            at = (at + 1) & mask;
        }
    }

    /// The first empty slot from `hash`'s home. Called by the holder of the shard's lock, the
    /// only writer, on a table at most half full.
    fn empty(&self, hash: u64) -> &Slot {
        let mask = self.slots.len() - 1;
        let mut at = hash as usize & mask;
        while !self.slots[at].value.load(Ordering::Relaxed).is_null() {
            at = (at + 1) & mask;
        }
        &self.slots[at]
    }
}

/// What [`Store::entry`] found.
pub(crate) enum Entry<'a> {
    Found(NonNull<u8>),
    /// Not there, with the shard locked until it is inserted.
    Vacant(Vacant<'a>),
}

pub(crate) struct Vacant<'a> {
    shard: &'a Shard,
    state: MutexGuard<'a, State>,
    hash: u64,
}

impl Store {
    pub(crate) fn new() -> Store {
        Store {
            shards: (0..SHARDS)
                .map(|_| Shard {
                    table: AtomicPtr::new(ptr::null_mut()),
                    state: Mutex::new(State { len: 0, bump: Bump::new() }),
                })
                .collect(),
        }
    }

    fn shard(&self, hash: u64) -> &Shard {
        // The probe starts from the hash's low bits; the shard is picked by all of them, mixed.
        let mixed = hash.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        &self.shards[(mixed >> (u64::BITS - SHARD_BITS)) as usize]
    }

    /// The value with `hash` that `matches`, without a lock.
    pub(crate) fn find(
        &self,
        hash: u64,
        matches: impl FnMut(NonNull<u8>) -> bool,
    ) -> Option<NonNull<u8>> {
        let table = self.shard(hash).table.load(Ordering::Acquire);
        // SAFETY: A published table is leaked and never freed or written but to fill an empty
        // slot (see `Slot`).
        unsafe { table.as_ref() }?.find(hash, matches)
    }

    /// The value with `hash` that `matches`, or the shard locked to insert it. `matches` is
    /// called with values of this store only, published and never freed.
    pub(crate) fn entry(
        &self,
        hash: u64,
        mut matches: impl FnMut(NonNull<u8>) -> bool,
    ) -> Entry<'_> {
        if let Some(found) = self.find(hash, &mut matches) {
            return Entry::Found(found);
        }
        let shard = self.shard(hash);
        // A panic under the lock (in a caller's `Eq`) leaves the shard as it was.
        let state = shard.state.lock().unwrap_or_else(PoisonError::into_inner);
        // Another thread may have inserted it between the look above and the lock.
        if let Some(found) = self.find(hash, &mut matches) {
            return Entry::Found(found);
        }
        Entry::Vacant(Vacant { shard, state, hash })
    }

    /// How many values the store holds.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.shards
            .iter()
            .map(|shard| shard.state.lock().unwrap_or_else(PoisonError::into_inner).len)
            .sum()
    }
}

impl Vacant<'_> {
    /// Allocates `layout` in the shard's arena, has `write` initialize it and publishes it.
    pub(crate) fn insert(mut self, layout: Layout, write: impl FnOnce(NonNull<u8>)) -> NonNull<u8> {
        let at = self.state.bump.alloc(layout);
        write(at);
        self.publish(at);
        at
    }

    /// Publishes `value`, which must be initialized and live, unmoved, for the life of the
    /// process, as everything [`Store::find`] hands out is.
    pub(crate) fn publish(mut self, value: NonNull<u8>) {
        let table = self.table_for_one_more();
        let slot = table.empty(self.hash);
        slot.hash.store(self.hash, Ordering::Relaxed);
        slot.value.store(value.as_ptr(), Ordering::Release);
        self.state.len += 1;
    }

    /// The table to insert one more value into, at most half full after it.
    fn table_for_one_more(&mut self) -> &'static Table {
        let current = self.shard.table.load(Ordering::Acquire);
        // SAFETY: As in `Store::find`; tables are leaked, so `'static`.
        let current: Option<&'static Table> = unsafe { current.as_ref() };
        let capacity = current.map_or(0, |table| table.slots.len());
        if let Some(table) = current
            && (self.state.len + 1) * 2 <= capacity
        {
            return table;
        }
        let grown = Table::new((capacity * 2).max(FIRST_TABLE));
        for slot in current.into_iter().flat_map(|table| &*table.slots) {
            // Relaxed: every write to the table was made by a holder of the lock we hold.
            let value = slot.value.load(Ordering::Relaxed);
            if value.is_null() {
                continue;
            }
            let hash = slot.hash.load(Ordering::Relaxed);
            let to = grown.empty(hash);
            to.hash.store(hash, Ordering::Relaxed);
            to.value.store(value, Ordering::Relaxed);
        }
        let grown: &'static Table = Box::leak(Box::new(grown));
        // Release: a reader that loads the new table sees its slots and, through the lock's
        // order, the values they point to.
        self.shard.table.store(ptr::from_ref(grown).cast_mut(), Ordering::Release);
        grown
    }
}

/// A bump allocator over chunks that are never freed or reused.
struct Bump {
    /// The next free byte of the current chunk; null before the first.
    cursor: *mut u8,
    /// The bytes left in the current chunk from `cursor`.
    left: usize,
    /// The size of the next chunk.
    next: usize,
}

// SAFETY: `cursor` points into chunks owned by no thread (leaked), and is only used by the
// holder of the shard's lock.
unsafe impl Send for Bump {}

impl Bump {
    fn new() -> Bump {
        Bump { cursor: ptr::null_mut(), left: 0, next: FIRST_CHUNK }
    }

    /// `layout.size()` bytes (at least one, so every value has its own address) aligned to
    /// `layout.align()`, uninitialized, never freed.
    fn alloc(&mut self, layout: Layout) -> NonNull<u8> {
        let size = layout.size().max(1);
        let align = layout.align();
        if align > CHUNK_ALIGN || size > LAST_CHUNK / 4 {
            return own(Layout::from_size_align(size, align).expect("a value's layout"));
        }
        let addr = self.cursor.addr();
        let mut pad = addr.next_multiple_of(align) - addr;
        if pad + size > self.left {
            let bytes = self.next.max(size.next_multiple_of(CHUNK_ALIGN));
            self.next = (self.next * 2).min(LAST_CHUNK);
            self.cursor = own(Layout::from_size_align(bytes, CHUNK_ALIGN).expect("a chunk's layout"))
                .as_ptr();
            self.left = bytes;
            pad = 0;
        }
        // SAFETY: `pad + size <= left`, the bytes of the current chunk from `cursor`.
        unsafe {
            let at = self.cursor.add(pad);
            self.cursor = at.add(size);
            self.left -= pad + size;
            NonNull::new_unchecked(at)
        }
    }
}

/// An allocation of its own, never freed. `layout` has a non-zero size.
fn own(layout: Layout) -> NonNull<u8> {
    // SAFETY: The size is not zero.
    let at = unsafe { alloc::alloc(layout) };
    NonNull::new(at).unwrap_or_else(|| alloc::handle_alloc_error(layout))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intern(store: &Store, hash: u64, value: u64) -> NonNull<u8> {
        let same = |at: NonNull<u8>| unsafe { *at.cast::<u64>().as_ref() == value };
        match store.entry(hash, same) {
            Entry::Found(at) => at,
            Entry::Vacant(vacant) => vacant
                .insert(Layout::new::<u64>(), |to| unsafe { to.cast::<u64>().write(value) }),
        }
    }

    /// Values of one hash all land in one shard and probe past each other: the table grows
    /// many times and every value is still found, where it was written.
    #[test]
    fn colliding_hashes_grow_one_shard() {
        let store = Store::new();
        let first: Vec<_> = (0..5_000).map(|n| intern(&store, 7, n)).collect();
        assert_eq!(store.len(), 5_000);
        for (n, &at) in first.iter().enumerate() {
            assert_eq!(intern(&store, 7, n as u64), at);
            assert_eq!(unsafe { *at.cast::<u64>().as_ref() }, n as u64);
        }
        assert_eq!(store.len(), 5_000);
    }

    #[test]
    fn found_without_lock_after_insert() {
        let store = Store::new();
        let at = intern(&store, 42, 1);
        let found = store.find(42, |it| unsafe { *it.cast::<u64>().as_ref() == 1 });
        assert_eq!(found, Some(at));
        assert_eq!(store.find(42, |it| unsafe { *it.cast::<u64>().as_ref() == 2 }), None);
        assert_eq!(store.find(43, |_| true), None);
    }

    #[test]
    fn bump_aligns_and_never_overlaps() {
        let mut bump = Bump::new();
        let mut taken: Vec<(usize, usize)> = Vec::new();
        for i in 0..10_000usize {
            let align = 1 << (i % 8);
            let size = (i * 7) % 300;
            let at = bump.alloc(Layout::from_size_align(size, align).unwrap());
            assert_eq!(at.as_ptr().addr() % align, 0);
            taken.push((at.as_ptr().addr(), size.max(1)));
        }
        let large = bump.alloc(Layout::from_size_align(1 << 20, 8).unwrap());
        taken.push((large.as_ptr().addr(), 1 << 20));
        let over_aligned = bump.alloc(Layout::from_size_align(8, 4096).unwrap());
        assert_eq!(over_aligned.as_ptr().addr() % 4096, 0);
        taken.push((over_aligned.as_ptr().addr(), 8));
        taken.sort_unstable();
        for pair in taken.windows(2) {
            assert!(pair[0].0 + pair[0].1 <= pair[1].0, "{pair:?}");
        }
    }
}

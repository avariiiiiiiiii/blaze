//! The store under every interner: values in append-only arenas sharded by hash.
//!
//! A value is written once into a chunk of its shard's arena and never moves or is freed: the
//! stores are statics, so a pointer to a value is good for the life of the process. Memory is
//! bounded by the database rebuilding what it keeps (docs/v2/plan/lanes/db.md), not by counting references.
//!
//! A shard's table maps a hash to the values with it, SwissTable-style (docs/v2/plan/design/hardware.md,
//! "interner and memo lookups"): slots in groups of eight, each group with one control word of
//! eight tag bytes, so one load and a few word operations check eight slots. Finding a value
//! takes no lock; interning one takes its shard's lock only to look again and insert. A table
//! grows by copying into one twice its size, published once full; the tables it outgrew are
//! kept, as a reader may be probing one, so a shard's tables cost at most twice its last.
//!
//! Chunks grow to 2 MiB; on Linux those are aligned to 2 MiB and backed by huge pages
//! (docs/v2/plan/design/hardware.md, "memory").

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

/// Slots in a group: one tag byte each in the group's control word.
const GROUP: usize = 8;
const FIRST_GROUPS: usize = 2;
const LOW_BITS: u64 = 0x0101_0101_0101_0101;
const HIGH_BITS: u64 = 0x8080_8080_8080_8080;

const CHUNK_ALIGN: usize = 64;
const FIRST_CHUNK: usize = 1024;
/// The largest chunk, one huge page.
const LAST_CHUNK: usize = 2 * 1024 * 1024;

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

/// `ctrl[g]` holds the tags of `slots[g * GROUP..][..GROUP]`, byte `i` (from the least
/// significant) for slot `i`: 0 when empty, `0x80 | ` the hash's top seven bits when full.
///
/// Only the holder of the shard's lock writes, and each slot once: its `hash` and `value`
/// first, then its tag into the control word with `Release`. A reader that loads the word with
/// `Acquire` and sees the tag sees the slot and the value's bytes.
struct Table {
    ctrl: Box<[AtomicU64]>,
    slots: Box<[Slot]>,
}

struct Slot {
    hash: AtomicU64,
    value: AtomicPtr<u8>,
}

fn tag(hash: u64) -> u64 {
    0x80 | (hash >> 57)
}

/// A byte's high bit set for each byte of `word` that may equal `tag`: every equal byte, and
/// rarely the byte above one (the borrow of the subtraction), which the caller checks.
fn matching(word: u64, tag: u64) -> u64 {
    let x = word ^ (LOW_BITS * tag);
    x.wrapping_sub(LOW_BITS) & !x & HIGH_BITS
}

/// A byte's high bit set for each empty byte of `word`: full bytes have their high bit set.
fn empty(word: u64) -> u64 {
    !word & HIGH_BITS
}

impl Table {
    fn new(groups: usize) -> Table {
        debug_assert!(groups.is_power_of_two());
        Table {
            ctrl: (0..groups).map(|_| AtomicU64::new(0)).collect(),
            slots: (0..groups * GROUP)
                .map(|_| Slot { hash: AtomicU64::new(0), value: AtomicPtr::new(ptr::null_mut()) })
                .collect(),
        }
    }

    fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// The value with `hash` that `matches`, probing groups from the hash's home until one with
    /// an empty slot.
    fn find(&self, hash: u64, mut matches: impl FnMut(NonNull<u8>) -> bool) -> Option<NonNull<u8>> {
        let mask = self.ctrl.len() - 1;
        let tag = tag(hash);
        let mut group = hash as usize & mask;
        loop {
            let word = self.ctrl[group].load(Ordering::Acquire);
            let mut hits = matching(word, tag);
            while hits != 0 {
                let byte = hits.trailing_zeros() / 8;
                hits &= hits - 1;
                // A byte matched only by the borrow may be a slot not published in `word`,
                // which a writer may be filling: only a published tag orders its slot.
                if (word >> (byte * 8)) & 0xff != tag {
                    continue;
                }
                let slot = &self.slots[group * GROUP + byte as usize];
                // Relaxed: the tag's `Acquire` above ordered the slot's writes before us.
                let value = NonNull::new(slot.value.load(Ordering::Relaxed))
                    .expect("a published slot holds a value");
                if slot.hash.load(Ordering::Relaxed) == hash && matches(value) {
                    return Some(value);
                }
            }
            if empty(word) != 0 {
                return None;
            }
            group = (group + 1) & mask;
        }
    }

    /// Fills the first empty slot from `hash`'s home group. Called by the holder of the shard's
    /// lock, the only writer, on a table that stays at most half full.
    fn put(&self, hash: u64, value: *mut u8, publish: Ordering) {
        let mask = self.ctrl.len() - 1;
        let mut group = hash as usize & mask;
        loop {
            let ctrl = &self.ctrl[group];
            let word = ctrl.load(Ordering::Relaxed);
            let free = empty(word);
            if free != 0 {
                let byte = free.trailing_zeros() / 8;
                let slot = &self.slots[group * GROUP + byte as usize];
                slot.hash.store(hash, Ordering::Relaxed);
                slot.value.store(value, Ordering::Relaxed);
                ctrl.store(word | (tag(hash) << (byte * 8)), publish);
                return;
            }
            group = (group + 1) & mask;
        }
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
        // The probe starts from the hash's low bits and the tag is its top ones; the shard is
        // picked by all of them, mixed.
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
        // slot (see `Table`).
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
        table.put(self.hash, value.as_ptr(), Ordering::Release);
        self.state.len += 1;
    }

    /// The table to insert one more value into, at most half full after it.
    fn table_for_one_more(&mut self) -> &'static Table {
        let current = self.shard.table.load(Ordering::Acquire);
        // SAFETY: As in `Store::find`; tables are leaked, so `'static`.
        let current: Option<&'static Table> = unsafe { current.as_ref() };
        let capacity = current.map_or(0, Table::capacity);
        if let Some(table) = current
            && (self.state.len + 1) * 2 <= capacity
        {
            return table;
        }
        let groups = current.map_or(FIRST_GROUPS, |table| table.ctrl.len() * 2);
        let grown = Table::new(groups);
        if let Some(old) = current {
            // Relaxed: every write to the table was made by a holder of the lock we hold.
            for (group, ctrl) in old.ctrl.iter().enumerate() {
                let mut full = ctrl.load(Ordering::Relaxed) & HIGH_BITS;
                while full != 0 {
                    let slot = &old.slots[group * GROUP + (full.trailing_zeros() / 8) as usize];
                    full &= full - 1;
                    let hash = slot.hash.load(Ordering::Relaxed);
                    grown.put(hash, slot.value.load(Ordering::Relaxed), Ordering::Relaxed);
                }
            }
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
            self.cursor = chunk(bytes).as_ptr();
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

/// A chunk of `bytes`, never freed. A chunk of `LAST_CHUNK` bytes is one huge page.
fn chunk(bytes: usize) -> NonNull<u8> {
    if bytes != LAST_CHUNK {
        return own(Layout::from_size_align(bytes, CHUNK_ALIGN).expect("a chunk's layout"));
    }
    let at = own(Layout::from_size_align(LAST_CHUNK, LAST_CHUNK).expect("a chunk's layout"));
    #[cfg(target_os = "linux")]
    {
        // SAFETY: `at..at + LAST_CHUNK` is one allocation of ours, aligned to the huge page
        // size; `MADV_HUGEPAGE` changes only how it is backed, not its contents.
        let advised = unsafe { libc::madvise(at.as_ptr().cast(), LAST_CHUNK, libc::MADV_HUGEPAGE) };
        assert!(
            advised == 0,
            "intern: madvise(MADV_HUGEPAGE) of a 2 MiB interner chunk failed: {} \
             (the kernel needs transparent huge pages in `madvise` or `always` mode)",
            std::io::Error::last_os_error(),
        );
    }
    at
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

    /// Values of one hash all land in one shard, one home group and one tag, and probe past
    /// each other: the table grows many times and every value is still found where it was
    /// written.
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

    /// Hashes that share a home group and differ in tag, and ones that share a tag and differ
    /// in the rest: each value is found under its own hash only.
    #[test]
    fn tags_and_groups_tell_values_apart() {
        let store = Store::new();
        let hashes: Vec<u64> = (0..2_000u64)
            .map(|n| if n % 2 == 0 { (n << 57) | 3 } else { (5 << 57) | (n << 8) })
            .collect();
        let at: Vec<_> = hashes.iter().map(|&hash| intern(&store, hash, hash)).collect();
        for (&hash, &at) in hashes.iter().zip(&at) {
            let same = |it: NonNull<u8>| unsafe { *it.cast::<u64>().as_ref() == hash };
            assert_eq!(store.find(hash, same), Some(at));
            assert_eq!(store.find(hash ^ (1 << 56), |_| true), None);
        }
    }

    #[test]
    fn matching_finds_every_equal_byte() {
        for tag in [0x80u64, 0x81, 0xfe, 0xff] {
            for slots in 0..=255u64 {
                let mut word = 0;
                let mut want = 0;
                for byte in 0..8 {
                    if slots & (1 << byte) != 0 {
                        word |= tag << (byte * 8);
                        want |= 0x80 << (byte * 8);
                    } else if byte % 3 == 0 {
                        word |= (tag ^ 1) << (byte * 8);
                    }
                }
                let hits = matching(word, tag);
                assert_eq!(hits & want, want, "{word:x} {tag:x}");
            }
        }
        assert_eq!(empty(0x80ff_0000_8100_0080), 0x0000_8080_0080_8000);
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
        for i in 0..40_000usize {
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
        assert_eq!(bump.next, LAST_CHUNK, "the test reached huge chunks");
    }
}

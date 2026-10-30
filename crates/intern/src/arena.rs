//! The store under every interner: values in append-only arenas sharded by hash.
//!
//! A value is written once into a chunk of its shard's arena and never moves or is freed: the
//! stores are statics, so a pointer to a value is good for the life of the process. Memory is
//! bounded by the database rebuilding what it keeps (docs/v2/plan/lanes/db.md), not by counting references.
//!
//! A shard's table maps a hash to the values with it, SwissTable-style (docs/v2/plan/design/hardware.md,
//! "interner and memo lookups"): slots in groups of seven, each group one cache line holding a
//! control word of tag bytes and the seven value pointers, so a lookup reads one line of the
//! table and then the value. Finding a value takes no lock and writes nothing; interning one
//! takes its shard's [`InsertLock`] only to look again and insert. A table grows by copying into
//! one twice its size, published once full; the tables it outgrew are kept, as a reader may be
//! probing one, so a shard's tables cost at most twice its last.
//!
//! Shards are a power of two of at least four times the core count, chosen once per process
//! (docs/v2/plan/design/concurrency.md, "Writers shard by hash"). Readers load only the packed
//! table pointers, which change when a table grows; writers lock a separate, padded array, so a
//! write never dirties a line a reader loads.
//!
//! Chunks grow to 2 MiB; on Linux those are aligned to 2 MiB and backed by huge pages
//! (docs/v2/plan/design/hardware.md, "memory").
//!
//! Cost: a value takes one 8-byte pointer and one tag byte in its group's line (9.1 bytes a
//! slot) and an 8-byte hash in a column only growth reads, at a load of a quarter to a half: 34
//! to 68 bytes of table a value, plus the value. A found value costs one acquire load of the
//! table pointer, one of the group's control word (almost always the home group's), one pointer
//! load from the same line and the caller's `Eq`.

use std::{
    alloc::{self, Layout},
    ptr::{self, NonNull},
    sync::{
        OnceLock,
        atomic::{AtomicPtr, AtomicU64, Ordering},
    },
};

use self::lock::{InsertLock, Locked};

/// Slots in a group: tag bytes 0 to 6 of the group's control word; byte 7 stays 0 and is
/// masked out of every match.
const GROUP: usize = 7;
const FIRST_GROUPS: usize = 2;
const LOW_BITS: u64 = 0x0001_0101_0101_0101;
const HIGH_BITS: u64 = 0x0080_8080_8080_8080;

/// Never fewer shards than this, so a small machine still spreads its writers.
const LEAST_SHARD_BITS: u32 = 4;

const CHUNK_ALIGN: usize = 64;
const FIRST_CHUNK: usize = 1024;
/// The largest chunk, one huge page.
const LAST_CHUNK: usize = 2 * 1024 * 1024;

/// `log2` of the shard count, the same for every store of the process.
fn shard_bits() -> u32 {
    static BITS: OnceLock<u32> = OnceLock::new();
    *BITS.get_or_init(|| {
        let cores = std::thread::available_parallelism()
            .expect("intern: the core count to size interner shards (available_parallelism)")
            .get();
        (cores * 4).next_power_of_two().trailing_zeros().max(LEAST_SHARD_BITS)
    })
}

pub(crate) struct Store {
    /// Shard `i`'s table in use, null until its first insert. Read by every lookup; stored only
    /// by the holder of `writers[i]` when the table grows. Packed: a pointer changes a few
    /// dozen times in a shard's life, so sharing a line with others' costs readers nearly
    /// nothing, and eight to a line keep the array in cache.
    tables: Box<[AtomicPtr<Table>]>,
    writers: Box<[InsertLock]>,
    bits: u32,
}

/// What a shard's writer owns.
struct State {
    /// The values in the shard.
    len: usize,
    bump: Bump,
}

/// Seven slots and their tags in one cache line.
///
/// `ctrl` byte `i` (from the least significant) is slot `i`'s tag: 0 when empty, `0x80 | ` the
/// hash's top seven bits when full. Only the holder of the shard's lock writes, and each slot
/// once: its `value` first, then its tag into `ctrl` with `Release`. A reader that loads `ctrl`
/// with `Acquire` and sees the tag sees the pointer and the value's bytes.
#[repr(C, align(64))]
struct Group {
    ctrl: AtomicU64,
    values: [AtomicPtr<u8>; GROUP],
}

const _: () = assert!(size_of::<Group>() == 64);

struct Table {
    groups: Box<[Group]>,
    /// The hash of `groups[i / GROUP].values[i % GROUP]`, written with it and read only by
    /// the writer growing the table, so growing never rehashes a value.
    hashes: Box<[AtomicU64]>,
}

fn tag(hash: u64) -> u64 {
    0x80 | (hash >> 57)
}

/// A byte's high bit set for each slot byte of `word` that may equal `tag`: every equal byte,
/// and rarely the byte above one (the borrow of the subtraction), which the caller checks.
fn matching(word: u64, tag: u64) -> u64 {
    let x = word ^ (LOW_BITS * tag);
    x.wrapping_sub(LOW_BITS) & !x & HIGH_BITS
}

/// A byte's high bit set for each empty slot byte of `word`: full bytes have their high bit
/// set.
fn empty(word: u64) -> u64 {
    !word & HIGH_BITS
}

/// A zeroed boxed slice of `len` `T`s.
///
/// # Safety
///
/// All zero bytes must be a valid `T`.
unsafe fn zeroed<T>(len: usize) -> Box<[T]> {
    // SAFETY: The caller's precondition.
    unsafe { Box::<[T]>::new_zeroed_slice(len).assume_init() }
}

impl Table {
    fn new(groups: usize) -> Table {
        debug_assert!(groups.is_power_of_two());
        // SAFETY: Zero is an empty control word, a null pointer and a zero hash.
        unsafe { Table { groups: zeroed(groups), hashes: zeroed(groups * GROUP) } }
    }

    fn capacity(&self) -> usize {
        self.hashes.len()
    }

    /// The value with `hash` that `matches`, probing groups from the hash's home until one with
    /// an empty slot.
    #[inline]
    fn find(&self, hash: u64, mut matches: impl FnMut(NonNull<u8>) -> bool) -> Option<NonNull<u8>> {
        let mask = self.groups.len() - 1;
        let tag = tag(hash);
        let mut at = hash as usize & mask;
        loop {
            let group = &self.groups[at];
            let word = group.ctrl.load(Ordering::Acquire);
            let mut hits = matching(word, tag);
            while hits != 0 {
                let byte = (hits.trailing_zeros() / 8) as usize;
                hits &= hits - 1;
                // A byte matched only by the borrow may be a slot not published in `word`,
                // which a writer may be filling: only a published tag orders its slot.
                if (word >> (byte * 8)) & 0xff != tag {
                    continue;
                }
                // Relaxed: the tag's `Acquire` above ordered the slot's writes before us.
                let value = NonNull::new(group.values[byte].load(Ordering::Relaxed))
                    .expect("a published slot holds a value");
                if matches(value) {
                    return Some(value);
                }
            }
            if empty(word) != 0 {
                return None;
            }
            at = (at + 1) & mask;
        }
    }

    /// Fills the first empty slot from `hash`'s home group. Called by the holder of the shard's
    /// lock, the only writer, on a table that stays at most half full.
    fn put(&self, hash: u64, value: *mut u8, publish: Ordering) {
        let mask = self.groups.len() - 1;
        let mut at = hash as usize & mask;
        loop {
            let group = &self.groups[at];
            let word = group.ctrl.load(Ordering::Relaxed);
            let free = empty(word);
            if free != 0 {
                let byte = (free.trailing_zeros() / 8) as usize;
                group.values[byte].store(value, Ordering::Relaxed);
                self.hashes[at * GROUP + byte].store(hash, Ordering::Relaxed);
                group.ctrl.store(word | (tag(hash) << (byte * 8)), publish);
                return;
            }
            at = (at + 1) & mask;
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
    table: &'a AtomicPtr<Table>,
    state: Locked<'a>,
    hash: u64,
}

impl Store {
    pub(crate) fn new() -> Store {
        let bits = shard_bits();
        let shards = 1usize << bits;
        Store {
            tables: (0..shards).map(|_| AtomicPtr::new(ptr::null_mut())).collect(),
            writers: (0..shards)
                .map(|_| InsertLock::new(State { len: 0, bump: Bump::new() }))
                .collect(),
            bits,
        }
    }

    #[inline]
    fn shard(&self, hash: u64) -> usize {
        // The probe starts from the hash's low bits and the tag is its top ones; the shard is
        // picked by all of them, mixed.
        let mixed = hash.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        (mixed >> (u64::BITS - self.bits)) as usize
    }

    /// The value with `hash` that `matches`, without a lock or a write.
    #[inline]
    pub(crate) fn find(
        &self,
        hash: u64,
        matches: impl FnMut(NonNull<u8>) -> bool,
    ) -> Option<NonNull<u8>> {
        let table = self.tables[self.shard(hash)].load(Ordering::Acquire);
        // SAFETY: A published table is leaked and never freed or written but to fill an empty
        // slot (see `Group`).
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
        let state = self.writers[shard].lock();
        // Another thread may have inserted it between the look above and the lock.
        if let Some(found) = self.find(hash, &mut matches) {
            return Entry::Found(found);
        }
        Entry::Vacant(Vacant { table: &self.tables[shard], state, hash })
    }

    /// How many values the store holds.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.writers.iter().map(|writer| writer.lock().len).sum()
    }

    #[cfg(test)]
    fn shards(&self) -> usize {
        self.tables.len()
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
        let current = self.table.load(Ordering::Acquire);
        // SAFETY: As in `Store::find`; tables are leaked, so `'static`.
        let current: Option<&'static Table> = unsafe { current.as_ref() };
        let capacity = current.map_or(0, Table::capacity);
        if let Some(table) = current
            && (self.state.len + 1) * 2 <= capacity
        {
            return table;
        }
        let groups = current.map_or(FIRST_GROUPS, |table| table.groups.len() * 2);
        let grown = Table::new(groups);
        if let Some(old) = current {
            // Relaxed: every write to the table was made by a holder of the lock we hold.
            for (at, group) in old.groups.iter().enumerate() {
                let mut full = group.ctrl.load(Ordering::Relaxed) & HIGH_BITS;
                while full != 0 {
                    let byte = (full.trailing_zeros() / 8) as usize;
                    full &= full - 1;
                    let hash = old.hashes[at * GROUP + byte].load(Ordering::Relaxed);
                    grown.put(hash, group.values[byte].load(Ordering::Relaxed), Ordering::Relaxed);
                }
            }
        }
        let grown: &'static Table = Box::leak(Box::new(grown));
        // Release: a reader that loads the new table sees its slots and, through the lock's
        // order, the values they point to.
        self.table.store(ptr::from_ref(grown).cast_mut(), Ordering::Release);
        grown
    }
}

mod lock {
    #![allow(
        clippy::disallowed_types,
        reason = "the one lock of `intern`, named and bounded as docs/v2/plan/design/concurrency.md asks"
    )]

    use std::{
        ops::{Deref, DerefMut},
        sync::{Mutex, MutexGuard, PoisonError},
    };

    use super::State;

    /// The lock a shard's writers take to look again for a value and insert it. It is held
    /// only for that: the caller's `Eq` against the shard's values, one bump allocation, the
    /// value's write and the slot's publication. Nothing called under it reaches a query or
    /// another lock, so it can't be held across a query call; a reader never takes it.
    ///
    /// Padded to 128 bytes, the pair of lines arm64 and x86 prefetch together, so writers of
    /// two shards never share a line.
    #[repr(align(128))]
    pub(super) struct InsertLock(Mutex<State>);

    pub(super) struct Locked<'a>(MutexGuard<'a, State>);

    impl InsertLock {
        pub(super) fn new(state: State) -> InsertLock {
            InsertLock(Mutex::new(state))
        }

        pub(super) fn lock(&self) -> Locked<'_> {
            // A panic under the lock (in a caller's `Eq`) leaves the shard as it was.
            Locked(self.0.lock().unwrap_or_else(PoisonError::into_inner))
        }
    }

    impl Deref for Locked<'_> {
        type Target = State;

        fn deref(&self) -> &State {
            &self.0
        }
    }

    impl DerefMut for Locked<'_> {
        fn deref_mut(&mut self) -> &mut State {
            &mut self.0
        }
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
    use std::collections::HashMap;

    use super::*;

    fn read(at: NonNull<u8>) -> u64 {
        // SAFETY: Every value of the test stores is a `u64`.
        unsafe { *at.cast::<u64>().as_ref() }
    }

    fn intern(store: &Store, hash: u64, value: u64) -> NonNull<u8> {
        match store.entry(hash, |at| read(at) == value) {
            Entry::Found(at) => at,
            Entry::Vacant(vacant) => vacant
                .insert(Layout::new::<u64>(), |to| unsafe { to.cast::<u64>().write(value) }),
        }
    }

    /// A splitmix64 stream: seeded, so a failure reproduces.
    fn stream(mut seed: u64) -> impl FnMut() -> u64 {
        move || {
            seed = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = seed;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }
    }

    #[test]
    fn shards_scale_with_cores() {
        let store = Store::new();
        let cores = std::thread::available_parallelism().unwrap().get();
        assert!(store.shards().is_power_of_two());
        assert!(store.shards() >= cores * 4);
        assert!(store.shards() >= 1 << LEAST_SHARD_BITS);
        assert!(store.shards() < (cores * 8).max(2 << LEAST_SHARD_BITS));
        assert_eq!(store.writers.len(), store.shards());
        assert_eq!(align_of::<InsertLock>(), 128);
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
            assert_eq!(read(at), n as u64);
        }
        assert_eq!(store.len(), 5_000);
    }

    /// Hashes that share a home group and differ in tag, and ones that share a tag and differ
    /// in the rest: each value is found under its own hash, and a value never inserted is not.
    #[test]
    fn tags_and_groups_tell_values_apart() {
        let store = Store::new();
        let hashes: Vec<u64> = (0..2_000u64)
            .map(|n| if n % 2 == 0 { (n << 57) | 3 } else { (5 << 57) | (n << 8) })
            .collect();
        let at: Vec<_> = hashes.iter().map(|&hash| intern(&store, hash, hash)).collect();
        for (&hash, &at) in hashes.iter().zip(&at) {
            assert_eq!(store.find(hash, |it| read(it) == hash), Some(at));
            let other = hash ^ (1 << 56);
            assert_eq!(store.find(other, |it| read(it) == other), None);
        }
    }

    /// The table against a `HashMap` model, over seeded hashes drawn from few enough values
    /// that many collide in tag, group and shard: the same values are found, each at the
    /// address of its first insert, and the counts agree.
    #[test]
    fn table_agrees_with_a_model() {
        for seed in 0..4 {
            let store = Store::new();
            let mut model: HashMap<u64, NonNull<u8>> = HashMap::new();
            let mut next = stream(seed);
            for _ in 0..30_000 {
                let value = next() % 9_000;
                // Few distinct hashes for many values, and some hashes that differ only in the
                // tag or only in the home group.
                let hash = match value % 3 {
                    0 => value % 97,
                    1 => (value % 61) << 57 | 5,
                    _ => value.wrapping_mul(0xff51_afd7_ed55_8ccd),
                };
                let at = intern(&store, hash, value);
                assert_eq!(read(at), value);
                assert_eq!(*model.entry(value).or_insert(at), at, "seed {seed}");
            }
            assert_eq!(store.len(), model.len());
        }
    }

    /// Many threads interning overlapping seeded values get one address per value.
    #[test]
    fn racing_writers_agree() {
        let store = Store::new();
        let found: Vec<Vec<(u64, usize)>> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..16)
                .map(|seed| {
                    let store = &store;
                    scope.spawn(move || {
                        let mut next = stream(seed);
                        (0..20_000)
                            .map(|_| {
                                let value = next() % 5_000;
                                let at = intern(store, value % 211, value);
                                assert_eq!(read(at), value);
                                (value, at.as_ptr().addr())
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            workers.into_iter().map(|it| it.join().unwrap()).collect()
        });
        let mut one: HashMap<u64, usize> = HashMap::new();
        for (value, addr) in found.into_iter().flatten() {
            assert_eq!(*one.entry(value).or_insert(addr), addr);
        }
        assert_eq!(store.len(), one.len());
    }

    #[test]
    fn matching_finds_every_equal_byte() {
        for tag in [0x80u64, 0x81, 0xfe, 0xff] {
            for slots in 0..=127u64 {
                let mut word = 0;
                let mut want = 0;
                for byte in 0..GROUP {
                    if slots & (1 << byte) != 0 {
                        word |= tag << (byte * 8);
                        want |= 0x80 << (byte * 8);
                    } else if byte % 3 == 0 {
                        word |= (tag ^ 1) << (byte * 8);
                    }
                }
                let hits = matching(word, tag);
                assert_eq!(hits & want, want, "{word:x} {tag:x}");
                assert_eq!(hits & !HIGH_BITS, 0, "only slot bytes match");
            }
        }
        assert_eq!(empty(0x0000_ff00_0081_0000), 0x0080_0080_8000_8080);
        assert_eq!(empty(0x00ff_ffff_ffff_ffff), 0, "byte 7 is never a free slot");
    }

    #[test]
    fn found_without_lock_after_insert() {
        let store = Store::new();
        let at = intern(&store, 42, 1);
        assert_eq!(store.find(42, |it| read(it) == 1), Some(at));
        assert_eq!(store.find(42, |it| read(it) == 2), None);
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

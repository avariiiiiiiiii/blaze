//! The key map of an interned ingredient: per shard, a SwissTable read with no lock.
//!
//! Ported from `crates/intern/src/arena.rs` (salsa can't depend on `intern`), the SwissTable
//! layout of docs/v2/plan/design/hardware.md, "interner and memo lookups": slots in groups of
//! seven, each group one cache line holding a control word of tag bytes and the seven value
//! pointers. Only the holder of a shard's lock writes, and each slot of a table at most once to
//! fill it: its pointer and hash first, then its tag into the control word with `Release`. A
//! reader that loads the control word with `Acquire` and sees the tag sees the slot and the
//! value's bytes behind it.
//!
//! Interned values are reused, so a key can leave: its tag becomes [`DEAD`], which probes walk
//! past and nothing refills in that table. A reader that loaded the tag before it died still
//! reads the pointer it was published with, which stays a live value of the ingredient.
//!
//! A table grows by copying its live slots into a new one, twice the size (or the same size when
//! most of its slots are dead), published once full. The tables it outgrew stay readable, as a
//! reader may be probing one, until `&mut` frees them ([`ShardedTable::free_outgrown`], called
//! once per revision, [`ShardedTable::clear`] and drop).
//!
//! Readers load only the packed `published` pointers, which change when a table grows; writers
//! lock a separate, padded array, so taking a lock never dirties a line a reader loads.
//!
//! Cost: a hit loads one table pointer, one control word (almost always the home group's) and
//! one value pointer from the same line, then the caller's check. A slot is a pointer and a tag
//! byte in its group's line (9.1 bytes) plus an 8-byte hash in a column read only to tell
//! colliding tags apart and to grow, at a load of a quarter to a half: 34 to 69 bytes a key.

use std::ptr::{self, NonNull};

use crossbeam_utils::CachePadded;

use crate::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
use crate::sync::{Mutex, MutexGuard};

/// Slots in a group: tag bytes 0 to 6 of the group's control word; byte 7 stays 0 and is
/// masked out of every match.
const GROUP: usize = 7;
const FIRST_GROUPS: usize = 2;
const LOW_BITS: u64 = 0x0001_0101_0101_0101;
const HIGH_BITS: u64 = 0x0080_8080_8080_8080;

/// The tag of a slot whose key left: full, so probes continue past it, and never a key's tag
/// (those are `0x81..=0xff`).
const DEAD: u64 = 0x80;

/// Seven slots and their tags in one cache line.
///
/// `ctrl` byte `i` (from the least significant) is slot `i`'s tag: 0 when empty, [`DEAD`] when
/// its key left, [`tag`] of the key's hash when full.
#[repr(C, align(64))]
struct Group {
    ctrl: AtomicU64,
    slots: [AtomicPtr<()>; GROUP],
}

#[cfg(all(not(feature = "shuttle"), target_pointer_width = "64"))]
const _: () = assert!(std::mem::size_of::<Group>() == 64);

impl Group {
    fn new() -> Group {
        Group {
            ctrl: AtomicU64::new(0),
            slots: std::array::from_fn(|_| AtomicPtr::new(ptr::null_mut())),
        }
    }
}

struct Groups {
    groups: Box<[Group]>,
    /// The hash of slot `i` (`groups[i / GROUP].slots[i % GROUP]`), written with it.
    hashes: Box<[AtomicU64]>,
}

/// A key's tag: the hash's top seven bits, `0` folded into `1` so no tag is [`DEAD`].
#[inline]
fn tag(hash: u64) -> u64 {
    0x80 | (hash >> 57).max(1)
}

/// A byte's high bit set for each slot byte of `word` that may equal `tag`: every equal byte,
/// and rarely the byte above one (the borrow of the subtraction), which the caller checks.
#[inline]
fn matching(word: u64, tag: u64) -> u64 {
    let x = word ^ (LOW_BITS * tag);
    x.wrapping_sub(LOW_BITS) & !x & HIGH_BITS
}

/// A byte's high bit set for each empty slot byte of `word`: full and dead bytes have their
/// high bit set.
#[inline]
fn empty(word: u64) -> u64 {
    !word & HIGH_BITS
}

/// A byte's high bit set for each slot byte of `word` holding a key: full, and not [`DEAD`]
/// (whose low seven bits are all zero). No byte's sum carries into the next.
#[inline]
fn live(word: u64) -> u64 {
    word & HIGH_BITS & ((word & !HIGH_BITS).wrapping_add(LOW_BITS * 0x7f) & HIGH_BITS)
}

/// A slot whose tag matched a lookup's.
pub(super) struct Candidate<'a> {
    value: NonNull<()>,
    hash: &'a AtomicU64,
}

impl Candidate<'_> {
    /// The value the slot was published with.
    #[inline]
    pub(super) fn value(&self) -> NonNull<()> {
        self.value
    }

    /// The full hash the slot was inserted under: one load from the hash column, for a caller
    /// that must tell keys apart before it can read the value.
    #[inline]
    pub(super) fn hash(&self) -> u64 {
        // Relaxed: the control word's `Acquire` ordered the slot's writes before us.
        self.hash.load(Ordering::Relaxed)
    }
}

impl Groups {
    fn new(groups: usize) -> Groups {
        debug_assert!(groups.is_power_of_two());
        Groups {
            // alloc: growth: one table per doubling of a shard, the old one kept until the database drops (readers hold no lock)
            groups: (0..groups).map(|_| Group::new()).collect(),
            // alloc: growth: one table per doubling of a shard, the old one kept until the database drops (readers hold no lock)
            hashes: (0..groups * GROUP).map(|_| AtomicU64::new(0)).collect(),
        }
    }

    fn capacity(&self) -> usize {
        self.hashes.len()
    }

    /// The value with `hash` that `matches` accepts, probing groups from the hash's home until
    /// one with an empty slot.
    #[inline]
    fn find(
        &self,
        hash: u64,
        matches: &mut impl FnMut(Candidate<'_>) -> bool,
    ) -> Option<NonNull<()>> {
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
                let value = NonNull::new(group.slots[byte].load(Ordering::Relaxed))
                    .expect("a published slot holds a value");
                let candidate = Candidate {
                    value,
                    hash: &self.hashes[at * GROUP + byte],
                };
                if matches(candidate) {
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
    /// lock, the only writer, on a table that stays at most half used.
    fn put(&self, hash: u64, value: NonNull<()>, publish: Ordering) {
        let mask = self.groups.len() - 1;
        let mut at = hash as usize & mask;
        loop {
            let group = &self.groups[at];
            let word = group.ctrl.load(Ordering::Relaxed);
            let free = empty(word);
            if free != 0 {
                let byte = (free.trailing_zeros() / 8) as usize;
                group.slots[byte].store(value.as_ptr(), Ordering::Relaxed);
                self.hashes[at * GROUP + byte].store(hash, Ordering::Relaxed);
                group
                    .ctrl
                    .store(word | (tag(hash) << (byte * 8)), publish);
                return;
            }
            at = (at + 1) & mask;
        }
    }

    /// Marks dead the slot holding `value` under `hash`; `false` if there is none. Called by
    /// the holder of the shard's lock.
    fn kill(&self, hash: u64, value: NonNull<()>) -> bool {
        let mask = self.groups.len() - 1;
        let tag = tag(hash);
        let mut at = hash as usize & mask;
        loop {
            let group = &self.groups[at];
            let word = group.ctrl.load(Ordering::Relaxed);
            let mut hits = matching(word, tag);
            while hits != 0 {
                let byte = (hits.trailing_zeros() / 8) as usize;
                hits &= hits - 1;
                let shift = byte * 8;
                if (word >> shift) & 0xff == tag
                    && group.slots[byte].load(Ordering::Relaxed) == value.as_ptr()
                    && self.hashes[at * GROUP + byte].load(Ordering::Relaxed) == hash
                {
                    let dead = (word & !(0xff << shift)) | (DEAD << shift);
                    group.ctrl.store(dead, Ordering::Release);
                    return true;
                }
            }
            if empty(word) != 0 {
                return false;
            }
            at = (at + 1) & mask;
        }
    }
}

/// Frees a table.
///
/// # Safety
///
/// `groups` must come from `Box::into_raw` (through [`Box::leak`]), be freed once, and no
/// reference to it may be live.
unsafe fn free(groups: NonNull<Groups>) {
    // SAFETY: The caller's precondition: we own the allocation and nothing borrows it.
    drop(unsafe { Box::from_raw(groups.as_ptr()) });
}

/// What a shard's writer owns, behind its lock.
struct Writer<S> {
    /// Every table the shard has published since the last `&mut`, the current one last. Each
    /// came from `Box::leak`, and is freed only through `&mut ShardedTable`.
    owned: Vec<NonNull<Groups>>,
    /// Keys in the current table.
    live: usize,
    /// Dead slots in the current table.
    dead: usize,
    state: S,
}

// SAFETY: `owned` holds the only owning pointers to its tables (as a `Vec<Box<Groups>>` would),
// and a `Groups` is atomics only, so it may move to and be used from any thread.
unsafe impl<S: Send> Send for Writer<S> {}

/// The shards' tables, with a caller state `S` behind each shard's lock.
pub(super) struct ShardedTable<S> {
    /// Shard `i`'s current table, null until its first insert. Loaded by every lookup; stored
    /// only by the holder of `writers[i]` when the table grows, and through `&mut`.
    published: Box<[AtomicPtr<Groups>]>,
    writers: Box<[CachePadded<Mutex<Writer<S>>>]>,
}

impl<S> ShardedTable<S> {
    pub(super) fn new(shards: usize, mut state: impl FnMut() -> S) -> Self {
        Self {
            published: (0..shards)
                .map(|_| AtomicPtr::new(ptr::null_mut()))
                // alloc: setup, once per interned ingredient
                .collect(),
            writers: (0..shards)
                .map(|_| {
                    CachePadded::new(Mutex::new(Writer {
                        // alloc: setup, once per interned ingredient
                        owned: Vec::new(),
                        live: 0,
                        dead: 0,
                        state: state(),
                    }))
                })
                // alloc: setup, once per interned ingredient
                .collect(),
        }
    }

    pub(super) fn shards(&self) -> usize {
        self.writers.len()
    }

    /// The value in `shard` with `hash` that `matches` accepts, without a lock or a write.
    /// `matches` sees values published in this shard, possibly since removed.
    #[inline]
    pub(super) fn find(
        &self,
        shard: usize,
        hash: u64,
        mut matches: impl FnMut(Candidate<'_>) -> bool,
    ) -> Option<NonNull<()>> {
        let groups = self.published[shard].load(Ordering::Acquire);
        // SAFETY: A published table is owned by `writers[shard].owned` and freed only through
        // `&mut self` (`free_outgrown`, `clear`, drop), so it outlives this `&self` borrow. Its
        // writes are atomic: filling an empty slot and killing a full one.
        unsafe { groups.as_ref() }?.find(hash, &mut matches)
    }

    /// Locks `shard` to look again, insert or remove.
    #[inline]
    pub(super) fn lock(&self, shard: usize) -> Locked<'_, S> {
        Locked {
            published: &self.published[shard],
            writer: self.writers[shard].lock(),
        }
    }

    /// Frees every table a shard outgrew.
    pub(super) fn free_outgrown(&mut self) {
        for writer in self.writers.iter_mut() {
            let writer = writer.get_mut();
            let Some(current) = writer.owned.pop() else {
                continue;
            };
            for groups in writer.owned.drain(..) {
                // SAFETY: `owned` held the only pointer to each outgrown table, from `Box::leak`,
                // and `&mut self` means no `find` or `Locked` borrows one.
                unsafe { free(groups) };
            }
            writer.owned.push(current);
        }
    }

    /// Empties every shard and has `reset` reset its state.
    pub(super) fn clear(&mut self, mut reset: impl FnMut(&mut S)) {
        for (published, writer) in self.published.iter().zip(self.writers.iter_mut()) {
            let writer = writer.get_mut();
            published.store(ptr::null_mut(), Ordering::Relaxed);
            for groups in writer.owned.drain(..) {
                // SAFETY: As in `free_outgrown`, for the current table too: it is unpublished
                // above, and `&mut self` means nothing borrows it.
                unsafe { free(groups) };
            }
            writer.live = 0;
            writer.dead = 0;
            reset(&mut writer.state);
        }
    }
}

impl<S> Drop for ShardedTable<S> {
    fn drop(&mut self) {
        self.clear(|_| {});
    }
}

/// A shard locked: the only writer of its table and owner of its state.
pub(super) struct Locked<'a, S> {
    published: &'a AtomicPtr<Groups>,
    writer: MutexGuard<'a, Writer<S>>,
}

impl<S> Locked<'_, S> {
    fn current(&self) -> Option<&Groups> {
        let groups = self.writer.owned.last()?;
        // SAFETY: `owned` keeps its tables alive until `&mut ShardedTable`, which can't exist
        // while this guard borrows the table.
        Some(unsafe { groups.as_ref() })
    }

    /// The value with `hash` that `matches` accepts. Under the lock nothing is inserted or
    /// removed meanwhile, so a miss is final until the lock is released.
    #[inline]
    pub(super) fn find(
        &self,
        hash: u64,
        mut matches: impl FnMut(Candidate<'_>) -> bool,
    ) -> Option<NonNull<()>> {
        self.current()?.find(hash, &mut matches)
    }

    /// Publishes `value` under `hash`. `value` must be ready for every reader that finds it:
    /// its bytes written before this call.
    pub(super) fn insert(&mut self, hash: u64, value: NonNull<()>) {
        self.table_for_one_more().put(hash, value, Ordering::Release);
        self.writer.live += 1;
    }

    /// Removes `value`, inserted under `hash`; `false` if it isn't there.
    pub(super) fn remove(&mut self, hash: u64, value: NonNull<()>) -> bool {
        let Some(groups) = self.current() else {
            return false;
        };
        if !groups.kill(hash, value) {
            return false;
        }
        self.writer.live -= 1;
        self.writer.dead += 1;
        true
    }

    /// Keys in the shard.
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.writer.live
    }

    pub(super) fn state_mut(&mut self) -> &mut S {
        &mut self.writer.state
    }

    /// The table to fill one more slot of, at most half used (keys and dead slots) after it.
    fn table_for_one_more(&mut self) -> &Groups {
        let writer = &mut *self.writer;
        let current = writer.owned.last().map(|groups| {
            // SAFETY: As in `current`.
            unsafe { groups.as_ref() }
        });
        if let Some(table) = current
            && (writer.live + writer.dead + 1) * 2 <= table.capacity()
        {
            return table;
        }
        // Double when keys fill a quarter; otherwise most used slots are dead, and copying the
        // keys into a table of the same size clears them.
        let groups = current.map_or(FIRST_GROUPS, |table| {
            if (writer.live + 1) * 4 > table.capacity() {
                table.groups.len() * 2
            } else {
                table.groups.len()
            }
        });
        let grown = Groups::new(groups);
        if let Some(old) = current {
            // Relaxed: every write to the table was made by a holder of the lock we hold.
            for (at, group) in old.groups.iter().enumerate() {
                let mut keys = live(group.ctrl.load(Ordering::Relaxed));
                while keys != 0 {
                    let byte = (keys.trailing_zeros() / 8) as usize;
                    keys &= keys - 1;
                    let hash = old.hashes[at * GROUP + byte].load(Ordering::Relaxed);
                    let value = NonNull::new(group.slots[byte].load(Ordering::Relaxed))
                        .expect("a live slot holds a value");
                    grown.put(hash, value, Ordering::Relaxed);
                }
            }
        }
        // alloc: growth: the grown table, once per doubling of a shard
        let grown = NonNull::from(Box::leak(Box::new(grown)));
        writer.owned.push(grown);
        writer.dead = 0;
        // Release: a reader that loads the new table sees its slots and, through the lock's
        // order, the values they point to.
        self.published.store(grown.as_ptr(), Ordering::Release);
        // SAFETY: Just pushed to `owned`, which keeps it alive as in `current`.
        unsafe { grown.as_ref() }
    }
}

#[cfg(test)]
#[cfg(not(feature = "shuttle"))]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;

    use blaze_ids::rng::SplitMix64;

    use super::*;

    /// Values are leaked `u64`s, like interned values that live as long as the database.
    fn leak(value: u64) -> NonNull<()> {
        NonNull::from(Box::leak(Box::new(value))).cast()
    }

    fn read(value: NonNull<()>) -> u64 {
        // SAFETY: Every value of these tests is a leaked `u64`.
        unsafe { *value.cast::<u64>().as_ref() }
    }

    fn is(key: u64) -> impl Fn(Candidate<'_>) -> bool {
        move |it| read(it.value()) == key
    }

    /// Few distinct hashes for many keys, and some that differ only in the tag or only in the
    /// home group, so tags, groups and shards collide.
    fn hash_of(key: u64) -> u64 {
        match key % 3 {
            0 => key % 97,
            1 => ((key % 61) << 57) | 5,
            _ => key.wrapping_mul(0xff51_afd7_ed55_8ccd),
        }
    }

    /// Today's map, a locked `hashbrown::HashTable` of value pointers, as the reference.
    struct Reference(StdMutex<hashbrown::HashTable<NonNull<()>>>);

    impl Reference {
        fn find(&self, hash: u64, key: u64) -> Option<NonNull<()>> {
            self.0
                .lock()
                .unwrap()
                .find(hash, |&value| read(value) == key)
                .copied()
        }

        fn insert(&self, hash: u64, value: NonNull<()>) {
            self.0
                .lock()
                .unwrap()
                .insert_unique(hash, value, |&value| hash_of(read(value)));
        }

        fn remove(&self, hash: u64, value: NonNull<()>) -> bool {
            match self.0.lock().unwrap().find_entry(hash, |&it| it == value) {
                Ok(entry) => {
                    entry.remove();
                    true
                }
                Err(_) => false,
            }
        }
    }

    const SHARDS: usize = 4;

    fn shard_of(hash: u64) -> usize {
        ((hash >> 13) as usize) % SHARDS
    }

    /// Seeded intern, lookup and remove sequences: the table and the locked reference give the
    /// same answer to every lookup, lock-free and locked, and hold the same number of keys.
    #[test]
    fn table_agrees_with_locked_hashbrown() {
        for seed in 0..6 {
            let table = ShardedTable::new(SHARDS, || ());
            let reference = Reference(StdMutex::new(hashbrown::HashTable::new()));
            let mut keys: HashMap<u64, NonNull<()>> = HashMap::new();
            let mut rng = SplitMix64::new(seed);
            for step in 0..40_000 {
                let key = rng.below(6_000);
                let hash = hash_of(key);
                let shard = shard_of(hash);
                let found = table.find(shard, hash, |it| read(it.value()) == key);
                assert_eq!(found, reference.find(hash, key), "seed {seed} step {step}");
                match rng.below(8) {
                    // Remove, as a reused slot leaves its old key.
                    0 => {
                        if let Some(value) = found {
                            assert!(table.lock(shard).remove(hash, value));
                            assert!(reference.remove(hash, value));
                            assert!(!table.lock(shard).remove(hash, value));
                            keys.remove(&key);
                        }
                    }
                    // Lookup only.
                    1 | 2 => {}
                    // Intern.
                    _ => {
                        let mut locked = table.lock(shard);
                        let again = locked.find(hash, |it| read(it.value()) == key);
                        assert_eq!(again, found);
                        if again.is_none() {
                            let value = leak(key);
                            locked.insert(hash, value);
                            reference.insert(hash, value);
                            keys.insert(key, value);
                        }
                    }
                }
            }
            for (&key, &value) in &keys {
                let hash = hash_of(key);
                let found = table.find(shard_of(hash), hash, |it| read(it.value()) == key);
                assert_eq!(found, Some(value));
            }
            let len: usize = (0..SHARDS).map(|shard| table.lock(shard).len()).sum();
            assert_eq!(len, keys.len());
        }
    }

    /// Removing and re-inserting under one hash, past many rehashes, never loses a key and frees
    /// outgrown tables on `&mut`.
    #[test]
    fn churn_rehashes_without_losing_keys() {
        let mut table = ShardedTable::new(1, || ());
        let values: Vec<_> = (0..64).map(leak).collect();
        for &value in &values {
            table.lock(0).insert(7, value);
        }
        let mut rng = SplitMix64::new(9);
        for round in 0..20_000 {
            let at = rng.index(values.len());
            let mut locked = table.lock(0);
            assert!(locked.remove(7, values[at]), "round {round}");
            locked.insert(7, values[at]);
            drop(locked);
            if round % 1_000 == 0 {
                table.free_outgrown();
                assert_eq!(table.writers[0].get_mut().owned.len(), 1);
            }
        }
        for &value in &values {
            assert_eq!(table.find(0, 7, |it| it.value() == value), Some(value));
        }
        assert_eq!(table.lock(0).len(), values.len());
    }

    /// A removed key is not found, and a candidate's hash is the one it was inserted under.
    #[test]
    fn removed_keys_are_gone_and_hashes_kept() {
        let table = ShardedTable::new(1, || ());
        let hashes: Vec<u64> = (0..500u64)
            .map(|n| {
                if n % 2 == 0 {
                    (n << 57) | 3
                } else {
                    (5 << 57) | (n << 8)
                }
            })
            .collect();
        let values: Vec<_> = hashes.iter().map(|&hash| leak(hash)).collect();
        for (&hash, &value) in hashes.iter().zip(&values) {
            table.lock(0).insert(hash, value);
        }
        for (&hash, &value) in hashes.iter().zip(&values) {
            let mut seen = None;
            let found = table.find(0, hash, |it| {
                if it.value() == value {
                    seen = Some(it.hash());
                }
                it.value() == value
            });
            assert_eq!(found, Some(value));
            assert_eq!(seen, Some(hash));
        }
        for (&hash, &value) in hashes.iter().zip(&values).step_by(3) {
            assert!(table.lock(0).remove(hash, value));
            assert_eq!(table.find(0, hash, |it| it.value() == value), None);
        }
    }

    /// Threads racing to intern overlapping keys, each looking without the lock first, agree on
    /// one value per key.
    #[test]
    fn racing_writers_agree() {
        let table = ShardedTable::new(SHARDS, || ());
        let found: Vec<Vec<(u64, usize)>> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..8)
                .map(|seed| {
                    let table = &table;
                    scope.spawn(move || {
                        let mut rng = SplitMix64::new(seed);
                        (0..20_000)
                            .map(|_| {
                                let key = rng.below(3_000);
                                let hash = hash_of(key);
                                let shard = shard_of(hash);
                                let value = match table.find(shard, hash, is(key)) {
                                    Some(value) => value,
                                    None => {
                                        let mut locked = table.lock(shard);
                                        match locked.find(hash, is(key)) {
                                            Some(value) => value,
                                            None => {
                                                let value = leak(key);
                                                locked.insert(hash, value);
                                                value
                                            }
                                        }
                                    }
                                };
                                assert_eq!(read(value), key);
                                (key, value.as_ptr().addr())
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            workers.into_iter().map(|it| it.join().unwrap()).collect()
        });
        let mut one: HashMap<u64, usize> = HashMap::new();
        for (key, addr) in found.into_iter().flatten() {
            assert_eq!(*one.entry(key).or_insert(addr), addr);
        }
        let len: usize = (0..SHARDS).map(|shard| table.lock(shard).len()).sum();
        assert_eq!(len, one.len());
    }

    #[test]
    fn tags_are_never_dead() {
        for top in 0..128u64 {
            let tag = tag(top << 57);
            assert!((0x81..=0xff).contains(&tag), "{tag:x}");
        }
    }

    #[test]
    fn matching_finds_every_equal_byte() {
        for tag in [0x81u64, 0x82, 0xfe, 0xff] {
            for slots in 0..=127u64 {
                let mut word = 0;
                let mut want = 0;
                for byte in 0..GROUP {
                    if slots & (1 << byte) != 0 {
                        word |= tag << (byte * 8);
                        want |= 0x80 << (byte * 8);
                    } else if byte % 3 == 0 {
                        word |= (tag ^ 1) << (byte * 8);
                    } else if byte % 3 == 1 {
                        word |= DEAD << (byte * 8);
                    }
                }
                let hits = matching(word, tag);
                assert_eq!(hits & want, want, "{word:x} {tag:x}");
                assert_eq!(hits & !HIGH_BITS, 0, "only slot bytes match");
            }
        }
        assert_eq!(empty(0x0000_ff00_0081_0000), 0x0080_0080_8000_8080);
        assert_eq!(empty(0x00ff_ffff_ffff_ffff), 0, "byte 7 is never a free slot");
        assert_eq!(empty(0x0000_0000_0000_0080), 0x0080_8080_8080_8000, "dead is not free");
    }

    #[test]
    fn live_skips_empty_and_dead() {
        for word in [
            0u64,
            0x0000_0000_0000_0080,
            0x0081_80ff_0000_8082,
            0x00ff_ffff_ffff_ffff,
            0x0080_8080_8080_8080,
        ] {
            let mut want = 0;
            for byte in 0..GROUP {
                let b = (word >> (byte * 8)) & 0xff;
                if b & 0x80 != 0 && b != DEAD {
                    want |= 0x80 << (byte * 8);
                }
            }
            assert_eq!(live(word), want, "{word:x}");
        }
    }
}

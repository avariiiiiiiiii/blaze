//! An append-only open-addressed table that any number of threads read and insert into with no
//! lock (docs/v2/plan/design/concurrency.md "Reads").
//!
//! - A slot holds a pointer to an immutable value, null until a value is published into it
//!   with one compare-and-swap. A lookup is acquire loads and key compares: it writes nothing.
//! - A key never moves: a full level is not rehashed; the table adds a level twice as large,
//!   and lookups look through every level, oldest first.
//! - A key is in one level at most. An insert walks the levels oldest first too, and leaves a
//!   level it finds too full by sealing the empty slot that ends the key's run there: every
//!   inserter of the key meets that same slot, so one compare-and-swap decides for all of them
//!   whether the key lands in that level or every one of them goes on to the next.
//! - Only the store (the one writer) swaps a published value for another of the same key, and
//!   hands the old one to its [`blaze_reclaim::Limbo`]: a reader may still hold it.
//! - Each row (a slot, numbered across levels) has flag bits in words beside the slots, set
//!   with one relaxed `fetch_or` and only when not set already.

use std::{marker::PhantomData, ptr};

use blaze_reclaim::Guard;

use crate::sync::{
    OnceLock,
    atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering},
};

const LEVELS: usize = 40;

/// What a sealed slot holds: an address no value has, never dereferenced.
static SEAL: u8 = 0;

fn seal<T>() -> *mut T {
    ptr::from_ref(&SEAL).cast_mut().cast()
}

/// What a probe of a level for a key met.
enum Probe<'a, T> {
    /// The key, at this slot.
    Found(usize, &'a T),
    /// The empty slot that ends the key's run: the key is not in this level yet.
    Empty(usize),
    /// A seal ending the key's run, or a level with no empty slot: the key is not in this level
    /// and never will be.
    Past,
}

/// A value of a table: immutable once published, found by its key.
pub(crate) trait Keyed: Send + Sync {
    type Key: Copy + Eq;
    fn key(&self) -> Self::Key;
    /// Where its probing starts: uniform over 64 bits.
    fn hash(key: &Self::Key) -> u64;
}

/// Proof that what a table gives out stays valid for `'a`: the reader is in a task
/// ([`Held::task`]), or it is the store borrowed shared ([`Held::writer`]).
///
/// A value unlinked from a table is freed only by the store's limbo, collected under `&mut`
/// of the store, once a grace begun after the unlink has passed. So while a task that began
/// before the grace runs, or while the store is borrowed shared, nothing it found is freed.
#[derive(Clone, Copy)]
pub(crate) struct Held<'a>(PhantomData<&'a ()>);

impl<'a> Held<'a> {
    pub(crate) fn task(_guard: &'a Guard) -> Held<'a> {
        Held(PhantomData)
    }

    /// For the store's own reads through `&'a Store`: only its `&mut` methods free anything.
    pub(crate) fn writer<T: ?Sized>(_store: &'a T) -> Held<'a> {
        Held(PhantomData)
    }
}

struct Level<T> {
    slots: Box<[AtomicPtr<T>]>,
    /// `columns` words a 64 rows, column by column.
    bits: Box<[AtomicU64]>,
    used: AtomicUsize,
}

pub(crate) struct Table<T: Keyed> {
    levels: [OnceLock<Level<T>>; LEVELS],
    /// Slots of level 0, a power of two; level `n` has `first << n`.
    first: usize,
    columns: usize,
    /// It owns what its slots point to.
    _owns: PhantomData<Box<T>>,
}

/// Where a value is: its row, and the value.
pub(crate) type Hit<'a, T> = (u64, &'a T);

impl<T: Keyed> Table<T> {
    /// A table whose first level has `first` slots (rounded up to a power of two, at least 64)
    /// and `columns` flag bits a row.
    pub(crate) fn new(first: usize, columns: usize) -> Table<T> {
        let table = Table {
            levels: [const { OnceLock::new() }; LEVELS],
            first: first.next_power_of_two().max(64),
            columns,
            _owns: PhantomData,
        };
        table.level(0);
        table
    }

    fn cap(&self, level: usize) -> usize {
        self.first << level
    }

    /// The row of level `level`'s first slot.
    fn base(&self, level: usize) -> u64 {
        (self.first as u64) * ((1u64 << level) - 1)
    }

    fn level(&self, level: usize) -> &Level<T> {
        assert!(level < LEVELS, "a table of more than {LEVELS} levels");
        self.levels[level].get_or_init(|| {
            let cap = self.cap(level);
            Level {
                slots: (0..cap).map(|_| AtomicPtr::new(ptr::null_mut())).collect(),
                bits: (0..self.columns * cap / 64).map(|_| AtomicU64::new(0)).collect(),
                used: AtomicUsize::new(0),
            }
        })
    }

    fn made(&self) -> impl Iterator<Item = (usize, &Level<T>)> {
        self.levels.iter().enumerate().map_while(|(at, level)| level.get().map(|it| (at, it)))
    }

    /// Walks `key`'s run in `level` to the key, or to the null or seal that ends the run.
    fn probe<'a>(&'a self, level: &'a Level<T>, key: &T::Key) -> Probe<'a, T> {
        let mask = level.slots.len() - 1;
        let mut at = T::hash(key) as usize & mask;
        for _ in 0..level.slots.len() {
            let found = level.slots[at].load(Ordering::Acquire);
            if found.is_null() {
                return Probe::Empty(at);
            }
            if found == seal() {
                return Probe::Past;
            }
            // SAFETY: a published value is freed only as `Held` says, and the caller holds one
            // for the lifetime it gives out.
            let found = unsafe { &*found };
            if found.key() == *key {
                return Probe::Found(at, found);
            }
            at = (at + 1) & mask;
        }
        Probe::Past
    }

    pub(crate) fn get<'a>(&'a self, _held: Held<'a>, key: &T::Key) -> Option<Hit<'a, T>> {
        for (number, level) in self.made() {
            if let Probe::Found(at, found) = self.probe(level, key) {
                return Some((self.base(number) + at as u64, found));
            }
        }
        None
    }

    /// Publishes `value` unless its key is there already: then it gives `value` back with
    /// what is there. Of inserts of one key racing, exactly one lands.
    pub(crate) fn insert<'a>(&'a self, _held: Held<'a>, value: Box<T>) -> Result<Hit<'a, T>, (Box<T>, Hit<'a, T>)> {
        let key = value.key();
        let raw = Box::into_raw(value);
        let mut number = 0;
        loop {
            let level = self.level(number);
            let at = match self.probe(level, &key) {
                Probe::Found(at, found) => {
                    // SAFETY: `raw` was never published.
                    let value = unsafe { Box::from_raw(raw) };
                    return Err((value, (self.base(number) + at as u64, found)));
                }
                Probe::Past => {
                    number += 1;
                    continue;
                }
                Probe::Empty(at) => at,
            };
            // A level half full takes no more: probes stay short. Its runs are sealed rather
            // than left open, so no inserter of this key can land here after this one moved on.
            let full = level.used.load(Ordering::Relaxed) * 2 >= level.slots.len();
            let put = if full { seal() } else { raw };
            match level.slots[at].compare_exchange(ptr::null_mut(), put, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) if full => number += 1,
                Ok(_) => {
                    level.used.fetch_add(1, Ordering::Relaxed);
                    // SAFETY: published just now; freed only as `Held` says.
                    return Ok((self.base(number) + at as u64, unsafe { &*raw }));
                }
                // Another value or a seal took the slot: probing the level again sees which.
                Err(_) => {}
            }
        }
    }

    /// Swaps the value of `value`'s key for `value` and gives back the old one, which the
    /// caller must hold in its limbo until a grace has passed; gives `value` back when the key
    /// is not there. Only the store calls it, under `&mut` of itself.
    pub(crate) fn replace(&self, value: Box<T>) -> Result<Box<T>, Box<T>> {
        let key = value.key();
        for (_, level) in self.made() {
            if let Probe::Found(at, _) = self.probe(level, &key) {
                let old = level.slots[at].swap(Box::into_raw(value), Ordering::AcqRel);
                // SAFETY: `old` is neither null nor the seal: `probe` found a value in this slot,
                // and a slot holding a value is only swapped for another `Box::into_raw`, never
                // cleared or sealed. The swap unlinked it, and the one caller that swaps (the
                // store, under `&mut`) owns it now; readers that found it keep reading it until
                // the limbo frees it.
                return Ok(unsafe { Box::from_raw(old) });
            }
        }
        Err(value)
    }

    /// Every value with its row.
    pub(crate) fn iter<'a>(&'a self, _held: Held<'a>) -> impl Iterator<Item = Hit<'a, T>> + 'a {
        self.made().flat_map(move |(number, level)| {
            let base = self.base(number);
            level.slots.iter().enumerate().filter_map(move |(at, slot)| {
                let found = slot.load(Ordering::Acquire);
                // SAFETY: as in `probe`.
                (!found.is_null() && found != seal()).then(|| (base + at as u64, unsafe { &*found }))
            })
        })
    }

    pub(crate) fn len(&self) -> usize {
        self.made().map(|(_, level)| level.used.load(Ordering::Relaxed)).sum()
    }

    /// The word and bit of `column` for `row`; `None` past the rows made.
    fn word(&self, row: u64, column: usize) -> Option<(&AtomicU64, u64)> {
        debug_assert!(column < self.columns);
        let (mut number, mut at) = (0, row);
        while at >= self.cap(number) as u64 {
            at -= self.cap(number) as u64;
            number += 1;
            if number >= LEVELS {
                return None;
            }
        }
        let level = self.levels[number].get()?;
        let words = level.slots.len() / 64;
        Some((&level.bits[column * words + (at / 64) as usize], 1 << (at % 64)))
    }

    pub(crate) fn flag(&self, row: u64, column: usize) -> bool {
        self.word(row, column).is_some_and(|(word, bit)| word.load(Ordering::Relaxed) & bit != 0)
    }

    /// Sets a flag: a hint about bytes that never change at a place, so relaxed.
    pub(crate) fn set_flag(&self, row: u64, column: usize) {
        if let Some((word, bit)) = self.word(row, column)
            && word.load(Ordering::Relaxed) & bit == 0
        {
            word.fetch_or(bit, Ordering::Relaxed);
        }
    }

    /// Moves column `from` into column `to` and clears `from`: a new period of reading.
    /// Only the store calls it; a flag set meanwhile lands in one period or the other.
    pub(crate) fn rotate(&self, from: usize, to: usize) {
        for (_, level) in self.made() {
            let words = level.slots.len() / 64;
            for at in 0..words {
                let moved = level.bits[from * words + at].swap(0, Ordering::Relaxed);
                level.bits[to * words + at].store(moved, Ordering::Relaxed);
            }
        }
    }
}

impl<T: Keyed> Drop for Table<T> {
    fn drop(&mut self) {
        for level in self.levels.iter_mut().filter_map(OnceLock::get_mut) {
            for slot in level.slots.iter_mut() {
                let value = *slot.get_mut();
                if !value.is_null() && value != seal() {
                    // SAFETY: the table owns what its slots hold, and nothing reads a table
                    // being dropped.
                    drop(unsafe { Box::from_raw(value) });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Value {
        key: u64,
        n: u64,
    }

    impl Keyed for Value {
        type Key = u64;
        fn key(&self) -> u64 {
            self.key
        }
        fn hash(key: &u64) -> u64 {
            // Clustered on purpose: neighbours probe the same run.
            *key / 3
        }
    }

    fn value(key: u64, n: u64) -> Box<Value> {
        Box::new(Value { key, n })
    }

    #[test]
    fn inserts_are_found_across_levels_and_a_second_of_a_key_is_refused() {
        let table: Table<Value> = Table::new(64, 2);
        let held = Held::writer(&table);
        for key in 0..10_000 {
            assert!(table.insert(held, value(key, key * 2)).is_ok());
        }
        assert!(table.made().count() > 1, "it grew");
        for key in 0..10_000 {
            let (_, found) = table.get(held, &key).unwrap();
            assert_eq!(found.n, key * 2);
        }
        let (back, (_, kept)) = table.insert(held, value(7, 0)).unwrap_err();
        assert_eq!((back.n, kept.n), (0, 14));
        assert!(table.get(held, &10_001).is_none());
        assert_eq!(table.len(), 10_000);
        assert_eq!(table.iter(held).count(), 10_000);
    }

    #[test]
    fn a_replace_keeps_the_row_and_gives_back_the_old_value() {
        let table: Table<Value> = Table::new(64, 1);
        let held = Held::writer(&table);
        let (row, _) = table.insert(held, value(5, 1)).unwrap();
        let old = table.replace(value(5, 2)).unwrap();
        assert_eq!(old.n, 1);
        let (again, found) = table.get(held, &5).unwrap();
        assert_eq!((again, found.n), (row, 2));
        assert!(table.replace(value(6, 0)).is_err());
    }

    #[test]
    fn flags_are_per_row_and_rotate() {
        let table: Table<Value> = Table::new(64, 2);
        let held = Held::writer(&table);
        for key in 0..300 {
            table.insert(held, value(key, 0)).unwrap();
        }
        let rows: Vec<u64> = (0..300).map(|key| table.get(held, &key).unwrap().0).collect();
        table.set_flag(rows[3], 0);
        table.set_flag(rows[299], 0);
        assert!(table.flag(rows[3], 0) && table.flag(rows[299], 0));
        assert!(!table.flag(rows[4], 0) && !table.flag(rows[3], 1));
        table.rotate(0, 1);
        assert!(!table.flag(rows[3], 0) && table.flag(rows[3], 1) && table.flag(rows[299], 1));
        assert!(!table.flag(1 << 50, 0), "past the rows made is clear");
    }

    /// Threads insert overlapping keys at once: each key ends with one value, every insert
    /// that lost found the winner, and every reader during it saw only whole values.
    #[test]
    #[cfg(not(blaze_check))]
    fn racing_inserts_keep_one_value_a_key() {
        use std::{sync::Arc, thread};

        let table: Arc<Table<Value>> = Arc::new(Table::new(64, 0));
        let threads: Vec<_> = (0..8u64)
            .map(|t| {
                let table = table.clone();
                thread::spawn(move || {
                    let guard = blaze_reclaim::enter();
                    let held = Held::task(&guard);
                    let (mut won, mut lost) = (0, 0);
                    for key in 0..20_000u64 {
                        let key = (key * 7 + t * 3) % 20_000;
                        match table.insert(held, value(key, key + 1)) {
                            Ok(_) => won += 1,
                            Err((_, (_, found))) => {
                                assert_eq!(found.n, key + 1);
                                lost += 1;
                            }
                        }
                        if let Some((_, found)) = table.get(held, &((key + 1) % 20_000)) {
                            assert_eq!(found.n, (key + 1) % 20_000 + 1);
                        }
                    }
                    (won, lost)
                })
            })
            .collect();
        let won: u64 = threads.into_iter().map(|it| it.join().unwrap().0).sum();
        let held = Held::writer(&*table);
        for key in 0..20_000 {
            assert_eq!(table.get(held, &key).map(|(_, it)| it.n), Some(key + 1));
        }
        assert_eq!(won, 20_000, "one insert of each key lands");
        assert_eq!(table.len(), 20_000);
        assert_eq!(table.iter(held).count(), 20_000);
    }
}

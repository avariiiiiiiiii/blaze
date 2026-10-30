//! What the store knows of each record beyond its bytes, as bit columns: one bit a record,
//! over the record's row. A row is its slot in the index table, or its number in the delta,
//! so a column is dense and a sweep tests 64 records a word.
//!
//! A read sets flags through `&self` with no lock: each column is atomic words, sized when a
//! row is made (under `&mut`), and a flag already set is only loaded, so reading a record again
//! writes no shared cache line. Cost: [`FLAGS`] bits a row; a test or a set is one word.

use std::sync::atomic::{AtomicU64, Ordering};

/// A record's row in its tier's flag columns.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Row {
    /// Its slot in the mapped index table.
    Slot(u64),
    /// Its number among the records the table does not hold.
    Delta(u64),
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Flag {
    /// Its bytes were hashed and found to be what was recorded. A mapping and the memory of a
    /// pack never change the bytes at a place, so once is enough.
    Verified,
    /// Read since the last [`Flags::rotate`].
    Read,
    /// Read between the two rotations before.
    ReadBefore,
    /// Compressing it saved too little to keep: not tried again.
    Tried,
    /// Made since the last [`Flags::rotate`]: a record is unread for two periods only once it
    /// has been there for two.
    New,
    /// Made between the two rotations before.
    NewBefore,
}

pub(crate) const FLAGS: usize = 6;

/// Flags are hints about bytes that never change at a place, so no ordering is needed.
const RELAXED: Ordering = Ordering::Relaxed;

#[derive(Default, Debug)]
struct Bits(Vec<AtomicU64>);

impl Bits {
    fn get(&self, at: u64) -> bool {
        self.0.get((at / 64) as usize).is_some_and(|word| (word.load(RELAXED) >> (at % 64)) & 1 == 1)
    }

    fn set(&self, at: u64) {
        let word = self.0.get((at / 64) as usize).expect("a row's flags are sized when it is made");
        let bit = 1 << (at % 64);
        if word.load(RELAXED) & bit == 0 {
            word.fetch_or(bit, RELAXED);
        }
    }

    fn fit(&mut self, rows: u64) {
        let words = rows.div_ceil(64) as usize;
        if self.0.len() < words {
            self.0.resize_with(words, || AtomicU64::new(0));
        }
    }

    /// Its bits, leaving it all clear and as long as it was.
    fn take(&mut self) -> Bits {
        Bits(self.0.iter_mut().map(|word| AtomicU64::new(std::mem::take(word.get_mut()))).collect())
    }
}

/// A tier's flag columns.
#[derive(Default, Debug)]
pub(crate) struct Flags {
    slots: [Bits; FLAGS],
    delta: [Bits; FLAGS],
}

impl Flags {
    fn column(&self, row: Row, flag: Flag) -> (&Bits, u64) {
        match row {
            Row::Slot(at) => (&self.slots[flag as usize], at),
            Row::Delta(at) => (&self.delta[flag as usize], at),
        }
    }

    pub(crate) fn get(&self, row: Row, flag: Flag) -> bool {
        let (bits, at) = self.column(row, flag);
        bits.get(at)
    }

    /// Sets `flag` of `row`, which [`Flags::fit`] has sized the columns for.
    pub(crate) fn set(&self, row: Row, flag: Flag) {
        let (bits, at) = self.column(row, flag);
        bits.set(at);
    }

    /// Sizes the columns for `slots` index slots and `delta` delta rows.
    pub(crate) fn fit(&mut self, slots: u64, delta: u64) {
        for bits in &mut self.slots {
            bits.fit(slots);
        }
        for bits in &mut self.delta {
            bits.fit(delta);
        }
    }

    /// A new period of reading: what was read becomes read before, what was made made before.
    pub(crate) fn rotate(&mut self) {
        for space in [&mut self.slots, &mut self.delta] {
            space[Flag::ReadBefore as usize] = space[Flag::Read as usize].take();
            space[Flag::NewBefore as usize] = space[Flag::New as usize].take();
        }
    }

    /// The tier has a new index table and has renumbered its delta: every row is new.
    pub(crate) fn forget(&mut self) {
        *self = Flags::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flag_is_set_per_row_and_space_and_reading_rotates() {
        let mut flags = Flags::default();
        flags.fit(4, 701);
        flags.set(Row::Slot(3), Flag::Read);
        flags.set(Row::Delta(700), Flag::Verified);
        flags.set(Row::Delta(5), Flag::New);
        assert!(flags.get(Row::Slot(3), Flag::Read));
        assert!(!flags.get(Row::Delta(3), Flag::Read));
        assert!(!flags.get(Row::Slot(700), Flag::Verified));
        assert!(flags.get(Row::Delta(700), Flag::Verified));
        assert!(!flags.get(Row::Slot(1 << 40), Flag::Read), "past the end is clear");
        flags.set(Row::Slot(3), Flag::Read);
        flags.rotate();
        assert!(!flags.get(Row::Slot(3), Flag::Read));
        assert!(flags.get(Row::Slot(3), Flag::ReadBefore));
        assert!(!flags.get(Row::Delta(5), Flag::New) && flags.get(Row::Delta(5), Flag::NewBefore));
        assert!(flags.get(Row::Delta(700), Flag::Verified), "rotating keeps the rest");
        flags.set(Row::Slot(3), Flag::Read);
        assert!(flags.get(Row::Slot(3), Flag::Read), "a column rotated out keeps its length");
        flags.rotate();
        flags.rotate();
        assert!(!flags.get(Row::Slot(3), Flag::ReadBefore));
        assert!(!flags.get(Row::Delta(5), Flag::NewBefore));
        flags.forget();
        assert!(!flags.get(Row::Delta(700), Flag::Verified));
    }

    #[test]
    fn flags_are_set_from_many_threads_at_once() {
        let mut flags = Flags::default();
        flags.fit(0, 64 * 64);
        std::thread::scope(|scope| {
            for thread in 0..8 {
                let flags = &flags;
                scope.spawn(move || {
                    for row in (thread..64 * 64).step_by(8) {
                        flags.set(Row::Delta(row), Flag::Read);
                    }
                });
            }
        });
        assert!((0..64 * 64).all(|row| flags.get(Row::Delta(row), Flag::Read)));
    }
}

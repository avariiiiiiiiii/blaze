//! What the store knows of each record beyond its bytes, as bit columns: one bit a record,
//! over the record's row. A row is its slot in the index table, or its row in a delta table
//! (`table.rs`, which keeps its rows' bits beside its slots), so a column is dense and a sweep
//! tests 64 records a word.
//!
//! A read sets a flag with no lock (docs/v2/plan/design/concurrency.md "Reads"): each column is
//! atomic words, as many as the table has rows when it is made, and a flag already set is only
//! loaded, so reading a record again writes no shared cache line. The words sit on a region
//! tagged `Tag::Store` (`words.rs`), committed whole when the columns are made.

use std::fmt;

use crate::{
    sync::atomic::{AtomicU64, Ordering},
    words::Words,
};

/// A record's row in its tier's flag columns.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Row {
    /// Its slot in the mapped index table.
    Slot(u64),
    /// Its row in the delta table. A settle, the one user of rows, does not run while an index
    /// swap leaves a delta before it.
    Delta(u64),
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Flag {
    /// Its bytes were hashed and found to be what was recorded. A mapping never changes the
    /// bytes at a place, so once is enough. A delta record keeps this on its spot instead,
    /// since its place changes when it moves from a segment to a pack.
    Verified,
    /// Read since the last rotation.
    Read,
    /// Read between the two rotations before.
    ReadBefore,
    /// Compressing it saved too little to keep: not tried again.
    Tried,
    /// Made (put, or moved by a settle) since the last rotation.
    New,
    /// Made between the two rotations before.
    NewBefore,
}

pub(crate) const FLAGS: usize = 6;

/// A flag is a hint about bytes that never change at a place, so no ordering is needed.
const RELAXED: Ordering = Ordering::Relaxed;

/// The flag columns of a fixed number of rows (an index table's slots).
pub(crate) struct Bits {
    /// `FLAGS` columns of `words` words each, one stretch.
    bits: Words<AtomicU64>,
    words: usize,
}

impl fmt::Debug for Bits {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Bits").field("rows", &(self.words * 64)).finish_non_exhaustive()
    }
}

impl Bits {
    pub(crate) fn new(rows: u64) -> Bits {
        let words = rows.div_ceil(64) as usize;
        let bits = Words::new(FLAGS * words);
        bits.make(0, 0, FLAGS * words);
        Bits { bits, words }
    }

    fn all(&self) -> &[AtomicU64] {
        self.bits.get(0, 0, FLAGS * self.words)
    }

    fn word(&self, row: u64, flag: Flag) -> Option<(&AtomicU64, u64)> {
        let at = usize::try_from(row / 64).ok().filter(|&at| at < self.words)?;
        Some((&self.all()[flag as usize * self.words + at], 1 << (row % 64)))
    }

    pub(crate) fn get(&self, row: u64, flag: Flag) -> bool {
        self.word(row, flag).is_some_and(|(word, bit)| word.load(RELAXED) & bit != 0)
    }

    /// Sets `flag` of `row`; a row past the table's is none of its records and is left.
    pub(crate) fn set(&self, row: u64, flag: Flag) {
        if let Some((word, bit)) = self.word(row, flag)
            && word.load(RELAXED) & bit == 0
        {
            word.fetch_or(bit, RELAXED);
        }
    }

    /// A new period of reading: what was read becomes read before, what was made made before.
    /// Only the store calls it; a flag set meanwhile lands in one period or the other.
    pub(crate) fn rotate(&self) {
        let bits = self.all();
        for (from, to) in [(Flag::Read, Flag::ReadBefore), (Flag::New, Flag::NewBefore)] {
            for at in 0..self.words {
                let moved = bits[from as usize * self.words + at].swap(0, RELAXED);
                bits[to as usize * self.words + at].store(moved, RELAXED);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flag_is_set_per_row_and_reading_rotates() {
        let bits = Bits::new(701);
        bits.set(3, Flag::Read);
        bits.set(700, Flag::Verified);
        bits.set(5, Flag::New);
        assert!(bits.get(3, Flag::Read));
        assert!(!bits.get(3, Flag::Verified));
        assert!(bits.get(700, Flag::Verified));
        assert!(!bits.get(1 << 40, Flag::Read), "past the end is clear");
        bits.set(1 << 40, Flag::Read);
        bits.rotate();
        assert!(!bits.get(3, Flag::Read));
        assert!(bits.get(3, Flag::ReadBefore));
        assert!(!bits.get(5, Flag::New) && bits.get(5, Flag::NewBefore));
        assert!(bits.get(700, Flag::Verified), "rotating keeps the rest");
        bits.rotate();
        assert!(!bits.get(3, Flag::ReadBefore));
    }

    /// Columns of a table big enough to span many commit steps answer as a `Vec<bool>` a flag
    /// does, through sets, rotations and rows past the end.
    #[test]
    #[cfg(not(blaze_check))]
    fn the_columns_answer_as_vecs_of_bools_do() {
        let rows = 3 << 20;
        let bits = Bits::new(rows);
        let flags = [Flag::Verified, Flag::Read, Flag::ReadBefore, Flag::Tried, Flag::New, Flag::NewBefore];
        let mut model = vec![vec![false; rows as usize]; FLAGS];
        let mut rng = blaze_ids::rng::SplitMix64::new(5);
        for step in 0..100_000u64 {
            let row = rng.next_u64() % (rows + 64);
            let flag = flags[(rng.next_u64() % FLAGS as u64) as usize];
            if step % 9 == 0 {
                bits.set(row, flag);
                if row < rows {
                    model[flag as usize][row as usize] = true;
                }
            } else if step % 10_007 == 0 {
                bits.rotate();
                for (from, to) in [(Flag::Read, Flag::ReadBefore), (Flag::New, Flag::NewBefore)] {
                    model[to as usize] = std::mem::replace(&mut model[from as usize], vec![false; rows as usize]);
                }
            } else {
                let want = row < rows && model[flag as usize][row as usize];
                assert_eq!(bits.get(row, flag), want, "step {step}: row {row} {flag:?}");
            }
        }
    }

    #[test]
    #[cfg(not(blaze_check))]
    fn flags_set_from_many_threads_at_once_all_land() {
        use std::{sync::Arc, thread};

        let bits = Arc::new(Bits::new(64 * 64));
        let threads: Vec<_> = (0..8u64)
            .map(|t| {
                let bits = bits.clone();
                thread::spawn(move || {
                    for row in (t..64 * 64).step_by(8) {
                        bits.set(row, Flag::Read);
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert!((0..64 * 64).all(|row| bits.get(row, Flag::Read)));
    }
}

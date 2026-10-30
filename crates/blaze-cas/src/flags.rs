//! What the store knows of each record beyond its bytes, as bit columns: one bit a record,
//! over the record's row. A row is its slot in the index table, or its number in the delta,
//! so a column is dense and a sweep tests 64 records a word.

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

const FLAGS: usize = 6;

#[derive(Clone, Default, Debug)]
pub(crate) struct Bits(Vec<u64>);

impl Bits {
    pub(crate) fn get(&self, at: u64) -> bool {
        self.0.get((at / 64) as usize).is_some_and(|word| (word >> (at % 64)) & 1 == 1)
    }

    pub(crate) fn set(&mut self, at: u64) {
        let word = (at / 64) as usize;
        if word >= self.0.len() {
            self.0.resize(word + 1, 0);
        }
        self.0[word] |= 1 << (at % 64);
    }
}

/// A tier's flag columns.
#[derive(Default, Debug)]
pub(crate) struct Flags {
    slots: [Bits; FLAGS],
    delta: [Bits; FLAGS],
}

impl Flags {
    fn column(&mut self, row: Row, flag: Flag) -> (&mut Bits, u64) {
        match row {
            Row::Slot(at) => (&mut self.slots[flag as usize], at),
            Row::Delta(at) => (&mut self.delta[flag as usize], at),
        }
    }

    pub(crate) fn get(&self, row: Row, flag: Flag) -> bool {
        match row {
            Row::Slot(at) => self.slots[flag as usize].get(at),
            Row::Delta(at) => self.delta[flag as usize].get(at),
        }
    }

    pub(crate) fn set(&mut self, row: Row, flag: Flag) {
        let (bits, at) = self.column(row, flag);
        bits.set(at);
    }

    /// A new period of reading: what was read becomes read before, what was made made before.
    pub(crate) fn rotate(&mut self) {
        for space in [&mut self.slots, &mut self.delta] {
            space[Flag::ReadBefore as usize] = std::mem::take(&mut space[Flag::Read as usize]);
            space[Flag::NewBefore as usize] = std::mem::take(&mut space[Flag::New as usize]);
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
        flags.set(Row::Slot(3), Flag::Read);
        flags.set(Row::Delta(700), Flag::Verified);
        flags.set(Row::Delta(5), Flag::New);
        assert!(flags.get(Row::Slot(3), Flag::Read));
        assert!(!flags.get(Row::Delta(3), Flag::Read));
        assert!(!flags.get(Row::Slot(700), Flag::Verified));
        assert!(flags.get(Row::Delta(700), Flag::Verified));
        assert!(!flags.get(Row::Slot(1 << 40), Flag::Read), "past the end is clear");
        flags.rotate();
        assert!(!flags.get(Row::Slot(3), Flag::Read));
        assert!(flags.get(Row::Slot(3), Flag::ReadBefore));
        assert!(!flags.get(Row::Delta(5), Flag::New) && flags.get(Row::Delta(5), Flag::NewBefore));
        assert!(flags.get(Row::Delta(700), Flag::Verified), "rotating keeps the rest");
        flags.rotate();
        assert!(!flags.get(Row::Slot(3), Flag::ReadBefore));
        assert!(!flags.get(Row::Delta(5), Flag::NewBefore));
        flags.forget();
        assert!(!flags.get(Row::Delta(700), Flag::Verified));
    }
}

//! A dense bitset over `0..len`, a `u64` per 64 indices: the visited, settled and assigned
//! sets of the MIR lints, indexed by block or local. O(1) insert and lookup, `len / 8` bytes.

#[derive(Default)]
pub(crate) struct Bits(Vec<u64>);

impl Bits {
    pub(crate) fn new(len: usize) -> Bits {
        Bits(vec![0; len.div_ceil(64)])
    }

    /// Empties it and sizes it for `0..len`, keeping its buffer.
    pub(crate) fn reset(&mut self, len: usize) {
        self.0.clear();
        self.0.resize(len.div_ceil(64), 0);
    }

    /// Adds `at`; whether it was not there.
    pub(crate) fn insert(&mut self, at: usize) -> bool {
        let (word, bit) = (at / 64, 1u64 << (at % 64));
        let added = (self.0[word] & bit) == 0;
        self.0[word] |= bit;
        added
    }

    pub(crate) fn remove(&mut self, at: usize) {
        self.0[at / 64] &= !(1u64 << (at % 64));
    }

    pub(crate) fn contains(&self, at: usize) -> bool {
        (self.0[at / 64] & (1u64 << (at % 64))) != 0
    }
}

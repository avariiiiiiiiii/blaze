//! A set of dense `u32` indexes (nodes, packages) as one `u64` per 64 of them.

#[derive(Debug)]
pub(crate) struct Bits(Vec<u64>);

impl Bits {
    /// A set of `0..len`, empty.
    pub(crate) fn new(len: usize) -> Bits {
        let mut words = Vec::with_capacity(len.div_ceil(64));
        words.resize(len.div_ceil(64), 0);
        Bits(words)
    }

    pub(crate) fn contains(&self, index: usize) -> bool {
        self.0.get(index / 64).is_some_and(|word| word & (1 << (index % 64)) != 0)
    }

    /// Adds `index`, growing the set if it is past the end; whether it was not there yet.
    pub(crate) fn insert(&mut self, index: usize) -> bool {
        let word = index / 64;
        if word >= self.0.len() {
            self.0.resize(word + 1, 0);
        }
        let bit = 1 << (index % 64);
        let new = self.0[word] & bit == 0;
        self.0[word] |= bit;
        new
    }

    pub(crate) fn remove(&mut self, index: usize) {
        if let Some(word) = self.0.get_mut(index / 64) {
            *word &= !(1 << (index % 64));
        }
    }
}

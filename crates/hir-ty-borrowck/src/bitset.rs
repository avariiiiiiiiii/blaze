//! Fixed-size sets of small integers as `u64` words, for the dataflow analyses: one set, and a
//! matrix of them with a row per block in one allocation.

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub(super) struct BitSet {
    words: Vec<u64>,
}

/// How many words a set of `size` bits takes.
pub(super) fn words_for(size: usize) -> usize {
    size.div_ceil(64)
}

impl BitSet {
    pub(super) fn new(size: usize) -> Self {
        BitSet { words: vec![0; words_for(size)] }
    }

    /// The set of every bit below `size`.
    pub(super) fn filled(size: usize) -> Self {
        let mut out = BitSet { words: vec![!0; words_for(size)] };
        if !size.is_multiple_of(64)
            && let Some(last) = out.words.last_mut()
        {
            *last = (1 << (size % 64)) - 1;
        }
        out
    }

    pub(super) fn contains(&self, bit: usize) -> bool {
        self.words.get(bit / 64).is_some_and(|word| word & (1 << (bit % 64)) != 0)
    }

    pub(super) fn insert(&mut self, bit: usize) -> bool {
        let word = &mut self.words[bit / 64];
        let before = *word;
        *word |= 1 << (bit % 64);
        before != *word
    }

    pub(super) fn remove(&mut self, bit: usize) {
        self.words[bit / 64] &= !(1 << (bit % 64));
    }

    /// Adds the bits of `other`, and says whether that added any.
    pub(super) fn union(&mut self, other: &BitSet) -> bool {
        union_words(&mut self.words, &other.words)
    }

    pub(super) fn clear(&mut self) {
        self.words.fill(0);
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        iter_words(&self.words)
    }

    pub(super) fn words(&self) -> &[u64] {
        &self.words
    }

    pub(super) fn words_mut(&mut self) -> &mut [u64] {
        &mut self.words
    }

    /// Makes this the set whose words are `words`, reusing its allocation.
    pub(super) fn load(&mut self, words: &[u64]) {
        self.words.clear();
        self.words.extend_from_slice(words);
    }
}

/// `into |= from`, saying whether that added any bit.
pub(super) fn union_words(into: &mut [u64], from: &[u64]) -> bool {
    let mut changed = 0;
    for (word, other) in into.iter_mut().zip(from) {
        changed |= *other & !*word;
        *word |= other;
    }
    changed != 0
}

pub(super) fn iter_words(words: &[u64]) -> impl Iterator<Item = usize> + '_ {
    words.iter().enumerate().flat_map(|(index, &word)| {
        let mut word = word;
        std::iter::from_fn(move || {
            if word == 0 {
                return None;
            }
            let bit = word.trailing_zeros() as usize;
            word &= word - 1;
            Some(index * 64 + bit)
        })
    })
}

/// A set of `columns` bits per row, every row in one array: the entry sets of every block.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub(super) struct BitMatrix {
    words: usize,
    data: Vec<u64>,
}

impl BitMatrix {
    pub(super) fn new(rows: usize, columns: usize) -> Self {
        let words = words_for(columns);
        BitMatrix { words, data: vec![0; rows * words] }
    }

    pub(super) fn row(&self, row: usize) -> &[u64] {
        &self.data[row * self.words..(row + 1) * self.words]
    }

    pub(super) fn row_mut(&mut self, row: usize) -> &mut [u64] {
        &mut self.data[row * self.words..(row + 1) * self.words]
    }

    pub(super) fn insert(&mut self, row: usize, bit: usize) {
        self.row_mut(row)[bit / 64] |= 1 << (bit % 64);
    }
}

#[cfg(test)]
mod tests {
    use super::{BitMatrix, BitSet, iter_words};

    #[test]
    fn filled_holds_exactly_the_bits_below_its_size() {
        for size in [0, 1, 63, 64, 65, 127, 128, 200] {
            let set = BitSet::filled(size);
            assert_eq!(set.iter().collect::<Vec<_>>(), (0..size).collect::<Vec<_>>(), "{size}");
            assert_eq!(set.words().len(), size.div_ceil(64));
        }
    }

    #[test]
    fn union_says_whether_it_added_a_bit() {
        let mut a = BitSet::new(130);
        let mut b = BitSet::new(130);
        b.insert(3);
        b.insert(129);
        assert!(a.union(&b));
        assert!(!a.union(&b));
        assert_eq!(a.iter().collect::<Vec<_>>(), vec![3, 129]);
        a.remove(3);
        let mut c = a.clone();
        c.load(b.words());
        assert_eq!(c, b);
    }

    #[test]
    fn matrix_rows_are_independent() {
        let mut m = BitMatrix::new(3, 70);
        m.insert(1, 69);
        m.insert(2, 0);
        assert_eq!(m.words, 2);
        assert_eq!(iter_words(m.row(0)).collect::<Vec<_>>(), Vec::<usize>::new());
        assert_eq!(iter_words(m.row(1)).collect::<Vec<_>>(), vec![69]);
        assert_eq!(iter_words(m.row(2)).collect::<Vec<_>>(), vec![0]);
    }
}

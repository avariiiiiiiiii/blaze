//! Fixed-size sets of small integers as `u64` words, for the dataflow analyses: one set, and a
//! matrix of them with a row per block in one allocation. Ranges are set and cleared a word at
//! a time, so a move path's subtree (a contiguous range of path ids) costs a few word ops.

use std::ops::Range;

use crate::mem::Frame;

#[derive(PartialEq, Eq, Debug, Default)]
pub(super) struct BitSet<'f> {
    words: &'f mut [u64],
}

/// How many words a set of `size` bits takes.
pub(super) fn words_for(size: usize) -> usize {
    size.div_ceil(64)
}

/// Calls `f(word, mask)` for each word `range` touches, `mask` its bits in the range.
#[inline]
pub(super) fn masks(range: Range<usize>, mut f: impl FnMut(usize, u64)) {
    if range.start >= range.end {
        return;
    }
    let (first, last) = (range.start / 64, (range.end - 1) / 64);
    let low = !0u64 << (range.start % 64);
    let high = !0u64 >> (63 - (range.end - 1) % 64);
    if first == last {
        f(first, low & high);
        return;
    }
    f(first, low);
    for word in first + 1..last {
        f(word, !0);
    }
    f(last, high);
}

/// What a statement does to a set: adds and removes bits, whatever is in it. Applied to a
/// [`BitSet`] as a walk goes, or composed into a block's gen and kill sets
/// ([`crate::dataflow::GenKill::row`]), from the one function either way.
pub(super) trait Transfer {
    fn gen_range(&mut self, range: Range<usize>);
    fn kill_range(&mut self, range: Range<usize>);

    #[inline]
    fn gen_(&mut self, bit: usize) {
        self.gen_range(bit..bit + 1);
    }

    #[inline]
    fn kill(&mut self, bit: usize) {
        self.kill_range(bit..bit + 1);
    }
}

impl<'f> BitSet<'f> {
    pub(super) fn new(frame: &'f Frame, size: usize) -> Self {
        BitSet { words: frame.alloc_slice_fill_with(words_for(size), |_| 0) }
    }

    /// An empty set of a row's size of `matrix`, to [`BitSet::load`] its rows into.
    pub(super) fn for_rows_of(frame: &'f Frame, matrix: &BitMatrix<'_>) -> Self {
        BitSet { words: frame.alloc_slice_fill_with(matrix.words, |_| 0) }
    }

    /// The set of every bit below `size`.
    pub(super) fn filled(frame: &'f Frame, size: usize) -> Self {
        let mut out = BitSet::new(frame, size);
        out.gen_range(0..size);
        out
    }

    /// A copy in `frame`.
    pub(super) fn clone_in(&self, frame: &'f Frame) -> Self {
        BitSet { words: frame.alloc_slice_copy(self.words) }
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
    pub(super) fn union(&mut self, other: &BitSet<'_>) -> bool {
        union_words(self.words, other.words)
    }

    pub(super) fn clear(&mut self) {
        self.words.fill(0);
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        iter_words(self.words)
    }

    /// The smallest bit of the set in `range`.
    pub(super) fn first_in(&self, range: Range<usize>) -> Option<usize> {
        let mut found = None;
        masks(range, |word, mask| {
            if found.is_none() {
                let hit = self.words[word] & mask;
                if hit != 0 {
                    found = Some(word * 64 + hit.trailing_zeros() as usize);
                }
            }
        });
        found
    }

    pub(super) fn words(&self) -> &[u64] {
        self.words
    }

    pub(super) fn words_mut(&mut self) -> &mut [u64] {
        self.words
    }

    /// Makes this the set whose words are `words`, which are as many as its own (a row of a
    /// matrix of its size).
    pub(super) fn load(&mut self, words: &[u64]) {
        self.words.copy_from_slice(words);
    }
}

impl Transfer for BitSet<'_> {
    #[inline]
    fn gen_range(&mut self, range: Range<usize>) {
        masks(range, |word, mask| self.words[word] |= mask);
    }

    #[inline]
    fn kill_range(&mut self, range: Range<usize>) {
        masks(range, |word, mask| self.words[word] &= !mask);
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

/// A set of `columns` bits per row, every row in one array: the entry sets of every block,
/// `columns / 64` words a row, a row found by one multiply.
#[derive(PartialEq, Eq, Debug, Default)]
pub(super) struct BitMatrix<'f> {
    words: usize,
    data: &'f mut [u64],
}

impl<'f> BitMatrix<'f> {
    pub(super) fn new(frame: &'f Frame, rows: usize, columns: usize) -> Self {
        let words = words_for(columns);
        BitMatrix { words, data: frame.alloc_slice_fill_with(rows * words, |_| 0) }
    }

    pub(super) fn row(&self, row: usize) -> &[u64] {
        &self.data[row * self.words..(row + 1) * self.words]
    }

    pub(super) fn row_mut(&mut self, row: usize) -> &mut [u64] {
        &mut self.data[row * self.words..(row + 1) * self.words]
    }

    pub(super) fn contains(&self, row: usize, bit: usize) -> bool {
        self.row(row).get(bit / 64).is_some_and(|word| word & (1 << (bit % 64)) != 0)
    }

    pub(super) fn insert(&mut self, row: usize, bit: usize) {
        self.row_mut(row)[bit / 64] |= 1 << (bit % 64);
    }

    /// Row `into` gains the bits of row `from`; says whether it gained any.
    pub(super) fn union_rows(&mut self, into: usize, from: usize) -> bool {
        if into == from {
            return false;
        }
        let words = self.words;
        let (into, from) = if into < from {
            let (low, high) = self.data.split_at_mut(from * words);
            (&mut low[into * words..(into + 1) * words], &high[..words])
        } else {
            let (low, high) = self.data.split_at_mut(into * words);
            (&mut high[..words], &low[from * words..(from + 1) * words])
        };
        union_words(into, from)
    }
}

#[cfg(test)]
mod tests {
    use blaze_ids::rng::SplitMix64;
    use blaze_mem::{Tag, frame};

    use super::{BitMatrix, BitSet, Transfer, iter_words};

    #[test]
    fn filled_holds_exactly_the_bits_below_its_size() {
        frame::task(Tag::Mir, |frame| {
            for size in [0, 1, 63, 64, 65, 127, 128, 200] {
                let set = BitSet::filled(frame, size);
                assert_eq!(set.iter().collect::<Vec<_>>(), (0..size).collect::<Vec<_>>(), "{size}");
                assert_eq!(set.words().len(), size.div_ceil(64));
            }
        });
    }

    #[test]
    fn ranges_match_a_bit_at_a_time() {
        frame::task(Tag::Mir, |frame| {
            let mut rng = SplitMix64::new(0x9e3779b97f4a7c15);
            for _ in 0..2000 {
                let size = 1 + rng.index(300);
                let mut fast = BitSet::new(frame, size);
                let mut slow = vec![false; size];
                for _ in 0..8 {
                    let start = rng.index(size + 1);
                    let end = start + rng.index(size + 1 - start);
                    let on = rng.index(2) == 0;
                    if on {
                        fast.gen_range(start..end);
                    } else {
                        fast.kill_range(start..end);
                    }
                    slow[start..end].fill(on);
                    let start = rng.index(size + 1);
                    let end = start + rng.index(size + 1 - start);
                    let want = (start..end).find(|&it| slow[it]);
                    assert_eq!(fast.first_in(start..end), want);
                }
                let want: Vec<usize> = (0..size).filter(|&it| slow[it]).collect();
                assert_eq!(fast.iter().collect::<Vec<_>>(), want);
            }
        });
    }

    #[test]
    fn union_says_whether_it_added_a_bit() {
        frame::task(Tag::Mir, |frame| {
            let mut a = BitSet::new(frame, 130);
            let mut b = BitSet::new(frame, 130);
            b.insert(3);
            b.insert(129);
            assert!(a.union(&b));
            assert!(!a.union(&b));
            assert_eq!(a.iter().collect::<Vec<_>>(), vec![3, 129]);
            a.remove(3);
            let mut c = a.clone_in(frame);
            c.load(b.words());
            assert_eq!(c, b);
        });
    }

    #[test]
    fn matrix_rows_are_independent() {
        frame::task(Tag::Mir, |frame| {
            let mut m = BitMatrix::new(frame, 3, 70);
            m.insert(1, 69);
            m.insert(2, 0);
            assert_eq!(m.words, 2);
            assert!(m.contains(1, 69) && !m.contains(0, 69));
            assert_eq!(iter_words(m.row(0)).collect::<Vec<_>>(), Vec::<usize>::new());
            assert_eq!(iter_words(m.row(1)).collect::<Vec<_>>(), vec![69]);
            assert!(m.union_rows(0, 2) && !m.union_rows(0, 2) && m.union_rows(2, 1));
            assert_eq!(iter_words(m.row(0)).collect::<Vec<_>>(), vec![0]);
            assert_eq!(iter_words(m.row(2)).collect::<Vec<_>>(), vec![0, 69]);
        });
    }

    #[test]
    fn a_set_and_a_matrix_take_no_global_memory() {
        frame::task(Tag::Mir, |frame| {
            let _ = BitSet::new(frame, 1);
            let _ = BitMatrix::new(frame, 1, 1);
            blaze_alloc_count::assert_at_most(0, "a bitset and a matrix in the frame", || {
                let mut set = BitSet::new(frame, 1000);
                let mut matrix = BitMatrix::new(frame, 64, 1000);
                set.gen_range(3..900);
                matrix.insert(63, 999);
                set.load(matrix.row(63));
                assert!(set.contains(999));
            });
        });
    }
}

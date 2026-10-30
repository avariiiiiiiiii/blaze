//! Sets of lints as bitsets over [`LintId`]: a group's members, the lints some level is set
//! for in a crate, the command line's lints at one level.

use super::{LintId, index::LINT_COUNT};

const WORDS: usize = LINT_COUNT.div_ceil(64);

/// A set of lints: bit `id % 64` of word `id / 64`. 8 bytes for every 64 lints (40 for
/// rustc's 270 and blaze's); a test, an insert or a removal is one load and one bit operation.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct LintSet([u64; WORDS]);

const _: () = assert!(size_of::<LintSet>() == 8 * LINT_COUNT.div_ceil(64));

impl LintSet {
    pub const EMPTY: LintSet = LintSet([0; WORDS]);

    #[inline]
    fn at(id: LintId) -> (usize, u64) {
        let id = usize::from(id.0);
        (id / 64, 1 << (id % 64))
    }

    #[inline]
    pub fn contains(&self, id: LintId) -> bool {
        let (word, bit) = Self::at(id);
        self.0[word] & bit != 0
    }

    #[inline]
    pub fn insert(&mut self, id: LintId) {
        let (word, bit) = Self::at(id);
        self.0[word] |= bit;
    }

    #[inline]
    pub fn remove(&mut self, id: LintId) {
        let (word, bit) = Self::at(id);
        self.0[word] &= !bit;
    }

    pub fn is_empty(&self) -> bool {
        self.0.iter().all(|&it| it == 0)
    }

    /// The lints of the set, in the order of their ids.
    pub fn iter(&self) -> impl Iterator<Item = LintId> + '_ {
        self.0.iter().enumerate().flat_map(|(word, &bits)| {
            let mut rest = bits;
            std::iter::from_fn(move || {
                if rest == 0 {
                    return None;
                }
                let bit = rest.trailing_zeros() as usize;
                rest &= rest - 1;
                Some(LintId((word * 64 + bit) as u16))
            })
        })
    }
}

impl Default for LintSet {
    fn default() -> Self {
        LintSet::EMPTY
    }
}

impl std::fmt::Debug for LintSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set().entries(self.iter().map(|it| it.lint().name)).finish()
    }
}

impl FromIterator<LintId> for LintSet {
    fn from_iter<I: IntoIterator<Item = LintId>>(ids: I) -> Self {
        let mut set = LintSet::EMPTY;
        for id in ids {
            set.insert(id);
        }
        set
    }
}

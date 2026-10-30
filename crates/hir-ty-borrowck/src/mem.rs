//! The run's frame: every temporary of a borrow check is in it, and goes in one store when the
//! task that runs the check ends. What a check returns to the database is owned instead.

pub(crate) use blaze_mem::frame::Frame;
use rustc_hash::FxBuildHasher;

pub(crate) type FVec<'f, T> = Vec<T, &'f Frame>;
pub(crate) type FMap<'f, K, V> = hashbrown::HashMap<K, V, FxBuildHasher, &'f Frame>;
pub(crate) type FSet<'f, T> = hashbrown::HashSet<T, FxBuildHasher, &'f Frame>;
pub(crate) type FDeque<'f, T> = std::collections::VecDeque<T, &'f Frame>;

#[inline]
pub(crate) fn map<'f, K, V>(frame: &'f Frame) -> FMap<'f, K, V> {
    hashbrown::HashMap::with_hasher_in(FxBuildHasher, frame)
}

#[inline]
pub(crate) fn set<'f, T>(frame: &'f Frame) -> FSet<'f, T> {
    hashbrown::HashSet::with_hasher_in(FxBuildHasher, frame)
}

/// `len` copies of `value` in one allocation of exactly `len`.
#[inline]
pub(crate) fn filled<'f, T: Clone>(frame: &'f Frame, len: usize, value: T) -> FVec<'f, T> {
    let mut out = Vec::with_capacity_in(len, frame);
    out.resize(len, value);
    out
}

/// The items of `iter` in a frame vector, its room the iterator's lower bound to start.
#[inline]
pub(crate) fn collect<'f, T>(frame: &'f Frame, iter: impl IntoIterator<Item = T>) -> FVec<'f, T> {
    let iter = iter.into_iter();
    let mut out = Vec::with_capacity_in(iter.size_hint().0, frame);
    out.extend(iter);
    out
}

/// The items of `iter`, which says its length, as an owned vector of exactly that length: a
/// memoized result built from frame values.
#[inline]
pub(crate) fn owned<T>(iter: impl ExactSizeIterator<Item = T>) -> Vec<T> {
    let mut out = Vec::with_capacity(iter.len()); // alloc: the memoized result, owned
    out.extend(iter);
    out
}

/// `iter`, of `len` items, as an iterator that says so.
pub(crate) struct ExactLen<I> {
    iter: I,
    len: usize,
}

impl<I> ExactLen<I> {
    pub(crate) fn new(iter: I, len: usize) -> Self {
        ExactLen { iter, len }
    }
}

impl<I: Iterator> Iterator for ExactLen<I> {
    type Item = I::Item;

    fn next(&mut self) -> Option<I::Item> {
        let it = self.iter.next()?;
        self.len -= 1;
        Some(it)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.len, Some(self.len))
    }
}

impl<I: Iterator> ExactSizeIterator for ExactLen<I> {}

#[cfg(test)]
mod tests {
    use blaze_mem::{Tag, frame};

    #[test]
    fn frame_collections_hold_what_global_ones_do() {
        frame::task(Tag::Mir, |frame| {
            let mut map = super::map(frame);
            let mut set = super::set(frame);
            for it in 0..1000u32 {
                map.insert(it, it * 2);
                set.insert(it % 37);
            }
            assert_eq!(map.len(), 1000);
            assert_eq!(map[&999], 1998);
            assert_eq!(set.len(), 37);
            let filled = super::filled(frame, 70, 7u64);
            assert_eq!((filled.len(), filled.capacity()), (70, 70));
            let owned = super::owned(filled.iter().copied());
            assert_eq!((owned.len(), owned.capacity()), (70, 70));
            assert!(frame.contains(filled.as_ptr().cast()));
            assert!(!frame.contains(owned.as_ptr().cast()));
        });
    }
}

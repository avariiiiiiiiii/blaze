//! What the resolver works out once a build and the code generator asks again for every unit
//! (docs/v2/plan/design/memory.md, "tables"): a function's symbol, a static's, an item's path,
//! a file's name, a crate's ids. [`Memo::get`] takes no lock and allocates nothing. A first
//! answer is worked out outside any lock and published once; two threads that ask at once may
//! both work it out, and every reader sees the first one published, the same value, since what
//! it is worked out from does not change during the build.
//!
//! Cost of a hit: one FxHash of the key, one probe of a SwissTable group (a word of tags beside
//! its slots, one cache line), one acquire load of the value's ready word, one key compare.

use std::{
    hash::{BuildHasher, Hash},
    sync::Mutex,
};

use blaze_mem::{Column, Tag, swiss};

/// `V`s by `K`, each key filled once: the values in a [`Column`], which never moves, so a `&V`
/// lives as long as the memo; a [`swiss::Index`] over it that readers probe without a lock.
/// `insert` serializes the inserts; no read takes it.
pub(crate) struct Memo<K, V> {
    index: swiss::Index,
    values: Column<(K, V)>,
    insert: Mutex<()>,
}

impl<K: Copy + Eq + Hash, V> Memo<K, V> {
    /// A memo of at most `max` keys: address space for them reserved, nothing committed and
    /// nothing allocated until the first insert. Out of address space stops the build, naming
    /// the tag and sizes.
    pub(crate) fn new(max: usize) -> Memo<K, V> {
        let values = match Column::new(max, Tag::Codegen) {
            Ok(values) => values,
            Err(error) => panic!("{error}"),
        };
        Memo { index: swiss::Index::new(), values, insert: Mutex::new(()) }
    }

    fn hash(key: &K) -> u64 {
        rustc_hash::FxBuildHasher.hash_one(key)
    }

    /// The value published for `key`, if any.
    #[inline]
    pub(crate) fn get(&self, key: K) -> Option<&V> {
        let is = |at: u32| self.values.get(at).is_some_and(|it| it.0 == key);
        let at = self.index.find(Self::hash(&key), is)?;
        self.values.get(at).map(|it| &it.1)
    }

    /// The value for `key`, made by `make` if none is published yet. `make` runs with no lock
    /// held, so it may ask this memo of other keys.
    pub(crate) fn get_or_insert_with(&self, key: K, make: impl FnOnce() -> V) -> &V {
        if let Some(value) = self.get(key) {
            return value;
        }
        let value = make();
        let _held = self.insert.lock().unwrap_or_else(|it| it.into_inner());
        if let Some(value) = self.get(key) {
            return value;
        }
        // Every push is made with `insert` held, so the column's length is the index's.
        let len = self.values.len();
        let at = match self.values.push((key, value)) {
            Ok(at) => at,
            Err(error) => panic!("{error}"),
        };
        let hash_of = |at: u32| match self.values.get(at) {
            Some(it) => Self::hash(&it.0),
            None => unreachable!("an indexed value is published"),
        };
        self.index.insert(len, Self::hash(&key), at, hash_of);
        match self.values.get(at) {
            Some(it) => &it.1,
            None => unreachable!("the value just pushed is published"),
        }
    }

    /// How many keys have a value.
    pub(crate) fn len(&self) -> usize {
        self.values.len()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::Memo;

    #[test]
    fn a_key_is_worked_out_once() {
        let memo: Memo<u32, u64> = Memo::new(1 << 10);
        let made = AtomicUsize::new(0);
        let make = |key: u32| {
            made.fetch_add(1, Ordering::Relaxed);
            u64::from(key) * 3
        };
        assert_eq!(memo.get(7), None);
        assert_eq!(*memo.get_or_insert_with(7, || make(7)), 21);
        assert_eq!(*memo.get_or_insert_with(7, || make(7)), 21);
        assert_eq!(*memo.get_or_insert_with(8, || make(8)), 24);
        assert_eq!(made.load(Ordering::Relaxed), 2);
        assert_eq!(memo.len(), 2);
        assert_eq!(memo.get(7), Some(&21));
    }

    /// Past the index's first levels, every key still finds its own value.
    #[test]
    fn a_memo_that_grew_finds_every_key() {
        let memo: Memo<u32, u32> = Memo::new(1 << 20);
        for key in 0..100_000 {
            memo.get_or_insert_with(key, || key ^ 0x5555);
        }
        for key in 0..100_000 {
            assert_eq!(memo.get(key), Some(&(key ^ 0x5555)));
        }
        assert_eq!(memo.get(100_000), None);
    }

    /// Threads that fill the same keys at once agree on one value each, and no key is in the
    /// column twice.
    #[test]
    fn threads_that_fill_one_key_see_one_value() {
        let memo: Memo<u32, u32> = Memo::new(1 << 16);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    for key in 0..4096 {
                        assert_eq!(*memo.get_or_insert_with(key, || key + 1), key + 1);
                    }
                });
            }
        });
        assert_eq!(memo.len(), 4096);
    }

    /// A hit allocates nothing (docs/v2/plan/design/memory.md, "nothing allocated on a cache
    /// hit").
    #[test]
    fn a_hit_allocates_nothing() {
        let memo: Memo<u32, Option<Box<str>>> = Memo::new(1 << 12);
        for key in 0..1000 {
            memo.get_or_insert_with(key, || Some(format!("symbol{key}").into_boxed_str()));
        }
        blaze_alloc_count::assert_at_most(0, "a memo hit", || {
            for key in 0..1000 {
                assert!(memo.get(key).is_some());
                std::hint::black_box(memo.get_or_insert_with(key, || None));
            }
        });
    }
}

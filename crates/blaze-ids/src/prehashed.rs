//! Maps keyed by something that is a hash already (a [`ContentId`](crate::ContentId), a store
//! key, a 64-bit Fx hash): the key's first word spreads over a table as well as any hash of it
//! would, so a lookup hashes nothing.
//!
//! [`Prehashed`] takes the first eight bytes of each `write` (a `u64` arrives as its eight
//! bytes) and ignores lengths, so a `[u8; 32]` hashes as its first word and a `u64` as itself.
//! A key of fewer than eight bytes per write (a `u32`, a `bool`) hashes to 0: such a key is not
//! a hash and wants another hasher.

/// The hasher of [`ByHash`] and [`ByHashSet`].
#[derive(Default, Clone, Copy)]
pub struct Prehashed(u64);

impl std::hash::Hasher for Prehashed {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }

    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        if let Some(word) = bytes.first_chunk::<8>() {
            self.0 ^= u64::from_le_bytes(*word);
        }
    }

    #[inline]
    fn write_u64(&mut self, word: u64) {
        self.0 ^= word;
    }

    #[inline]
    fn write_usize(&mut self, _: usize) {}
}

/// The `BuildHasher` of [`Prehashed`], for a map built elsewhere (`hashbrown`, `with_hasher`).
pub type BuildPrehashed = std::hash::BuildHasherDefault<Prehashed>;

/// A map on hashes, indexed by the hash itself.
#[expect(clippy::disallowed_types, reason = "Uses a custom hasher")]
pub type ByHash<K, V> = std::collections::HashMap<K, V, BuildPrehashed>; // alloc: not SipHash: hashed by `Prehashed`, the key's first word

/// A set of hashes, indexed by the hash itself.
#[expect(clippy::disallowed_types, reason = "Uses a custom hasher")]
pub type ByHashSet<K> = std::collections::HashSet<K, BuildPrehashed>; // alloc: not SipHash: as above

#[cfg(test)]
mod tests {
    use std::hash::{BuildHasher as _, Hash, Hasher as _};

    use proptest::prelude::*;

    use super::*;
    use crate::ContentId;

    /// blaze-cas's and blaze-global's copy as it was, the reference.
    #[derive(Default)]
    struct Reference(u64);

    impl std::hash::Hasher for Reference {
        fn finish(&self) -> u64 {
            self.0
        }

        fn write(&mut self, bytes: &[u8]) {
            if let Some(word) = bytes.first_chunk::<8>() {
                self.0 ^= u64::from_le_bytes(*word);
            }
        }

        fn write_usize(&mut self, _: usize) {}
    }

    /// blaze-graph's `plane.rs` and blaze-cargo's `print.rs` copy for a `u64` key: the word itself.
    fn word_reference(word: u64) -> u64 {
        word
    }

    fn hash_of(key: &impl Hash) -> u64 {
        BuildPrehashed::default().hash_one(key)
    }

    fn reference_of(key: &impl Hash) -> u64 {
        let mut state = Reference::default();
        key.hash(&mut state);
        state.finish()
    }

    #[test]
    fn a_content_id_hashes_as_its_first_word() {
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&0x0123_4567_89ab_cdefu64.to_le_bytes());
        bytes[8] = 0xff;
        assert_eq!(hash_of(&ContentId(bytes)), 0x0123_4567_89ab_cdef);
    }

    #[test]
    fn a_map_finds_what_it_holds() {
        let mut map: ByHash<ContentId, u32> = ByHash::default();
        let mut set: ByHashSet<u64> = ByHashSet::default();
        for i in 0..64u32 {
            let mut bytes = [0u8; 32];
            bytes[..4].copy_from_slice(&i.to_le_bytes());
            map.insert(ContentId(bytes), i);
            set.insert(u64::from(i) << 40);
        }
        for i in 0..64u32 {
            let mut bytes = [0u8; 32];
            bytes[..4].copy_from_slice(&i.to_le_bytes());
            assert_eq!(map.get(&ContentId(bytes)), Some(&i));
            assert!(set.contains(&(u64::from(i) << 40)));
        }
    }

    #[test]
    fn a_lookup_allocates_nothing() {
        let mut map: ByHash<ContentId, u32> = ByHash::default();
        map.insert(ContentId([7; 32]), 7);
        blaze_alloc_count::assert_at_most(0, "a prehashed lookup", || {
            for _ in 0..100 {
                std::hint::black_box(map.get(&ContentId([7; 32])));
            }
        });
    }

    proptest! {
        #[test]
        fn hashes_as_the_copies_did(bytes in any::<[u8; 32]>(), word in any::<u64>()) {
            prop_assert_eq!(hash_of(&ContentId(bytes)), reference_of(&ContentId(bytes)));
            prop_assert_eq!(hash_of(&bytes), reference_of(&bytes));
            prop_assert_eq!(hash_of(&word), word_reference(word));
            prop_assert_eq!(hash_of(&word), reference_of(&word));
        }
    }
}

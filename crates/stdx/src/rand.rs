//! We don't use `rand` because that is too many things for us.
//!
//! `oorandom` is used instead, but it's missing these two utilities.
//! Switching to `fastrand` or our own small PRNG may be good because only xor-shift is needed.

pub fn shuffle<T>(slice: &mut [T], mut rand_index: impl FnMut(usize) -> usize) {
    let mut remaining = slice.len() - 1;
    while remaining > 0 {
        let index = rand_index(remaining);
        slice.swap(remaining, index);
        remaining -= 1;
    }
}

pub fn seed() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    #[expect(clippy::disallowed_types, reason = "we need a source of randomness for the seed")]
    std::collections::hash_map::RandomState::new().build_hasher().finish()
}

/// Fills `out` with random bytes: the keys of one fresh `RandomState`, which the standard
/// library seeds from the system once per thread and steps per call, hashing the index of each
/// eight bytes.
pub fn fill(out: &mut [u8]) {
    use std::hash::{BuildHasher, Hasher};
    #[expect(clippy::disallowed_types, reason = "we need a source of randomness")]
    let state = std::collections::hash_map::RandomState::new();
    for (index, chunk) in out.chunks_mut(8).enumerate() {
        let mut hasher = state.build_hasher();
        hasher.write_usize(index);
        chunk.copy_from_slice(&hasher.finish().to_le_bytes()[..chunk.len()]);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn fill_fills_every_byte_and_differs_per_call() {
        for len in [0, 1, 7, 8, 9, 64, 1000] {
            let (mut one, mut two) = (vec![0u8; len], vec![0u8; len]);
            super::fill(&mut one);
            super::fill(&mut two);
            if len >= 8 {
                assert_ne!(one, two, "{len} bytes twice");
                assert!(one.iter().any(|&it| it != 0));
            }
        }
    }
}

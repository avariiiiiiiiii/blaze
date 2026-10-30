//! The seeded generator of the random runs: `blaze_ids::rng::SplitMix64`, step for step, so a
//! seed means the same here as in every other crate's tests. It is a copy, not a dependency:
//! `blaze-ids` depends on `blaze-check`. `tests.rs` holds it to SplitMix64's reference outputs.

#[derive(Clone, Debug)]
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    pub(crate) fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n`; 0 when `n` is 0.
    pub(crate) fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next_u64() % n }
    }

    /// True once in `n` on average; never when `n` is 0.
    pub(crate) fn one_in(&mut self, n: u32) -> bool {
        n != 0 && self.below(u64::from(n)) == 0
    }

    /// The items of `items` in an order of the seed's.
    pub(crate) fn shuffle<T>(&mut self, items: &mut [T]) {
        for at in (1..items.len()).rev() {
            let other = self.below(at as u64 + 1) as usize;
            items.swap(at, other);
        }
    }
}

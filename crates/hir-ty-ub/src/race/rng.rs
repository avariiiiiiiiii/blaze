//! The run's randomness: SplitMix64 (Steele, Lea and Flood, "Fast Splittable Pseudorandom Number
//! Generators", 2014), integer arithmetic only, so a seed gives the same schedule and the same
//! weak-memory choices on every machine. Miri uses `StdRng` the same way (`machine.rs`,
//! `MiriMachine::rng`); the sequences differ, the distributions don't.

#[derive(Clone, Debug)]
pub(super) struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    pub(super) fn new(seed: u64) -> SplitMix64 {
        SplitMix64 { state: seed }
    }

    #[inline]
    pub(super) fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// True with probability `p` (rand's `random_bool`): the top 53 bits as a fraction of one.
    #[inline]
    pub(super) fn chance(&mut self, p: f64) -> bool {
        if p <= 0.0 {
            return false;
        }
        if p >= 1.0 {
            return true;
        }
        const SCALE: f64 = 1.0 / (1u64 << 53) as f64;
        ((self.next_u64() >> 11) as f64 * SCALE) < p
    }

    /// Uniform in `0..n`, `n > 0`, by Lemire's multiply-shift.
    #[inline]
    pub(super) fn below(&mut self, n: usize) -> usize {
        debug_assert!(n > 0);
        ((u128::from(self.next_u64()) * n as u128) >> 64) as usize
    }
}

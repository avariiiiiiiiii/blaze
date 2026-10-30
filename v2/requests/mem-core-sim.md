## dedup: seeded RNG onto blaze_ids::rng

From `dedup-codec-rng` (report `/tmp/blaze-lanes/v2-dedup-codec-rng.md`). `blaze_ids::rng::SplitMix64` (`crates/blaze-ids/src/rng.rs`) is the one seeded test RNG: `new`, `next_u64`, `below` (`% n`, 0 for 0), `index`, `one_in`, `shuffle`, `fill`, and more. Each local copy below gives the same stream; please delete it and use the shared one.

`crates/blaze-core/src/sim.rs` keeps its `Rng` (public through `sim::Program`, and `blaze-core` stays below `blake3`: docs/v2/plan/lanes/dedup.md keeps `blaze-radix` separate for that reason). Only its comment changes, so a reader knows it is a kept copy:

Before: `/// SplitMix64: small, fast, and the same everywhere.`
After: `/// SplitMix64, a copy of `blaze_ids::rng::SplitMix64` (the same stream from a seed): `blaze-core` stays below `blake3`, so it can't depend on `blaze-ids`.`

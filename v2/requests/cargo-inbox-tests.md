## dedup: seeded RNG onto blaze_ids::rng

From `dedup-codec-rng` (report `/tmp/blaze-lanes/v2-dedup-codec-rng.md`). `blaze_ids::rng::SplitMix64` (`crates/blaze-ids/src/rng.rs`) is the one seeded test RNG (`new`, `next_u64`, `below` = `% n`, `index`, `fill`, ...); please use it instead of a local copy. The crate already depends on `blaze-ids`.

`crates/blaze-cargo/src/serve/unit/test/tests.rs` (new; I didn't edit it while you write it):

1. Delete `/// splitmix64.` and `struct Rng(u64);` with its `impl Rng { fn below .. }` (an exact SplitMix64 `below`, so the values stay the same).
2. `use blaze_ids::ContentId;` -> `use blaze_ids::{ContentId, rng::SplitMix64 as Rng};`.
3. `Rng(7)` -> `Rng::new(7)`, `Rng(11)` -> `Rng::new(11)`. `fn lines(rng: &mut Rng, ..)` and the `rng.below(..)` calls stay as they are.

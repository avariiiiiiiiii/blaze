## dedup: seeded RNG onto blaze_ids::rng

From `dedup-codec-rng` (report `/tmp/blaze-lanes/v2-dedup-codec-rng.md`). `blaze_ids::rng::SplitMix64` (`crates/blaze-ids/src/rng.rs`) is the one seeded test RNG: `new`, `next_u64`, `below` (`% n`, 0 for 0), `index`, `one_in`, `shuffle`, `fill`, and more. Each local copy below gives the same stream; please delete it and use the shared one.

`crates/span/src/map.rs` test module (`span` already depends on `blaze-ids`):

1. Delete `/// splitmix64: the seeded inputs of the tests below.` and `fn next(state: &mut u64) -> u64 { ... }` (lines 730-737 today).
2. Add `use blaze_ids::rng::SplitMix64;` after `use blaze_ids::ContentId;` (or merge: `use blaze_ids::{ContentId, rng::SplitMix64};`).
3. Mechanically, same values: `let mut state = N;` -> `let mut rng = SplitMix64::new(N);`; a helper parameter `state: &mut u64` -> `rng: &mut SplitMix64`; an argument `&mut state` -> `&mut rng` (and a passed-through `state` -> `rng`); `next(&mut state) % X` (or `next(state) % X`) -> `rng.below(X)`; a bare `next(&mut state)` -> `rng.next_u64()`.

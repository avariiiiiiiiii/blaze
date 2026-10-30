
## dedup: LEB128 and seeded RNGs onto `blaze_format::leb` and `blaze_ids::rng`

From `dedup-codec-leb` (report `/tmp/blaze-lanes/v2-dedup-codec-leb.md`). The one LEB128 is
`crates/blaze-format/src/leb.rs`; its API is listed in `v2/requests/mem-backend-link.md` under
the same heading. The RNG is `blaze_ids::rng::SplitMix64` (`crates/blaze-ids/src/rng.rs`).

- `crates/blaze-macro-wasm/Cargo.toml`: add `blaze-format.workspace = true` to
  `[dependencies]` (it depends only on `blaze-ids`, `serde`, `zstd`: no cycle).
- `src/lib.rs` `carried_list`: the nested `fn leb` becomes
  ```rust
      fn leb(bytes: &[u8], at: &mut usize) -> Option<usize> {
          blaze_format::leb::read_u32(bytes, at).ok().map(|it| it as usize)
      }
  ```
  (wasm's `u32`: at most 5 bytes, as the loop read, and now also refusing bits past 32).

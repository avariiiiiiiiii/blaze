
## mem-backend-cas: `Cargo.lock` for blaze-cas's new dependencies

From `mem-backend-cas` (report `/tmp/blaze-lanes/v2-mem-backend-cas.md`, design
`docs/v2/plan/lanes/mem/backend-cas.md`). `crates/blaze-cas/Cargo.toml` now depends on
`blaze-mem` and, for tests, `blaze-alloc-count`; both are workspace dependencies already. The
lane may not run cargo, so `Cargo.lock` is not updated: its `blaze-cas` entry already lacks
`blaze-reclaim`, `lz4_flex`, `rustc-hash` and `zstd`, so whoever next regenerates the lock file
picks these up with them. Nothing else outside `crates/blaze-cas` changed.

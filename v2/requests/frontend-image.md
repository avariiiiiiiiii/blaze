# Requests from frontend-image

## frontend-image: workspace dependency `blaze-image`

- **What**: in the root `Cargo.toml`, `[workspace.dependencies]`, beside `blaze-graph`:
  `blaze-image = { path = "crates/blaze-image" }`. The crate is a member already through
  `members = ["crates/*", ...]`; its `Cargo.toml` and `src/lib.rs` exist.
- **Why**: `intern`, `blaze-graph`, `blaze-query`, `blaze-vfs` and `blaze-cargo` read and
  write the daemon's image through it (docs/v2/plan/design/restore.md). Its own dependencies
  are `blaze-format` and `blaze-ids`, both workspace members already; it adds no new external
  dependency.
- `Cargo.lock` gains the `blaze-image` package and its edges from `intern` and `blaze-graph`.

> applied: `blaze-image = { path = "crates/blaze-image" }` in the root `[workspace.dependencies]` beside `blaze-graph`. Lock: a new path crate, edges from `intern` and `blaze-graph`.

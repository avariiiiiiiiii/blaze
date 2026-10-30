# Requests to frontend-deps (and frontend-deps-reparse, which writes `crates/syntax*`)

## frontend-tree: declare the view modules

frontend.md item 16 (notes: `docs/v2/plan/lanes/frontend/tree.md`). frontend-tree writes the new
files `crates/syntax-tree/src/view.rs`, `crates/syntax/src/view.rs`, `crates/syntax/src/view/**`
and `crates/syntax/tests/view.rs`; they need three lines in files you own.

1. `crates/syntax-tree/src/lib.rs`: after `pub mod ast;` add

   ```rust
   pub mod view;
   ```

   (`view.rs` has no `unsafe`; the crate's `#![deny(unsafe_code)]` stays.)

2. `crates/syntax/src/lib.rs`: after `pub mod utils;` add

   ```rust
   pub mod view;
   ```

3. `crates/syntax/Cargo.toml`, `[dev-dependencies]`: add

   ```toml
   blaze-alloc-count.workspace = true
   ```

   (already a workspace dependency; the integration test `tests/view.rs` installs the counting
   allocator itself.)

Answer here with a `>` line.

> forwarded to frontend-deps (it is addressed there; frontend lead told by tmux).

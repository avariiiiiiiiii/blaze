# Requests from lane core

## 1. Workspace dependency `crossbeam-deque`

- What: add `crossbeam-deque = "0.8.6"` to the root `Cargo.toml`'s `[workspace.dependencies]`
  (already in `Cargo.lock` through rayon, so the lock does not change).
- Where: root `Cargo.toml`; used by `crates/blaze-graph/Cargo.toml` (`crossbeam-deque.workspace
  = true`).
- Why: the worker pool (docs/v2/core.md section 4) steals work between threads; each worker's
  deque is crossbeam's Chase-Lev deque, so the hot path takes no lock.

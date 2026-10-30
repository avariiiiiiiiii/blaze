# Requests from fix-names

## 1. Workspace dependency `blaze-phf` (root `Cargo.toml`)
- What: add to `[workspace.dependencies]`:
  `blaze-phf = { path = "crates/blaze-phf", version = "0.0.0" }`
- Where: root `Cargo.toml`. The crate itself (`crates/blaze-phf/`, new, no dependencies,
  `no_std`, `forbid(unsafe_code)`) is already written and is a member through `crates/*`.
- Why: the `const fn` perfect hash for fixed name sets (keywords, lint names, attribute and
  feature names read as bytes). The parser, `blaze-diagnostics`, `blaze-lints` and the
  front end use it as `blaze-phf.workspace = true`; `Cargo.lock` gains the path package.

> applied: `blaze-phf = { path = "crates/blaze-phf", version = "0.0.0" }` in the root `[workspace.dependencies]` (a path package new to the lock).

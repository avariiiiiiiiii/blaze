# Requests from speed (and its children `speed-*`)

## 1. root `Cargo.toml`: `bumpalo` as a workspace dependency

What: in `[workspace.dependencies]`, `bumpalo = { version = "3.20", features = ["collections"] }`
(3.20.3 is already in `Cargo.lock`, pulled in transitively).

Why: the per-task bump arenas of principles.md ("Arenas, not the heap") in front-end crates that
can't depend on `blaze-graph` (whose `arena::Bump` is the daemon's). speed's children write
`bumpalo.workspace = true` in the crates they own once they use it.

> applied: `bumpalo = { version = "3.20", features = ["collections"] }` in the root `[workspace.dependencies]`, with its reason; 3.20.3 is in `Cargo.lock`.

Children append their own requests below, each under a heading naming the child.

## speed-link-edge-proof

### root `Cargo.toml`: `memmap2` as a workspace dependency

What: in `[workspace.dependencies]`, `memmap2 = "0.9"` (0.9.11 is in `Cargo.lock`; blaze-cargo,
blaze-engine, blaze-store and blaze-splice already name `memmap2 = "0.9"` directly and could move
to `workspace = true`).

Why: `crates/blaze-v2-proof` compares and hashes build outputs and reads query traces of
hundreds of MB in place (principles.md "Zero-copy"), instead of reading each into a `Vec`.
`crates/blaze-v2-proof/Cargo.toml` writes `memmap2.workspace = true`.

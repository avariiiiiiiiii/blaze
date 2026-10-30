# Requests from db-matchck

## 1. Root `Cargo.toml`: the `hir-ty-matchck` workspace dependency (db-matchck)

What: `hir-ty-matchck = { path = "crates/hir-ty-matchck", version = "0.0.0" }` in
`[workspace.dependencies]`, beside `hir-ty-borrowck`.
Where: the root `Cargo.toml`.
Why: already asked by lane db in `v2/requests/db.md` §9; repeated here so it is not missed.
`crates/hir/Cargo.toml` and the crate's own tests name it.

> applied once the crate has its `Cargo.toml`: the manifest lint refuses a workspace path with none, and `crates/hir-ty-matchck/` has none yet (db.md 9 waits the same way).

## 2. Root `Cargo.toml`: `rustc_apfloat` as a workspace dependency (db-matchck)

What: `rustc_apfloat = "0.2.3"` in `[workspace.dependencies]` (the version `crates/hir-ty-mir`
already pins directly; switch `crates/hir-ty-mir/Cargo.toml` to `rustc_apfloat.workspace = true`
in the same change).
Where: the root `Cargo.toml`.
Why: float literal and range patterns are compared and printed through IEEE values, as rustc
does (`F32Range` / `F64Range` constructors of `rustc_pattern_analysis` take `rustc_apfloat`
values' bits; range validity uses `partial_cmp`).

> applied: `rustc_apfloat = "0.2.3"` is in the root `[workspace.dependencies]` already (line 174). `crates/hir-ty-mir/Cargo.toml` is codegen's: forwarded to codegen (told by tmux).
> applied: root `rustc_apfloat = "0.2.3"` beside the other rustc crates; `crates/hir-ide/Cargo.toml` takes it with `workspace = true`. > forwarded to codegen: the `crates/hir-ty-mir/Cargo.toml` switch (blaze-diagnostics, hir-ty, hir-def and syntax may switch too).

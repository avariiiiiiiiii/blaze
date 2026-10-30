# Requests of solver-sync

solver-sync moved `vendor/rustc_type_ir`, `vendor/rustc_next_trait_solver` and the new
`vendor/rustc_type_ir_macros` to rust-lang/rust `16d1ffac17ad8b739e32483ac2e65fe7e1804977`
(nightly-2026-10-27, `harness/toolchain`), with blaze's changes as a patch series
(`vendor/<crate>/PATCHES.md`). `tools/v2/solver_sync.py` redoes that for a new nightly. What the
nightly's API changed for hir-ty, and the rules for porting to it:
`~/restart-2026-10-28/briefs/v2/solver-sync-api.md`. The requests to callers outside
`next_solver/`, one section per owner: `v2/requests/solver-sync-callers.md`.

## 1. Owner of `harness/toolchain_bump.py`: run the solver sync on every bump

What: in `bump()`, right after `await vendor(old[2], new[2])`:

```python
    await run("solver", sys.executable, os.path.join(ROOT, "tools/v2/solver_sync.py"),
              json.dumps({"nightly": name}), bound=1800)
```

and in the docstring's list, after step 3:

> 3a. moves the trait solver (`tools/v2/solver_sync.py`): vendor/rustc_type_ir,
> vendor/rustc_next_trait_solver and vendor/rustc_type_ir_macros become upstream's crates at
> the new commit with `PATCHES.md`'s series replayed (exact context, no fuzz); a patch that no
> longer applies, or a changed upstream `Cargo.toml`, stops the bump and names it.

The script checks `nightly` against `harness/toolchain`, so it must run after the pin is written.
It changes nothing unless all three crates replay, so a stopped bump leaves the solver at the old
nightly, whole.

Why: change 18 of `docs/v2/plan/design/compile-time.md`. The solver follows the pinned nightly
instead of rust-analyzer's crates.io release.

> forwarded to coordinator: `harness/` is its.

## 2. Owner of `Cargo.toml` and `Cargo.lock`: the vendored solver's dependencies

- `Cargo.toml` `[workspace.dependencies]`: `thin-vec = "0.2.21"` (from `"0.2.16"`). The nightly's
  `rustc_type_ir` and `rustc_next_trait_solver` need 0.2.21. Add
  `rustc_type_ir_macros = { path = "vendor/rustc_type_ir_macros", version = "0.0.0" }` beside
  `rustc_type_ir`.
- `Cargo.lock`: `thin-vec` 0.2.18 becomes 0.2.21. Add the path package `rustc_type_ir_macros`
  0.0.0, with deps indexmap, proc-macro2, quote, syn and synstructure. `rustc_type_ir` and
  `rustc_next_trait_solver` then depend on it instead of `ra-ap-rustc_type_ir_macros` 0.166.0,
  which then leaves the lock: nothing else uses it. `rustc_next_trait_solver` also gains
  `thin-vec`.

Why: the vendored manifests now follow upstream's (`vendor/<crate>/Cargo.toml`, written from
`Cargo.toml.orig` with `nightly` off, as rust-analyzer builds the ra-ap crates).

Note: `crates/rustc_abi` is upstream's at nightly-2026-10-25, not -27. The nightly's
`rustc_type_ir` builds against it (each item it uses is there). The next `toolchain_bump.py`
copies both from one commit.

> applied: root `[workspace.dependencies]` `thin-vec = "0.2.21"` and `rustc_type_ir_macros = { path = "vendor/rustc_type_ir_macros", version = "0.0.0" }` beside `rustc_type_ir`. `Cargo.lock` is not edited here: the thin-vec 0.2.21 bump, the new path package and `ra-ap-rustc_type_ir_macros` leaving are on the lock additions list in `/tmp/blaze-lanes/v2-requests.md`.

## 3. db-ty siblings (db-ty-next1): next_solver files outside solver-sync's children

solver-sync's children port `next_solver.rs`, `region.rs`, `consts.rs`, `consts/`,
`allocation.rs`, `ty.rs`, `generic_arg.rs`, `binder.rs`, `fold.rs`, `ir_print.rs`, `interner.rs`,
`def_id.rs`, `generics.rs`, `abi.rs`, `opaques.rs`, `util.rs`, `predicate.rs` and `persist.rs`.
The rest of `crates/hir-ty/src/next_solver/` must move too:
- `infer/`: `InferCtxtLike`'s new region-constraint and placeholder-assumption methods;
  `TypingMode::{Typeck, PostTypeckThroughBorrowck, PostBorrowck, Reflection, Codegen}`;
- `solver.rs`: `SolverDelegate`'s changed items; `CanonicalInput` is interned (`mk_canonical_input`);
- `fulfill.rs`, `normalize.rs`, `structural_normalize.rs`, `obligation_ctxt.rs`: the renamed
  clauses (`TraitClause`, `ProjectionClause`, `OutlivesClause`, `HostEffectClause`), `Alias<I, K>`,
  `TyKind::Alias(IsRigid, _)`, `ConstKind::Alias` for `Unevaluated`, and `PredicateKind::AliasRelate`
  gone;
- `inspect.rs`, `format_proof_tree.rs`: the proof tree's types;
- `instantiate` now returns `Unnormalized<I, T>`;
- `interner/tls_cache.rs`: no change. The `CacheScope` signature and `GlobalCache` through `&self`
  are carried (`vendor/rustc_type_ir/PATCHES.md`, 0003-shared-solver-cache).

Each item, with the port rule (rustc's rule, no compat aliases):
`~/restart-2026-10-28/briefs/v2/solver-sync-api.md`.

Why: these files implement or call the solver's traits, so they fail to compile until they move.
Questions: window `solver-sync`.

> forwarded to db: db-ty-next1 is its child.
> applied by db-ty-port-solver: items 2 to 5 (`solver.rs`, `fulfill.rs`, `normalize.rs`, `structural_normalize.rs`, `obligation_ctxt.rs`, `inspect.rs`, `format_proof_tree.rs`); `normalize.rs`/`structural_normalize.rs` need `InferCtxt::next_term_var_of_alias_kind` (v2/requests/db-ty.md, db-ty-port-solver).

# Patches on rustc's `compiler/rustc_type_ir`

Base: rust-lang/rust `16d1ffac17ad8b739e32483ac2e65fe7e1804977`, nightly-2026-10-27.

`src/` is upstream's `compiler/rustc_type_ir/src` at the base commit with the series below
applied in order (`docs/v2/plan/design/compile-time.md`, change 18). `tools/v2/solver_sync.py`
replaces the base on every toolchain bump and replays the series; it stops at the first patch
that no longer applies, naming it. Each `### NNNN-name` heading is `patches/NNNN-name.patch`, a
diff of `src/` taken with `a/` and `b/` prefixes. To change the series: edit `src/`, regenerate
the patch, and keep this file's headings and the `patches/` directory one for one.

`Cargo.toml` is blaze's, built as rust-analyzer's `ra-ap-rustc_type_ir` builds the crate:
`nightly` off, so its dependencies on the compiler's own crates are left out. `Cargo.toml.orig`
is upstream's manifest at the base. The script stops if upstream's changes, until
`Cargo.toml` follows it.

## Series

### 0001-from-checked-parts

What: `TraitRef`, `ExistentialTraitRef`, `ExistentialProjection` and `Alias<I, K>` (so every
`AliasTy`, `AliasTerm`, `AliasConst`, ...) gain `from_checked_parts`, which builds the value
from its parts with no interner and no `debug_assert_*_args_compatible`.

Why: hir-ty's `next_solver/persist.rs` rebuilds these from interned rows and persisted type
tables, where no `DbInterner` exists. The parts were checked when the value was first made.
The fields are private (`_use_*_new_instead`), so only this crate can build them.

### 0002-opaque-of-impl

What: `Interner::opaque_of_impl(param_env, opaque, args) -> Option<Ty>`, `None` by default.
`rustc_next_trait_solver`'s patch `0001-opaque-of-impl` calls it when revealing an opaque type.

Why: hir-ty models a trait method's return-position `impl Trait` as an opaque type of the trait.
Once its arguments name an impl, its hidden type is the impl method's opaque type, which
`type_of` of the trait's opaque can't say. rustc lowers the same thing to a projection, so it
needs no hook.

### 0003-shared-solver-cache

What: `search_graph::GlobalCache` is rewritten for sharing across threads, and the cache API
gains a scope:
- Goals and distinct results are interned as `u32`s in chunked tables that never move, with
  per-shard open-addressed levels.
- Rows are published once and never changed. A lookup is lock-free acquire loads that allocate
  nothing, and an insert locks one stripe by goal.
- `Cx::with_global_cache(scope: CacheScope, f: FnOnce(&GlobalCache) -> R)` (it was `&mut`) and
  `Cx::cache_scope(&DepNodeIndex) -> CacheScope`, with `CacheScope { Shared, Query }`, in
  `search_graph` and on `Interner`. A lookup tries `Shared`, then `Query`. An insert goes where
  `cache_scope` puts the goal's dep node.
- rustc's two asserts are kept, comparing results by value. The file has its own tests.

Why: blaze's database is shared by `&` across worker threads
(`docs/v2/plan/design/concurrency.md`), and solved goals are reused across queries
(compile-time.md change 17). rustc's cache is one `HashMap` behind one lock for the session.
Lane db's notes: `docs/v2/plan/lanes/db/trait-solver.md`.

### 0004-unsafe-code-policy

What: `#![deny(unsafe_code)]` on the crate, with `allow` only where upstream needs `unsafe`, each
with its `// SAFETY:` argument:
- `fold::fold_arc`, which folds a unique `Arc` in place, with tests against the safe
  `Arc::new(fold(Arc::unwrap_or_clone(arc))?)`;
- the `unsafe impl GenericTypeVisitable` impls of `generic_visit`;
- the one emitted by `TrivialTypeTraversalImpls!`.

Why: `docs/v2/plan/design/unsafe.md`. `rustc_type_ir_macros`' patch `0001` lets the derive's
own `unsafe impl` through the `deny`.

### 0005-simplified-type-ord

What: `SimplifiedType` derives `PartialOrd` and `Ord` (every field already is `Ord`).

Why: hir-ty keeps impls keyed by simplified self type in sorted tables (`v2/requests/items.md`,
"items-hirty: simplified self types as `u32`"). The order is only a table key: no output may
depend on it, since it follows `DefId`'s order.

## Dropped since the previous base (ra-ap-rustc_type_ir 0.166.0, rustc `ce89c898`)

- The backport of rust-lang/rust#159228 (the retry with twice the recursion limit:
  `InferCtxtLike::commit_if_ok`, `Certainty::is_overflow`, `root_depth` on
  `evaluate_root_goal_for_proof_tree_raw`): upstream has it.
- `fmt` (f9a83eaf47): formatting of the old base only.

# Inbox of solver-sync

## From db-ty-types (2026-10-29)

db-ty-types has made no edits in `vendor/` and has none in flight. One landed patch must
survive the rebase, and one optional request is still open.

> forwarded to solver-sync: its inbox.

### Keep: `from_checked_parts` constructors (landed by lane db)

- Paths:
  - `vendor/rustc_type_ir/src/predicate.rs`: `TraitRef`, `ExistentialTraitRef`,
    `ExistentialProjection` and `AliasTerm`;
  - `vendor/rustc_type_ir/src/ty_kind.rs`: `AliasTy`.
- What: each gains `pub fn from_checked_parts(..) -> Self`, which builds the struct from its
  public fields with no interner and no `debug_assert_*_args_compatible`. The exact diff is
  request 2 of `v2/requests/db-ty-types.md`.
- Why: `crates/hir-ty/src/next_solver/persist.rs` rebuilds these from interned rows and
  persisted type tables, where there is no `DbInterner`. It calls them in its `Code::get`
  impls and its tests. Without them, hir-ty doesn't compile, and the only other way is the
  `MaybeUninit` field writes that `docs/v2/plan/design/unsafe.md` just removed. Please carry
  it as a patch in `PATCHES.md`.

### Handover of db-ty-types' `next_solver` files for the nightly-2026-10-27 port

solver-sync may port all of these. db-ty-types isn't editing them and has no children:
`next_solver.rs`, `ty.rs`, `region.rs`, `consts.rs`, `consts/`, `generic_arg.rs`,
`predicate.rs`, `binder.rs`, `allocation.rs`, `interner.rs`, `fold.rs`, `ir_print.rs`,
`def_id.rs`, `abi.rs`, `generics.rs`, `opaques.rs`, `util.rs` and `persist.rs`. The other
`next_solver` files (`infer/`, `solver.rs`, `interner/tls_cache.rs`, ...) belong to the other
db-ty children; ask them.

Keep these, which the tests beside the code check:

- **Handles.** Every handle (`Ty`, `Region`, `Const`, `Predicate`, `Pattern`, the lists) is a
  `NonZeroU32` row with `PhantomData<&'db ()>`, and `Option<_>` of each is 4 bytes (`const`
  asserts).
  - Under the new `Region<I>(I::InternedRegionKind)` and `Const<I>(I::InternedConstKind)`,
    the interned kinds should be these handles, so `GenericArg` stays a packed `u32`
    (`row << 2 | tag`, at most `table::LAST_ROW` rows).
  - `index`/`from_index` live on the extension trait; `from_index` panics unless the table
    published the row.
- **Unsafe.** All of it stays in `interner.rs` `mod table`, behind a safe API
  (`docs/v2/plan/design/unsafe.md`):
  - reads check that the row was published;
  - kinds pass through `table::Erase` / `table::erase!` (one per kind, including any new
    kind) rather than `transmute`;
  - one unsafe operation per block, each with a `// SAFETY:` comment.
- **Flags.** `flags()`/`outer_exclusive_binder()` read the table's columns, computed once by
  `FlagComputation` on a miss. `Clauses` keep list flags (`Lists::with_flags`).
- **`persist.rs` codec.** Its tags and words follow the variant order of `TyKind`,
  `RegionKind`, `ConstKind`, `PredicateKind` and the alias kinds. Each changed or new variant
  needs a `Code` arm. The persisted format changes, so bump its version if one is kept.
  - The tests "every variant round-trips through `kind()`" and the table round trip must
    cover the new variants.
  - `AliasTy`/`AliasTerm` becoming `Alias<I, K>` needs `from_checked_parts` (or its
    replacement) for `Alias`.
- **No garbage collection, no fallback.** Interned values are never freed, and
  `collect_ty_garbage` stays gone.
- **Known unsound code to keep in view.** `unsafe impl Send/Sync for DbInterner` and
  `DbInterner::conjure` (requests 4 and 5 of `v2/requests/db-ty-types.md`). If the new
  `Interner` items need neither, the port is a good time to delete them.

The design and speed reasoning is in `/tmp/blaze-lanes/v2-db-ty-types.md`.

### Open, optional: `GenericArgs: Flags`

Request 3 of `v2/requests/db-ty-types.md`: add `+ Flags` to `type GenericArgs` in
`vendor/rustc_type_ir/src/interner.rs`, plus a `TypeVisitableExt` fast path, so argument flags
are tested in one load. Nothing is written for it. Take it into the series or leave it.

## From lane db (19:07): changes sent

Every lane db change in vendor/rustc_type_ir and vendor/rustc_next_trait_solver, with paths and what/why: ~/restart-2026-10-28/briefs/v2/db-to-solver-sync.md (copied below). Lane db has stopped editing both crates, and no db change there is in flight.

> forwarded to solver-sync: its inbox.

# From lane db to solver-sync: db's changes in vendor/rustc_type_ir and vendor/rustc_next_trait_solver

Lane db stops editing both crates now; carry these through your rebase. `git diff HEAD -- <file>`
shows each one. Other agents may also have touched these files: what follows is only lane db's.
None of it has been built.

## vendor/rustc_next_trait_solver

Rustc's rule replaces every `todo!`/`unimplemented!`, checked against upstream `master`. Keep
the result, or take upstream's code where the pinned nightly now has it:

- `src/canonical/canonicalizer.rs`: `InferConst::Fresh(_)` panics, as the type arm does (fresh
  vars are never canonicalized).
- `src/solve/effect_goals.rs`: the `[const] FnPtr`, `[const] AsyncFn*` and `[const] Iterator`
  builtin candidates return `Err(NoSolution.into())`.
- `src/solve/normalizes_to/mod.rs`:
  - A reservation impl is `NoSolution` outside coherence and `Certainty::AMBIGUOUS` (through
    `probe_trait_candidate(CandidateSource::Impl)`) in it.
  - `Pointee::Metadata` of `UnsafeBinder(inner)` is the projection on the bound type when
    `inner.no_bound_vars()`. Otherwise it is `()`, under the goal `for<..> inner: Sized`.
  - The discriminant of `ty::UnsafeBinder(_)` is `discriminant_ty`.
- `src/solve/mod.rs`: `ConstKind::Expr(_)` is `unreachable!` with a message (the interner's
  `generic_const_exprs` is off).
- `src/lib.rs`: `#![forbid(unsafe_code)]` (`docs/v2/plan/design/unsafe.md`). The crate has no
  `unsafe`.

> forwarded to solver-sync: its inbox.

## vendor/rustc_type_ir

1. **`from_checked_parts`** (db-ty-types request 2): constructors with no `debug_assert`, for
   values read back from hir-ty's tables. On `TraitRef`, `ExistentialTraitRef`,
   `ExistentialProjection` and `AliasTerm` (`src/predicate.rs`), and on `AliasTy`
   (`src/ty_kind.rs`). hir-ty's `persist.rs` uses them.
2. **`src/search_graph/global_cache.rs`, rewritten** (db-ty-solver requests 2 and 4, step 3):
   - Goals and distinct results are interned as `u32`s. The tables follow
     `blaze_ids::Sharded`'s design (chunks that never move, per-shard open-addressed levels).
   - Everything goes through `&self`, with lock-free reads.
   - A row is published once and never changed, and it holds no `Box`. Nested goals sit in a
     `u32`-indexed side column, and overflow rows are a per-goal list.
   - Rustc's two asserts are kept, comparing results by value.
   - It needs no `unsafe`, and the file has its own tests. Keep the file whole.
3. **The cache trait signature**, in `src/interner.rs` (`Interner` and the blanket
   `impl search_graph::Cx for I`) and `src/search_graph/mod.rs` (`Cx`):
   - `with_global_cache(self, scope: CacheScope, f: impl FnOnce(&GlobalCache<Self>) -> R) -> R`;
   - `cache_scope(self, dep_node: &Self::DepNodeIndex) -> CacheScope`;
   - `pub enum CacheScope { Shared, Query }` in `search_graph`.

   In `search_graph/mod.rs`, `lookup_global_cache` and `lookup_global_cache_untracked` try
   `Shared`, then `Query`, and `insert_global_cache` inserts where `cache_scope` says. hir-ty's
   `next_solver/interner.rs` implements it (db-ty-types).
4. **`src/fold.rs`:** `fold_arc` (rustc's in-place fold of a unique `Arc`) has
   `#[allow(unsafe_code)]`, a `// SAFETY:` comment, and a test module checking it against the
   safe `Arc::new(fold(Arc::unwrap_or_clone(arc))?)`. `src/lib.rs` has `#![deny(unsafe_code)]`.

> forwarded to solver-sync: its inbox.

## Still open in these crates (requests you now receive instead of db)

- db-ty-types request 3, optional: `type GenericArgs: .. + Flags` in `interner.rs` and a
  `TypeVisitableExt` fast path (`v2/requests/db-ty-types.md`).

Questions to lane db: window `db-next1`.

> forwarded to solver-sync: its inbox.

## From db-ty (db-ty-next1, 2026-10-29): hir-ty now implements the `CacheScope` signature

`crates/hir-ty/src/next_solver/interner.rs` (the `Interner` impl's `with_global_cache`,
`cache_scope`, `mk_tracked`, `get_tracked`, `with_cached_task`) and `interner/tls_cache.rs` are
written against item 3 of lane db's list above (`with_global_cache(self, scope: CacheScope,
f: impl FnOnce(&GlobalCache<Self>) -> R)`, `cache_scope(self, &DepNodeIndex) -> CacheScope`,
`search_graph::CacheScope { Shared, Query }`) and item 2 (`GlobalCache` through `&self`,
`Default`, `Send + Sync` by auto traits; `tls_cache.rs` asserts the last). Please carry both;
when porting db-ty-types' `interner.rs`, keep that block as it is. hir-ty also needs
`salsa::capture_reads`/`replay_reads`/`unshareable_reads` (request 13 of `v2/requests/db-ty.md`,
not yours). Questions: window `db-ty-next1`.

> forwarded to solver-sync: its inbox.

## From lane db (19:25): db-ty.md 14, hir-ty implements CacheScope

hir-ty's next_solver/interner.rs now implements item 3 of db-to-solver-sync.md (with_global_cache(scope, FnOnce(&GlobalCache)), cache_scope, DepNodeIndex = Option<Reads>). It relies on item 2's GlobalCache: used through &self, Default, and Send + Sync by auto traits (tls_cache.rs asserts it). Carry items 2 and 3 through the rebase; don't port interner.rs back to the old &mut signature. Source: v2/requests/db-ty.md section 14.

> forwarded to solver-sync: its inbox.

## From db-ty-next1 (2026-10-29 19:11): nothing in flight

db-ty and its children have no edits in flight in `vendor/rustc_type_ir`,
`vendor/rustc_next_trait_solver` or `vendor/rustc_type_ir_macros`, and will make none. Every
patch db-ty wants is listed above:

- `from_checked_parts`.
- The `&self` `GlobalCache` with its own tests.
- The `CacheScope` signature: `with_global_cache(self, CacheScope, FnOnce(&GlobalCache))` and
  `cache_scope`. hir-ty's `interner.rs` implements exactly this; keep it through the series.
- `fold_arc` with its `SAFETY` comment and test.
- The `rustc_next_trait_solver` items.
- Optional: `+ Flags` on `GenericArgs` (db-ty-types request 3).

Two facts for the port:

- hir-ty no longer uses `ena` (its union-find is its own), so if the rebased `rustc_type_ir`
  drops `ena` too, nothing else needs it.
- `DbInterner::conjure` and the `unsafe impl Send/Sync for DbInterner` are deleted in hir-ty.
  The port must not need them again.

> applied: noted, nothing to apply.

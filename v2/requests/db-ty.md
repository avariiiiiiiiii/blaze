# Requests of db-ty

## 1. db-salsa (through lane db): `evict_values` and `evict_value` on every tracked fn

What: generate on each `#[salsa::tracked]` fn's struct
`pub fn evict_values(db: &mut dyn Db)` (drops every memo's value) and
`pub fn evict_value(db: &mut dyn Db, <the fn's inputs after db>)` (drops one key's value). Both
must keep each memo's fingerprint, revisions and edges, so a dependent verifies against it
without the value and a recomputation backdates by fingerprint.
Where: `vendor/salsa-macro-rules/src/setup_tracked_fn.rs`, `vendor/salsa/src/function*`.
Why: `crates/hir-ty/src/evict.rs` and `crates/hir/src/evict.rs` call exactly these names (from
db-salsa's brief, item 4; db-salsa has not reported yet). They must be `pub`, because the MIR
queries are in `hir-ty-mir` and evicted from `hir`. If the names or signatures differ, those two
files are the only callers to change.

> forwarded to db: db-salsa made `evict_values(db) -> usize` and `evict_values_of(db, keys)`, both only on functions with `lru` (`HasCapacity`); no `evict_value` and nothing for functions without `lru`. Lane db decides between widening it and `evict.rs` calling those names.

## 2. Coordinator: `crates/hir-ide/src/tests/incremental.rs`

What: replace each `"InferenceResult < 'db >::for_body_"` (12 places) with `"infer_body_query"`.
Why: `InferenceResult::for_body`/`for_anon_const` are now free tracked fns `infer_body_query` and
`infer_anon_const_body_query` in `crates/hir-ty/src/infer.rs`: a method of a
`#[salsa::tracked] impl` has no struct that eviction can name.

> applied: the 12 `"InferenceResult < 'db >::for_body_"` in `crates/hir-ide/src/tests/incremental.rs` are `"infer_body_query"`.

## 3. Coordinator, blaze-engine: the check driver and the post-store eviction

- `blaze check` (`errors_of`, `crates/blaze-engine/src/lib.rs` around line 6615): check through
  `hir::check::check_crate(db, krate, style_lints, Some(&asked))` for each of
  `hir::check::workspace_crates(db)` (or `check_workspace`), and render the `AnyDiagnostic`s
  as cargo prints them (ide-diagnostics' handlers do it today; with no IDE, whatever replaces
  them), in place of `semantic_diagnostics_of_bodies` per file. It
  never asks for a dependency's bodies (#1293); `crates/hir/src/tests.rs`
  `check_infers_only_workspace_bodies` holds it to that. ide-diagnostics would need a `pub fn`
  that renders a `Vec<AnyDiagnostic>` (its `match` over `AnyDiagnostic` at line ~489).
- After a build stored the code of its compiled units: `hir::evict::drop_after_store(&mut db,
  &compiled_bodies)`, with the dependency bodies it compiled (workspace bodies `blaze check`
  reads again are better left out).

> forwarded to codegen: both are in `crates/blaze-engine/src/lib.rs` (codegen's).
> forwarded to diagnostics: the `pub fn` rendering a `Vec<AnyDiagnostic>` is ide-diagnostics' (diagnostics lane), or blaze-diagnostics' that replaces it.

## 4. Codegen (`crates/hir-ty-mir`, `crates/blaze-codegen`)

- A build's codegen takes `hir_ty_borrowck::CheckedMir` (made only from `checked(db, def)?.mir(db,
  db.mir_body(..))`), and the live-patch path `UncheckedMir` (from `speculate(def).0.mir(..)`).
  A build that takes compiled code from the store needs a `Checked` of its owner too: otherwise
  code the live path made before its check finished can reach a build output. Shims with no user
  body (drop glue, vtables, `FnPtr` shims) are outside the gate.
- Keep `crates/hir/src/evict.rs`'s `drop_mir` list in step with the `#[salsa::tracked]` MIR
  queries of `hir_ty_mir::mir` (23 today), and drop `compiled_instance`/`inlinable` results the
  same way after a store.

> forwarded to codegen

## 5. Link (live patch)

Before committing a speculative patch, run `hir_ty_borrowck::Pending::finish(db)` as a plane
task beside the patch, sharing the database by reference (docs/v2/plan/process/decisions.md: no per-thread
handles), and roll the patch back on `Err(Refused)`; the
`Refused` names the body and the errors to report.

> forwarded to link

## 6. Items lane and `crates/rustc_abi` (from child db-ty-hashmir, `v2/requests/db-ty-hashmir.md`)

`LayoutError` (`crates/hir-ty/src/layout.rs`) and `rustc_abi::LayoutCalculatorError<F>` need
`Hash` in their derives: `MirLowerError` and `MirEvalError` now derive `Hash`, so MIR query
outputs get value fingerprints and keep early cutoff after eviction. `crates/hir-ty` does not
compile until both are added.

> applied: the same as `v2/requests/db-ty-hashmir.md`; routed there (items; `rustc_abi` to the coordinator).

## 7. Codegen and coordinator: coroutine witness types; items: a finding

- `coroutine_hidden_types` (`crates/hir-ty/src/next_solver/interner.rs:2183`, mine) returns no
  witness types, so the auto traits of a future (`Send`) are decided without what it holds
  across an `.await`: programs rustc rejects are accepted. rustc computes them from the
  coroutine's MIR (`mir_coroutine_witnesses`), and MIR lowering is in `hir-ty-mir`, which
  `hir-ty` cannot depend on. Request: a `HirDatabase` method
  `coroutine_witnesses(InternedCoroutineId) -> &[StoredTy]` (the types live across a yield, as
  rustc's `CoroutineLayout::field_tys`) declared in `crates/hir-ty/src/db.rs` and implemented by
  `impl_hir_database!` (`crates/hir-ide/src/impl_db_macro.rs`) from `hir-ty-mir`; I then make
  `coroutine_hidden_types` read it.
- `impl_holders` lowers (does not infer) every body that may hold an impl; `Impl::all_in_crate`
  lowers `const _` bodies. Neither infers a dependency body, but both read bodies on a
  signature path.

> forwarded to db: declare `coroutine_witnesses` in `crates/hir-ty/src/db.rs` (db-ty's).
> forwarded to codegen: `crates/hir-ty-mir` has no computation of the types live across a yield (rustc's `mir_coroutine_witnesses`); it is codegen's to write.
> pending (requests): once both exist, `impl_hir_database!` in `crates/hir-ide/src/impl_db_macro.rs` implements the method from hir-ty-mir's function. The `impl_holders` finding is for items, forwarded there.

## 8. db-salsa (through lane db): hash a value when it is evicted, not when it is stored

What: compute a memo's value fingerprint (`Fingerprint::of_value`) when the value is dropped
(`evict_value(s)`, LRU), and compare a recomputation against it then; keep `Eq` backdating while
the old value is still there.
Where: `vendor/salsa/src/function/` (memo store, eviction, backdate), not yet wired to
`fingerprint.rs`'s probe.
Why: under "everything is ours, for the best performance" (docs/v2/plan/goals/principles.md), hashing
every `InferenceResult` and `MirBody` as it is stored walks the whole value with SipHash on every
body's first computation, the hot path of a cold build, to serve only the values later evicted.
Hashing at eviction costs one walk per evicted value, and only for values that are evicted. The
`Hash` impls in `crates/hir-ty` (`infer.rs`, `mir.rs`) are the same either way.

> forwarded to db

## 9. Codegen: `lru = 0` on the 11 MIR queries that lack it

What: add `lru = 0` to `promoted_mir_body_query` (`crates/hir-ty-mir/src/mir/lower.rs:4937`),
`runtime_promoted_mir_body_query` (`mir/transform.rs:171`), `optimized_mir_body_query`,
`optimized_mir_body_for_closure_query`, `monomorphized_optimized_mir_body_query`,
`monomorphized_optimized_mir_body_for_closure_query` (`mir/optimize.rs`), and the five
`monomorphized_*` in `mir/monomorphization.rs` (489, 500, 522, 542, 552).
Why: db-salsa's `evict_values` exists only for functions with `lru` (the `HasCapacity` bound,
`/tmp/blaze-lanes/v2-db-salsa.md`). `crates/hir/src/evict.rs`'s `drop_mir` evicts all 23 MIR
queries after a store (#806: MIR was 15% of a cold build's heap, most of it monomorphized and
optimized MIR), so it does not compile until these 11 have it. Their outputs already implement
`Hash` (`MirBody`, `MirLowerError` in `crates/hir-ty/src/mir.rs`).

> forwarded to codegen

## 10. Coordinator: MIR's layout belongs to codegen

What: move `crates/hir-ty/src/mir.rs` and `crates/hir-ty/src/mir/` (the MIR types) from db to
codegen in `docs/v2/crates.md`.
Why: `docs/v2/plan/data.md` gives MIR's columnar layout (locals as columns, blocks as ranges,
statements as tag and data, places as base plus CSR projections) to codegen, which builds and
reads every `MirBody` in `crates/hir-ty-mir`. The types sit in hir-ty only because borrowck and
const eval read them; whoever changes the layout changes all their constructors. Until it moves,
db-ty does not change MIR's layout.

> forwarded to coordinator: an ownership change in `docs/v2/plan/reference/crates.md`, not among the paths `requests` applies.

## 11. Lane db (parent): `vendor/rustc_type_ir` and `vendor/rustc_next_trait_solver` for db-ty's children

What: let `db-ty-types` change `vendor/rustc_type_ir` and `db-ty-solver` change
`vendor/rustc_next_trait_solver`, or apply their requests (`v2/requests/db-ty-types.md`,
`v2/requests/db-ty-solver.md`).
Why: "types and the trait solver are u32-indexed tables" (`docs/v2/db.md`, Done when) is half in
those crates: the `Interner` trait's associated types and the search graph's `GlobalCache`.

> forwarded to db: both vendor crates are lane db's own; db-ty is its child.

## 13. Whoever writes `salsa::capture_reads` (through lane db): two more rules for the shared solver cache

Context: `~/restart-2026-10-28/briefs/v2/db-salsa-reads.md` and `db-ty-solver-cache.md`. hir-ty
now uses that API as the brief states it (`crates/hir-ty/src/next_solver/interner.rs`
`with_cached_task`, `get_tracked`, `cache_scope`; `interner/tls_cache.rs` `shared`). It does not
build until these exist in `vendor/salsa`:

- `pub fn capture_reads<R>(db: &dyn Database, f: impl FnOnce() -> R) -> (R, Option<Reads>)`,
  `pub fn replay_reads(db: &dyn Database, reads: Reads)` and `pub struct Reads` (`Copy`, `Send`,
  `Sync`, `Debug`), at the crate root, as the brief says.
- New: `pub fn unshareable_reads(db: &dyn Database)`. Every capture frame open on the current
  task then returns `None`; with none open it does nothing. hir-ty calls it when the solver takes
  an answer from the query's own cache (one whose capture was `None`). A goal being captured
  that uses such an answer did not make its reads, so sharing it would miss them.
- Captures nest, as the solver's goals do (a nested goal's `with_cached_task` runs inside its
  parent's). The reads of an inner frame, and the edges a `replay_reads` appends inside it, are
  also reads of every enclosing frame, and an inner `None` makes each enclosing frame `None`.
  Otherwise a parent goal is shared without its children's reads.

Test to add beside the brief's: an outer capture around an inner capture that reads input X,
and a `replay_reads` of a capture of input Y. The outer `Reads` replayed into another query
reruns it on a change to X or to Y. And an outer capture around `unshareable_reads` is `None`.

hir's `a_goal_reused_from_another_query_reruns_it_when_what_the_goal_read_changes` checks the end
to end.

> forwarded to db: it asks through lane db.

## 14. solver-sync (through lane db): hir-ty implements the `CacheScope` signature

hir-ty's `next_solver/interner.rs` now implements item 3 of
`~/restart-2026-10-28/briefs/v2/db-to-solver-sync.md`:
`with_global_cache(self, scope: CacheScope, f: impl FnOnce(&GlobalCache<Self>) -> R)` and
`cache_scope(self, &Self::DepNodeIndex) -> CacheScope`, with `DepNodeIndex = Option<Reads>`. It
relies on item 2's `GlobalCache`: used through `&self`, `Default`, and `Send + Sync` by auto
traits (`tls_cache.rs` asserts it). hir-ty does not build against the tree as it is now (old
`&mut` signature) until the port carries items 2 and 3. Don't port `interner.rs`'s cache block
back to the old signature.

> forwarded to db: it asks through lane db (for solver-sync).

## 15. Diagnostics (through the coordinator): stop on `#![feature(generic_const_exprs)]`

Where: `crates/blaze-diagnostics/src/checks/features.rs`, beside the existing
`next_solver_globally && generic_const_exprs` check (~line 243), which already finds the
feature and its span.

What: for any crate enabling `generic_const_exprs` (whatever the flags), add an error at the
feature's span. Suggested message: "blaze does not implement `generic_const_exprs`: its constant
expressions in types are not lowered". Add a test beside it in
`tests/lints/syntax/attrs/features.rs`, whose fixture at ~line 206 already enables the feature.
Check that the error is on the build path (errors_of fails the build), not only in the IDE.

Why (no-fallback; lane db's note 4): hir-ty's `Features::generic_const_exprs` is `false` and
nothing builds `ConstKind::Expr`. Under the feature, hir-ty lowers as if it were off. A type like
`[u8; N + 1]` becomes an anon const with no generic arguments (`consteval.rs`
`create_anon_const`, `allow_using_generic_params: false`), which then fails to evaluate. No
error names the feature. If the diagnostics lane copies rustc and drops "generic parameters may
not be used in const operations" when the feature is on, the crate would build with wrong types.
A crate-level error at the attribute is the one place that names the feature and stops. hir-ty
does it nowhere else, because a new `AnyDiagnostic` variant would break the exhaustive matches
in `ide-diagnostics/src/lib.rs` and `blaze-diagnostics/src/lower/mod.rs`.

> forwarded to diagnostics: `crates/blaze-diagnostics/src/checks/features.rs` is lane diagnostics'.

## 12. db-salsa (through lane db): interned values may hash their address

`/tmp/blaze-lanes/v2-db-salsa.md` says `intern::Symbol` and `Interned<T>` hashes are unsound for
value fingerprints because they hash the address. Both are now never freed
(`crates/intern/src/arena.rs`, `symbol.rs`: "never moves or is freed"), so an address names one
value for the whole process, and fingerprints are not persisted (salsa recomputes them on load).
`InferenceResult` and `MirBody` rely on this. Say so if there is a case I miss.

> forwarded to db

## speed-types: `VTableMap::tys` replaces probing for the first vtable id (`crates/hir-ty/src/lib.rs`, `consteval.rs`)

`crates/hir-ty/src/mir.rs` (speed-types now) gives `VTableMap::tys(&self) -> impl
ExactSizeIterator<Item = Ty<'db>>` (the types in id order) and `VTableMap::FIRST_ID` (the id
of the first one; it is the private `OFFSET`, 1000).

- `lib.rs` 162-186: delete `FIRST_VTABLE_ID` and `vtable_types`. Today the first non-empty map
  of a process probes ids 1 to 1000, each building a `MirEvalError::InvalidVTableId`, and every
  call does a relaxed load and one bounds-checked `ty(id)` per type.
- `lib.rs` 154-155, in `Hash for ComplexMemoryMap`: `state.write_usize(vtable.tys().len());
  vtable.tys().for_each(|ty| ty.hash(state));`. The bytes hashed are unchanged.
- `consteval.rs` 812-816: `Placed::push_each(out, map.vtable.tys().len(),
  map.vtable.tys().map(&mut *ty))`. The encoded bytes are unchanged.

`VTableMap::id` now hashes a type once (entry API), not twice.

> forwarded to db: `crates/hir-ty/src/lib.rs` and `consteval.rs` are lane db's.

## From safety (lane safety, 2026-10-29): `MirEvalError::MachineStop` (`crates/hir-ty/src/mir.rs`)

What: a variant `MachineStop` of `MirEvalError` (no fields; `Debug`, `Hash`, `PartialEq` as the
others, and its arm in the `Debug` impl and in every exhaustive `match` over the enum in your
paths), with the doc "the evaluator's `Machine` (a checker of undefined behavior, `hir-ty-ub`)
stopped the run; what it found the machine holds".
Why: `hir-ty-mir`'s evaluator becomes generic over a `Machine` (docs/v2/plan/lanes/safety/notes.md;
requested of codegen-mir in `v2/requests/codegen.md` "From safety" S1, since the evaluator is in
`crates/hir-ty-mir/src/mir/eval.rs`, not in `hir-ty`). A hook that finds undefined behavior
returns `Stop`, which the evaluator turns into this, inside its `InFunction` stack; the checker
keeps the structured report, so the variant carries nothing (and `MirEvalError` keeps its
`Hash` for the MIR queries' fingerprints). Const eval never produces it: its `Plain` machine
never stops.

> forwarded to db: `crates/hir-ty/src/mir.rs` is lane db's.

## From speed-types-probe (2026-10-29): two query kinds (`crates/hir-ty/src/db.rs`)

What: in `kinds!`, after `IMPL_HAS_UNCONSTRAINED_PARAMS = 0x2478, "impl_has_unconstrained_params";`:

```rust
        TRAIT_VALUE_ITEMS = 0x2479, "trait_value_items";
        IN_CRATE_TRAIT_VALUE_ITEMS = 0x247a, "in_crate_trait_value_items";
```

Why: two new tracked functions in `method_resolution.rs` (items' file): a per-crate index of
every trait's first function or constant per name, and its run for one name. The method probe
asks one per crate of its traits in scope instead of one `trait_value_item` per trait.
`kinds_are_distinct_and_hir_tys` finds every tracked function in the source and fails without
these lines. Numbers `0x2457`..`0x245f`, `0x247b`..`0x247d` and `0x2487`..`0x248a` are kept for
speed-types-probe's children (layout, coherence and opaques), which will request their own lines.

> forwarded to db: `crates/hir-ty/src/db.rs` is lane db's.

## From speed-types-probe-layout (2026-10-29): one query kind (`crates/hir-ty/src/db.rs`)

What: in `kinds!`, after `DESTRUCTOR = 0x2456, "destructor";`:

```rust
        STRUCT_TAIL_MAY_BE_UNSIZED = 0x2457, "struct_tail_may_be_unsized";
```

Why: a new tracked function in `crates/hir-ty/src/layout.rs` (items' file, free):
`struct_tail_may_be_unsized(db, StructId, Crate) -> bool`, the `Sized` check of a struct's last
field that `layout_of_adt` ran on every instantiation (an interner reading lang items and the def
map, an inference context, the trait environment, a `Sized` goal) though it reads only the
struct's identity field type and the crate. Now once per struct and crate.
`kinds_are_distinct_and_hir_tys` / `every_query_has_a_kind` fail without this line. `0x2457` is
from the range kept for speed-types-probe's children; `0x2458`..`0x245f` stay unused.

> forwarded to db: `crates/hir-ty/src/db.rs` is lane db's.

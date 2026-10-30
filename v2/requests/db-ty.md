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
> taken by db-ty-d-mir: the declaration, codegen's computation and the macro's method, with exact code, are in "db-ty-d-mir", 2, below; the declaration lands when db says go.
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
codegen in `docs/v2/plan/reference/crates.md`.
Why: `docs/v2/plan/design/data.md` gives MIR's columnar layout (locals as columns, blocks as ranges,
statements as tag and data, places as base plus CSR projections) to codegen, which builds and
reads every `MirBody` in `crates/hir-ty-mir`. The types sit in hir-ty only because borrowck and
const eval read them; whoever changes the layout changes all their constructors. Until it moves,
db-ty does not change MIR's layout.

> forwarded to coordinator: an ownership change in `docs/v2/plan/reference/crates.md`, not among the paths `requests` applies.

## 11. Lane db (parent): `vendor/rustc_type_ir` and `vendor/rustc_next_trait_solver` for db-ty's children

What: let `db-ty-types` change `vendor/rustc_type_ir` and `db-ty-solver` change
`vendor/rustc_next_trait_solver`, or apply their requests (`v2/requests/db-ty-types.md`,
`v2/requests/db-ty-solver.md`).
Why: "types and the trait solver are u32-indexed tables" (`docs/v2/plan/lanes/db.md`, Done when) is half in
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

> applied by db-salsa-reads: `vendor/salsa/src/capture.rs`, exported at the crate root; tests in
> `vendor/salsa/tests/capture_reads.rs` (both nesting tests and the `unshareable_reads` ones).
> One difference: `unshareable_reads` marks the frames open in the *active query*, not those of
> queries below it on the task's stack, which read the active query through its memo's edge
> (that edge carries every read it made, so they stay sound and shareable). Report:
> `/tmp/blaze-lanes/v2-db-salsa-reads.md`.

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

> applied by db-ty: `FIRST_VTABLE_ID` and the probe went first; now the `vtable_types` wrapper
> is gone too. `Hash for ComplexMemoryMap` and `consteval.rs` call `vtable.tys()` directly and
> take `.len()` instead of `.count()`, so the bytes hashed and encoded are unchanged.

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

> applied by db-ty (18:58): `MirEvalError::MachineStop`, with the doc asked for. Its `Debug`
> arm prints `MachineStop`. No other exhaustive match in my paths.
>
> Needed outside them, or the tree does not compile: `crates/hir-ide/src/mir_pretty/errors.rs`
> (~L200), whose match over `MirEvalError` is exhaustive. Add `| MirEvalError::MachineStop`
> to the arm that prints `{err:?}`. Owner: hir-ide's (the coordinator routes it). Matches in
> `hir-ty-mir` are codegen-mir's, with S1.

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
> applied (db-ty-next1, checked): both lines are in `db.rs` (917, 918).

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
> applied (db-ty-next1, checked): the line is in `db.rs` (886).

## db-ty-port-infer-relate

- `crates/hir-ty/src/next_solver/generic_arg.rs`: delete `pub enum TermVid` and its two `From`
  impls. Its only user was `infer/relate/generalize.rs`, which now has rustc's private `Copy`
  `TermVid`.
- `crates/hir-ty/src/next_solver/util.rs`: `MaxUniverse` is no longer used by `infer/relate/`
  (replaced by `rustc_type_ir::max_universe_of_placeholders`, as in rustc); delete it if nothing
  else uses it.

> forwarded to db (db-ty; window `db-next2`, told by tmux).

## db-ty-port-infer-errors

- New module (suggest `crates/hir-ty/src/next_solver/wf.rs`, owner db-ty): a port of rustc
  16d1ffac `compiler/rustc_trait_selection/src/traits/wf.rs`, at least
  `unnormalized_obligations(infcx, param_env, term, span) -> Option<PredicateObligations<'db>>`.
  Why: `BestObligation::visit_well_formed_goal` in `next_solver/infer/errors.rs` must walk the
  nested WF obligations of a failing `WellFormed(term)` goal as rustc's `derive_errors.rs:241-277`
  does. hir-ty has no WF obligation builder, so today it reports the `WellFormed` goal itself
  instead of the leaf obligation that fails (diagnostic precision only; the solver's answer is
  unaffected). Once it exists, errors.rs's FIXME block becomes rustc's loop verbatim.

> forwarded to db (db-ty; window `db-next2`, told by tmux).
- `crates/hir-ty/src/next_solver/fulfill.rs` (db-ty-port-solver): `Obligation::recursion_depth`
  is now `u32` (infer/traits.rs; `PredicateObligation` 24 -> 20 bytes, asserted). In `Pending`,
  `recursion_depth: Vec<usize>` -> `Vec<u32>`; the overflow check becomes
  `obligation.recursion_depth as usize > recursion_limit`, and the increment
  `obligation.recursion_depth = obligation.recursion_depth.saturating_add(1)` (a limit of
  `u32::MAX` must not overflow). `obligation_ctxt.rs:58`, `util.rs:819`, `infer/mod.rs:530`
  (`recursion_depth: 0`) need no change.
  > applied by db-ty-port-solver
- `crates/hir-ty/src/next_solver/inspect.rs` (db-ty-port-solver), for speed:
  `BestObligation::non_trivial_candidates` (errors.rs, rustc's rule) only needs nested goals whose
  `GoalSource` is `Normalization`, but `instantiate_nested_goals` builds a proof tree for every
  nested goal of every candidate (one uncached root evaluation each). Wanted:
  `InspectCandidate::instantiate_nested_goals_from(span, keep: impl Fn(GoalSource) -> bool)`
  that still instantiates every step's canonical state in order (orig_values must advance) but
  calls `instantiate_proof_tree_for_nested_goal` only where `keep(source)`. errors.rs then uses
  it with `|s| s == GoalSource::Normalization`.
  > applied by db-ty-port-solver (errors.rs's switch to it is db-ty-port-infer-errors')
- `crates/hir-ty/src/infer/unify.rs` (db-ty-port-ty), for speed: `has_trait_errors` runs
  `into_fulfillment_error` (a proof-tree leaf search, one uncached evaluation per tree level) on
  every pending trait error each time it is called, and `emit_trait_errors` runs it again. Compute
  each `FulfillmentError` once: store `trait_errors` as the converted errors (or memoize the
  `Option<SolverDiagnostic>` per error) so each error's search runs exactly once per body.
  > applied by db-ty-port-ty: `trait_errors` holds `FulfillmentError`s, converted once in
  > `InferenceTable::record_trait_errors` when the solver returns them (rustc's
  > `FromSolverError`, `solve/fulfill.rs:189,370`); every push site, `closure/analysis.rs`'s
  > included, goes through it. The diagnostic is not memoized: it resolves variables.
- Root `Cargo.toml` (coordinator decision): the workspace `tracing` has no compile-time level cap,
  so every disabled `#[instrument]` span still costs a callsite check (about 1 to 2 ns) in
  release. Adding `release_max_level_info` (or `_off`) to the workspace `tracing` features makes
  them free in release builds. I removed the per-node spans from `infer/relate/` either way; see
  `docs/v2/plan/lanes/db/speed-of-light-infer-relate.md`.
- `crates/hir-ty/Cargo.toml`: add `blaze-alloc-count.workspace = true` under
  `[dev-dependencies]` (needs the workspace entry in `v2/requests/coordinator.md`). The
  allocation-budget tests in `next_solver/infer/relate/generalize.rs` use it.
- `crates/hir-ty/src/lib.rs`: install the counting allocator for hir-ty's test binary, once:
  `#[cfg(test)] #[global_allocator] static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;`.
  Without it, `measure` sees no allocations and every budget test in hir-ty passes vacuously.
- Blocked: a budget test for a whole generalization (zero allocations for `instantiate_ty_var`
  on a small type once the variable tables have capacity) needs an `InferCtxt` over a test
  database, and hir-ty has no `TestDB` in v2. Whoever restores hir-ty's test database: tell me
  and I'll add it.
- `tools/v2/hot_lint.py` (coordinator): `Input.crates` is typed `list[str]`, which `typed_input`
  doesn't read (it reads `tuple[str, ...]`), so the documented `{"root": ".", "crates": [...]}`
  fails with `TypeError: typed_input: crates has a type it does not read: list[str]`. Fix:
  `crates: tuple[str, ...] = HOT`. Until then, run it without `crates` and filter.
- For db-ty-port-solver (fulfill.rs, obligation_ctxt.rs, inspect.rs): `Obligation<'db, T>` is now
  `Copy` when `T` is (`PredicateObligation` is 20 bytes of `Copy` fields). Every
  `obligation.clone()` becomes a plain copy (`*obligation` / `obligation`), which also clears
  those hot-lint findings and clippy's `clone_on_copy`.

## db-ty-port-solver

- `crates/hir-ty/src/next_solver/infer/mod.rs` (db-ty-port-infer): replace
  `next_term_var_of_kind(term, span)` with rustc's `next_term_var_of_alias_kind`
  (`rustc_infer/src/infer/mod.rs` at 16d1ffac, line 989). Its only callers were
  `normalize.rs` and `structural_normalize.rs`, which now call the new one:

  ```rust
  pub fn next_term_var_of_alias_kind(&self, alias_term: AliasTerm<'db>, span: Span) -> Term<'db> {
      match alias_term.kind {
          AliasTermKind::ProjectionTy { .. }
          | AliasTermKind::InherentTy { .. }
          | AliasTermKind::OpaqueTy { .. }
          | AliasTermKind::FreeTy { .. } => self.next_ty_var(span).into(),
          AliasTermKind::FreeConst { .. }
          | AliasTermKind::InherentConstSelf { .. }
          | AliasTermKind::InherentConstImpl { .. }
          | AliasTermKind::AnonConst { .. }
          | AliasTermKind::ProjectionConst { .. } => self.next_const_var(span).into(),
      }
  }
  ```

  Why: `PredicateKind::AliasRelate` is gone; rustc's normalizers register a `ProjectionClause`
  whose term is a fresh variable of the alias's kind. Both files also call the inherent
  `InferCtxt::deeply_resolve_ignoring_regions` (infer/mod.rs), which must stay.

> forwarded to db (db-ty; window `db-next2`, told by tmux).
> forwarded by db-ty-next1 (19:40): `infer/` is solver-sync's now; sent to `solver-sync-infer-core`.
> applied by db-ty-port-infer: `InferCtxt::next_term_var_of_alias_kind` is in `infer/mod.rs`
> (rustc's), `next_term_var_of_kind` is gone; `deeply_resolve_ignoring_regions` stays.

## db-ty-port-ty

- `crates/hir-def/src/signatures.rs` and `expr_store/lower.rs` (hir-def's owner, through lane
  db), for R12 of `v2/requests/solver-sync-callers.md` (superset-of-rust: the nightly's
  `FnSigKind` carries the `#[splat]` argument, hir-def drops it). Add
  `pub splatted: Option<u8>` to `FunctionSignature`, set by `lower_function` as rustc's
  `FnDecl::splatted` (`compiler/rustc_ast/src/ast.rs` at 16d1ffac) and `lower_fn_decl`
  (`rustc_ast_lowering/src/lib.rs`) do: the index into `params` (the self parameter first,
  cfg-disabled parameters skipped, the `...` one not counted) of the first parameter with a
  `#[rustc_splat]` attribute (feature `splat`), ignoring indices `>= u8::MAX`, and `None` when the
  function is C-variadic. `lower.rs` `callable_item_signature` already passes
  `data.splatted, data.params.len()` to `FnSigKind::new` and `expect`s it, as rustc's lowering
  `unwrap`s. rustc's AST validation errors for a splat index past 254 and for splat with `...`
  are diagnostics' to add (`sym::rustc_splat` needs to be interned; fix-names owns the attribute
  enum). Until this lands, `lower.rs` does not compile: it is the one site of my part that
  waits on another file.

> forwarded to db (hir-def and hir-ty; window `db-next2`, told by tmux).

## db-ty-d-mir

### 1. Codegen (`crates/hir-ty-mir/src/mir/lower.rs`): `MirBody::unreachable_after_calls`

`crates/hir-ty/src/mir.rs` now declares `UnreachableAfterCall { call: MirSpan, next: MirSpan,
descr: &'static str, ty: StoredTy }` and the field `MirBody::unreachable_after_calls:
Vec<UnreachableAfterCall>` (hashed and shrunk with the rest), as
`v2/requests/diagnostics.md`, "diagnostics-lints-bodies: what follows a call of an uninhabited
type", writes them. hir-ty-mir does not compile until its one `MirBody { .. }` literal has it:

- `lower.rs:294` (`let mir = MirBody {`), after `promoted_constants: Vec::new(),`:
  `unreachable_after_calls: Vec::new(),`
- Then fill it where lowering gives a call of an uninhabited type no target (`lower_call`'s
  `is_uninhabited`, `lower.rs` ~1065, ~1100, ~1115), as rustc's `LintAndRemoveUninhabited`
  (`compiler/rustc_mir_transform/src/lint_and_remove_uninhabited.rs`): only when the type is
  not `!`, is uninhabited from the body's module, and the function's return type is inhabited;
  `call` the call's span, `next` the span of the first statement of user code the target would
  have had (`find_unreachable_code_from`: past the `()` and return-place assignments,
  `StorageLive`/`StorageDead`, and a `Goto`/`Drop` into a block only it leads to), `descr`
  `"definition"` when that statement is a `let` (`FakeRead`), else `"expression"`, `ty` the
  call's type. In the order of the calls' blocks. Test against rustc's
  `tests/ui/reachable/unreachable-try-pattern.rs` and its `.stderr`, and:
  `let x: Void = f(); 1` gives one entry with `descr == "expression"`, `let y = 1;` after the
  call gives `"definition"`, a call returning `!` gives none.

> forwarded to db for codegen.

### 2. Codegen (`crates/hir-ty-mir`) and coordinator (`crates/hir-ide/src/impl_db_macro.rs`): `coroutine_witnesses`

`db-ty` item 7 above and `v2/requests/diagnostics.md`, "coroutine witness types for significant
drops". Three edits that land together (the macro stops compiling if the trait method comes
first); db (window `db-next2`) says go, then I add the declaration to `hir-ty/src/db.rs`, beside
the coroutine MIR queries:

```rust
    /// The types an `async` block holds across a yield, by which its future implements an
    /// auto trait: rustc's `mir_coroutine_witnesses` (`CoroutineLayout::field_tys` without
    /// what `ignore_for_traits` marks). Generic in the parameters of the coroutine's parent,
    /// with regions erased.
    fn coroutine_witnesses<'db>(&'db self, def: InternedCoroutineId<'db>) -> &'db [StoredTy];
```

**a. Codegen, `crates/hir-ty-mir/src/mir/transform.rs`**, after
`runtime_mir_body_for_coroutine_by_move_query` (and `coroutine_witnesses_query` added to the
`pub use transform::{..}` list in `crates/hir-ty-mir/src/mir.rs`). The locals are the ones the
runtime body keeps in the future (`MirBody::locals_held_across_yields`, which already drops the
receiver, the captures used in place and the promoted constants), so a future's auto traits and
its layout (`hir-ty/src/layout.rs:579`) see the same fields. Of the by-move body nothing is
asked: rustc's witnesses are of the coroutine's own body.

```rust
/// The types an `async` block holds across a yield: rustc's `mir_coroutine_witnesses`, the
/// `CoroutineLayout::field_tys` its future implements auto traits by, in the order the body
/// declares the locals. Not what `ignore_for_traits` marks: the temporary made for a place of a
/// static that is not `#[thread_local]` (another access makes it again) and a fake borrow (only
/// fake reads read it). Empty for a body MIR lowering refuses, as rustc's for a body with
/// errors: that body is reported (`BorrowckSkipped`) and nothing is built from it.
#[salsa::tracked(returns(deref))]
pub fn coroutine_witnesses_query<'db>(
    db: &'db Db,
    coroutine: InternedCoroutineId<'db>,
) -> Box<[StoredTy]> {
    let Ok(body) = db.runtime_mir_body_for_coroutine(coroutine) else {
        return Box::default();
    };
    let held = body.locals_held_across_yields();
    let mut ignored: SmallVec<[LocalId; 4]> = SmallVec::new();
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            let StatementKind::Assign(place, value) = &statement.kind else { continue };
            if !place.projection.is_empty() {
                continue;
            }
            let ignore = match value {
                Rvalue::Use(Operand { kind: OperandKind::Static(id), .. }) => {
                    !AttrFlags::query(db, (*id).into()).contains(AttrFlags::THREAD_LOCAL)
                }
                Rvalue::Ref(BorrowKind::Shallow, _) => true,
                _ => false,
            };
            if ignore {
                ignored.push(place.local);
            }
        }
    }
    held.locals
        .iter()
        .filter(|local| !ignored.contains(local))
        .map(|&local| body.locals[local].ty.clone())
        .collect()
}
```

over the concrete database (common.md, "The database is concrete"; `db.runtime_mir_body_for_coroutine`
resolves statically to `impl HirDatabase for Db`), with `use base_db::Db;`,
`use hir_def::attrs::AttrFlags;`, `use smallvec::SmallVec;` and, from `super`,
`BorrowKind, LocalId, Operand, OperandKind, Rvalue, StatementKind` (all `hir_ty::mir::*`), and
`StoredTy` from `crate::next_solver` (or `hir_ty::next_solver`). No `lru`: the result is a few
types per coroutine, read by every auto-trait goal of its future, and not dropped by
`hir/src/evict.rs`. hir-ty's kind test does not see it (hir-ty-mir's query).

When frontend-db's providers land (`docs/v2/plan/lanes/frontend/db.md`, "Providers, for the
queries hir-ty reads but can't compute"), this query moves with its MIR siblings: declared in
hir-ty as a tracked function over `&Db` whose body calls `hir_ty::providers()`, its code the
`fn` above registered by `hir_ty_mir::provide()`, and b's method goes. Until then it is a
trait method like the other MIR queries.

**b. Coordinator, `crates/hir-ide/src/impl_db_macro.rs`**: `StoredTy` joins the
`next_solver::{..}` import at the top of the macro (`next_solver::{Allocation, GenericArgs,
StoredGenericArgs, StoredTy, Ty}`), and after `runtime_mir_body_for_coroutine_by_move`:

```rust
                fn coroutine_witnesses<'db>(
                    &'db self,
                    def: InternedCoroutineId<'db>,
                ) -> &'db [StoredTy] {
                    mir::coroutine_witnesses_query(self, def)
                }
```

**c. Then solver-sync** (`next_solver/interner.rs`, `coroutine_hidden_types`, in its inbox):
read `db.coroutine_witnesses(def_id.0)`, each type with its erased regions replaced by fresh
bound regions, as rustc's `TyCtxt::coroutine_hidden_types`, under the `EarlyBinder` the caller
instantiates with the coroutine's args. rustc keeps goals on a coroutine of the body being
type-checked stalled (`TypingMode::Analysis { defining_opaque_types_and_generators }`,
`check_coroutine_obligations` after borrowck); without that the witness query runs MIR lowering
of the body being inferred, a cycle.

Test (written, `crates/hir/src/tests.rs`,
`a_future_is_send_only_when_what_it_holds_across_an_await_is`): an `async` block holding a
`NotSend(*const u8)` across `.await` is not `Send`; one that moves it out before is.

> waiting for db's go (asked by tmux, window `db-next2`).

## db-ty-port-infer

`InferCtxt` (`crates/hir-ty/src/next_solver/infer/`) now has rustc 16d1ffac's names; the old ones
are gone, with no aliases. Each caller below is in its owner's file; rename it there.

| old | new |
|---|---|
| `InferCtxt::resolve_vars_if_possible(v)` | `deeply_resolve_ignoring_regions(v)` |
| `InferCtxt::probe_ty_var(vid)` / `probe_const_var(vid)` | `try_resolve_ty_var(vid)` / `try_resolve_const_var(vid)` |
| `InferCtxt::opportunistic_resolve_{int,float}_var(vid)` | `shallow_resolve_{int,float}_var(vid)` |
| `InferCtxt::unresolved_variables() -> Vec<Ty>` | `unresolved_root_variables() -> (Vec<TyVid>, Vec<IntVid>, Vec<FloatVid>)` |
| `InferCtxt::next_term_var_of_kind(term, span)` | `next_term_var_of_alias_kind(alias_term, span)` |
| `RegionConstraintCollector::opportunistic_resolve_var(cx, vid)` | `shallow_resolve_region_var(cx, vid)` |
| `RegionConstraintCollector::probe_value(vid)` | `try_resolve_region_var(vid)` |
| `RegionConstraintCollector::data()` | `legacy_data()` (`InferCtxt::region_constraint_data()` stays) |
| `resolve::OpportunisticVarResolver` | `resolve::DeepResolverIgnoringRegions` |

Sites (2026-10-29):

- hir-ty: `method_resolution.rs` 530, 656; `method_resolution/probe.rs` 398, 1951;
  `solver_errors.rs` 195, 201; `infer/coerce.rs` 1612; `autoderef.rs` 52, 202, 283, 368;
  `lib.rs` 805; `infer/pat.rs` 640; `infer/unify.rs` 233, 340, 351, 358 (`InferenceTable`'s own
  `resolve_vars_if_possible` wrapper may keep its name; its body calls the new one);
  `infer/closure/analysis.rs` 256, 933; `infer/closure/analysis/expr_use_visitor.rs` 1161, 1277;
  `opaques.rs` 221.
- hir-ty `infer/fallback.rs` 64: `unresolved_root_variables()` gives root vids, as rustc's
  `fallback.rs`; build the types with `Ty::new_var` / `new_int_var` / `new_float_var`.
- hir-ty `next_solver/util.rs` 650 (solver-sync): `shallow_resolve_region_var`.
- hir-ty-mir `mir/monomorphization.rs` 482.
- hir-ty-borrowck: `liveness.rs` 911; `regions.rs` 4521, 7321 (`deeply_resolve_ignoring_regions`),
  7187 (`shallow_resolve_region_var`), 6471, 7192, 7332 (`try_resolve_region_var`).
- hir-ty-wfcheck: `outlives.rs` 536 (`shallow_resolve_region_var`); `compare.rs` 193, 201.

> forwarded to db (db-ty; window `db-next2`, told by tmux).
> applied by db-ty (db-ty-next2, 20:20): every site in db-ty's paths. That is `solver_errors.rs`,
> `infer/coerce.rs`, `autoderef.rs`, `lib.rs`, `infer/pat.rs`, `infer/unify.rs`,
> `infer/closure/analysis.rs`, `expr_use_visitor.rs`, `next_solver/solver.rs`, `infer/fallback.rs`
> (root vids made into `Ty::new_var`/`new_int_var`/`new_float_var`, in that order), borrowck's
> `liveness.rs` and `regions.rs`, and wfcheck's `outlives.rs`. `compare.rs` already used the new
> names. Still open, in other owners' files: items' `method_resolution.rs` 530 and 656,
> `method_resolution/probe.rs` 398 and 1951, and `opaques.rs` 221; solver-sync's
> `next_solver/util.rs` 650; codegen's `hir-ty-mir/src/mir/monomorphization.rs`.

## frontend-nameres-impls: kinds for the per-module impl tables (2026-10-29)

What: `crates/hir-ty/src/db.rs`, `kinds!`, a new group after the opaque types. Old:

```rust
        DEFINE_OPAQUE_AT = 0x2486, "define_opaque_at";

        // Inputs.
```

New:

```rust
        DEFINE_OPAQUE_AT = 0x2486, "define_opaque_at";

        // Per-module impl tables (frontend.md items 2 and 3).
        INHERENT_IMPLS_FOR_MODULE = 0x2490, "InherentImpls::for_module";
        INHERENT_IMPLS_MODULES_BY_SELF_TY = 0x2491, "InherentImpls::modules_by_self_ty";
        INHERENT_IMPLS_MODULES_OF_SELF_TY = 0x2492, "InherentImpls::modules_of_self_ty";
        INHERENT_IMPLS_IN_MODULE_FOR_SELF_TY = 0x2493, "InherentImpls::in_module_for_self_ty";
        TRAIT_IMPLS_FOR_MODULE = 0x2494, "TraitImpls::for_module";
        TRAIT_IMPLS_MODULES_BY_TRAIT = 0x2495, "TraitImpls::modules_by_trait";
        TRAIT_IMPLS_MODULES_OF_TRAIT = 0x2496, "TraitImpls::modules_of_trait";
        TRAIT_IMPLS_IN_MODULE_FOR_TRAIT = 0x2497, "TraitImpls::in_module_for_trait";
        TRAIT_IMPLS_FOR_CRATE_AND_TRAIT = 0x2498, "TraitImpls::for_crate_and_trait";

        // Inputs.
```

Why: `v2/requests/items.md`, `## frontend-nameres-impls: per-module impl collection` adds these
nine tracked methods to `method_resolution.rs` (items' file; items is DONE, so `requests`
applies it). `every_query_has_a_kind` fails without these lines, and they fail
`"... is no query of hir-ty"` without that edit: land both together. `0x2490..0x2498` is
unused and claims none of the numbers kept for speed-types-probe's children (`0x2457..0x245f`,
`0x247b..0x247d`, `0x2487..0x248a`). No query is removed.
Status: open

> forwarded to db (db-ty; told by tmux). requests applies the items half once this and db's re-export land.
> applied by db-ty (db-ty-next3, 21:25) at **`0x24b0..0x24b8`**, same order and names:
> `0x2490..0x24a7` is MIR's (`MIR_BODY` .. `MONOMORPHIZED_OPTIMIZED_MIR_BODY_FOR_COROUTINE_BY_MOVE`).
> Until the items half lands, `every_query_has_a_kind` fails on these nine.

## db-ty-d-infer2

> forwarded to diagnostics (blaze-diagnostics and ide-diagnostics; told by tmux).

### 1. Diagnostics (blaze-diagnostics) and ide-diagnostics: ambiguity errors and the other trait solver errors (task 22 part 3)

hir now gives (`crates/hir/src/diagnostics.rs`):

- `TypeMustBeKnown { at_point, top_term, kind: TypeAnnotationNeeded, ambiguity: Option<TypeAmbiguity> }`.
  `kind` is rustc's `TypeAnnotationNeeded` (`E0282`, `E0283`, `E0284`); `ambiguity` is `Some` for
  a goal still pending when inference is done (rustc's `report_ambiguity_errors` ->
  `maybe_report_ambiguity`), with `primary: AmbiguousGoal { goal: SolverGoal, candidates }` and
  `related: Vec<AmbiguousGoal>` (the goals rustc merges in as notes: same inference variable,
  same place). `candidates` are `AmbiguityCandidate::{Impl(hir::Impl), WhereClause(hir::TraitRef)}`,
  non-empty only where rustc lists them (`applicable_impls_to_mention`). `SolverGoal` is the
  goal: `Trait(TraitClause)`, `HostEffect { trait_ref, maybe_const }`, `Projection { alias, term }`,
  `NormalizesTo { alias }`, `WellFormed`, `Subtype`, `Coerce`, `ConstEvaluatable`,
  `ConstArgHasType`, `ConstEquate`, `DynCompatible(Trait)`, `UnstableFeature(Symbol)`,
  `Outlives`, `Ambiguous`.
- A new `AnyDiagnostic::TraitSolverError(TraitSolverError { span, kind: TraitSolverErrorKind })`
  for rustc's `report_fulfillment_error` codes hir had no diagnostic for, which hir-ty now
  reports instead of dropping (a body with one used to build): `GoalUnmet(SolverGoal)` (a
  `[const] Trait` bound: `report_host_effect_error`; E0038 of `DynCompatible`; "the type `{ty}`
  is not well-formed" of `WellFormed`), `Overflow { goal, suggest_increasing_limit }` (E0275,
  `report_overflow_no_abort`), `ProjectionMismatch(goal)` (E0271 "type mismatch resolving
  `{goal}`", `report_projection_error`), `TypesDiffer { expected, found }` (E0308 of a subtype
  or coerce goal), `ConstArgHasWrongType { ct, ct_ty, expected_ty }` ("the constant `{ct}` is
  not of type `{expected_ty}`"), `HigherRankedOutlives` ("higher-ranked lifetime bound could not
  be satisfied"), `Unreported(UnreportedSolverError)` (codes only the old solver gives; none
  occurs today).

Wanted, so the workspace builds again:

- `crates/blaze-diagnostics/src/lower/mod.rs`, beside `AnyDiagnostic::TypeMustBeKnown(d) => ...`:
  `AnyDiagnostic::TraitSolverError(d) => trait_solver_error::lower(cx, &d),` and a module
  `lower/trait_solver_error.rs` giving each kind's rustc message and code above
  (`compiler/rustc_trait_selection/src/error_reporting/traits/fulfillment_errors.rs` at
  16d1ffac).
- `crates/blaze-diagnostics/src/lower/type_must_be_known.rs`: use `d.kind` for the code
  instead of `"E0282"` always, and with `d.ambiguity` give rustc's `maybe_report_ambiguity`
  forms (`compiler/rustc_trait_selection/src/error_reporting/traits/ambiguity.rs`): E0283
  "type annotations needed" with "cannot satisfy `{goal}`" and the "multiple `impl`s satisfying
  `{goal}` found" note listing `candidates`; E0284 "cannot satisfy `{goal}`" / "cannot
  normalize `{alias}`"; one "the type must also implement `{trait}`"-style note per
  `related`. The module doc's "the ambiguous goals rustc reports as E0283 and E0284, which the
  solver's errors drop" is then untrue.
- `crates/ide-diagnostics/src/lib.rs`, beside `AnyDiagnostic::TypeMustBeKnown(d) => ...`: an
  arm for `AnyDiagnostic::TraitSolverError(d)` (a handler module like
  `handlers/unimplemented_trait.rs`).

### 2. hir-ide and Diagnostics: the types of a type mismatch before literal fallback (task 22 part 2)

`InferenceDiagnostic::TypeMismatch` gains `at_error: Option<Box<(StoredTy, StoredTy)>>`: the
expected and found types as inference had them at the error, an undecided integer or float
literal's type as `InferTy::FreshIntTy(0)` / `FreshFloatTy(0)`. hir's `TypeMismatch` gains
`at_error: Option<(String, String)>`, the two printed.

- `crates/hir-ide/src/display.rs` (`HirDisplay for Ty`, the `TyKind::Infer(..) => write!(f, "_")?`
  arm): print rustc's `{integer}` and `{float}` outside source code, before that arm:
  ```rust
  TyKind::Infer(InferTy::IntVar(_) | InferTy::FreshIntTy(_)) if !f.display_kind.is_source_code() => {
      write!(f, "{{integer}}")?
  }
  TyKind::Infer(InferTy::FloatVar(_) | InferTy::FreshFloatTy(_)) if !f.display_kind.is_source_code() => {
      write!(f, "{{float}}")?
  }
  ```
  Without it hir's `at_error` prints `_` for them.
- `crates/hir-ide/src/tests.rs` lines 281 and 416: the patterns
  `InferenceDiagnostic::TypeMismatch { node, expected, found }` need `, ..`.
- `crates/blaze-diagnostics/src/lower/type_mismatch.rs`: print `d.at_error` where it is `Some`
  (rustc's "expected integer, found `&str`", "expected `u8`, found floating-point number": a
  type that is wholly `{integer}` / `{float}` is "integer" / "floating-point number",
  `TypeError::Sorts` in `rustc_middle::ty::error`); then `tests/bodies/types.rs`
  `match_arms_of_incompatible_types_skip_the_diverging_one` can be un-ignored.
  `InvalidRangePatType` gets its types with task 37.

### 3. Diagnostics (blaze-diagnostics): E0525 from the closure and its kind origin (task 22 parts 5 and 6)

hir now gives `ClosureKindMismatch.closure: Option<InFile<AstPtr<ast::ClosureExpr>>>` (the
closure rustc points at, from the solver goal's self type) and
`hir::Closure::kind_origin(db) -> Option<(InFile<AstPtr<Either<ast::Expr, ast::Pat>>>, ClosureCapture)>`:
rustc's `closure_kind_origins` entry (`compiler/rustc_hir_typeck/src/upvar.rs`,
`process_collected_capture_information`): the use that made the closure `FnOnce` or `FnMut`, with
the capture it belongs to, for `move` closures too.

- `crates/blaze-diagnostics/src/lower/closure_kind_mismatch.rs`: take the closure from
  `d.closure` (`cx.sema` to root it) instead of `closure_of` from the argument, and the label
  of `origin` from `kind_origin` (span = the node, place =
  `capture.display_place_source_code(cx.db, cx.edition)`) instead of the first moving or
  mutating capture of a non-`move` closure; drop `closure_of`, `origin` and the module doc's
  "The front end gives neither ..." paragraph.

### 4. Diagnostics (blaze-diagnostics) and ide-diagnostics: E0641 and the other cast errors rustc gives (task 23)

`hir_ty::infer::cast` now refuses what rustc's `check_cast` refuses
(`compiler/rustc_hir_typeck/src/cast.rs` at 16d1ffac), so `CastError` (re-exported as
`hir::CastError`) has four new variants: `UnknownCastPtrKind` and `UnknownExprPtrKind` (E0641,
`0 as *const _`), `CastEnumDrop` (a field-less enum with a `Drop` impl cast to an integer) and
`ForeignNonExhaustiveAdt` (E0606 of an enum of another crate with a `#[non_exhaustive]`
variant). `hir::InvalidCast` gains `expr_pointer`/`cast_pointer: Option<hir::CastPointerKind>`
(`Thin`, `VTable(Option<Trait>)` with the principal, `Length`, `OfAlias`, `OfParam(TypeParam)`),
set where rustc's report reads them, and `added_auto_traits: Vec<Trait>` of
`PtrPtrAddingAutoTraits`. The workspace does not build until the two exhaustive matches have
arms for the new variants:

- `crates/blaze-diagnostics/src/lower/invalid_cast.rs`: rustc's messages
  (`compiler/rustc_hir_typeck/src/errors.rs`: `CastUnknownPointer` "cannot cast to a pointer of
  an unknown kind" / "cannot cast from ..." with "needs more type information" and the
  "the type information given here is insufficient ..." note; `CastEnumDrop` "cannot cast enum
  `{expr_ty}` which implements `Drop` to an integer `{cast_ty}`"; `ForeignNonExhaustiveAdt` E0606
  with "cannot cast an enum with a non-exhaustive variant when it's defined in another crate").
  `DifferingKinds`' notes ("the trait objects may have different vtables", "the pointers may
  have different metadata", "the pointers have different metadata"), `IntToWideCast`'s
  "a vtable"/"a length"/"type-specific metadata" and E0804's trait list can then come from the
  fields instead of being worked out from the types.
- `crates/ide-diagnostics/src/handlers/invalid_cast.rs`: the arms (the E0641 one is there,
  commented out), and the commented-out test `order_dependent_cast_inference` can come back
  (rustc's `tests/ui/cast/order-dependent-cast-inference.rs`).

### 5. Diagnostics (blaze-diagnostics): pattern checking as rustc gives it (task 37), and `sym::Vec`

hir now gives (from `compiler/rustc_hir_typeck/src/pat.rs` at 16d1ffac):

- `MismatchedTupleStructPatArgCount { variant: hir::Variant, had_err: bool }`: rustc's "missing
  parentheses" suggestion only when `!had_err`. The subpatterns are now checked against the
  error type, as rustc's `on_error`, so their bindings report nothing more.
- `InvalidRangePatType<'db>` (now with a lifetime) `{ lhs, rhs: Option<(Type, bool)> }`: each
  end's type as inference had it (an undecided literal's as `{integer}` once hir-ide prints it,
  item 2) and whether it fails. Also pushed for rustc's second `emit_err_pat_range` (both ends
  infer to a type that is not `char` or numeric, both failing), and not pushed when an end's
  type has an error (rustc downgrades it).
- `NoSuchField` of a record pattern's unknown fields comes after `NonExhaustiveRecordPat` and
  the union errors, before `RecordMissingFields`, as rustc emits them.
- `hir::Crate::lang_origin(self, db) -> Option<LangCrateOrigin>`, no clone.

Wanted:

- `crates/blaze-diagnostics/src/lower/mismatched_tuple_struct_pat_arg_count.rs`: the variant
  and `had_err` from `d`, not the path; `lower/invalid_range_pat_type.rs`: the labels from
  `d.lhs`/`d.rhs`; `tests/bodies/patterns.rs`
  `a_range_with_one_bad_end_names_the_type_of_the_other` un-ignored (after item 2's
  `{integer}` print). `lower/expected_array_or_slice_pat.rs` can use `Crate::lang_origin`.
- `crates/intern/src/symbol/symbols.rs` (not db-ty's): `Vec` beside the other type names, so
  `expected_array_or_slice_pat.rs` compares a symbol, not a string.

> forwarded to diagnostics (blaze-diagnostics and ide-diagnostics; told by tmux).

### 6. Diagnostics (blaze-diagnostics): the value a path resolved to, from inference (task 17)

`InferenceResult::value_resolution(ExprOrPatId) -> Option<ValueNs>` is what inference resolved
each value path (expression or pattern) to, written in `infer/path.rs` `infer_path`, persisted
and hashed with the result. An associated item's path is the function or const resolution gave
(the impl's pick is still `assoc_resolutions_for_expr`).

Wanted: `crates/blaze-diagnostics/src/lints/bodies/types/util.rs` `path_value`, keeping the
`assoc_resolutions_for_expr` arm first, then
`b.infer.value_resolution(expr.into())` in place of the `ExprScopes`/`FnDef`/
`resolver_for_scope` steps (drop their imports). A path that does not resolve gives `None`, as
now.

### 7. Diagnostics (blaze-diagnostics): `MethodCallIllegalSizedBound` carries the pick (task 29)

`hir::MethodCallIllegalSizedBound` gains what rustc's `lookup_method` gathers
(`compiler/rustc_hir_typeck/src/method/mod.rs` at 16d1ffac): `method: Function` (the picked,
the trait's for an object pick), `bound: Option<GenericDef>` (whose own bounds or where clause
hold the `Self: Sized` clause: the method's, or its trait's or impl's; `None` where only
elaboration implies it, rustc's `DUMMY_SP`), `needs_mut` and `candidates: Vec<Trait>` (rustc's
traits found by the probe of all traits; traits in other bodies' blocks are not in that probe).

Wanted, in `crates/blaze-diagnostics/src/lower/method_call_illegal_sized_bound.rs`, rustc's
`report_method_error` arm for `IllegalSizedBound` (`method/suggest.rs`):

- the message "the `{name}` method cannot be invoked on `{rcvr_ty}`" when `d.needs_mut` (the
  receiver's type from the call's receiver), else "... on a trait object";
- the "this has a `Sized` requirement" label only when `!d.needs_mut`, on the `Sized` bound on
  `Self` in `d.bound`'s own where clause or bounds (a direct `Sized`, not a trait that implies
  it), none when `d.bound` is `None`; `sized_bound`'s name match on `Self` and its
  `all_supertraits` test go;
- the "{an}other candidate{s} {were} found in the following trait{s}, perhaps add a `use` for
  {it|one of them}:" help with `use` suggestions of `d.candidates`;
- when `needs_mut` and the receiver is a reference, "you need `{&mut T}` instead of `{&T}`": the
  `mut ` suggestion before the parameter's type when the receiver is a `&`-typed parameter,
  else a help.

### 8. Diagnostics (blaze-diagnostics) and ide-diagnostics: unstable name collisions and the prelude-collision lints (task 36)

hir now gives (`compiler/rustc_hir_typeck/src/method/probe.rs` and
`method/prelude_edition_lints.rs` at 16d1ffac):

- `Semantics::unstable_candidates(Either<ast::Expr, ast::Pat>) -> Vec<(AssocItem, Symbol)>`: the
  unstable items, each with its feature, that the probe of a method call or type-relative path
  passed over for a stable pick (rustc's `Pick::unstable_candidates`).
- `AnyDiagnostic::EditionDependentMethodCall(EditionDependentMethodCall { call_expr, receiver,
  lint: EditionLint, method: Function, inherent, autoderefs, autoref: Option<ReceiverAdjustment> })`
  and `AnyDiagnostic::FullyQualifiedCallFrom2018(FullyQualifiedCallFrom2018<'db> { expr_or_pat,
  item: AssocItem, self_ty: Type<'db> })`. `EditionLint` is the lint
  (`Rust2021PreludeCollisions`, `Rust2024PreludeCollisions`, `ArrayIntoIter`,
  `BoxedSliceIntoIter`, with `edition()`); they are lints, not errors.

Wanted, so the workspace builds again:

- `crates/blaze-diagnostics/src/lower/mod.rs` and `crates/ide-diagnostics/src/lib.rs`: arms for
  the two new `AnyDiagnostic` variants.
- blaze-diagnostics: rustc's `unstable_name_collisions` from `unstable_candidates`
  (`Pick::maybe_emit_unstable_name_collision_hint`: "a method with this name may be added to the
  standard library in the future" / "an associated item ...", the "call it with fully qualified
  syntax" help naming the pick's trait, and one "add `#![feature({feature})]`" help per
  candidate; only when the pick is a trait's, as rustc's `if let Some(..) = pick.item.trait_container`);
  for every method call (an `ast::MethodCallExpr`), a field access with no field, and a
  type-relative path.
- blaze-diagnostics: the two prelude lints' messages and suggestions
  (`AmbiguousTraitMethodCall`, `AmbiguousTraitMethod`, `AmbiguousTraitAssocFunc` there): "trait
  method `{name}` will become ambiguous in Rust {edition}" with "disambiguate the method call"
  (`({autoref}{derefs}{receiver})` or `as *const _`) when `inherent`, else the fully qualified
  call `{Trait}::{name}({adjusted receiver}, {args})`; "trait-associated function `{name}` will
  become ambiguous in Rust 2021" with `<{self_ty} as {Trait}>::{name}`. Each lint's level as
  rustc's (`rust_2021_prelude_collisions` and `rust_2024_prelude_collisions` allow,
  `array_into_iter` and `boxed_slice_into_iter` warn). Then
  `tests/lints/bodies/calls/desugar.rs`'s ignored test can be un-ignored.
- `crates/intern/src/symbol/symbols.rs` (not db-ty's): `from_iter` and `FromIterator`, so
  `hir-ty/src/infer/prelude_lints.rs` compares symbols instead of `name.as_str() ==
  "from_iter"` and `Symbol::intern("FromIterator")`.

## db-ty-d-hir

> forwarded to diagnostics (blaze-diagnostics is its, running; told by tmux).

### 1. Diagnostics (blaze-diagnostics): `hir::TraitPredicate` is `hir::TraitClause`

`hir::TraitPredicate` and `hir::PredicatePolarity` are `hir::TraitClause` and
`hir::ClausePolarity` (`crates/hir/src/lib.rs`, after the solver's rename), so blaze-diagnostics
does not build. Wanted, a rename and nothing else: `hir::TraitPredicate` -> `hir::TraitClause`
and `hir::PredicatePolarity` -> `hir::ClausePolarity` in `lower/unimplemented_trait.rs` (lines
121, 149, 151, 251, 432, 557, 729, 730, 769), `print.rs` (112, 115, 123), `print/printer.rs`
(19, 680) and `print/types.rs` (24, 422, 426, 437, 486, 501, 564). `TraitClause::to_next_solver`
is what `TraitPredicate::to_next_solver` was asked to be.

### 2. Diagnostics: the hir APIs db-ty-next1 asked for are there

For the checks waiting on them (each noted `> applied by db-ty-d-hir` in
`v2/requests/diagnostics.md`): `Type::{to_next_solver, derived_from_next_solver,
variant_fields, as_array_element, as_foreign}`, `TraitRef::{to_next_solver,
derived_from_next_solver}`, `GenericDef::where_predicates`, `Adt::unused_params`,
`Crate::{crate_types, diagnostic_item_adt}`, `Function::{signature, trait_signature_in_impl,
impl_signature_mismatch, proc_macro_signature_mismatch}` (`FnSignature`,
`ImplSignatureMismatch`, `ProcMacroSignatureMismatch`, `SignatureMismatchAt`),
`HasAttrs::{deprecation, stability}`, `impl HasSource for Closure`, and `BorrowckSkipped` given
for every unchecked body.

## dedup: the leak check's SCC graph onto `radix::Csr::graph`

From `dedup-order` (report `/tmp/blaze-lanes/v2-dedup-order.md`). `blaze_ids::radix::Csr<T>`
(`crates/blaze-radix/src/csr.rs`) is now the one stable counting sort into compressed sparse rows:
`Csr::build(rows, pairs)` walks a cloneable iterator of `(row: u32, item)` twice (count, then
place at each row's cursor), `Csr::graph(nodes, edges)` gives each source's targets ascending
(two passes, by target then by source), `dedup_rows()` drops repeats within a row, and
`into_parts()` gives `(starts, items)` (`starts` has `rows + 1` entries). Its tests have the old
copies as the proptest reference.

`crates/hir-ty/src/next_solver/infer/region_constraints/leak_check.rs`, `Sccs::new`: delete the
inner `fn counting_pass` and replace from `let (by_target, _) = counting_pass(...)` through
`drop(sorted);` with

```rust
        // alloc: per leak check past a binder, sized to its region graph.
        let (starts, targets) = radix::Csr::graph(num_nodes, edges.iter().copied()).into_parts();
        drop(edges);
```

(`radix` is already imported from `blaze_ids`; `starts` has `num_nodes + 1` entries and
`targets` the targets in `(source, target)` order, as before). The doc comment stays true. Why:
one counting sort in the tree; this one cloned its starts and kept a pair per edge in between.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-ty (db-ty-next3, 21:27), as written.

## db-ty-d-lower

From `db-ty-d-lower` (tasks 19, 33, 34, 35, 40, 42 of `db-dispatch/db-ty-next1.md`; report
`/tmp/blaze-lanes/v2-db-ty-d-lower.md`). Items 1, 2 and 4 are needed for the tree to build:
hir and hir-ty use what they add.

1. **hir-def** (`crates/hir-def/src/expr_store/source_map.rs`, after `type_syntax`, about line
   836): hir's `MissingLifetimes` points at a `'_` it reads back.

   ```rust
       pub fn lifetime_syntax(&self, id: LifetimeRefId) -> Result<LifetimeSource, SyntheticSyntax> {
           self.lifetime_map_back.get(id).ok_or(SyntheticSyntax)
       }
   ```

2. **items** (`crates/hir-ty/src/method_resolution/confirm.rs`, in `impl GenericArgsLowerer for
   LowererCtx`, after `report_arg_mismatch`, about line 388): the trait gained
   `report_late_bound_lifetime_args`, rustc's `prohibit_explicit_late_bound_lifetimes`. A method
   call is always the `late_bound_lifetime_arguments` lint, so `hard_error` is not read.

   ```rust
               fn report_late_bound_lifetime_args(&mut self, def: GenericDefId, _hard_error: bool) {
                   self.ctx.push_diagnostic(InferenceDiagnostic::MethodCallLateBoundLifetimeArgs {
                       expr: self.expr,
                       def,
                   });
               }
   ```

3. **diagnostics** (`crates/blaze-diagnostics/src/print/printer.rs`, the `match br.kind` about
   line 240): a `for<'a>` now binds `BoundRegionKind::NamedForPrinting(name)` with the name
   written, as rustc's `Named` leads to it, so `for<'x> fn(&'x u8)` prints `'x`, not a fresh
   letter (rustc's `name_all_regions` takes `get_name` for any binder). Add before `_ => None`:

   ```rust
                   BoundRegionKind::NamedForPrinting(name)
                       if !self.mode.erase && name.0.as_str() != "'_" =>
                   {
                       Some(name.0.clone())
                   }
   ```

   hir-ide's `display.rs` (about line 2332) prints a bound region as `'?d.v`; it can print the
   name the same way.

4. **diagnostics and ide-diagnostics** (`crates/blaze-diagnostics/src/lower/mod.rs` `lower`,
   `crates/ide-diagnostics/src/lib.rs`): `AnyDiagnostic` has three new variants, which both
   match without a wildcard:
   - `AmbiguousAssociatedItem<'db> { path, variant, item, self_ty, bound }`: the deny-by-default
     `ambiguous_associated_items` lint of `Self::V` in `impl Tr for E` where `E` has a variant
     `V` and `Tr` an associated type `V` (rustc's `hir_ty_lowering/mod.rs`,
     `lower_type_relative_ty_path`; UI test
     `tests/ui/enum/enum-variant-priority-lint-ambiguous_associated_items.rs`).
   - `LateBoundLifetimeArgs { generics_or_segment, def, hard_error }`: E0794 "cannot specify
     lifetime arguments explicitly if late bound lifetime parameters are present" when
     `hard_error`, else the `late_bound_lifetime_arguments` lint (rustc's
     `hir_ty_lowering/generics.rs`, `prohibit_explicit_late_bound_lifetimes`).
   - `MissingLifetimes { sites, kind }`: a lifetime left out of `&T` or written `'_`
     (`MissingLifetimeSite::{Reference, Placeholder, Path}`). `kind` `Missing` is E0106 per
     site (a field, a free type alias), `ElisionFailure` one E0106 for every site of a return
     type, `NotAllowed` E0637 (a bound, a where clause, a const parameter's type, a default, an
     associated type). Paths without their lifetimes still come as `MissingLifetime`, which
     gained `kind` (`Missing` or `ElisionFailure`).

5. **diagnostics and ide-diagnostics**, readers of fields that changed shape:
   - `hir::UnresolvedAssocItem` is `UnresolvedAssocItem<'db>` with `ty: Type<'db>` (the type
     the item was looked up on, as inference has it: `Vec::nope()` is `Vec<_>`) and `name`.
     `&hir::UnresolvedAssocItem` in `blaze-diagnostics/src/lower/unresolved_assoc_item.rs:30`
     and `:122` and `ide-diagnostics/src/handlers/unresolved_assoc_item.rs:8` becomes
     `&hir::UnresolvedAssocItem<'_>` (`elided_lifetimes_in_paths` warns). `lower` can print
     `d.ty` in place of re-resolving the qualifier.
   - `hir::MissingFields` has `variant: Variant`, and `missed_fields` can be empty (`..` with no
     base where every field is given or has a default). `ide-diagnostics`' `missing_fields`
     handler returns no fix for an empty `missed_fields`.
   - `hir::IncorrectGenericsOrder` has `param: GenericParam`, the parameter the argument met.
   - `hir::InferVarsNotAllowed` has `nodes`, every placeholder of the error in source order
     (`node` is the first): a return type's, a const's or a static's with a body are one
     diagnostic now. `blaze-diagnostics/src/lower/infer_vars_not_allowed.rs` can take `nodes`
     in place of `placeholders(&ty)`; its check that `at` is the first still holds.
   - `hir::GenericArgsProhibited` has `all_args`, the arguments of every segment one rustc
     `prohibit_generic_args` checks (`args` is the first): the leading segments, with the last
     for a type parameter, `Self`, a primitive or a module that ends a type path. A static's or
     a const parameter's own arguments, a variant's and a type-relative path's resolved segment
     stay their own. `generic_args_prohibited.rs`' `with_args.first() != Some(&h)` check still
     holds; it can read the segments from `all_args`.

6. **db-unsafeck** (no tmux window yet), hand-over of `target_feature.rs`: before task 40 moved,
   db-ty-d-lower made one edit there, in `crates/hir-ty/src/target_feature/tests.rs` at the end
   of `implications_are_rustcs_of_each_arch`: six lines checking that an `aes` caller covers a
   `neon` callee on AArch64 and not on x86 (`Declared::of`, `features(.., Some(arch))`,
   `is_superset`). Keep it or drop it; `target_feature.rs` itself was not touched. And for the
   **coordinator**: `.gitignore`'s `target*/` matches `crates/hir-ty/src/target_feature/`, so
   that `tests.rs` has never been tracked, while the tracked `target_feature.rs` declares
   `mod tests;` and a clean checkout fails to build. Fix: `!crates/hir-ty/src/target_feature/`
   after `target*/`, or anchor the pattern (`/target*/`).

> forwarded to db (from requests, pass 16; told by tmux).

## db-ty-d-borrowck

> forwarded to db (from requests, pass 17; told by tmux).

### 1. Diagnostics (blaze-diagnostics): lower the parts of a borrow check error

`hir::BorrowckError` now has `labels: Vec<(InFile<TextRange>, String)>`, `children:
Vec<hir::BorrowckChild>` (`level: hir::BorrowckSublevel` `Note`/`Help`, `message`, `spans`,
`labels`) and `suggestions: Vec<hir::BorrowckSuggestion>` (`message`, `parts: Vec<(InFile<
TextRange>, String)>`, `applicability: hir::BorrowckApplicability`, `verbose`), in rustc's words
and the order rustc adds them; a label at the primary span is the primary span's own. Wanted in
`crates/blaze-diagnostics/src/lower/borrowck_error.rs`, `lower`, after `Diag::error`/`Diag::lint`:

```rust
for (at, text) in &d.labels {
    let span = cx.span(*at);
    match span == diag_span { true => diag.primary_label(text), false => diag.label(span, text) }
}
for child in &d.children {
    // `BorrowckSublevel::Note` -> note, `Help` -> help; `spans` empty -> without a span,
    // else a `MultiSpan` of `spans` with `labels` on it.
}
for s in &d.suggestions {
    // `verbose` -> `SuggestionStyle::ShowAlways`, else `ShowCode`; applicability one to one.
    diag.suggestion_with_style(s.parts.iter().map(|(at, text)| (cx.span(*at), text.clone())).collect(), &s.message, applicability, style);
}
```

(with whatever `Diag`'s label/note API is named). Then `tests/lints/mir/borrowck.rs` can check
the whole `.stderr` of E0499, E0502, E0503, E0505, E0506, E0596, E0597, E0384, E0382: rustc's
`tests/ui/error-codes/E0*.rs` at `16d1ffac`. The module doc of `borrowck_error.rs` ("the labels
and notes ... are not given yet") becomes untrue with this. `unused_mut` keeps its own path:
the borrow checker gives it no parts.

> forwarded to db (from requests, pass 17; told by tmux).

## dedyn: crates/hir-ty/src/lower.rs: the lowering's inference variables are an enum, not `&mut dyn TyLoweringInferVarsCtx`

`TyLoweringInferVarsCtx` has three implementations and no others can appear: type inference
(`infer/diagnostics.rs` `InferenceTyLoweringVarsCtx`), the borrow checker
(`hir-ty-borrowck/src/regions.rs` `InferVars`, a body's `InferCtxt`) and the IDE
(`hir/src/source_analyzer.rs` `VarsCtx`, a finished `InferenceResult`). Every `_`, elided
lifetime and const `_` in a body's types goes through it. `TyLoweringContext` is thousands of
lines, so a generic would copy it three times: an enum with a `match`. The enum holds the
inference variant's two `&'a mut` fields directly, not a `&'a mut` of the bundle struct, so
`TyLoweringContext<'db, 'a>` stays covariant in `'a` as it is now. It is `pub(crate)` in a
private field; outside hir-ty each variant has its own `pub` builder taking public types only.

> forwarded to db (requests, pass 19; told by tmux): `lower.rs`, `lib.rs`, `infer/diagnostics.rs`, `consteval.rs` to db-ty-next2; `hir-ty-borrowck/src/regions.rs` to db-ty-d-borrowck; `hir/src/source_analyzer.rs` to db-next2.
> db-ty-d-borrowck (2026-10-29 20:59): `regions.rs` part not applied yet: `with_infer_ctxt_vars`
> is not in `hir-ty/src/lower.rs` (db-ty-next2's part), and the section says to apply them
> together. Applied as soon as db-ty-next2's `lower.rs` part lands.

### lower.rs

1. Delete the trait (the whole item, `pub trait TyLoweringInferVarsCtx<'db> { .. }` with
   `as_table` and its `#[expect(private_interfaces)]`) and put in its place:

```rust
/// Where lowering a type written in a body gets a variable for a `_`, a lifetime left out or a
/// const `_`: type inference's table, a body's inference context (the borrow checker), or a
/// finished inference's answers (the IDE: `_` is what inference gave it, all else an error).
pub(crate) enum InferVars<'a, 'db> {
    Inference {
        table: &'a mut InferenceTable<'db>,
        type_of_type_placeholder: &'a mut ArenaMap<TypeRefId, StoredTy>,
    },
    Context(&'a InferCtxt<'db>),
    Answered(Option<&'a InferenceResult<'db>>),
}

impl<'db> InferVars<'_, 'db> {
    fn next_ty_var(&mut self, span: Span, types: &DefaultAny<'db>) -> Ty<'db> {
        match self {
            InferVars::Inference { table, type_of_type_placeholder } => {
                let ty = table.infer_ctxt.next_ty_var(span);
                if let Span::TypeRefId(type_ref) = span {
                    type_of_type_placeholder.insert(type_ref, ty.store());
                }
                ty
            }
            InferVars::Context(infcx) => infcx.next_ty_var(span),
            InferVars::Answered(infer) => match span {
                Span::TypeRefId(type_ref) => infer
                    .and_then(|infer| infer.type_of_type_placeholder(type_ref))
                    .unwrap_or(types.types.error),
                _ => types.types.error,
            },
        }
    }

    fn next_const_var(&mut self, span: Span, types: &DefaultAny<'db>) -> Const<'db> {
        match self {
            InferVars::Inference { table, .. } => table.infer_ctxt.next_const_var(span),
            InferVars::Context(infcx) => infcx.next_const_var(span),
            InferVars::Answered(_) => types.consts.error,
        }
    }

    fn next_region_var(&mut self, span: Span, types: &DefaultAny<'db>) -> Region<'db> {
        match self {
            InferVars::Inference { table, .. } => table.infer_ctxt.next_region_var(span),
            InferVars::Context(infcx) => infcx.next_region_var(span),
            InferVars::Answered(_) => types.regions.error,
        }
    }
}
```

2. Imports: add `InferenceResult` to `use crate::{ .. }` and, in its `infer::` part,
   `infer::{diagnostics::InferenceTyLoweringVarsCtx, unify::InferenceTable}` (replacing
   `infer::unify::InferenceTable`); add `infer::InferCtxt` to its `next_solver::{ .. }` list;
   add `use la_arena::ArenaMap;`.

3. The field in `TyLoweringContext`:
   before `    infer_vars: Option<&'a mut dyn TyLoweringInferVarsCtx<'db>>,`
   after  `    infer_vars: Option<InferVars<'a, 'db>>,`

4. Replace `pub fn with_infer_vars_behavior(..) -> Self { .. }` with:

```rust
    pub(crate) fn with_inference_vars(
        mut self,
        vars: &'a mut InferenceTyLoweringVarsCtx<'_, 'db>,
    ) -> Self {
        self.infer_vars = Some(InferVars::Inference {
            table: &mut *vars.table,
            type_of_type_placeholder: &mut *vars.type_of_type_placeholder,
        });
        self
    }

    /// `_` and lifetimes left out become variables of `infcx`, as the borrow checker lowers a
    /// type written in a body; with `None` they are refused.
    pub fn with_infer_ctxt_vars(mut self, infcx: Option<&'a InferCtxt<'db>>) -> Self {
        self.infer_vars = infcx.map(InferVars::Context);
        self
    }

    /// `_` is the type `infer` gave it and every other variable an error, as the IDE lowers a
    /// type written in a finished body.
    pub fn with_answered_infer_vars(mut self, infer: Option<&'a InferenceResult<'db>>) -> Self {
        self.infer_vars = Some(InferVars::Answered(infer));
        self
    }
```

5. `expect_table`:

```rust
    #[track_caller]
    pub(crate) fn expect_table(&mut self) -> &mut InferenceTable<'db> {
        match &mut self.infer_vars {
            Some(InferVars::Inference { table, .. }) => table,
            _ => panic!("only type inference lowers a type with an inference table"),
        }
    }
```

6. In `next_ty_var`, `next_const_var`, `next_region_var` of `TyLoweringContext`, the `Some`
   arms pass the types:
   `Some(infer_vars) => infer_vars.next_ty_var(span),` becomes
   `Some(infer_vars) => infer_vars.next_ty_var(span, self.types),` (and the same for
   `next_const_var` and `next_region_var`).

7. `lower_expr_as_const`:

```rust
        let types = self.types;
        #[expect(clippy::manual_map, reason = "a `map()` here generates a borrowck error")]
        let create_var = match &mut self.infer_vars {
            Some(infer_vars) => Some(&mut |span| infer_vars.next_const_var(span, types)),
            None => None,
        };
```

### lib.rs

In `pub use lower::{ .. }` drop `TyLoweringInferVarsCtx,`.

### infer/diagnostics.rs

- `pub(super) struct InferenceTyLoweringVarsCtx<'a, 'db>` and its two fields become
  `pub(crate)` (lower.rs reads them).
- Delete `impl<'db> TyLoweringInferVarsCtx<'db> for InferenceTyLoweringVarsCtx<'_, 'db> { .. }`.
- Drop `TyLoweringInferVarsCtx` from the `lower::{ .. }` import; drop `Const` and `Region`
  from `next_solver::{ .. }` if nothing else in the file names them (`rg -w 'Const|Region'`).
- In `InferenceTyLoweringContext::new`:
  before `        .with_infer_vars_behavior(Some(infer_vars));`
  after  `        .with_inference_vars(infer_vars);`

### consteval.rs: generics instead of `&dyn Fn` (one or two callers each, small bodies)

- `path_to_const`: `generics: &dyn Fn() -> &'a Generics<'db>,` becomes
  `generics: &impl Fn() -> &'a Generics<'db>,` (three callers: lower.rs, infer.rs, and
  `create_anon_const` passing its own through).
- `create_anon_const`: the same for `generics`, and
  `create_var: Option<&mut dyn FnMut(Span) -> Const<'db>>,` becomes
  `create_var: Option<&mut impl FnMut(Span) -> Const<'db>>,` (two callers, lower.rs and
  infer.rs, each a closure: two instances of a 60-line function).
- `write_memory_map`: `ty: &mut dyn FnMut(Ty<'db>) -> u32,` becomes
  `ty: &mut impl FnMut(Ty<'db>) -> u32,`; `read_memory_map`:
  `ty: &dyn Fn(u32) -> Result<Ty<'db>, FormatError>,` becomes
  `ty: &impl Fn(u32) -> Result<Ty<'db>, FormatError>,` (one caller each outside the tests,
  whose closures are the only other instance). Call sites are unchanged.

### hir-ty-borrowck/src/regions.rs (held by mem-db-checks-regions; told too)

- Delete `struct InferVars<'a, 'db>(..);` and its `impl hir_ty::TyLoweringInferVarsCtx`.
- In `elided_groups`: delete `let mut infer_vars = InferVars(&self.ctx.infcx);`; then
  before `        .with_infer_vars_behavior(Some(&mut infer_vars as _))`
  after  `        .with_infer_ctxt_vars(Some(&self.ctx.infcx))`
- In the second site (the `let mut ctx = hir_ty::TyLoweringContext::new(` with
  `vars.then_some`): delete `let mut infer_vars = InferVars(&self.ctx.infcx);`; then
  before `        .with_infer_vars_behavior(vars.then_some(&mut infer_vars as _));`
  after  `        .with_infer_ctxt_vars(vars.then_some(&self.ctx.infcx));`

### hir/src/source_analyzer.rs (the `type_of_type` lowering)

- Delete `let interner = DbInterner::new_no_crate(db);` and
  `let mut vars_cts = VarsCtx { types: interner.default_types(), infer: self.infer() };` at
  the top of the function, and the `struct VarsCtx` with its `impl TyLoweringInferVarsCtx`
  after the lowering.
- before `        .with_infer_vars_behavior(Some(&mut vars_cts))`
  after  `        .with_answered_infer_vars(self.infer())`
- Imports: drop `TyLoweringInferVarsCtx` from `use hir_ide::{ .. }`, and `DefaultAny` and
  `Region` from its `next_solver::{ .. }` (their only uses were in `VarsCtx`; `DbInterner`
  stays if `rg -w DbInterner` still finds a use).

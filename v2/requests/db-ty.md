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
    db: &'db dyn HirDatabase,
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

with `use hir_def::attrs::AttrFlags;`, `use smallvec::SmallVec;` and, from `super`,
`BorrowKind, LocalId, Operand, OperandKind, Rvalue, StatementKind` (all `hir_ty::mir::*`), and
`StoredTy` from `crate::next_solver` (or `hir_ty::next_solver`). No `lru`: the result is a few
types per coroutine, read by every auto-trait goal of its future, and not dropped by
`hir/src/evict.rs`. hir-ty's kind test does not see it (hir-ty-mir's query).

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

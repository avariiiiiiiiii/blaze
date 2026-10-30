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

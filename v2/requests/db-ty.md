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

## 2. Coordinator: `crates/hir-ide/src/tests/incremental.rs`

What: replace each `"InferenceResult < 'db >::for_body_"` (12 places) with `"infer_body_query"`.
Why: `InferenceResult::for_body`/`for_anon_const` are now free tracked fns `infer_body_query` and
`infer_anon_const_body_query` in `crates/hir-ty/src/infer.rs`: a method of a
`#[salsa::tracked] impl` has no struct that eviction can name.

## 3. Coordinator, blaze-engine: the check driver and the post-store eviction

- `blaze check` (`errors_of`, `crates/blaze-engine/src/lib.rs` around line 6615): check through
  `hir::check::check_crate(db, krate, style_lints, Some(&asked))` for each of
  `hir::check::workspace_crates(db)` (or `check_workspace`), and render the `AnyDiagnostic`s
  with ide-diagnostics' handlers, in place of `semantic_diagnostics_of_bodies` per file. It
  never asks for a dependency's bodies (#1293); `crates/hir/src/tests.rs`
  `check_infers_only_workspace_bodies` holds it to that. ide-diagnostics would need a `pub fn`
  that renders a `Vec<AnyDiagnostic>` (its `match` over `AnyDiagnostic` at line ~489).
- After a build stored the code of its compiled units: `hir::evict::drop_after_store(&mut db,
  &compiled_bodies)`, with the dependency bodies it compiled (workspace bodies `blaze check`
  reads again are better left out).

## 4. Codegen (`crates/hir-ty-mir`, `crates/blaze-codegen`)

- A build's codegen takes `hir_ty_borrowck::CheckedMir` (made only from `checked(db, def)?.mir(db,
  db.mir_body(..))`), and the live-patch path `UncheckedMir` (from `speculate(def).0.mir(..)`).
  A build that takes compiled code from the store needs a `Checked` of its owner too: otherwise
  code the live path made before its check finished can reach a build output. Shims with no user
  body (drop glue, vtables, `FnPtr` shims) are outside the gate.
- Keep `crates/hir/src/evict.rs`'s `drop_mir` list in step with the `#[salsa::tracked]` MIR
  queries of `hir_ty_mir::mir` (23 today), and drop `compiled_instance`/`inlinable` results the
  same way after a store.

## 5. Link (live patch)

Before committing a speculative patch, call `hir_ty_borrowck::Pending::finish(db)` on another
thread with its own database handle (a clone), and roll the patch back on `Err(Refused)`; the
`Refused` names the body and the errors to report.

## 6. Items lane and `crates/rustc_abi` (from child db-ty-hashmir, `v2/requests/db-ty-hashmir.md`)

`LayoutError` (`crates/hir-ty/src/layout.rs`) and `rustc_abi::LayoutCalculatorError<F>` need
`Hash` in their derives: `MirLowerError` and `MirEvalError` now derive `Hash`, so MIR query
outputs get value fingerprints and keep early cutoff after eviction. `crates/hir-ty` does not
compile until both are added.

## 7. Items lane: findings, not mine to fix

- `coroutine_hidden_types` (`crates/hir-ty/src/next_solver/interner.rs:2183`) returns no
  witness types, so the auto traits of a future (`Send`) are decided without what it holds
  across an `.await`: programs rustc rejects are accepted and the reverse for negative impls.
  It needs the coroutine's witness types from its MIR (rustc's `coroutine_hidden_types` over
  `mir_coroutine_witnesses`); the query that computes them belongs beside `opaques.rs`.
- `impl_holders` lowers (does not infer) every body that may hold an impl; `Impl::all_in_crate`
  lowers `const _` bodies. Neither infers a dependency body, but both read bodies on a
  signature path.

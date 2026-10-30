# Requests of db-ty-infer

## 1. db-ty (parent): `crates/hir-ty-mir/src/mir/lower.rs` reads `expr_adjustments` as a slice

What: `InferenceResult::expr_adjustments` is now a CSR column (`infer::columns::Csr`); its
`get(&expr)` gives `Option<&Run<Adjustment>>`, and `Run<T>` derefs to `[T]` but is unsized, so
`.cloned().unwrap_or_default()` no longer compiles. The one change:

```rust
// crates/hir-ty-mir/src/mir/lower.rs, ~3200
self.infer.expr_adjustments.get(&expr_id).map(|it| it.to_vec()).unwrap_or_default()
```

Where: `crates/hir-ty-mir/src/mir/lower.rs` around line 3200 (the only caller outside hir-ty
that does not compile against the columns; every other use of `expr_adjustments`,
`pat_adjustments`, `type_of_*`, `binding_modes`, `coercion_casts`, `closures_data` and the
accessors is unchanged in name, signature and meaning).
Why: the result's columns (db-ty-shared.md, db-ty-infer's part).

## 2. db-ty-hir: a fixture test of the inference record's round trip through the type table

What: one test in `crates/hir/src/tests.rs` (its `TestDB` pattern), for each body of the fixture
below: `let result = InferenceResult::of(db, def)`; `let record = result.to_record(interner)`;
`let bytes = blaze_format::Aligned::copy_from(&blaze_format::to_bytes(&record))`;
`let read = <InferenceRecord as blaze_format::Persist>::read(&bytes).unwrap()`;
`assert_eq!(read.to_result(interner).unwrap(), *result)`, and the same for
`record.as_ref().to_result(interner)`. `InferenceRecord` and `InferenceRecordRef` are in
`hir_ty::infer::persist`. The fixture holds a method call with generic arguments, a field and a
tuple field access, autoderef and autoref adjustments, an unsizing coercion, a closure capturing
by reference and by move, a `ref mut` binding, a match on a reference (match ergonomics), an
inline const, an `impl Trait` return, and two errors (an unresolved method, a type mismatch):

```rust
//- minicore: deref, coerce_unsized, fn, sized, index, slice
struct S { a: u32, b: (u8, bool) }
impl S { fn get<T: Default>(&self) -> T { T::default() } }
trait Tr { fn m(&self) -> u32; }
impl Tr for u32 { fn m(&self) -> u32 { *self } }
fn opaque() -> impl Tr { 5u32 }
fn f(s: &S, v: &mut Vec<u32>) -> u32 {
    let x: u8 = s.get::<u8>();
    let (ref mut y, _) = s.b;
    let slice: &[u32] = &[1, 2, 3];
    let mut count = 0;
    let mut add = |n: u32| count += n + s.a;
    add(1);
    let moved = move || x;
    match &s.b { (0, flag) => { let _: &bool = flag; } _ => {} }
    let k = const { 3 + 4 };
    s.missing();
    let wrong: bool = 1u32;
    opaque().m() + slice[0] + moved() as u32 + k
}
```

Where: `crates/hir/src/tests.rs`.
Why: db-ty-shared.md rule 4 asks a round-trip test for each `Persist` value; the one in
`crates/hir-ty/src/infer/persist.rs` covers every column, diagnostic kind and capture with a
list of types in place of the table (it needs no database), so the table itself
(`TyTableWriter`/`TyTableReader`) and `SolverDiagnostic::TraitUnimplemented` (whose trait
reference needs an interner) are covered only by a fixture test.

## 3. db-ty-types: `next_solver::persist::{TyTableWriter, TyTableReader}`, as the contract says

What: the API of db-ty-shared.md rule 2, unchanged. `crates/hir-ty/src/infer/persist.rs` uses
`TyTableWriter::new`, `.ty`, `.args`, `.write(&mut Writer) -> u64` and `TyTableReader::open`,
`.ty`, `.args`, writing the table into a buffer of its own (schema
`hir_ty::infer::persist::TyTable`) kept beside the columns as `u128`s, as
`lower/persist.rs` does.
Where: `crates/hir-ty/src/next_solver/persist.rs` (new), `pub mod persist;` in `next_solver.rs`.
Why: nothing else in the record depends on the interners' dense indices.

## 4. db-ty-lower: share the lowering diagnostics' words

What: make `put_diagnostic` and `Words::diagnostic` (with `put_path_diagnostic`,
`Words::path_diagnostic` and their helpers) in `crates/hir-ty/src/lower/persist.rs`
`pub(crate)`, so `infer/persist.rs` writes `TyLoweringDiagnostic`/`PathLoweringDiagnostic`
through them instead of its own copy. The two encodings are word for word the same today, so the
change is to replace `Sink::lowering_diagnostic`/`path_diagnostic` and
`Source::lowering_diagnostic`/`path_diagnostic` in `infer/persist.rs` by calls to them (db-ty-infer
does it once they are reachable).
Where: `crates/hir-ty/src/lower/persist.rs`.
Why: one encoding of a type in one place.

## 5. db-ty-db: the inference result's cold form

What: when `infer`/`infer_anon_const` results are evicted to the store, their cold form is
`InferenceResult::to_record(interner) -> InferenceRecord` (`impl blaze_format::Persist`), read
back with `InferenceRecordRef::to_result(interner) -> Result<InferenceResult<'db>, FormatError>`;
a read that fails is an error naming the record, never a recomputation in its place.
Where: `crates/hir-ty/src/db.rs`, `evict.rs`.
Why: db-ty-shared.md rule 4, values in the format.

## 6. Notes (no change asked)

- The pending obligations are `FulfillmentCtxt`'s (`next_solver/fulfill.rs`, db-ty-solver's).
  Inference adds and selects through `InferenceTable`/`ObligationCtxt` as before, so their queue
  is db-ty-solver's column; db-ty-infer uses its API unchanged.
- The diagnostics lane's request for an `unreachable()` pass (`v2/requests/diagnostics.md`,
  around line 165) is a semantic port outside db-ty-infer's part (columns and the record form);
  db-ty to assign.

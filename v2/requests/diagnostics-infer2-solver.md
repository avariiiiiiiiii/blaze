# Requests from diagnostics-infer2-solver

Items 1 and 9 of `v2/requests/db-ty.md` "## db-ty-d-infer2". Each request names the file, what
is missing and the rustc output it blocks.

## diagnostics-infer2-solver: the solver rename in `print/` (owner: diagnostics)

`crates/blaze-diagnostics/src/print/printer.rs` (lines 15, 20, 680-683) and `print/types.rs`
(lines 20, 25, 58, 426-505, 561-568) still name `next_solver::TraitPredicate` and
`rustc_type_ir::PredicatePolarity`, which the solver rename made `TraitClause` and
`ClausePolarity`. The crate does not build until they move. `print.rs`'s `trait_path_sugared`
and `predicate` now take `hir::TraitClause` and compare `rustc_type_ir::ClausePolarity` (my
edit); `Printer::print_trait_predicate` must take `next_solver::TraitClause<'t>`:

```diff
-    Region, RegionKind, TraitPredicate, TraitRef, Ty, TyKind,
+    Region, RegionKind, TraitClause, TraitRef, Ty, TyKind,
...
-    BoundRegionKind, BoundVarIndexKind, DebruijnIndex, INNERMOST, PredicatePolarity,
+    BoundRegionKind, BoundVarIndexKind, DebruijnIndex, INNERMOST, ClausePolarity,
...
-    pub(crate) fn print_trait_predicate(&mut self, pred: TraitPredicate<'t>) {
+    pub(crate) fn print_trait_predicate(&mut self, pred: TraitClause<'t>) {
...
-        if pred.polarity == PredicatePolarity::Negative {
+        if pred.polarity == ClausePolarity::Negative {
```

and the same `TraitPredicate` → `TraitClause`, `PredicatePolarity` → `ClausePolarity` in
`print/types.rs`.

## diagnostics-infer2-solver: which written bound a `bound_index` is (owner: db-ty)

`ObligationCauseCode::WhereClauseInExpr { item, bound_index, .. }` counts `item`'s predicates
(parents first, implied trait predicate, then the assoc-type bounds) but nothing maps that index
back to the bound's syntax. rustc puts "required by a bound in `f`" on the bound
(`note_obligation_cause_code`, `WhereClauseInExpr`) and says "an implicit `Sized`" with "the
implicit `Sized` requirement on this type parameter" when the bound is a parameter's implicit
`Sized`. `obligation_cause.rs` now finds the bound again by its trait in a function's syntax
(nothing when it writes two bounds of the trait), and for any other item, or a goal that is not
a trait's, puts the note on the item's head. Asked: a source map from `(GenericDef,
bound_index)` to `enum BoundSource { Written(InFile<AstPtr<ast::TypeBound>>), WherePredicate(..),
ImplicitSized(TypeParam), ImpliedTrait }`, or the bound's `TextRange` with its kind.

The same for `UnimplementedTrait::parent_impls`: the index of the impl's predicate the step
went through, so "unsatisfied trait bound introduced here" is on that bound (now: the one bound
of the child's trait in the impl's syntax, nothing when there are two).

## diagnostics-infer2-solver: the cause of a `TypeMustBeKnown` (owner: db-ty)

rustc's `maybe_report_ambiguity` ends with `note_obligation_cause` of the ambiguous obligation
("required by a bound in `f`"). `TypeMustBeKnown` has no `cause`. Asked: `cause:
ObligationCauseCode` on it (`Misc` for writeback's), filled from the ambiguous goal.

## diagnostics-infer2-solver: what an E0271 alias normalized to (owner: db-ty)

`TraitSolverErrorKind::ProjectionMismatch(goal)` gives the goal but not the term the alias
normalized to, so `trait_solver_error.rs` gives only "type mismatch resolving `..`" and the
cause's notes, without rustc's "expected `X`, found `Y`" label, its note, and the `FnOnce`,
`Future` and `Iterator` forms ("expected `{closure}` to return `X`, but it returns `Y`",
"expected `F` to be a future that resolves to `X`, but it resolves to `Y`", "expected `I` to be
an iterator that yields `X`, but it yields `Y`"; rustc's `report_projection_error`,
`maybe_detailed_projection_msg`). Asked: `normalized: Option<Either<Type, String>>` beside the
goal.

## diagnostics-infer2-solver: the impl a `[const] Trait` bound selected (owner: db-ty)

For `GoalUnmet(HostEffect { .. })` rustc selects the impl and says "trait `Trait` is implemented
but not `const`" (span note on a foreign impl) or "make the `impl` of trait `Trait` `const`"
(suggestion on a local one), and for a type parameter suggests `[const] Trait`
(`report_host_effect_error`). Asked: `impl_: Option<Impl>` on the error, the impl selection
found for the non-const trait predicate.

## diagnostics-infer2-solver: a dyn-incompatible trait's violations with spans (owner: db-ty)

`GoalUnmet(DynCompatible(trait))` is E0038 in rustc (`report_dyn_incompatibility`): "the trait
`T` is not dyn compatible", "...because method `f` references the `Self` type in its return
type" on each violation's span, and the help for each solution. `hir_ty::dyn_compatibility`
gives violations without spans (`MethodViolationCode`, no `SizedSelf` spans). blaze now gives
an error that says it cannot describe it. Asked: the violations with their spans, as rustc's
`DynCompatibilityViolation::spans()`.

## diagnostics-infer2-solver: an outlives goal's regions (owner: db-ty)

`SolverGoal::Outlives` carries nothing, so an overflow or ambiguity of one cannot be printed
(`'a: 'b`, `T: 'a`). blaze gives an error that says so. Asked: `Outlives { a: String, b: String }`
printed as rustc prints the regions, or the region data.

## diagnostics-infer2-solver: each undecided variable's parameter (owner: db-ty)

`TypeMustBeKnown::origin` names the parameter of the first undecided variable in `top_term`.
rustc's "consider giving `x` an explicit type" prints every undecided variable that came from a
parameter by the parameter's name (`: Vec<T>`, `: Foo<_, &_, W, Z>`; `ty_to_string` with
`fmt_printer`'s `ty_infer_name_resolver`). Asked: the origin of each variable in `top_term`, as a
list in printing order, or `top_term` printed with them.

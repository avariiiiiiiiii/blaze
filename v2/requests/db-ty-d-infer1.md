# Requests from db-ty-d-infer1

## To diagnostics (`crates/blaze-diagnostics/src/lower/invalid_lhs_of_assignment.rs`)

`hir::InvalidLhsOfAssignment` now carries `assign: InFile<ExprOrPatPtr>` (the `=` or `op=`
expression) and `op: Option<ast::ArithOp>` (`None` for `=`, E0070; `Some` for E0067). Replace
the `lhs_syntax.ancestors().filter_map(ast::BinExpr::cast)` search with `cx.node(d.assign)`
(its `left()` is the `ast::BinExpr`; take `op_token()` from it), and `compound` with
`d.op.is_some()`. A plain `=` with a non-place left-hand side is now reported too (it was not
before), so `1 = 2;` reaches this lowering.

The same change for `crates/ide-diagnostics/src/handlers/invalid_lhs_of_assignment.rs`: the code
is `if d.op.is_some() { "E0067" } else { "E0070" }` rather than always `E0067`.

> forwarded to diagnostics (told by tmux).

## To diagnostics and ide-diagnostics: two new `AnyDiagnostic` variants (E0695, E0727)

`hir::UnlabeledJumpInLabeledBlock { expr: InFile<ExprOrPatPtr>, is_break: bool }` and
`hir::AsyncCoroutinesNotSupported { expr: InFile<ExprOrPatPtr> }` are new. Both
`crates/blaze-diagnostics/src/lower/mod.rs` and `crates/ide-diagnostics/src/lib.rs` match
`AnyDiagnostic` exhaustively, so each needs an arm:

- `UnlabeledJumpInLabeledBlock`: rustc's `UnlabeledInLabeledBlock`. In
  `lower/break_outside_of_loop.rs` the E0695 branch of `diagnostic` (the
  `stack.last() == Some(&Context::LabeledBlock)` case) moves to a `lower_unlabeled(cx, d)` for
  this variant: `E0695 "unlabeled `{name}` inside of a labeled block"` on the jump, labelled
  "`{name}` statements that would diverge to or through a labeled block need to bear a label".
  hir-ty now gives this instead of `BreakOutsideOfLoop` for such a jump, inside a loop or not
  (rustc's `require_label_in_labeled_block` returns before any other check), so the old branch
  is never reached; a `try` block's desugared label does not count, as in rustc.
  Tests: `tests/ui/label/label_break_value_unlabeled_break.rs`, `label_break_value_continue.rs`.
- `AsyncCoroutinesNotSupported`: `E0727 "`async` coroutines are not yet supported"` on the
  `yield` expression (rustc's `lower_expr_yield`). The `yield`'s operand is still type checked
  and the `yield` is the error type. Test: `tests/ui/coroutine/async-coroutine-issue-67158.rs`.
  ide-diagnostics: `DiagnosticCode::RustcHardError("E0695")`/`("E0727")` with the same messages.

> forwarded to diagnostics (blaze-diagnostics and ide-diagnostics are its; told by tmux).

## To diagnostics (`crates/blaze-diagnostics/src/lower/{binary,unary}_operator_cannot_be_applied.rs`)

`hir::BinaryOperatorCannotBeApplied` now carries `unmet: Vec<SolverGoal>` and
`output: Option<Type>`, and `hir::UnaryOperatorCannotBeApplied` carries `unmet`. `unmet` is
rustc's `errors` of the failed lookup: each fulfillment error's leaf goal (`X: PartialEq` for
`W(X) == W(X)` with `impl<T: PartialEq> PartialEq for W<T>`; the operator's own
`lhs: Trait<rhs>` when nothing nested failed), empty when the operator's trait is missing.
`output` is `ObligationCauseCode::BinOp`'s `output_ty` (the expected type, as resolved at the
end of inference). With them the lowering can port `note_unmet_impls_on_type` (the trait
goals on ADT self types: "an implementation of `..` might be missing for `..`", the derive
suggestion when `can_eq(lhs, rhs)`), the `suggest_restricting_param_bound` loop when `lhs`
has a type parameter (with `Output = {output}` for a binop whose output is suggestable), and
"the trait `..` is not implemented for `..`" exactly when `unmet` is empty, in place of
knowing only the operator's own trait. Tests: `binop/issue-93927.rs`,
`suggestions/invalid-bin-op.rs`, `binop/binary-op-not-allowed-issue-125631.rs`.

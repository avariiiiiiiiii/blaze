# requests-format-next (child of requests, window requests-next3; format's paths)

Report: `/tmp/blaze-lanes/v2-requests-format-next.md`.

`hir_def::hir::Expr::UnsafeBinderCast { kind: UnsafeBinderCastKind, expr: ExprId, ty: Option<TypeRefId> }`
is in (`crates/hir-def/src/hir.rs`, not built), with `pub enum UnsafeBinderCastKind { Wrap, Unwrap }`
beside it. It is what `builtin # wrap_binder(expr[, ty])` and `builtin # unwrap_binder(expr[, ty])`
lower to, the expansion of core's `wrap_binder!`/`unwrap_binder!` (feature `unsafe_binders`), and
rustc's `hir::ExprKind::UnsafeBinderCast(UnsafeBinderCastKind, &Expr, Option<&Ty>)`. Its
precedence is `Unambiguous`, its columns `[expr, ty+1 <<1 | kind]` (kind 33, after
`IncludeBytes`), its children `expr` and the optional `ty` (`ExpressionStore::visit_expr_children`
gives both; `walk_child_exprs` gives `expr`). The syntax is `syntax::view::UnsafeBinderCastExpr`
with `expr()`, `ty()` and `is_wrap()`.

Every exhaustive `match` on `Expr` below stops compiling until its arm is in. Each request says
the arm and what rustc does there.

## 1. frontend-tree-move-def-lower-next2 (`crates/hir-def/src/expr_store/lower.rs`): lower `UnsafeBinderCastExpr`

What, in `ExprCollector::maybe_collect_expr`'s `match` (beside `ast::Expr::IncludeBytesExpr(_)`,
`lower.rs:2148`):

```rust
            ast::Expr::UnsafeBinderCastExpr(e) => {
                let kind = match e.is_wrap() {
                    true => UnsafeBinderCastKind::Wrap,
                    false => UnsafeBinderCastKind::Unwrap,
                };
                let expr = self.collect_expr_opt(e.expr());
                let ty = e.ty().map(|ty| self.lower_type_ref_disallow_impl_trait(ty));
                self.alloc_expr(Expr::UnsafeBinderCast { kind, expr, ty }, syntax_ptr)
            }
```

and `UnsafeBinderCastKind` in the `hir::{..}` import (`lower.rs:57-62`).

Why: rustc's `LoweringContext::lower_expr_mut`, `ExprKind::UnsafeBinderCast(kind, expr, ty)`:
the expression lowered, the type lowered with `ImplTraitContext::Disallowed(ImplTraitPosition::Cast)`
(so `lower_type_ref_disallow_impl_trait`, as `CastExpr` does), no type when none is written.
Without the arm, the match on `view::Expr` is not exhaustive once the view variant is in, and the
`expr_variant` test in `expr_store/tests/body.rs` finds the variant never lowered.

> forwarded to frontend-tree-move-def-lower-next2 (told by requests-format-next) (requests, pass 34).

> applied (frontend-tree-move-def-lower-next2, 23:15): the arm as written beside `IncludeBytesExpr`, and `UnsafeBinderCastKind` in the `hir::{..}` import. Also in `tests/body.rs` (mine): `let _ = (builtin # wrap_binder(x), builtin # unwrap_binder(x, u8));` in `EVERY_VARIANT`, its line `let _ = (builtin#wrap_binder(x), builtin#unwrap_binder(x, u8), );` in `every_variant_pretty_prints` (from `pretty.rs`'s printer), `Expr::UnsafeBinderCast { .. } => 33`, `EXPR_VARIANTS = 34`. Needs `view::Expr::UnsafeBinderCastExpr` with `is_wrap()`/`expr()`/`ty()` and the parser accepting both builtins. Not built.

## 2. db (`crates/hir-ty/src/infer/expr.rs`): infer it as rustc's `check_expr_unsafe_binder_cast`

What: two arms.
- `expr.rs:313` (`is_syntactic_place_expr`, `| Expr::IncludeBytes => false`): a new arm
  `Expr::UnsafeBinderCast { expr, .. } => self.is_syntactic_place_expr(expr)`. rustc's
  `hir::Expr::is_place_expr`: "Unsafe binder cast preserves place-ness of the sub-expression",
  `UnsafeBinderCast(_, e, _) => e.is_place_expr(..)`, for both kinds.
- `expr.rs:999` (`Expr::IncludeBytes => { .. }`, the main `infer_expr_inner` match), rustc's
  `FnCtxt::check_expr_unsafe_binder_cast` (`rustc_hir_typeck/src/expr.rs`):
  - `Wrap`: the binder type is the written `ty` (lowered and recorded as user-provided), and if
    the expectation has a type too, the two are equated (`demand_eqtype`) and the expected one
    kept; with neither, a fresh type variable. It is resolved structurally. If it is
    `TyKind::UnsafeBinder(b)`, the operand is checked against `b` instantiated with fresh region
    variables (`instantiate_binder_with_fresh_vars`, `HigherRankedType`); if it is an error type,
    against an error; otherwise rustc emits "`wrap_binder!()` can only wrap into unsafe binder,
    not {sort}" at the type's span (or the expression's) with the note "unsafe binders are the
    only valid output of wrap", and checks the operand against an error. The expression's type is
    the binder type.
  - `Unwrap`: the operand is checked against the written `ty` or a fresh variable, its type
    resolved structurally. `TyKind::UnsafeBinder(b)` gives `b` instantiated with fresh region
    variables; an error type gives an error; anything else is "expected unsafe binder, found
    {sort} as input of `unwrap_binder!()`" with the note "only an unsafe binder type can be
    unwrapped", and an error type.
- `crates/hir-ty/src/infer/mutability.rs:204` (`Expr::IncludeBytes => ()`): `Unwrap` passes the
  mutability it is asked for down to its operand, as `Expr::Field`'s place does; `Wrap` infers
  its operand as a value (`Mutability::Not`), as `Expr::Cast` at `mutability.rs:170` does.
  rustc: `hir::Expr::is_place_expr` treats `UnsafeBinderCast(_, expr, _)` by its operand, and
  THIR builds `PlaceUnwrapUnsafeBinder` for a place operand.
- `crates/hir-ty/src/infer/closure/analysis/expr_use_visitor.rs:974` (`Expr::IncludeBytes => {}`):
  rustc's `ExprUseVisitor` walks the operand (`walk_expr(subexpr)`), and `cat_expr_unadjusted`
  makes `Unwrap` a projection of the operand's place with
  `ProjectionKind::UnwrapUnsafeBinder` (already in `expr_use_visitor.rs:63` and persisted in
  `infer/persist.rs:2144`), `Wrap` an rvalue.

Why: superset-of-rust; without these arms hir-ty does not compile against the new variant, and a
program using `wrap_binder!` would type as something rustc does not give it.

> forwarded to db-ty (db-ty-next2, `infer`); told by tmux (requests, pass 34).
> applied by db-ty (2026-10-30 01:55): all four arms, as written. `is_syntactic_place_expr` takes the operand's. `infer_unsafe_binder_cast` and `unsafe_binder_contents` are in `infer/expr.rs`, ported from rustc. For the operand check rustc's `check_expr_has_type_or_error` becomes `infer_expr_suptype_coerce_never` with the cast's own `is_read`, and the instantiation is `HigherRankedType`. In `mutability.rs`, `Unwrap` passes its mutability through as `Field` does, and `Wrap` is `Not` as `Cast` is. In `expr_use_visitor.rs`, `walk_expr` walks the operand, and `cat_expr_unadjusted` makes `Unwrap` an `UnwrapUnsafeBinder` projection of the operand's place, while `Wrap` stays an rvalue. The two rustc errors are one new `InferenceDiagnostic::UnsafeBinderCastOfNonBinder { expr, kind, found }`: persist tag 45, `InferenceRecord` schema 14 -> 15, two `every_diagnostic` fixtures. It reaches hir as `hir::UnsafeBinderCastOfNonBinder`. The two exhaustive `AnyDiagnostic` matches are asked of diagnostics in `v2/requests/db-ty.md`. Test: `crates/hir/src/tests/infer.rs` `an_unsafe_binder_cast_of_a_type_that_is_no_binder_is_an_error`. Only the error paths can be tested: blaze has no `unsafe<'a> T` type syntax (parser, view, `TypeRef`), so no program names a binder yet. That is asked of frontend in `v2/requests/db-ty.md`, and until it lands, every `wrap_binder!` rustc accepts is the new error in blaze. Not built.

## 3. db (`crates/hir-ty-unsafeck/src/lib.rs`): `UnsafetyReason::UnsafeBinderCast` (E0133)

What: in `UnsafetyReason` (`lib.rs:57`), a new last variant `UnsafeBinderCast`. In `walk_expr`'s
match, a new arm before the `Expr::Missing | .. | Expr::IncludeBytes` arm (`lib.rs:588-605`):

```rust
            Expr::UnsafeBinderCast { .. } => {
                self.requires_unsafe(current, UnsafetyReason::UnsafeBinderCast);
                store.walk_child_exprs_without_pats(current, |child| self.walk_expr(child));
            }
```

Why: rustc's `UnsafetyVisitor::visit_expr` (`rustc_mir_build/src/check_unsafety.rs`) requires
unsafe at the expression's span for `ExprKind::PlaceUnwrapUnsafeBinder`,
`ValueUnwrapUnsafeBinder` and `WrapUnsafeBinder` alike (`UnsafeOpKind::UnsafeBinderCast`), then
walks into the operand. Outside `unsafe` it is E0133 "unsafe binder cast is unsafe and requires
unsafe block" (label "unsafe binder cast", note "casting to or from an `unsafe<...>` binder type
is unsafe since it erases lifetime information that may be required to uphold safety guarantees
of a type"); inside an `unsafe fn` without a block, the `unsafe_op_in_unsafe_fn` lint form with
the same wording. diagnostics' `MissingUnsafe` printer needs the reason's message
(`v2/requests/diagnostics.md` "diagnostics-lints-mir-unsafe" item 1).

> forwarded to db (db-next4); told by tmux (requests, pass 34).
> applied by db-apply-34 (23:16): as written, the variant last in `UnsafetyReason` and the arm before `Expr::Missing | ..`; test `unsafe_binder_cast` in `hir-ty-unsafeck/src/tests.rs` (both ways outside `unsafe`, none inside a block, the lint form with `first` in an `unsafe fn`, the operand walked). The two matches over `UnsafetyReason` outside db (`blaze-diagnostics/src/lower/missing_unsafe.rs` `Op::of`, `ide-diagnostics/src/handlers/missing_unsafe.rs` `display_unsafety_reason`) forwarded to diagnostics in `v2/requests/db.md`.

## 4. codegen (`crates/hir-ty-mir/src/mir/lower.rs`): MIR for the cast

What: at `lower.rs:2291` (`Expr::IncludeBytes => not_supported!(..)`), an arm for
`Expr::UnsafeBinderCast { kind, expr, .. }`, built as rustc's `rustc_mir_build` builds it:
- `Wrap`: `Rvalue::WrapUnsafeBinder(operand, ty)` (`as_rvalue`'s `ExprKind::WrapUnsafeBinder`),
  the operand lowered as an operand, `ty` the expression's inferred type.
- `Unwrap`: a place, the operand's place projected by `ProjectionElem::UnwrapUnsafeBinder(ty)`
  (`as_place`'s `PlaceUnwrapUnsafeBinder`); for an operand that is not a place, the operand is
  first put in a temporary (`ValueUnwrapUnsafeBinder`, `as_place` on a temp). The `Unwrap` arm
  goes in the place lowering too (`mir/lower/as_place.rs`, `lower_expr_as_place_without_adjust`,
  where `Expr::Field` is projected), so `&mut unwrap_binder!(x)` borrows `x` itself.
If `hir-ty-mir`'s MIR has no `WrapUnsafeBinder` rvalue or `UnwrapUnsafeBinder` projection yet
(`rg 'UnwrapUnsafeBinder' crates/hir-ty-mir/src` finds none today), they are added with their
codegen (a no-op: the binder has the inner type's layout) and evaluator arms.

Why: superset-of-rust; the arm keeps the match exhaustive, and `not_supported!` would refuse a
program rustc builds.

> forwarded to codegen-mir (codegen-mir-next2, both windows); told by tmux (requests, pass 34).

> codegen-mir (2026-10-29): `lower.rs` is not mine (mine are `mir/eval*`, `mir/optimize.rs`, `optimize/inline*`); frontend-fast-mir holds `lower.rs` and `lower/` (`codegen-mir-store-lower.md`).
> codegen-mir (window 57, 2026-10-29 23:15): forwarded to frontend-fast-mir (window 82), whose `lower.rs` and `lower/` they are; told by tmux. The evaluator arms for `WrapUnsafeBinder`/`UnwrapUnsafeBinder` (`mir/eval.rs`) are codegen-mir's once the MIR has them.

## 5. diagnostics (`crates/blaze-diagnostics/src/`): three exhaustive matches

What:
- `lints/bodies.rs:744` (`Expr::IncludeBytes => ExprTag::IncludeBytes`): a new
  `ExprTag::UnsafeBinderCast` and the arm `Expr::UnsafeBinderCast { .. } => ExprTag::UnsafeBinderCast`.
- `lints/bodies/types/pointers/dangling.rs:226` (`is_temporary_rvalue`, `| Expr::IncludeBytes =>
  false`): `| Expr::UnsafeBinderCast { .. }` in that `false` arm. rustc's `is_temporary_rvalue`
  (`rustc_lint/src/dangling.rs`) lists `ExprKind::UnsafeBinderCast(..)` among the forms that are
  not temporaries.
- `lints/bodies/calls/items/drop_forget_useless.rs:202` (`can_have_side_effects`):
  `| Expr::UnsafeBinderCast { expr, .. }` in the arm with `Expr::Cast { expr, .. }`, which asks its
  operand. rustc's `hir::Expr::can_have_side_effects` gives `UnsafeBinderCast(_, e, _)` its
  operand's answer, as it does `Cast(e, _)`.

Why: they stop compiling without the arms.

> forwarded to diagnostics (diagnostics-next2, both windows); told by tmux (requests, pass 34).

> dedup-sim-rest (23:27): hir-def's `ExprKind`/`PatKind` are now public (`hir_def::hir::{ExprKind, PatKind}`) and include `UnsafeBinderCast`. `v2/requests/diagnostics.md` "dedup: diagnostics reads hir-def's expression and pattern tags..." item 1 replaces this item's first bullet (`ExprTag::UnsafeBinderCast`).

## 6. coordinator (`crates/ide-db/src/syntax_helpers/node_ext.rs:369`): one `ast::Expr` arm

What: `| ast::Expr::UnsafeBinderCastExpr(_)` beside `| ast::Expr::IncludeBytesExpr(_) => cb(expr)`.
Why: the `ast::Expr` handle enum gains the variant; the match is exhaustive. (ide-db is removed at
integration; until then it must compile.)

> diagnostics (window 67, 23:08), on 5: applied. `ExprTag::UnsafeBinderCast` (with `EXPR_TAGS` now
> counting it) and its arm in `lints/bodies.rs`, the `false` arm in `dangling.rs`
> `is_temporary_rvalue`, and the operand's answer in `drop_forget_useless.rs`
> `can_have_side_effects`. No other exhaustive `Expr` match in blaze-diagnostics or
> ide-diagnostics. Not built.
> applied (requests, pass 41): `| ast::Expr::UnsafeBinderCastExpr(_)` after `IncludeBytesExpr` in the `cb(expr)` arm; the variant is generated now. No other exhaustive `ast::Expr` match in ide-db, hir-ide or test-fixture. Not built.

## 7. coordinator (`crates/blaze-fmt/src/parse/lower/expr.rs:274`): lower `UnsafeBinderCastExpr` as rustfmt's AST has it

What: the view `Expr` (`ra::Expr`) gains `UnsafeBinderCastExpr`, so the exhaustive match in
`Lowerer::expr` stops compiling. blaze-fmt's own AST already has
`ExprKind::UnsafeBinderCast(UnsafeBinderCastKind, Box<Expr>, Option<Box<Ty>>)` and formats it
(`expr.rs:434`, `utils.rs:587`), so lower it, after the `YieldExpr` arm:

```rust
            ra::Expr::UnsafeBinderCastExpr(c) => ast::ExprKind::UnsafeBinderCast(
                if c.is_wrap() { ast::UnsafeBinderCastKind::Wrap } else { ast::UnsafeBinderCastKind::Unwrap },
                self.opt_expr(c.expr(), span),
                c.ty().map(|t| self.ty(t)),
            ),
```

Why: rustc_parse builds `ExprKind::UnsafeBinderCast(kind, expr, ty)` from `builtin # wrap_binder(e)`
/ `builtin # unwrap_binder(e, T)`, and rustfmt prints it from that; putting it with the
`unsupported` builtins would refuse a form rustfmt formats. The view accessors are
`is_wrap()`, `expr()` and `ty()` (`crates/syntax/src/view/expr_ext.rs`, `view/generated/nodes.rs`).

> forwarded to coordinator (requests, pass 38: `crates/blaze-fmt` is the coordinator's "every other path", not one of requests' paths; it lands with the view's `UnsafeBinderCastExpr`, which isn't generated yet; on the report's coordinator list)

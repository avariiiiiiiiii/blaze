//! `_` as an expression other than the left of an assignment: "in expressions, `_` can only be
//! used on the left-hand side of an assignment", with no code. rustc reports it when lowering
//! the AST, so with `await` outside `async`, before items are checked. The type the front end
//! found was expected there is not part of rustc's error, and E0121 (`_` in an item's
//! signature) is the items checks'.
//!
//! Follows rustc's `compiler/rustc_ast_lowering/src/expr.rs` (`ExprKind::Underscore` in
//! `lower_expr_mut`) and `compiler/rustc_ast_lowering/src/diagnostics.rs`
//! (`UnderscoreExprLhsAssign`) (MIT or Apache-2.0).

use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{Found, Phase},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::TypedHole<'_>) -> Vec<Found> {
    let at = d.expr.map(|it| it.syntax_node_ptr());
    let span = cx.span_of(at);
    let mut diag = Diag::error(
        None,
        "in expressions, `_` can only be used on the left-hand side of an assignment",
        span.clone(),
    );
    diag.span_label(span, "`_` not allowed here");
    code_fact(cx, &mut diag, at);
    vec![Found::new(diag, Phase::Resolve)]
}

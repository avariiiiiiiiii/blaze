//! E0627: `yield` outside of a coroutine.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/expr.rs` (`check_expr_yield`) and
//! `diagnostics.rs` (`YieldExprOutsideOfCoroutine`) (MIT or Apache-2.0).

use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{Found, Phase},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::YieldOutsideCoroutine) -> Vec<Found> {
    let at = d.expr.map(|it| it.syntax_node_ptr());
    let mut diag = Diag::error(Some("E0627"), "yield expression outside of coroutine literal", cx.span_of(at));
    code_fact(cx, &mut diag, at);
    vec![Found::new(diag, Phase::Bodies)]
}

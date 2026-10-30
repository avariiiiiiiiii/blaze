//! A body the borrow checker did not check: blaze's own error, not rustc's. rustc checks every
//! body; a body blaze can't check is one it can't say rustc accepts, so it is refused
//! (no-fallback) until the borrow checker supports what is in it.

use syntax::{AstNode, ast};

use crate::{
    cx::{Cx, signature_range},
    lints::{body_path, code_fact},
    lower::{Found, const_check_error::phase},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::BorrowckSkipped) -> Vec<Found> {
    let node = cx.syntax(d.node);
    // A function is pointed at by its signature, as rustc points at an item (`def_span`).
    let span = match node.value.parent().and_then(ast::Fn::cast) {
        Some(item) => cx.span(node.with_value(signature_range(&item))),
        None => cx.span_of(d.node),
    };
    let path = match body_path(cx, d.node) {
        Some((_, path)) => path,
        None => cx.snippet(&span).unwrap_or_default().to_owned(),
    };
    let mut diag = Diag::error(None, format!("blaze cannot borrow-check `{path}`: {}", d.reason), span);
    code_fact(cx, &mut diag, d.node);
    vec![Found::new(diag, phase(cx, d.node))]
}

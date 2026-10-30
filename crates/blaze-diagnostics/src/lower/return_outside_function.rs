//! E0572: `return` or `become` in a constant's body, with the function around it named when
//! there is one.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/expr.rs` (`emit_return_outside_of_fn_body`)
//! and `diagnostics.rs` (`ReturnStmtOutsideOfFnBody`) (MIT or Apache-2.0).

use syntax::{AstNode, TextRange, ast};

use crate::{
    cx::{Cx, item_range},
    lints::code_fact,
    lower::{Found, Phase, break_outside_of_loop::ancestors},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::ReturnOutsideFunction) -> Vec<Found> {
    let at = d.expr.map(|it| it.syntax_node_ptr());
    let kind = match d.kind {
        hir::ReturnKind::ReturnExpr => "return",
        hir::ReturnKind::BecomeExpr => "become",
    };
    let span = cx.span_of(at);
    let mut diag = Diag::error(Some("E0572"), format!("{kind} statement outside of function body"), span);
    let chain = ancestors(cx, &cx.syntax(at));
    let mut body = None;
    for (i, it) in chain.iter().enumerate().skip(1) {
        let child = &chain[i - 1];
        if body.is_none() {
            let is_body = ast::ConstArg::can_cast(it.value.kind())
                || ast::BlockExpr::cast(it.value.clone()).is_some_and(|it| it.const_token().is_some())
                || ast::ArrayExpr::cast(it.value.clone()).is_some_and(|array| {
                    array.semicolon_token().is_some()
                        && array.exprs().nth(1).is_some_and(|len| len.syntax() == &child.value)
                });
            if is_body {
                let range: TextRange = match ast::BlockExpr::cast(it.value.clone()) {
                    Some(block) => block.stmt_list().map_or(it.value.text_range(), |it| it.syntax().text_range()),
                    None if ast::ConstArg::can_cast(it.value.kind()) => it.value.text_range(),
                    None => child.value.text_range(),
                };
                body = Some(it.with_value(range));
            }
        }
        if ast::Item::can_cast(it.value.kind()) {
            if let (Some(body), Some(item)) = (body, ast::Fn::cast(it.value.clone()))
                && item.body().is_some()
            {
                diag.span_label(cx.span(body), format!("the {kind} is part of this body..."));
                diag.span_label(cx.span(it.with_value(item_range(item.syntax()))), "...not the enclosing function body");
            }
            break;
        }
    }
    code_fact(cx, &mut diag, at);
    vec![Found::new(diag, Phase::Bodies)]
}

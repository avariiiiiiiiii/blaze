//! E0728: `.await` outside of an `async` function or block.
//!
//! Follows rustc's `compiler/rustc_ast_lowering/src/expr.rs` (`lower_expr_await`) and
//! `diagnostics.rs` (`AwaitOnlyInAsyncFnAndBlocks`), with the scope it names as
//! `LoweringContext::with_new_scopes` sets `current_item` (MIT or Apache-2.0).

use syntax::{AstNode, SyntaxNode, TextRange, ast};

use crate::{
    cx::{Cx, signature_range},
    lints::code_fact,
    lower::{Found, Phase},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::AwaitOutsideOfAsync) -> Vec<Found> {
    let node = cx.node(d.node);
    let keyword = match node.value.await_token() {
        Some(it) => it.text_range(),
        None => node.value.syntax().text_range(),
    };
    let mut diag = Diag::error(
        Some("E0728"),
        "`await` is only allowed inside `async` functions and blocks",
        cx.span(node.with_value(keyword)),
    );
    diag.span_label(diag.span.primary[0].clone(), "only allowed inside `async` functions and blocks");
    if let Some(scope) = scope(node.value.syntax()) {
        diag.span_label(cx.span(node.with_value(scope)), "this is not `async`");
    }
    code_fact(cx, &mut diag, d.node.map(|it| it.syntax_node_ptr()));
    vec![Found::new(diag, Phase::Resolve)]
}

/// The span of the scope rustc's AST lowering is in at `node` (`current_item`): a function's
/// signature, a closure's head (`move |x| -> T`), an anonymous const's expression.
fn scope(node: &SyntaxNode) -> Option<TextRange> {
    let mut child = node.clone();
    for ancestor in node.ancestors().skip(1) {
        if let Some(it) = ast::Fn::cast(ancestor.clone()) {
            return Some(signature_range(&it));
        }
        if let Some(it) = ast::ClosureExpr::cast(ancestor.clone()) {
            let in_body = it.body().is_some_and(|body| body.syntax() == &child);
            if in_body {
                let end = match it.ret_type() {
                    Some(ret) => ret.syntax().text_range().end(),
                    None => it.param_list()?.syntax().text_range().end(),
                };
                return Some(TextRange::new(it.syntax().text_range().start(), end));
            }
        }
        if let Some(it) = ast::BlockExpr::cast(ancestor.clone())
            && it.const_token().is_some()
        {
            return Some(it.stmt_list()?.syntax().text_range());
        }
        if let Some(it) = ast::ArrayExpr::cast(ancestor.clone())
            && it.semicolon_token().is_some()
            && it.exprs().nth(1).is_some_and(|len| len.syntax() == &child)
        {
            return Some(child.text_range());
        }
        if ast::ConstArg::can_cast(ancestor.kind()) {
            return Some(ancestor.text_range());
        }
        if ast::Item::can_cast(ancestor.kind()) {
            return None;
        }
        child = ancestor;
    }
    None
}

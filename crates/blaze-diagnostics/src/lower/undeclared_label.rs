//! E0426: a `break` or `continue` naming a label no loop or block around it has, with rustc's
//! suggestion of a label of a similar name.
//!
//! Follows rustc's `compiler/rustc_resolve/src/late.rs` (`resolve_label`, the label ribs and
//! their barriers: `with_label_rib`, `with_constant_rib`, `is_label_valid_from_rib`),
//! `late/diagnostics.rs` (`suggestion_for_label_in_rib`), `diagnostics/impls.rs` and
//! `diagnostics/mod.rs` (`UndeclaredLabel`) (MIT or Apache-2.0).

use hir::InFile;
use syntax::{AstNode, SyntaxNode, TextRange, ast, ast::HasLoopBody};

use crate::{
    cx::Cx,
    lints::{code_fact, edit_distance::find_best_match_for_name},
    lower::{Found, Phase, break_outside_of_loop::ancestors},
    model::{Applicability, Diag},
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::UndeclaredLabel) -> Vec<Found> {
    let node = cx.node(d.node);
    let name = node.value.text().to_owned();
    let span = cx.span_of_ptr(d.node);
    let mut diag = Diag::error(Some("E0426"), format!("use of undeclared label `{name}`"), span.clone());
    diag.span_label(span.clone(), format!("undeclared label `{name}`"));
    match resolve(cx, &node.map(|it| it.syntax().clone()), &name) {
        Resolution::Undeclared { similar: Some(similar) } if similar.reachable => {
            diag.span_label(cx.span(similar.at), "a label with a similar name is reachable");
            diag.suggestion(span, "try using similarly named label", similar.name, Applicability::MaybeIncorrect);
        }
        Resolution::Undeclared { similar: Some(similar) } => {
            diag.span_label(cx.span(similar.at), "a label with a similar name exists but is unreachable");
        }
        _ => {}
    }
    code_fact(cx, &mut diag, d.node.map(|it| it.syntax_node_ptr()));
    vec![Found::new(diag, Phase::Resolve)]
}

/// A label of a name like the one asked for.
pub(crate) struct Similar {
    pub(crate) name: String,
    pub(crate) at: InFile<TextRange>,
    pub(crate) reachable: bool,
}

/// What rustc's `resolve_label` finds for a label used at `node`.
pub(crate) enum Resolution {
    Found,
    Unreachable { defined: InFile<TextRange>, similar: Option<Similar> },
    Undeclared { similar: Option<Similar> },
}

/// A rib of rustc's label ribs: a label, or a barrier labels are not reachable through
/// (a function, a closure, an `async` or `gen` block, a constant).
enum Rib {
    Label { name: String, at: InFile<TextRange> },
    Barrier,
}

/// rustc's `resolve_label` of the label `name` used at `node`.
pub(crate) fn resolve(cx: &Cx<'_, '_>, node: &InFile<SyntaxNode>, name: &str) -> Resolution {
    let ribs = ribs(cx, node);
    let mut similar: Option<Similar> = None;
    for (i, rib) in ribs.iter().enumerate() {
        let reachable = !ribs[..i].iter().any(|it| matches!(it, Rib::Barrier));
        let Rib::Label { name: label, at } = rib else { continue };
        if label == name {
            return match reachable {
                true => Resolution::Found,
                false => Resolution::Unreachable { defined: *at, similar },
            };
        }
        if similar.is_none()
            && at.file_id == node.file_id
            && find_best_match_for_name(&[label.as_str()], name, None).is_some()
        {
            similar = Some(Similar { name: label.clone(), at: *at, reachable });
        }
    }
    Resolution::Undeclared { similar }
}

/// The label ribs around `node`, innermost first.
fn ribs(cx: &Cx<'_, '_>, node: &InFile<SyntaxNode>) -> Vec<Rib> {
    let chain = ancestors(cx, node);
    let mut ribs = Vec::new();
    for (i, it) in chain.iter().enumerate().skip(1) {
        let child = &chain[i - 1].value;
        let label = |label: Option<ast::Label>| -> Option<Rib> {
            let lifetime = label?.lifetime()?;
            let at = it.with_value(lifetime.syntax().text_range());
            Some(Rib::Label { name: lifetime.text().to_owned(), at })
        };
        let rib = match ast::Expr::cast(it.value.clone()) {
            Some(ast::Expr::LoopExpr(loop_)) => label(loop_.label()),
            Some(ast::Expr::WhileExpr(loop_)) => label(loop_.label()),
            Some(ast::Expr::ForExpr(loop_)) => {
                let in_iterable = loop_.iterable().is_some_and(|it| it.syntax() == child);
                if in_iterable { None } else { label(loop_.label()) }
            }
            Some(ast::Expr::BlockExpr(block)) => {
                let coroutine = block.async_token().is_some() || block.gen_token().is_some();
                let constant = block.const_token().is_some();
                match coroutine || constant {
                    true => Some(Rib::Barrier),
                    false => label(block.label()),
                }
            }
            Some(ast::Expr::ClosureExpr(closure)) => {
                closure.body().is_some_and(|it| it.syntax() == child).then_some(Rib::Barrier)
            }
            _ => {
                let barrier = ast::Fn::can_cast(it.value.kind())
                    || ast::Const::can_cast(it.value.kind())
                    || ast::Static::can_cast(it.value.kind())
                    || ast::ConstArg::can_cast(it.value.kind())
                    || ast::ArrayExpr::cast(it.value.clone()).is_some_and(|array| {
                        array.semicolon_token().is_some()
                            && array.exprs().nth(1).is_some_and(|len| len.syntax() == child)
                    });
                barrier.then_some(Rib::Barrier)
            }
        };
        ribs.extend(rib);
    }
    ribs
}

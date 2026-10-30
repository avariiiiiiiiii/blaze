//! The lints rustc's parser buffers: `varargs_without_pattern` and `break_with_label_and_loop`.
//!
//! The parser buffers them at the crate (`CRATE_NODE_ID`), so the crate's attributes decide
//! their level and they are emitted as the early lint pass starts, in the order the source was
//! parsed: the crate's files, then what macros expand to. It parses what `cfg` then removes
//! and the items attribute macros are given, so all of each tree is linted.
//!
//! Follows rustc's `compiler/rustc_parse/src/parser/function.rs` (`parse_param_general`),
//! `parser/item.rs` (the `FnParseMode` of free, impl, trait and foreign functions),
//! `parser/expr.rs` (`parse_expr_break`) and `diagnostics.rs` (`VarargsWithoutPattern`,
//! `BreakWithLabelAndLoop`) (MIT or Apache-2.0).

use hir::{HirFileId, InFile};
use syntax::{
    AstNode, NodeOrToken, SyntaxKind, SyntaxNodePtr, T, TextRange,
    ast::{self},
};

use super::{edition_at, emit, span};
use crate::{
    cx::Cx,
    lints::syntax::{Emitted, Pass, Tree, Walk},
    model::{Applicability, Diag},
};

pub(super) fn lints(cx: &Cx<'_, '_>, walk: &Walk<'_>) -> Vec<Emitted> {
    let mut out = Vec::new();
    let Some(root) = cx.krate.root_module(cx.db).as_source_file_id(cx.db) else { return out };
    let Some(root) = walk.tree(HirFileId::from(root)) else { return out };
    let crate_at = root.ptr(&root.root);
    let parsed = walk.files().chain(walk.trees.iter().filter(|it| !it.in_file()));
    for tree in parsed {
        for node in tree.root.descendants() {
            if let Some(param) = ast::Param::cast(node.clone()) {
                varargs_without_pattern(cx, tree, &param, crate_at, &mut out);
            } else if let Some(expr) = ast::BreakExpr::cast(node) {
                break_with_label_and_loop(cx, tree, &expr, crate_at, &mut out);
            }
        }
    }
    out
}

/// `varargs_without_pattern`: a `...` parameter with no pattern where rustc's parser asks for
/// one: of a free function, an impl's, or a trait's from Rust 2018 (by the edition of the
/// `...`); never of a foreign function, a function pointer or a closure.
fn varargs_without_pattern(
    cx: &Cx<'_, '_>,
    tree: &Tree,
    param: &ast::Param,
    crate_at: InFile<SyntaxNodePtr>,
    out: &mut Vec<Emitted>,
) {
    let first = param
        .syntax()
        .children_with_tokens()
        .find(|it| !it.kind().is_trivia() && it.kind() != SyntaxKind::ATTR);
    let Some(NodeOrToken::Token(dots)) = first else { return };
    if dots.kind() != T![...] {
        return;
    }
    let Some(list) = param.syntax().parent().filter(|it| it.kind() == SyntaxKind::PARAM_LIST) else { return };
    let Some(item) = list.parent().filter(|it| it.kind() == SyntaxKind::FN) else { return };
    let required = match item.parent().map(|it| it.kind()) {
        Some(SyntaxKind::EXTERN_ITEM_LIST) => false,
        Some(SyntaxKind::ASSOC_ITEM_LIST) => {
            let in_trait = item.parent().and_then(|it| it.parent()).is_some_and(|it| it.kind() == SyntaxKind::TRAIT);
            !in_trait || edition_at(cx, tree.file, dots.text_range().start()).at_least_2018()
        }
        _ => true,
    };
    if !required {
        return;
    }
    let span = span(cx, tree, dots.text_range());
    let mut diag = Diag::lint("varargs_without_pattern", "missing pattern for `...` argument", span.clone());
    diag.suggestion(
        span,
        "name the argument, or use `_` to continue ignoring it",
        "_: ...",
        Applicability::MachineApplicable,
    );
    emit(cx, diag, Pass::Buffered, crate_at, tree.ptr(param.syntax()), out);
}

/// `break_with_label_and_loop`: `break 'label` whose value is an unlabeled `loop`, `while`,
/// `for` or plain block, which reads as an unlabeled `break` of a labeled loop.
fn break_with_label_and_loop(
    cx: &Cx<'_, '_>,
    tree: &Tree,
    expr: &ast::BreakExpr,
    crate_at: InFile<SyntaxNodePtr>,
    out: &mut Vec<Emitted>,
) {
    let (Some(break_), Some(_), Some(value)) = (expr.break_token(), expr.lifetime(), expr.expr()) else { return };
    let unlabeled = |node: &syntax::SyntaxNode| !node.children().any(|it| it.kind() == SyntaxKind::LABEL);
    let confusable = match &value {
        ast::Expr::LoopExpr(it) => unlabeled(it.syntax()),
        ast::Expr::WhileExpr(it) => unlabeled(it.syntax()),
        ast::Expr::ForExpr(it) => unlabeled(it.syntax()),
        ast::Expr::BlockExpr(it) => it.modifier().is_none(),
        _ => false,
    };
    if !confusable {
        return;
    }
    let value_range = value.syntax().text_range();
    let span = span(cx, tree, TextRange::new(break_.text_range().start(), value_range.end()));
    let left = span_at(cx, tree, value_range.start());
    let right = span_at(cx, tree, value_range.end());
    let mut diag = Diag::lint(
        "break_with_label_and_loop",
        "this labeled break expression is easy to confuse with an unlabeled break with a labeled value expression",
        span,
    );
    diag.multipart_suggestion(
        "wrap this expression in parentheses",
        vec![(left, "(".to_owned()), (right, ")".to_owned())],
        Applicability::MachineApplicable,
    );
    emit(cx, diag, Pass::Buffered, crate_at, tree.ptr(expr.syntax()), out);
}

fn span_at(cx: &Cx<'_, '_>, tree: &Tree, offset: syntax::TextSize) -> crate::model::Span {
    span(cx, tree, TextRange::empty(offset))
}

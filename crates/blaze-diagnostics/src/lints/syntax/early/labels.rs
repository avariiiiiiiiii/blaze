//! Labels: `unused_labels`, which late resolution buffers, and the warning it gives when a label
//! shadows one already in scope.
//!
//! Follows rustc's `compiler/rustc_resolve/src/late.rs` (`with_resolved_label`, `resolve_label`,
//! `resolve_block`) and `compiler/rustc_resolve/src/late/diagnostics.rs`
//! (`signal_label_shadowing`) (MIT or Apache-2.0).

use hir::{HirFileId, InFile};
use hir_def::{expr_store::Body, hir::Expr, nameres::demand::every_body};
use rustc_hash::FxHashSet;
use span::SyntaxContext;
use syntax::{
    AstNode, SyntaxKind, SyntaxNode, SyntaxNodePtr,
    ast::{self, HasLoopBody as _},
};

use super::ctxt;
use crate::{
    cx::Cx,
    lints::syntax::{Emitted, Pass, Walk, source_fact},
    lower::{Found, Phase, break_outside_of_loop::ancestors},
    model::{Diag, Level, MultiSpan},
};

pub(crate) fn lints(cx: &Cx<'_, '_>, walk: &Walk<'_>) -> Vec<Emitted> {
    let mut out = shadowing(cx, walk);
    out.extend(unused(cx));
    out
}

/// `unused_labels`: each label of a loop or block no `break` or `continue` resolved to, but
/// those whose name starts with `'_`. A desugared label (of a `try` block) is not one.
fn unused(cx: &Cx<'_, '_>) -> Vec<Emitted> {
    let mut out = Vec::new();
    for owner in every_body(cx.db, cx.krate.base()) {
        let (body, source_map) = Body::with_source_map(cx.db, owner);
        let mut declared = Vec::new();
        let mut used = FxHashSet::default();
        for (_, expr) in body.exprs() {
            match expr {
                Expr::Loop { label: Some(label), .. } | Expr::Block { label: Some(label), .. } => {
                    declared.push(*label)
                }
                Expr::Break { label: Some(label), .. } | Expr::Continue { label: Some(label) } => {
                    used.insert(*label);
                }
                _ => {}
            }
        }
        for label in declared {
            if used.contains(&label) {
                continue;
            }
            let source = source_map.label_syntax(label);
            let ptr = source.value.syntax_node_ptr();
            if ptr.kind() != SyntaxKind::LABEL {
                continue;
            }
            let node = cx.syntax(InFile::new(source.file_id, ptr));
            let Some(lifetime) = ast::Label::cast(node.value.clone()).and_then(|it| it.lifetime()) else {
                continue;
            };
            if lifetime.text().as_bytes().get(1) == Some(&b'_') {
                continue;
            }
            let Some(labeled) = node.value.parent() else { continue };
            let at = InFile::new(node.file_id, SyntaxNodePtr::new(&labeled));
            let span = cx.span(InFile::new(node.file_id, lifetime.syntax().text_range()));
            let mut diag = Diag::lint("unused_labels", "unused label", span);
            source_fact(cx, &mut diag, at);
            out.push(Emitted::new(diag, Phase::Resolve, Pass::Buffered, at));
        }
    }
    out
}

/// The warning resolution gives when a label resolves, as a `break` to it would, to a label of an
/// enclosing loop or block: in walk order, so in the order rustc's visitor declares them.
fn shadowing(cx: &Cx<'_, '_>, walk: &Walk<'_>) -> Vec<Emitted> {
    let mut out = Vec::new();
    for tree in &walk.trees {
        for node in &tree.nodes {
            let Some(label) = ast::Label::cast(node.clone()) else { continue };
            let Some(lifetime) = label.lifetime() else { continue };
            let Some(labeled) = node.parent() else { continue };
            let range = lifetime.syntax().text_range();
            let name = lifetime.text().to_owned();
            let mut hygiene = ctxt(cx, tree.file, range);
            let labeled = InFile::new(tree.file, labeled);
            let Some(orig) = resolve(cx, &labeled, &name, &mut hygiene) else { continue };
            let shadower = cx.span(InFile::new(tree.file, range));
            let mut diag = Diag::new(
                Level::Warning,
                None,
                format!("label name `{name}` shadows a label name that is already in scope"),
            );
            diag.span = MultiSpan::of(shadower.clone());
            diag.span_label(cx.span(orig), "first declared here");
            diag.span_label(shadower, format!("label `{name}` already in scope"));
            source_fact(cx, &mut diag, labeled.map(|it| SyntaxNodePtr::new(&it)));
            out.push(Emitted { found: Found::new(diag, Phase::Resolve), pass: Pass::Buffered });
        }
    }
    out
}

/// rustc's `resolve_label` from the label ribs enclosing `labeled`: the range of the lifetime of
/// the label `name` resolves to, when it is reachable (no closure, coroutine or constant lies
/// between).
fn resolve(
    cx: &Cx<'_, '_>,
    labeled: &InFile<SyntaxNode>,
    name: &str,
    hygiene: &mut SyntaxContext,
) -> Option<InFile<syntax::TextRange>> {
    let chain = ancestors(cx, labeled);
    for pair in chain.windows(2) {
        let (child, parent) = (&pair[0], &pair[1]);
        if barrier(&parent.value, &child.value) {
            return None;
        }
        if let Some(list) = ast::StmtList::cast(parent.value.clone()) {
            macro_definitions(cx, parent.file_id, &list, &child.value, hygiene);
        }
        let Some(label) = label_over(&parent.value, &child.value) else { continue };
        let Some(lifetime) = label.lifetime() else { continue };
        let range = lifetime.syntax().text_range();
        if lifetime.text() == name
            && ctxt(cx, parent.file_id, range).normalize_to_macro_rules(cx.db)
                == hygiene.normalize_to_macro_rules(cx.db)
        {
            return Some(InFile::new(parent.file_id, range));
        }
    }
    None
}

/// The label `parent` declares over `child`: rustc's label rib of a loop or block covers a
/// `loop`'s body, a `while`'s condition and body, a `for`'s pattern and body (not what it
/// iterates) and a block's statements.
fn label_over(parent: &SyntaxNode, child: &SyntaxNode) -> Option<ast::Label> {
    if let Some(it) = ast::LoopExpr::cast(parent.clone()) {
        return it.loop_body().filter(|body| body.syntax() == child).and(it.label());
    }
    if let Some(it) = ast::WhileExpr::cast(parent.clone()) {
        return (child.kind() != SyntaxKind::LABEL).then(|| it.label()).flatten();
    }
    if let Some(it) = ast::ForExpr::cast(parent.clone()) {
        let in_scope = it.loop_body().is_some_and(|body| body.syntax() == child)
            || it.pat().is_some_and(|pat| pat.syntax() == child);
        return in_scope.then(|| it.label()).flatten();
    }
    if let Some(it) = ast::BlockExpr::cast(parent.clone()) {
        return it.stmt_list().filter(|list| list.syntax() == child).and(it.label());
    }
    None
}

/// Whether rustc's label ribs have a barrier at `parent` over `child`: a closure, a coroutine
/// (`async` or `gen` block), a constant (an item, an inline `const` block, an array's length, a
/// const argument or parameter default, a discriminant) or an item's own scope.
fn barrier(parent: &SyntaxNode, child: &SyntaxNode) -> bool {
    match parent.kind() {
        SyntaxKind::CLOSURE_EXPR | SyntaxKind::CONST_ARG | SyntaxKind::CONST_PARAM | SyntaxKind::VARIANT => {
            true
        }
        SyntaxKind::BLOCK_EXPR => ast::BlockExpr::cast(parent.clone()).is_some_and(|it| {
            it.async_token().is_some() || it.gen_token().is_some() || it.const_token().is_some()
        }),
        SyntaxKind::ARRAY_EXPR => ast::ArrayExpr::cast(parent.clone())
            .and_then(|it| it.semicolon_token())
            .is_some_and(|semi| child.text_range().start() > semi.text_range().start()),
        SyntaxKind::ARRAY_TYPE => ast::Expr::can_cast(child.kind()) || child.kind() == SyntaxKind::CONST_ARG,
        SyntaxKind::MACRO_CALL => false,
        kind => ast::Item::can_cast(kind) || kind == SyntaxKind::SOURCE_FILE,
    }
}

/// rustc's `MacroDefinition` ribs of a block: each macro defined by a statement up to `child`.
/// When the outermost mark of the label's context is of a call of one of them, the label is
/// taken as written where the macro is defined (`remove_mark`).
fn macro_definitions(
    cx: &Cx<'_, '_>,
    file: HirFileId,
    list: &ast::StmtList,
    child: &SyntaxNode,
    hygiene: &mut SyntaxContext,
) {
    let end = child.text_range().end();
    let defined: Vec<SyntaxNodePtr> = list
        .statements()
        .filter(|stmt| stmt.syntax().text_range().start() < end)
        .filter_map(|stmt| match stmt {
            ast::Stmt::Item(ast::Item::MacroRules(it)) => Some(SyntaxNodePtr::new(it.syntax())),
            ast::Stmt::Item(ast::Item::MacroDef(it)) => Some(SyntaxNodePtr::new(it.syntax())),
            _ => None,
        })
        .collect();
    for ptr in defined.iter().rev() {
        let Some(call) = hygiene.outer_expn(cx.db) else { return };
        let def = hir_expand::MacroCallId::from(call).loc(cx.db).def;
        let Some(ast_id) = def.ast_id().left() else { continue };
        if ast_id.file_id == file && ast_id.to_ptr(cx.db).syntax_node_ptr() == *ptr {
            hygiene.remove_mark(cx.db);
        }
    }
}

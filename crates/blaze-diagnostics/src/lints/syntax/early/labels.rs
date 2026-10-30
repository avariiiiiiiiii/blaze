//! Labels: `unused_labels`, which late resolution buffers, and the warning it gives when a label
//! shadows one already in scope.
//!
//! `unused_labels` costs one pass over each body's expression column, a buffer of the labels it
//! declares and a bitset over its dense `u32` label indices of those used, both reused across
//! bodies: no hashing, nothing allocated per label (rustc keeps an `FxIndexMap` of every label it
//! declares). The shadowing check runs inside the
//! early pass's one walk (`builtin::lints`) and walks up from each label without collecting its
//! ancestors; its names are compared in place, and a context is computed only once two names
//! match.
//!
//! Follows rustc's `compiler/rustc_resolve/src/late.rs` (`with_resolved_label`, `resolve_label`,
//! `resolve_block`) and `compiler/rustc_resolve/src/late/diagnostics.rs`
//! (`signal_label_shadowing`) (MIT or Apache-2.0).

use hir::{HirFileId, InFile};
use hir_def::{expr_store::Body, hir::Expr, nameres::demand::every_body};
use span::SyntaxContext;
use syntax::{
    AstNode, SyntaxKind, SyntaxNode, SyntaxNodePtr, T, TextRange,
    ast::{self, HasLoopBody as _},
};

use super::ctxt;
use crate::{
    cx::Cx,
    lints::syntax::{Emitted, Pass, Tree, source_fact},
    lower::{Found, Phase},
    model::{Diag, Level, MultiSpan},
};

/// A set of a body's labels by their arena index: one bit each, a word per 64 labels.
#[derive(Default)]
struct Bits(Vec<u64>);

impl Bits {
    fn clear(&mut self) {
        self.0.clear();
    }

    fn insert(&mut self, index: u32) {
        let word = (index / 64) as usize;
        if word >= self.0.len() {
            self.0.resize(word + 1, 0);
        }
        self.0[word] |= 1 << (index % 64);
    }

    fn contains(&self, index: u32) -> bool {
        self.0.get((index / 64) as usize).is_some_and(|word| word & (1 << (index % 64)) != 0)
    }
}

/// `unused_labels`: each label of a loop or block no `break` or `continue` resolved to, but
/// those whose name starts with `'_`. A desugared label (of a `try` block) is not one.
pub(super) fn unused(cx: &Cx<'_, '_>) -> Vec<Emitted> {
    let mut out = Vec::new();
    let (mut declared, mut used) = (Vec::new(), Bits::default());
    for owner in every_body(cx.db, cx.krate.base()) {
        let (body, source_map) = Body::with_source_map(cx.db, owner);
        declared.clear();
        used.clear();
        for (_, expr) in body.exprs() {
            match expr {
                Expr::Loop { label: Some(label), .. } | Expr::Block { label: Some(label), .. } => declared.push(label),
                Expr::Break { label: Some(label), .. } | Expr::Continue { label: Some(label) } => {
                    used.insert(u32::from(label.into_raw()))
                }
                _ => {}
            }
        }
        for &label in &declared {
            if used.contains(u32::from(label.into_raw())) {
                continue;
            }
            let source = source_map.label_syntax(label);
            let ptr = source.value.syntax_node_ptr();
            if ptr.kind() != SyntaxKind::LABEL {
                continue;
            }
            let node = cx.syntax(InFile::new(source.file_id, ptr));
            let Some(lifetime) = node.value.children().find_map(ast::Lifetime::cast) else { continue };
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

/// The warning resolution gives when the label `node` (of `tree`) resolves, as a `break` to it
/// would, to a label of an enclosing loop or block. Called in walk order, so in the order rustc's
/// visitor declares them.
pub(super) fn shadowing(cx: &Cx<'_, '_>, tree: &Tree, node: &SyntaxNode, out: &mut Vec<Emitted>) {
    let Some(lifetime) = node.children().find_map(ast::Lifetime::cast) else { return };
    let Some(labeled) = node.parent() else { return };
    let range = lifetime.syntax().text_range();
    let labeled = InFile::new(tree.file, labeled);
    let Some(orig) = resolve(cx, &labeled, lifetime.text(), ctxt(cx, tree.file, range)) else { return };
    let name = lifetime.text();
    let shadower = cx.span(InFile::new(tree.file, range));
    let mut diag =
        Diag::new(Level::Warning, None, format!("label name `{name}` shadows a label name that is already in scope"));
    diag.span = MultiSpan::of(shadower.clone());
    diag.span_label(cx.span(orig), "first declared here");
    diag.span_label(shadower, format!("label `{name}` already in scope"));
    source_fact(cx, &mut diag, labeled.map(|it| SyntaxNodePtr::new(&it)));
    out.push(Emitted { found: Found::new(diag, Phase::Resolve), pass: Pass::Buffered });
}

/// rustc's `resolve_label` from the label ribs enclosing `labeled`: the range of the lifetime of
/// the label `name` resolves to, when it is reachable (no closure, coroutine or constant lies
/// between). Walks up through each expansion to its call; the root of an expansion and its call
/// are no rib.
fn resolve(
    cx: &Cx<'_, '_>,
    labeled: &InFile<SyntaxNode>,
    name: &str,
    mut hygiene: SyntaxContext,
) -> Option<InFile<TextRange>> {
    let (mut file, mut child) = (labeled.file_id, labeled.value.clone());
    let mut key: Option<SyntaxContext> = None;
    loop {
        let Some(parent) = child.parent() else {
            let call = file.macro_file()?.loc(cx.db).to_node(cx.db);
            (file, child) = (call.file_id, call.value);
            continue;
        };
        if barrier(&parent, &child) {
            return None;
        }
        if parent.kind() == SyntaxKind::STMT_LIST && macro_definitions(cx, file, &parent, &child, &mut hygiene) {
            key = None;
        }
        if let Some(label) = label_over(&parent, &child)
            && let Some(lifetime) = label.lifetime()
            && lifetime.text() == name
        {
            let range = lifetime.syntax().text_range();
            let key = *key.get_or_insert_with(|| hygiene.normalize_to_macro_rules(cx.db));
            if ctxt(cx, file, range).normalize_to_macro_rules(cx.db) == key {
                return Some(InFile::new(file, range));
            }
        }
        child = parent;
    }
}

/// The label `parent` declares over `child`: rustc's label rib of a loop or block covers a
/// `loop`'s body, a `while`'s condition and body, a `for`'s pattern and body (not what it
/// iterates) and a block's statements.
fn label_over(parent: &SyntaxNode, child: &SyntaxNode) -> Option<ast::Label> {
    match parent.kind() {
        SyntaxKind::LOOP_EXPR => {
            let it = ast::LoopExpr::cast(parent.clone())?;
            it.loop_body().filter(|body| body.syntax() == child).and(it.label())
        }
        SyntaxKind::WHILE_EXPR if child.kind() != SyntaxKind::LABEL => ast::WhileExpr::cast(parent.clone())?.label(),
        SyntaxKind::FOR_EXPR => {
            let it = ast::ForExpr::cast(parent.clone())?;
            let in_scope = it.loop_body().is_some_and(|body| body.syntax() == child)
                || it.pat().is_some_and(|pat| pat.syntax() == child);
            in_scope.then(|| it.label()).flatten()
        }
        SyntaxKind::BLOCK_EXPR if child.kind() == SyntaxKind::STMT_LIST => ast::BlockExpr::cast(parent.clone())?.label(),
        _ => None,
    }
}

/// Whether rustc's label ribs have a barrier at `parent` over `child`: a closure, a coroutine
/// (`async` or `gen` block), a constant (an item, an inline `const` block, an array's length, a
/// const argument or parameter default, a discriminant) or an item's own scope.
fn barrier(parent: &SyntaxNode, child: &SyntaxNode) -> bool {
    match parent.kind() {
        SyntaxKind::CLOSURE_EXPR | SyntaxKind::CONST_ARG | SyntaxKind::CONST_PARAM | SyntaxKind::VARIANT => {
            true
        }
        SyntaxKind::BLOCK_EXPR => parent
            .children_with_tokens()
            .any(|it| matches!(it.kind(), T![async] | T![gen] | T![const])),
        SyntaxKind::ARRAY_EXPR => parent
            .children_with_tokens()
            .find(|it| it.kind() == T![;])
            .is_some_and(|semi| child.text_range().start() > semi.text_range().start()),
        SyntaxKind::ARRAY_TYPE => ast::Expr::can_cast(child.kind()) || child.kind() == SyntaxKind::CONST_ARG,
        SyntaxKind::MACRO_CALL => false,
        kind => ast::Item::can_cast(kind) || kind == SyntaxKind::SOURCE_FILE,
    }
}

/// rustc's `MacroDefinition` ribs of a block (`list`, in `file`): each macro defined by a
/// statement before `child`, the last first. While the outermost mark of the label's context
/// is of a call of one of them, the label is taken as written where that macro is defined
/// (`remove_mark`), and only the macros before it are left. Whether a mark was removed. A label
/// no macro wrote costs one lookup of its context's outermost mark.
fn macro_definitions(
    cx: &Cx<'_, '_>,
    file: HirFileId,
    list: &SyntaxNode,
    child: &SyntaxNode,
    hygiene: &mut SyntaxContext,
) -> bool {
    let mut bound = child.text_range().start();
    let mut removed = false;
    while let Some(call) = hygiene.outer_expn(cx.db) {
        let Some(ast_id) = hir_expand::MacroCallId::from(call).loc(cx.db).def.ast_id().left() else { break };
        if ast_id.file_id != file {
            break;
        }
        let ptr = ast_id.to_ptr(cx.db).syntax_node_ptr();
        let start = ptr.text_range().start();
        let defined_here = start < bound
            && list.children().any(|stmt| {
                matches!(stmt.kind(), SyntaxKind::MACRO_RULES | SyntaxKind::MACRO_DEF) && SyntaxNodePtr::new(&stmt) == ptr
            });
        if !defined_here {
            break;
        }
        hygiene.remove_mark(cx.db);
        removed = true;
        bound = start;
    }
    removed
}

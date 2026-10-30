//! The lints of rustc's early lint passes in `builtin.rs` and beside it: `while_true`,
//! `unsafe_code`, `special_module_name`, `anonymous_parameters`,
//! `ellipsis_inclusive_range_patterns` (an error, E0783, from the 2021 edition on),
//! `double_negations`, `ambiguous_negative_literals` and `redundant_semicolons`.
//!
//! Follows rustc's `compiler/rustc_lint/src/builtin.rs` (`WhileTrue`, `UnsafeCode`,
//! `SpecialModuleName`, `AnonymousParameters`, `EllipsisInclusiveRangePatterns`,
//! `DoubleNegations`), `compiler/rustc_lint/src/precedence.rs`,
//! `compiler/rustc_lint/src/redundant_semicolon.rs`, their diagnostics in
//! `compiler/rustc_lint/src/lints.rs` and `compiler/rustc_lint/src/diagnostics.rs`, and
//! `compiler/rustc_parse/src/parser/stmt.rs` (`parse_full_stmt`: which `;` is a statement of
//! its own) (MIT or Apache-2.0).

use hir::{HirFileId, InFile};
use hir_expand::{MacroDefKind, builtin::BuiltinFnLikeExpander};
use span::Edition;
use syntax::{
    AstNode, NodeOrToken, SyntaxKind, SyntaxNode, TextRange, T,
    ast::{self, HasLoopBody as _, HasName as _, LiteralKind, RangeItem as _, UnaryOp},
};

use super::{Expansions, allows_unsafe, ctxt, range_from_expansion};
use crate::{
    cx::{Cx, item_range},
    lints::syntax::{Emitted, Pass, Tree, Walk, from_expansion, source_fact},
    lower::{Found, Phase},
    model::{Applicability, Diag, SuggestionStyle},
};

pub(crate) fn lints(cx: &Cx<'_, '_>, walk: &Walk<'_>) -> Vec<Emitted> {
    let expansions = Expansions::of(cx, walk);
    let mut out = Vec::new();
    special_module_names(cx, walk, &expansions, &mut out);
    for tree in &walk.trees {
        global_asm(cx, tree, &mut out);
        // The range pattern whose `...` was linted with the `&` before it: nothing in it is
        // linted again (`EllipsisInclusiveRangePatterns::node_id`).
        let mut linted_ref: Option<TextRange> = None;
        for node in &tree.nodes {
            unsafe_code(cx, walk, tree, node, &mut out);
            if cx.edition == Edition::Edition2015 {
                anonymous_parameters(cx, tree, node, &mut out);
            }
            if let Some(range) = linted_ref
                && !range.contains_range(node.text_range())
            {
                linted_ref = None;
            }
            if linted_ref.is_none() {
                linted_ref = ellipsis_range(cx, tree, node, &mut out);
            }
            while_true(cx, tree, node, &mut out);
            redundant_semicolons(cx, &expansions, tree, node, &mut out);
            ambiguous_negative_literal(cx, tree, node, &mut out);
            double_negation(cx, tree, node, &mut out);
        }
    }
    out
}

/// A lint of the early lint pass at `at`, with the fact of the source it read.
fn emitted(cx: &Cx<'_, '_>, mut diag: Diag, pass: Pass, at: InFile<syntax::SyntaxNodePtr>) -> Emitted {
    source_fact(cx, &mut diag, at);
    Emitted::new(diag, Phase::Resolve, pass, at)
}

/// rustc's `span` of a node: from its first token after its attributes.
fn range_of(node: &SyntaxNode) -> TextRange {
    item_range(node)
}

/// `while true { .. }`: `WhileTrue`.
fn while_true(cx: &Cx<'_, '_>, tree: &Tree, node: &SyntaxNode, out: &mut Vec<Emitted>) {
    let Some(it) = ast::WhileExpr::cast(node.clone()) else { return };
    let Some(cond) = it.condition() else { return };
    let mut peeled = cond.clone();
    while let ast::Expr::ParenExpr(paren) = &peeled {
        let Some(inner) = paren.expr() else { return };
        peeled = inner;
    }
    let ast::Expr::Literal(lit) = &peeled else { return };
    if lit.kind() != LiteralKind::Bool(true) || from_expansion(cx, InFile::new(tree.file, cond.syntax())) {
        return;
    }
    let range = TextRange::new(range_of(node).start(), cond.syntax().text_range().end());
    let span = cx.span(InFile::new(tree.file, range));
    let replace = match it.label().and_then(|it| it.lifetime()) {
        Some(label) => format!("{}: loop", label.text()),
        None => "loop".to_owned(),
    };
    let mut diag = Diag::lint("while_true", "denote infinite loops with `loop { ... }`", span.clone());
    diag.suggestion_with_style(
        vec![(span, replace)],
        "use `loop`",
        Applicability::MachineApplicable,
        SuggestionStyle::HideCodeInline,
    );
    out.push(emitted(cx, diag, Pass::Early, tree.ptr(node)));
}

/// `unsafe` blocks, traits, impls, `extern` blocks and functions, and
/// `#[allow_internal_unsafe]` on a macro: `UnsafeCode`.
fn unsafe_code(cx: &Cx<'_, '_>, walk: &Walk<'_>, tree: &Tree, node: &SyntaxNode, out: &mut Vec<Emitted>) {
    let mut report = |range: TextRange, message: &str| {
        if allows_unsafe(cx, tree.file, range) {
            return;
        }
        let diag = Diag::lint("unsafe_code", message, cx.span(InFile::new(tree.file, range)));
        out.push(emitted(cx, diag, Pass::Early, tree.ptr(node)));
    };
    if let Some(it) = ast::BlockExpr::cast(node.clone())
        && it.unsafe_token().is_some()
    {
        report(range_of(node), "usage of an `unsafe` block");
    }
    if let Some(it) = ast::Trait::cast(node.clone())
        && it.unsafe_token().is_some()
    {
        report(range_of(node), "declaration of an `unsafe` trait");
    }
    if let Some(it) = ast::Impl::cast(node.clone())
        && it.unsafe_token().is_some()
        && it.trait_().is_some()
    {
        report(range_of(node), "implementation of an `unsafe` trait");
    }
    if let Some(it) = ast::ExternBlock::cast(node.clone())
        && it.unsafe_token().is_some()
    {
        report(range_of(node), "usage of an `unsafe extern` block");
    }
    if (ast::MacroRules::can_cast(node.kind()) || ast::MacroDef::can_cast(node.kind()))
        && let Some(owner) = ast::AnyHasAttrs::cast(node.clone())
    {
        let attr = walk.attrs(&owner).into_iter().find(|(meta, _)| meta.simple_name().as_deref() == Some("allow_internal_unsafe"));
        if let Some((meta, attr)) = attr {
            // An attribute `cfg_attr` gives is spanned by what `cfg_attr` holds of it.
            let written = attr.meta().is_some_and(|it| it.syntax() == meta.syntax());
            let range = if written { attr.syntax().text_range() } else { meta.syntax().text_range() };
            report(
                range,
                "`allow_internal_unsafe` allows defining macros using unsafe without triggering the `unsafe_code` lint at their call site",
            );
        }
    }
    if let Some(it) = ast::Fn::cast(node.clone())
        && it.unsafe_token().is_some()
    {
        let message = match node.parent().map(|it| it.kind()) {
            Some(SyntaxKind::EXTERN_ITEM_LIST) => return,
            Some(SyntaxKind::ASSOC_ITEM_LIST) if it.body().is_none() => "declaration of an `unsafe` method",
            Some(SyntaxKind::ASSOC_ITEM_LIST) => "implementation of an `unsafe` method",
            _ => "declaration of an `unsafe` function",
        };
        report(range_of(node), message);
    }
}

/// A call of `global_asm!`, the item it expands to: `UnsafeCode`'s `ItemKind::GlobalAsm`.
fn global_asm(cx: &Cx<'_, '_>, tree: &Tree, out: &mut Vec<Emitted>) {
    let Some(call) = tree.call else { return };
    let loc = call.loc(cx.db);
    if !matches!(loc.def.kind, MacroDefKind::BuiltIn(_, BuiltinFnLikeExpander::GlobalAsm)) {
        return;
    }
    let node = loc.to_node(cx.db);
    let Some(macro_call) = ast::MacroCall::cast(node.value.clone()) else { return };
    let start = range_of(&node.value).start();
    let end = macro_call.token_tree().map_or(node.value.text_range().end(), |it| it.syntax().text_range().end());
    let range = TextRange::new(start, end);
    if allows_unsafe(cx, node.file_id, range) {
        return;
    }
    let mut diag = Diag::lint("unsafe_code", "usage of `core::arch::global_asm`", cx.span(node.with_value(range)));
    diag.note("using this macro is unsafe even though it does not need an `unsafe` block");
    out.push(emitted(cx, diag, Pass::Early, node.map(|it| syntax::SyntaxNodePtr::new(&it))));
}

/// `mod lib;` and `mod main;` among the items of the crate root: `SpecialModuleName`, which
/// rustc checks in `check_crate`.
fn special_module_names(cx: &Cx<'_, '_>, walk: &Walk<'_>, expansions: &Expansions<'_>, out: &mut Vec<Emitted>) {
    let root = cx.krate.root_module(cx.db);
    let Some(file) = root.as_source_file_id(cx.db) else { return };
    let Some(tree) = walk.tree(HirFileId::from(file)) else { return };
    let mut items = Vec::new();
    crate_items(tree, &tree.root, expansions, &mut items);
    for (in_tree, item) in items {
        let Some(module) = ast::Module::cast(item.clone()) else { continue };
        if module.item_list().is_some() {
            continue;
        }
        let Some(owner) = ast::AnyHasAttrs::cast(item.clone()) else { continue };
        if walk.attrs(&owner).iter().any(|(meta, _)| meta.simple_name().as_deref() == Some("path")) {
            continue;
        }
        let Some(name) = module.name() else { continue };
        let span = cx.span(InFile::new(in_tree.file, range_of(&item)));
        let mut diag = match name.text().trim_start_matches("r#") {
            "lib" => {
                let mut diag = Diag::lint("special_module_name", "found module declaration for lib.rs", span);
                diag.note("lib.rs is the root of this crate's library target");
                diag.help("to refer to it from other targets, use the library's name as the path");
                diag
            }
            "main" => {
                let mut diag = Diag::lint("special_module_name", "found module declaration for main.rs", span);
                diag.note("a binary crate cannot be used as library");
                diag
            }
            _ => continue,
        };
        let at = tree.ptr(&tree.root);
        source_fact(cx, &mut diag, InFile::new(in_tree.file, syntax::SyntaxNodePtr::new(&item)));
        out.push(Emitted::new(diag, Phase::Resolve, Pass::Early, at));
    }
}

/// The items of the crate root as rustc's AST has them once expanded: those of the file, and
/// in place of a macro call or of an item an attribute macro replaces, what it expands to.
fn crate_items<'w>(tree: &'w Tree, parent: &SyntaxNode, expansions: &Expansions<'w>, out: &mut Vec<(&'w Tree, SyntaxNode)>) {
    for child in parent.children() {
        if let Some(expanded) = expansions.of_call(tree.file, &child)
            && (ast::MacroCall::can_cast(child.kind()) || tree.replaced.contains(&child))
        {
            crate_items(expanded, &expanded.root, expansions, out);
            continue;
        }
        if tree.skipped(child.text_range()) || !ast::Item::can_cast(child.kind()) {
            continue;
        }
        out.push((tree, child));
    }
}

/// A trait method's parameter with a type and no name, in the 2015 edition:
/// `AnonymousParameters`.
fn anonymous_parameters(cx: &Cx<'_, '_>, tree: &Tree, node: &SyntaxNode, out: &mut Vec<Emitted>) {
    let Some(it) = ast::Fn::cast(node.clone()) else { return };
    let in_trait = node
        .parent()
        .filter(|it| it.kind() == SyntaxKind::ASSOC_ITEM_LIST)
        .and_then(|it| it.parent())
        .is_some_and(|it| it.kind() == SyntaxKind::TRAIT);
    if !in_trait {
        return;
    }
    let Some(params) = it.param_list() else { return };
    for param in params.params() {
        if param.pat().is_some() || tree.skipped(param.syntax().text_range()) {
            continue;
        }
        let Some(ty) = param.ty() else { continue };
        let span = cx.span(InFile::new(tree.file, ty.syntax().text_range()));
        let (ty_snip, applicability) = match cx.snippet(&span) {
            Some(snip) => (snip.to_owned(), Applicability::MachineApplicable),
            None => ("<type>".to_owned(), Applicability::HasPlaceholders),
        };
        let mut diag = Diag::lint(
            "anonymous_parameters",
            "anonymous parameters are deprecated and will be removed in the next edition",
            span.clone(),
        );
        diag.suggestion(span, "try naming the parameter or explicitly ignoring it", format!("_: {ty_snip}"), applicability);
        out.push(emitted(cx, diag, Pass::Early, tree.ptr(node)));
    }
}

/// A `...` range pattern: `EllipsisInclusiveRangePatterns`. The lint before the 2021 edition
/// of the `...`, error E0783 from it on. The range of the pattern linted with the `&` before
/// it, when it is, whose patterns are not linted again.
fn ellipsis_range(cx: &Cx<'_, '_>, tree: &Tree, node: &SyntaxNode, out: &mut Vec<Emitted>) -> Option<TextRange> {
    let ellipsis = |pat: &ast::Pat| -> Option<(ast::RangePat, syntax::SyntaxToken, ast::Pat)> {
        let ast::Pat::RangePat(range) = pat else { return None };
        let op = range.op_token().filter(|it| it.kind() == T![...])?;
        let end = range.end()?;
        Some((range.clone(), op, end))
    };
    let pat = ast::Pat::cast(node.clone())?;
    let (parenthesise, (range, join, end)) = match &pat {
        ast::Pat::RefPat(it) => (true, ellipsis(&it.pat()?)?),
        _ => (false, ellipsis(&pat)?),
    };
    let edition = ctxt(cx, tree.file, join.text_range()).edition(cx.db);
    let pat_span = cx.span(InFile::new(tree.file, node.text_range()));
    let join_span = cx.span(InFile::new(tree.file, join.text_range()));
    let message = "`...` range patterns are deprecated";
    let suggestion = "use `..=` for an inclusive range";
    let at = tree.ptr(node);
    let mut diag = if parenthesise {
        let end = end.syntax().text().to_string();
        let replace = match range.start() {
            Some(start) => format!("&({}..={end})", start.syntax().text()),
            None => format!("&(..={end})"),
        };
        if edition >= Edition::Edition2021 {
            let mut diag = Diag::error(Some("E0783"), message, pat_span.clone());
            diag.suggestion_with_style(vec![(pat_span, replace)], suggestion, Applicability::MachineApplicable, SuggestionStyle::HideCodeInline);
            diag
        } else {
            let mut diag = Diag::lint("ellipsis_inclusive_range_patterns", message, pat_span.clone());
            diag.suggestion(pat_span, suggestion, replace, Applicability::MachineApplicable);
            diag
        }
    } else if edition >= Edition::Edition2021 {
        let mut diag = Diag::error(Some("E0783"), message, pat_span);
        diag.suggestion_with_style(vec![(join_span, "..=".to_owned())], suggestion, Applicability::MachineApplicable, SuggestionStyle::HideCodeInline);
        diag
    } else {
        let mut diag = Diag::lint("ellipsis_inclusive_range_patterns", message, join_span.clone());
        diag.suggestion_with_style(vec![(join_span, "..=".to_owned())], suggestion, Applicability::MachineApplicable, SuggestionStyle::HideCodeInline);
        diag
    };
    source_fact(cx, &mut diag, at);
    let found = match diag.is_lint() {
        true => Found::lint(diag, Phase::Resolve, at),
        // An error of the early lint pass, placed where the pass emits it.
        false => Found { diag, phase: Phase::Resolve, lint_at: Some(at) },
    };
    out.push(Emitted { found, pass: Pass::Early });
    parenthesise.then(|| node.text_range())
}

/// `--x`, linted at the innermost `--` of a chain of `-`: `DoubleNegations`.
fn double_negation(cx: &Cx<'_, '_>, tree: &Tree, node: &SyntaxNode, out: &mut Vec<Emitted>) {
    let neg = |it: &ast::PrefixExpr| it.op_kind() == Some(UnaryOp::Neg);
    let Some(outer) = ast::PrefixExpr::cast(node.clone()).filter(neg) else { return };
    let Some(ast::Expr::PrefixExpr(inner)) = outer.expr() else { return };
    if !neg(&inner) {
        return;
    }
    if let Some(ast::Expr::PrefixExpr(it)) = inner.expr()
        && neg(&it)
    {
        return;
    }
    let (outer_range, inner_range) = (range_of(node), range_of(inner.syntax()));
    if ctxt(cx, tree.file, outer_range) != ctxt(cx, tree.file, inner_range) {
        return;
    }
    let at = |range: TextRange| cx.span(InFile::new(tree.file, range));
    let mut diag = Diag::lint("double_negations", "use of a double negation", at(outer_range));
    diag.note("the prefix `--` could be misinterpreted as a decrement operator which exists in other languages");
    diag.note("use `-= 1` if you meant to decrement the value");
    diag.suggestion_with_style(
        vec![
            (at(TextRange::empty(inner_range.start())), "(".to_owned()),
            (at(TextRange::empty(inner_range.end())), ")".to_owned()),
        ],
        "add parentheses for clarity",
        Applicability::MaybeIncorrect,
        SuggestionStyle::ShowCode,
    );
    out.push(emitted(cx, diag, Pass::Early, tree.ptr(node)));
}

/// `-1.abs()`: a `-` before method calls on a number literal: `Precedence`.
fn ambiguous_negative_literal(cx: &Cx<'_, '_>, tree: &Tree, node: &SyntaxNode, out: &mut Vec<Emitted>) {
    let Some(expr) = ast::PrefixExpr::cast(node.clone()) else { return };
    if expr.op_kind() != Some(UnaryOp::Neg) {
        return;
    }
    let Some(operand) = expr.expr() else { return };
    let mut arg = operand.clone();
    let mut at_least_one = false;
    while let ast::Expr::MethodCallExpr(call) = &arg {
        at_least_one = true;
        let Some(receiver) = call.receiver() else { return };
        arg = receiver;
    }
    let ast::Expr::Literal(lit) = &arg else { return };
    if !at_least_one || !matches!(lit.kind(), LiteralKind::IntNumber(_) | LiteralKind::FloatNumber(_)) {
        return;
    }
    let at = |range: TextRange| cx.span(InFile::new(tree.file, range));
    let (expr_range, arg_range, operand_range) = (range_of(node), range_of(arg.syntax()), range_of(operand.syntax()));
    let mut diag = Diag::lint(
        "ambiguous_negative_literals",
        "`-` has lower precedence than method calls, which might be unexpected",
        at(expr_range),
    );
    diag.note("e.g. `-4.abs()` equals `-4`; while `(-4).abs()` equals `4`");
    diag.suggestion_with_style(
        vec![
            (at(TextRange::empty(expr_range.start())), "(".to_owned()),
            (at(TextRange::empty(arg_range.end())), ")".to_owned()),
        ],
        "add parentheses around the `-` and the literal to call the method on a negative literal",
        Applicability::MaybeIncorrect,
        SuggestionStyle::ShowCode,
    );
    diag.suggestion_with_style(
        vec![
            (at(TextRange::empty(operand_range.start())), "(".to_owned()),
            (at(TextRange::empty(operand_range.end())), ")".to_owned()),
        ],
        "add parentheses around the literal and the method call to keep the current behavior",
        Applicability::MaybeIncorrect,
        SuggestionStyle::ShowCode,
    );
    out.push(emitted(cx, diag, Pass::Early, tree.ptr(node)));
}

/// A statement of a block as rustc's AST has it once expanded: an empty one (a `;` of its
/// own) where it is written, or any other.
enum Stmt {
    Empty(InFile<TextRange>),
    Other,
}

/// `;` after a statement that already ends: `RedundantSemicolons`, of each block.
fn redundant_semicolons(
    cx: &Cx<'_, '_>,
    expansions: &Expansions<'_>,
    tree: &Tree,
    node: &SyntaxNode,
    out: &mut Vec<Emitted>,
) {
    if node.kind() != SyntaxKind::STMT_LIST {
        return;
    }
    let Some(block) = node.parent() else { return };
    let mut stmts = Vec::new();
    statements(tree, node, expansions, &mut stmts);
    let mut seq: Option<(InFile<TextRange>, bool)> = None;
    let mut flush = |seq: &mut Option<(InFile<TextRange>, bool)>| {
        let Some((range, multiple)) = seq.take() else { return };
        let span = cx.span(range);
        let message = match multiple {
            true => "unnecessary trailing semicolons",
            false => "unnecessary trailing semicolon",
        };
        let mut diag = Diag::lint("redundant_semicolons", message, span.clone());
        if !range_from_expansion(cx, range) {
            let remove = match multiple {
                true => "remove these semicolons",
                false => "remove this semicolon",
            };
            diag.suggestion(span, remove, "", Applicability::MaybeIncorrect);
        }
        out.push(emitted(cx, diag, Pass::Early, tree.ptr(&block)));
    };
    for stmt in stmts {
        let Stmt::Empty(range) = stmt else {
            flush(&mut seq);
            continue;
        };
        if let Some((at, multiple)) = &mut seq
            && at.file_id == range.file_id
        {
            *at = at.with_value(at.value.cover(range.value));
            *multiple = true;
        } else {
            flush(&mut seq);
            seq = Some((range, false));
        }
    }
    flush(&mut seq);
}

/// The statements of `parent` (a block's statements, or what a macro call in statement
/// position expands to), in order, without what `cfg` removes, each macro call in statement
/// position replaced by its expansion's statements.
fn statements<'w>(tree: &'w Tree, parent: &SyntaxNode, expansions: &Expansions<'w>, out: &mut Vec<Stmt>) {
    for child in parent.children_with_tokens() {
        let child = match child {
            NodeOrToken::Token(token) => {
                if token.kind() == T![;] {
                    out.push(Stmt::Empty(InFile::new(tree.file, token.text_range())));
                }
                continue;
            }
            NodeOrToken::Node(node) => node,
        };
        if matches!(child.kind(), SyntaxKind::ATTR | SyntaxKind::DOC_COMMENT) || tree.skipped(child.text_range()) {
            continue;
        }
        let call = match ast::ExprStmt::cast(child.clone()).and_then(|it| it.expr()) {
            Some(ast::Expr::MacroExpr(it)) => it.macro_call().map(|it| it.syntax().clone()),
            _ => ast::MacroExpr::cast(child.clone())
                .and_then(|it| it.macro_call())
                .map(|it| it.syntax().clone())
                .or_else(|| ast::MacroCall::can_cast(child.kind()).then(|| child.clone())),
        };
        if let Some(call) = call
            && let Some(expanded) = expansions.of_call(tree.file, &call)
            && expanded.root.kind() == SyntaxKind::MACRO_STMTS
        {
            statements(expanded, &expanded.root, expansions, out);
            continue;
        }
        out.push(Stmt::Other);
    }
}

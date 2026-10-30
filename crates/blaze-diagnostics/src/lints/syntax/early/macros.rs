//! The lints of `macro_rules!`, of expansion and of the builtin macros:
//!
//! - `semicolon_in_expressions_from_macros` and `semicolon_in_expressions_from_non_local_macros`:
//!   a `macro_rules!` macro called as an expression whose expansion ends in `;`.
//! - `incomplete_include`: an `include!` as an expression of a file with more than one.
//! - `duplicate_macro_attributes`: `#[test]`, `#[bench]`, `#[test_case]` or `#[cfg_eval]` on an
//!   item that has it again.
//! - `matchers`: what rustc lints on a macro's definition (`meta_variable_misuse`,
//!   `rust_2021_incompatible_or_patterns`, `edition_2024_expr_fragment_specifier`).
//! - `asm`: `bad_asm_style` (`named_asm_labels` and `binary_asm_labels` are the late pass
//!   `AsmLabels`, in `lints/bodies/calls/methods`).
//!
//! Follows rustc's `compiler/rustc_expand/src/mbe/macro_rules.rs` (`ParserAnyMacro::make`,
//! `is_defined_in_current_crate`), `compiler/rustc_expand/src/expand.rs` (`assign_id!`, the
//! `lint_node_id` a lint of expansion is buffered at; `flat_map_stmt`'s `is_trailing_mac`),
//! `compiler/rustc_parse/src/parser/stmt.rs` (`parse_stmt_mac`: which macro calls in a block
//! are expressions), `compiler/rustc_builtin_macros/src/source_util.rs` (`expand_include`) and
//! `compiler/rustc_builtin_macros/src/util.rs` (`warn_on_duplicate_attribute`), with the text
//! of `compiler/rustc_expand/src/diagnostics.rs` (`TrailingMacro`) and
//! `compiler/rustc_builtin_macros/src/diagnostics.rs` (`IncompleteInclude`,
//! `DuplicateMacroAttribute`) (MIT or Apache-2.0).

mod asm;
mod matchers;

use hir::{InFile, MacroCallId};
use hir_expand::{ExpandTo, MacroCallKind, MacroDefKind, builtin::BuiltinAttrExpander};
use syntax::{
    AstNode, AstToken as _, NodeOrToken, SyntaxKind, SyntaxNode, SyntaxNodePtr, SyntaxToken, T,
    TextSize,
    ast::{self, HasName as _},
};

use crate::{
    cx::Cx,
    lints::syntax::{Emitted, Pass, Tree, Walk, source_fact},
    lower::Phase,
    model::{Diag, Span},
};

/// The lints of macros and their expansions in the crate, as rustc's expansion buffers them,
/// and those of its early and late lint passes the parts give.
pub(crate) fn lints(cx: &Cx<'_, '_>, walk: &Walk<'_>) -> Vec<Emitted> {
    let mut emitted = Vec::new();
    for tree in &walk.trees {
        let Some(call) = tree.call else { continue };
        emitted.extend(trailing_semicolon(cx, tree, call));
        emitted.extend(incomplete_include(cx, tree, call));
        emitted.extend(duplicate_attribute(cx, walk, call));
    }
    emitted.extend(matchers::lints(cx, walk));
    emitted.extend(asm::lints(cx, walk));
    emitted
}

/// `semicolon_in_expressions_from_macros` (`..._from_non_local_macros` for a macro of another
/// crate): rustc parses what a `macro_rules!` macro called as an expression expands to as one
/// expression, and when a `;` comes after it, buffers the lint at the `;` and drops it.
fn trailing_semicolon(cx: &Cx<'_, '_>, tree: &Tree, call: MacroCallId) -> Option<Emitted> {
    let db = cx.db;
    let loc = call.loc(db);
    let MacroDefKind::Declarative(def, _) = loc.def.kind else { return None };
    let MacroCallKind::FnLike { expand_to, .. } = &loc.kind else { return None };
    let node = loc.to_node(db);
    let mac = ast::MacroCall::cast(node.value.clone())?;
    let is_trailing = is_trailing(&mac);
    let semicolon = match expand_to {
        ExpandTo::Expr => {
            let expanded = call.parse_macro_expansion(db);
            if expanded.err.is_some() || !expanded.value.0.errors().is_empty() {
                return None;
            }
            let semicolon = dropped_semicolon(cx, call)?;
            span_of_token(cx, call, semicolon)
        }
        // The front end expands a block's trailing macro call as statements, where rustc
        // expands it as an expression: the `;` after the first statement's expression is the
        // one rustc would see after the expression it parses.
        ExpandTo::Statements if is_trailing => {
            let first = tree.root.children().next()?;
            let stmt = ast::ExprStmt::cast(first)?;
            stmt.expr()?;
            let semicolon = stmt.semicolon_token()?;
            cx.span(InFile::new(tree.file, semicolon.text_range()))
        }
        _ => return None,
    };
    let lint = match hir::Crate::from(loc.def.krate) == cx.krate {
        true => "semicolon_in_expressions_from_macros",
        false => "semicolon_in_expressions_from_non_local_macros",
    };
    let mut diag = Diag::lint(lint, "trailing semicolon in macro used in expression position", semicolon);
    if is_trailing {
        let name = def.to_node(db).name().map(|it| it.text().to_string())?;
        diag.note("macro invocations at the end of a block are treated as expressions");
        diag.note(format!(
            "to ignore the value produced by the macro, add a semicolon after the invocation of `{name}`"
        ));
    }
    let at = enclosing(cx, node);
    source_fact(cx, &mut diag, at);
    Some(Emitted::new(diag, Phase::Resolve, Pass::Buffered, at))
}

/// Whether rustc's `flat_map_stmt` expands `call` with `is_trailing_mac`: it is the trailing
/// expression of a block, not in braces (`parse_stmt_mac` makes `m! {}` a statement).
fn is_trailing(call: &ast::MacroCall) -> bool {
    let Some(expr) = call.syntax().parent().and_then(ast::MacroExpr::cast) else { return false };
    let Some(list) = expr.syntax().parent().and_then(ast::StmtList::cast) else { return false };
    let braced = call.token_tree().and_then(|it| it.left_delimiter_token()).is_some_and(|it| it.kind() == T!['{']);
    !braced && list.tail_expr().is_some_and(|it| it.syntax() == expr.syntax())
}

/// The `;` the front end drops from the end of what the `macro_rules!` call `call` expands to
/// as an expression (`without_last_semicolon` in `crates/hir-expand/src/lib.rs`), with its
/// span, from the macro's transcription.
fn dropped_semicolon(cx: &Cx<'_, '_>, call: MacroCallId) -> Option<span::Span> {
    let db = cx.db;
    let loc = call.loc(db);
    let MacroDefKind::Declarative(def, _) = loc.def.kind else { return None };
    let (arg, _, span) = call.macro_arg_considering_derives(db, &loc.kind);
    let expanded = def.decl_macro_expander(db, loc.def.krate).expand(db, arg, call, *span);
    if expanded.err.is_some() {
        return None;
    }
    match expanded.value.0.token_trees().iter().last()? {
        hir_expand::tt::TtElement::Leaf(hir_expand::tt::Leaf::Punct(punct)) if punct.char == ';' => {
            Some(punct.span)
        }
        _ => None,
    }
}

/// [`Cx::span`] of a token of the expansion of `call` by its span: where it is written when it
/// is of the source the call was handed; where the crate's own macro writes it, inside the
/// expansion; else the call of the other crate's macro.
fn span_of_token(cx: &Cx<'_, '_>, call: MacroCallId, token: span::Span) -> Span {
    let db = cx.db;
    let range = hir_expand::resolve_span(db, token);
    let written = cx.span_in_file(range.file_id.file_id(db), range.range);
    if token.ctx.is_root() {
        return written;
    }
    let expansion = cx.expansion_of(call).map(Box::new);
    let loc = call.loc(db);
    if hir::Crate::from(loc.def.krate) == cx.krate {
        return Span { expansion, ..written };
    }
    let at = match &expansion {
        Some(it) => Span { expansion: None, ..it.call_site.clone() },
        None => {
            let at = loc.kind.original_call_range(db, loc.krate);
            cx.span_in_file(at.file_id.file_id(db), at.range)
        }
    };
    Span { expansion, ..at }
}

/// `incomplete_include`: rustc parses what `include!` called as an expression or a statement
/// reads as one expression (`ExpandInclude::make_expr`), and buffers the lint at the first
/// token of the file after it.
fn incomplete_include(cx: &Cx<'_, '_>, tree: &Tree, call: MacroCallId) -> Option<Emitted> {
    let db = cx.db;
    let loc = call.loc(db);
    if !loc.def.is_include() {
        return None;
    }
    let MacroCallKind::FnLike { expand_to, .. } = &loc.kind else { return None };
    let first = tree.root.children().next();
    let expr = match expand_to {
        ExpandTo::Expr if ast::Expr::can_cast(tree.root.kind()) => tree.root.clone(),
        ExpandTo::Expr => first.filter(|it| ast::Expr::can_cast(it.kind()))?,
        ExpandTo::Statements => match first? {
            it if ast::Expr::can_cast(it.kind()) => it,
            it => ast::ExprStmt::cast(it)?.expr()?.syntax().clone(),
        },
        _ => return None,
    };
    let file = loc.include_file_id(db, call)?;
    let (read, _) = InFile::new(tree.file, expr.text_range()).original_node_file_range_opt(db)?;
    if read.file_id != file {
        return None;
    }
    let source = cx.sema.parse(file);
    let rest = next_token(source.syntax(), read.range.end())?;
    let span = cx.span_in_file(file.file_id(db), rest.text_range());
    let mut diag = Diag::lint("incomplete_include", "include macro expected single expression in source", span);
    let at = enclosing(cx, loc.to_node(db));
    source_fact(cx, &mut diag, at);
    Some(Emitted::new(diag, Phase::Resolve, Pass::Buffered, at))
}

/// The first token of `root` from `offset` that rustc's lexer gives: not white space nor a
/// comment, but a doc comment.
fn next_token(root: &SyntaxNode, offset: TextSize) -> Option<SyntaxToken> {
    root.descendants_with_tokens().filter_map(NodeOrToken::into_token).find(|token| {
        if token.text_range().start() < offset {
            return false;
        }
        match ast::Comment::cast(token.clone()) {
            Some(comment) => comment.kind().doc.is_some(),
            None => !token.kind().is_trivia(),
        }
    })
}

/// `duplicate_macro_attributes`: as rustc expands the builtin attribute `#[test]`, `#[bench]`,
/// `#[test_case]` or `#[cfg_eval]`, the first other attribute of that name on the item it is on
/// (`warn_on_duplicate_attribute`).
fn duplicate_attribute(cx: &Cx<'_, '_>, walk: &Walk<'_>, call: MacroCallId) -> Option<Emitted> {
    let db = cx.db;
    let loc = call.loc(db);
    let MacroDefKind::BuiltInAttr(_, expander) = loc.def.kind else { return None };
    let name = match expander {
        BuiltinAttrExpander::Test => "test",
        BuiltinAttrExpander::Bench => "bench",
        BuiltinAttrExpander::TestCase => "test_case",
        BuiltinAttrExpander::CfgEval => "cfg_eval",
        _ => return None,
    };
    let MacroCallKind::Attr { ast_id, censored_attr_ids, .. } = &loc.kind else { return None };
    let node = loc.to_node(db);
    // rustc does not look for it on an item that is a statement.
    if node.value.parent().is_some_and(|it| matches!(it.kind(), SyntaxKind::STMT_LIST | SyntaxKind::MACRO_STMTS)) {
        return None;
    }
    let owner = ast::AnyHasAttrs::cast(node.value.clone())?;
    let (_, invoked) = censored_attr_ids.invoc_attr().find_attr_range(db, loc.krate, *ast_id);
    let invoked = invoked.syntax().text_range();
    let (meta, attr) = walk.attrs(&owner).into_iter().find(|(meta, _)| {
        meta.syntax().text_range() != invoked && meta.simple_name().is_some_and(|it| it.as_str() == name)
    })?;
    // An attribute `cfg_attr` gives is spanned by what `cfg_attr` holds of it.
    let range = match meta.syntax().parent().is_some_and(|it| it == *attr.syntax()) {
        true => attr.syntax().text_range(),
        false => meta.syntax().text_range(),
    };
    let mut diag = Diag::lint("duplicate_macro_attributes", "duplicated attribute", cx.span(node.with_value(range)));
    let at = enclosing(cx, node);
    source_fact(cx, &mut diag, at);
    Some(Emitted::new(diag, Phase::Resolve, Pass::Buffered, at))
}

/// The node rustc's expansion buffers a lint of the macro call or attribute macro `node` at
/// (`lint_node_id`): the node around the call, not the call itself (it is replaced), and for
/// a call at the top of an expansion, the node around the call that expansion is of.
fn enclosing(cx: &Cx<'_, '_>, node: InFile<SyntaxNode>) -> InFile<SyntaxNodePtr> {
    let mut node = node;
    loop {
        let parent = node.value.parent();
        match parent {
            Some(parent) if matches!(parent.kind(), SyntaxKind::MACRO_EXPR | SyntaxKind::MACRO_PAT | SyntaxKind::MACRO_TYPE) => {
                node = node.with_value(parent);
            }
            Some(parent) if parent.parent().is_some() || node.file_id.macro_file().is_none() => {
                return InFile::new(node.file_id, SyntaxNodePtr::new(&parent));
            }
            _ => match node.file_id.macro_file() {
                Some(call) => node = call.loc(cx.db).to_node(cx.db),
                None => return InFile::new(node.file_id, SyntaxNodePtr::new(&node.value)),
            },
        }
    }
}

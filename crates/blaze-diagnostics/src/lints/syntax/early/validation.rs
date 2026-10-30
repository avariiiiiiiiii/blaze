//! What rustc's AST validation buffers for the early lint pass (`deprecated_where_clause_location`,
//! `missing_abi`, `missing_unsafe_on_extern`, `patterns_in_fns_without_body`,
//! `unused_visibilities`), what its parser buffers (`varargs_without_pattern`,
//! `break_with_label_and_loop`, in [`parser`]), and `non_camel_case_types` of type parameters.
//!
//! `non_camel_case_types` of the other names rustc's `NonCamelCaseTypes` checks (types,
//! traits, trait aliases, associated types, variants) is hir's `IncorrectCase`
//! (`hir_ide::diagnostics::decl_check`), lowered in `crate::lower::incorrect_case`: it checks
//! no type parameter, so each name is linted once.
//!
//! AST validation runs on the crate once it is expanded and `cfg` has removed what it removes,
//! so it sees what [`Walk`] has; each lint is buffered at the node rustc gives it and emitted
//! as the early lint pass comes to that node ([`Pass::Buffered`]).
//!
//! Follows rustc's `compiler/rustc_ast_passes/src/ast_validation.rs` (`visit_item`,
//! `visit_assoc_item`, `visit_fn`, `visit_ty_common`, `handle_missing_abi`,
//! `check_decl_no_pat`, `check_type_alias_where_clause_location`) and `diagnostics.rs`
//! (`MissingAbiSugg`, `MissingUnsafeOnExternLint`, `UnusedVisibility`,
//! `PatternsInFnsWithoutBody`, `DeprecatedWhereClauseLocation`),
//! `compiler/rustc_lint/src/nonstandard_style.rs` (`NonCamelCaseTypes::check_generic_param`)
//! and `lints.rs` (`NonCamelCaseType`), and `compiler/rustc_lint/src/early.rs`
//! (`visit_generic_param`) (MIT or Apache-2.0).

mod parser;
mod pretty;

use hir::{HirFileId, InFile};
use span::{Edition, SyntaxContext};
use syntax::{
    AstNode, SyntaxKind, SyntaxNode, SyntaxNodePtr, TextRange, TextSize,
    ast::{self, HasName as _, HasVisibility as _},
};

use crate::{
    cx::{Cx, item_range},
    lints::syntax::{Emitted, Pass, Tree, Walk, source_fact},
    lower::{
        Phase,
        incorrect_case::{is_upper_camel_case, to_upper_camel_case},
    },
    model::{Applicability, Diag, Span, SuggestionStyle},
};

pub(crate) fn lints(cx: &Cx<'_, '_>, walk: &Walk<'_>) -> Vec<Emitted> {
    let mut out = Vec::new();
    for tree in &walk.trees {
        for node in &tree.nodes {
            validate(cx, tree, node, &mut out);
            if let Some(param) = ast::TypeParam::cast(node.clone()) {
                type_param_case(cx, tree, &param, &mut out);
            }
        }
    }
    out.extend(parser::lints(cx, walk));
    out
}

/// The lints AST validation buffers at `node`, in the order it buffers them.
fn validate(cx: &Cx<'_, '_>, tree: &Tree, node: &SyntaxNode, out: &mut Vec<Emitted>) {
    if let Some(block) = ast::ExternBlock::cast(node.clone()) {
        if block.unsafe_token().is_none() {
            missing_unsafe_on_extern(cx, tree, node, out);
        }
        if let Some(abi) = block.abi() {
            missing_abi(cx, tree, &abi, node, out);
        }
    } else if let Some(item) = ast::Fn::cast(node.clone()) {
        if let Some(abi) = item.abi() {
            missing_abi(cx, tree, &abi, node, out);
        }
        if item.body().is_none() && in_assoc_item_list(node) {
            patterns_without_body(cx, tree, &item, out);
        }
    } else if let Some(ty) = ast::FnPtrType::cast(node.clone()) {
        if let Some(abi) = ty.abi() {
            missing_abi(cx, tree, &abi, node, out);
        }
    } else if let Some(item) = ast::Const::cast(node.clone()) {
        if !in_assoc_item_list(node) {
            unused_visibility(cx, tree, &item, out);
        }
    } else if let Some(alias) = ast::TypeAlias::cast(node.clone())
        && in_assoc_item_list(node)
    {
        where_clause_location(cx, tree, &alias, out);
    }
}

/// Whether `node` is an item of a trait or an impl.
fn in_assoc_item_list(node: &SyntaxNode) -> bool {
    node.parent().is_some_and(|it| it.kind() == SyntaxKind::ASSOC_ITEM_LIST)
}

/// The lint `diag` found at `read` (whose source it records it read), its level decided at
/// `at`.
fn emit(
    cx: &Cx<'_, '_>,
    mut diag: Diag,
    pass: Pass,
    at: InFile<SyntaxNodePtr>,
    read: InFile<SyntaxNodePtr>,
    out: &mut Vec<Emitted>,
) {
    source_fact(cx, &mut diag, read);
    out.push(Emitted::new(diag, Phase::Resolve, pass, at));
}

/// The syntax context of the token at `offset` of `file`: rustc's `Span::ctxt` of it, which
/// says which expansion made it and in which edition.
fn ctxt_at(cx: &Cx<'_, '_>, file: HirFileId, offset: TextSize) -> SyntaxContext {
    match file.macro_file() {
        None => SyntaxContext::root(file.edition(cx.db)),
        Some(call) => call.expansion_span_map(cx.db).span_at(offset).ctx,
    }
}

/// rustc's `Span::edition` of the token at `offset` of `file`.
fn edition_at(cx: &Cx<'_, '_>, file: HirFileId, offset: TextSize) -> Edition {
    ctxt_at(cx, file, offset).edition(cx.db)
}

fn span(cx: &Cx<'_, '_>, tree: &Tree, range: TextRange) -> Span {
    cx.span(InFile::new(tree.file, range))
}

/// `missing_unsafe_on_extern`: an `extern` block not marked `unsafe`, before Rust 2024 (in
/// 2024 it is the error `extern blocks must be unsafe`).
fn missing_unsafe_on_extern(cx: &Cx<'_, '_>, tree: &Tree, node: &SyntaxNode, out: &mut Vec<Emitted>) {
    let range = item_range(node);
    if edition_at(cx, tree.file, range.start()).at_least_2024() {
        return;
    }
    let span = span(cx, tree, range);
    let mut diag = Diag::lint("missing_unsafe_on_extern", "extern blocks should be unsafe", span.clone());
    diag.suggestion(
        span.shrink_to_lo(),
        "needs `unsafe` before the extern keyword",
        "unsafe ",
        Applicability::MachineApplicable,
    );
    emit(cx, diag, Pass::Buffered, tree.ptr(node), tree.ptr(node), out);
}

/// `missing_abi`: `extern` with no ABI, on an `extern` block, a function or a function pointer
/// type `at`. rustc skips an `extern` whose span is an attribute's (a proc macro's tokens).
fn missing_abi(cx: &Cx<'_, '_>, tree: &Tree, abi: &ast::Abi, at: &SyntaxNode, out: &mut Vec<Emitted>) {
    if abi.string_token().is_some() {
        return;
    }
    let Some(extern_) = abi.extern_token() else { return };
    let span = span(cx, tree, extern_.text_range());
    if !cx.snippet(&span).is_some_and(|it| !it.starts_with("#[")) {
        return;
    }
    let mut diag =
        Diag::lint("missing_abi", "`extern` declarations without an explicit ABI are deprecated", span.clone());
    diag.suggestion(span, "explicitly specify the \"C\" ABI", "extern \"C\"", Applicability::MachineApplicable);
    emit(cx, diag, Pass::Buffered, tree.ptr(at), tree.ptr(at), out);
}

/// `patterns_in_fns_without_body`: a `mut x` parameter of an associated function without a
/// body (any other pattern there is the error E0642, of a foreign function E0130).
fn patterns_without_body(cx: &Cx<'_, '_>, tree: &Tree, item: &ast::Fn, out: &mut Vec<Emitted>) {
    let Some(params) = item.param_list() else { return };
    let mut found: Vec<(TextRange, String)> = Vec::new();
    if let Some(it) = params.self_param()
        && it.amp_token().is_none()
        && let (Some(mut_), Some(name)) = (it.mut_token(), it.name())
    {
        let range = TextRange::new(mut_.text_range().start(), name.syntax().text_range().end());
        found.push((range, name.text().to_owned()));
    }
    for param in params.params() {
        let Some(ast::Pat::IdentPat(pat)) = param.pat() else { continue };
        if pat.mut_token().is_none() || pat.ref_token().is_some() || pat.pat().is_some() {
            continue;
        }
        let Some(name) = pat.name() else { continue };
        found.push((pat.syntax().text_range(), name.text().to_owned()));
    }
    for (range, name) in found {
        let span = span(cx, tree, range);
        let edition = edition_at(cx, tree.file, range.start());
        let mut diag =
            Diag::lint("patterns_in_fns_without_body", "patterns aren't allowed in functions without bodies", span.clone());
        diag.suggestion(
            span,
            "remove `mut` from the parameter",
            pretty::ident(&name, edition),
            Applicability::MachineApplicable,
        );
        emit(cx, diag, Pass::Buffered, tree.ptr(item.syntax()), tree.ptr(item.syntax()), out);
    }
}

/// `unused_visibilities`: a visibility on `const _`, when it and the `_` are of one syntax
/// context (rustc's `eq_ctxt`: not a `pub` of a macro on a `_` its caller gave).
fn unused_visibility(cx: &Cx<'_, '_>, tree: &Tree, item: &ast::Const, out: &mut Vec<Emitted>) {
    let Some(underscore) = item.underscore_token() else { return };
    let Some(vis) = item.visibility() else { return };
    let vis_range = vis.syntax().text_range();
    if ctxt_at(cx, tree.file, underscore.text_range().start()) != ctxt_at(cx, tree.file, vis_range.start()) {
        return;
    }
    let span = span(cx, tree, vis_range);
    let mut diag =
        Diag::lint("unused_visibilities", "visibility qualifiers have no effect on `const _` declarations", span.clone());
    diag.note("`const _` does not declare a name, so there is nothing for the qualifier to apply to");
    diag.suggestion_with_style(
        vec![(span, String::new())],
        "remove the qualifier",
        Applicability::MachineApplicable,
        SuggestionStyle::HideCodeInline,
    );
    emit(cx, diag, Pass::Buffered, tree.ptr(item.syntax()), tree.ptr(item.syntax()), out);
}

/// `deprecated_where_clause_location`: an associated type with a `where` clause before its
/// `= Type`, with rustc's suggestion to move its predicates after the type (or, when there are
/// none and a `where` is already there, to remove it).
fn where_clause_location(cx: &Cx<'_, '_>, tree: &Tree, alias: &ast::TypeAlias, out: &mut Vec<Emitted>) {
    let (Some(eq), Some(ty)) = (alias.eq_token(), alias.ty()) else { return };
    let eq = eq.text_range();
    let clauses: Vec<ast::WhereClause> = alias.syntax().children().filter_map(ast::WhereClause::cast).collect();
    let Some(before) = clauses.iter().find(|it| it.syntax().text_range().end() <= eq.start()) else { return };
    let after = clauses.iter().find(|it| it.syntax().text_range().start() >= eq.end());
    let range = before.syntax().text_range();
    let span = span(cx, tree, range);
    let mut diag = Diag::lint("deprecated_where_clause_location", "where clause not allowed here", span.clone());
    diag.note("see issue #89122 <https://github.com/rust-lang/rust/issues/89122> for more information");
    let predicates: Vec<ast::WherePred> = before.predicates().collect();
    if !predicates.is_empty() || after.is_none() {
        let edition = edition_at(cx, tree.file, range.start());
        let mut needs_comma = after.is_some_and(|it| it.predicates().next().is_some());
        let mut moved = String::new();
        match after {
            None => moved.push_str(" where "),
            Some(_) if !needs_comma => moved.push(' '),
            Some(_) => {}
        }
        for predicate in &predicates {
            if needs_comma {
                moved.push_str(", ");
            }
            needs_comma = true;
            moved.push_str(&pretty::where_predicate(predicate, edition));
        }
        // rustc's printer writes a break it ends on as nothing.
        let moved = moved.trim_end().to_owned();
        let end = after.map_or(ty.syntax().text_range().end(), |it| it.syntax().text_range().end());
        let right = cx.span(InFile::new(tree.file, TextRange::empty(end)));
        diag.multipart_suggestion(
            "move it to the end of the type declaration",
            vec![(extended_back_over_white_space(cx, &span), String::new()), (right, moved)],
            Applicability::MachineApplicable,
        );
    } else {
        diag.suggestion(span, "remove this `where`", "", Applicability::MachineApplicable);
    }
    emit(cx, diag, Pass::Buffered, tree.ptr(alias.syntax()), tree.ptr(alias.syntax()), out);
}

/// rustc's `span_extend_prev_while(span, char::is_whitespace)`: `span` with the white space
/// before it.
fn extended_back_over_white_space(cx: &Cx<'_, '_>, span: &Span) -> Span {
    let Some(file) = cx.file_of(span) else { return span.clone() };
    let Some(before) = cx.file_text(file).get(..span.lo as usize) else { return span.clone() };
    let lo = before.trim_end_matches(char::is_whitespace).len();
    span.with_lo(lo as u32)
}

/// `non_camel_case_types` of a type parameter, which rustc checks as its early lint pass comes
/// to it (`check_generic_param`), its level decided by the parameter's attributes.
fn type_param_case(cx: &Cx<'_, '_>, tree: &Tree, param: &ast::TypeParam, out: &mut Vec<Emitted>) {
    let Some(name) = param.name() else { return };
    let written = name.text();
    let name_str = written.strip_prefix("r#").unwrap_or(written);
    if is_upper_camel_case(name_str) {
        return;
    }
    let span = span(cx, tree, name.syntax().text_range());
    let mut diag = Diag::lint(
        "non_camel_case_types",
        format!("type parameter `{name_str}` should have an upper camel case name"),
        span.clone(),
    );
    let camel = to_upper_camel_case(name_str);
    if camel != name_str {
        diag.suggestion(span, "convert the identifier to upper camel case", camel, Applicability::MaybeIncorrect);
    } else {
        diag.span_label(span, "should have an UpperCamelCase name");
    }
    emit(cx, diag, Pass::Early, tree.ptr(param.syntax()), tree.ptr(param.syntax()), out);
}

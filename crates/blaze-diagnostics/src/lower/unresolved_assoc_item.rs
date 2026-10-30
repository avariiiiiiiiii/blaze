//! E0599 of a path: `T::f` (an expression, or a pattern `T::f(..)` / `T::C`) where no
//! associated function, constant or variant `f` is found for `T`. With rustc's labels: "not
//! found in `T`", the item of a similar name, the traits that would give it, and where a local
//! type is defined.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/method/suggest.rs`
//! (`report_no_match_method_error` with `Mode::Path`, `create_no_assoc_err`,
//! `set_not_found_span_label`, `find_likely_intended_associated_item`, `suggest_traits_to_import`)
//! and `compiler/rustc_hir_typeck/src/fn_ctxt/_impl.rs` (`resolve_ty_and_res_fully_qualified_call`)
//! (MIT or Apache-2.0).

use either::Either;
use hir::{InFile, PathResolution};
use hir_expand::name::AsName as _;
use syntax::{AstNode, TextRange, ast};

use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{
        Found, Phase,
        unresolved_method_call::{
            NoMatch, adt_fact, likely_intended, not_found_for_this, not_found_label, similar_candidate,
            traits_to_import,
        },
    },
    model::{Diag, Span},
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::UnresolvedAssocItem) -> Vec<Found> {
    let node = cx.node(d.expr_or_pat);
    let at = |range: TextRange| cx.span(InFile::new(node.file_id, range));
    let path = path_of(&node.value);
    let name_ref = path.as_ref().and_then(|it| it.segment()?.name_ref());
    let (Some(path), Some(name_ref)) = (path, name_ref) else {
        let written = node.value.as_ref().either(|it| it.syntax().to_string(), |it| it.syntax().to_string());
        let (qualifier, name) = written.rsplit_once("::").unwrap_or(("", written.as_str()));
        return vec![unknown(cx, d, cx.span_of_ptr(d.expr_or_pat), qualifier, name)];
    };
    let name_span = at(name_ref.syntax().text_range());
    let qualifier = path.qualifier();
    let Some(receiver) = qualifier.as_ref().and_then(|it| qualifier_ty(cx, it)) else {
        let written = qualifier.map(|it| it.syntax().to_string()).unwrap_or_default();
        return vec![unknown(cx, d, name_span, &written, name_ref.text())];
    };
    let args = match &node.value {
        Either::Left(expr) => callee_args(expr),
        Either::Right(_) => None,
    };
    let no_match = NoMatch {
        receiver,
        name: name_ref.as_name(),
        name_span,
        sugg_span: at(path.syntax().text_range()),
        scope: cx.sema.scope(path.syntax()),
        is_method: false,
        args,
    };
    let item_kind = no_match.item_kind();
    let mut diag = no_match.error(cx, item_kind);
    let similar = similar_candidate(cx, &no_match);
    if similar.is_none() {
        not_found_label(cx, &mut diag, &no_match, item_kind);
    }
    traits_to_import(cx, &mut diag, &no_match, &[]);
    if let Some(similar) = similar
        && no_match.name_span.expansion.is_none()
    {
        likely_intended(cx, &mut diag, &no_match, similar);
    }
    not_found_for_this(cx, &mut diag, &no_match, item_kind);

    let name = no_match.name();
    let ty = cx.ty(&no_match.receiver);
    diag.fact_opt(cx.method_fact(&no_match.receiver, &name), format!("what `{name}` is on `{ty}`"));
    if let Some(adt) = no_match.receiver.as_adt() {
        adt_fact(cx, &mut diag, adt);
    }
    code_fact(cx, &mut diag, d.expr_or_pat.map(|it| it.syntax_node_ptr()));
    vec![Found::new(diag, Phase::Bodies)]
}

/// The path of a path expression or pattern.
fn path_of(node: &Either<ast::Expr, ast::Pat>) -> Option<ast::Path> {
    match node {
        Either::Left(ast::Expr::PathExpr(it)) => it.path(),
        Either::Left(ast::Expr::RecordExpr(it)) => it.path(),
        Either::Right(ast::Pat::PathPat(it)) => it.path(),
        Either::Right(ast::Pat::TupleStructPat(it)) => it.path(),
        Either::Right(ast::Pat::RecordPat(it)) => it.path(),
        _ => None,
    }
}

/// The number of arguments when the path is the callee of a call.
fn callee_args(expr: &ast::Expr) -> Option<usize> {
    let call = expr.syntax().parent().and_then(ast::CallExpr::cast)?;
    let callee = call.expr()?;
    (callee.syntax() == expr.syntax()).then(|| call.arg_list().map_or(0, |it| it.args().count()))
}

/// The type the qualifier of `T::f` names: `<T>::f`'s `T`, a type, `Self`, an alias or a type
/// parameter.
fn qualifier_ty<'db>(cx: &Cx<'_, 'db>, qualifier: &ast::Path) -> Option<hir::Type<'db>> {
    if let Some(anchor) = qualifier.segment().and_then(|it| it.type_anchor())
        && anchor.as_token().is_none()
    {
        return cx.sema.resolve_type(&anchor.ty()?);
    }
    Some(match cx.sema.resolve_path(qualifier)? {
        PathResolution::Def(hir::ModuleDef::Adt(it)) => it.ty(cx.db),
        PathResolution::Def(hir::ModuleDef::BuiltinType(it)) => it.ty(cx.db),
        PathResolution::Def(hir::ModuleDef::TypeAlias(it)) => it.ty(cx.db),
        PathResolution::SelfType(it) => it.self_ty(cx.db),
        PathResolution::TypeParam(it) => it.ty(cx.db),
        _ => return None,
    })
}

/// E0599 where the type the item was looked for on is not known here: rustc's message with the
/// qualifier as written, so the program is still refused at the name.
fn unknown(cx: &Cx<'_, '_>, d: &hir::UnresolvedAssocItem, span: Span, qualifier: &str, name: &str) -> Found {
    let message = format!("no associated function or constant named `{name}` found for `{qualifier}` in the current scope");
    let mut diag = Diag::error(Some("E0599"), message, span.clone());
    diag.span_label(span, format!("associated function or constant not found in `{qualifier}`"));
    code_fact(cx, &mut diag, d.expr_or_pat.map(|it| it.syntax_node_ptr()));
    Found::new(diag, Phase::Bodies)
}

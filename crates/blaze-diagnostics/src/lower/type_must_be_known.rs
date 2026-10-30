//! E0282: a type inference could not decide. Like rustc, the error points at the `let` whose
//! binding has the undecided type, "type annotations needed" (or "type annotations needed for
//! `Vec<_>`" when the type has a known shape), with "consider giving `x` an explicit type" and
//! its `: Vec<_>` or `: /* Type */`; "type must be known at this point" where type checking
//! needed it; else "cannot infer type" where it was found.
//!
//! Follows rustc's `compiler/rustc_trait_selection/src/error_reporting/infer/
//! need_type_info.rs` (`emit_inference_failure_err`, `bad_inference_failure_err`,
//! `InferSourceKind::ty_localized_msg`, `FindInferSourceVisitor::visit_local`, `ty_to_string`)
//! and `compiler/rustc_trait_selection/src/errors.rs` (`AnnotationRequired`,
//! `InferenceBadError`, `SourceKindSubdiag::LetLike`) (MIT or Apache-2.0). The `let` is found
//! from the syntax around where the front end reports it.
//!
//! What the front end does not give yet (requested in `v2/requests/diagnostics.md`): the
//! generic parameter an undecided type stands for, which rustc names (", where the type for
//! type parameter `T` is specified", "cannot infer type for type parameter `T` declared on
//! the function `f`") and turbofishes (`::<T>`); and the ambiguous goals rustc reports as
//! E0283 and E0284, which the solver's errors drop.

use either::Either;
use hir::{InFile, PathResolution};
use syntax::{AstNode, SyntaxNode, ast};

use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{Found, Phase},
    model::{Applicability, Diag},
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::TypeMustBeKnown<'_>) -> Vec<Found> {
    let at = d.at_point.map(|it| it.syntax_node_ptr());
    let failure = cx.span_of(at);
    let node = cx.syntax(at);
    let mut diag = match (let_source(cx, &node), &d.top_term) {
        (_, Some(Either::Right(konst))) => {
            let mut diag = Diag::error(Some("E0282"), "type annotations needed", failure.clone());
            diag.span_label(failure, format!("cannot infer the value of the constant `{konst}`"));
            diag
        }
        (Some(source), _) => {
            let span = cx.span(source.pat.as_ref().map(|it| it.syntax().text_range()));
            let printed = placeholders(&cx.ty(&source.ty));
            let message = if names_type(&source.ty) {
                format!("type annotations needed for `{printed}`")
            } else {
                "type annotations needed".to_owned()
            };
            let mut diag = Diag::error(Some("E0282"), message, span.clone());
            // Where type checking needed it, when it is not the binding: `structurally_resolve_type`
            // labels it, writeback does not.
            if d.top_term.is_none() && !overlaps(&failure, &span) {
                diag.span_label(failure, "type must be known at this point");
            }
            let name = match &source.pat.value {
                ast::Pat::IdentPat(it) => it.name().map(|it| it.text().to_string()),
                _ => None,
            };
            let message = match name {
                Some(name) => format!("consider giving `{name}` an explicit type"),
                None => "consider giving this pattern a type".to_owned(),
            };
            let type_name = if printed == "_" { "/* Type */".to_owned() } else { printed };
            diag.suggestion_verbose(
                span.shrink_to_hi(),
                message,
                format!(": {type_name}"),
                Applicability::HasPlaceholders,
            );
            diag
        }
        (None, _) => {
            let mut diag = Diag::error(Some("E0282"), "type annotations needed", failure.clone());
            diag.span_label(failure, "cannot infer type");
            diag
        }
    };
    code_fact(cx, &mut diag, at);
    vec![Found::new(diag, Phase::Bodies)]
}

/// A `let` with no type whose pattern's type is not decided: rustc's `InferSourceKind::
/// LetBinding`.
struct LetSource<'db> {
    pat: InFile<ast::Pat>,
    ty: hir::Type<'db>,
}

/// The `let` the undecided type at `node` is of: the binding itself, the local an expression
/// names, or the `let` whose initializer `node` is in.
fn let_source<'db>(cx: &Cx<'_, 'db>, node: &InFile<SyntaxNode>) -> Option<LetSource<'db>> {
    let of_stmt = |stmt: ast::LetStmt, file| {
        if stmt.ty().is_some() {
            return None;
        }
        let pat = stmt.pat()?;
        let ty = cx.sema.type_of_pat(&pat)?.original;
        ty.contains_unknown().then(|| LetSource { pat: InFile::new(file, pat), ty })
    };
    if ast::Pat::can_cast(node.value.kind())
        && let Some(stmt) = node.value.parent().and_then(ast::LetStmt::cast)
    {
        return of_stmt(stmt, node.file_id);
    }
    if let Some(ast::Expr::PathExpr(path)) = ast::Expr::cast(node.value.clone())
        && let Some(PathResolution::Local(local)) = path.path().and_then(|it| cx.sema.resolve_path(&it))
    {
        let source = local.primary_source(cx.db);
        if let Some(stmt) = source.syntax().parent().and_then(ast::LetStmt::cast)
            && let Some(found) = of_stmt(stmt, source.file())
        {
            return Some(found);
        }
    }
    // The node the walk came from is a child of the next, in its file.
    let mut prev = node.value.clone();
    for ancestor in cx.sema.ancestors_with_macros_file(node.clone()).skip(1) {
        let kind = ancestor.value.kind();
        if ast::Item::can_cast(kind) || ast::ClosureExpr::can_cast(kind) {
            break;
        }
        if let Some(stmt) = ast::LetStmt::cast(ancestor.value.clone()) {
            let in_init = stmt
                .initializer()
                .is_some_and(|init| init.syntax().text_range().contains_range(prev.text_range()));
            return if in_init { of_stmt(stmt, ancestor.file_id) } else { None };
        }
        prev = ancestor.value;
    }
    None
}

/// rustc's `ty_localized_msg` "normal": whether the message names the type, which it does not
/// of `_`, a primitive, or an ADT with no type or const arguments.
fn names_type(ty: &hir::Type<'_>) -> bool {
    if ty.is_unknown() || ty.as_builtin().is_some() || ty.is_unit() || ty.is_never() {
        return false;
    }
    match ty.as_adt() {
        Some(_) => ty.type_arguments().next().is_some(),
        None => true,
    }
}

/// rustc's `ty_to_string`: the type with what is not decided as `_`.
fn placeholders(printed: &str) -> String {
    printed.replace("{unknown}", "_").replace("{type error}", "_")
}

fn overlaps(a: &crate::model::Span, b: &crate::model::Span) -> bool {
    a.file == b.file && a.lo < b.hi && b.lo < a.hi
}

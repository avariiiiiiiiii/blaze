//! E0282, E0283 and E0284: a type inference could not decide, or a goal on it that could be met
//! in more than one way. Like rustc, the error points at the best place to annotate: the `let`
//! whose binding has the undecided type, "type annotations needed" (or "type annotations needed
//! for `Vec<_>`" when the type has a known shape), with "consider giving `x` an explicit type,
//! where the type for type parameter `T` is specified" and its `: Vec<T>` or `: /* Type */`; or
//! the path or method whose generic parameter it is, "cannot infer type of the type parameter
//! `T` declared on the function `f`" with "consider specifying a concrete type for the type
//! parameter `T`" and its `::</* Type */>`; "type must be known at this point" where type
//! checking needed it; else "cannot infer type" (or "cannot infer type for type parameter `T`
//! declared on the function `f`") where it was found. An ambiguous goal adds rustc's notes:
//! "multiple `impl`s satisfying `T: Trait` found" on the impls and where clauses that could
//! meet it, "the type must implement `Trait`", "cannot satisfy `..`", and for a goal with
//! nothing to point at "type annotations needed: cannot satisfy `..`" or "cannot normalize
//! `..`".
//!
//! Follows rustc's `compiler/rustc_trait_selection/src/error_reporting/infer/
//! need_type_info.rs` (`emit_inference_failure_err`, `bad_inference_failure_err`,
//! `InferenceDiagnosticsData`, `InferSourceKind::ty_localized_msg`, `FindInferSourceVisitor`'s
//! `visit_local`, `expr_inferred_arg_iter` and `source_cost`, `ty_to_string`),
//! `error_reporting/traits/ambiguity.rs` (`maybe_report_ambiguity`,
//! `annotate_source_of_ambiguity`), `error_reporting/traits/mod.rs` (`to_pretty_impl_header`)
//! and `compiler/rustc_trait_selection/src/diagnostics.rs` (`AnnotationRequired`,
//! `InferenceBadError`, `SourceKindSubdiag`, `SpecifyGenericParamsSuggestion`) (MIT or
//! Apache-2.0). The `let` is found from the syntax around where the front end reports it, the
//! path from where the undecided variable was made.
//!
//! What the front end does not give yet (requested in `v2/requests/diagnostics-infer2-solver.md`):
//! the parameter of each undecided variable but the first, so `: Foo<_, T>` names only one;
//! the other places rustc's visitor weighs (every expression whose type holds the variable), so
//! a path is a source only when its item has that one generic parameter; the cause of an
//! ambiguous goal ("required by a bound in `f`"); the single impl rustc's E0283 hint puts in the
//! `let`; and rustc's "the following types implement trait" help, E0790 and fully qualified
//! path suggestions.

use either::Either;
use hir::{InFile, PathResolution};
use syntax::view::{self as ast, AstNode as _, HasName as _};

use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{Found, Phase},
    model::{Applicability, Diag},
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::TypeMustBeKnown<'_>) -> Vec<Found> {
    let at = d.at_point.map(|it| it.syntax_node_ptr());
    let failure = cx.span_of(at);
    let node = cx.syntax_view(at);
    let mut diag = match (let_source(cx, node), &d.top_term) {
        (_, Some(Either::Right(konst))) => {
            // clone: on the error path, for an error hir found
            let mut diag = Diag::error(Some("E0282"), "type annotations needed", failure.clone());
            // alloc: the error's text, for an error hir found
            diag.span_label(failure, format!("cannot infer the value of the constant `{konst}`"));
            diag
        }
        (Some(source), _) => {
            let span = cx.span(source.pat.map(|it| it.syntax().text_range()));
            let printed = placeholders(&cx.ty(&source.ty));
            let message = if names_type(&source.ty) {
                // alloc: the error's text, for an error hir found
                format!("type annotations needed for `{printed}`")
            } else {
                // alloc: the error's text, for an error hir found
                "type annotations needed".to_owned()
            };
            // clone: on the error path, for an error hir found
            let mut diag = Diag::error(Some("E0282"), message, span.clone());
            // Where type checking needed it, when it is not the binding: `structurally_resolve_type`
            // labels it, writeback does not.
            if d.top_term.is_none() && !overlaps(&failure, &span) {
                diag.span_label(failure, "type must be known at this point");
            }
            let name = match source.pat.value {
                ast::Pat::IdentPat(it) => it.name().map(|it| it.text()),
                _ => None,
            };
            let message = match name {
                // alloc: the error's text, for an error hir found
                Some(name) => format!("consider giving `{name}` an explicit type"),
                // alloc: the error's text, for an error hir found
                None => "consider giving this pattern a type".to_owned(),
            };
            // alloc: the error's text, for an error hir found
            let type_name = if printed == "_" { "/* Type */".to_owned() } else { printed };
            diag.suggestion_verbose(
                span.shrink_to_hi(),
                message,
                // alloc: the error's text, for an error hir found
                format!(": {type_name}"),
                Applicability::HasPlaceholders,
            );
            diag
        }
        (None, _) => {
            // clone: on the error path, for an error hir found
            let mut diag = Diag::error(Some("E0282"), "type annotations needed", failure.clone());
            diag.span_label(failure, "cannot infer type");
            diag
        }
    };
    code_fact(cx, &mut diag, at);
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Bodies)]
}

/// A `let` with no type whose pattern's type is not decided: rustc's `InferSourceKind::
/// LetBinding`.
struct LetSource<'db> {
    pat: InFile<ast::Pat<'db>>,
    ty: hir::Type<'db>,
}

/// The `let` the undecided type at `node` is of: the binding itself, the local an expression
/// names, or the `let` whose initializer `node` is in.
fn let_source<'db>(cx: &Cx<'_, 'db>, node: InFile<ast::Node<'db>>) -> Option<LetSource<'db>> {
    let of_stmt = |stmt: ast::LetStmt<'db>, file| {
        if stmt.ty().is_some() {
            return None;
        }
        let pat = stmt.pat()?;
        let ty = cx.vsema.type_of_pat(pat)?.original;
        ty.contains_unknown().then(|| LetSource { pat: InFile::new(file, pat), ty })
    };
    if ast::Pat::can_cast(node.value.kind())
        && let Some(stmt) = node.value.parent().and_then(ast::LetStmt::cast)
    {
        return of_stmt(stmt, node.file_id);
    }
    if let Some(ast::Expr::PathExpr(path)) = ast::Expr::cast(node.value)
        && let Some(PathResolution::Local(local)) = path.path().and_then(|it| cx.vsema.resolve_path(it))
    {
        let source = local.primary_source_view(cx.db);
        // The local's tree is given out by `vsema`, so `of_stmt` may ask it of the pattern.
        cx.root(source.file());
        if let Some(stmt) = source.syntax().parent().and_then(ast::LetStmt::cast)
            && let Some(found) = of_stmt(stmt, source.file())
        {
            return Some(found);
        }
    }
    // The node the walk came from is a child of the next, in its file.
    let mut prev = node.value;
    for ancestor in cx.vsema.ancestors_with_macros_file(node).skip(1) {
        let kind = ancestor.value.kind();
        if ast::Item::can_cast(kind) || ast::ClosureExpr::can_cast(kind) {
            break;
        }
        if let Some(stmt) = ast::LetStmt::cast(ancestor.value) {
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

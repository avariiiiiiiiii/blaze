//! E0525: a closure that implements fewer of `Fn`, `FnMut` and `FnOnce` than the bound it is
//! given to asks for, labelled at the closure's head, at the use that made it `FnOnce` or
//! `FnMut`, and at the argument the requirement comes from, with the note of the callee's
//! bound.
//!
//! Follows rustc's `compiler/rustc_trait_selection/src/error_reporting/traits/
//! fulfillment_errors.rs` (`report_closure_error`), `compiler/rustc_trait_selection/src/
//! errors.rs` (`ClosureKindMismatch`, `ClosureFnOnceLabel`, `ClosureFnMutLabel`) and
//! `compiler/rustc_hir_typeck/src/upvar.rs` (`closure_kind_origins`) (MIT or Apache-2.0).
//!
//! The front end gives neither the closure (found here from the argument: a closure written
//! there, or a local a `let` binds one to) nor rustc's `closure_kind_origins` (here the first
//! capture that moves, or that mutates, and its first use; only of a closure that is not
//! `move`, whose captures all move). Both are requested in `v2/requests/diagnostics.md`.

use hir::{FnTrait, InFile, LangItem, PathResolution};
use syntax::{AstNode, SyntaxNode, TextRange, ast};

use crate::{
    cx::{Cx, item_range},
    lints::code_fact,
    lower::{
        Found, Phase,
        unimplemented_trait::{Cause, where_clause_note},
    },
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::ClosureKindMismatch) -> Vec<Found> {
    let (expected, found) = (d.expected, d.found);
    let at = cx.syntax(d.span.map(|it| it.syntax_node_ptr()));
    let cause = Cause::of(cx, &at, |ty| ty.as_closure().is_some_and(|it| it.fn_trait(cx.db) == found));
    let closure = closure_of(cx, &cause.node);
    let head = closure.as_ref().map(|it| cx.span(it.with_value(head_range(&it.value))));
    let mut diag = Diag::error(
        Some("E0525"),
        format!(
            "expected a closure that implements the `{expected}` trait, but this closure only \
             implements `{found}`"
        ),
        head.clone().unwrap_or_else(|| cause.span.clone()),
    );
    if let Some(head) = head {
        diag.span_label(head, format!("this closure implements `{found}`, not `{expected}`"));
    }
    if let Some(closure) = &closure
        && let Some((span, place)) = origin(cx, closure, found)
    {
        let label = match found {
            FnTrait::FnOnce | FnTrait::AsyncFnOnce => {
                format!("closure is `{found}` because it moves the variable `{place}` out of its environment")
            }
            _ => format!("closure is `{found}` because it mutates the variable `{place}` here"),
        };
        diag.span_label(span, label);
    }
    diag.span_label(cause.span.clone(), format!("the requirement to implement `{expected}` derives from here"));
    cause.label_call(&mut diag);
    if let Some(callee) = cause.callee
        && let Some(trait_) = hir::Trait::lang(cx.db, cx.krate, lang_item(expected))
    {
        where_clause_note(cx, &mut diag, callee, trait_, None);
    }
    code_fact(cx, &mut diag, d.span.map(|it| it.syntax_node_ptr()));
    vec![Found::new(diag, Phase::Bodies)]
}

fn lang_item(kind: FnTrait) -> LangItem {
    match kind {
        FnTrait::Fn => LangItem::Fn,
        FnTrait::FnMut => LangItem::FnMut,
        FnTrait::FnOnce => LangItem::FnOnce,
        FnTrait::AsyncFn => LangItem::AsyncFn,
        FnTrait::AsyncFnMut => LangItem::AsyncFnMut,
        FnTrait::AsyncFnOnce => LangItem::AsyncFnOnce,
    }
}

/// The closure an argument is: written there, or the closure a `let` binds the local it names
/// to.
fn closure_of(cx: &Cx<'_, '_>, node: &InFile<SyntaxNode>) -> Option<InFile<ast::ClosureExpr>> {
    let mut expr = ast::Expr::cast(node.value.clone())?;
    while let ast::Expr::ParenExpr(inner) = &expr {
        expr = inner.expr()?;
    }
    match expr {
        ast::Expr::ClosureExpr(it) => Some(node.with_value(it)),
        ast::Expr::PathExpr(path) => {
            let PathResolution::Local(local) = cx.sema.resolve_path(&path.path()?)? else {
                return None;
            };
            let source = local.primary_source(cx.db);
            let pat = source.as_ident_pat()?.clone();
            let file = source.file();
            let stmt = pat.syntax().parent().and_then(ast::LetStmt::cast)?;
            match stmt.initializer()? {
                ast::Expr::ClosureExpr(it) => Some(InFile::new(file, it)),
                _ => None,
            }
        }
        _ => None,
    }
}

/// rustc's `def_span` of a closure: from `move` or `async` to the end of its parameters, or
/// of its return type when it writes one.
fn head_range(closure: &ast::ClosureExpr) -> TextRange {
    let start = item_range(closure.syntax()).start();
    let end = closure
        .ret_type()
        .map(|it| it.syntax().text_range().end())
        .or_else(|| closure.param_list().map(|it| it.syntax().text_range().end()))
        .unwrap_or_else(|| closure.syntax().text_range().end());
    TextRange::new(start, end)
}

/// rustc's `closure_kind_origins` of a closure that is not `move`: the first use of the first
/// capture that moves (of an `FnOnce` closure) or mutates (of an `FnMut` one), and the place.
fn origin(
    cx: &Cx<'_, '_>,
    closure: &InFile<ast::ClosureExpr>,
    found: FnTrait,
) -> Option<(crate::model::Span, String)> {
    if closure.value.move_token().is_some() {
        return None;
    }
    let ty = cx.sema.type_of_expr(&ast::Expr::ClosureExpr(closure.value.clone()))?.original;
    let captures = ty.as_closure()?.captured_items(cx.db);
    let capture = captures.iter().find(|it| match found {
        FnTrait::FnOnce | FnTrait::AsyncFnOnce => it.kind() == hir::CaptureKind::Move,
        FnTrait::FnMut | FnTrait::AsyncFnMut => matches!(
            it.kind(),
            hir::CaptureKind::MutableRef | hir::CaptureKind::UniqueSharedRef
        ),
        FnTrait::Fn | FnTrait::AsyncFn => false,
    })?;
    let usage = capture.usages().sources(cx.db).into_iter().next()?;
    let span = cx.span_of(InFile::new(usage.file_id(), usage.source().syntax_node_ptr()));
    Some((span, capture.display_place_source_code(cx.db, cx.edition)))
}

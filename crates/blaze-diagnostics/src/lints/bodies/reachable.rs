//! `unreachable_code`: what type checking finds after an expression that diverges.
//!
//! Follows `compiler/rustc_hir_typeck/src/fn_ctxt/_impl.rs` (`warn_if_unreachable`: "unreachable
//! {kind}" at what is unreachable, "any code following this expression is unreachable" or the
//! custom note at what diverged) and its callers in `expr.rs`, `_match.rs` and
//! `fn_ctxt/checks.rs`, which give the kind, the span and the node of each, and
//! `compiler/rustc_mir_transform/src/lint_and_remove_uninhabited.rs` with its
//! `UnreachableDueToUninhabited` of `diagnostics.rs` (MIT or Apache-2.0). Which code is
//! unreachable is type checking's (`InferenceResult::unreachable`, hir-ty's port of rustc's
//! `Diverges`) and MIR lowering's (`MirBody::unreachable_after_calls`).

use hir::InFile;
use hir_def::hir::{Expr, ExprId, ExprOrPatId, Statement};
use hir_ty::{Unreachable, UnreachableAt};
use syntax::{AstNode, SyntaxNodePtr, ast};

use super::BodyCx;
use crate::{
    lower::{Found, Phase},
    model::{Diag, Span},
};

/// The lints this module gives: it runs only when one may fire.
pub(super) const LINTS: super::LintSet = super::lint_set(["unreachable_code"]);

pub(super) fn lints(b: &BodyCx<'_, '_>) -> Vec<Found> {
    b.infer.unreachable().iter().filter_map(|it| lint(b, it)).collect()
}

/// `LintAndRemoveUninhabited`, in `mir_promoted`: what follows a call of a type that is
/// uninhabited but not `!`, which type checking does not see (`MirBody::unreachable_after_calls`).
/// None when the call is in a macro of another crate.
pub(super) fn after_calls(b: &BodyCx<'_, '_>) -> Vec<Found> {
    if b.mir_tainted() {
        return Vec::new();
    }
    let Some(mir) = b.mir() else { return Vec::new() };
    let mut found = Vec::new();
    for it in &mir.unreachable_after_calls {
        let (Some(call), Some(next)) = (b.mir_node(it.call), b.mir_node(it.next)) else { continue };
        if b.in_external_macro(call) {
            continue;
        }
        let (Some(call_span), Some(next_span)) = (b.mir_span(it.call), b.mir_span(it.next)) else { continue };
        let message = format!("unreachable {}", it.descr);
        let mut diag = Diag::lint("unreachable_code", message.clone(), next_span.clone());
        diag.span_label(next_span, message);
        diag.span_label(call_span.clone(), "any code following this expression is unreachable");
        diag.span_note(call_span, format!("this expression has type `{}`, which is uninhabited", b.ty(it.ty.as_ref())));
        found.push(b.lint(diag, Phase::Mir, next));
    }
    found
}

fn lint(b: &BodyCx<'_, '_>, unreachable: &Unreachable) -> Option<Found> {
    let (kind, span, at) = place(b, &unreachable.what)?;
    let after = match unreachable.after {
        ExprOrPatId::ExprId(expr) => b.expr_span(expr)?,
        ExprOrPatId::PatId(pat) => b.pat_span(pat)?,
    };
    let message = format!("unreachable {kind}");
    let mut diag = Diag::lint("unreachable_code", message.clone(), span.clone());
    diag.span_label(span, message);
    let note = unreachable.note.unwrap_or("any code following this expression is unreachable");
    diag.span_label(after, note);
    Some(b.lint(diag, Phase::Bodies, at))
}

/// What rustc calls what is unreachable, where it points and the node it lints at.
fn place(b: &BodyCx<'_, '_>, what: &UnreachableAt) -> Option<(&'static str, Span, InFile<SyntaxNodePtr>)> {
    let store = b.store();
    Some(match *what {
        UnreachableAt::Expr(expr) => {
            ("expression", b.expr_span(expr)?, b.expr_node(expr)?)
        }
        UnreachableAt::Call(expr) => {
            let node = b.expr_node(expr)?;
            let span = match store.expr(expr) {
                Expr::Call { callee, .. } => b.expr_span(callee)?,
                Expr::MethodCall { .. } => {
                    let call = b.cx.syntax(node);
                    let name = ast::MethodCallExpr::cast(call.value)?.name_ref()?;
                    b.cx.span(InFile::new(call.file_id, name.syntax().text_range()))
                }
                _ => b.expr_span(expr)?,
            };
            ("call", span, node)
        }
        UnreachableAt::Stmt { block, index } => {
            let Expr::Block { statements, .. } = store.expr(block) else { return None };
            let node = statement_node(b, statements.get(index as usize)?)?;
            ("statement", b.cx.span_of(node), node)
        }
        UnreachableAt::Arm { match_expr, index } => {
            let Expr::Match { arms, .. } = store.expr(match_expr) else { return None };
            let body = arms.get(index as usize)?.expr;
            ("arm", b.expr_span(body)?, b.expr_node(body)?)
        }
        UnreachableAt::IfBlock(if_expr) => {
            let Expr::If { condition, then_branch, .. } = store.expr(if_expr) else { return None };
            ("block in `if` or `while` expression", b.expr_span(then_branch)?, b.expr_node(condition)?)
        }
        UnreachableAt::LetInit(let_expr) => {
            let Expr::Let { expr, .. } = store.expr(let_expr) else { return None };
            ("block in `let` expression", b.expr_span(expr)?, b.expr_node(expr)?)
        }
    })
}

/// A statement as rustc spans it: an expression statement with its `;`, a `let` with its `;`.
fn statement_node(b: &BodyCx<'_, '_>, statement: Statement<'_>) -> Option<InFile<SyntaxNodePtr>> {
    let (node, is_let) = match statement {
        Statement::Expr { expr, .. } => (b.expr_node(expr)?, false),
        Statement::Let { pat, .. } => (b.pat_node(pat)?, true),
        Statement::Item(_) => return None,
    };
    let syntax = b.cx.syntax(node);
    let found = match is_let {
        true => syntax.value.ancestors().find(|it| ast::LetStmt::can_cast(it.kind()))?,
        false => match syntax.value.parent().and_then(ast::ExprStmt::cast) {
            Some(stmt) => match stmt.expr() {
                Some(ast::Expr::MacroExpr(call)) => call.syntax().clone(),
                _ => stmt.syntax().clone(),
            },
            None => syntax.value.clone(),
        },
    };
    Some(InFile::new(syntax.file_id, SyntaxNodePtr::new(&found)))
}

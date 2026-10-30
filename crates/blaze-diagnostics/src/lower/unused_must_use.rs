//! `unused_must_use`: a statement's value whose type, or the function it is the result of,
//! is `#[must_use]`.
//!
//! Follows rustc's `compiler/rustc_lint/src/unused/must_use.rs` (`UnusedResults::check_stmt`,
//! `is_ty_must_use`, `check_fn_must_use`, `emit_must_use_untranslated`) and
//! `compiler/rustc_lint/src/lints.rs` (`UnusedDef`, `UnusedDefSuggestion`) (MIT or
//! Apache-2.0). rustc gives the lint of the type first, then the one of the function, both
//! when both are `#[must_use]`.

use hir::{InFile, ModuleDef, PathResolution};
use hir_def::attrs::AttrFlags;
use syntax::{AstNode, SyntaxKind, SyntaxNodePtr, ast};

use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{Found, Phase},
    model::{Applicability, Diag, Span},
};

pub(crate) fn lower<'db>(cx: &Cx<'_, 'db>, d: &hir::UnusedMustUse<'db>) -> Vec<Found> {
    let at = d.expr.map(|it| it.syntax_node_ptr());
    let node = cx.syntax(at);
    let Some(expr) = ast::Expr::cast(node.value.clone()) else { return Vec::new() };
    let span = cx.span_of(at);
    // Through the tails of blocks, as rustc's `check_stmt` goes: then the suggestion ends
    // the tail with `;`.
    let from_block = node.value.parent().is_some_and(|it| it.kind() == SyntaxKind::STMT_LIST);
    let mut found = Vec::new();
    if let Some(ty) = cx.sema.type_of_expr(&expr).map(|it| it.original)
        && let Some((adt, post)) = must_use_adt(cx, &ty, "")
    {
        let def = ModuleDef::Adt(adt);
        let reason = AttrFlags::must_use_message(cx.db, hir_def::AdtId::from(adt).into()).flatten();
        let diag = unused_def(cx, "", &post, def, reason, &span, post.is_empty().then_some(from_block));
        found.push(lint(cx, diag, at));
    }
    if let Some(function) = callee(cx, &expr)
        && let Ok(id) = hir_def::FunctionId::try_from(function)
        && let Some(reason) = AttrFlags::must_use_message(cx.db, id.into())
    {
        let diag =
            unused_def(cx, "return value of ", "", ModuleDef::Function(function), reason, &span, Some(from_block));
        found.push(lint(cx, diag, at));
    }
    found
}

fn lint(cx: &Cx<'_, '_>, mut diag: Diag, at: InFile<SyntaxNodePtr>) -> Found {
    code_fact(cx, &mut diag, at);
    Found::lint(diag, Phase::Late, at)
}

/// rustc's `UnusedDef`: "unused {pre}`{def}`{post} that must be used", the attribute's reason
/// as a note, and when the value is the statement's own (`suggest` is whether it is a block's
/// tail), the suggestion to bind it to `_`.
fn unused_def(
    cx: &Cx<'_, '_>,
    pre: &str,
    post: &str,
    def: ModuleDef,
    reason: Option<&str>,
    span: &Span,
    suggest: Option<bool>,
) -> Diag {
    let path = cx.def_path(def);
    let mut diag = Diag::lint("unused_must_use", format!("unused {pre}`{path}`{post} that must be used"), span.clone());
    if let Some(reason) = reason {
        diag.note(reason);
    }
    let message = "use `let _ = ...` to ignore the resulting value";
    match suggest {
        Some(true) => {
            diag.multipart_suggestion(
                message,
                vec![(span.shrink_to_lo(), "let _ = ".to_owned()), (span.shrink_to_hi(), ";".to_owned())],
                Applicability::MaybeIncorrect,
            );
        }
        Some(false) => {
            diag.suggestion_verbose(span.shrink_to_lo(), message, "let _ = ", Applicability::MaybeIncorrect);
        }
        None => {}
    }
    match def {
        ModuleDef::Adt(adt) => {
            let key = cx.key(blaze_items::ItemId::Adt(adt.into()));
            diag.fact(blaze_items::Fact::Signature(key), format!("the attributes of `{path}`"));
        }
        ModuleDef::Function(function) => {
            if let Ok(id) = hir_def::FunctionId::try_from(function) {
                let key = cx.key(blaze_items::ItemId::Function(id));
                diag.fact(blaze_items::Fact::Signature(key), format!("the signature of `{path}`"));
            }
        }
        _ => {}
    }
    diag
}

/// The `#[must_use]` ADT a value of `ty` is one to use for, and what rustc says after its
/// name: `Result<T, E>` with `E` uninhabited is what `T` is, " in a `Result` with an
/// uninhabited error".
fn must_use_adt<'db>(cx: &Cx<'_, 'db>, ty: &hir::Type<'db>, post: &str) -> Option<(hir::Adt, String)> {
    let adt = ty.as_adt()?;
    let lang = matches!(adt.module(cx.db).krate(cx.db).origin(cx.db), base_db::CrateOrigin::Lang(_));
    let name = adt.name(cx.db);
    let args: Vec<hir::Type<'db>> = ty.type_arguments().collect();
    if lang && name.as_str() == "Result" && args.len() == 2 && args[1].is_never() {
        return must_use_adt(cx, &args[0], &format!(" in a `Result` with an uninhabited error{post}"));
    }
    if lang && name.as_str() == "ControlFlow" && args.len() == 2 && args[0].is_never() {
        return must_use_adt(cx, &args[1], &format!(" in a `ControlFlow` with an uninhabited break {post}"));
    }
    AttrFlags::must_use_message(cx.db, hir_def::AdtId::from(adt).into())?;
    Some((adt, post.to_owned()))
}

/// The function a call or a method call calls, when it names one: rustc's
/// `check_fn_must_use` (a local holding a function names none).
fn callee(cx: &Cx<'_, '_>, expr: &ast::Expr) -> Option<hir::Function> {
    match expr {
        ast::Expr::CallExpr(call) => {
            let ast::Expr::PathExpr(path) = call.expr()? else { return None };
            match cx.sema.resolve_path(&path.path()?)? {
                PathResolution::Def(ModuleDef::Function(it)) => Some(it),
                _ => None,
            }
        }
        ast::Expr::MethodCallExpr(call) => cx.sema.resolve_method_call(call),
        _ => None,
    }
}

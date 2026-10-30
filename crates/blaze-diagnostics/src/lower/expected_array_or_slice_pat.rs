//! E0529: a slice pattern matched against what is neither an array nor a slice, with rustc's
//! suggestions to slice the scrutinee (`v[..]`) or to call `as_deref` on it when it is an
//! `Option` or a `Result` of what derefs to one.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/pat.rs` (`check_pat_slice`,
//! `error_expected_array_or_slice`, `is_slice_or_array_or_vector`, and `TopInfo` as
//! `check_decl` and `check_match` give it) and `compiler/rustc_hir_typeck/src/diagnostics.rs`
//! (`ExpectedArrayOrSlice`, `AsDerefSuggestion`, `SlicingSuggestion`) (MIT or Apache-2.0).
//! The scrutinee rustc's `TopInfo` holds is found again from the syntax: the expression of the
//! `match`, of a `let` with no type, of an `if let`, or the right of a destructuring
//! assignment. A `for` loop's pattern has the call of `next` rustc desugars it to as its
//! scrutinee, which is in no source: rustc's suggestion there is not given.

use base_db::{CrateOrigin, LangCrateOrigin};
use hir::InFile;
use syntax::{
    AstNode, SyntaxNode,
    ast::{self, BinaryOp},
};

use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{Found, Phase},
    model::{Applicability, Diag},
};

pub(crate) fn lower<'db>(cx: &Cx<'_, 'db>, d: &hir::ExpectedArrayOrSlicePat<'db>) -> Vec<Found> {
    let span = cx.span_of_ptr(d.pat);
    let ty = cx.ty(&d.found);
    let mut diag =
        Diag::error(Some("E0529"), format!("expected an array or slice, found `{ty}`"), span.clone());
    diag.span_label(span, format!("pattern cannot match with input type `{ty}`"));
    suggest(cx, d, &mut diag);
    code_fact(cx, &mut diag, d.pat.map(|it| it.syntax_node_ptr()));
    vec![Found::new(diag, Phase::Bodies)]
}

/// The scrutinee of the pattern a slice pattern is in, and whether the slice pattern is that
/// pattern itself (rustc's `current_depth <= 1`).
struct Origin {
    expr: InFile<ast::Expr>,
    top_level: bool,
}

fn suggest<'db>(cx: &Cx<'_, 'db>, d: &hir::ExpectedArrayOrSlicePat<'db>, diag: &mut Diag) {
    let found = &d.found;
    if let Some((inner, _)) = found.as_reference()
        && (inner.is_array() || inner.is_slice())
    {
        return;
    }
    if !found.autoderef(cx.db).any(|it| it.is_slice() || it.is_array()) {
        return;
    }
    let Some(origin) = origin(cx, cx.syntax(d.pat.map(|it| it.syntax_node_ptr()))) else { return };
    let Some(root) = cx.sema.type_of_expr(&origin.expr.value).map(|it| it.original) else {
        return;
    };
    let (sequence, root) = slice_or_array_or_vector(cx, root);
    let at = cx.span(origin.expr.as_ref().map(|it| it.syntax().text_range())).shrink_to_hi();
    if let Some(hir::Adt::Enum(it)) = root.as_adt()
        && is_core_item(cx, it.module(cx.db), &it.name(cx.db), &["Option", "Result"])
    {
        diag.suggestion_verbose(
            at.clone(),
            "consider using `as_deref` here",
            ".as_deref()",
            Applicability::MaybeIncorrect,
        );
    }
    if sequence && origin.top_level {
        diag.suggestion_verbose(at, "consider slicing here", "[..]", Applicability::MaybeIncorrect);
    }
}

/// rustc's `is_slice_or_array_or_vector`: whether `ty` behind its references is a `Vec`, an
/// array or a slice, and `ty` without them.
fn slice_or_array_or_vector<'db>(cx: &Cx<'_, 'db>, ty: hir::Type<'db>) -> (bool, hir::Type<'db>) {
    if let Some((inner, _)) = ty.as_reference() {
        return slice_or_array_or_vector(cx, inner);
    }
    let vec = match ty.as_adt() {
        Some(hir::Adt::Struct(it)) => {
            is_lang_crate_item(cx, it.module(cx.db), &it.name(cx.db), "Vec", LangCrateOrigin::Alloc)
        }
        _ => false,
    };
    (vec || ty.is_slice() || ty.is_array(), ty)
}

/// Whether the item named `name` in `module` is core's of one of `names`: rustc's diagnostic
/// items `Option` and `Result`.
fn is_core_item(cx: &Cx<'_, '_>, module: hir::Module, name: &hir::Name, names: &[&str]) -> bool {
    names.iter().any(|it| is_lang_crate_item(cx, module, name, it, LangCrateOrigin::Core))
}

fn is_lang_crate_item(
    cx: &Cx<'_, '_>,
    module: hir::Module,
    name: &hir::Name,
    expected: &str,
    krate: LangCrateOrigin,
) -> bool {
    name.as_str() == expected
        && module.krate(cx.db).origin(cx.db) == CrateOrigin::Lang(krate)
}

/// rustc's `TopInfo` of the pattern `node` is in: the expression it matches, and whether
/// `node` is the whole pattern, found by going up through the patterns (or, of a
/// destructuring assignment, the expressions) around it.
fn origin(cx: &Cx<'_, '_>, node: InFile<SyntaxNode>) -> Option<Origin> {
    let is_pat = ast::Pat::can_cast(node.value.kind());
    let mut depth = 0u32;
    let mut prev = node.value.clone();
    for ancestor in cx.sema.ancestors_with_macros_file(node).skip(1) {
        let it = &ancestor.value;
        let at = |expr: Option<ast::Expr>| expr.map(|e| ancestor.with_value(e));
        if is_pat {
            if let Some(arm) = ast::MatchArm::cast(it.clone()) {
                let expr = arm.syntax().parent()?.parent().and_then(ast::MatchExpr::cast)?.expr();
                return at(expr).map(|expr| Origin { expr, top_level: depth == 0 });
            }
            if let Some(stmt) = ast::LetStmt::cast(it.clone()) {
                if stmt.ty().is_some() {
                    return None;
                }
                return at(stmt.initializer()).map(|expr| Origin { expr, top_level: depth == 0 });
            }
            if let Some(expr) = ast::LetExpr::cast(it.clone()) {
                return at(expr.expr()).map(|expr| Origin { expr, top_level: depth == 0 });
            }
            match ast::Pat::cast(it.clone()) {
                Some(ast::Pat::ParenPat(_) | ast::Pat::MacroPat(_)) => {}
                Some(_) => depth += 1,
                None if ast::RecordPatField::can_cast(it.kind())
                    || ast::RecordPatFieldList::can_cast(it.kind())
                    || ast::MacroCall::can_cast(it.kind())
                    || ast::TokenTree::can_cast(it.kind()) => {}
                None => return None,
            }
        } else {
            if let Some(bin) = ast::BinExpr::cast(it.clone())
                && bin.op_kind() == Some(BinaryOp::Assignment { op: None })
            {
                if bin.lhs().is_none_or(|lhs| lhs.syntax() != &prev) {
                    return None;
                }
                return at(bin.rhs()).map(|expr| Origin { expr, top_level: depth == 0 });
            }
            match ast::Expr::cast(it.clone()) {
                Some(ast::Expr::ParenExpr(_)) => {}
                Some(
                    ast::Expr::TupleExpr(_)
                    | ast::Expr::ArrayExpr(_)
                    | ast::Expr::CallExpr(_)
                    | ast::Expr::RecordExpr(_),
                ) => depth += 1,
                None if ast::ArgList::can_cast(it.kind())
                    || ast::RecordExprField::can_cast(it.kind())
                    || ast::RecordExprFieldList::can_cast(it.kind()) => {}
                _ => return None,
            }
        }
        prev = it.clone();
    }
    None
}

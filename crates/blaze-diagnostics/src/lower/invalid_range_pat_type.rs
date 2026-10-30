//! E0029: a range pattern whose ends are not `char`s or numbers, as `"a"..="z"`, with a label
//! on each end of what it is.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/pat.rs` (`check_pat_range`,
//! `emit_err_pat_range`, `endpoint_has_type`) (MIT or Apache-2.0). The `-Z teach` note is
//! not written: blaze is not run with it.

use hir::InFile;
use syntax::{
    AstNode,
    ast::{self, RangeItem},
};

use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{Found, Phase},
    model::{Diag, Span},
};

/// An end of the range: where it is, its type, and whether it is what fails.
struct End<'db> {
    span: Span,
    ty: hir::Type<'db>,
    fail: bool,
}

pub(crate) fn lower<'db>(cx: &Cx<'_, 'db>, d: &hir::InvalidRangePatType) -> Vec<Found> {
    let node = cx.node(d.pat);
    let Some(ast::Pat::RangePat(range)) = node.value.as_ref().right() else { return Vec::new() };
    let file = node.file_id;
    let end = |pat: Option<ast::Pat>| {
        let pat = pat?;
        let ty = cx.sema.type_of_pat(&pat)?.original;
        let fail = !(ty.is_int_or_uint() || ty.is_float() || ty.is_char() || ty.contains_unknown());
        Some(End { span: cx.span(InFile::new(file, pat.syntax().text_range())), ty, fail })
    };
    let mut lhs = end(range.start());
    let mut rhs = end(range.end());
    // The ends were each fine, but not once unified: rustc's check after `demand_eqtype`,
    // which takes both to fail.
    if !lhs.as_ref().is_some_and(|it| it.fail) && !rhs.as_ref().is_some_and(|it| it.fail) {
        lhs.iter_mut().chain(rhs.iter_mut()).for_each(|it| it.fail = true);
    }
    if lhs.iter().chain(rhs.iter()).any(|it| it.ty.contains_unknown()) {
        // rustc's `downgrade_to_delayed_bug`: an error of an end was given already.
        return Vec::new();
    }
    let span = match (&lhs, &rhs) {
        (Some(l), Some(r)) if l.fail && r.fail => cx.span_of_ptr(d.pat),
        (Some(l), _) if l.fail => l.span.clone(),
        (_, Some(r)) if r.fail => r.span.clone(),
        _ => return Vec::new(),
    };
    let mut diag = Diag::error(
        Some("E0029"),
        "only `char` and numeric types are allowed in range patterns",
        span,
    );
    let wrong = |end: &End<'db>| {
        format!("this is of type `{}` but it should be `char` or numeric", cx.ty(&end.ty))
    };
    let one_side = |diag: &mut Diag, first: &End<'db>, second: Option<&End<'db>>| {
        diag.span_label(first.span.clone(), wrong(first));
        if let Some(other) = second {
            diag.span_label(other.span.clone(), format!("this is of type `{}`", cx.ty(&other.ty)));
        }
    };
    match (&lhs, &rhs) {
        (Some(l), Some(r)) if l.fail && r.fail => {
            diag.span_label(l.span.clone(), wrong(l));
            diag.span_label(r.span.clone(), wrong(r));
        }
        (Some(l), r) if l.fail => one_side(&mut diag, l, r.as_ref()),
        (l, Some(r)) if r.fail => one_side(&mut diag, r, l.as_ref()),
        _ => return Vec::new(),
    }
    code_fact(cx, &mut diag, d.pat.map(|it| it.syntax_node_ptr()));
    vec![Found::new(diag, Phase::Bodies)]
}

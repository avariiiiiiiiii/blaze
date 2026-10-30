//! E0130 "patterns aren't allowed in foreign function declarations": a parameter of a function
//! in an `extern` block bound by anything but a plain name or `_`.
//!
//! Follows rustc's `compiler/rustc_ast_passes/src/ast_validation.rs` (`check_decl_no_pat`,
//! `visit_fn`) and `compiler/rustc_ast_passes/src/diagnostics.rs` (`PatternInForeign`)
//! (MIT or Apache-2.0).

use hir::InFile;
use syntax::AstNode;

use crate::{
    cx::Cx,
    lower::{Found, Phase, incorrect_generics_len::site_fact},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::PatternArgInExternFn) -> Vec<Found> {
    let pat = cx.node(d.node);
    let span = cx.span(pat.as_ref().map(|it| it.syntax().text_range()));
    let mut diag = Diag::error(
        Some("E0130"),
        "patterns aren't allowed in foreign function declarations",
        span.clone(),
    );
    diag.span_label(span, "pattern not allowed in foreign function");
    site_fact(cx, &mut diag, InFile::new(pat.file_id, pat.value.syntax()));
    vec![Found::new(diag, Phase::Resolve)]
}

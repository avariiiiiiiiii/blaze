//! "return type notation not allowed in this position yet": `Trait::method(..)` where
//! return type notation is not taken (a path that does not name an associated function's
//! return type, or a segment that takes no `Fn`-style arguments).
//!
//! Follows rustc's `compiler/rustc_ast_lowering/src/path.rs` (`lower_path_segment`,
//! `GenericArgs::ParenthesizedElided`) and `compiler/rustc_ast_lowering/src/diagnostics.rs`
//! (`BadReturnTypeNotation::Position`) (MIT or Apache-2.0).

use hir::InFile;
use syntax::AstNode;

use crate::{
    cx::Cx,
    lower::{Found, Phase, incorrect_generics_len::site_fact},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::BadRtn) -> Vec<Found> {
    let rtn = cx.node(d.rtn);
    let span = cx.span(rtn.as_ref().map(|it| it.syntax().text_range()));
    let mut diag = Diag::error(None, "return type notation not allowed in this position yet", span);
    site_fact(cx, &mut diag, InFile::new(rtn.file_id, rtn.value.syntax()));
    vec![Found::new(diag, Phase::Resolve)]
}

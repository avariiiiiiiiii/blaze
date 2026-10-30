//! A diagnostic hir found in desugared code with no syntax of its own to place it at: blaze's
//! error at the item or constant it is in, never dropped. rustc places each of these, so it
//! has no rustc counterpart.

use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::UnplacedDiagnostic<'_>) -> Vec<Found> {
    let node = cx.syntax_view(d.owner);
    let span = cx.span(node.map(|it| it.text_range()));
    let what = match &d.what {
        // alloc: the error's text, for an error hir found
        hir::Unplaced::Inference(it) => format!("type checking's {it:?}"),
        // alloc: the error's text, for an error hir found
        hir::Unplaced::MissingUnsafe { reason, lint } => format!("{reason:?} ({lint:?}) needs `unsafe`"),
        hir::Unplaced::Borrowck { code, message } | hir::Unplaced::ConstCheck { code, message } => {
            // alloc: the error's text, for an error hir found
            format!("{code}: {message}")
        }
    };
    // alloc: the error's text, for an error hir found
    let message = format!("blaze: {what}, in code with no syntax of its own, which blaze cannot place");
    // alloc: the error found
    vec![Found::new(Diag::error(None, message, span), Phase::Bodies)]
}

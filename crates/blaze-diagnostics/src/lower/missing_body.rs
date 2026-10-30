//! An item that needs a body written without one: "free constant item without body", "free
//! static item without body", "free type alias without body", "associated constant in `impl`
//! without body", "associated type in `impl` without body", each with its placeholder
//! definition.
//!
//! Follows rustc's `compiler/rustc_ast_passes/src/ast_validation.rs` (`visit_item`,
//! `visit_assoc_item`, `ending_semi_or_hi`) and `compiler/rustc_ast_passes/src/diagnostics.rs`
//! (`ConstWithoutBody`, `StaticWithoutBody`, `TyAliasWithoutBody`, `AssocConstWithoutBody`,
//! `AssocTypeWithoutBody`) (MIT or Apache-2.0).

use hir::{InFile, MissingBodyItemKind};
use syntax::{T, TextRange};

use crate::{
    cx::{Cx, item_range},
    lower::{Found, Phase, incorrect_generics_len::site_fact},
    model::{Applicability, Diag},
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::MissingBody) -> Vec<Found> {
    let node = cx.syntax(d.node);
    let file = node.file_id;
    let range = item_range(&node.value);
    let span = |range: TextRange| cx.span(InFile::new(file, range));
    let (message, what, code) = match d.kind {
        MissingBodyItemKind::Const => ("free constant item without body", "constant", " = <expr>;"),
        MissingBodyItemKind::Static => ("free static item without body", "static", " = <expr>;"),
        MissingBodyItemKind::TypeAlias => ("free type alias without body", "type", " = <type>;"),
        MissingBodyItemKind::AssocConst => {
            ("associated constant in `impl` without body", "constant", " = <expr>;")
        }
        MissingBodyItemKind::AssocType => {
            ("associated type in `impl` without body", "type", " = <type>;")
        }
    };
    let semi = node
        .value
        .last_token()
        .filter(|it| it.kind() == T![;] && it.text_range().end() == range.end())
        .map(|it| it.text_range());
    let replace = semi.unwrap_or_else(|| TextRange::empty(range.end()));
    let mut diag = Diag::error(None, message, span(range));
    diag.suggestion(
        span(replace),
        format!("provide a definition for the {what}"),
        code,
        Applicability::HasPlaceholders,
    );
    site_fact(cx, &mut diag, node.as_ref());
    vec![Found::new(diag, Phase::Resolve)]
}

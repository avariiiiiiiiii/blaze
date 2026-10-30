use crate::{Diagnostic, DiagnosticCode, DiagnosticsContext};

// Diagnostic: ambiguous-associated-items
//
// This diagnostic is triggered when `Self::V` in an impl of a trait for an enum names both a
// variant of the enum and an associated type of the trait.
pub(crate) fn ambiguous_associated_item(
    ctx: &DiagnosticsContext<'_, '_>,
    d: &hir::AmbiguousAssociatedItem<'_>,
) -> Diagnostic {
    Diagnostic::new_with_syntax_node_ptr(
        ctx,
        DiagnosticCode::RustcLint("ambiguous_associated_items"),
        "ambiguous associated item",
        d.path.map(Into::into),
    )
}

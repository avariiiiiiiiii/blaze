use hir::TraitItemImportWhy;

use crate::{Diagnostic, DiagnosticCode, DiagnosticsContext, Severity};

// Diagnostic: trait-item-import
//
// This diagnostic is triggered by an import of a trait's associated items: without
// `import_trait_associated_functions` (E0658), or of a trait an item of which a macro writes.
pub(crate) fn trait_item_import(ctx: &DiagnosticsContext<'_, '_>, d: &hir::TraitItemImport) -> Diagnostic {
    let (code, message) = match d.why {
        TraitItemImportWhy::Unstable => {
            (DiagnosticCode::RustcHardError("E0658"), "`use` associated items of traits is unstable")
        }
        TraitItemImportWhy::ItemsFromMacros => (
            DiagnosticCode::Ra("trait-item-import", Severity::Error),
            "blaze cannot import the items of this trait: an item of it comes from a macro or an attribute macro",
        ),
    };
    Diagnostic::new_with_syntax_node_ptr(ctx, code, message, d.decl.map(|it| it.into()))
}

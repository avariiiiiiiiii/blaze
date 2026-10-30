use hir::{InFile, MissingLifetimeKind, MissingLifetimeSite};
use syntax::SyntaxNodePtr;

use crate::{Diagnostic, DiagnosticCode, DiagnosticsContext};

// Diagnostic: missing-lifetimes
//
// This diagnostic is triggered when `&T` or `'_` leaves a lifetime out where there is none to
// elide to (E0106), or where an elided lifetime is not allowed (E0637).
pub(crate) fn missing_lifetimes(
    ctx: &DiagnosticsContext<'_, '_>,
    d: &hir::MissingLifetimes,
) -> Vec<Diagnostic> {
    let node = |site: &MissingLifetimeSite| -> InFile<SyntaxNodePtr> {
        match site {
            MissingLifetimeSite::Reference(it) => it.map(Into::into),
            MissingLifetimeSite::Placeholder(it) => it.map(Into::into),
            MissingLifetimeSite::Path { generics_or_segment, .. } => {
                generics_or_segment.map(Into::into)
            }
        }
    };
    let e0106 = |site: &MissingLifetimeSite| {
        Diagnostic::new_with_syntax_node_ptr(
            ctx,
            DiagnosticCode::RustcHardError("E0106"),
            "missing lifetime specifier",
            node(site),
        )
    };
    match d.kind {
        MissingLifetimeKind::Missing => d.sites.iter().map(e0106).collect(),
        MissingLifetimeKind::ElisionFailure => d.sites.first().map(e0106).into_iter().collect(),
        MissingLifetimeKind::NotAllowed => d
            .sites
            .iter()
            .map(|site| match site {
                MissingLifetimeSite::Reference(_) => Diagnostic::new_with_syntax_node_ptr(
                    ctx,
                    DiagnosticCode::RustcHardError("E0637"),
                    "`&` without an explicit lifetime name cannot be used here",
                    node(site),
                ),
                MissingLifetimeSite::Placeholder(_) => Diagnostic::new_with_syntax_node_ptr(
                    ctx,
                    DiagnosticCode::RustcHardError("E0637"),
                    "`'_` cannot be used here",
                    node(site),
                ),
                MissingLifetimeSite::Path { .. } => e0106(site),
            })
            .collect(),
    }
}

use hir::HirDisplay;
use ide_db::Severity;

use crate::{Diagnostic, DiagnosticCode, DiagnosticsContext};

// Diagnostic: unsafe-binder-cast-of-non-binder
//
// This diagnostic is triggered when `wrap_binder!()` wraps into, or `unwrap_binder!()`
// unwraps, a type that is not an unsafe binder.
pub(crate) fn unsafe_binder_cast_of_non_binder(
    ctx: &DiagnosticsContext<'_, '_>,
    d: &hir::UnsafeBinderCastOfNonBinder<'_>,
) -> Diagnostic {
    let found = d.found.display(ctx.sema.db, ctx.display_target);
    let message = match d.kind {
        hir::UnsafeBinderCastKind::Wrap => {
            format!("`wrap_binder!()` can only wrap into unsafe binder, not `{found}`")
        }
        hir::UnsafeBinderCastKind::Unwrap => {
            format!("expected unsafe binder, found `{found}` as input of `unwrap_binder!()`")
        }
    };
    Diagnostic::new_with_syntax_node_ptr(
        ctx,
        DiagnosticCode::Ra("unsafe-binder-cast-of-non-binder", Severity::Error),
        message,
        d.expr.map(|it| it.into()),
    )
    .stable()
}

use crate::{Diagnostic, DiagnosticCode, DiagnosticsContext};

// Diagnostic: late-bound-lifetime-arguments
//
// This diagnostic is triggered when lifetime arguments are given to a function with late-bound
// lifetime parameters. It is a hard error when not every lifetime is given outside a method
// call, and a lint otherwise.
pub(crate) fn late_bound_lifetime_args(
    ctx: &DiagnosticsContext<'_, '_>,
    d: &hir::LateBoundLifetimeArgs,
) -> Diagnostic {
    let code = match d.hard_error {
        true => DiagnosticCode::RustcHardError("E0794"),
        false => DiagnosticCode::RustcLint("late_bound_lifetime_arguments"),
    };
    Diagnostic::new_with_syntax_node_ptr(
        ctx,
        code,
        "cannot specify lifetime arguments explicitly if late bound lifetime parameters are present",
        d.generics_or_segment.map(Into::into),
    )
}

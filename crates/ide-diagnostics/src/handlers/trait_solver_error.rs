use either::Either;
use hir::HirDisplay;

use crate::{Diagnostic, DiagnosticCode, DiagnosticsContext, Severity};

// Diagnostic: trait-solver-error
//
// This diagnostic is triggered when the trait solver fails on a goal that is not a trait bound:
// a `[const] Trait` bound, overflow, a projection or subtyping mismatch, a const argument of the
// wrong type.
pub(crate) fn trait_solver_error<'db>(
    ctx: &DiagnosticsContext<'_, 'db>,
    d: &hir::TraitSolverError<'db>,
) -> Diagnostic {
    let ty = |ty: &hir::Type<'db>| ty.display(ctx.db(), ctx.display_target).to_string();
    let (code, message) = match &d.kind {
        hir::TraitSolverErrorKind::GoalUnmet(goal) => match goal {
            hir::SolverGoal::HostEffect { trait_ref, maybe_const } => (
                DiagnosticCode::RustcHardError("E0277"),
                format!(
                    "the trait bound `{}: {}{}` is not satisfied",
                    ty(&trait_ref.self_ty()),
                    if *maybe_const { "[const] " } else { "const " },
                    trait_ref.trait_().name(ctx.db()).display(ctx.db(), ctx.edition),
                ),
            ),
            hir::SolverGoal::DynCompatible(trait_) => (
                DiagnosticCode::RustcHardError("E0038"),
                format!("the trait `{}` is not dyn compatible", trait_.name(ctx.db()).display(ctx.db(), ctx.edition)),
            ),
            hir::SolverGoal::WellFormed(term) => (
                DiagnosticCode::Ra("trait-solver-error", Severity::Error),
                format!("the type `{}` is not well-formed", either_text(term, ty)),
            ),
            _ => (DiagnosticCode::Ra("trait-solver-error", Severity::Error), "a requirement is not satisfied".to_owned()),
        },
        hir::TraitSolverErrorKind::Overflow { .. } => {
            (DiagnosticCode::RustcHardError("E0275"), "overflow evaluating a requirement".to_owned())
        }
        hir::TraitSolverErrorKind::ProjectionMismatch(_) => {
            (DiagnosticCode::RustcHardError("E0271"), "type mismatch resolving a projection".to_owned())
        }
        hir::TraitSolverErrorKind::TypesDiffer { expected, found } => (
            DiagnosticCode::RustcHardError("E0308"),
            format!("expected {}, found {}", ty(expected), ty(found)),
        ),
        hir::TraitSolverErrorKind::ConstArgHasWrongType { ct, expected_ty, .. } => (
            DiagnosticCode::Ra("trait-solver-error", Severity::Error),
            format!("the constant `{ct}` is not of type `{}`", ty(expected_ty)),
        ),
        hir::TraitSolverErrorKind::HigherRankedOutlives => (
            DiagnosticCode::Ra("trait-solver-error", Severity::Error),
            "higher-ranked lifetime bound could not be satisfied".to_owned(),
        ),
        hir::TraitSolverErrorKind::Unreported(code) => (
            DiagnosticCode::Ra("trait-solver-error", Severity::Error),
            format!("trait solver error: {code:?}"),
        ),
    };
    Diagnostic::new_with_syntax_node_ptr(ctx, code, message, d.span.map(Into::into))
}

fn either_text<'db>(term: &Either<hir::Type<'db>, String>, ty: impl Fn(&hir::Type<'db>) -> String) -> String {
    match term {
        Either::Left(it) => ty(it),
        Either::Right(it) => it.clone(),
    }
}

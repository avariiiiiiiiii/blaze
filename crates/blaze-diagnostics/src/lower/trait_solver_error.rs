//! The trait solver's errors that are not an unmet trait bound (`unimplemented_trait.rs`) or a
//! closure of the wrong kind (`closure_kind_mismatch.rs`), in rustc's words: E0277 "the trait
//! bound `T: [const] Trait` is not satisfied"; "the type `T` is not well-formed"; E0275
//! "overflow evaluating the requirement `..`" with the help to raise `#![recursion_limit]`;
//! E0271 "type mismatch resolving `<T as Trait>::Assoc == U`"; E0308 of a subtyping or
//! coercion goal as `type_mismatch.rs` gives it; "the constant `N` is not of type `u8`";
//! "higher-ranked lifetime bound could not be satisfied". Each with the obligation cause's
//! notes (`obligation_cause.rs`).
//!
//! Follows rustc's `compiler/rustc_trait_selection/src/error_reporting/traits/
//! fulfillment_errors.rs` (`report_selection_error`, `report_host_effect_error`,
//! `report_projection_error`), `overflow.rs` (`build_overflow_error`,
//! `suggest_new_overflow_limit`) and `compiler/rustc_middle/src/ty/print/pretty.rs` (how each
//! predicate prints) (MIT or Apache-2.0).
//!
//! What blaze does not describe yet, each an error that says so: a trait that is not dyn
//! compatible (E0038 needs the violations' spans, which the front end does not give), an
//! outlives goal (the front end gives no regions), a goal rustc never reports as unmet, and the
//! old solver's errors the next solver never gives. Not given yet (requested in
//! `v2/requests/diagnostics-infer2-solver.md`): E0271's "expected `X`, found `Y`" (the front end
//! does not give what the alias normalized to), the impl a `[const] Trait` bound selected (for
//! "trait `Trait` is implemented but not `const`"), and rustc's shortening of a requirement
//! longer than 50 characters.
//!
//! Cost: nothing until the solver reports the error; then O(the goal printed once).

use either::Either;
use hir::{InFile, ModuleDef};
use syntax::ast;

use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{Found, Phase, obligation_cause, type_mismatch},
    model::{Diag, Span},
    print,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::TraitSolverError<'_>) -> Vec<Found> {
    let at = d.span.map(|it| it.syntax_node_ptr());
    let span = cx.span_of(at);
    let mut diag = match &d.kind {
        hir::TraitSolverErrorKind::GoalUnmet(goal) => goal_unmet(cx, goal, span),
        hir::TraitSolverErrorKind::Overflow { goal, suggest_increasing_limit } => {
            overflow(cx, goal, *suggest_increasing_limit, span)
        }
        hir::TraitSolverErrorKind::ProjectionMismatch(goal) => match goal_text(cx, goal) {
            // alloc: the error's text, for an error hir found
            Some(goal) => Diag::error(Some("E0271"), format!("type mismatch resolving `{goal}`"), span),
            None => cannot_print(goal, span),
        },
        hir::TraitSolverErrorKind::TypesDiffer { expected, found } => {
            if let Some(expr_or_pat) = d.span.value.cast::<Either<ast::Expr, ast::Pat>>() {
                let mismatch = hir::TypeMismatch {
                    expr_or_pat: InFile::new(d.span.file_id, expr_or_pat),
                    // clone: on the error path, for an error hir found
                    expected: expected.clone(),
                    // clone: on the error path, for an error hir found
                    actual: found.clone(),
                    at_error: None,
                    cause: d.cause,
                };
                return type_mismatch::lower(cx, &mismatch);
            }
            // clone: on the error path, for an error hir found
            let mut diag = Diag::error(Some("E0308"), "mismatched types", span.clone());
            diag.span_label(span, print::sorts(cx, expected, found));
            type_mismatch::note_expected_found(cx, expected, found, None, &mut diag);
            diag
        }
        hir::TraitSolverErrorKind::ConstArgHasWrongType { ct, ct_ty, expected_ty } => {
            // alloc: the error's text, for an error hir found
            let message = format!("the constant `{ct}` is not of type `{}`", cx.ty(expected_ty));
            // clone: on the error path, for an error hir found
            let mut diag = Diag::error(None, message, span.clone());
            diag.span_label(span, print::sorts(cx, expected_ty, ct_ty));
            obligation_cause::note(cx, &mut diag, d.cause, None, None);
            diag
        }
        hir::TraitSolverErrorKind::HigherRankedOutlives => {
            Diag::error(None, "higher-ranked lifetime bound could not be satisfied", span)
        }
        hir::TraitSolverErrorKind::Unreported(code) => Diag::error(
            None,
            // alloc: the error's text, for an error hir found
            format!("blaze cannot describe this trait solver error ({code:?}), which only rustc's old solver gives"),
            span,
        ),
    };
    if let hir::TraitSolverErrorKind::GoalUnmet(goal)
    | hir::TraitSolverErrorKind::Overflow { goal, .. }
    | hir::TraitSolverErrorKind::ProjectionMismatch(goal) = &d.kind
    {
        let (trait_, self_ty) = match goal {
            hir::SolverGoal::Trait(pred) => (Some(pred.trait_ref().trait_()), pred.trait_ref().get_type_argument(0)),
            hir::SolverGoal::HostEffect { trait_ref, .. } => (Some(trait_ref.trait_()), trait_ref.get_type_argument(0)),
            _ => (None, None),
        };
        obligation_cause::note(cx, &mut diag, d.cause, trait_, self_ty.as_ref());
    }
    code_fact(cx, &mut diag, at);
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Bodies)]
}

/// rustc's `report_selection_error` of a goal that is not a trait's and does not hold.
fn goal_unmet(cx: &Cx<'_, '_>, goal: &hir::SolverGoal<'_>, span: Span) -> Diag {
    match goal {
        hir::SolverGoal::HostEffect { trait_ref, maybe_const } => Diag::error(
            Some("E0277"),
            // alloc: the error's text, for an error hir found
            format!("the trait bound `{}` is not satisfied", print::host_effect(cx, trait_ref, *maybe_const)),
            span,
        ),
        // alloc: the error's text, for an error hir found
        hir::SolverGoal::WellFormed(ty) => Diag::error(None, format!("the type `{}` is not well-formed", term(cx, ty)), span),
        hir::SolverGoal::DynCompatible(trait_) => Diag::error(
            None,
            // alloc: the error's text, for an error hir found
            format!(
                "blaze cannot describe why the trait `{}` is not dyn compatible (E0038): the front end does not give the violations' spans",
                print::def_path(cx, ModuleDef::Trait(*trait_))
            ),
            span,
        ),
        _ => match goal_text(cx, goal) {
            Some(text) => Diag::error(
                None,
                // alloc: the error's text, for an error hir found
                format!("blaze cannot describe the unmet goal `{text}`, which rustc never reports as unmet"),
                span,
            ),
            None => cannot_print(goal, span),
        },
    }
}

/// rustc's `build_overflow_error` (E0275).
fn overflow(cx: &Cx<'_, '_>, goal: &hir::SolverGoal<'_>, suggest_increasing_limit: bool, span: Span) -> Diag {
    let message = match goal {
        hir::SolverGoal::Subtype { a, b } | hir::SolverGoal::Coerce { a, b } => {
            // alloc: the error's text, for an error hir found
            format!("overflow assigning `{}` to `{}`", cx.ty(a), cx.ty(b))
        }
        hir::SolverGoal::WellFormed(ty) => {
            // alloc: the error's text, for an error hir found
            format!("overflow evaluating whether `{}` is well-formed", term(cx, ty))
        }
        _ => match goal_text(cx, goal) {
            // alloc: the error's text, for an error hir found
            Some(text) => format!("overflow evaluating the requirement `{text}`"),
            None => return cannot_print(goal, span),
        },
    };
    let mut diag = Diag::error(Some("E0275"), message, span);
    if suggest_increasing_limit {
        diag.help(recursion_limit_help(cx));
    }
    diag
}

/// rustc's `suggest_new_overflow_limit`.
fn recursion_limit_help(cx: &Cx<'_, '_>) -> String {
    let limit = hir_def::nameres::crate_def_map(cx.db, cx.krate.into()).recursion_limit();
    let suggested = if limit == 0 { 2 } else { limit * 2 };
    let crate_name = match cx.krate.display_name(cx.db) {
        Some(name) => name.crate_name().to_string(), // alloc: the error's text, for an error hir found
        None => {
            let path = cx.file_name(cx.krate.root_file(cx.db));
            let stem = path.rsplit('/').next().unwrap_or(&path);
            stem.strip_suffix(".rs").unwrap_or(stem).replace('-', "_")
        }
    };
    // alloc: the error's text, for an error hir found
    format!(
        "consider increasing the recursion limit by adding a `#![recursion_limit = \"{suggested}\"]` attribute to your crate (`{crate_name}`)"
    )
}

/// blaze's error for a goal it cannot print.
fn cannot_print(goal: &hir::SolverGoal<'_>, span: Span) -> Diag {
    let what = match goal {
        hir::SolverGoal::Outlives => "an outlives goal (the front end gives no regions)",
        hir::SolverGoal::NormalizesTo { .. } => "a normalizes-to goal without its term",
        _ => "a goal",
    };
    // alloc: the error's text, for an error hir found
    Diag::error(None, format!("blaze cannot print the requirement of this trait solver error: {what}"), span)
}

/// A goal as rustc prints its predicate: `T: Trait`, `T: [const] Trait`, `<T as Trait>::Assoc ==
/// U`, `T well-formed`, `a <: b`, `a -> b`, "the constant `N` can be evaluated". `None` for a
/// goal the front end does not give enough of to print.
pub(super) fn goal_text(cx: &Cx<'_, '_>, goal: &hir::SolverGoal<'_>) -> Option<String> {
    // alloc: the error's text, for an error hir found
    Some(match goal {
        hir::SolverGoal::Trait(pred) => print::predicate(cx, pred),
        hir::SolverGoal::HostEffect { trait_ref, maybe_const } => print::host_effect(cx, trait_ref, *maybe_const),
        hir::SolverGoal::Projection { alias, term: to } => format!("{} == {}", term(cx, alias), term(cx, to)),
        hir::SolverGoal::NormalizesTo { .. } | hir::SolverGoal::Outlives => return None,
        hir::SolverGoal::WellFormed(it) => format!("{} well-formed", term(cx, it)),
        hir::SolverGoal::Subtype { a, b } => format!("{} <: {}", cx.ty(a), cx.ty(b)),
        hir::SolverGoal::Coerce { a, b } => format!("{} -> {}", cx.ty(a), cx.ty(b)),
        hir::SolverGoal::ConstEvaluatable(ct) => format!("the constant `{ct}` can be evaluated"),
        hir::SolverGoal::ConstArgHasType { ct, ty } => format!("the constant `{ct}` has type `{}`", cx.ty(ty)),
        hir::SolverGoal::ConstEquate { a, b } => format!("the constant `{a}` equals `{b}`"),
        hir::SolverGoal::DynCompatible(trait_) => {
            format!("the trait `{}` is dyn-compatible", print::def_path(cx, ModuleDef::Trait(*trait_)))
        }
        hir::SolverGoal::UnstableFeature(feature) => format!("feature({}) is enabled", feature.as_str()),
        hir::SolverGoal::Ambiguous => "ambiguous".to_owned(),
    })
}

/// A type or a constant, as the front end gives a term.
pub(super) fn term(cx: &Cx<'_, '_>, term: &Either<hir::Type<'_>, String>) -> String {
    match term {
        Either::Left(ty) => cx.ty(ty),
        // clone: on the error path, for an error hir found
        Either::Right(ct) => ct.clone(),
    }
}

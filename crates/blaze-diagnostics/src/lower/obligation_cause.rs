//! The notes rustc adds to a trait solver error for why the bound was required: rustc's
//! `note_obligation_cause_code` for the causes the front end records ("required by a bound in
//! `f`" on the bound of the item used, "all function arguments must have a statically known
//! size" and the other `Sized` notes). The causes of a type mismatch (`if` and `match` arms,
//! a return value, a block's tail) add nothing here; `type_mismatch.rs` gives their labels, as
//! rustc's `note_type_err` does.
//!
//! Follows rustc's `compiler/rustc_trait_selection/src/error_reporting/traits/suggestions.rs`
//! (`note_obligation_cause_code`) (MIT or Apache-2.0).
//!
//! What the front end does not give yet (requested in `v2/requests/diagnostics-infer2-solver.md`):
//! which written bound `bound_index` counts to, so a bound is found again by its trait in the
//! item's syntax, and rustc's note on the item's head when that is not one bound; and the
//! argument or `let` a `Sized` bound is of, for rustc's "consider borrowing here".
//!
//! Cost: nothing until the solver reports the error; then O(the item's written bounds).

use hir::{LangItem, ModuleDef};
use hir_def::unstable_features::UnstableFeatures;
use intern::sym;

use crate::{
    cx::Cx,
    lower::{
        unimplemented_trait::{is_lang, where_clause_note},
        unresolved_ident::{Ns, def_span},
    },
    model::Diag,
    print,
};

/// rustc's `note_obligation_cause_code` of `cause`, for a bound of `trait_` on `self_ty` when
/// the failed goal is a trait's.
pub(super) fn note<'db>(
    cx: &Cx<'_, 'db>,
    diag: &mut Diag,
    cause: hir::ObligationCauseCode,
    trait_: Option<hir::Trait>,
    self_ty: Option<&hir::Type<'db>>,
) {
    match cause {
        hir::ObligationCauseCode::WhereClauseInExpr { item, .. } => {
            where_clause(cx, diag, item, trait_, self_ty)
        }
        hir::ObligationCauseCode::SizedArgumentType => {
            diag.note("all function arguments must have a statically known size");
            if !UnstableFeatures::query(cx.db, cx.krate.base()).is_enabled(&sym::unsized_fn_params) {
                diag.help("unsized fn params are gated as an unstable feature");
            }
        }
        hir::ObligationCauseCode::SizedReturnType => {
            diag.note("the return type of a function must have a statically known size");
        }
        hir::ObligationCauseCode::LetType => {
            diag.note("all local variables must have a statically known size");
        }
        hir::ObligationCauseCode::Misc
        | hir::ObligationCauseCode::BlockTailExpression
        | hir::ObligationCauseCode::ReturnValue
        | hir::ObligationCauseCode::IfExpression
        | hir::ObligationCauseCode::MatchExpressionArm { .. } => {}
    }
}

/// rustc's "required by a bound in `item`": on the bound of a function written for `trait_`;
/// else, as rustc does for a bound it has no span of, on the item's head.
fn where_clause<'db>(
    cx: &Cx<'_, 'db>,
    diag: &mut Diag,
    item: hir::GenericDef,
    trait_: Option<hir::Trait>,
    self_ty: Option<&hir::Type<'db>>,
) {
    if let hir::GenericDef::Function(f) = item
        && let Some(trait_) = trait_
    {
        where_clause_note(cx, diag, f, trait_, self_ty);
        return;
    }
    let (def, ns) = match item {
        hir::GenericDef::Function(it) => (ModuleDef::Function(it), Ns::Value),
        hir::GenericDef::Adt(it) => (ModuleDef::Adt(it), Ns::Type),
        hir::GenericDef::Trait(it) => (ModuleDef::Trait(it), Ns::Type),
        hir::GenericDef::TypeAlias(it) => (ModuleDef::TypeAlias(it), Ns::Type),
        hir::GenericDef::Const(it) => (ModuleDef::Const(it), Ns::Value),
        hir::GenericDef::Static(it) => (ModuleDef::Static(it), Ns::Value),
        hir::GenericDef::Impl(_) => {
            diag.note("blaze cannot name the `impl` whose bound this is required by");
            return;
        }
    };
    let sized = trait_.is_some_and(|it| is_lang(cx, it, LangItem::Sized));
    let a = if sized { "an implicit `Sized`" } else { "a" };
    // alloc: the error's text, for an error hir found
    let descr = format!("required by {a} bound in `{}`", print::def_path(cx, def));
    match def_span(cx, ns, def) {
        Some(at) => diag.span_note(at, descr),
        None => diag.note(descr),
    };
}

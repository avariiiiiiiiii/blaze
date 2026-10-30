//! E0198, E0199, E0200 and E0569: an impl whose `unsafe` does not match its trait (hir's
//! `TraitImplIncorrectSafety`, which joins the four): an `unsafe` negative impl, an `unsafe`
//! impl of a safe trait, a safe impl of an `unsafe` trait, and a safe `Drop` impl with a
//! `#[may_dangle]` parameter.
//!
//! Follows rustc's `compiler/rustc_hir_analysis/src/coherence/unsafety.rs` (`check_item`) and,
//! for E0198, `compiler/rustc_ast_passes/src/ast_validation.rs` with its struct
//! `UnsafeNegativeImpl` in `compiler/rustc_ast_passes/src/diagnostics.rs` (MIT or Apache-2.0).

use blaze_items::{Fact, ItemId};

use crate::{
    cx::Cx,
    lower::{
        Found, Phase,
        incoherent_impl::{ImplAt, unresolved},
    },
    model::{Applicability, Diag},
};

const INVARIANTS: &str = "enforces invariants that the compiler can't check. Review the trait documentation and make sure this implementation upholds those invariants before adding the `unsafe` keyword";

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::TraitImplIncorrectSafety) -> Vec<Found> {
    let Some(at) = ImplAt::of_ptr(cx, d.file_id, d.impl_) else {
        return unresolved(cx, d.file_id, d.impl_, "an impl safety error");
    };
    let db = cx.db;
    let negative = at.impl_.is_negative(db);
    if d.should_be_safe && negative {
        return unsafe_negative(cx, &at);
    }
    let Some(trait_ref) = at.impl_.trait_ref(db) else {
        return unresolved(cx, d.file_id, d.impl_, "an impl safety error");
    };
    let trait_ = trait_ref.trait_();
    let printed = crate::print::trait_path(cx, &trait_ref);
    let span = at.span(cx);
    let mut diag = if d.should_be_safe {
        let mut diag =
            Diag::error(Some("E0199"), format!("implementing the trait `{printed}` is not unsafe"), span.clone());
        diag.suggestion_verbose(
            span.with_hi(span.lo + "unsafe ".len() as u32),
            "remove `unsafe` from this trait implementation",
            "",
            Applicability::MachineApplicable,
        );
        diag
    } else if trait_.is_unsafe(db) {
        let mut diag = Diag::error(
            Some("E0200"),
            format!("the trait `{printed}` requires an `unsafe impl` declaration"),
            span.clone(),
        );
        diag.note(format!("the trait `{printed}` {INVARIANTS}"));
        add_unsafe(&mut diag, &span);
        diag
    } else {
        let mut diag = Diag::error(
            Some("E0569"),
            "requires an `unsafe impl` declaration due to `#[may_dangle]` attribute",
            span.clone(),
        );
        diag.note(format!("the trait `{printed}` {INVARIANTS}"));
        add_unsafe(&mut diag, &span);
        diag
    };
    diag.fact(at.coherence(cx), "the header of the impl");
    diag.fact(Fact::Signature(cx.key(ItemId::Trait(trait_.into()))), format!("whether `{printed}` is unsafe"));
    vec![Found::new(diag, Phase::Items)]
}

fn add_unsafe(diag: &mut Diag, span: &crate::model::Span) {
    diag.suggestion_verbose(
        span.shrink_to_lo(),
        "add `unsafe` to this trait implementation",
        "unsafe ",
        Applicability::MaybeIncorrect,
    );
}

/// E0198, which rustc's AST validation gives: from the `!` to the end of the trait's path.
fn unsafe_negative(cx: &Cx<'_, '_>, at: &ImplAt) -> Vec<Found> {
    let node = &at.node.value;
    let (Some(excl), Some(unsafe_)) = (node.excl_token(), node.unsafe_token()) else {
        return unresolved(cx, at.node.file_id, syntax::AstPtr::new(node), "an unsafe negative impl error");
    };
    let negative = cx.span(at.node.with_value(excl.text_range()));
    let path_end = at.trait_path().map_or(negative.hi, |path| at.span_of(cx, &path).hi);
    let span = negative.with_hi(path_end.max(negative.hi));
    let mut diag = Diag::error(Some("E0198"), "negative impls cannot be unsafe", span);
    diag.span_label(negative, "negative because of this");
    diag.span_label(cx.span(at.node.with_value(unsafe_.text_range())), "unsafe because of this");
    diag.fact(at.coherence(cx), "the header of the impl");
    vec![Found::new(diag, Phase::Resolve)]
}

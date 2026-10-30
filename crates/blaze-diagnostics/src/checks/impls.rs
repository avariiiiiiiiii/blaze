//! The errors rustc gives of impls that hir reports nothing of, in rustc's order: coherence
//! trait by trait (`coherent_trait`: the explicit impls of a trait that forbids them, the
//! orphan rules where hir's check takes what rustc refuses, the builtin traits' checks), then
//! each trait impl's items against its trait's (`compare_impl_item`) and the duplicates of an
//! impl's items.
//!
//! What needs trait solving or inference is the front end's and asked of it
//! (`v2/requests/diagnostics.md`, `diagnostics-items-impls`): overlap (E0119, E0592 across
//! impls), unconstrained parameters (E0207), signatures that differ in type (E0053), `Drop`
//! impls with more bounds than their type (E0367).
//!
//! Follows rustc's `compiler/rustc_hir_analysis/src/coherence/mod.rs` (`coherent_trait`,
//! `check_impl`, `enforce_trait_manually_implementable`) and
//! `compiler/rustc_hir_analysis/src/coherence/orphan.rs` (`orphan_check_impl`) (MIT or
//! Apache-2.0).

mod builtin;
mod compare;

use blaze_items::{Fact, ItemId};
use hir::HasSource as _;
use hir_def::lang_item::lang_items;
use syntax::ast::HasAttrs as _;

use crate::{
    cx::Cx,
    lower::{Found, Phase, incoherent_impl::ImplAt, trait_impl_orphan},
    model::Diag,
};

pub(crate) fn check(cx: &Cx<'_, '_>) -> Vec<Found> {
    let db = cx.db;
    let impls: Vec<hir::Impl> =
        crate::checks::modules(cx).into_iter().flat_map(|module| module.impl_defs(db)).collect();
    let mut found = Vec::new();
    // rustc's `local_trait_impls`: the traits in the order their first impl is written.
    let mut traits: Vec<(hir::Trait, Vec<hir::Impl>)> = Vec::new();
    for &impl_ in &impls {
        let Some(trait_) = impl_.trait_(db) else { continue };
        match traits.iter_mut().find(|(it, _)| *it == trait_) {
            Some((_, impls)) => impls.push(impl_),
            None => traits.push((trait_, vec![impl_])),
        }
    }
    for (trait_, impls) in &traits {
        for &impl_ in impls {
            let at = ImplAt::of(cx, impl_);
            if let Some(at) = &at {
                found.extend(explicit_impl(cx, at, *trait_));
                if impl_.check_orphan_rules(db) {
                    found.extend(trait_impl_orphan::report(cx, at));
                }
            }
            found.extend(builtin::check(cx, impl_, at.as_ref(), *trait_));
        }
    }
    for &impl_ in &impls {
        let Some(at) = ImplAt::of(cx, impl_) else { continue };
        found.extend(compare::check(cx, &at));
    }
    found
}

/// `enforce_trait_manually_implementable`: E0322 (E0328 for `Unsize`) for an impl of a trait
/// marked `#[rustc_deny_explicit_impl]`.
fn explicit_impl(cx: &Cx<'_, '_>, at: &ImplAt, trait_: hir::Trait) -> Vec<Found> {
    let db = cx.db;
    let Some(source) = trait_.source(db) else { return Vec::new() };
    let denied = source.value.attrs().any(|attr| {
        attr.path().and_then(|path| path.as_single_name_ref()).is_some_and(|name| name.text() == "rustc_deny_explicit_impl")
    });
    if !denied {
        return Vec::new();
    }
    let name = trait_.name(db);
    let name = name.as_str();
    let unsize = lang_items(db, cx.krate.base()).Unsize == Some(trait_.into());
    let span = at.span(cx);
    let code = if unsize { "E0328" } else { "E0322" };
    let mut diag =
        Diag::error(Some(code), format!("explicit impls for the `{name}` trait are not permitted"), span.clone());
    diag.span_label(span, format!("impl of `{name}` not allowed"));
    diag.fact(Fact::Signature(cx.key(ItemId::Trait(trait_.into()))), format!("the attributes of `{name}`"));
    diag.fact(at.coherence(cx), "the header of the impl");
    vec![Found::new(diag, Phase::Items)]
}

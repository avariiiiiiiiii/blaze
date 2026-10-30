//! The errors rustc gives of impls that hir reports nothing of, in rustc's order: coherence
//! trait by trait (`coherent_trait`: the explicit impls of a trait that forbids them, the
//! orphan rules where hir's check takes what rustc refuses, the builtin traits' checks), then
//! each impl's items: duplicates (E0201 of a trait impl, which the resolver gives; E0592 of an
//! inherent one) and each trait impl item against its trait's (`compare_impl_item`).
//!
//! What needs trait solving or inference is the front end's and asked of it
//! (`v2/requests/diagnostics.md`, `diagnostics-items-impls`): overlap (E0119, E0592 across
//! impls), unconstrained parameters (E0207), signatures that differ in type (E0053), lifetime
//! parameters that differ in being early or late bound (E0195), `Drop` impls with more bounds
//! than their type (E0367), the coercion traits (E0374 to E0378).
//!
//! Follows rustc's `compiler/rustc_hir_analysis/src/coherence/mod.rs` (`coherent_trait`,
//! `check_impl`, `enforce_trait_manually_implementable`) and
//! `compiler/rustc_hir_analysis/src/coherence/orphan.rs` (`orphan_check_impl`) (MIT or
//! Apache-2.0).

mod builtin;
mod compare;

use blaze_items::{Fact, ItemId};
use hir::HasSource as _;
use hir_def::{BuiltinDeriveImplId, lang_item::lang_items};
use syntax::{
    AstNode as _,
    ast::{HasAttrs as _, HasName as _},
};

use crate::{
    cx::Cx,
    lower::{Found, Phase, incoherent_impl::ImplAt, trait_impl_orphan},
    model::{Diag, Expansion, MacroKind, Span},
};

/// Where an impl is: written in source, or made by a builtin derive of a type.
pub(super) enum Origin {
    Written(ImplAt),
    Derive { id: BuiltinDeriveImplId, adt: hir::Adt },
}

impl Origin {
    /// The span rustc gives the impl's self type: as written, or for a derive the type's name
    /// in the derive's expansion.
    pub(super) fn self_ty_span(&self, cx: &Cx<'_, '_>) -> Option<Span> {
        match self {
            Origin::Written(at) => Some(at.self_ty_span(cx)),
            Origin::Derive { id, adt } => {
                let source = adt.source(cx.db)?;
                let name = source.value.name()?;
                let mut span = cx.span(source.with_value(name.syntax().text_range()));
                let loc = id.loc(cx.db);
                span.expansion = Some(Box::new(Expansion {
                    call_site: cx.span(loc.source(cx.db)),
                    kind: MacroKind::Derive,
                    name: loc.trait_.name().as_str().to_owned(),
                    def_site: None,
                }));
                Some(span)
            }
        }
    }

    /// The fact of the impl's coherence: its own, or for a derive the derives of its type.
    pub(super) fn coherence(&self, cx: &Cx<'_, '_>) -> Fact {
        match self {
            Origin::Written(at) => at.coherence(cx),
            Origin::Derive { adt, .. } => Fact::Derive(cx.key(ItemId::Adt((*adt).into()))),
        }
    }

    pub(super) fn written(&self) -> Option<&ImplAt> {
        match self {
            Origin::Written(at) => Some(at),
            Origin::Derive { .. } => None,
        }
    }
}

pub(crate) fn check(cx: &Cx<'_, '_>) -> Vec<Found> {
    let db = cx.db;
    let impls = impls(cx);
    let mut found = Vec::new();
    // rustc's `local_trait_impls`: the traits in the order their first impl is written.
    let mut traits: Vec<(hir::Trait, Vec<usize>)> = Vec::new();
    for (index, (impl_, _)) in impls.iter().enumerate() {
        let Some(trait_) = impl_.trait_(db) else { continue };
        match traits.iter_mut().find(|(it, _)| *it == trait_) {
            Some((_, of)) => of.push(index),
            None => traits.push((trait_, vec![index])),
        }
    }
    let mut builtin = builtin::Builtin::new(cx);
    for (trait_, of) in &traits {
        for &index in of {
            let (impl_, origin) = &impls[index];
            if let Origin::Written(at) = origin {
                found.extend(explicit_impl(cx, at, *trait_));
                if impl_.check_orphan_rules(db) {
                    found.extend(trait_impl_orphan::report(cx, at));
                }
            }
            found.extend(builtin.check(*impl_, origin, *trait_));
        }
    }
    for (_, origin) in &impls {
        if let Origin::Written(at) = origin {
            found.extend(compare::check(cx, at));
        }
    }
    found
}

/// The impls of the crate, in rustc's order: each scope's (`defs::scopes`), as they are
/// written, a derive's where its type is.
fn impls(cx: &Cx<'_, '_>) -> Vec<(hir::Impl, Origin)> {
    let db = cx.db;
    let mut all = Vec::new();
    for module in super::defs::scopes(cx) {
        let id = hir_def::ModuleId::from(module);
        let scope = id.def_map(db).module(db, id).scope(db);
        let mut here: Vec<(u32, hir::Impl, Origin)> = Vec::new();
        for impl_id in scope.impls() {
            let impl_ = hir::Impl::from(impl_id);
            let Some(at) = ImplAt::of(cx, impl_) else { continue };
            here.push((at.span(cx).lo, impl_, Origin::Written(at)));
        }
        for derive in scope.builtin_derive_impls() {
            let adt = hir::Adt::from(derive.loc(db).adt);
            let at = cx.span(derive.loc(db).source(db)).lo;
            here.push((at, hir::Impl::from(derive), Origin::Derive { id: derive, adt }));
        }
        here.sort_by_key(|(at, _, _)| *at);
        all.extend(here.into_iter().map(|(_, impl_, origin)| (impl_, origin)));
    }
    all
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

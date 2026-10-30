//! `missing_copy_implementations` and `missing_debug_implementations` (both allowed by
//! default) of a reachable struct, enum or union.
//!
//! `Copy` is suggested for a type with no generic parameters, no `Drop` impl, no raw pointer
//! field, not `Copy` or `!Copy` already, not `#[non_exhaustive]` nor with such a variant, not
//! an iterator, at most 256 bytes and all of whose fields are `Copy`. `Debug` for a type with no
//! impl of it for itself, and, without generic parameters, none that applies.
//!
//! Follows `compiler/rustc_lint/src/builtin.rs` (`MissingCopyImplementations`,
//! `MissingDebugImplementations`) and `compiler/rustc_trait_selection/src/traits/misc.rs`
//! (`type_allowed_to_implement_copy`) (MIT or Apache-2.0).

use blaze_items::Fact;
use hir::{Adt, Field};
use hir_def::lang_item::lang_items;
use syntax::{
    AstNode,
    ast::{self, HasGenericParams},
};

use super::{
    visibility::{Def, Effective},
    walk::{self, Item},
};
use crate::{
    cx::{Cx, item_range},
    lower::{Found, Phase},
    model::Diag,
};

/// The type an item defines, when it is a struct, an enum or a union with no generic
/// parameters (`Some(false)`) or some (`Some(true)`).
fn adt(cx: &Cx<'_, '_>, item: &Item) -> Option<(Adt, bool)> {
    let node = item.node.as_ref();
    let (adt, params): (Adt, Option<ast::GenericParamList>) = match node.value {
        ast::Item::Struct(it) => (cx.sema.to_def2(node.with_value(it))?.into(), it.generic_param_list()),
        ast::Item::Enum(it) => (cx.sema.to_def2(node.with_value(it))?.into(), it.generic_param_list()),
        ast::Item::Union(it) => (cx.sema.to_def2(node.with_value(it))?.into(), it.generic_param_list()),
        _ => return None,
    };
    Some((adt, params.is_some_and(|it| it.generic_params().next().is_some())))
}

fn fields(cx: &Cx<'_, '_>, adt: Adt) -> Vec<Field> {
    match adt {
        Adt::Struct(it) => it.fields(cx.db),
        Adt::Union(it) => it.fields(cx.db),
        Adt::Enum(it) => it.variants(cx.db).into_iter().flat_map(|it| it.fields(cx.db)).collect(),
    }
}

pub(super) fn copy(cx: &Cx<'_, '_>, item: &Item, effective: &Effective) -> Vec<Found> {
    let Some((adt, false)) = adt(cx, item) else { return Vec::new() };
    if !effective.is_reachable(Def::Item(adt.into())) {
        return Vec::new();
    }
    let db = cx.db;
    let lang = lang_items(db, cx.krate.into());
    let ty = adt.ty(db);
    let impls = hir::Impl::all_for_type(db, ty.clone());
    let impl_of = |trait_: Option<hir_def::TraitId>, negative: bool| {
        trait_.is_some_and(|trait_| {
            impls.iter().any(|it| {
                it.trait_(db).is_some_and(|t| hir_def::TraitId::from(t) == trait_) && it.is_negative(db) == negative
            })
        })
    };
    if impl_of(lang.Drop, false) {
        return Vec::new();
    }
    let fields = fields(cx, adt);
    if fields.iter().any(|it| it.ty(db).is_raw_ptr()) || ty.is_copy(db) || impl_of(lang.Copy, true) {
        return Vec::new();
    }
    let non_exhaustive = walk::attr(cx, &item.node.value, "non_exhaustive").is_some()
        || item.own::<ast::Variant>().any(|it| walk::attr(cx, &it.value, "non_exhaustive").is_some());
    if non_exhaustive || ty.clone().impls_iterator(db) {
        return Vec::new();
    }
    if ty.layout(db).is_ok_and(|it| it.size() > 256) {
        return Vec::new();
    }
    if !fields.iter().all(|it| it.ty(db).is_copy(db)) {
        return Vec::new();
    }
    let span = cx.span(item.node.with_value(item_range(item.node.value.syntax())));
    let mut diag = Diag::lint("missing_copy_implementations", "type could implement `Copy`; consider adding `impl Copy`", span);
    if let Some((id, name)) = super::item_id(cx, item) {
        diag.fact(Fact::Layout(cx.key(id)), format!("the fields of `{name}`"));
    }
    vec![Found::lint(diag, Phase::Late, item.ptr())]
}

pub(super) fn debug(cx: &Cx<'_, '_>, item: &Item, effective: &Effective) -> Vec<Found> {
    let Some((adt, generic)) = adt(cx, item) else { return Vec::new() };
    if !effective.is_reachable(Def::Item(adt.into())) {
        return Vec::new();
    }
    let db = cx.db;
    let Some(debug) = lang_items(db, cx.krate.into()).Debug else { return Vec::new() };
    let ty = adt.ty(db);
    let own = hir::Impl::all_for_type(db, ty.clone())
        .into_iter()
        .any(|it| it.trait_(db).is_some_and(|t| hir_def::TraitId::from(t) == debug));
    if own || (!generic && ty.impls_trait(db, hir::Trait::from(debug), &[])) {
        return Vec::new();
    }
    let span = cx.span(item.node.with_value(item_range(item.node.value.syntax())));
    let message = "type does not implement `Debug`; consider adding `#[derive(Debug)]` or a manual implementation";
    let mut diag = Diag::lint("missing_debug_implementations", message, span);
    if let Some((id, name)) = super::item_id(cx, item) {
        diag.fact(Fact::Derive(cx.key(id)), format!("the derives of `{name}`"));
    }
    vec![Found::lint(diag, Phase::Late, item.ptr())]
}

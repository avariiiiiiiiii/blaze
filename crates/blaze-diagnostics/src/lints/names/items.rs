//! The lints of items rustc gives, each found as its pass finds it:
//!
//! - before the abort, `rustc_hir_analysis`'s of collecting and checking items
//!   (`invalid_type_param_default`, `uninhabited_static`: `statics`) and resolve's late pass
//!   over lifetimes (`unused_lifetimes`, `single_use_lifetimes`: `lifetimes`);
//! - after it, `rustc_privacy`'s `check_private_in_public` (`private_interfaces`,
//!   `private_bounds`, E0446: `privacy`), `rustc_lint`'s late module pass over each item
//!   (`externs`' `improper_ctypes`, `naming`, `derives`, `bounds`, `mangle`, `publicity`,
//!   `track_caller`, `docs`, `traits`, `non_local`, `runtime_symbols`, `lifetime_syntax`, `c_void`), and `clashing_extern_declarations` (`externs`).
//!
//! `walk` gives the items in the order rustc's passes visit them; `visibility` their effective
//! visibilities, as `rustc_privacy` computes them.
//!
//! Follows `compiler/rustc_interface/src/passes.rs` (`run_required_analyses`, `analysis`,
//! `misc_checking_3`), `compiler/rustc_lint/src/late.rs` (`late_lint_mod_inner`: the crate's
//! `check_crate` before the root module's items) and `compiler/rustc_lint/src/lib.rs`
//! (`BuiltinCombinedLateLintModPass`: the order of the passes at each item) (MIT or
//! Apache-2.0).

mod bounds;
mod c_void;
mod derives;
mod docs;
mod externs;
mod lifetime_syntax;
mod lifetimes;
mod mangle;
mod naming;
mod non_local;
mod privacy;
mod publicity;
mod runtime_symbols;
mod statics;
mod track_caller;
mod traits;
mod visibility;
mod walk;

use std::cell::OnceCell;

use blaze_items::ItemId;
use hir_def::{AdtId, FunctionId, lang_item::lang_items, unstable_features::UnstableFeatures};
use intern::Symbol;
use syntax::{
    AstNode, SyntaxNode, TextRange, TextSize,
    ast::{self, HasGenericParams, HasName, HasTypeBounds},
};

use self::walk::Item;
use crate::{
    cx::{Cx, def_range, item_range, signature_range},
    lower::Found,
};

/// What `rustc_hir_analysis` and resolve's late pass lint of items, given whatever else failed.
pub(super) fn before_abort(cx: &Cx<'_, '_>) -> Vec<Found> {
    let items = walk::items(cx);
    let mut found = lifetimes::lints(cx, &items);
    for item in &items {
        found.extend(statics::item(cx, item));
    }
    found
}

/// The crate's items in the order rustc visits them, walked once for every pass after the abort.
pub(super) struct Walk {
    items: Vec<Item>,
}

impl Walk {
    pub(super) fn of(cx: &Cx<'_, '_>) -> Walk {
        Walk { items: walk::items(cx) }
    }
}

/// `check_private_in_public`: `private_interfaces` and `private_bounds`.
pub(super) fn private_in_public(cx: &Cx<'_, '_>, walk: &Walk) -> Vec<Found> {
    privacy::lints(cx, &walk.items)
}

/// rustc's late module pass over each item, in the order of `BuiltinCombinedLateLintModPass`.
pub(super) fn after_abort(cx: &Cx<'_, '_>, walk: &Walk) -> Vec<Found> {
    let items = &walk.items;
    let effective = visibility::Effective::of(cx, items);
    let canonical = runtime_symbols::Canonical::of(cx, items);
    let krate = Krate::of(cx);
    let mut found = docs::krate(cx);
    found.extend(naming::krate(cx));
    for (index, item) in items.iter().enumerate() {
        found.extend(externs::ctypes_item(cx, item, &krate));
        found.extend(naming::item(cx, item));
        found.extend(derives::copy(cx, item, &effective, &krate));
        found.extend(bounds::type_alias(cx, item, &krate));
        found.extend(mangle::item(cx, item));
        found.extend(publicity::item(cx, item, &effective));
        found.extend(track_caller::item(cx, item, krate.async_fn_track_caller));
        found.extend(bounds::drop(cx, item, &krate));
        found.extend(derives::debug(cx, item, &effective, &krate));
        found.extend(docs::item(cx, item, &effective));
        found.extend(traits::async_fn(cx, item, &effective, &krate));
        found.extend(non_local::item(cx, item));
        found.extend(runtime_symbols::item(cx, item, item.held(items, index), &canonical));
        found.extend(externs::ctypes_nested(cx, item));
        found.extend(lifetime_syntax::item(cx, item, krate.anonymous_lifetime_in_impl_trait));
        found.extend(c_void::item(cx, item, &krate));
    }
    found
}

/// What the late passes read of the crate itself, read once for all of its items.
pub(super) struct Krate {
    pub(super) anonymous_lifetime_in_impl_trait: bool,
    pub(super) async_fn_track_caller: bool,
    pub(super) checked_type_aliases: bool,
    pub(super) return_type_notation: bool,
    /// The target is AIX, the one `uses_power_alignment` applies to.
    pub(super) aix: bool,
    pub(super) c_void: Option<hir::Enum>,
    pub(super) drop: Option<hir_def::TraitId>,
    pub(super) copy: Option<hir_def::TraitId>,
    pub(super) debug: Option<hir_def::TraitId>,
    /// `core::mem::needs_drop` as rustc prints it, found when a `Drop` bound first needs it.
    needs_drop: OnceCell<Option<String>>,
}

impl Krate {
    fn of(cx: &Cx<'_, '_>) -> Krate {
        let features = UnstableFeatures::query(cx.db, cx.krate.base());
        let enabled = |name: &str| features.is_enabled(&Symbol::intern(name));
        let lang = lang_items(cx.db, cx.krate.into());
        Krate {
            anonymous_lifetime_in_impl_trait: enabled("anonymous_lifetime_in_impl_trait"),
            async_fn_track_caller: enabled("async_fn_track_caller"),
            checked_type_aliases: enabled("checked_type_aliases"),
            return_type_notation: enabled("return_type_notation"),
            aix: cx.krate.cfg(cx.db).get_cfg_values("target_os").any(|it| it.as_str() == "aix"),
            c_void: lang.CVoid.map(hir::Enum::from),
            drop: lang.Drop,
            copy: lang.Copy,
            debug: lang.Debug,
            needs_drop: OnceCell::new(),
        }
    }

    pub(super) fn needs_drop(&self, cx: &Cx<'_, '_>) -> Option<&str> {
        self.needs_drop.get_or_init(|| core_path(cx, "mem", "needs_drop")).as_deref()
    }
}

/// `clashing_extern_declarations`, its own query after the late lint passes.
pub(super) fn clashing_extern_declarations(cx: &Cx<'_, '_>, walk: &Walk) -> Vec<Found> {
    externs::lints(cx, &walk.items)
}

/// rustc's `def_span` of an item, an associated or foreign item, a variant or a field: a
/// function's signature; an impl up to its `where` clause; a const or a static up to its type;
/// a trait or an associated type up to its last bound; a `use` its tree; a field all of it;
/// else up to its name and generics.
pub(super) fn def_span(node: &SyntaxNode) -> TextRange {
    let range = item_range(node);
    let until = |end: Option<TextSize>| TextRange::new(range.start(), end.unwrap_or(range.end()).max(range.start()));
    let end_of = |it: Option<&SyntaxNode>| it.map(|it| it.text_range().end());
    if let Some(it) = ast::Fn::cast(node.clone()) {
        return signature_range(&it);
    }
    if ast::Impl::can_cast(node.kind()) {
        return def_range(node);
    }
    if let Some(it) = ast::Const::cast(node.clone()) {
        return until(end_of(it.ty().as_ref().map(AstNode::syntax)));
    }
    if let Some(it) = ast::Static::cast(node.clone()) {
        return until(end_of(it.ty().as_ref().map(AstNode::syntax)));
    }
    if ast::RecordField::can_cast(node.kind()) || ast::TupleField::can_cast(node.kind()) || ast::ExternBlock::can_cast(node.kind()) {
        return range;
    }
    if let Some(it) = ast::UseTree::cast(node.clone()) {
        return it.syntax().text_range();
    }
    let named = |name: Option<ast::Name>, generics: Option<ast::GenericParamList>| {
        let name = end_of(name.as_ref().map(AstNode::syntax));
        until(end_of(generics.as_ref().map(AstNode::syntax)).or(name))
    };
    if let Some(it) = ast::Trait::cast(node.clone()) {
        let bound = it.type_bound_list().and_then(|it| it.bounds().last());
        if let Some(bound) = bound {
            return until(end_of(Some(bound.syntax())));
        }
        return named(it.name(), it.generic_param_list());
    }
    if let Some(it) = ast::TypeAlias::cast(node.clone()) {
        let bound = it.type_bound_list().and_then(|it| it.bounds().last());
        if let Some(bound) = bound {
            return until(end_of(Some(bound.syntax())));
        }
        return named(it.name(), it.generic_param_list());
    }
    let name = node.children().find_map(ast::Name::cast);
    let generics = node.children().find_map(ast::GenericParamList::cast);
    if let Some(it) = ast::ExternCrate::cast(node.clone()) {
        let rename = it.rename().and_then(|it| it.name());
        let name_ref = it.name_ref().map(|it| it.syntax().text_range().end());
        return until(end_of(rename.as_ref().map(AstNode::syntax)).or(name_ref));
    }
    match name {
        Some(_) => named(name, generics),
        None => range,
    }
}

/// Whether the crate enables the feature `name`: its `#![feature(..)]`s, `cfg_attr`s expanded,
/// as its def map collected them (one interned-symbol lookup).
pub(super) fn feature(cx: &Cx<'_, '_>, name: &str) -> bool {
    UnstableFeatures::query(cx.db, cx.krate.base()).is_enabled(&Symbol::intern(name))
}

/// The Rust path of `core`'s `name` in `module` as rustc prints it: through `std` when the
/// crate has it.
fn core_path(cx: &Cx<'_, '_>, module: &str, name: &str) -> Option<String> {
    let mut core = None;
    let mut std = false;
    for dep in cx.krate.dependencies(cx.db) {
        match dep.name.symbol().as_str() {
            "core" => core = Some(dep.krate),
            "std" => std = true,
            _ => {}
        }
    }
    let core = core?;
    let root = core.root_module(cx.db);
    let module = root.children(cx.db).find(|it| it.name(cx.db).is_some_and(|it| it.as_str() == module))?;
    module.declarations(cx.db).into_iter().find(|it| it.name(cx.db).is_some_and(|it| it.as_str() == name))?;
    let krate = if std { "std" } else { "core" };
    Some(format!("{krate}::{}::{name}", module.name(cx.db)?.as_str()))
}

/// The item `item` defines, and its name, for the facts of its lints.
fn item_id(cx: &Cx<'_, '_>, item: &Item) -> Option<(ItemId, String)> {
    let node = item.node.as_ref();
    let name = |it: &dyn HasName| it.name().map_or_else(|| "_".to_owned(), |it| it.text().to_string());
    let id = match node.value {
        ast::Item::Fn(it) => ItemId::Function(FunctionId::try_from(cx.sema.to_def2(node.with_value(it))?).ok()?),
        ast::Item::Struct(it) => ItemId::Adt(AdtId::StructId(cx.sema.to_def2(node.with_value(it))?.into())),
        ast::Item::Enum(it) => ItemId::Adt(AdtId::EnumId(cx.sema.to_def2(node.with_value(it))?.into())),
        ast::Item::Union(it) => ItemId::Adt(AdtId::UnionId(cx.sema.to_def2(node.with_value(it))?.into())),
        ast::Item::Trait(it) => ItemId::Trait(cx.sema.to_def2(node.with_value(it))?.into()),
        ast::Item::Const(it) => ItemId::Const(cx.sema.to_def2(node.with_value(it))?.into()),
        ast::Item::Static(it) => ItemId::Static(cx.sema.to_def2(node.with_value(it))?.into()),
        ast::Item::TypeAlias(it) => ItemId::Alias(cx.sema.to_def2(node.with_value(it))?.into()),
        ast::Item::Impl(it) => {
            let id = hir_def::ImplId::try_from(cx.sema.to_def2(node.with_value(it))?).ok()?;
            return Some((ItemId::Impl(id), "impl".to_owned()));
        }
        _ => return None,
    };
    let name = match node.value {
        ast::Item::Fn(it) => name(it),
        ast::Item::Struct(it) => name(it),
        ast::Item::Enum(it) => name(it),
        ast::Item::Union(it) => name(it),
        ast::Item::Trait(it) => name(it),
        ast::Item::Const(it) => name(it),
        ast::Item::Static(it) => name(it),
        ast::Item::TypeAlias(it) => name(it),
        _ => return None,
    };
    Some((id, name))
}

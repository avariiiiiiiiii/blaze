//! The lints of items rustc gives, each found as its pass finds it:
//!
//! - before the abort, `rustc_hir_analysis`'s of collecting and checking items
//!   (`invalid_type_param_default`, `uninhabited_static`: `statics`) and resolve's late pass
//!   over lifetimes (`unused_lifetimes`, `single_use_lifetimes`: `lifetimes`);
//! - after it, `rustc_privacy`'s `check_private_in_public` (`private_interfaces`,
//!   `private_bounds`: `privacy`), `rustc_lint`'s late module pass over each item
//!   (`naming`, `derives`, `bounds`, `mangle`, `publicity`, `docs`, `traits`), and
//!   `clashing_extern_declarations` (`externs`).
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
mod derives;
mod docs;
mod externs;
mod lifetimes;
mod mangle;
mod naming;
mod privacy;
mod publicity;
mod statics;
mod traits;
mod visibility;
mod walk;

use blaze_items::ItemId;
use hir_def::{AdtId, FunctionId};
use syntax::ast::{self, HasName};

use self::walk::Item;
use crate::{cx::Cx, lower::Found};

/// What `rustc_hir_analysis` and resolve's late pass lint of items, given whatever else failed.
pub(super) fn before_abort(cx: &Cx<'_, '_>) -> Vec<Found> {
    let items = walk::items(cx);
    let mut found = lifetimes::lints(cx, &items);
    for item in &items {
        found.extend(statics::item(cx, item));
    }
    found
}

/// `check_private_in_public`: `private_interfaces` and `private_bounds`.
pub(super) fn private_in_public(cx: &Cx<'_, '_>) -> Vec<Found> {
    let items = walk::items(cx);
    let effective = visibility::Effective::of(cx, &items);
    privacy::lints(cx, &items, &effective)
}

/// rustc's late module pass over each item, in the order of `BuiltinCombinedLateLintModPass`.
pub(super) fn after_abort(cx: &Cx<'_, '_>) -> Vec<Found> {
    let items = walk::items(cx);
    let effective = visibility::Effective::of(cx, &items);
    let mut found = docs::krate(cx);
    found.extend(naming::krate(cx));
    for item in &items {
        found.extend(naming::item(cx, item));
        found.extend(derives::copy(cx, item, &effective));
        found.extend(bounds::type_alias(cx, item));
        found.extend(mangle::item(cx, item));
        found.extend(publicity::item(cx, item, &effective));
        found.extend(bounds::drop(cx, item));
        found.extend(derives::debug(cx, item, &effective));
        found.extend(docs::item(cx, item, &effective));
        found.extend(traits::async_fn(cx, item, &effective));
    }
    found
}

/// `clashing_extern_declarations`, its own query after the late lint passes.
pub(super) fn clashing_extern_declarations(cx: &Cx<'_, '_>) -> Vec<Found> {
    externs::lints(cx, &walk::items(cx))
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

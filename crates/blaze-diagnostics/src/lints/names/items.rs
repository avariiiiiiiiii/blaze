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

/// `check_private_in_public`: `private_interfaces` and `private_bounds`.
pub(super) fn private_in_public(cx: &Cx<'_, '_>) -> Vec<Found> {
    privacy::lints(cx, &walk::items(cx))
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

/// Whether the crate's root enables the feature `name` (`#![feature(name)]`, its `cfg_attr`s
/// expanded).
pub(super) fn feature(cx: &Cx<'_, '_>, name: &str) -> bool {
    let root = cx.krate.root_module(cx.db);
    let file = root.definition_source_file_id(cx.db);
    let Some(source_file) = ast::SourceFile::cast(cx.sema.parse_or_expand(file)) else { return false };
    walk::attrs_named(cx, &source_file, "feature").into_iter().any(|(meta, _)| match meta {
        ast::Meta::TokenTreeMeta(it) => it.token_tree().is_some_and(|tt| {
            tt.syntax()
                .children_with_tokens()
                .filter_map(syntax::NodeOrToken::into_token)
                .any(|token| token.kind() == syntax::SyntaxKind::IDENT && token.text() == name)
        }),
        _ => false,
    })
}

/// The Rust path of `core`'s `name` in `module` as rustc prints it: through `std` when the
/// crate has it.
pub(super) fn core_path(cx: &Cx<'_, '_>, module: &str, name: &str) -> Option<String> {
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

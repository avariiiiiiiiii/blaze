//! What a path, a method call or a pattern names, as rustc's `Res` the dead-code pass handles:
//! a definition of the crate, a variant (which a pattern does not make live), or nothing it
//! marks (a local, a generic parameter, a primitive, a module, a macro, an item of another
//! crate). An item of a trait impl that the front end resolved a call or a path to is the
//! trait's item, as typeck's `type_dependent_def_id` is: the impl's item is live through the
//! trait's.
//!
//! Follows `compiler/rustc_passes/src/dead.rs` (`handle_res`) and
//! `compiler/rustc_hir/src/def.rs` (`Res`) (MIT or Apache-2.0).

use hir::{AsAssocItem, AssocItem, GenericDef, ModuleDef, PathResolution};
use hir_def::{AdtId, FunctionId};

use super::defs::{DefIdx, Defs, Id};
use crate::cx::Cx;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Res {
    Def(DefIdx),
    /// A variant or its constructor.
    Variant(DefIdx),
    /// `Self` as a value or a pattern: rustc's `Res::SelfCtor`, which marks nothing.
    SelfCtor,
    None,
}

impl Res {
    fn of(defs: &Defs, id: Id) -> Res {
        defs.get(id).map_or(Res::None, Res::Def)
    }

    /// `res`, written as the path `Self` alone in a value or a pattern when `self_ctor`.
    pub(super) fn path(cx: &Cx<'_, '_>, defs: &Defs, res: PathResolution<'_>, self_ctor: bool) -> Res {
        match res {
            PathResolution::Def(def) => Res::def(cx, defs, def),
            PathResolution::SelfType(_) if self_ctor => Res::SelfCtor,
            PathResolution::SelfType(it) => match hir_def::ImplId::try_from(it) {
                Ok(id) => Res::of(defs, Id::Impl(id)),
                Err(()) => Res::None,
            },
            // `Self` in a trait: rustc's `Res::SelfTyParam { trait_ }`.
            PathResolution::TypeParam(it) => match it.parent(cx.db) {
                GenericDef::Trait(trait_) if it.is_implicit(cx.db) => Res::of(defs, Id::Trait(trait_.into())),
                _ => Res::None,
            },
            PathResolution::Local(_)
            | PathResolution::ConstParam(_)
            | PathResolution::BuiltinAttr(_)
            | PathResolution::ToolModule(_)
            | PathResolution::DeriveHelper(_) => Res::None,
        }
    }

    pub(super) fn def(cx: &Cx<'_, '_>, defs: &Defs, def: ModuleDef) -> Res {
        match def {
            ModuleDef::Function(it) => Res::function(cx, defs, it),
            ModuleDef::Adt(it) => Res::of(defs, Id::Adt(AdtId::from(it))),
            ModuleDef::EnumVariant(it) => defs.get(Id::Variant(it.into())).map_or(Res::None, Res::Variant),
            ModuleDef::Const(it) => match trait_item(cx, AssocItem::Const(it)) {
                AssocItem::Const(it) => Res::of(defs, Id::Const(it.into())),
                _ => Res::None,
            },
            ModuleDef::Static(it) => Res::of(defs, Id::Static(it.into())),
            ModuleDef::Trait(it) => Res::of(defs, Id::Trait(it.into())),
            ModuleDef::TypeAlias(it) => match trait_item(cx, AssocItem::TypeAlias(it)) {
                AssocItem::TypeAlias(it) => Res::of(defs, Id::Alias(it.into())),
                _ => Res::None,
            },
            ModuleDef::Module(_) | ModuleDef::BuiltinType(_) | ModuleDef::Macro(_) => Res::None,
        }
    }

    /// A function a path or a method call resolved to.
    pub(super) fn function(cx: &Cx<'_, '_>, defs: &Defs, function: hir::Function) -> Res {
        let function = match function.as_assoc_item(cx.db) {
            Some(item) => match trait_item(cx, item) {
                AssocItem::Function(it) => it,
                _ => return Res::None,
            },
            None => function,
        };
        match FunctionId::try_from(function) {
            Ok(id) => Res::of(defs, Id::Fn(id)),
            Err(()) => Res::None,
        }
    }
}

/// The item of the trait that `item`, an item of a trait impl, implements; else `item`.
fn trait_item(cx: &Cx<'_, '_>, item: AssocItem) -> AssocItem {
    let db = cx.db;
    let Some(trait_) = item.implemented_trait(db) else { return item };
    let Some(name) = item.name(db) else { return item };
    let same_kind = |it: &AssocItem| {
        matches!(
            (it, item),
            (AssocItem::Function(_), AssocItem::Function(_))
                | (AssocItem::Const(_), AssocItem::Const(_))
                | (AssocItem::TypeAlias(_), AssocItem::TypeAlias(_))
        )
    };
    trait_
        .items(db)
        .into_iter()
        .find(|it| same_kind(it) && it.name(db).as_ref() == Some(&name))
        .unwrap_or(item)
}

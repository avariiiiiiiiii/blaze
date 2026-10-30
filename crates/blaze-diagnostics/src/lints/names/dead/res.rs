//! What a path, a method call or a pattern names, as rustc's `Res` the dead-code pass handles:
//! a definition of the crate, a variant (which a pattern does not make live), or nothing it
//! marks (a local, a generic parameter, a primitive, a module, a macro, an item of another
//! crate). An item of a trait impl that the front end resolved a call or a path to is the
//! trait's item, as typeck's `type_dependent_def_id` is: the impl's item is live through the
//! trait's.
//!
//! The trait's item is found in `Defs` (an impl's resolved trait, then its children by kind and
//! interned name), with no query: an item of another crate's impl can only implement another
//! crate's trait, so it is never one of the crate's.
//!
//! Follows `compiler/rustc_passes/src/dead.rs` (`handle_res`) and
//! `compiler/rustc_hir/src/def.rs` (`Res`) (MIT or Apache-2.0).

use hir::{GenericDef, ModuleDef, PathResolution};
use hir_def::{AdtId, FunctionId};

use super::defs::{DefIdx, Defs, Id, Kind, Parent};
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

    /// An associated item of the crate: of a trait impl, the item of its trait it implements
    /// (nothing when the trait is another crate's); else itself.
    fn assoc(defs: &Defs, id: Id) -> Res {
        let Some(def) = defs.get(id) else { return Res::None };
        let Parent::Def(parent) = defs[def].parent else { return Res::Def(def) };
        if defs[parent].kind != (Kind::Impl { of_trait: true }) {
            return Res::Def(def);
        }
        match defs.impl_of(parent).and_then(|it| it.trait_) {
            Some(_) => Res::Def(defs.trait_item_of(def).unwrap_or(def)),
            None => Res::None,
        }
    }

    /// `res`, written as the path `Self` alone in a value or a pattern when `self_ctor`.
    pub(super) fn path(cx: &Cx<'_, '_>, defs: &Defs, res: PathResolution<'_>, self_ctor: bool) -> Res {
        match res {
            PathResolution::Def(def) => Res::def(defs, def),
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

    pub(super) fn def(defs: &Defs, def: ModuleDef) -> Res {
        match def {
            ModuleDef::Function(it) => Res::function(defs, it),
            ModuleDef::Adt(it) => Res::of(defs, Id::Adt(AdtId::from(it))),
            ModuleDef::EnumVariant(it) => defs.get(Id::Variant(it.into())).map_or(Res::None, Res::Variant),
            ModuleDef::Const(it) => Res::assoc(defs, Id::Const(it.into())),
            ModuleDef::Static(it) => Res::of(defs, Id::Static(it.into())),
            ModuleDef::Trait(it) => Res::of(defs, Id::Trait(it.into())),
            ModuleDef::TypeAlias(it) => Res::assoc(defs, Id::Alias(it.into())),
            ModuleDef::Module(_) | ModuleDef::BuiltinType(_) | ModuleDef::Macro(_) => Res::None,
        }
    }

    /// A function a path or a method call resolved to. The method of a builtin derive's impl is
    /// its trait's, another crate's.
    pub(super) fn function(defs: &Defs, function: hir::Function) -> Res {
        match FunctionId::try_from(function) {
            Ok(id) => Res::assoc(defs, Id::Fn(id)),
            Err(()) => Res::None,
        }
    }
}

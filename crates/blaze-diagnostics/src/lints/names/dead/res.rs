//! What a path, a resolved method or an inferred variant names, as rustc's `Res` the dead-code
//! pass handles: a definition of the crate, a variant (which a pattern does not make live), or
//! nothing it marks (a local, a generic parameter, a primitive, `Self` as a constructor, an
//! item of another crate).
//!
//! Follows `compiler/rustc_passes/src/dead.rs` (`handle_res`) and
//! `compiler/rustc_hir/src/def.rs` (`Res`) (MIT or Apache-2.0).

use hir_def::{
    AdtId, AssocItemId, VariantId,
    expr_store::path::Path,
    resolver::{Resolver, TypeNs, ValueNs},
};
use hir_ty::{db::HirDatabase, method_resolution::CandidateId};

use super::defs::{DefIdx, Defs, Id};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Res {
    Def(DefIdx),
    /// A variant or its constructor.
    Variant(DefIdx),
    None,
}

impl Res {
    fn of(defs: &Defs, id: Id) -> Res {
        defs.get(id).map_or(Res::None, Res::Def)
    }

    pub(super) fn type_ns(defs: &Defs, ns: TypeNs) -> Res {
        match ns {
            TypeNs::SelfType(it) => Res::of(defs, Id::Impl(it)),
            TypeNs::AdtId(it) | TypeNs::AdtSelfType(it) => Res::of(defs, Id::Adt(it)),
            TypeNs::EnumVariantId(it) => defs.get(Id::Variant(it)).map_or(Res::None, Res::Variant),
            TypeNs::TypeAliasId(it) => Res::of(defs, Id::Alias(it)),
            TypeNs::TraitId(it) => Res::of(defs, Id::Trait(it)),
            TypeNs::GenericParam(_) | TypeNs::BuiltinType(_) | TypeNs::ModuleId(_) => Res::None,
        }
    }

    pub(super) fn value_ns(defs: &Defs, ns: ValueNs) -> Res {
        match ns {
            ValueNs::FunctionId(it) => Res::of(defs, Id::Fn(it)),
            ValueNs::ConstId(it) => Res::of(defs, Id::Const(it)),
            ValueNs::StaticId(it) => Res::of(defs, Id::Static(it)),
            ValueNs::StructId(it) => Res::of(defs, Id::Adt(AdtId::StructId(it))),
            ValueNs::EnumVariantId(it) => defs.get(Id::Variant(it)).map_or(Res::None, Res::Variant),
            ValueNs::ImplSelf(_) | ValueNs::LocalBinding(_) | ValueNs::GenericParam(_) => Res::None,
        }
    }

    pub(super) fn candidate(defs: &Defs, candidate: CandidateId) -> Res {
        match candidate {
            CandidateId::FunctionId(it) => Res::of(defs, Id::Fn(it)),
            CandidateId::ConstId(it) => Res::of(defs, Id::Const(it)),
        }
    }

    pub(super) fn variant(defs: &Defs, variant: VariantId) -> Res {
        match variant {
            VariantId::StructId(it) => Res::of(defs, Id::Adt(AdtId::StructId(it))),
            VariantId::UnionId(it) => Res::of(defs, Id::Adt(AdtId::UnionId(it))),
            VariantId::EnumVariantId(it) => defs.get(Id::Variant(it)).map_or(Res::None, Res::Variant),
        }
    }

    pub(super) fn assoc(defs: &Defs, item: AssocItemId) -> Res {
        match item {
            AssocItemId::FunctionId(it) => Res::of(defs, Id::Fn(it)),
            AssocItemId::ConstId(it) => Res::of(defs, Id::Const(it)),
            AssocItemId::TypeAliasId(it) => Res::of(defs, Id::Alias(it)),
        }
    }
}

/// What a type path names: in full, or (`T::Assoc`, `<S as Trait>::Assoc`, `Self::Assoc`)
/// what its prefix names, with the segments left. `None` when it does not resolve.
pub(super) fn type_path(
    db: &dyn HirDatabase,
    resolver: &Resolver<'_>,
    path: &Path,
) -> Option<(TypeNs, Option<usize>)> {
    let (ns, remaining, _) = resolver.resolve_path_in_type_ns(db, path)?;
    Some((ns, remaining))
}

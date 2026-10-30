//! Utilities for computing drop info about types.

use hir_def::{AdtId, ImplId, signatures::StructFlags};
use rustc_hash::FxHashSet;
use rustc_type_ir::inherent::{AdtDef, GenericArgs as _, IntoKind};

use crate::{
    consteval,
    db::HirDatabase,
    layout::{enum_variant_ids, struct_flags},
    method_resolution::{SelfTyKey, TraitImpls},
    next_solver::{
        DbInterner, ParamEnv, SimplifiedType, Ty, TyKind,
        infer::{InferCtxt, traits::ObligationCause},
        obligation_ctxt::ObligationCtxt,
    },
};

#[salsa::tracked]
pub fn destructor(db: &dyn HirDatabase, adt: AdtId) -> Option<ImplId> {
    let module = match adt {
        AdtId::EnumId(id) => id.loc(db).container,
        AdtId::StructId(id) => id.loc(db).container,
        AdtId::UnionId(id) => id.loc(db).container,
    };
    let interner = DbInterner::new_with(db, module.krate(db));
    let drop_trait = interner.lang_items().Drop?;
    let self_ty = SelfTyKey(SimplifiedType::Adt(adt.into()));
    let impls = match module.block(db) {
        Some(block) => TraitImpls::in_block_for_trait_and_self_ty(db, block, drop_trait, self_ty),
        None => {
            TraitImpls::in_crate_for_trait_and_self_ty(db, module.krate(db), drop_trait, self_ty)
        }
    };
    impls.first().copied()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DropGlue {
    // Order of variants is important.
    None,
    /// May have a drop glue if some type parameter has it.
    ///
    /// For the compiler this is considered as a positive result, IDE distinguishes this from "yes".
    DependOnParams,
    HasDropGlue,
}

/// Whether no part of a value of `ty` can change behind a shared reference: no `UnsafeCell`,
/// no union, no type parameter. rustc promotes `&expr` only of such a type.
pub fn is_freeze<'db>(infcx: &InferCtxt<'db>, ty: Ty<'db>, env: ParamEnv<'db>) -> bool {
    let mut ocx = ObligationCtxt::new(infcx);
    // A field of an associated type, `Cow<str>`'s `<str as ToOwned>::Owned`, is what it
    // normalizes to; one the solver can't decide stays the alias, which is not `Freeze`.
    let ty = match ocx.structurally_normalize_ty(&ObligationCause::dummy(), env, ty) {
        Ok(normalized) if !matches!(normalized.kind(), TyKind::Infer(_)) => normalized,
        _ => ty,
    };
    let interner = infcx.interner;
    let db = interner.db;
    match ty.kind() {
        TyKind::Bool
        | TyKind::Char
        | TyKind::Int(_)
        | TyKind::Uint(_)
        | TyKind::Float(_)
        | TyKind::Str
        | TyKind::Never
        | TyKind::Ref(..)
        | TyKind::RawPtr(..)
        | TyKind::FnPtr(..)
        | TyKind::FnDef(..) => true,
        TyKind::Array(element, _) | TyKind::Slice(element) | TyKind::Pat(element, _) => {
            is_freeze(infcx, element, env)
        }
        TyKind::Tuple(fields) => fields.iter().all(|field| is_freeze(infcx, field, env)),
        TyKind::Closure(_, args) => is_freeze(infcx, args.as_closure().tupled_upvars_ty(), env),
        TyKind::Adt(def, args) => {
            let fields_freeze = |variant: hir_def::VariantId| {
                db.field_types(variant).iter().all(|(_, field)| {
                    let ty = field.ty().instantiate(interner, args).skip_norm_wip();
                    is_freeze(infcx, ty, env)
                })
            };
            match def.def_id() {
                AdtId::StructId(id) => {
                    Some(id) != interner.lang_items().UnsafeCell && fields_freeze(id.into())
                }
                AdtId::EnumId(id) => {
                    enum_variant_ids(db, id).iter().all(|&variant| fields_freeze(variant.into()))
                }
                AdtId::UnionId(_) => false,
            }
        }
        _ => false,
    }
}

pub fn has_drop_glue<'db>(infcx: &InferCtxt<'db>, ty: Ty<'db>, env: ParamEnv<'db>) -> DropGlue {
    has_drop_glue_impl(infcx, ty, env, &mut FxHashSet::default())
}

fn has_drop_glue_impl<'db>(
    infcx: &InferCtxt<'db>,
    ty: Ty<'db>,
    env: ParamEnv<'db>,
    visited: &mut FxHashSet<Ty<'db>>,
) -> DropGlue {
    let mut ocx = ObligationCtxt::new(infcx);
    // An alias the solver can't decide under `env` comes back as the variable it was to equal,
    // which is no type: it stays the alias, which may have drop glue.
    let ty = match ocx.structurally_normalize_ty(&ObligationCause::dummy(), env, ty) {
        Ok(normalized) if !matches!(normalized.kind(), TyKind::Infer(_)) => normalized,
        _ => ty,
    };

    if !visited.insert(ty) {
        // Recursive type.
        return DropGlue::None;
    }

    let db = infcx.interner.db;
    match ty.kind() {
        TyKind::Adt(adt_def, subst) => {
            let adt_id = adt_def.def_id();
            if adt_def.destructor(infcx.interner).is_some() {
                return DropGlue::HasDropGlue;
            }
            match adt_id {
                AdtId::StructId(id) => {
                    if struct_flags(db, id)
                        .intersects(StructFlags::IS_MANUALLY_DROP | StructFlags::IS_PHANTOM_DATA)
                    {
                        return DropGlue::None;
                    }
                    db.field_types(id.into())
                        .iter()
                        .map(|(_, field)| {
                            has_drop_glue_impl(
                                infcx,
                                field.ty().instantiate(infcx.interner, subst).skip_norm_wip(),
                                env,
                                visited,
                            )
                        })
                        .max()
                        .unwrap_or(DropGlue::None)
                }
                // Unions cannot have fields with destructors.
                AdtId::UnionId(_) => DropGlue::None,
                AdtId::EnumId(id) => enum_variant_ids(db, id)
                    .iter()
                    .map(|&variant| {
                        db.field_types(variant.into())
                            .iter()
                            .map(|(_, field)| {
                                has_drop_glue_impl(
                                    infcx,
                                    field.ty().instantiate(infcx.interner, subst).skip_norm_wip(),
                                    env,
                                    visited,
                                )
                            })
                            .max()
                            .unwrap_or(DropGlue::None)
                    })
                    .max()
                    .unwrap_or(DropGlue::None),
            }
        }
        TyKind::Tuple(tys) => tys
            .iter()
            .map(|ty| has_drop_glue_impl(infcx, ty, env, visited))
            .max()
            .unwrap_or(DropGlue::None),
        TyKind::Array(ty, len) => {
            if consteval::try_const_usize(db, len) == Some(0) {
                // Arrays of size 0 don't have drop glue.
                return DropGlue::None;
            }
            has_drop_glue_impl(infcx, ty, env, visited)
        }
        TyKind::Slice(ty) => has_drop_glue_impl(infcx, ty, env, visited),
        TyKind::Closure(_, args) => {
            has_drop_glue_impl(infcx, args.as_closure().tupled_upvars_ty(), env, visited)
        }
        // What is suspended inside it is not visible from here.
        TyKind::Coroutine(..) => DropGlue::HasDropGlue,
        TyKind::CoroutineClosure(_, args) => {
            has_drop_glue_impl(infcx, args.as_coroutine_closure().tupled_upvars_ty(), env, visited)
        }
        // FIXME: Coroutine witness.
        TyKind::CoroutineWitness(..) => DropGlue::None,
        TyKind::Ref(..)
        | TyKind::RawPtr(..)
        | TyKind::FnDef(..)
        | TyKind::Str
        | TyKind::Never
        | TyKind::Bool
        | TyKind::Char
        | TyKind::Int(_)
        | TyKind::Uint(_)
        | TyKind::Float(_)
        | TyKind::FnPtr(..)
        | TyKind::Foreign(_)
        | TyKind::Error(_)
        | TyKind::Bound(..)
        | TyKind::Placeholder(..) => DropGlue::None,
        TyKind::Dynamic(..) => DropGlue::HasDropGlue,
        TyKind::Alias(..) => {
            if infcx.type_is_copy_modulo_regions(env, ty) {
                DropGlue::None
            } else {
                DropGlue::HasDropGlue
            }
        }
        TyKind::Param(_) => {
            if infcx.type_is_copy_modulo_regions(env, ty) {
                DropGlue::None
            } else {
                DropGlue::DependOnParams
            }
        }
        TyKind::Infer(..) => unreachable!("inference vars shouldn't exist out of inference"),
        TyKind::Pat(ty, _) => has_drop_glue_impl(infcx, ty, env, visited),
        TyKind::UnsafeBinder(ty) => has_drop_glue_impl(infcx, ty.skip_binder(), env, visited),
    }
}

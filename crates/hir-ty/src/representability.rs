//! Detecting whether a type is infinitely-sized.

use hir_def::{AdtId, VariantId};
use rustc_type_ir::inherent::IntoKind;
use thin_vec::ThinVec;

use crate::{
    db::HirDatabase,
    next_solver::{GenericArgKind, GenericArgs, Ty, TyKind},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Representability {
    Representable,
    Infinite,
}

macro_rules! rtry {
    ($e:expr) => {
        match $e {
            e @ Representability::Infinite => return e,
            Representability::Representable => {}
        }
    };
}

#[salsa::tracked(cycle_result = representability_cycle, returns(copy))]
pub(crate) fn representability(db: &dyn HirDatabase, id: AdtId) -> Representability {
    match id {
        AdtId::StructId(id) => variant_representability(db, id.into()),
        AdtId::UnionId(id) => variant_representability(db, id.into()),
        AdtId::EnumId(id) => {
            for &(variant, ..) in id.enum_variants(db).variants.values() {
                rtry!(variant_representability(db, variant.into()));
            }
            Representability::Representable
        }
    }
}

pub(crate) fn representability_cycle(
    _db: &dyn HirDatabase,
    _: salsa::Id,
    _id: AdtId,
) -> Representability {
    Representability::Infinite
}

fn variant_representability(db: &dyn HirDatabase, id: VariantId) -> Representability {
    for ty in db.field_types(id).values() {
        rtry!(representability_ty(db, ty.ty().instantiate_identity().skip_norm_wip()));
    }
    Representability::Representable
}

fn representability_ty<'db>(db: &'db dyn HirDatabase, ty: Ty<'db>) -> Representability {
    match ty.kind() {
        TyKind::Adt(adt_id, args) => representability_adt_ty(db, adt_id.def_id(), args),
        // FIXME(#11924) allow zero-length arrays?
        TyKind::Array(ty, _) => representability_ty(db, ty),
        TyKind::Tuple(tys) => {
            for ty in tys {
                rtry!(representability_ty(db, ty));
            }
            Representability::Representable
        }
        _ => Representability::Representable,
    }
}

fn representability_adt_ty<'db>(
    db: &'db dyn HirDatabase,
    def_id: AdtId,
    args: GenericArgs<'db>,
) -> Representability {
    rtry!(representability(db, def_id));

    // At this point, we know that the item of the ADT type is representable;
    // but the type parameters may cause a cycle with an upstream type
    let params_in_repr = params_in_repr(db, def_id);
    for (i, arg) in args.iter().enumerate() {
        if let GenericArgKind::Type(ty) = arg.kind()
            && params_in_repr.get(i)
        {
            rtry!(representability_ty(db, ty));
        }
    }
    Representability::Representable
}

/// Which of an ADT's generic parameters, by index, appear in its representation: a bitset, the
/// first 64 parameters in one word, which allocates nothing for nearly every ADT.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
struct ParamsInRepr {
    first: u64,
    rest: ThinVec<u64>,
}

const _: () = assert!(size_of::<ParamsInRepr>() == 16);

impl ParamsInRepr {
    #[inline]
    fn get(&self, index: usize) -> bool {
        let word = match index / 64 {
            0 => self.first,
            at => self.rest.get(at - 1).copied().unwrap_or(0),
        };
        word & (1 << (index % 64)) != 0
    }

    fn insert(&mut self, index: usize) {
        let bit = 1 << (index % 64);
        match index / 64 {
            0 => self.first |= bit,
            at => {
                if self.rest.len() < at {
                    self.rest.resize(at, 0);
                }
                self.rest[at - 1] |= bit;
            }
        }
    }
}

fn params_in_repr_cycle(_db: &dyn HirDatabase, _: salsa::Id, _def_id: AdtId) -> ParamsInRepr {
    ParamsInRepr::default()
}

// This is a query because a chain of types that each hold the one before several times would
// otherwise be walked once per path through it.
#[salsa::tracked(cycle_result = params_in_repr_cycle, returns(ref))]
fn params_in_repr(db: &dyn HirDatabase, def_id: AdtId) -> ParamsInRepr {
    let mut params_in_repr = ParamsInRepr::default();
    let mut handle_variant = |variant| {
        for field in db.field_types(variant).values() {
            params_in_repr_ty(
                db,
                field.ty().instantiate_identity().skip_norm_wip(),
                &mut params_in_repr,
            );
        }
    };
    match def_id {
        AdtId::StructId(def_id) => handle_variant(def_id.into()),
        AdtId::UnionId(def_id) => handle_variant(def_id.into()),
        AdtId::EnumId(def_id) => {
            for &(variant, ..) in def_id.enum_variants(db).variants.values() {
                handle_variant(variant.into());
            }
        }
    }
    params_in_repr.rest.shrink_to_fit();
    params_in_repr
}

fn params_in_repr_ty<'db>(
    db: &'db dyn HirDatabase,
    ty: Ty<'db>,
    params_in_repr: &mut ParamsInRepr,
) {
    match ty.kind() {
        TyKind::Adt(adt, args) => {
            let inner_params_in_repr = self::params_in_repr(db, adt.def_id());
            for (i, arg) in args.iter().enumerate() {
                if let GenericArgKind::Type(ty) = arg.kind()
                    && inner_params_in_repr.get(i)
                {
                    params_in_repr_ty(db, ty, params_in_repr);
                }
            }
        }
        TyKind::Array(ty, _) => params_in_repr_ty(db, ty, params_in_repr),
        TyKind::Tuple(tys) => tys.iter().for_each(|ty| params_in_repr_ty(db, ty, params_in_repr)),
        TyKind::Param(param) => params_in_repr.insert(param.index as usize),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::ParamsInRepr;

    #[test]
    fn params_in_repr_is_a_bitset_past_one_word() {
        let mut set = ParamsInRepr::default();
        for index in [0, 5, 63, 64, 130] {
            set.insert(index);
        }
        let set_bits: Vec<usize> = (0..200).filter(|&it| set.get(it)).collect();
        assert_eq!(set_bits, [0, 5, 63, 64, 130]);
        assert_eq!(set.rest.len(), 2);
    }

    #[test]
    fn params_in_repr_of_few_params_allocates_nothing() {
        let mut set = ParamsInRepr::default();
        set.insert(3);
        set.insert(40);
        assert!(set.rest.is_empty());
        assert!(set.get(3) && set.get(40) && !set.get(4) && !set.get(64));
    }
}

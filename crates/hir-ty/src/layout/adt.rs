//! Compute the binary representation of structs, unions and enums

use std::cmp;

use hir_def::{AdtId, VariantId, attrs::AttrFlags, signatures::StructFlags};
use rustc_abi::{
    BackendRepr, Float, Integer, NicheOptimizations, Primitive, ReprOptions, Scalar,
    TargetDataLayout, WrappingRange,
};
use rustc_index::IndexVec;
use smallvec::SmallVec;
use triomphe::Arc;

use crate::{
    db::HirDatabase,
    layout::{
        FIELDS_INLINE, Layout, LayoutCx, LayoutError, RustcEnumVariantIdx, RustcFieldIdx,
        enum_variant_ids, struct_flags, struct_tail_may_be_unsized, variant_field_ids,
    },
    next_solver::{DbInterner, GenericArgs, StoredGenericArgs},
    representability::{Representability, representability},
    traits::StoredParamEnvAndCrate,
};

#[salsa::tracked(cycle_result = layout_of_adt_cycle_result, returns(clone))]
pub fn layout_of_adt_query(
    db: &dyn HirDatabase,
    def: AdtId,
    args: StoredGenericArgs,
    trait_env: StoredParamEnvAndCrate,
) -> Result<Arc<Layout>, LayoutError> {
    let krate = trait_env.krate;
    let Ok(target) = db.target_data_layout(krate) else {
        return Err(LayoutError::TargetLayoutNotAvailable);
    };
    if representability(db, def) == Representability::Infinite {
        return Err(LayoutError::RecursiveTypeWithoutIndirection);
    }
    let dl = target;
    let cx = LayoutCx::new(dl);
    let args = args.as_ref();
    // Every variant's field layouts in one buffer, variant `i` at `fields[ends[i - 1]..ends[i]]`.
    let mut fields = SmallVec::<[Arc<Layout>; FIELDS_INLINE]>::new();
    let mut ends = SmallVec::<[u32; FIELDS_INLINE]>::new();
    let mut push_variant = |variant: VariantId| -> Result<(), LayoutError> {
        variant_field_layouts(db, variant, args, &trait_env, &mut fields)?;
        ends.push(fields.len() as u32);
        Ok(())
    };
    let flags = match def {
        AdtId::StructId(s) => Some(struct_flags(db, s)),
        AdtId::UnionId(_) | AdtId::EnumId(_) => None,
    };
    let (repr, niche_optimizations, enum_variants) = match def {
        AdtId::StructId(s) => {
            push_variant(s.into())?;
            (
                AttrFlags::repr(db, s.into()).unwrap_or_default(),
                // As rustc: `UnsafeCell` and `UnsafePinned` hide their fields' niches.
                if flags.is_some_and(|it| {
                    it.intersects(StructFlags::IS_UNSAFE_CELL | StructFlags::IS_UNSAFE_PINNED)
                }) {
                    NicheOptimizations::Disabled
                } else {
                    NicheOptimizations::Enabled
                },
                &[][..],
            )
        }
        AdtId::UnionId(id) => {
            let repr = AttrFlags::repr(db, id.into());
            push_variant(id.into())?;
            (repr.unwrap_or_default(), NicheOptimizations::Enabled, &[][..])
        }
        AdtId::EnumId(e) => {
            let variants = enum_variant_ids(db, e);
            for &v in variants {
                push_variant(v.into())?;
            }
            (AttrFlags::repr(db, e.into()).unwrap_or_default(), NicheOptimizations::Enabled, variants)
        }
    };
    let maybe_unsized = match def {
        AdtId::StructId(s) => struct_tail_may_be_unsized(db, s, krate),
        AdtId::UnionId(_) | AdtId::EnumId(_) => false,
    };
    let refs: SmallVec<[&Layout; FIELDS_INLINE]> = fields.iter().map(|it| &**it).collect();
    let variants = variants_of(&refs, &ends);
    let mut result = if matches!(def, AdtId::UnionId(..)) {
        cx.calc.layout_of_union(&repr, &variants)?
    } else {
        // As rustc has a discriminant: the bits of its type, and no more.
        let discr_size = Integer::from_attr(dl, repr.discr_type()).size();
        cx.calc.layout_of_struct_or_enum(
            &repr,
            &variants,
            matches!(def, AdtId::EnumId(..)),
            niche_optimizations,
            |min, max| repr_discr(dl, &repr, min.start, max.last).unwrap_or((Integer::I8, false)),
            enum_variants.iter().enumerate().filter_map(|(index, &v)| {
                let d = db.const_eval_discriminant(v).ok()?;
                Some((RustcEnumVariantIdx(index), discr_size.truncate(d as u128)))
            }),
            !matches!(def, AdtId::EnumId(..)) && !maybe_unsized,
        )?
    };
    // As rustc's `layout_of_uncached`: the `f16b` and `ppcf128` lang items are structs over an
    // integer of their size, laid out as the backend's float scalar of that format.
    if let Some(flags) = flags {
        let float = if flags.contains(StructFlags::IS_F16B) {
            Some(Float::F16B)
        } else if flags.contains(StructFlags::IS_PPC_F128) {
            Some(Float::PpcF128)
        } else {
            None
        };
        if let Some(float) = float {
            assert_eq!(result.size, float.size());
            result.align = float.align(dl);
            result.backend_repr = BackendRepr::Scalar(Scalar::Initialized {
                value: Primitive::Float(float),
                valid_range: WrappingRange::full(float.size()),
            });
        }
    }
    Ok(Arc::new(result))
}

/// Pushes the layout of each field of `variant` onto `out`, in order, reading the variant's
/// field types once.
fn variant_field_layouts<'db>(
    db: &'db dyn HirDatabase,
    variant: VariantId,
    args: GenericArgs<'db>,
    trait_env: &StoredParamEnvAndCrate,
    out: &mut SmallVec<[Arc<Layout>; FIELDS_INLINE]>,
) -> Result<(), LayoutError> {
    let interner = DbInterner::new_no_crate(db);
    let types = db.field_types(variant);
    let ids = variant_field_ids(db, variant);
    out.reserve(ids.len());
    for &fd in ids {
        let ty = types[fd].ty().instantiate(interner, args).skip_norm_wip();
        out.push(super::field_layout(db, ty.store(), trait_env.clone())?);
    }
    Ok(())
}

/// The fields of each variant as the layout calculator takes them: variant `i` is
/// `refs[ends[i - 1]..ends[i]]`. The calculator wants an owned `IndexVec` per variant; each is
/// one exact allocation, none for a variant without fields.
fn variants_of<'a, 'b>(
    refs: &'a [&'b Layout],
    ends: &[u32],
) -> IndexVec<RustcEnumVariantIdx, IndexVec<RustcFieldIdx, &'a &'b Layout>> {
    let mut variants = IndexVec::with_capacity(ends.len());
    let mut start = 0;
    for &end in ends {
        let end = end as usize;
        variants.push(refs[start..end].iter().collect());
        start = end;
    }
    variants
}

fn layout_of_adt_cycle_result(
    _: &dyn HirDatabase,
    _: salsa::Id,
    _def: AdtId,
    _args: StoredGenericArgs,
    _trait_env: StoredParamEnvAndCrate,
) -> Result<Arc<Layout>, LayoutError> {
    Err(LayoutError::RecursiveTypeWithoutIndirection)
}

/// Finds the appropriate Integer type and signedness for the given
/// signed discriminant range and `#[repr]` attribute.
/// N.B.: `u128` values above `i128::MAX` will be treated as signed, but
/// that shouldn't affect anything, other than maybe debuginfo.
fn repr_discr(
    dl: &TargetDataLayout,
    repr: &ReprOptions,
    min: i128,
    max: u128,
) -> Result<(Integer, bool), LayoutError> {
    // Theoretically, negative values could be larger in unsigned representation
    // than the unsigned representation of the signed minimum. However, if there
    // are any negative values, the only valid unsigned representation is u128
    // which can fit all i128 values, so the result remains unaffected.
    let unsigned_fit = Integer::fit_unsigned(cmp::max(min as u128, max));
    let signed_fit = cmp::max(Integer::fit_signed(min), Integer::fit_signed(max as i128));

    if let Some(ity) = repr.int {
        let discr = Integer::from_attr(dl, ity);
        let fit = if ity.is_signed() { signed_fit } else { unsigned_fit };
        if discr < fit {
            return Err(LayoutError::UserReprTooSmall);
        }
        return Ok((discr, ity.is_signed()));
    }

    let at_least = if repr.c() {
        // This is usually I32, however it can be different on some platforms,
        // notably hexagon and arm-none/thumb-none
        dl.c_enum_min_size
    } else {
        // repr(Rust) enums try to be as small as possible
        Integer::I8
    };

    // `min` and `max` are the ends of a wrapping range, so their sign is not a usable test.
    Ok(if unsigned_fit <= signed_fit {
        (cmp::max(unsigned_fit, at_least), false)
    } else {
        (cmp::max(signed_fit, at_least), true)
    })
}

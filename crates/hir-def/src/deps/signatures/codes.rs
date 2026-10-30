//! What each record of the part holds besides its store, in the words of
//! [`crate::hir::persist`]: one [`Sig`] per signature query, and [`Code`] for the parts of
//! signatures nothing else writes (generic params, where clauses, fields, a trait ref).

use blaze_format::FormatError;
use cfg::CfgOptions;
use hir_expand::name::Name;
use la_arena::Arena;
use smallvec::SmallVec;
use thin_vec::ThinVec;
use triomphe::Arc;

use crate::{
    FxIndexMap,
    expr_store::ExpressionStore,
    hir::{
        generics::{
            ConstParamData, GenericParams, LifetimeBoundType, LifetimeParamData,
            TypeOrConstParamData, TypeParamData, TypeParamProvenance, WherePredicate,
        },
        persist::{Code, Read, Words},
    },
    signatures::{
        ConstSignature, EnumSignature, EnumVariants, FieldData, FunctionSignature,
        ImplSignature, InactiveEnumVariantCode, StaticSignature, StructSignature,
        TraitSignature, TypeAliasSignature, UnionSignature, VariantFields,
    },
    type_ref::TraitRef,
};

/// One signature query, as a record holds its value.
pub(super) trait Sig {
    /// The value the record is written from.
    type Of;
    /// What the query returns.
    type Value;

    fn of(value: &Self::Value) -> &Self::Of;

    /// The store, written as a buffer of its own beside the words.
    fn store(of: &Self::Of) -> Option<&ExpressionStore>;

    fn put(of: &Self::Of, w: &mut Words);

    /// The value of `r`, with its `store`; `cfg` is the options of the item's crate.
    fn get(
        r: &mut Read<'_>,
        store: Option<ExpressionStore>,
        cfg: &CfgOptions,
    ) -> Result<Self::Value, FormatError>;
}

fn needs(store: Option<ExpressionStore>, r: &Read<'_>) -> Result<ExpressionStore, FormatError> {
    store.ok_or_else(|| r.invalid("a signature record without its store"))
}

fn put_flags<F: bitflags::Flags>(w: &mut Words, flags: &F)
where
    F::Bits: Into<u32>,
{
    w.u32(flags.bits().into());
}

fn get_flags<F: bitflags::Flags>(r: &mut Read<'_>) -> Result<F, FormatError>
where
    F::Bits: TryFrom<u32>,
{
    let word = r.u32()?;
    let bits = <F::Bits as TryFrom<u32>>::try_from(word).ok();
    bits.and_then(F::from_bits).ok_or_else(|| r.invalid("signature flags out of range"))
}

fn put_arena<T: Code>(w: &mut Words, arena: &Arena<T>) {
    w.len(arena.len());
    arena.iter().for_each(|(_, it)| it.put(w));
}

fn get_arena<T: Code>(r: &mut Read<'_>) -> Result<Arena<T>, FormatError> {
    let len = r.len()?;
    let mut arena = Arena::with_capacity(len);
    for _ in 0..len {
        arena.alloc(r.get()?);
    }
    Ok(arena)
}

impl Code for TypeParamProvenance {
    fn put(&self, w: &mut Words) {
        w.u32(match self {
            TypeParamProvenance::TypeParamList => 0,
            TypeParamProvenance::TraitSelf => 1,
            TypeParamProvenance::ArgumentImplTrait => 2,
        });
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(match r.tag(3, "a type param provenance out of range")? {
            0 => TypeParamProvenance::TypeParamList,
            1 => TypeParamProvenance::TraitSelf,
            _ => TypeParamProvenance::ArgumentImplTrait,
        })
    }
}

impl Code for LifetimeBoundType {
    fn put(&self, w: &mut Words) {
        w.u32(match self {
            LifetimeBoundType::EarlyBound => 0,
            LifetimeBoundType::LateBound => 1,
        });
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(match r.tag(2, "a lifetime bound type out of range")? {
            0 => LifetimeBoundType::EarlyBound,
            _ => LifetimeBoundType::LateBound,
        })
    }
}

impl Code for TypeOrConstParamData {
    fn put(&self, w: &mut Words) {
        match self {
            TypeOrConstParamData::TypeParamData(TypeParamData { name, default, provenance }) => {
                w.u32(0);
                w.put(name);
                w.put(default);
                w.put(provenance);
            }
            TypeOrConstParamData::ConstParamData(ConstParamData { name, ty, default }) => {
                w.u32(1);
                w.put(name);
                w.put(ty);
                w.put(default);
            }
        }
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(match r.tag(2, "a generic param kind out of range")? {
            0 => TypeOrConstParamData::TypeParamData(TypeParamData {
                name: r.get()?,
                default: r.get()?,
                provenance: r.get()?,
            }),
            _ => TypeOrConstParamData::ConstParamData(ConstParamData {
                name: r.get()?,
                ty: r.get()?,
                default: r.get()?,
            }),
        })
    }
}

impl Code for LifetimeParamData {
    fn put(&self, w: &mut Words) {
        w.put(&self.name);
        w.put(&self.bound_type);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(LifetimeParamData { name: r.get()?, bound_type: r.get()? })
    }
}

impl Code for WherePredicate {
    fn put(&self, w: &mut Words) {
        match self {
            WherePredicate::TypeBound { lifetimes, target, bound } => {
                w.u32(0);
                w.put(lifetimes);
                w.put(target);
                w.put(bound);
            }
            WherePredicate::Lifetime { target, bound } => {
                w.u32(1);
                w.put(target);
                w.put(bound);
            }
        }
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(match r.tag(2, "a where predicate kind out of range")? {
            0 => WherePredicate::TypeBound {
                lifetimes: r.get()?,
                target: r.get()?,
                bound: r.get()?,
            },
            _ => WherePredicate::Lifetime { target: r.get()?, bound: r.get()? },
        })
    }
}

impl Code for GenericParams {
    fn put(&self, w: &mut Words) {
        put_arena(w, &self.type_or_consts);
        put_arena(w, &self.lifetimes);
        w.put(&self.where_predicates);
        w.put(&self.early_bound_lifetimes_len);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(GenericParams {
            type_or_consts: get_arena(r)?,
            lifetimes: get_arena(r)?,
            where_predicates: r.get()?,
            early_bound_lifetimes_len: r.get()?,
        })
    }
}

impl Code for TraitRef {
    fn put(&self, w: &mut Words) {
        w.put(&self.path);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(TraitRef { path: r.get()? })
    }
}

impl Code for FieldData {
    fn put(&self, w: &mut Words) {
        let FieldData { name, type_ref, visibility, is_unsafe, default_value } = self;
        w.put(name);
        w.put(type_ref);
        w.put(visibility);
        w.bool(*is_unsafe);
        w.put(default_value);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(FieldData {
            name: r.get()?,
            type_ref: r.get()?,
            visibility: r.get()?,
            is_unsafe: r.bool()?,
            default_value: r.get()?,
        })
    }
}

/// [`StructSignature::of`].
pub(super) struct Struct;
/// [`UnionSignature::of`].
pub(super) struct Union;
/// [`EnumSignature::of`].
pub(super) struct Enum;
/// [`ConstSignature::of`].
pub(super) struct Const;
/// [`StaticSignature::of`].
pub(super) struct Static;
/// [`ImplSignature::of`].
pub(super) struct Impl;
/// [`TraitSignature::of`].
pub(super) struct Trait;
/// [`FunctionSignature::of`].
pub(super) struct Function;
/// [`TypeAliasSignature::of`].
pub(super) struct TypeAlias;
/// [`VariantFields::of`].
pub(super) struct Fields;
/// [`EnumVariants::of`].
pub(super) struct Variants;

impl Sig for Struct {
    type Of = StructSignature;
    type Value = Arc<StructSignature>;

    fn of(value: &Self::Value) -> &Self::Of {
        value
    }

    fn store(of: &Self::Of) -> Option<&ExpressionStore> {
        Some(&of.store)
    }

    fn put(of: &Self::Of, w: &mut Words) {
        w.put(&of.name);
        w.put(&of.generic_params);
        put_flags(w, &of.flags);
        w.put(&of.shape);
    }

    fn get(
        r: &mut Read<'_>,
        store: Option<ExpressionStore>,
        _: &CfgOptions,
    ) -> Result<Self::Value, FormatError> {
        Ok(Arc::new(StructSignature {
            name: r.get()?,
            generic_params: r.get()?,
            flags: get_flags(r)?,
            shape: r.get()?,
            store: needs(store, r)?,
        }))
    }
}

impl Sig for Union {
    type Of = UnionSignature;
    type Value = Arc<UnionSignature>;

    fn of(value: &Self::Value) -> &Self::Of {
        value
    }

    fn store(of: &Self::Of) -> Option<&ExpressionStore> {
        Some(&of.store)
    }

    fn put(of: &Self::Of, w: &mut Words) {
        w.put(&of.name);
        w.put(&of.generic_params);
        put_flags(w, &of.flags);
    }

    fn get(
        r: &mut Read<'_>,
        store: Option<ExpressionStore>,
        _: &CfgOptions,
    ) -> Result<Self::Value, FormatError> {
        Ok(Arc::new(UnionSignature {
            name: r.get()?,
            generic_params: r.get()?,
            flags: get_flags(r)?,
            store: needs(store, r)?,
        }))
    }
}

impl Sig for Enum {
    type Of = EnumSignature;
    type Value = Arc<EnumSignature>;

    fn of(value: &Self::Value) -> &Self::Of {
        value
    }

    fn store(of: &Self::Of) -> Option<&ExpressionStore> {
        Some(&of.store)
    }

    fn put(of: &Self::Of, w: &mut Words) {
        w.put(&of.name);
        w.put(&of.generic_params);
        put_flags(w, &of.flags);
    }

    fn get(
        r: &mut Read<'_>,
        store: Option<ExpressionStore>,
        _: &CfgOptions,
    ) -> Result<Self::Value, FormatError> {
        Ok(Arc::new(EnumSignature {
            name: r.get()?,
            generic_params: r.get()?,
            flags: get_flags(r)?,
            store: needs(store, r)?,
        }))
    }
}

impl Sig for Const {
    type Of = ConstSignature;
    type Value = Arc<ConstSignature>;

    fn of(value: &Self::Value) -> &Self::Of {
        value
    }

    fn store(of: &Self::Of) -> Option<&ExpressionStore> {
        Some(&of.store)
    }

    fn put(of: &Self::Of, w: &mut Words) {
        w.put(&of.name);
        w.put(&of.type_ref);
        put_flags(w, &of.flags);
    }

    fn get(
        r: &mut Read<'_>,
        store: Option<ExpressionStore>,
        _: &CfgOptions,
    ) -> Result<Self::Value, FormatError> {
        Ok(Arc::new(ConstSignature {
            name: r.get()?,
            type_ref: r.get()?,
            flags: get_flags(r)?,
            store: needs(store, r)?,
        }))
    }
}

impl Sig for Static {
    type Of = StaticSignature;
    type Value = Arc<StaticSignature>;

    fn of(value: &Self::Value) -> &Self::Of {
        value
    }

    fn store(of: &Self::Of) -> Option<&ExpressionStore> {
        Some(&of.store)
    }

    fn put(of: &Self::Of, w: &mut Words) {
        w.put(&of.name);
        w.put(&of.type_ref);
        put_flags(w, &of.flags);
    }

    fn get(
        r: &mut Read<'_>,
        store: Option<ExpressionStore>,
        _: &CfgOptions,
    ) -> Result<Self::Value, FormatError> {
        Ok(Arc::new(StaticSignature {
            name: r.get()?,
            type_ref: r.get()?,
            flags: get_flags(r)?,
            store: needs(store, r)?,
        }))
    }
}

impl Sig for Impl {
    type Of = ImplSignature;
    type Value = Arc<ImplSignature>;

    fn of(value: &Self::Value) -> &Self::Of {
        value
    }

    fn store(of: &Self::Of) -> Option<&ExpressionStore> {
        Some(&of.store)
    }

    fn put(of: &Self::Of, w: &mut Words) {
        w.put(&of.generic_params);
        w.put(&of.self_ty);
        w.put(&of.target_trait);
        put_flags(w, &of.flags);
    }

    fn get(
        r: &mut Read<'_>,
        store: Option<ExpressionStore>,
        _: &CfgOptions,
    ) -> Result<Self::Value, FormatError> {
        Ok(Arc::new(ImplSignature {
            generic_params: r.get()?,
            self_ty: r.get()?,
            target_trait: r.get()?,
            flags: get_flags(r)?,
            store: needs(store, r)?,
        }))
    }
}

impl Sig for Trait {
    type Of = TraitSignature;
    type Value = Arc<TraitSignature>;

    fn of(value: &Self::Value) -> &Self::Of {
        value
    }

    fn store(of: &Self::Of) -> Option<&ExpressionStore> {
        Some(&of.store)
    }

    fn put(of: &Self::Of, w: &mut Words) {
        w.put(&of.name);
        w.put(&of.generic_params);
        put_flags(w, &of.flags);
    }

    fn get(
        r: &mut Read<'_>,
        store: Option<ExpressionStore>,
        _: &CfgOptions,
    ) -> Result<Self::Value, FormatError> {
        Ok(Arc::new(TraitSignature {
            name: r.get()?,
            generic_params: r.get()?,
            flags: get_flags(r)?,
            store: needs(store, r)?,
        }))
    }
}

impl Sig for Function {
    type Of = FunctionSignature;
    type Value = Arc<FunctionSignature>;

    fn of(value: &Self::Value) -> &Self::Of {
        value
    }

    fn store(of: &Self::Of) -> Option<&ExpressionStore> {
        Some(&of.store)
    }

    fn put(of: &Self::Of, w: &mut Words) {
        w.put(&of.name);
        w.put(&of.generic_params);
        w.put(&of.params);
        w.put(&of.ret_type);
        w.put(&of.abi);
        put_flags(w, &of.flags);
    }

    fn get(
        r: &mut Read<'_>,
        store: Option<ExpressionStore>,
        _: &CfgOptions,
    ) -> Result<Self::Value, FormatError> {
        Ok(Arc::new(FunctionSignature {
            name: r.get()?,
            generic_params: r.get()?,
            params: r.get()?,
            ret_type: r.get()?,
            abi: r.get()?,
            flags: get_flags(r)?,
            store: needs(store, r)?,
        }))
    }
}

impl Sig for TypeAlias {
    type Of = TypeAliasSignature;
    type Value = Arc<TypeAliasSignature>;

    fn of(value: &Self::Value) -> &Self::Of {
        value
    }

    fn store(of: &Self::Of) -> Option<&ExpressionStore> {
        Some(&of.store)
    }

    fn put(of: &Self::Of, w: &mut Words) {
        w.put(&of.name);
        w.put(&of.generic_params);
        w.put(&of.bounds);
        w.put(&of.ty);
        put_flags(w, &of.flags);
    }

    fn get(
        r: &mut Read<'_>,
        store: Option<ExpressionStore>,
        _: &CfgOptions,
    ) -> Result<Self::Value, FormatError> {
        Ok(Arc::new(TypeAliasSignature {
            name: r.get()?,
            generic_params: r.get()?,
            bounds: r.get()?,
            ty: r.get()?,
            flags: get_flags(r)?,
            store: needs(store, r)?,
        }))
    }
}

impl Sig for Fields {
    type Of = VariantFields;
    type Value = Arc<VariantFields>;

    fn of(value: &Self::Value) -> &Self::Of {
        value
    }

    fn store(of: &Self::Of) -> Option<&ExpressionStore> {
        Some(&of.store)
    }

    fn put(of: &Self::Of, w: &mut Words) {
        put_arena(w, of.fields());
        w.put(&of.shape);
    }

    fn get(
        r: &mut Read<'_>,
        store: Option<ExpressionStore>,
        _: &CfgOptions,
    ) -> Result<Self::Value, FormatError> {
        let fields = get_arena(r)?;
        let shape = r.get()?;
        Ok(Arc::new(VariantFields::from_parts(fields, needs(store, r)?, shape)))
    }
}

impl Sig for Variants {
    type Of = (EnumVariants, ThinVec<InactiveEnumVariantCode>);
    type Value = (EnumVariants, ThinVec<InactiveEnumVariantCode>);

    fn of(value: &Self::Value) -> &Self::Of {
        value
    }

    fn store(_: &Self::Of) -> Option<&ExpressionStore> {
        None
    }

    fn put((variants, inactive): &Self::Of, w: &mut Words) {
        w.len(variants.variants.len());
        let explicit = variants.with_explicit_discriminant().map(|(_, _, explicit)| explicit);
        for ((name, (id, shape)), explicit) in variants.variants.iter().zip(explicit) {
            w.put(name);
            w.put(id);
            w.put(shape);
            w.bool(explicit);
        }
        w.len(inactive.len());
        for code in inactive {
            w.put(&code.cfg);
            w.put(&code.ast_id);
        }
    }

    fn get(
        r: &mut Read<'_>,
        store: Option<ExpressionStore>,
        cfg: &CfgOptions,
    ) -> Result<Self::Value, FormatError> {
        if store.is_some() {
            return Err(r.invalid("enum variants with a store"));
        }
        let len = r.len()?;
        let mut variants = FxIndexMap::with_capacity_and_hasher(len, Default::default());
        let mut explicit = SmallVec::<[bool; 32]>::with_capacity(len);
        for _ in 0..len {
            let name: Name = r.get()?;
            let value = (r.get()?, r.get()?);
            explicit.push(r.bool()?);
            if variants.insert(name, value).is_some() {
                return Err(r.invalid("an enum variant named twice"));
            }
        }
        let len = r.len()?;
        let mut inactive = ThinVec::with_capacity(len);
        for _ in 0..len {
            let cfg_expr = r.get()?;
            inactive.push(InactiveEnumVariantCode { cfg: cfg_expr, opts: cfg.clone(), ast_id: r.get()? });
        }
        Ok((EnumVariants::from_parts(variants, &explicit), inactive))
    }
}

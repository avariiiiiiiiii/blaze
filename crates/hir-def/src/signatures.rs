//! Item signature IR definitions

use std::cell::LazyCell;

use base_db::SourceDatabase;
use bitflags::bitflags;
use cfg::{CfgExpr, CfgOptions};
use hir_expand::{
    InFile, Intern, Lookup,
    name::{AsName, Name},
};
use intern::{Symbol, sym};
use la_arena::{Arena, Idx};
use rustc_abi::{ExternAbi, IntegerType, ReprOptions};
use smallvec::{SmallVec, smallvec};
use syntax::{
    AstNode, SyntaxKind, SyntaxNode, SyntaxNodePtr, T,
    ast::{self, HasGenericParams, HasName, HasVisibility, IsString},
};
use thin_vec::ThinVec;
use triomphe::Arc;

use crate::{
    ConstId, EnumId, EnumVariantId, EnumVariantLoc, ExternBlockId, FunctionId, FxIndexMap,
    HasModule, ImplId, ItemContainerId, LoweringMode, ModuleId, StaticId, StructId, TraitId,
    TypeAliasId, UnionId, VariantId,
    attrs::AttrFlags,
    expr_store::{
        ExpressionStore, ExpressionStoreSourceMap,
        lower::{
            ExprCollector, lower_function, lower_generic_params, lower_trait, lower_type_alias,
        },
    },
    hir::{ExprId, PatId, generics::GenericParams},
    item_tree::{FieldsShape, RawVisibility, visibility_from_ast},
    src::HasSource,
    type_ref::{ConstRef, TraitRef, TypeBound, TypeRefId},
};

#[inline]
fn as_name_opt(name: Option<ast::Name>) -> Name {
    name.map_or_else(Name::missing, |it| it.as_name())
}

/// The flags of `$Flags` that the attributes `$attrs` set: each a shift and a mask of its one
/// `AttrFlags` bit, OR-ed together, with no branch per flag.
macro_rules! flags_from_attrs {
    ($attrs:expr, $Flags:ident { $($attr:ident => $flag:ident),+ $(,)? }) => {{
        type Bits = <$Flags as bitflags::Flags>::Bits;
        let attrs: AttrFlags = $attrs;
        let attrs = attrs.bits();
        $Flags::from_bits_retain(0 $(| {
            const FROM: u32 = single_bit(AttrFlags::$attr.bits());
            const TO: u32 = single_bit($Flags::$flag.bits() as u64);
            (((attrs >> FROM) & 1) as Bits) << TO
        })+)
    }};
}

/// The position of the one bit of `bits`.
const fn single_bit(bits: u64) -> u32 {
    assert!(bits.count_ones() == 1, "a flag read off an attribute is one bit");
    bits.trailing_zeros()
}

/// Which of `kinds` are among the tokens directly under `node`, read in one pass over its
/// children in the flat tree's kind column: each `*_token()` accessor scans them once more.
fn child_tokens<const N: usize>(node: &SyntaxNode, kinds: [SyntaxKind; N]) -> [bool; N] {
    let Some((tree, at)) = node.flat() else { unreachable!("a syntax node outside a flat tree") };
    let kinds = kinds.map(u16::from);
    let mut found = [false; N];
    for child in tree.children_with_tokens(at) {
        if tree.is_token(child) {
            let kind = tree.kind(child).0;
            for (found, want) in found.iter_mut().zip(kinds) {
                *found |= kind == want;
            }
        }
    }
    found
}

#[derive(Debug, PartialEq, Eq)]
pub struct StructSignature {
    pub name: Name,
    pub generic_params: GenericParams,
    pub store: ExpressionStore,
    pub flags: StructFlags,
    pub shape: FieldsShape,
}

// Each signature is one `Arc` (8 bytes of count before it) holding its store and generic
// params (72 bytes each today) and its own fields, whose bytes are what these assert:
// 160 bytes for a struct, union, enum or trait, 184 for a function or type alias, 88 for a
// const or static (no generic params), 160 for an impl.
const _: () = assert!(
    size_of::<StructSignature>() == size_of::<ExpressionStore>() + size_of::<GenericParams>() + 16
);
const _: () = assert!(
    size_of::<UnionSignature>() == size_of::<ExpressionStore>() + size_of::<GenericParams>() + 16
);
const _: () = assert!(
    size_of::<EnumSignature>() == size_of::<ExpressionStore>() + size_of::<GenericParams>() + 16
);
const _: () = assert!(size_of::<ConstSignature>() == size_of::<ExpressionStore>() + 16);
const _: () = assert!(size_of::<StaticSignature>() == size_of::<ExpressionStore>() + 16);
const _: () = assert!(
    size_of::<ImplSignature>() == size_of::<ExpressionStore>() + size_of::<GenericParams>() + 16
);
const _: () = assert!(
    size_of::<TraitSignature>() == size_of::<ExpressionStore>() + size_of::<GenericParams>() + 16
);
const _: () = assert!(
    size_of::<FunctionSignature>()
        == size_of::<ExpressionStore>() + size_of::<GenericParams>() + 40
);
const _: () = assert!(
    size_of::<TypeAliasSignature>()
        == size_of::<ExpressionStore>() + size_of::<GenericParams>() + 40
);
// One word of flags per signature, tested with a mask.
const _: () = assert!(size_of::<StructFlags>() == 2);
const _: () = assert!(size_of::<EnumFlags>() == 1);
const _: () = assert!(size_of::<ConstFlags>() == 1);
const _: () = assert!(size_of::<StaticFlags>() == 1);
const _: () = assert!(size_of::<ImplFlags>() == 1);
const _: () = assert!(size_of::<TraitFlags>() == 2);
const _: () = assert!(size_of::<FnFlags>() == 2);
const _: () = assert!(size_of::<TypeAliasFlags>() == 1);

bitflags! {
    #[derive(Debug, Copy, Clone, PartialEq, Eq)]
    pub struct StructFlags: u16 {
        /// Indicates whether this struct has `#[repr]`.
        const HAS_REPR = 1 << 0;
        /// Indicates whether the struct has a `#[rustc_has_incoherent_inherent_impls]` attribute.
        const RUSTC_HAS_INCOHERENT_INHERENT_IMPLS = 1 << 1;
        /// Indicates whether the struct has a `#[fundamental]` attribute.
        const FUNDAMENTAL      = 1 << 2;
        /// Indicates whether the struct is `PhantomData`.
        const IS_PHANTOM_DATA  = 1 << 3;
        /// Indicates whether this struct is `Box`.
        const IS_BOX           = 1 << 4;
        /// Indicates whether this struct is `ManuallyDrop`.
        const IS_MANUALLY_DROP = 1 << 5;
        /// Indicates whether this struct is `UnsafeCell`.
        const IS_UNSAFE_CELL   = 1 << 6;
        /// Indicates whether this struct is `UnsafePinned`.
        const IS_UNSAFE_PINNED = 1 << 7;
        /// Indicates whether this struct is `CovariantUnsafeCell`.
        const IS_COVARIANT_UNSAFE_CELL = 1 << 8;
        /// Indicates whether this struct is `f16b`, laid out as the backend's bfloat scalar.
        const IS_F16B = 1 << 9;
        /// Indicates whether this struct is `ppcf128`, laid out as the backend's IBM
        /// double-double scalar.
        const IS_PPC_F128 = 1 << 10;
    }
}

/// The lang items a struct is laid out or checked as no other struct is, with the flag each
/// sets: at most eight pointer compares for a struct with a `#[lang]`, none for any other.
const STRUCT_LANG_ITEMS: [(Symbol, StructFlags); 8] = [
    (sym::phantom_data, StructFlags::IS_PHANTOM_DATA),
    (sym::owned_box, StructFlags::IS_BOX),
    (sym::manually_drop, StructFlags::IS_MANUALLY_DROP),
    (sym::unsafe_cell, StructFlags::IS_UNSAFE_CELL),
    (sym::covariant_unsafe_cell, StructFlags::IS_COVARIANT_UNSAFE_CELL),
    (sym::unsafe_pinned, StructFlags::IS_UNSAFE_PINNED),
    (sym::f16b, StructFlags::IS_F16B),
    (sym::ppcf128, StructFlags::IS_PPC_F128),
];

impl StructFlags {
    /// The flags of a struct or union that its attributes set.
    fn from_attrs(attrs: AttrFlags) -> StructFlags {
        flags_from_attrs!(attrs, StructFlags {
            HAS_REPR => HAS_REPR,
            RUSTC_HAS_INCOHERENT_INHERENT_IMPLS => RUSTC_HAS_INCOHERENT_INHERENT_IMPLS,
            FUNDAMENTAL => FUNDAMENTAL,
        })
    }

    /// The flag of a struct that is the lang item `lang`.
    fn from_lang_item(lang: &Symbol) -> StructFlags {
        STRUCT_LANG_ITEMS
            .iter()
            .find(|(it, _)| it == lang)
            .map_or(StructFlags::empty(), |&(_, flag)| flag)
    }
}

#[salsa::tracked]
impl StructSignature {
    #[salsa::tracked(returns(deref))]
    pub fn of(db: &dyn SourceDatabase, id: StructId) -> Arc<Self> {
        Self::with_source_map(db, id).0.clone()
    }

    #[salsa::tracked(returns(ref))]
    pub fn with_source_map(
        db: &dyn SourceDatabase,
        id: StructId,
    ) -> (Arc<Self>, ExpressionStoreSourceMap) {
        let loc = id.lookup(db);
        let InFile { file_id, value: source } = loc.source(db);
        let attrs = AttrFlags::query(db, id.into());

        let mut flags = StructFlags::from_attrs(attrs);
        if let Some(lang) = attrs.lang_item_with_attrs(db, id.into()) {
            flags |= StructFlags::from_lang_item(&lang);
        }
        let shape = adt_shape(source.kind());

        let (store, generic_params, source_map) = lower_generic_params(
            db,
            loc.container,
            id.into(),
            file_id,
            source.generic_param_list(),
            source.where_clause(),
            LoweringMode::Analysis,
        );
        (
            Arc::new(StructSignature {
                generic_params,
                store,
                flags,
                shape,
                name: as_name_opt(source.name()),
            }),
            source_map,
        )
    }

    #[inline]
    pub fn repr(&self, db: &dyn SourceDatabase, id: StructId) -> Option<ReprOptions> {
        if self.flags.contains(StructFlags::HAS_REPR) {
            AttrFlags::repr_assume_has(db, id.into())
        } else {
            None
        }
    }
}

#[inline]
fn adt_shape(adt_kind: ast::StructKind) -> FieldsShape {
    match adt_kind {
        ast::StructKind::Record(_) => FieldsShape::Record,
        ast::StructKind::Tuple(_) => FieldsShape::Tuple,
        ast::StructKind::Unit => FieldsShape::Unit,
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct UnionSignature {
    pub name: Name,
    pub generic_params: GenericParams,
    pub store: ExpressionStore,
    pub flags: StructFlags,
}

#[salsa::tracked]
impl UnionSignature {
    #[salsa::tracked(returns(deref))]
    pub fn of(db: &dyn SourceDatabase, id: UnionId) -> Arc<Self> {
        Self::with_source_map(db, id).0.clone()
    }

    #[salsa::tracked(returns(ref))]
    pub fn with_source_map(
        db: &dyn SourceDatabase,
        id: UnionId,
    ) -> (Arc<Self>, ExpressionStoreSourceMap) {
        let loc = id.lookup(db);
        let flags = StructFlags::from_attrs(AttrFlags::query(db, id.into()));

        let InFile { file_id, value: source } = loc.source(db);
        let (store, generic_params, source_map) = lower_generic_params(
            db,
            loc.container,
            id.into(),
            file_id,
            source.generic_param_list(),
            source.where_clause(),
            LoweringMode::Analysis,
        );
        (
            Arc::new(UnionSignature {
                generic_params,
                store,
                flags,
                name: as_name_opt(source.name()),
            }),
            source_map,
        )
    }

    #[inline]
    pub fn repr(&self, db: &dyn SourceDatabase, id: UnionId) -> Option<ReprOptions> {
        if self.flags.contains(StructFlags::HAS_REPR) {
            AttrFlags::repr_assume_has(db, id.into())
        } else {
            None
        }
    }
}

bitflags! {
    #[derive(Debug, Copy, Clone, PartialEq, Eq)]
    pub struct EnumFlags: u8 {
        /// Indicates whether this enum has `#[repr]`.
        const HAS_REPR = 1 << 0;
        /// Indicates whether the enum has a `#[rustc_has_incoherent_inherent_impls]` attribute.
        const RUSTC_HAS_INCOHERENT_INHERENT_IMPLS  = 1 << 1;
        /// Whether this enum has `#[fundamental]`.
        const FUNDAMENTAL = 1 << 2;
    }
}

impl EnumFlags {
    fn from_attrs(attrs: AttrFlags) -> EnumFlags {
        flags_from_attrs!(attrs, EnumFlags {
            HAS_REPR => HAS_REPR,
            RUSTC_HAS_INCOHERENT_INHERENT_IMPLS => RUSTC_HAS_INCOHERENT_INHERENT_IMPLS,
            FUNDAMENTAL => FUNDAMENTAL,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct EnumSignature {
    pub name: Name,
    pub generic_params: GenericParams,
    pub store: ExpressionStore,
    pub flags: EnumFlags,
}

#[salsa::tracked]
impl EnumSignature {
    #[salsa::tracked(returns(deref))]
    pub fn of(db: &dyn SourceDatabase, id: EnumId) -> Arc<Self> {
        Self::with_source_map(db, id).0.clone()
    }

    #[salsa::tracked(returns(ref))]
    pub fn with_source_map(
        db: &dyn SourceDatabase,
        id: EnumId,
    ) -> (Arc<Self>, ExpressionStoreSourceMap) {
        let loc = id.lookup(db);
        let flags = EnumFlags::from_attrs(AttrFlags::query(db, id.into()));

        let InFile { file_id, value: source } = loc.source(db);
        let (store, generic_params, source_map) = lower_generic_params(
            db,
            loc.container,
            id.into(),
            file_id,
            source.generic_param_list(),
            source.where_clause(),
            LoweringMode::Analysis,
        );

        (
            Arc::new(EnumSignature {
                generic_params,
                store,
                flags,
                name: as_name_opt(source.name()),
            }),
            source_map,
        )
    }
}

impl EnumSignature {
    pub fn variant_body_type(db: &dyn SourceDatabase, id: EnumId) -> IntegerType {
        match AttrFlags::repr(db, id.into()) {
            Some(ReprOptions { int: Some(builtin), .. }) => builtin,
            _ => IntegerType::Pointer(true),
        }
    }

    #[inline]
    pub fn repr(&self, db: &dyn SourceDatabase, id: EnumId) -> Option<ReprOptions> {
        if self.flags.contains(EnumFlags::HAS_REPR) {
            AttrFlags::repr_assume_has(db, id.into())
        } else {
            None
        }
    }
}
bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
    pub struct ConstFlags: u8 {
        const HAS_BODY = 1 << 1;
        const RUSTC_ALLOW_INCOHERENT_IMPL = 1 << 7;
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ConstSignature {
    pub name: Option<Name>,
    // generic_params: GenericParams,
    pub store: ExpressionStore,
    pub type_ref: TypeRefId,
    pub flags: ConstFlags,
}

#[salsa::tracked]
impl ConstSignature {
    #[salsa::tracked(returns(deref))]
    pub fn of(db: &dyn SourceDatabase, id: ConstId) -> Arc<Self> {
        Self::with_source_map(db, id).0.clone()
    }

    #[salsa::tracked(returns(ref))]
    pub fn with_source_map(
        db: &dyn SourceDatabase,
        id: ConstId,
    ) -> (Arc<Self>, ExpressionStoreSourceMap) {
        let loc = id.lookup(db);

        let module = loc.container.module(db);
        let attrs = AttrFlags::query(db, id.into());
        let mut flags = flags_from_attrs!(attrs, ConstFlags {
            RUSTC_ALLOW_INCOHERENT_IMPL => RUSTC_ALLOW_INCOHERENT_IMPL,
        });
        let source = loc.source(db);
        flags.set(ConstFlags::HAS_BODY, source.value.body().is_some());

        let (store, source_map, type_ref) =
            crate::expr_store::lower::lower_type_ref(db, module, source.as_ref().map(|it| it.ty()));

        (
            Arc::new(ConstSignature {
                store,
                type_ref,
                flags,
                name: source.value.name().map(|it| it.as_name()),
            }),
            source_map,
        )
    }
}

impl ConstSignature {
    pub fn has_body(&self) -> bool {
        self.flags.contains(ConstFlags::HAS_BODY)
    }
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
    pub struct StaticFlags: u8 {
        const HAS_BODY = 1 << 1;
        const MUTABLE    = 1 << 3;
        const UNSAFE     = 1 << 4;
        const EXPLICIT_SAFE = 1 << 5;
        const EXTERN     = 1 << 6;
        const RUSTC_ALLOW_INCOHERENT_IMPL = 1 << 7;
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct StaticSignature {
    pub name: Name,

    // generic_params: GenericParams,
    pub store: ExpressionStore,
    pub type_ref: TypeRefId,
    pub flags: StaticFlags,
}

#[salsa::tracked]
impl StaticSignature {
    #[salsa::tracked(returns(deref))]
    pub fn of(db: &dyn SourceDatabase, id: StaticId) -> Arc<Self> {
        Self::with_source_map(db, id).0.clone()
    }

    #[salsa::tracked(returns(ref))]
    pub fn with_source_map(
        db: &dyn SourceDatabase,
        id: StaticId,
    ) -> (Arc<Self>, ExpressionStoreSourceMap) {
        let loc = id.lookup(db);

        let module = loc.container.module(db);
        let attrs = AttrFlags::query(db, id.into());
        let mut flags = flags_from_attrs!(attrs, StaticFlags {
            RUSTC_ALLOW_INCOHERENT_IMPL => RUSTC_ALLOW_INCOHERENT_IMPL,
        });
        flags.set(StaticFlags::EXTERN, matches!(loc.container, ItemContainerId::ExternBlockId(_)));

        let source = loc.source(db);
        let [mut_, unsafe_, safe] =
            child_tokens(source.value.syntax(), [T![mut], T![unsafe], T![safe]]);
        flags.set(StaticFlags::HAS_BODY, source.value.body().is_some());
        flags.set(StaticFlags::MUTABLE, mut_);
        flags.set(StaticFlags::UNSAFE, unsafe_);
        flags.set(StaticFlags::EXPLICIT_SAFE, safe);

        let (store, source_map, type_ref) =
            crate::expr_store::lower::lower_type_ref(db, module, source.as_ref().map(|it| it.ty()));

        (
            Arc::new(StaticSignature {
                store,
                type_ref,
                flags,
                name: as_name_opt(source.value.name()),
            }),
            source_map,
        )
    }
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
    pub struct ImplFlags: u8 {
        const NEGATIVE = 1 << 1;
        const DEFAULT = 1 << 2;
        const UNSAFE = 1 << 3;
        /// `const impl Tr for T` (or the older `impl const Tr for T`).
        const CONST = 1 << 4;
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ImplSignature {
    pub generic_params: GenericParams,
    pub store: ExpressionStore,
    pub self_ty: TypeRefId,
    pub target_trait: Option<TraitRef>,
    pub flags: ImplFlags,
}

#[salsa::tracked]
impl ImplSignature {
    #[salsa::tracked(returns(deref))]
    pub fn of(db: &dyn SourceDatabase, id: ImplId) -> Arc<Self> {
        Self::with_source_map(db, id).0.clone()
    }

    #[salsa::tracked(returns(ref))]
    pub fn with_source_map(
        db: &dyn SourceDatabase,
        id: ImplId,
    ) -> (Arc<Self>, ExpressionStoreSourceMap) {
        let loc = id.lookup(db);

        let src = loc.source(db);
        let [unsafe_, negative, default, const_] =
            child_tokens(src.value.syntax(), [T![unsafe], T![!], T![default], T![const]]);
        let mut flags = ImplFlags::empty();
        flags.set(ImplFlags::UNSAFE, unsafe_);
        flags.set(ImplFlags::NEGATIVE, negative);
        flags.set(ImplFlags::DEFAULT, default);
        flags.set(ImplFlags::CONST, const_);

        let (store, source_map, self_ty, target_trait, generic_params) =
            crate::expr_store::lower::lower_impl(db, loc.container, src, id);

        (
            Arc::new(ImplSignature { store, generic_params, self_ty, target_trait, flags }),
            source_map,
        )
    }
}

impl ImplSignature {
    #[inline]
    pub fn is_negative(&self) -> bool {
        self.flags.contains(ImplFlags::NEGATIVE)
    }

    #[inline]
    pub fn is_default(&self) -> bool {
        self.flags.contains(ImplFlags::DEFAULT)
    }

    #[inline]
    pub fn is_const(&self) -> bool {
        self.flags.contains(ImplFlags::CONST)
    }
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
    pub struct TraitFlags: u16 {
        const RUSTC_HAS_INCOHERENT_INHERENT_IMPLS = 1 << 1;
        const FUNDAMENTAL = 1 << 2;
        const UNSAFE = 1 << 3;
        const AUTO = 1 << 4;
        const SKIP_ARRAY_DURING_METHOD_DISPATCH = 1 << 5;
        const SKIP_BOXED_SLICE_DURING_METHOD_DISPATCH = 1 << 6;
        const RUSTC_PAREN_SUGAR = 1 << 7;
        const COINDUCTIVE = 1 << 8;
        const ALIAS = 1 << 9;
        /// `const trait Tr`.
        const CONST = 1 << 10;
    }
}

impl TraitFlags {
    fn from_attrs(attrs: AttrFlags) -> TraitFlags {
        flags_from_attrs!(attrs, TraitFlags {
            FUNDAMENTAL => FUNDAMENTAL,
            RUSTC_HAS_INCOHERENT_INHERENT_IMPLS => RUSTC_HAS_INCOHERENT_INHERENT_IMPLS,
            RUSTC_PAREN_SUGAR => RUSTC_PAREN_SUGAR,
            RUSTC_COINDUCTIVE => COINDUCTIVE,
            RUSTC_SKIP_ARRAY_DURING_METHOD_DISPATCH => SKIP_ARRAY_DURING_METHOD_DISPATCH,
            RUSTC_SKIP_BOXED_SLICE_DURING_METHOD_DISPATCH => SKIP_BOXED_SLICE_DURING_METHOD_DISPATCH,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct TraitSignature {
    pub name: Name,
    pub generic_params: GenericParams,
    pub store: ExpressionStore,
    pub flags: TraitFlags,
}

#[salsa::tracked]
impl TraitSignature {
    #[salsa::tracked(returns(deref))]
    pub fn of(db: &dyn SourceDatabase, id: TraitId) -> Arc<Self> {
        Self::with_source_map(db, id).0.clone()
    }

    #[salsa::tracked(returns(ref))]
    pub fn with_source_map(
        db: &dyn SourceDatabase,
        id: TraitId,
    ) -> (Arc<Self>, ExpressionStoreSourceMap) {
        let loc = id.lookup(db);

        let mut flags = TraitFlags::from_attrs(AttrFlags::query(db, id.into()));
        let source = loc.source(db);
        let [auto, unsafe_, alias, const_] = child_tokens(
            source.value.syntax(),
            [T![auto], T![unsafe], T![=], T![const]],
        );
        if auto {
            flags |= TraitFlags::AUTO | TraitFlags::COINDUCTIVE;
        }
        flags.set(TraitFlags::UNSAFE, unsafe_);
        flags.set(TraitFlags::ALIAS, alias);
        flags.set(TraitFlags::CONST, const_);

        let name = as_name_opt(source.value.name());
        let (store, source_map, generic_params) = lower_trait(db, loc.container, source, id);

        (Arc::new(TraitSignature { store, generic_params, flags, name }), source_map)
    }
}

bitflags! {
    #[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
    pub struct FnFlags: u16 {
        const HAS_BODY = 1 << 1;
        const DEFAULT = 1 << 2;
        const CONST = 1 << 3;
        const ASYNC = 1 << 4;
        const GEN = 1 << 5;
        const UNSAFE = 1 << 6;
        const HAS_VARARGS = 1 << 7;
        const RUSTC_ALLOW_INCOHERENT_IMPL = 1 << 8;
        const HAS_SELF_PARAM = 1 << 9;
        /// The `#[target_feature]` attribute is necessary to check safety (with RFC 2396),
        /// but keeping it for all functions will consume a lot of memory when there are
        /// only very few functions with it. So we only encode its existence here, and lookup
        /// it if needed.
        const HAS_TARGET_FEATURE = 1 << 10;
        /// `#[rustc_deprecated_safe_2024]` on an `unsafe fn`, which keeps `UNSAFE`: its body
        /// is an unsafe function's, and a call of it before edition 2024 needs no `unsafe`
        /// block, only the `deprecated_safe_2024` lint (rustc's `check_unsafety`).
        const DEPRECATED_SAFE_2024 = 1 << 11;
        const EXPLICIT_SAFE = 1 << 12;
        const HAS_LEGACY_CONST_GENERICS = 1 << 13;
        const RUSTC_INTRINSIC = 1 << 14;
    }
}

impl FnFlags {
    /// The flags of a function its attributes and its `unsafe` keyword set.
    fn from_attrs(attrs: AttrFlags, unsafe_: bool) -> FnFlags {
        let mut flags = flags_from_attrs!(attrs, FnFlags {
            RUSTC_ALLOW_INCOHERENT_IMPL => RUSTC_ALLOW_INCOHERENT_IMPL,
            HAS_TARGET_FEATURE => HAS_TARGET_FEATURE,
            RUSTC_INTRINSIC => RUSTC_INTRINSIC,
            HAS_LEGACY_CONST_GENERICS => HAS_LEGACY_CONST_GENERICS,
            RUSTC_DEPRECATED_SAFE_2024 => DEPRECATED_SAFE_2024,
        });
        if !unsafe_ {
            flags.remove(FnFlags::DEPRECATED_SAFE_2024);
        }
        flags.set(FnFlags::UNSAFE, unsafe_);
        flags
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct FunctionSignature {
    pub name: Name,
    pub generic_params: GenericParams,
    pub store: ExpressionStore,
    pub params: Box<[TypeRefId]>,
    pub ret_type: Option<TypeRefId>,
    pub abi: ExternAbi,
    pub flags: FnFlags,
}

#[salsa::tracked]
impl FunctionSignature {
    #[salsa::tracked(returns(deref))]
    pub fn of(db: &dyn SourceDatabase, id: FunctionId) -> Arc<Self> {
        Self::with_source_map(db, id).0.clone()
    }

    #[salsa::tracked(returns(ref))]
    pub fn with_source_map(
        db: &dyn SourceDatabase,
        id: FunctionId,
    ) -> (Arc<Self>, ExpressionStoreSourceMap) {
        let loc = id.lookup(db);
        let module = loc.container.module(db);

        let attrs = AttrFlags::query(db, id.into());
        let source = loc.source(db);
        let [unsafe_, async_, gen_, const_, default, safe] = child_tokens(
            source.value.syntax(),
            [T![unsafe], T![async], T![gen], T![const], T![default], T![safe]],
        );
        let mut flags = FnFlags::from_attrs(attrs, unsafe_);
        flags.set(FnFlags::ASYNC, async_);
        flags.set(FnFlags::GEN, gen_);
        flags.set(FnFlags::CONST, const_);
        flags.set(FnFlags::DEFAULT, default);
        flags.set(FnFlags::EXPLICIT_SAFE, safe);
        flags.set(FnFlags::HAS_BODY, source.value.body().is_some());

        let name = as_name_opt(source.value.name());
        let abi = source
            .value
            .abi()
            .map(|abi| {
                abi.abi_string()
                    .and_then(|abi| abi.text_without_quotes().parse().ok())
                    .unwrap_or(ExternAbi::FALLBACK)
            })
            .or_else(|| match loc.container {
                ItemContainerId::ExternBlockId(extern_block) => {
                    Some(extern_block_abi(db, extern_block))
                }
                ItemContainerId::ModuleId(_)
                | ItemContainerId::ImplId(_)
                | ItemContainerId::TraitId(_) => None,
            })
            .unwrap_or(ExternAbi::Rust);
        let (store, source_map, generic_params, params, ret_type, self_param, variadic) =
            lower_function(db, module, source, id);
        flags.set(FnFlags::HAS_SELF_PARAM, self_param);
        flags.set(FnFlags::HAS_VARARGS, variadic);
        (
            Arc::new(FunctionSignature {
                generic_params,
                store,
                params,
                ret_type,
                abi,
                flags,
                name,
            }),
            source_map,
        )
    }
}

impl FunctionSignature {
    pub fn has_body(&self) -> bool {
        self.flags.contains(FnFlags::HAS_BODY)
    }

    /// True if the first param is `self`. This is relevant to decide whether this
    /// can be called as a method.
    pub fn has_self_param(&self) -> bool {
        self.flags.contains(FnFlags::HAS_SELF_PARAM)
    }

    pub fn is_default(&self) -> bool {
        self.flags.contains(FnFlags::DEFAULT)
    }

    pub fn is_const(&self) -> bool {
        self.flags.contains(FnFlags::CONST)
    }

    pub fn is_async(&self) -> bool {
        self.flags.contains(FnFlags::ASYNC)
    }

    pub fn is_gen(&self) -> bool {
        self.flags.contains(FnFlags::GEN)
    }

    pub fn is_unsafe(&self) -> bool {
        self.flags.contains(FnFlags::UNSAFE)
    }

    pub fn is_deprecated_safe_2024(&self) -> bool {
        self.flags.contains(FnFlags::DEPRECATED_SAFE_2024)
    }

    pub fn is_safe(&self) -> bool {
        self.flags.contains(FnFlags::EXPLICIT_SAFE)
    }

    pub fn is_varargs(&self) -> bool {
        self.flags.contains(FnFlags::HAS_VARARGS)
    }

    pub fn has_target_feature(&self) -> bool {
        self.flags.contains(FnFlags::HAS_TARGET_FEATURE)
    }

    #[inline]
    pub fn legacy_const_generics_indices<'db>(
        &self,
        db: &'db dyn SourceDatabase,
        id: FunctionId,
    ) -> Option<&'db [u32]> {
        if !self.flags.contains(FnFlags::HAS_LEGACY_CONST_GENERICS) {
            return None;
        }

        AttrFlags::legacy_const_generic_indices(db, id).as_deref()
    }

    pub fn is_intrinsic(db: &dyn SourceDatabase, id: FunctionId) -> bool {
        let data = FunctionSignature::of(db, id);
        data.flags.contains(FnFlags::RUSTC_INTRINSIC)
    }
}

bitflags! {
    #[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
    pub struct TypeAliasFlags: u8 {
        const RUSTC_HAS_INCOHERENT_INHERENT_IMPL = 1 << 1;
        const IS_EXTERN = 1 << 6;
        const RUSTC_ALLOW_INCOHERENT_IMPL = 1 << 7;
    }
}

impl TypeAliasFlags {
    fn from_attrs(attrs: AttrFlags) -> TypeAliasFlags {
        flags_from_attrs!(attrs, TypeAliasFlags {
            RUSTC_HAS_INCOHERENT_INHERENT_IMPLS => RUSTC_HAS_INCOHERENT_INHERENT_IMPL,
            RUSTC_ALLOW_INCOHERENT_IMPL => RUSTC_ALLOW_INCOHERENT_IMPL,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct TypeAliasSignature {
    pub name: Name,
    pub generic_params: GenericParams,
    pub store: ExpressionStore,
    pub bounds: Box<[TypeBound]>,
    pub ty: Option<TypeRefId>,
    pub flags: TypeAliasFlags,
}

#[salsa::tracked]
impl TypeAliasSignature {
    #[salsa::tracked(returns(deref))]
    pub fn of(db: &dyn SourceDatabase, id: TypeAliasId) -> Arc<Self> {
        Self::with_source_map(db, id).0.clone()
    }

    #[salsa::tracked(returns(ref))]
    pub fn with_source_map(
        db: &dyn SourceDatabase,
        id: TypeAliasId,
    ) -> (Arc<Self>, ExpressionStoreSourceMap) {
        let loc = id.lookup(db);

        let mut flags = TypeAliasFlags::from_attrs(AttrFlags::query(db, id.into()));
        flags.set(
            TypeAliasFlags::IS_EXTERN,
            matches!(loc.container, ItemContainerId::ExternBlockId(_)),
        );
        let source = loc.source(db);
        let name = as_name_opt(source.value.name());
        let (store, source_map, generic_params, bounds, ty) =
            lower_type_alias(db, loc.container, source, id);

        (
            Arc::new(TypeAliasSignature { store, generic_params, flags, bounds, name, ty }),
            source_map,
        )
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct FunctionBody {
    pub store: ExpressionStore,
    pub parameters: Box<[PatId]>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct VariantFieldsBody {
    pub store: ExpressionStore,
    pub fields: Box<[Option<ExprId>]>,
}

/// A single field of an enum variant or struct
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldData {
    pub name: Name,
    pub type_ref: TypeRefId,
    pub visibility: RawVisibility,
    pub is_unsafe: bool,
    pub default_value: Option<ConstRef>,
}

// 40 bytes per field with today's 16-byte `RawVisibility`, in one array per variant.
const _: () = assert!(size_of::<FieldData>() == size_of::<RawVisibility>() + 24);

pub type LocalFieldId = Idx<FieldData>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariantFields {
    fields: Arena<FieldData>,
    pub store: ExpressionStore,
    pub shape: FieldsShape,
}

// The field array (24 bytes), the shape and the store (72 bytes today): 104 bytes in one `Arc`.
const _: () = assert!(size_of::<VariantFields>() == size_of::<ExpressionStore>() + 32);

#[salsa::tracked]
impl VariantFields {
    #[salsa::tracked(returns(ref))]
    pub fn with_source_map(
        db: &dyn SourceDatabase,
        id: VariantId,
    ) -> (Arc<Self>, ExpressionStoreSourceMap) {
        let (shape, result) = match id {
            VariantId::EnumVariantId(id) => {
                let loc = id.lookup(db);
                let parent = loc.parent.lookup(db);
                let source = loc.source(db);
                let shape = adt_shape(source.value.kind());
                let enum_vis = Some(source.value.parent_enum().visibility());
                let fields = lower_field_list(
                    db,
                    parent.container,
                    source.map(|src| src.field_list()),
                    enum_vis,
                );
                (shape, fields)
            }
            VariantId::StructId(id) => {
                let loc = id.lookup(db);
                let source = loc.source(db);
                let shape = adt_shape(source.value.kind());
                let fields =
                    lower_field_list(db, loc.container, source.map(|src| src.field_list()), None);
                (shape, fields)
            }
            VariantId::UnionId(id) => {
                let loc = id.lookup(db);
                let source = loc.source(db);
                let fields = lower_field_list(
                    db,
                    loc.container,
                    source.map(|src| src.record_field_list().map(ast::FieldList::RecordFieldList)),
                    None,
                );
                (FieldsShape::Record, fields)
            }
        };
        match result {
            Some((fields, store, source_map)) => {
                (Arc::new(VariantFields { fields, store, shape }), source_map)
            }
            None => (
                Arc::new(VariantFields {
                    fields: Arena::default(),
                    store: ExpressionStore::empty().clone(),
                    shape,
                }),
                ExpressionStoreSourceMap::default(),
            ),
        }
    }

    #[salsa::tracked(returns(deref))]
    pub fn of(db: &dyn SourceDatabase, id: VariantId) -> Arc<Self> {
        Self::with_source_map(db, id).0.clone()
    }
}

impl VariantFields {
    pub fn len(&self) -> usize {
        self.fields.len()
    }

    pub fn fields(&self) -> &Arena<FieldData> {
        &self.fields
    }

    pub fn field(&self, name: &Name) -> Option<LocalFieldId> {
        self.fields().iter().find_map(|(id, data)| if &data.name == name { Some(id) } else { None })
    }
}

fn lower_field_list(
    db: &dyn SourceDatabase,
    module: ModuleId,
    fields: InFile<Option<ast::FieldList>>,
    override_visibility: Option<Option<ast::Visibility>>,
) -> Option<(Arena<FieldData>, ExpressionStore, ExpressionStoreSourceMap)> {
    let file_id = fields.file_id;
    match fields.value? {
        ast::FieldList::RecordFieldList(fields) => lower_fields(
            db,
            module,
            fields.fields().count(),
            InFile::new(file_id, fields.fields().map(|field| (field.ty(), field))),
            |_, field| as_name_opt(field.name()),
            |field| field.eq_token().and_then(|_| field.default_val()),
            override_visibility,
        ),
        ast::FieldList::TupleFieldList(fields) => lower_fields(
            db,
            module,
            fields.fields().count(),
            InFile::new(file_id, fields.fields().map(|field| (field.ty(), field))),
            |idx, _| Name::new_tuple_field(idx),
            |_| None,
            override_visibility,
        ),
    }
}

/// The `count` fields of a list, written with their types; `None` when it has none written,
/// before anything is set up to lower them.
fn lower_fields<Field: ast::HasAttrs + ast::HasVisibility>(
    db: &dyn SourceDatabase,
    module: ModuleId,
    count: usize,
    fields: InFile<impl Iterator<Item = (Option<ast::Type>, Field)>>,
    mut field_name: impl FnMut(usize, &Field) -> Name,
    mut default_value: impl FnMut(&Field) -> Option<ast::ConstArg>,
    override_visibility: Option<Option<ast::Visibility>>,
) -> Option<(Arena<FieldData>, ExpressionStore, ExpressionStoreSourceMap)> {
    if count == 0 {
        return None;
    }
    let cfg_options = module.krate(db).cfg_options(db);
    let mut col = ExprCollector::new(db, module, fields.file_id, crate::LoweringMode::Analysis);
    let override_visibility = override_visibility.map(|vis| {
        LazyCell::new(|| {
            let span_map = fields.file_id.span_map(db);
            visibility_from_ast(db, vis, &mut |range| span_map.span_for_range(range).ctx)
        })
    });

    let mut arena = Arena::with_capacity(count);
    let mut idx = 0;
    for (ty, field) in fields.value {
        match AttrFlags::is_cfg_enabled_for(&field, cfg_options) {
            Ok(()) => {
                let type_ref =
                    col.lower_type_ref_opt(ty, &mut ExprCollector::impl_trait_error_allocator);
                let visibility = override_visibility.as_ref().map_or_else(
                    || {
                        visibility_from_ast(db, field.visibility(), &mut |range| {
                            col.span_map().span_for_range(range).ctx
                        })
                    },
                    |it| RawVisibility::clone(it),
                );
                let [is_unsafe] = child_tokens(field.syntax(), [T![unsafe]]);
                let name = field_name(idx, &field);
                let default_value = default_value(&field).map(|expr| col.lower_const_arg(expr));

                arena.alloc(FieldData { name, type_ref, visibility, is_unsafe, default_value });
                idx += 1;
            }
            Err(cfg) => {
                col.store.diagnostics.push(
                    crate::expr_store::ExpressionStoreDiagnostics::InactiveCode {
                        node: InFile::new(fields.file_id, SyntaxNodePtr::new(field.syntax())),
                        cfg,
                        opts: cfg_options.clone(),
                    },
                );
            }
        }
    }
    let (store, source_map) = col.store.finish();
    if arena.len() < count {
        arena.shrink_to_fit();
    }
    Some((arena, store, source_map))
}

#[derive(Debug, PartialEq, Eq)]
pub struct InactiveEnumVariantCode {
    pub cfg: CfgExpr,
    pub opts: CfgOptions,
    pub ast_id: span::FileAstId<ast::Variant>,
}

#[derive(Debug, Clone)]
pub struct EnumVariants {
    pub variants: FxIndexMap<Name, (EnumVariantId, FieldsShape)>,
    /// For each of [`EnumVariants::variants`], at the same index, whether it is written with
    /// an explicit discriminant (`= expr`): rustc's `VariantDiscr::Explicit`, read off the
    /// syntax, so that nothing a signature asks lowers the discriminant's body.
    explicit_discriminant: VariantBits,
}

/// One bit per variant, by index, kept inline up to 64 variants: no allocation for almost
/// every enum. Bits past the last variant are zero, so equal sets are equal words.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct VariantBits(SmallVec<[u64; 1]>);

// The inline word and its length, or a heap pointer and its length, plus the capacity.
const _: () = assert!(size_of::<VariantBits>() == 24);

impl VariantBits {
    /// `len` bits, all clear.
    fn with_len(len: usize) -> VariantBits {
        VariantBits(smallvec![0; len.div_ceil(64)])
    }

    fn get(&self, index: usize) -> bool {
        (self.0[index / 64] >> (index % 64)) & 1 == 1
    }

    fn set(&mut self, index: usize, value: bool) {
        let (word, bit) = (&mut self.0[index / 64], index % 64);
        *word = (*word & !(1 << bit)) | (u64::from(value) << bit);
    }

    /// Keeps the words of the first `len` bits, the rest being clear.
    fn truncate(&mut self, len: usize) {
        self.0.truncate(len.div_ceil(64));
        self.0.shrink_to_fit();
    }
}

// The order of the variants is part of the enum: discriminants and layout follow it. Two
// `IndexMap`s with the same entries are equal whatever their order.
impl PartialEq for EnumVariants {
    fn eq(&self, other: &Self) -> bool {
        self.variants.len() == other.variants.len()
            && self.variants.iter().eq(other.variants.iter())
            && self.explicit_discriminant == other.explicit_discriminant
    }
}

impl Eq for EnumVariants {}

#[salsa::tracked]
impl EnumVariants {
    #[salsa::tracked(returns(ref))]
    pub(crate) fn of(
        db: &dyn SourceDatabase,
        e: EnumId,
    ) -> (EnumVariants, ThinVec<InactiveEnumVariantCode>) {
        let loc = e.lookup(db);
        let source = loc.source(db);
        let ast_id_map = source.file_id.ast_id_map(db);

        let mut diagnostics = ThinVec::new();
        let cfg_options = loc.container.krate(db).cfg_options(db);
        let Some(list) = source.value.variant_list() else {
            let variants = FxIndexMap::default();
            let explicit_discriminant = VariantBits::default();
            return (EnumVariants { variants, explicit_discriminant }, ThinVec::new());
        };
        let count = list.variants().count();
        let mut variants = FxIndexMap::with_capacity_and_hasher(count, Default::default());
        let mut explicit_discriminant = VariantBits::with_len(count);
        for variant in list.variants() {
            let ast_id = ast_id_map.ast_id(&variant);
            match AttrFlags::is_cfg_enabled_for(&variant, cfg_options) {
                Ok(()) => {
                    let name = as_name_opt(variant.name());
                    let id = source.with_value(ast_id);
                    let enum_variant = EnumVariantLoc { id, parent: e, name: name.clone() }.intern(db);
                    let shape = adt_shape(variant.kind());
                    // The expression the variant's body is lowered from, when it has one.
                    let explicit = variant.const_arg().and_then(|it| it.expr()).is_some();
                    // A name written twice keeps its first place and the last variant's value.
                    let (index, _) = variants.insert_full(name, (enum_variant, shape));
                    explicit_discriminant.set(index, explicit);
                }
                Err(cfg) => {
                    diagnostics.push(InactiveEnumVariantCode {
                        ast_id,
                        cfg,
                        opts: cfg_options.clone(),
                    });
                }
            }
        }
        if variants.len() < count {
            variants.shrink_to_fit();
            explicit_discriminant.truncate(variants.len());
        }
        diagnostics.shrink_to_fit();

        (EnumVariants { variants, explicit_discriminant }, diagnostics)
    }
}

impl EnumVariants {
    /// Whether `variant` is written with an explicit discriminant, `= expr`. Read off the
    /// syntax when the variants are lowered; its body is not lowered.
    pub fn has_explicit_discriminant(&self, variant: EnumVariantId) -> bool {
        self.variants
            .values()
            .position(|&(it, _)| it == variant)
            .is_some_and(|index| self.explicit_discriminant.get(index))
    }

    /// Each variant with whether it is written with an explicit discriminant.
    pub fn with_explicit_discriminant(
        &self,
    ) -> impl Iterator<Item = (EnumVariantId, FieldsShape, bool)> + '_ {
        self.variants
            .values()
            .enumerate()
            .map(|(index, &(id, shape))| (id, shape, self.explicit_discriminant.get(index)))
    }

    pub fn variant(&self, name: &Name) -> Option<EnumVariantId> {
        self.variants.get(name).map(|&(id, _)| id)
    }

    pub fn variant_name_by_id(&self, variant_id: EnumVariantId) -> Option<Name> {
        self.variants
            .iter()
            .find_map(|(name, (id, _))| if *id == variant_id { Some(name.clone()) } else { None })
    }

    // [Adopted from rustc](https://github.com/rust-lang/rust/blob/bd53aa3bf7a24a70d763182303bd75e5fc51a9af/compiler/rustc_middle/src/ty/adt.rs#L446-L448)
    pub fn is_payload_free(&self, db: &dyn SourceDatabase) -> bool {
        self.with_explicit_discriminant().all(|(v, shape, explicit)| {
            // The condition check order is slightly modified from rustc
            // to improve performance by early returning with relatively fast checks

            // The outer if condition is whether this variant has const ctor or not
            if !matches!(shape, FieldsShape::Unit) {
                let variant = v.fields(db);
                if !variant.fields().is_empty() {
                    return false;
                }

                if explicit {
                    return false;
                }
            }
            true
        })
    }
}

#[salsa::tracked(returns(copy))]
pub(crate) fn extern_block_abi(db: &dyn SourceDatabase, extern_block: ExternBlockId) -> ExternAbi {
    let source = extern_block.lookup(db).source(db);
    source
        .value
        .abi()
        .and_then(|abi| abi.abi_string()?.text_without_quotes().parse().ok())
        .unwrap_or(ExternAbi::FALLBACK)
}

#[cfg(test)]
mod tests {
    use syntax::{AstNode, Edition, NodeOrToken, SourceFile, T, ast};

    use super::*;

    fn splitmix(state: &mut u64) -> u64 {
        *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Every single bit, none, all, and seeded random words.
    fn attr_inputs() -> Vec<AttrFlags> {
        let mut state = 0x5167_5f66_6c61_6773;
        (0..64)
            .map(|bit| 1u64 << bit)
            .chain([0, u64::MAX])
            .chain((0..4096).map(|_| splitmix(&mut state)))
            .map(AttrFlags::from_bits_retain)
            .collect()
    }

    /// The flags as they were read before: one `if` per attribute.
    fn struct_reference(attrs: AttrFlags) -> StructFlags {
        let mut flags = StructFlags::empty();
        if attrs.contains(AttrFlags::RUSTC_HAS_INCOHERENT_INHERENT_IMPLS) {
            flags |= StructFlags::RUSTC_HAS_INCOHERENT_INHERENT_IMPLS;
        }
        if attrs.contains(AttrFlags::FUNDAMENTAL) {
            flags |= StructFlags::FUNDAMENTAL;
        }
        if attrs.contains(AttrFlags::HAS_REPR) {
            flags |= StructFlags::HAS_REPR;
        }
        flags
    }

    fn enum_reference(attrs: AttrFlags) -> EnumFlags {
        let mut flags = EnumFlags::empty();
        if attrs.contains(AttrFlags::RUSTC_HAS_INCOHERENT_INHERENT_IMPLS) {
            flags |= EnumFlags::RUSTC_HAS_INCOHERENT_INHERENT_IMPLS;
        }
        if attrs.contains(AttrFlags::HAS_REPR) {
            flags |= EnumFlags::HAS_REPR;
        }
        if attrs.contains(AttrFlags::FUNDAMENTAL) {
            flags |= EnumFlags::FUNDAMENTAL;
        }
        flags
    }

    fn trait_reference(attrs: AttrFlags) -> TraitFlags {
        let mut flags = TraitFlags::empty();
        if attrs.contains(AttrFlags::FUNDAMENTAL) {
            flags |= TraitFlags::FUNDAMENTAL;
        }
        if attrs.contains(AttrFlags::RUSTC_HAS_INCOHERENT_INHERENT_IMPLS) {
            flags |= TraitFlags::RUSTC_HAS_INCOHERENT_INHERENT_IMPLS;
        }
        if attrs.contains(AttrFlags::RUSTC_PAREN_SUGAR) {
            flags |= TraitFlags::RUSTC_PAREN_SUGAR;
        }
        if attrs.contains(AttrFlags::RUSTC_COINDUCTIVE) {
            flags |= TraitFlags::COINDUCTIVE;
        }
        if attrs.contains(AttrFlags::RUSTC_SKIP_ARRAY_DURING_METHOD_DISPATCH) {
            flags |= TraitFlags::SKIP_ARRAY_DURING_METHOD_DISPATCH;
        }
        if attrs.contains(AttrFlags::RUSTC_SKIP_BOXED_SLICE_DURING_METHOD_DISPATCH) {
            flags |= TraitFlags::SKIP_BOXED_SLICE_DURING_METHOD_DISPATCH;
        }
        flags
    }

    /// As before, except that an `unsafe fn` with `#[rustc_deprecated_safe_2024]` keeps
    /// `UNSAFE` (rustc's `fn_sig` of it is unsafe).
    fn fn_reference(attrs: AttrFlags, unsafe_: bool) -> FnFlags {
        let mut flags = FnFlags::empty();
        if attrs.contains(AttrFlags::RUSTC_ALLOW_INCOHERENT_IMPL) {
            flags.insert(FnFlags::RUSTC_ALLOW_INCOHERENT_IMPL);
        }
        if attrs.contains(AttrFlags::HAS_TARGET_FEATURE) {
            flags.insert(FnFlags::HAS_TARGET_FEATURE);
        }
        if attrs.contains(AttrFlags::RUSTC_INTRINSIC) {
            flags.insert(FnFlags::RUSTC_INTRINSIC);
        }
        if attrs.contains(AttrFlags::HAS_LEGACY_CONST_GENERICS) {
            flags.insert(FnFlags::HAS_LEGACY_CONST_GENERICS);
        }
        if unsafe_ {
            flags.insert(FnFlags::UNSAFE);
            if attrs.contains(AttrFlags::RUSTC_DEPRECATED_SAFE_2024) {
                flags.insert(FnFlags::DEPRECATED_SAFE_2024);
            }
        }
        flags
    }

    fn type_alias_reference(attrs: AttrFlags) -> TypeAliasFlags {
        let mut flags = TypeAliasFlags::empty();
        if attrs.contains(AttrFlags::RUSTC_HAS_INCOHERENT_INHERENT_IMPLS) {
            flags.insert(TypeAliasFlags::RUSTC_HAS_INCOHERENT_INHERENT_IMPL);
        }
        if attrs.contains(AttrFlags::RUSTC_ALLOW_INCOHERENT_IMPL) {
            flags.insert(TypeAliasFlags::RUSTC_ALLOW_INCOHERENT_IMPL);
        }
        flags
    }

    #[test]
    fn flags_from_attrs_as_one_if_per_flag() {
        for attrs in attr_inputs() {
            assert_eq!(StructFlags::from_attrs(attrs), struct_reference(attrs), "{attrs:?}");
            assert_eq!(EnumFlags::from_attrs(attrs), enum_reference(attrs), "{attrs:?}");
            assert_eq!(TraitFlags::from_attrs(attrs), trait_reference(attrs), "{attrs:?}");
            assert_eq!(TypeAliasFlags::from_attrs(attrs), type_alias_reference(attrs), "{attrs:?}");
            for unsafe_ in [false, true] {
                assert_eq!(
                    FnFlags::from_attrs(attrs, unsafe_),
                    fn_reference(attrs, unsafe_),
                    "{attrs:?} unsafe: {unsafe_}"
                );
            }
            let allow = attrs.contains(AttrFlags::RUSTC_ALLOW_INCOHERENT_IMPL);
            let const_flags = flags_from_attrs!(attrs, ConstFlags {
                RUSTC_ALLOW_INCOHERENT_IMPL => RUSTC_ALLOW_INCOHERENT_IMPL,
            });
            assert_eq!(const_flags.contains(ConstFlags::RUSTC_ALLOW_INCOHERENT_IMPL), allow);
            assert_eq!(const_flags - ConstFlags::RUSTC_ALLOW_INCOHERENT_IMPL, ConstFlags::empty());
            let static_flags = flags_from_attrs!(attrs, StaticFlags {
                RUSTC_ALLOW_INCOHERENT_IMPL => RUSTC_ALLOW_INCOHERENT_IMPL,
            });
            assert_eq!(static_flags.contains(StaticFlags::RUSTC_ALLOW_INCOHERENT_IMPL), allow);
            assert_eq!(
                static_flags - StaticFlags::RUSTC_ALLOW_INCOHERENT_IMPL,
                StaticFlags::empty()
            );
        }
    }

    #[test]
    fn struct_lang_items_as_a_match() {
        let reference = |lang: &Symbol| match lang {
            _ if *lang == sym::phantom_data => StructFlags::IS_PHANTOM_DATA,
            _ if *lang == sym::owned_box => StructFlags::IS_BOX,
            _ if *lang == sym::manually_drop => StructFlags::IS_MANUALLY_DROP,
            _ if *lang == sym::unsafe_cell => StructFlags::IS_UNSAFE_CELL,
            _ if *lang == sym::covariant_unsafe_cell => StructFlags::IS_COVARIANT_UNSAFE_CELL,
            _ if *lang == sym::unsafe_pinned => StructFlags::IS_UNSAFE_PINNED,
            _ if *lang == sym::f16b => StructFlags::IS_F16B,
            _ if *lang == sym::ppcf128 => StructFlags::IS_PPC_F128,
            _ => StructFlags::empty(),
        };
        let others = [sym::sized, sym::copy, sym::non_null, sym::va_list, sym::drop];
        let interned = ["phantom_data", "owned_box", "ppcf128", "not_a_lang_item"].map(Symbol::intern);
        for lang in STRUCT_LANG_ITEMS.iter().map(|(it, _)| it.clone()).chain(others).chain(interned) {
            assert_eq!(StructFlags::from_lang_item(&lang), reference(&lang), "{lang:?}");
        }
    }

    const KEYWORDS: &str = r#"
unsafe fn a() {}
const async unsafe extern "C" fn b() {}
impl Z { default fn c() {} }
pub(crate) const fn d() {}
fn e() -> i32 { 0 }
async gen fn f() {}
unsafe extern "C" {
    safe fn g();
    unsafe fn h();
    safe static S: u8;
    unsafe static mut T: u8;
    static U: u8;
}
static mut V: u8 = 0;
static W: u8 = 0;
unsafe impl Send for X {}
impl !Sync for X {}
default impl<T> Tr for T {}
impl const Tr for X {}
impl Y {}
auto trait Au {}
unsafe trait Un {}
trait Al = Clone;
const trait Co {}
trait Pl {}
struct R { unsafe a: u8, b: u8, pub c: u8 }
struct Q(pub u8, u8);
"#;

    #[test]
    fn child_tokens_as_the_accessors() {
        let file = SourceFile::parse(KEYWORDS, Edition::CURRENT).tree();
        let mut seen_fn = [false; 6];
        let mut seen_static = [false; 3];
        let mut seen_impl = [false; 4];
        let mut seen_trait = [false; 3];
        let mut seen_unsafe_field = false;
        fn or(seen: &mut [bool], found: &[bool]) {
            seen.iter_mut().zip(found).for_each(|(seen, found)| *seen |= *found)
        }
        for node in file.syntax().descendants() {
            if let Some(it) = ast::Fn::cast(node.clone()) {
                let found = child_tokens(
                    it.syntax(),
                    [T![unsafe], T![async], T![gen], T![const], T![default], T![safe]],
                );
                let accessors = [
                    it.unsafe_token().is_some(),
                    it.async_token().is_some(),
                    it.gen_token().is_some(),
                    it.const_token().is_some(),
                    it.default_token().is_some(),
                    it.safe_token().is_some(),
                ];
                assert_eq!(found, accessors, "{it}");
                or(&mut seen_fn, &found);
            } else if let Some(it) = ast::Static::cast(node.clone()) {
                let found = child_tokens(it.syntax(), [T![mut], T![unsafe], T![safe]]);
                let accessors = [
                    it.mut_token().is_some(),
                    it.unsafe_token().is_some(),
                    it.safe_token().is_some(),
                ];
                assert_eq!(found, accessors, "{it}");
                or(&mut seen_static, &found);
            } else if let Some(it) = ast::Impl::cast(node.clone()) {
                let found =
                    child_tokens(it.syntax(), [T![unsafe], T![!], T![default], T![const]]);
                let accessors = [
                    it.unsafe_token().is_some(),
                    it.excl_token().is_some(),
                    it.default_token().is_some(),
                    it.const_token().is_some(),
                ];
                assert_eq!(found, accessors, "{it}");
                or(&mut seen_impl, &found);
            } else if let Some(it) = ast::Trait::cast(node.clone()) {
                let found = child_tokens(it.syntax(), [T![auto], T![unsafe], T![=], T![const]]);
                let accessors = [
                    it.auto_token().is_some(),
                    it.unsafe_token().is_some(),
                    it.eq_token().is_some(),
                    it.const_token().is_some(),
                ];
                assert_eq!(found, accessors, "{it}");
                or(&mut seen_trait, &found[..3]);
            } else if let Some(it) = ast::RecordField::cast(node.clone()) {
                let [found] = child_tokens(it.syntax(), [T![unsafe]]);
                let scanned = it
                    .syntax()
                    .children_with_tokens()
                    .filter_map(NodeOrToken::into_token)
                    .any(|token| token.kind() == T![unsafe]);
                assert_eq!(found, scanned, "{it}");
                seen_unsafe_field |= found;
            } else if let Some(it) = ast::TupleField::cast(node) {
                let [found] = child_tokens(it.syntax(), [T![unsafe]]);
                assert!(!found, "{it}");
            }
        }
        // `gen` is left out: whether `async gen fn` parses is the parser's.
        assert_eq!([seen_fn[0], seen_fn[1], seen_fn[3], seen_fn[4], seen_fn[5]], [true; 5]);
        assert_eq!(seen_static, [true; 3]);
        assert_eq!(seen_impl, [true; 4]);
        assert_eq!(seen_trait, [true; 3]);
        assert!(seen_unsafe_field);
    }

    #[test]
    fn variant_bits_as_bools() {
        let mut state = 0x7661_7269_616e_7473;
        for len in [0, 1, 2, 63, 64, 65, 127, 128, 129, 200, 1000] {
            // Only the first `kept` are ever set, as only the variants kept get a bit.
            let kept = if len == 0 { 0 } else { (splitmix(&mut state) as usize) % (len + 1) };
            let mut model = vec![false; kept];
            let mut bits = VariantBits::with_len(len);
            for _ in 0..3 * len {
                if kept == 0 {
                    break;
                }
                let index = (splitmix(&mut state) as usize) % kept;
                let value = splitmix(&mut state) & 1 == 1;
                model[index] = value;
                bits.set(index, value);
                assert_eq!(bits.get(index), value);
            }
            for (index, &value) in model.iter().enumerate() {
                assert_eq!(bits.get(index), value, "len {len} index {index}");
            }
            for index in kept..len {
                assert!(!bits.get(index), "len {len} index {index}");
            }

            bits.truncate(kept);
            let mut fresh = VariantBits::with_len(kept);
            for (index, &value) in model.iter().enumerate() {
                fresh.set(index, value);
            }
            assert_eq!(bits, fresh, "len {len} kept {kept}");
            for (index, &value) in model.iter().enumerate() {
                assert_eq!(bits.get(index), value, "len {len} kept {kept} index {index}");
            }
        }
        assert!(!VariantBits::with_len(64).0.spilled());
        assert!(VariantBits::with_len(65).0.spilled());
    }
}

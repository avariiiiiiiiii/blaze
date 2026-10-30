//! rustc's intrinsics as one 2-byte enum, [`Intrinsic`]: every `#[rustc_intrinsic]` function of
//! the pinned nightly's library (`core::intrinsics`, its `simd`, `simd::scalable`, `gpu` and
//! `reflection` modules, and `alloc::intrinsics`).
//!
//! A callee is recognized once, when MIR lowering lowers the call
//! (`TerminatorKind::Call::intrinsic`), from its pre-interned symbol: a pointer subtraction and
//! one load in a [`KnownNames`] table, with no string compared. The interpreter and the back
//! ends then `match` on the enum. A `#[rustc_intrinsic]` whose name is none of rustc's is an
//! error naming it ([`UnknownIntrinsic`]), as it is for rustc (E0093).
//!
//! The atomic and SIMD families carry their operation, the float intrinsics named by width
//! carry it. The pinned nightly passes an atomic's orderings as const generic arguments
//! (`atomic_load::<T, ORD>`), so no name carries one: they are read from the call's generic
//! arguments where they matter.

use std::fmt;

use hir_def::{FunctionId, signatures::FunctionSignature};
use intern::{KnownNames, Symbol, sym, sym::Declared};

use crate::db::HirDatabase;

/// rustc's intrinsic, its family's operation inside: 2 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Intrinsic {
    /// `atomic_*`.
    Atomic(AtomicOp),
    /// `simd_*`.
    Simd(SimdOp),
    /// `copysignf16` to `copysignf128`.
    CopySign(FloatWidth),
    /// `maximumf16` to `maximumf128`.
    Maximum(FloatWidth),
    /// `minimumf16` to `minimumf128`.
    Minimum(FloatWidth),
    /// `maximum_number_nsz_f16` to `maximum_number_nsz_f128`.
    MaximumNumberNsz(FloatWidth),
    /// `minimum_number_nsz_f16` to `minimum_number_nsz_f128`.
    MinimumNumberNsz(FloatWidth),
    AbortImmediate, AddWithOverflow, AggregateRawPtr, AlignOf, AlignOfVal, AmdgpuDispatchPtr,
    ArithOffset, AssertInhabited, AssertMemUninitializedValid, AssertZeroValid, Assume, Autodiff,
    Bitreverse, BlackBox, Breakpoint, Bswap, CallerLocation, CarryingMulAdd, CarrylessMul,
    CatchUnwind, Ceil, CodeviewAnnotation, ColdPath, CompareBytes, ConstAllocate, ConstDeallocate,
    ConstEvalSelect, ConstMakeGlobal, ContractCheckEnsures, ContractCheckRequires, Copy,
    CopyNonoverlapping, Cos, Ctlz, CtlzNonzero, Ctpop, Cttz, CttzNonzero, DiscriminantValue,
    DisjointBitor, ExactDiv, Exp, Exp2, Fabs, FaddAlgebraic, FaddFast, FdivAlgebraic, FdivFast,
    FieldOffset, FieldRepresentingTypeActualTypeId, FieldRepresentingTypeName,
    FieldRepresentingTypeOffset, FloatToIntUnchecked, Floor, Fma, FmulAlgebraic, FmulFast, Fmuladd,
    Forget, FremAlgebraic, FremFast, FsubAlgebraic, FsubFast, GpuLaunchSizedWorkgroupMem,
    IntegerMax, IntegerMin, IsValStaticallyKnown, Log, Log10, Log2, MulWithOverflow, NeedsDrop,
    NontemporalStore, Offload, OffloadGetNumDevices, Offset, OffsetOf, OverflowChecks, Powf, Powi,
    PrefetchReadData, PrefetchReadInstruction, PrefetchWriteData, PrefetchWriteInstruction,
    PtrGuaranteedCmp, PtrMask, PtrMetadata, PtrOffsetFrom, PtrOffsetFromUnsigned, RawEq,
    ReadViaCopy, ReturnAddress, RotateLeft, RotateRight, Round, RoundTiesEven, RustcPeek,
    SaturatingAdd, SaturatingSub, SelectUnpredictable, Sin, SizeOf, SizeOfVal, SliceGetUnchecked,
    Sqrt, SubWithOverflow, SveTupleCreate2, SveTupleCreate3, SveTupleCreate4, SveTupleGet,
    SveTupleSet, ThreeWayCompare, Transmute, TransmuteUnchecked, Trunc, TypeId, TypeIdArrayLen,
    TypeIdElementTy, TypeIdEq, TypeIdFieldRepresentingType, TypeIdFields, TypeIdFunctionPtr,
    TypeIdGenerics, TypeIdIsSigned, TypeIdNonExhaustive, TypeIdPointsMutably, TypeIdPointsTo,
    TypeIdSizeOf, TypeIdTypeOf, TypeIdVariantName, TypeIdVariantNonExhaustive, TypeIdVariants,
    TypeIdVtable, TypeName, TypedSwapNonoverlapping, UbChecks, UnalignedVolatileLoad,
    UnalignedVolatileStore, UncheckedAdd, UncheckedDiv, UncheckedFunnelShl, UncheckedFunnelShr,
    UncheckedMul, UncheckedRem, UncheckedShl, UncheckedShr, UncheckedSub, Unreachable, VaArg,
    VaCopy, VaEnd, VariantCount, VolatileCopyMemory, VolatileCopyNonoverlappingMemory,
    VolatileLoad, VolatileSetMemory, VolatileStore, VtableAlign, VtableSize, WrappingAdd,
    WrappingMul, WrappingSub, WriteBoxViaMove, WriteBytes, WriteViaMove,
}

const _: () = assert!(size_of::<Intrinsic>() == 2);
const _: () = assert!(size_of::<Option<Intrinsic>>() == 2);

/// The operation of an `atomic_*` intrinsic, named as after the prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum AtomicOp {
    And, Cxchg, CxchgWeak, Fence, Load, Max, Min, Nand, Or, SingleThreadFence, Store, Umax, Umin,
    Xadd, Xchg, Xor, Xsub,
}

/// The operation of a `simd_*` intrinsic, named as after the prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum SimdOp {
    Add, And, ArithOffset, As, Bitmask, Bitreverse, Bswap, CarrylessMul, Cast, CastPtr, Ceil, Ctlz,
    Ctpop, Cttz, Div, Eq, ExposeProvenance, Extract, ExtractDyn, Fabs, Fcos, Fexp, Fexp2, Flog,
    Flog10, Flog2, Floor, Fma, Fsin, Fsqrt, FunnelShl, FunnelShr, Gather, Ge, Gt, Insert,
    InsertDyn, Le, Lt, MaskedLoad, MaskedStore, MaximumNumberNsz, MinimumNumberNsz, Mul, Ne, Neg,
    Or, ReduceAddOrdered, ReduceAddUnordered, ReduceAll, ReduceAnd, ReduceAny, ReduceMax,
    ReduceMin, ReduceMulOrdered, ReduceMulUnordered, ReduceOr, ReduceXor, RelaxedFma, Rem, Round,
    RoundTiesEven, SaturatingAdd, SaturatingSub, Scatter, Select, SelectBitmask, Shl, Shr, Shuffle,
    Splat, Sub, Trunc, WithExposedProvenance, Xor,
}

/// The float type a float intrinsic's name ends in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum FloatWidth {
    F16,
    F32,
    F64,
    F128,
}

impl FloatWidth {
    pub const fn bits(self) -> u32 {
        match self {
            FloatWidth::F16 => 16,
            FloatWidth::F32 => 32,
            FloatWidth::F64 => 64,
            FloatWidth::F128 => 128,
        }
    }
}

/// A `#[rustc_intrinsic]` function whose name is none of rustc's intrinsics.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UnknownIntrinsic(pub Symbol);

impl fmt::Display for UnknownIntrinsic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "`{}` is declared `#[rustc_intrinsic]`, but the pinned nightly's rustc has no \
             intrinsic by that name",
            self.0
        )
    }
}

/// The intrinsic `func` is: `None` for a function that is not `#[rustc_intrinsic]`, an error
/// for one whose name is none of rustc's.
pub fn intrinsic_of(
    db: &dyn HirDatabase,
    func: FunctionId,
) -> Result<Option<Intrinsic>, UnknownIntrinsic> {
    if !FunctionSignature::is_intrinsic(db, func) {
        return Ok(None);
    }
    let name = FunctionSignature::of(db, func).name.symbol();
    match Intrinsic::from_symbol(name) {
        Some(intrinsic) => Ok(Some(intrinsic)),
        None => Err(UnknownIntrinsic(name.clone())),
    }
}

impl Intrinsic {
    /// How many intrinsics rustc has.
    pub const COUNT: usize = BY_INDEX.len();

    /// The intrinsic named `name`, if rustc has one: a pointer subtraction and one load.
    #[inline]
    pub fn from_symbol(name: &Symbol) -> Option<Intrinsic> {
        KNOWN.get(name).map(|index| BY_INDEX[usize::from(index)])
    }

    /// The intrinsic at `index` in rustc's names in byte order.
    #[inline]
    pub fn from_index(index: u16) -> Option<Intrinsic> {
        BY_INDEX.get(usize::from(index)).copied()
    }

    /// The name's rank among rustc's intrinsics in byte order: dense, below [`Self::COUNT`], so a
    /// set of intrinsics is a bitset.
    pub const fn index(self) -> u16 {
        self.entry().0
    }

    /// The intrinsic's name.
    pub const fn symbol(self) -> Symbol {
        self.entry().1
    }
}

impl fmt::Display for Intrinsic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.symbol().as_str())
    }
}

macro_rules! intrinsics {
    ($($index:literal $name:ident => $variant:ident $(($($data:tt)*))?,)+) => {
        /// The intrinsics in rustc's names' byte order: their [`Intrinsic::index`].
        const MEMBERS: &[Declared] = &[$(Declared::$name,)+];

        const BY_INDEX: [Intrinsic; MEMBERS.len()] = [$(Intrinsic::$variant $(($($data)*))?,)+];

        impl Intrinsic {
            const fn entry(self) -> (u16, Symbol) {
                match self {
                    $(Intrinsic::$variant $(($($data)*))? => ($index, sym::$name),)+
                }
            }
        }
    };
}

/// [`MEMBERS`] by declared symbol: 2 bytes a declared symbol, one load a lookup.
static KNOWN: KnownNames = KnownNames::new(MEMBERS);

intrinsics! {
    0 abort_immediate => AbortImmediate,
    1 add_with_overflow => AddWithOverflow,
    2 aggregate_raw_ptr => AggregateRawPtr,
    3 align_of => AlignOf,
    4 align_of_val => AlignOfVal,
    5 amdgpu_dispatch_ptr => AmdgpuDispatchPtr,
    6 arith_offset => ArithOffset,
    7 assert_inhabited => AssertInhabited,
    8 assert_mem_uninitialized_valid => AssertMemUninitializedValid,
    9 assert_zero_valid => AssertZeroValid,
    10 assume => Assume,
    11 atomic_and => Atomic(AtomicOp::And),
    12 atomic_cxchg => Atomic(AtomicOp::Cxchg),
    13 atomic_cxchgweak => Atomic(AtomicOp::CxchgWeak),
    14 atomic_fence => Atomic(AtomicOp::Fence),
    15 atomic_load => Atomic(AtomicOp::Load),
    16 atomic_max => Atomic(AtomicOp::Max),
    17 atomic_min => Atomic(AtomicOp::Min),
    18 atomic_nand => Atomic(AtomicOp::Nand),
    19 atomic_or => Atomic(AtomicOp::Or),
    20 atomic_singlethreadfence => Atomic(AtomicOp::SingleThreadFence),
    21 atomic_store => Atomic(AtomicOp::Store),
    22 atomic_umax => Atomic(AtomicOp::Umax),
    23 atomic_umin => Atomic(AtomicOp::Umin),
    24 atomic_xadd => Atomic(AtomicOp::Xadd),
    25 atomic_xchg => Atomic(AtomicOp::Xchg),
    26 atomic_xor => Atomic(AtomicOp::Xor),
    27 atomic_xsub => Atomic(AtomicOp::Xsub),
    28 autodiff => Autodiff,
    29 bitreverse => Bitreverse,
    30 black_box => BlackBox,
    31 breakpoint => Breakpoint,
    32 bswap => Bswap,
    33 caller_location => CallerLocation,
    34 carrying_mul_add => CarryingMulAdd,
    35 carryless_mul => CarrylessMul,
    36 catch_unwind => CatchUnwind,
    37 ceil => Ceil,
    38 codeview_annotation => CodeviewAnnotation,
    39 cold_path => ColdPath,
    40 compare_bytes => CompareBytes,
    41 const_allocate => ConstAllocate,
    42 const_deallocate => ConstDeallocate,
    43 const_eval_select => ConstEvalSelect,
    44 const_make_global => ConstMakeGlobal,
    45 contract_check_ensures => ContractCheckEnsures,
    46 contract_check_requires => ContractCheckRequires,
    47 copy => Copy,
    48 copy_nonoverlapping => CopyNonoverlapping,
    49 copysignf128 => CopySign(FloatWidth::F128),
    50 copysignf16 => CopySign(FloatWidth::F16),
    51 copysignf32 => CopySign(FloatWidth::F32),
    52 copysignf64 => CopySign(FloatWidth::F64),
    53 cos => Cos,
    54 ctlz => Ctlz,
    55 ctlz_nonzero => CtlzNonzero,
    56 ctpop => Ctpop,
    57 cttz => Cttz,
    58 cttz_nonzero => CttzNonzero,
    59 discriminant_value => DiscriminantValue,
    60 disjoint_bitor => DisjointBitor,
    61 exact_div => ExactDiv,
    62 exp => Exp,
    63 exp2 => Exp2,
    64 fabs => Fabs,
    65 fadd_algebraic => FaddAlgebraic,
    66 fadd_fast => FaddFast,
    67 fdiv_algebraic => FdivAlgebraic,
    68 fdiv_fast => FdivFast,
    69 field_offset => FieldOffset,
    70 field_representing_type_actual_type_id => FieldRepresentingTypeActualTypeId,
    71 field_representing_type_name => FieldRepresentingTypeName,
    72 field_representing_type_offset => FieldRepresentingTypeOffset,
    73 float_to_int_unchecked => FloatToIntUnchecked,
    74 floor => Floor,
    75 fma => Fma,
    76 fmul_algebraic => FmulAlgebraic,
    77 fmul_fast => FmulFast,
    78 fmuladd => Fmuladd,
    79 forget => Forget,
    80 frem_algebraic => FremAlgebraic,
    81 frem_fast => FremFast,
    82 fsub_algebraic => FsubAlgebraic,
    83 fsub_fast => FsubFast,
    84 gpu_launch_sized_workgroup_mem => GpuLaunchSizedWorkgroupMem,
    85 integer_max => IntegerMax,
    86 integer_min => IntegerMin,
    87 is_val_statically_known => IsValStaticallyKnown,
    88 log => Log,
    89 log10 => Log10,
    90 log2 => Log2,
    91 maximum_number_nsz_f128 => MaximumNumberNsz(FloatWidth::F128),
    92 maximum_number_nsz_f16 => MaximumNumberNsz(FloatWidth::F16),
    93 maximum_number_nsz_f32 => MaximumNumberNsz(FloatWidth::F32),
    94 maximum_number_nsz_f64 => MaximumNumberNsz(FloatWidth::F64),
    95 maximumf128 => Maximum(FloatWidth::F128),
    96 maximumf16 => Maximum(FloatWidth::F16),
    97 maximumf32 => Maximum(FloatWidth::F32),
    98 maximumf64 => Maximum(FloatWidth::F64),
    99 minimum_number_nsz_f128 => MinimumNumberNsz(FloatWidth::F128),
    100 minimum_number_nsz_f16 => MinimumNumberNsz(FloatWidth::F16),
    101 minimum_number_nsz_f32 => MinimumNumberNsz(FloatWidth::F32),
    102 minimum_number_nsz_f64 => MinimumNumberNsz(FloatWidth::F64),
    103 minimumf128 => Minimum(FloatWidth::F128),
    104 minimumf16 => Minimum(FloatWidth::F16),
    105 minimumf32 => Minimum(FloatWidth::F32),
    106 minimumf64 => Minimum(FloatWidth::F64),
    107 mul_with_overflow => MulWithOverflow,
    108 needs_drop => NeedsDrop,
    109 nontemporal_store => NontemporalStore,
    110 offload => Offload,
    111 offload_get_num_devices => OffloadGetNumDevices,
    112 offset => Offset,
    113 offset_of => OffsetOf,
    114 overflow_checks => OverflowChecks,
    115 powf => Powf,
    116 powi => Powi,
    117 prefetch_read_data => PrefetchReadData,
    118 prefetch_read_instruction => PrefetchReadInstruction,
    119 prefetch_write_data => PrefetchWriteData,
    120 prefetch_write_instruction => PrefetchWriteInstruction,
    121 ptr_guaranteed_cmp => PtrGuaranteedCmp,
    122 ptr_mask => PtrMask,
    123 ptr_metadata => PtrMetadata,
    124 ptr_offset_from => PtrOffsetFrom,
    125 ptr_offset_from_unsigned => PtrOffsetFromUnsigned,
    126 raw_eq => RawEq,
    127 read_via_copy => ReadViaCopy,
    128 return_address => ReturnAddress,
    129 rotate_left => RotateLeft,
    130 rotate_right => RotateRight,
    131 round => Round,
    132 round_ties_even => RoundTiesEven,
    133 rustc_peek => RustcPeek,
    134 saturating_add => SaturatingAdd,
    135 saturating_sub => SaturatingSub,
    136 select_unpredictable => SelectUnpredictable,
    137 simd_add => Simd(SimdOp::Add),
    138 simd_and => Simd(SimdOp::And),
    139 simd_arith_offset => Simd(SimdOp::ArithOffset),
    140 simd_as => Simd(SimdOp::As),
    141 simd_bitmask => Simd(SimdOp::Bitmask),
    142 simd_bitreverse => Simd(SimdOp::Bitreverse),
    143 simd_bswap => Simd(SimdOp::Bswap),
    144 simd_carryless_mul => Simd(SimdOp::CarrylessMul),
    145 simd_cast => Simd(SimdOp::Cast),
    146 simd_cast_ptr => Simd(SimdOp::CastPtr),
    147 simd_ceil => Simd(SimdOp::Ceil),
    148 simd_ctlz => Simd(SimdOp::Ctlz),
    149 simd_ctpop => Simd(SimdOp::Ctpop),
    150 simd_cttz => Simd(SimdOp::Cttz),
    151 simd_div => Simd(SimdOp::Div),
    152 simd_eq => Simd(SimdOp::Eq),
    153 simd_expose_provenance => Simd(SimdOp::ExposeProvenance),
    154 simd_extract => Simd(SimdOp::Extract),
    155 simd_extract_dyn => Simd(SimdOp::ExtractDyn),
    156 simd_fabs => Simd(SimdOp::Fabs),
    157 simd_fcos => Simd(SimdOp::Fcos),
    158 simd_fexp => Simd(SimdOp::Fexp),
    159 simd_fexp2 => Simd(SimdOp::Fexp2),
    160 simd_flog => Simd(SimdOp::Flog),
    161 simd_flog10 => Simd(SimdOp::Flog10),
    162 simd_flog2 => Simd(SimdOp::Flog2),
    163 simd_floor => Simd(SimdOp::Floor),
    164 simd_fma => Simd(SimdOp::Fma),
    165 simd_fsin => Simd(SimdOp::Fsin),
    166 simd_fsqrt => Simd(SimdOp::Fsqrt),
    167 simd_funnel_shl => Simd(SimdOp::FunnelShl),
    168 simd_funnel_shr => Simd(SimdOp::FunnelShr),
    169 simd_gather => Simd(SimdOp::Gather),
    170 simd_ge => Simd(SimdOp::Ge),
    171 simd_gt => Simd(SimdOp::Gt),
    172 simd_insert => Simd(SimdOp::Insert),
    173 simd_insert_dyn => Simd(SimdOp::InsertDyn),
    174 simd_le => Simd(SimdOp::Le),
    175 simd_lt => Simd(SimdOp::Lt),
    176 simd_masked_load => Simd(SimdOp::MaskedLoad),
    177 simd_masked_store => Simd(SimdOp::MaskedStore),
    178 simd_maximum_number_nsz => Simd(SimdOp::MaximumNumberNsz),
    179 simd_minimum_number_nsz => Simd(SimdOp::MinimumNumberNsz),
    180 simd_mul => Simd(SimdOp::Mul),
    181 simd_ne => Simd(SimdOp::Ne),
    182 simd_neg => Simd(SimdOp::Neg),
    183 simd_or => Simd(SimdOp::Or),
    184 simd_reduce_add_ordered => Simd(SimdOp::ReduceAddOrdered),
    185 simd_reduce_add_unordered => Simd(SimdOp::ReduceAddUnordered),
    186 simd_reduce_all => Simd(SimdOp::ReduceAll),
    187 simd_reduce_and => Simd(SimdOp::ReduceAnd),
    188 simd_reduce_any => Simd(SimdOp::ReduceAny),
    189 simd_reduce_max => Simd(SimdOp::ReduceMax),
    190 simd_reduce_min => Simd(SimdOp::ReduceMin),
    191 simd_reduce_mul_ordered => Simd(SimdOp::ReduceMulOrdered),
    192 simd_reduce_mul_unordered => Simd(SimdOp::ReduceMulUnordered),
    193 simd_reduce_or => Simd(SimdOp::ReduceOr),
    194 simd_reduce_xor => Simd(SimdOp::ReduceXor),
    195 simd_relaxed_fma => Simd(SimdOp::RelaxedFma),
    196 simd_rem => Simd(SimdOp::Rem),
    197 simd_round => Simd(SimdOp::Round),
    198 simd_round_ties_even => Simd(SimdOp::RoundTiesEven),
    199 simd_saturating_add => Simd(SimdOp::SaturatingAdd),
    200 simd_saturating_sub => Simd(SimdOp::SaturatingSub),
    201 simd_scatter => Simd(SimdOp::Scatter),
    202 simd_select => Simd(SimdOp::Select),
    203 simd_select_bitmask => Simd(SimdOp::SelectBitmask),
    204 simd_shl => Simd(SimdOp::Shl),
    205 simd_shr => Simd(SimdOp::Shr),
    206 simd_shuffle => Simd(SimdOp::Shuffle),
    207 simd_splat => Simd(SimdOp::Splat),
    208 simd_sub => Simd(SimdOp::Sub),
    209 simd_trunc => Simd(SimdOp::Trunc),
    210 simd_with_exposed_provenance => Simd(SimdOp::WithExposedProvenance),
    211 simd_xor => Simd(SimdOp::Xor),
    212 sin => Sin,
    213 size_of => SizeOf,
    214 size_of_val => SizeOfVal,
    215 slice_get_unchecked => SliceGetUnchecked,
    216 sqrt => Sqrt,
    217 sub_with_overflow => SubWithOverflow,
    218 sve_tuple_create2 => SveTupleCreate2,
    219 sve_tuple_create3 => SveTupleCreate3,
    220 sve_tuple_create4 => SveTupleCreate4,
    221 sve_tuple_get => SveTupleGet,
    222 sve_tuple_set => SveTupleSet,
    223 three_way_compare => ThreeWayCompare,
    224 transmute => Transmute,
    225 transmute_unchecked => TransmuteUnchecked,
    226 trunc => Trunc,
    227 type_id => TypeId,
    228 type_id_array_len => TypeIdArrayLen,
    229 type_id_element_ty => TypeIdElementTy,
    230 type_id_eq => TypeIdEq,
    231 type_id_field_representing_type => TypeIdFieldRepresentingType,
    232 type_id_fields => TypeIdFields,
    233 type_id_function_ptr => TypeIdFunctionPtr,
    234 type_id_generics => TypeIdGenerics,
    235 type_id_is_signed => TypeIdIsSigned,
    236 type_id_non_exhaustive => TypeIdNonExhaustive,
    237 type_id_points_mutably => TypeIdPointsMutably,
    238 type_id_points_to => TypeIdPointsTo,
    239 type_id_size_of => TypeIdSizeOf,
    240 type_id_type_of => TypeIdTypeOf,
    241 type_id_variant_name => TypeIdVariantName,
    242 type_id_variant_non_exhaustive => TypeIdVariantNonExhaustive,
    243 type_id_variants => TypeIdVariants,
    244 type_id_vtable => TypeIdVtable,
    245 type_name => TypeName,
    246 typed_swap_nonoverlapping => TypedSwapNonoverlapping,
    247 ub_checks => UbChecks,
    248 unaligned_volatile_load => UnalignedVolatileLoad,
    249 unaligned_volatile_store => UnalignedVolatileStore,
    250 unchecked_add => UncheckedAdd,
    251 unchecked_div => UncheckedDiv,
    252 unchecked_funnel_shl => UncheckedFunnelShl,
    253 unchecked_funnel_shr => UncheckedFunnelShr,
    254 unchecked_mul => UncheckedMul,
    255 unchecked_rem => UncheckedRem,
    256 unchecked_shl => UncheckedShl,
    257 unchecked_shr => UncheckedShr,
    258 unchecked_sub => UncheckedSub,
    259 unreachable => Unreachable,
    260 va_arg => VaArg,
    261 va_copy => VaCopy,
    262 va_end => VaEnd,
    263 variant_count => VariantCount,
    264 volatile_copy_memory => VolatileCopyMemory,
    265 volatile_copy_nonoverlapping_memory => VolatileCopyNonoverlappingMemory,
    266 volatile_load => VolatileLoad,
    267 volatile_set_memory => VolatileSetMemory,
    268 volatile_store => VolatileStore,
    269 vtable_align => VtableAlign,
    270 vtable_size => VtableSize,
    271 wrapping_add => WrappingAdd,
    272 wrapping_mul => WrappingMul,
    273 wrapping_sub => WrappingSub,
    274 write_box_via_move => WriteBoxViaMove,
    275 write_bytes => WriteBytes,
    276 write_via_move => WriteViaMove,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_name_is_its_own_intrinsic() {
        assert_eq!(Intrinsic::COUNT, 277);
        let mut seen = [false; Intrinsic::COUNT];
        for (at, &intrinsic) in BY_INDEX.iter().enumerate() {
            assert_eq!(usize::from(intrinsic.index()), at, "{intrinsic}");
            assert!(!std::mem::replace(&mut seen[at], true));
            let name = intrinsic.symbol();
            assert_eq!(Intrinsic::from_symbol(&name), Some(intrinsic));
            assert_eq!(Intrinsic::from_symbol(&Symbol::intern(name.as_str())), Some(intrinsic));
            assert_eq!(Intrinsic::from_index(intrinsic.index()), Some(intrinsic));
        }
        assert_eq!(Intrinsic::from_index(Intrinsic::COUNT as u16), None);
    }

    #[test]
    fn names_are_in_byte_order() {
        for pair in BY_INDEX.windows(2) {
            let (a, b) = (pair[0].symbol(), pair[1].symbol());
            assert!(a.as_str().as_bytes() < b.as_str().as_bytes(), "{a} before {b}");
        }
    }

    #[test]
    fn families_carry_their_operation() {
        let of = |name: &str| Intrinsic::from_symbol(&Symbol::intern(name));
        assert_eq!(of("atomic_load"), Some(Intrinsic::Atomic(AtomicOp::Load)));
        assert_eq!(of("atomic_cxchgweak"), Some(Intrinsic::Atomic(AtomicOp::CxchgWeak)));
        assert_eq!(
            of("atomic_singlethreadfence"),
            Some(Intrinsic::Atomic(AtomicOp::SingleThreadFence))
        );
        assert_eq!(of("simd_add"), Some(Intrinsic::Simd(SimdOp::Add)));
        assert_eq!(of("simd_reduce_add_ordered"), Some(Intrinsic::Simd(SimdOp::ReduceAddOrdered)));
        assert_eq!(of("simd_shuffle"), Some(Intrinsic::Simd(SimdOp::Shuffle)));
        assert_eq!(of("copysignf128"), Some(Intrinsic::CopySign(FloatWidth::F128)));
        assert_eq!(of("minimumf16"), Some(Intrinsic::Minimum(FloatWidth::F16)));
        assert_eq!(
            of("maximum_number_nsz_f64"),
            Some(Intrinsic::MaximumNumberNsz(FloatWidth::F64))
        );
        assert_eq!(of("sqrt"), Some(Intrinsic::Sqrt));
        assert_eq!(of("transmute"), Some(Intrinsic::Transmute));
        assert_eq!(of("write_box_via_move"), Some(Intrinsic::WriteBoxViaMove));
        assert_eq!(of("type_id_vtable"), Some(Intrinsic::TypeIdVtable));
        assert_eq!(Intrinsic::Simd(SimdOp::Eq).to_string(), "simd_eq");
        assert_eq!(Intrinsic::Atomic(AtomicOp::Xsub).to_string(), "atomic_xsub");
        assert_eq!(FloatWidth::F128.bits(), 128);
    }

    #[test]
    fn names_shared_with_other_symbols_are_intrinsics() {
        assert_eq!(Intrinsic::from_symbol(&sym::copy), Some(Intrinsic::Copy));
        assert_eq!(Intrinsic::from_symbol(&sym::type_id), Some(Intrinsic::TypeId));
        assert_eq!(Intrinsic::from_symbol(&sym::unreachable), Some(Intrinsic::Unreachable));
        assert_eq!(Intrinsic::from_symbol(&sym::overflow_checks), Some(Intrinsic::OverflowChecks));
    }

    #[test]
    fn names_rustc_does_not_have_are_none() {
        // Names of older nightlies, and declared symbols that are not intrinsics.
        for name in [
            "sqrtf32",
            "atomic_load_seqcst",
            "atomic_fence_acqrel",
            "box_new",
            "likely",
            "unlikely",
            "abort",
            "simd_fmin",
            "min_align_of",
            "simd_",
            "atomic_",
            "",
            "self",
            "Copy",
        ] {
            assert_eq!(Intrinsic::from_symbol(&Symbol::intern(name)), None, "{name}");
        }
        assert_eq!(Intrinsic::from_symbol(&sym::self_), None);
        assert_eq!(Intrinsic::from_symbol(&sym::Integer::get(1)), None);
    }

    #[test]
    fn unknown_intrinsics_name_themselves() {
        let message = UnknownIntrinsic(Symbol::intern("sqrtf32")).to_string();
        assert!(message.starts_with("`sqrtf32` is declared `#[rustc_intrinsic]`"), "{message}");
    }
}

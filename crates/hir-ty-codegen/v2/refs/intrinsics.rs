//! What the IR builder's lowerings of intrinsics, `llvm.*` functions, operators and float
//! conversions ask for besides their operands, decided as they decide it but without building
//! IR: compiler-builtins' and the C library's functions, the routines Cranelift calls where
//! the baseline x86-64 it compiles for has no instruction, and the functions of assembly the
//! lowerings call, by their text.
//!
//! The lowerings that build IR are mirrored here on the walk's own types: `FunctionCx::operand_ty`
//! of a projected place builds the place, so `immediate`, `splat_immediate` and `half_lanes` are
//! too. Where a lowering decides by the code around the call (the fused multiply-adds of
//! `fma.rs`), every form it may take is listed.

use cranelift_codegen::ir::{self, types};
use hir_def::{
    AdtId, FunctionId, HasModule, VariantId,
    attrs::AttrFlags,
    layout::{BackendRepr, Primitive, Variants},
    signatures::FunctionSignature,
};
use hir_ty::{
    consteval::try_const_usize,
    layout::Layout,
    next_solver::{ConstKind, GenericArgs, Ty, TyKind},
};
use hir_ty_mir::mir::{AggregateKind, BinOp, CastKind, MirBody, Operand, OperandKind, Rvalue, UnOp};
use rustc_abi::Size;
use rustc_hash::FxHashSet;
use rustc_type_ir::inherent::{GenericArgs as _, IntoKind, SliceLike as _};
use smallvec::SmallVec;

use super::{Walk, told};
use crate::{
    AbiScalar, Arch, CodegenError, VariableArguments, asm, avx512, f16_f128, neon, sve, x86,
};

/// The generic intrinsics `wasm_simd_intrinsic` may make wasm's vector instructions of, which
/// ask for nothing but their operands.
const WASM_SIMD: &[&str] = &[
    "simd_add",
    "simd_sub",
    "simd_mul",
    "simd_div",
    "simd_and",
    "simd_or",
    "simd_xor",
    "simd_eq",
    "simd_ne",
    "simd_lt",
    "simd_le",
    "simd_gt",
    "simd_ge",
    "simd_saturating_add",
    "simd_saturating_sub",
    "simd_shl",
    "simd_shr",
    "simd_neg",
    "simd_fabs",
    "simd_fsqrt",
    "simd_floor",
    "simd_ceil",
    "simd_trunc",
    "simd_round_ties_even",
    "simd_ctpop",
    "simd_select",
    "simd_splat",
    "simd_shuffle",
    "simd_cast",
    "simd_as",
    "simd_bitmask",
];

/// An operation of a float Cranelift's x86-64 backend calls the C library for: the baseline it
/// compiles for has neither SSE4.1's rounding nor FMA.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Routine {
    Floor,
    Ceil,
    Trunc,
    Nearest,
    Fma,
}

fn routine(op: Routine, ty: ir::Type) -> ir::LibCall {
    use ir::LibCall as L;
    match (op, ty == types::F32) {
        (Routine::Floor, true) => L::FloorF32,
        (Routine::Floor, false) => L::FloorF64,
        (Routine::Ceil, true) => L::CeilF32,
        (Routine::Ceil, false) => L::CeilF64,
        (Routine::Trunc, true) => L::TruncF32,
        (Routine::Trunc, false) => L::TruncF64,
        (Routine::Nearest, true) => L::NearestF32,
        (Routine::Nearest, false) => L::NearestF64,
        (Routine::Fma, true) => L::FmaF32,
        (Routine::Fma, false) => L::FmaF64,
    }
}

/// The error the IR builder's `unsupported!` makes of `what`.
fn unsupported(what: impl Into<String>) -> String {
    told(CodegenError::Unsupported(what.into()))
}

/// compiler-builtins' quotient or remainder of two 128-bit integers (`FunctionCx::divide`).
fn divide_builtin(div: bool, signed: bool) -> &'static str {
    match (div, signed) {
        (true, true) => "__divti3",
        (true, false) => "__udivti3",
        (false, true) => "__modti3",
        (false, false) => "__umodti3",
    }
}

/// compiler-builtins' conversion between an `f32` or `f64` and a 128-bit integer
/// (`FunctionCx::float_cast`), `None` for a cast it has none for.
fn wide_int_float(from: AbiScalar, to: AbiScalar) -> Option<&'static str> {
    Some(match (from.ty, from.signed, to.ty, to.signed) {
        (types::F32, _, _, true) => "__fixsfti",
        (types::F32, _, _, false) => "__fixunssfti",
        (types::F64, _, _, true) => "__fixdfti",
        (types::F64, _, _, false) => "__fixunsdfti",
        (_, true, types::F32, _) => "__floattisf",
        (_, false, types::F32, _) => "__floatuntisf",
        (_, true, types::F64, _) => "__floattidf",
        (_, false, types::F64, _) => "__floatuntidf",
        _ => return None,
    })
}

/// compiler-builtins' `f128` of an integer of `bits` (`int_to_quad`).
fn quad_of_int(bits: u32, signed: bool) -> &'static str {
    match (bits, signed) {
        (..=32, true) => "__floatsitf",
        (..=32, false) => "__floatunsitf",
        (64, true) => "__floatditf",
        (64, false) => "__floatunditf",
        (_, true) => "__floattitf",
        (_, false) => "__floatuntitf",
    }
}

/// compiler-builtins' integer of `bits` of an `f128` (`quad_to_int`).
fn int_of_quad(bits: u32, signed: bool) -> &'static str {
    match (bits, signed) {
        (..=32, true) => "__fixtfsi",
        (..=32, false) => "__fixunstfsi",
        (64, true) => "__fixtfdi",
        (64, false) => "__fixunstfdi",
        (_, true) => "__fixtfti",
        (_, false) => "__fixunstfti",
    }
}

/// The letter `asm::fcvt` names a float of `ty` by.
fn letter(ty: ir::Type) -> char {
    match ty {
        types::F16 => 'h',
        types::F32 => 's',
        _ => 'd',
    }
}

/// Whether `neon_lanes` lowers `llvm.aarch64.neon.{name}` of `arity` operands lane by lane.
fn neon_pairwise(name: &str, arity: usize) -> bool {
    let mut parts = name.split('.');
    let op = parts.next().unwrap_or_default();
    let parts: Vec<&str> = parts.collect();
    let Some((lanes, ty)) = parts.last().and_then(|it| {
        let (lanes, bits) = it.strip_prefix('v')?.split_once('i')?;
        Some((lanes.parse::<i32>().ok()?, ir::Type::int(bits.parse().ok()?)?))
    }) else {
        return false;
    };
    let width = ty.bytes() as i32;
    match (op, arity) {
        ("addp", 2) if lanes * width == 16 && ty.bits() <= 32 => true,
        ("umaxp" | "smaxp" | "uminp" | "sminp" | "addp", 2) => true,
        ("sqdmulh", 2) => ty.bits() <= 32,
        ("uaddlp" | "saddlp", 1) => ty.bits() <= 32,
        ("uaddlv" | "saddlv" | "uaddv" | "saddv", 1) => parts
            .first()
            .and_then(|it| ir::Type::int(it.strip_prefix('i')?.parse().ok()?))
            .is_some(),
        ("sqdmull", 2) => ty.bits() >= 32,
        ("tbl1", 2) | ("tbx1", 3) => ty == types::I8,
        _ => false,
    }
}

/// The instructions `lower_fma` may make of an fma of `bytes` with the instruction suffix
/// `suffix`: each folded negation (`vfnmadd`, `vfmsub`, `vfnmsub`), and either factor first.
fn fma_texts(bytes: u32, suffix: &str) -> Vec<String> {
    let reg = |number: usize| match bytes {
        64 => format!("zmm{number}"),
        32 => format!("ymm{number}"),
        _ => format!("xmm{number}"),
    };
    let mut texts = Vec::with_capacity(8);
    for op in ["vfmadd", "vfnmadd", "vfmsub", "vfnmsub"] {
        for (first, second) in [(1, 2), (2, 1)] {
            texts.push(format!(
                "vmovaps {}, {}\n{op}213{suffix} {}, {}, {}",
                reg(0),
                reg(second),
                reg(0),
                reg(first),
                reg(3),
            ));
        }
    }
    texts
}

impl<'cx, 'db> Walk<'_, 'cx, 'db> {
    fn ty_layout(&self, ty: Ty<'db>) -> Result<&'cx Layout, String> {
        self.fx.layout(ty).map_err(told)
    }

    fn ty_scalar(&self, ty: Ty<'db>) -> Result<AbiScalar, String> {
        self.fx.scalar_of(ty).map_err(told)
    }

    fn simd_lanes_of(&self, ty: Ty<'db>) -> Result<(AbiScalar, u64), String> {
        self.fx.simd_lanes(ty).map_err(told)
    }

    fn on_x86(&self) -> bool {
        self.fx.cg.target.arch == Arch::X86_64
    }

    /// `check_bfloat_blocks`, and what `receive_bfloat` rounds an `f16b` parameter by on entry.
    pub(super) fn bfloat(&mut self, body: &'db MirBody<'db>) -> Result<(), String> {
        self.fx.check_bfloat_blocks(body).map_err(told)?;
        if !self.on_x86() {
            return Ok(());
        }
        for &local in &body.param_locals {
            // A parameter without a layout is refused where it is compiled, for that.
            let Ok(layout) = self.fx.layout(body.locals[local].ty.as_ref()) else { continue };
            if let BackendRepr::Scalar(scalar) = layout.backend_repr
                && scalar.primitive() == Primitive::Float(rustc_abi::Float::F16B)
            {
                return self.symbol("__truncsfbf2");
            }
        }
        Ok(())
    }

    /// `FunctionCx::neon_fused`: when the body is one of core's NEON fused multiply-adds, the
    /// function of its instruction, which is then the whole body.
    pub(super) fn neon_fused(&mut self) -> Result<bool, String> {
        let Some((name, lowered)) = self.fx.neon_fused().map_err(told)? else { return Ok(false) };
        let body = self.fx.body.expect("MIR body");
        let arg_tys: Vec<Ty<'db>> =
            body.param_locals.iter().map(|&local| body.locals[local].ty.as_ref()).collect();
        let dst_ty = body.locals[crate::returned_local(body)].ty.as_ref();
        let text = self.neon_text(&name, &lowered, &arg_tys, dst_ty)?;
        self.assembly(text)?;
        Ok(true)
    }

    /// `binary_op` and `overflowing_op`: a constant pattern's comparison part by part, a soft
    /// float's operator, `%` of floats, and the 128-bit products and quotients Cranelift has
    /// no instruction for.
    pub(super) fn binary_op(
        &mut self,
        op: &BinOp,
        lhs: &Operand,
        rhs: &Operand,
    ) -> Result<(), String> {
        let given = op;
        let (op, overflowing) = match op {
            BinOp::AddWithOverflow => (&BinOp::Add, true),
            BinOp::SubWithOverflow => (&BinOp::Sub, true),
            BinOp::MulWithOverflow => (&BinOp::Mul, true),
            op => (op, false),
        };
        let (mut ty, mut r_ty) = (self.operand_ty(lhs), self.operand_ty(rhs));
        while let TyKind::Ref(_, inner, _) = ty.kind() {
            ty = inner;
        }
        while let TyKind::Ref(_, inner, _) = r_ty.kind() {
            r_ty = inner;
        }
        if let BinOp::Eq | BinOp::Ne = op
            && matches!(
                ty.kind(),
                TyKind::Adt(..)
                    | TyKind::Tuple(_)
                    | TyKind::Array(..)
                    | TyKind::Slice(_)
                    | TyKind::Str
            )
        {
            return self.equal_parts(ty, &mut FxHashSet::default());
        }
        if matches!(ty.kind(), TyKind::RawPtr(..))
            && matches!(self.ty_layout(ty)?.backend_repr, BackendRepr::ScalarPair { .. })
        {
            return match op {
                BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => Ok(()),
                _ => Err(unsupported(format!("{op:?} on wide pointers"))),
            };
        }
        let scalar = self.ty_scalar(ty)?;
        self.ty_scalar(r_ty)?;
        let float = f16_f128::soft(scalar.ty) || scalar.ty.is_float();
        if f16_f128::soft(scalar.ty) {
            self.soft_binary(op, scalar.ty)?;
        } else if scalar.ty.is_float() {
            self.float_binary(op, scalar.ty)?;
        }
        if float {
            if overflowing {
                return Err(unsupported(format!("{given:?} of floats")));
            }
            return Ok(());
        }
        match op {
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => Ok(()),
            BinOp::Mul if scalar.ty == types::I128 => self.wide_mul(scalar.signed),
            BinOp::Div | BinOp::Rem => self.int_divide(scalar, matches!(op, BinOp::Div)),
            BinOp::Offset => Err(unsupported("pointer offset")),
            _ => Ok(()),
        }
    }

    /// `UnaryOp`: `!` and `-` are instructions, and `-` of a soft float (`soft_neg`) a flip of
    /// its sign bit.
    pub(super) fn unary_op(&mut self, op: &UnOp, operand: &Operand) -> Result<(), String> {
        let ty = self.operand_ty(operand);
        match op {
            UnOp::Not | UnOp::Neg => self.ty_scalar(ty).map(drop),
        }
    }

    /// `float_cast` of the scalars of `from` and `to`.
    pub(super) fn float_cast(
        &mut self,
        _kind: &CastKind,
        from: Ty<'db>,
        to: Ty<'db>,
    ) -> Result<(), String> {
        let (from, to) = (self.ty_scalar(from)?, self.ty_scalar(to)?);
        self.scalar_cast(from, to)
    }

    /// `FunctionCx::intrinsic`: true where it lowers the call (`Intrinsic::Inline` or
    /// `Diverges`), false where the function's body is called (`Unknown`).
    pub(super) fn intrinsic(
        &mut self,
        func: FunctionId,
        generic_args: GenericArgs<'db>,
        args: &[Operand],
        dst_ty: Ty<'db>,
    ) -> Result<bool, String> {
        let name = FunctionSignature::of(self.db(), func).name.as_str();
        if self.scalable_intrinsic(name, generic_args, args, dst_ty)? {
            return Ok(true);
        }
        // `wasm_simd_intrinsic` makes wasm's instructions of what it can, which ask for
        // nothing; the rest is lowered as on any target.
        let wasm = self.fx.cg.target.is_wasm() && self.fx.simd128() && WASM_SIMD.contains(&name);
        match self.lowered_intrinsic(name, generic_args, args, dst_ty) {
            Ok(inline) => Ok(inline || wasm),
            Err(_) if wasm => {
                self.operands(args)?;
                Ok(true)
            }
            Err(why) => Err(why),
        }
    }

    /// `scalable_intrinsic`: the stubs of SVE's selects and conversions.
    fn scalable_intrinsic(
        &mut self,
        name: &str,
        generic_args: GenericArgs<'db>,
        args: &[Operand],
        dst_ty: Ty<'db>,
    ) -> Result<bool, String> {
        let ours = name.starts_with("sve_tuple_") || matches!(name, "simd_select" | "simd_cast");
        if !ours {
            return Ok(false);
        }
        let Some(to) = self.fx.scalable(dst_ty).map_err(told)? else { return Ok(false) };
        let mut shapes = Vec::with_capacity(args.len());
        for arg in args {
            let ty = self.operand(arg)?;
            shapes.push(self.fx.scalable(ty).map_err(told)?);
        }
        match (name, &shapes[..]) {
            ("sve_tuple_create2" | "sve_tuple_create3" | "sve_tuple_create4", _) => {
                if shapes.len() as u64 != to.vectors
                    || shapes.iter().any(|it| it.is_none_or(|it| it.vectors != 1 || it.predicate()))
                {
                    return Err(unsupported(format!("`{name}` of {dst_ty:?}")));
                }
            }
            ("sve_tuple_get" | "sve_tuple_set", _) => {
                let Some(index) = generic_args.iter().nth(2).and_then(|it| it.konst()) else {
                    return Err(unsupported(format!("`{name}` without its index")));
                };
                let ConstKind::Value(index) = index.kind() else {
                    return Err(unsupported(format!("`{name}` with an index not evaluated")));
                };
                let index = index.value.inner().to_leaf().to_bits(Size::from_bytes(4)) as i32;
                let fits = match (name, &shapes[..]) {
                    ("sve_tuple_get", [Some(of)]) => {
                        (0..of.vectors as i32).contains(&index) && to.vectors == 1
                    }
                    ("sve_tuple_set", [Some(of), Some(one)]) => {
                        (0..of.vectors as i32).contains(&index) && one.vectors == 1 && *of == to
                    }
                    _ => false,
                };
                if !fits {
                    return Err(unsupported(format!("`{name}` of {dst_ty:?} at {index}")));
                }
            }
            ("simd_select", [Some(mask), Some(value), Some(_)]) => {
                let text = sve::select(*mask, *value).map_err(unsupported)?;
                self.assembly(text)?;
            }
            ("simd_cast", [Some(from)]) => {
                if let Some(text) = sve::cast(*from, to).map_err(unsupported)? {
                    self.assembly(text)?;
                }
            }
            _ => return Err(unsupported(format!("intrinsic `{name}` on scalable vectors"))),
        }
        Ok(true)
    }

    /// The match of `FunctionCx::intrinsic`, arm by arm.
    fn lowered_intrinsic(
        &mut self,
        name: &str,
        generic_args: GenericArgs<'db>,
        args: &[Operand],
        dst_ty: Ty<'db>,
    ) -> Result<bool, String> {
        let unread = matches!(
            name,
            "assume"
                | "cold_path"
                | "forget"
                | "assert_inhabited"
                | "assert_zero_valid"
                | "assert_mem_uninitialized_valid"
                | "contract_checks"
                | "is_val_statically_known"
                | "unreachable"
                | "abort"
                | "abort_immediate"
                | "breakpoint"
        );
        let arg_tys = match unread {
            true => args.iter().map(|it| self.operand_ty(it)).collect(),
            false => self.operands(args)?,
        };
        let soft = f16_f128::soft;
        match (name, args) {
            (
                "wrapping_add" | "unchecked_add" | "wrapping_sub" | "unchecked_sub"
                | "wrapping_mul" | "unchecked_mul" | "unchecked_div" | "exact_div"
                | "unchecked_rem" | "unchecked_shl" | "unchecked_shr" | "rotate_left"
                | "rotate_right" | "saturating_add" | "saturating_sub" | "three_way_compare",
                [_, _],
            ) => {
                let scalar = self.ty_scalar(arg_tys[0])?;
                match name {
                    "unchecked_div" | "exact_div" => self.int_divide(scalar, true)?,
                    "unchecked_rem" => self.int_divide(scalar, false)?,
                    _ => (),
                }
            }
            ("add_with_overflow" | "sub_with_overflow" | "mul_with_overflow", [_, _]) => {
                let scalar = self.ty_scalar(arg_tys[0])?;
                if name == "mul_with_overflow" && scalar.ty == types::I128 {
                    self.wide_mul(scalar.signed)?;
                }
            }
            ("type_name", []) => {
                let ty = generic_args.type_at(0);
                if hir_ty_mir::mir::type_name::rustc_type_name(self.db(), ty).is_none() {
                    return Err(unsupported(format!("`type_name` of `{ty:?}`")));
                }
            }
            ("va_arg", [_]) => match self.fx.cg.target.variable_arguments() {
                Some(
                    VariableArguments::Buffer
                    | VariableArguments::Homed
                    | VariableArguments::Stack
                    | VariableArguments::Registers,
                ) => (),
                _ => return Err(unsupported("intrinsic `va_arg` for this target")),
            },
            ("float_to_int_unchecked", [_]) => {
                let (from, to) = (self.ty_scalar(arg_tys[0])?, self.ty_scalar(dst_ty)?);
                self.scalar_cast(from, to)?;
            }
            ("atomic_load", [_]) => self.wide_atomic(dst_ty)?,
            ("atomic_store", [_, _]) => self.wide_atomic(arg_tys[1])?,
            (
                "atomic_xchg" | "atomic_xadd" | "atomic_xsub" | "atomic_and" | "atomic_nand"
                | "atomic_or" | "atomic_xor" | "atomic_max" | "atomic_min" | "atomic_umax"
                | "atomic_umin",
                [_, _],
            ) => self.wide_atomic(dst_ty)?,
            ("atomic_cxchg" | "atomic_cxchgweak", [_, _, _]) => self.wide_atomic(arg_tys[1])?,
            (
                "assume"
                | "cold_path"
                | "forget"
                | "assert_inhabited"
                | "assert_zero_valid"
                | "assert_mem_uninitialized_valid"
                | "contract_checks"
                | "is_val_statically_known",
                _,
            ) => (),
            // A trap.
            ("unreachable" | "abort" | "abort_immediate" | "breakpoint", _) => (),
            ("raw_eq" | "compare_bytes", [_, _, ..]) => {
                if name == "raw_eq" {
                    let size = self.ty_layout(generic_args.type_at(0))?.size.bytes();
                    // `copies::equal_bytes` compares as much inline.
                    if size <= 2 * u64::from(crate::ptr().bytes()) {
                        return Ok(true);
                    }
                }
                self.symbol("memcmp")?;
            }
            ("simd_fma" | "simd_relaxed_fma", _) if self.fx.x86_fused() => {
                self.x86_fma(name, args, dst_ty)?
            }
            (
                "simd_add" | "simd_sub" | "simd_mul" | "simd_div" | "simd_eq" | "simd_ge"
                | "simd_gt" | "simd_le" | "simd_lt" | "simd_neg" | "simd_fabs" | "simd_fsqrt"
                | "simd_floor" | "simd_ceil" | "simd_fma" | "simd_cast",
                _,
            ) if self.half_lanes(&arg_tys, dst_ty)? => {
                self.half_vector(name, args, &arg_tys, dst_ty)?
            }
            (
                "simd_and" | "simd_or" | "simd_xor" | "simd_add" | "simd_sub" | "simd_mul"
                | "simd_shl" | "simd_shr" | "simd_eq" | "simd_ne" | "simd_lt" | "simd_le"
                | "simd_gt" | "simd_ge" | "simd_div" | "simd_saturating_add"
                | "simd_saturating_sub",
                [_, _],
            ) => {
                let (lane, _) = self.simd_lanes_of(arg_tys[0])?;
                self.simd_lanes_of(dst_ty)?;
                let by_lanes = matches!(
                    name,
                    "simd_eq"
                        | "simd_ne"
                        | "simd_lt"
                        | "simd_le"
                        | "simd_gt"
                        | "simd_ge"
                        | "simd_add"
                        | "simd_sub"
                        | "simd_mul"
                        | "simd_div"
                );
                if lane.ty.is_float() && !by_lanes {
                    return Err(unsupported(format!("`{name}` of floats")));
                }
                if !lane.ty.is_float() && name == "simd_div" {
                    return Err(unsupported("`simd_div` of integers"));
                }
            }
            ("simd_fma" | "simd_relaxed_fma", [_, _, _]) => {
                let (lane, count) = self.simd_lanes_of(dst_ty)?;
                if !lane.ty.is_float() {
                    return Err(unsupported(format!("`{name}` of integers")));
                }
                let swapped = self.fx.cg.target.arch == Arch::Aarch64 && count > 1;
                let bytes = count * u64::from(lane.ty.bytes());
                let chunk = match bytes {
                    8 => 8,
                    _ if bytes % 16 == 0 => 16,
                    _ => 0,
                };
                let vectors = swapped && matches!(lane.ty, types::F32 | types::F64) && chunk != 0;
                let unfused = name == "simd_relaxed_fma" && self.on_x86();
                if !vectors && !unfused {
                    self.cranelift_routine(Routine::Fma, lane.ty)?;
                }
            }
            ("simd_cast" | "simd_as", [_]) => {
                let (from, _) = self.simd_lanes_of(arg_tys[0])?;
                let (to, _) = self.simd_lanes_of(dst_ty)?;
                if from.ty.is_float() || to.ty.is_float() {
                    self.scalar_cast(from, to)?;
                }
            }
            ("simd_bitmask", [_]) => {
                let (lane, count) = self.simd_lanes_of(arg_tys[0])?;
                let BackendRepr::Scalar(_) = self.ty_layout(dst_ty)?.backend_repr else {
                    return Err(unsupported(format!("simd_bitmask into {dst_ty:?}")));
                };
                self.ty_scalar(dst_ty)?;
                if count > 64 || lane.ty.bits() > 64 {
                    return Err(unsupported(format!("simd_bitmask of {count} lanes")));
                }
            }
            (
                "simd_reduce_and" | "simd_reduce_or" | "simd_reduce_xor" | "simd_reduce_max"
                | "simd_reduce_min",
                [_],
            ) => {
                let (lane, _) = self.simd_lanes_of(arg_tys[0])?;
                if lane.ty.is_float() {
                    return Err(unsupported(format!("`{name}` of floats")));
                }
            }
            ("simd_trunc" | "simd_round" | "simd_round_ties_even", [_])
                if self.fx.cg.target.arch == Arch::Aarch64 =>
            {
                let ty = arg_tys[0];
                let lanes = match self.ty_layout(ty)?.backend_repr {
                    BackendRepr::SimdVector { element, count } => match element.primitive() {
                        Primitive::Float(float) => {
                            neon::Lanes::floats(float.size().bits() as u32, count.as_u64() as u32)
                        }
                        _ => return Err(unsupported(format!("`{name}` of integers"))),
                    },
                    _ => return Err(unsupported(format!("vector operation on {ty:?}"))),
                };
                let instruction = match name {
                    "simd_trunc" => "frintz",
                    "simd_round" => "frinta",
                    _ => "frintx",
                };
                let Some(lowered) = neon::rounding(instruction, lanes) else {
                    return Err(unsupported(format!("`{name}` of {ty:?}")));
                };
                self.neon_assembly(name, &lowered, args, dst_ty)?;
            }
            (
                "simd_ctpop" | "simd_ctlz" | "simd_cttz" | "simd_bswap" | "simd_bitreverse"
                | "simd_neg" | "simd_fabs" | "simd_fsqrt" | "simd_floor" | "simd_ceil",
                [_],
            ) => {
                let (lane, _) = self.simd_lanes_of(arg_tys[0])?;
                let float = lane.ty.is_float();
                match name {
                    "simd_ctpop" | "simd_ctlz" | "simd_cttz" | "simd_bswap" | "simd_bitreverse"
                        if float =>
                    {
                        return Err(unsupported(format!("`{name}` of floats")));
                    }
                    "simd_fabs" | "simd_fsqrt" | "simd_floor" | "simd_ceil" if !float => {
                        return Err(unsupported(format!("`{name}` of integers")));
                    }
                    "simd_fsqrt" | "simd_floor" | "simd_ceil"
                        if lane.ty != types::F32 && lane.ty != types::F64 =>
                    {
                        return Err(unsupported(format!("`{name}` of {:?} lanes", lane.ty)));
                    }
                    "simd_floor" => self.cranelift_routine(Routine::Floor, lane.ty)?,
                    "simd_ceil" => self.cranelift_routine(Routine::Ceil, lane.ty)?,
                    _ => (),
                }
            }
            ("simd_funnel_shl" | "simd_funnel_shr", [_, _, _]) => {
                let (lane, _) = self.simd_lanes_of(arg_tys[0])?;
                if lane.ty.is_float() {
                    return Err(unsupported(format!("`{name}` of floats")));
                }
            }
            (
                "simd_reduce_add_ordered"
                | "simd_reduce_mul_ordered"
                | "simd_reduce_add_unordered"
                | "simd_reduce_mul_unordered",
                [_, start @ ..],
            ) if start.len() == name.ends_with("_ordered") as usize => (),
            (
                "fabs" | "fabsf32" | "fabsf64" | "sqrtf32" | "sqrtf64" | "floorf32" | "floorf64"
                | "ceilf32" | "ceilf64" | "truncf32" | "truncf64" | "round_ties_even_f32"
                | "round_ties_even_f64" | "roundf32" | "roundf64" | "sqrt" | "floor" | "ceil"
                | "trunc" | "round_ties_even" | "round",
                [_],
            ) => {
                let scalar = self.ty_scalar(arg_tys[0])?;
                if !scalar.ty.is_float() {
                    return Err(unsupported(format!("intrinsic `{name}` on {:?}", scalar.ty)));
                }
                let op = name.trim_end_matches("f32").trim_end_matches("f64").trim_end_matches('_');
                if soft(scalar.ty) {
                    self.soft_unary(op, scalar.ty)?;
                    return Ok(true);
                }
                match op {
                    "fabs" | "sqrt" => (),
                    "floor" => self.cranelift_routine(Routine::Floor, scalar.ty)?,
                    "ceil" => self.cranelift_routine(Routine::Ceil, scalar.ty)?,
                    "trunc" => self.cranelift_routine(Routine::Trunc, scalar.ty)?,
                    "round_ties_even" => self.cranelift_routine(Routine::Nearest, scalar.ty)?,
                    _ => self.symbol(if scalar.ty == types::F32 { "roundf" } else { "round" })?,
                }
            }
            ("powif32" | "powif64" | "powi", [_, _]) => {
                let scalar = self.ty_scalar(arg_tys[0])?;
                self.ty_scalar(arg_tys[1])?;
                if soft(scalar.ty) {
                    self.soft_powi(scalar.ty)?;
                } else {
                    self.symbol(if scalar.ty == types::F32 { "__powisf2" } else { "__powidf2" })?;
                }
            }
            (
                "copysignf32" | "copysignf64" | "copysignf16" | "copysignf128" | "minnumf32"
                | "minnumf64" | "maxnumf32" | "maxnumf64" | "powf32" | "powf64" | "fadd_fast"
                | "fsub_fast" | "fmul_fast" | "fdiv_fast" | "frem_fast" | "fadd_algebraic"
                | "fsub_algebraic" | "fmul_algebraic" | "fdiv_algebraic" | "frem_algebraic",
                [_, _],
            ) => {
                let scalar = self.ty_scalar(arg_tys[0])?;
                self.ty_scalar(arg_tys[1])?;
                if !scalar.ty.is_float() {
                    return Err(unsupported(format!("intrinsic `{name}` on {:?}", scalar.ty)));
                }
                if soft(scalar.ty) {
                    let op = match name {
                        "copysignf16" | "copysignf128" => None,
                        "fadd_fast" | "fadd_algebraic" => Some(BinOp::Add),
                        "fsub_fast" | "fsub_algebraic" => Some(BinOp::Sub),
                        "fmul_fast" | "fmul_algebraic" => Some(BinOp::Mul),
                        "fdiv_fast" | "fdiv_algebraic" => Some(BinOp::Div),
                        "frem_fast" | "frem_algebraic" => Some(BinOp::Rem),
                        _ => {
                            let why = format!("intrinsic `{name}` on {:?}", scalar.ty);
                            return Err(unsupported(why));
                        }
                    };
                    // `soft_copysign` moves bits.
                    if let Some(op) = op {
                        self.soft_binary(&op, scalar.ty)?;
                    }
                    return Ok(true);
                }
                let single = scalar.ty == types::F32;
                match name {
                    "minnumf32" | "minnumf64" => self.symbol(if single { "fminf" } else { "fmin" })?,
                    "maxnumf32" | "maxnumf64" => self.symbol(if single { "fmaxf" } else { "fmax" })?,
                    "powf32" | "powf64" => self.symbol(if single { "powf" } else { "pow" })?,
                    "frem_fast" | "frem_algebraic" => {
                        if !single && scalar.ty != types::F64 {
                            let why = format!("intrinsic `{name}` on {:?}", scalar.ty);
                            return Err(unsupported(why));
                        }
                        self.float_binary(&BinOp::Rem, scalar.ty)?;
                    }
                    _ => (),
                }
            }
            ("fmaf32" | "fmaf64" | "fmuladdf32" | "fmuladdf64" | "fma", [_, _, _])
                if self.fx.x86_fused() && self.ty_scalar(dst_ty)?.ty != types::F128 =>
            {
                self.x86_fma(name, args, dst_ty)?
            }
            ("fmaf32" | "fmaf64" | "fmuladdf32" | "fmuladdf64" | "fma", [_, _, _]) => {
                let scalar = self.ty_scalar(arg_tys[0])?;
                if soft(scalar.ty) {
                    self.soft_fma(scalar.ty)?;
                } else if !name.starts_with("fmuladd") {
                    self.symbol(if scalar.ty == types::F32 { "fmaf" } else { "fma" })?;
                }
            }
            ("catch_unwind", [_, _, _]) => {
                let outcome = self.ty_layout(dst_ty)?.size.bytes();
                if ir::Type::int_with_byte_size(outcome as u16).is_none() {
                    return Err(unsupported(format!("`catch_unwind` giving {outcome} bytes")));
                }
                // The calls of the closures go through the pointers MIR passes, whose
                // functions their reification listed; the landing pad is `outside`'s. With
                // SEH what was caught is released.
                if !self.fx.cg.target.exception_in_register() {
                    self.symbol(crate::unwind::seh::RELEASE)?;
                }
            }
            // Instructions of their operands, constants and layouts, and data.
            ("ctpop" | "ctlz" | "ctlz_nonzero" | "cttz" | "cttz_nonzero" | "bswap" | "bitreverse", [_])
            | ("size_of" | "align_of" | "min_align_of" | "pref_align_of", [])
            | ("needs_drop" | "ub_checks" | "overflow_checks" | "caller_location", [])
            | ("transmute" | "transmute_unchecked" | "black_box" | "likely" | "unlikely", [_])
            | ("atomic_fence" | "atomic_singlethreadfence", [])
            | ("read_via_copy" | "volatile_load" | "unaligned_volatile_load", [_])
            | ("write_via_move" | "volatile_store" | "unaligned_volatile_store", [_, _])
            | ("write_box_via_move", [_, _])
            | ("offset" | "arith_offset", [_, _])
            | ("ptr_mask", [_, _])
            | ("ptr_offset_from" | "ptr_offset_from_unsigned", [_, _])
            | ("copy" | "copy_nonoverlapping" | "write_bytes", [_, _, _])
            | ("simd_select" | "simd_select_bitmask", [_, _, _])
            | ("simd_extract", [_, _])
            | ("simd_insert", [_, _, _])
            | ("simd_shuffle", [_, _, _])
            | ("simd_splat", [_])
            | ("simd_reduce_all" | "simd_reduce_any", [_])
            | ("simd_masked_load" | "simd_masked_store", [_, _, _])
            | ("discriminant_value", [_])
            | ("ptr_metadata", [_])
            | ("aggregate_raw_ptr", [_, _])
            | ("slice_get_unchecked", [_, _])
            | ("vtable_size" | "vtable_align", [_])
            | ("size_of_val" | "align_of_val", [_]) => (),
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// `FunctionCx::llvm_intrinsic`: a `#[link_name = "llvm.*"]` function, `name` without
    /// `llvm.`.
    pub(super) fn llvm_intrinsic(
        &mut self,
        name: &str,
        args: &[Operand],
        dst_ty: Ty<'db>,
    ) -> Result<(), String> {
        let arg_tys = self.operands(args)?;
        match self.llvm_lowered(name, args, &arg_tys, dst_ty) {
            // `wasm_llvm_intrinsic` makes wasm's instructions of what it knows, which ask
            // for nothing.
            Err(_) if self.fx.cg.target.is_wasm() => Ok(()),
            done => done,
        }
    }

    fn llvm_lowered(
        &mut self,
        name: &str,
        args: &[Operand],
        arg_tys: &[Ty<'db>],
        dst_ty: Ty<'db>,
    ) -> Result<(), String> {
        let arch = self.fx.cg.target.arch;
        if let Some(neon) = name.strip_prefix("aarch64.neon.")
            && neon_pairwise(neon, args.len())
        {
            return Ok(());
        }
        if name.starts_with("aarch64.sve.")
            || name.starts_with("vector.reverse.nxv")
            || name.starts_with("vector.insert.nxv")
        {
            return self.sve_intrinsic(name, args, arg_tys, dst_ty);
        }
        match (name, args) {
            (
                "x86.sse.cmp.ps" | "x86.sse2.cmp.pd" | "x86.avx.cmp.ps.256" | "x86.avx.cmp.pd.256"
                | "x86.sse.cmp.ss" | "x86.sse2.cmp.sd",
                [_, _, predicate],
            ) => {
                if self.const_immediate(predicate)?.is_none() {
                    let why = format!("`llvm.{name}` with a predicate that is not a constant");
                    return Err(unsupported(why));
                }
            }
            (
                "x86.sse41.round.ps"
                | "x86.avx.round.ps.256"
                | "x86.sse41.round.pd"
                | "x86.avx.round.pd.256",
                [_, mode],
            ) => {
                let Some(mode) = self.const_immediate(mode)? else {
                    let why = format!("LLVM intrinsic `{name}` with a mode that is not a constant");
                    return Err(unsupported(why));
                };
                let double = name.ends_with("pd") || name.contains(".pd.");
                let lane = if double { types::F64 } else { types::F32 };
                let rounding = match if mode & 4 != 0 { 0 } else { mode & 3 } {
                    0 => Routine::Nearest,
                    1 => Routine::Floor,
                    2 => Routine::Ceil,
                    _ => Routine::Trunc,
                };
                self.cranelift_routine(rounding, lane)?;
            }
            (
                "x86.sha1msg1" | "x86.sha1msg2" | "x86.sha1nexte" | "x86.sha256msg1"
                | "x86.sha256msg2",
                [_, _],
            ) => {
                let instruction = format!("{} xmm0, xmm1", name.trim_start_matches("x86."));
                self.assembly(asm::vector_instruction(&instruction, "xmm0"))?;
            }
            ("x86.sha256rnds2", [_, _, _]) => {
                let rounds = "movdqa xmm3, xmm0\nmovdqa xmm0, xmm2\nsha256rnds2 xmm3, xmm1";
                self.assembly(asm::vector_instruction(rounds, "xmm3"))?;
            }
            (
                "aarch64.crypto.sha256su0"
                | "aarch64.crypto.sha512su0"
                | "aarch64.crypto.sha1su1"
                | "aarch64.crypto.aese"
                | "aarch64.crypto.aesd",
                [_, _],
            )
            | ("aarch64.crypto.aesmc" | "aarch64.crypto.aesimc", [_])
            | (
                "aarch64.crypto.sha256h"
                | "aarch64.crypto.sha256h2"
                | "aarch64.crypto.sha256su1"
                | "aarch64.crypto.sha512h"
                | "aarch64.crypto.sha512h2"
                | "aarch64.crypto.sha512su1"
                | "aarch64.crypto.sha1su0",
                [_, _, _],
            ) => self.assembly(crate::neon_crypto(name))?,
            ("x86.sha1rnds4", [_, _, _]) => self.assembly(asm::sha1rnds4())?,
            ("x86.xgetbv", [_]) => self.assembly(asm::xgetbv())?,
            (
                "aarch64.crc32b" | "aarch64.crc32h" | "aarch64.crc32w" | "aarch64.crc32x"
                | "aarch64.crc32cb" | "aarch64.crc32ch" | "aarch64.crc32cw" | "aarch64.crc32cx",
                [_, _],
            ) => self.assembly(asm::crc32(&name["aarch64.".len()..]))?,
            (
                "x86.sse42.crc32.32.8"
                | "x86.sse42.crc32.32.16"
                | "x86.sse42.crc32.32.32"
                | "x86.sse42.crc32.64.64",
                [_, _],
            ) => {
                let data = self.ty_scalar(arg_tys[1])?;
                self.assembly(asm::x86_crc32(data.ty.bits()))?;
            }
            ("aarch64.dmb" | "aarch64.dsb" | "aarch64.hint", [option]) => {
                let Some(option) = self.const_immediate(option)? else {
                    let why = format!("`llvm.{name}` of an option that is not a constant");
                    return Err(unsupported(why));
                };
                let (word, most, at) = match name {
                    "aarch64.dmb" => (0xd503_30bf_u32, 15, 8),
                    "aarch64.dsb" => (0xd503_309f, 15, 8),
                    _ => (0xd503_201f, 127, 5),
                };
                if option > most {
                    return Err(unsupported(format!("`llvm.{name}` of option {option}")));
                }
                self.assembly(asm::instruction_word(word | (option as u32) << at))?;
            }
            ("aarch64.rndr" | "aarch64.rndrrs", []) => {
                let layout = self.ty_layout(dst_ty)?;
                if layout.size.bytes() != 16 || layout.fields.offset(1).bytes() != 8 {
                    return Err(unsupported(format!("`llvm.{name}` returning {dst_ty:?}")));
                }
                self.assembly(asm::random(name == "aarch64.rndrrs"))?;
            }
            ("x86.sse2.pause", []) => self.assembly(asm::pause())?,
            ("aarch64.isb", [_]) => self.assembly(asm::isb())?,
            ("x86.vcvtps2ph.128" | "x86.vcvtps2ph.256", [_, _]) => {
                let register = if name.ends_with("256") { "ymm0" } else { "xmm0" };
                self.assembly(asm::vcvtps2ph(register))?;
            }
            (_, [_, _, _]) if name.starts_with("x86.avx512.vpermi2var.") => {
                if !matches!(name.split('.').nth(3), Some("qi" | "hi" | "d" | "ps" | "q" | "pd")) {
                    return Err(unsupported(format!("LLVM intrinsic `{name}`")));
                }
            }
            // Instructions of their operands.
            ("x86.sse3.ldu.dq" | "x86.avx.ldu.dq.256", [_])
            | ("x86.ssse3.pshuf.b.128" | "x86.avx2.pshuf.b" | "x86.avx512.pshuf.b.512", [_, _])
            | ("x86.avx2.permd" | "x86.avx2.permps", [_, _])
            | ("x86.sse2.psad.bw" | "x86.avx2.psad.bw" | "x86.avx512.psad.bw.512", [_, _])
            | ("x86.sse2.pmadd.wd" | "x86.avx2.pmadd.wd" | "x86.avx512.pmaddw.d.512", [_, _])
            | (
                "x86.ssse3.pmadd.ub.sw.128" | "x86.avx2.pmadd.ub.sw" | "x86.avx512.pmaddubs.w.512",
                [_, _],
            )
            | (
                "x86.sse.min.ps" | "x86.sse.max.ps" | "x86.avx.min.ps.256" | "x86.avx.max.ps.256"
                | "x86.sse2.min.pd" | "x86.sse2.max.pd" | "x86.avx.min.pd.256"
                | "x86.avx.max.pd.256",
                [_, _],
            )
            | ("x86.sse2.cvttps2dq" | "x86.avx.cvtt.ps2dq.256", [_])
            | ("x86.sse2.psrl.w" | "x86.avx2.psrl.w", [_, _])
            | ("x86.sse2.packsswb.128" | "x86.avx2.packsswb" | "x86.avx512.packsswb.512", [_, _])
            | ("x86.sse2.packuswb.128" | "x86.avx2.packuswb" | "x86.avx512.packuswb.512", [_, _])
            | ("x86.sse2.packssdw.128" | "x86.avx2.packssdw" | "x86.avx512.packssdw.512", [_, _])
            | ("x86.sse41.packusdw" | "x86.avx2.packusdw" | "x86.avx512.packusdw.512", [_, _])
            | ("x86.pclmulqdq" | "x86.pclmulqdq.256" | "x86.pclmulqdq.512", [_, _, _])
            | ("x86.sse41.phminposuw", [_])
            | ("x86.avx.vzeroupper", [])
            | ("prefetch.p0", [_, _, _, _]) => (),
            (_, [_, _]) if name.starts_with("x86.avx512.permvar.") => (),
            (_, [_, _]) if crate::x86_lanewise(name).is_some() => (),
            (_, [_, _, _, _]) if name.starts_with("x86.avx512.pternlog.") => (),
            (_, [_, _, _])
                if name.starts_with("x86.avx512.vpmadd52l.uq.")
                    || name.starts_with("x86.avx512.vpmadd52h.uq.") => {}
            _ if arch == Arch::Aarch64
                && let Some(lowered) = neon::lower(name) =>
            {
                self.neon_assembly(name, &lowered, args, dst_ty)?
            }
            _ if arch == Arch::X86_64
                && [
                    "x86.avx512.",
                    "x86.avx512fp16.",
                    "x86.avx512bf16.",
                    "x86.avx2.vpdp",
                    "x86.vcvtne",
                    "x86.vbcstne",
                ]
                .iter()
                .any(|it| name.starts_with(it))
                && self.avx512_assembly(name, args, arg_tys, dst_ty)? => {}
            _ if arch == Arch::X86_64
                && let Some(lowered) = x86::lower(name) =>
            {
                self.x86_assembly(name, &lowered, args, arg_tys, dst_ty)?
            }
            _ => return Err(unsupported(format!("LLVM intrinsic `{name}`"))),
        }
        Ok(())
    }

    /// What `x86_inlined_fma` makes of a call of `func` in a function of core with FMA: the
    /// fma of the wrapper rustc's MIR inliner puts into it, in any of its forms. Whether `func`
    /// is such a wrapper is `fma.rs`'s to say, so every call it could be is taken as one.
    pub(super) fn inlined_fma(&mut self, func: FunctionId, dst_ty: Ty<'db>) -> Result<(), String> {
        let db = self.db();
        let caller = self.fx.body.and_then(|it| it.owner.as_function());
        if !self.fx.x86_fused()
            || FunctionSignature::is_intrinsic(db, func)
            || !caller.is_some_and(|it| it.krate(db).data(db).origin.is_lang())
        {
            return Ok(());
        }
        match self.fma_shape("simd_fma", dst_ty) {
            Ok((bytes, suffix)) => self.fused_forms(bytes, suffix),
            // `lower_fma` refuses it, if it is made at all.
            Err(_) => Ok(()),
        }
    }

    /// `x86_fma`: the fma of the intrinsic call in a function with FMA.
    fn x86_fma(&mut self, name: &str, args: &[Operand], dst_ty: Ty<'db>) -> Result<(), String> {
        if args.len() != 3 {
            return Err(unsupported(format!("`{name}` with {} arguments", args.len())));
        }
        if matches!(name, "simd_relaxed_fma" | "fmuladdf32" | "fmuladdf64") {
            return Err(unsupported(format!(
                "`{name}` in an x86-64 function with FMA, which LLVM may or may not fuse there"
            )));
        }
        let (bytes, suffix) = self.fma_shape(name, dst_ty)?;
        self.fused_forms(bytes, suffix)
    }

    /// What `lower_fma` refuses of an fma giving `dst_ty`, and else its bytes and suffix.
    fn fma_shape(&self, name: &str, dst_ty: Ty<'db>) -> Result<(u32, &'static str), String> {
        if self.enables_feature("fma4") {
            return Err(unsupported(format!("`{name}` in an x86-64 function with FMA4")));
        }
        let layout = self.ty_layout(dst_ty)?;
        let bytes = layout.size.bytes() as u32;
        let (lane, scalar) = match layout.backend_repr {
            BackendRepr::Scalar(scalar) => (self.fx.scalar(scalar).map_err(told)?.ty, true),
            BackendRepr::SimdVector { element, .. } => {
                (self.fx.scalar(element).map_err(told)?.ty, false)
            }
            _ => return Err(unsupported(format!("`{name}` of {dst_ty:?}"))),
        };
        if !lane.is_float() || lane == types::F128 {
            return Err(unsupported(format!("`{name}` of {lane} in an x86-64 function with FMA")));
        }
        if lane == types::F16 && !self.enables_feature("avx512fp16") {
            return Err(unsupported(format!(
                "`{name}` of f16 in an x86-64 function with FMA and without AVX512-FP16, \
                 which LLVM fuses by its context"
            )));
        }
        let vl = lane == types::F16 && !scalar && bytes < 64 && !self.enables_feature("avx512vl");
        if !scalar && !matches!(bytes, 16 | 32 | 64)
            || bytes == 64 && !self.enables_feature("avx512f")
            || vl
        {
            let why = format!("`{name}` of {dst_ty:?} in an x86-64 function with FMA");
            return Err(unsupported(why));
        }
        let suffix = match (lane, scalar) {
            (types::F16, true) => "sh",
            (types::F32, true) => "ss",
            (types::F64, true) => "sd",
            (types::F16, false) => "ph",
            (types::F32, false) => "ps",
            _ => "pd",
        };
        Ok((bytes, suffix))
    }

    /// The functions of each form `lower_fma` may give an fma of `bytes` and `suffix`, as
    /// `call_assembled` makes them.
    fn fused_forms(&mut self, bytes: u32, suffix: &str) -> Result<(), String> {
        let loads = [avx512::Load::Vector(bytes); 3];
        for text in fma_texts(bytes, suffix) {
            self.assembly(asm::avx512_function(&loads, &text, avx512::Out::Vector(bytes)))?;
        }
        Ok(())
    }

    /// `fma.rs`'s `enables`: whether the function's crate or the function has `feature`.
    fn enables_feature(&self, feature: &str) -> bool {
        let atom = cfg::CfgAtom::KeyValue {
            key: intern::sym::target_feature,
            value: intern::Symbol::intern(feature),
        };
        self.fx.env.krate.cfg_options(self.db()).check_atom(&atom)
            || self.fx.function_enables(feature)
    }

    /// `FunctionCx::half_lanes` of the operands' types.
    fn half_lanes(&self, arg_tys: &[Ty<'db>], dst_ty: Ty<'db>) -> Result<bool, String> {
        for ty in arg_tys.first().copied().into_iter().chain([dst_ty]) {
            if let BackendRepr::SimdVector { element, .. } = self.ty_layout(ty)?.backend_repr
                && let Primitive::Float(rustc_abi::Float::F16) = element.primitive()
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// `FunctionCx::half_vector` and `x86_half_vector`: the instruction of a generic intrinsic
    /// on vectors of `f16`.
    fn half_vector(
        &mut self,
        name: &str,
        args: &[Operand],
        arg_tys: &[Ty<'db>],
        dst_ty: Ty<'db>,
    ) -> Result<(), String> {
        let Some(&ty) = arg_tys.first() else {
            return Err(unsupported(format!("`{name}` without operands")));
        };
        let (a, a_signed) = self.fx.neon_lanes_of(ty).map_err(told)?;
        let (r, r_signed) = self.fx.neon_lanes_of(dst_ty).map_err(told)?;
        let widths = (r.float && a.float).then_some((r.bits, a.bits));
        if self.on_x86() {
            // `vpxor` of the sign of each lane.
            if name == "simd_neg" && self.fx.function_enables("avx512fp16") && a.float && a.bits == 16
            {
                if args.len() != 1 {
                    return Err(unsupported(format!("`simd_neg` with {} arguments", args.len())));
                }
                return Ok(());
            }
            let lowered = avx512::half_vector(name, a.count * a.bits / 8, r.count * r.bits / 8, |it| {
                self.fx.function_enables(it)
            });
            let Some(lowered) = lowered.filter(|it| it.loads.len() == args.len()) else {
                return Err(unsupported(format!(
                    "`{name}` of lanes of f16 on x86-64 without an instruction for it here"
                )));
            };
            return self.assembly(asm::avx512_function(&lowered.loads, &lowered.text, lowered.out));
        }
        let single_and_half = name == "simd_cast" && matches!(widths, Some((16, 32) | (32, 16)));
        if self.fx.cg.target.arch != Arch::Aarch64
            || !(single_and_half || self.fx.half_precision())
        {
            return Err(unsupported(
                "vector operation on lanes of f16 without the half-precision instructions",
            ));
        }
        let signed = if a.float { r_signed } else { a_signed };
        let Some(lowered) = neon::half_vector(name, r, a, signed) else {
            return Err(unsupported(format!("`{name}` of lanes of f16 into {dst_ty:?}")));
        };
        self.neon_assembly(name, &lowered, args, dst_ty)
    }

    /// `neon_assembly`: the function of `lowered`'s instructions, its immediate put in.
    fn neon_assembly(
        &mut self,
        name: &str,
        lowered: &neon::Lowered,
        args: &[Operand],
        dst_ty: Ty<'db>,
    ) -> Result<(), String> {
        let mut args: SmallVec<[&Operand; 8]> = args.iter().collect();
        let mut with_immediate = None;
        if let Some(immediate) = &lowered.immediate {
            if immediate.arg >= args.len() {
                return Err(unsupported(format!("`{name}` with {} arguments", args.len())));
            }
            let arg = args.remove(immediate.arg);
            let value = match immediate.splat {
                Some(lane) => self.splat_immediate(arg, lane)?,
                None => self.const_immediate(arg)?,
            };
            let range = &immediate.range;
            let Some(value) =
                value.and_then(|it| u32::try_from(it).ok()).filter(|it| range.contains(it))
            else {
                return Err(unsupported(format!(
                    "`{name}` with an immediate that is not a constant of {}..={}",
                    range.start(),
                    range.end()
                )));
            };
            let text = lowered.text.replace("{imm}", &value.to_string());
            with_immediate = Some(neon::Lowered { text, ..lowered.clone() });
        }
        let arg_tys: SmallVec<[Ty<'db>; 8]> = args.iter().map(|arg| self.operand_ty(arg)).collect();
        let text =
            self.neon_text(name, with_immediate.as_ref().unwrap_or(lowered), &arg_tys, dst_ty)?;
        self.assembly(text)
    }

    /// The text `neon_call` assembles for `lowered` on operands of `arg_tys`.
    fn neon_text(
        &self,
        name: &str,
        lowered: &neon::Lowered,
        arg_tys: &[Ty<'db>],
        dst_ty: Ty<'db>,
    ) -> Result<String, String> {
        if arg_tys.len() != lowered.args.len() {
            return Err(unsupported(format!("`{name}` with {} arguments", arg_tys.len())));
        }
        for (&ty, expected) in arg_tys.iter().zip(&lowered.args) {
            let pointer = matches!(ty.kind(), TyKind::RawPtr(..));
            let fits = match expected {
                None => pointer,
                Some(bytes) => !pointer && self.ty_layout(ty)?.size.bytes() == u64::from(*bytes),
            };
            if !fits {
                return Err(unsupported(format!(
                    "`{name}` of {ty:?}, which is not what its name says"
                )));
            }
        }
        let layout = self.ty_layout(dst_ty)?;
        let size: u64 = lowered.results.iter().map(|&it| u64::from(it)).sum();
        let results: Vec<(u32, u32)> = match lowered.results.as_slice() {
            [] if layout.is_zst() => Vec::new(),
            &[bytes] if layout.size.bytes() == u64::from(bytes) => vec![(0, bytes)],
            many if layout.size.bytes() == size
                && layout.fields.count() == many.len()
                && (0..many.len())
                    .all(|i| layout.fields.offset(i).bytes() == i as u64 * u64::from(many[0])) =>
            {
                many.iter().enumerate().map(|(i, &bytes)| (i as u32 * bytes, bytes)).collect()
            }
            _ => {
                return Err(unsupported(format!(
                    "`{name}` returning {dst_ty:?}, which is not what its name says"
                )));
            }
        };
        let directives = asm::extensions(Arch::Aarch64, lowered.features.iter().copied());
        Ok(asm::neon_function(&directives, &lowered.args, &lowered.text, &results))
    }

    /// `FunctionCx::immediate`, of the walk's types.
    fn const_immediate(&mut self, operand: &Operand) -> Result<Option<u128>, String> {
        if let Some((bits, _)) = self.fx.literal_bits(operand) {
            return Ok(Some(bits));
        }
        let ty = self.operand_ty(operand);
        if !matches!(ty.kind(), TyKind::Int(_) | TyKind::Uint(_)) {
            return Ok(None);
        }
        let bits = self.ty_layout(ty)?.size.bits();
        let mask = u128::MAX >> (128 - bits);
        match &operand.kind {
            OperandKind::Constant { konst, .. } => {
                let ConstKind::Unevaluated(named) = konst.as_ref().kind() else {
                    return Ok(None);
                };
                let allocation = self.recorded(|fx| fx.eval_const(named))?;
                let Some(memory) = allocation.memory.get(..bits as usize / 8) else {
                    return Ok(None);
                };
                let mut bytes = [0; 16];
                bytes[..memory.len()].copy_from_slice(memory);
                Ok(Some(u128::from_le_bytes(bytes)))
            }
            OperandKind::Copy(_) | OperandKind::Move(_) => {
                let Some(Rvalue::Use(from) | Rvalue::Cast(CastKind::IntToInt, from, _)) =
                    self.fx.assigned_once(operand)
                else {
                    return Ok(None);
                };
                let from_ty = self.operand_ty(from);
                let Some(value) = self.const_immediate(from)? else { return Ok(None) };
                let unused = 128 - self.ty_layout(from_ty)?.size.bits() as u32;
                let value = match from_ty.kind() {
                    TyKind::Int(_) => ((value << unused) as i128 >> unused) as u128,
                    _ => value,
                };
                Ok(Some(value & mask))
            }
            _ => Ok(None),
        }
    }

    /// `FunctionCx::splat_immediate`, of the walk's types.
    fn splat_immediate(&mut self, operand: &Operand, lane: u32) -> Result<Option<u128>, String> {
        let memory = match &operand.kind {
            OperandKind::Allocation { allocation } => allocation.as_ref().memory.to_vec(),
            OperandKind::Constant { konst, ty } => match konst.as_ref().kind() {
                ConstKind::Value(value) => {
                    self.recorded(|fx| fx.valtree_blob(ty.as_ref(), value.value))?.bytes
                }
                ConstKind::Unevaluated(named) => {
                    self.recorded(|fx| fx.eval_const(named))?.memory.to_vec()
                }
                _ => return Ok(None),
            },
            OperandKind::Copy(_) | OperandKind::Move(_) => {
                return match self.fx.assigned_once(operand) {
                    Some(Rvalue::Use(from)) => self.splat_immediate(from, lane),
                    Some(Rvalue::Aggregate(AggregateKind::Adt(..), fields)) if fields.len() == 1 => {
                        self.splat_immediate(&fields[0], lane)
                    }
                    Some(Rvalue::Repeat(from, _)) => {
                        let ty = self.operand_ty(from);
                        if self.ty_layout(ty)?.size.bytes() != u64::from(lane) {
                            return Ok(None);
                        }
                        self.const_immediate(from)
                    }
                    _ => Ok(None),
                };
            }
            _ => return Ok(None),
        };
        let ty = self.operand_ty(operand);
        let size = self.ty_layout(ty)?.size.bytes_usize();
        let Some(memory) = memory.get(..size) else { return Ok(None) };
        let mut lanes = memory.chunks_exact(lane as usize);
        let Some(first) = lanes.next() else { return Ok(None) };
        if !lanes.remainder().is_empty() || lanes.any(|it| it != first) {
            return Ok(None);
        }
        let mut bytes = [0; 16];
        bytes[..first.len()].copy_from_slice(first);
        Ok(Some(u128::from_le_bytes(bytes)))
    }

    /// `avx512_assembly`: false where `avx512::lower` knows no instruction for `name`.
    fn avx512_assembly(
        &mut self,
        name: &str,
        args: &[Operand],
        arg_tys: &[Ty<'db>],
        dst_ty: Ty<'db>,
    ) -> Result<bool, String> {
        let mut given = Vec::with_capacity(args.len());
        for (arg, &ty) in args.iter().zip(arg_tys) {
            given.push(avx512::Given {
                bytes: self.ty_layout(ty)?.size.bytes() as u32,
                pointer: matches!(ty.kind(), TyKind::RawPtr(..)),
                constant: self.const_immediate(arg)?,
            });
        }
        let layout = self.ty_layout(dst_ty)?;
        let func = self.fx.body.and_then(|it| it.owner.as_function());
        let features = func.map(|func| AttrFlags::target_features(self.db(), func));
        let features: Vec<&str> = features.into_iter().flatten().map(|it| it.as_str()).collect();
        let lowered = match avx512::lower(name, &given, layout.size.bytes() as u32, &features) {
            None => return Ok(false),
            Some(lowered) => lowered.map_err(unsupported)?,
        };
        if let avx512::Out::Masks(_, at) = lowered.out
            && (layout.fields.count() != 2 || layout.fields.offset(1).bytes() != u64::from(at))
        {
            return Err(unsupported(format!("`llvm.{name}` returning {dst_ty:?}")));
        }
        self.assembly(asm::avx512_function(&lowered.loads, &lowered.text, lowered.out))?;
        Ok(true)
    }

    /// `x86_assembly`: the function of `lowered`'s instruction, its immediate put in.
    fn x86_assembly(
        &mut self,
        name: &str,
        lowered: &x86::Lowered,
        args: &[Operand],
        arg_tys: &[Ty<'db>],
        dst_ty: Ty<'db>,
    ) -> Result<(), String> {
        if args.len() != lowered.args.len() {
            return Err(unsupported(format!("`{name}` with {} arguments", args.len())));
        }
        let mut text = lowered.text.clone();
        let mut reads = Vec::new();
        let mut size = 0;
        for ((arg, &ty), register) in args.iter().zip(arg_tys).zip(&lowered.args) {
            let Some(register) = register else {
                let Some(immediate) = self.const_immediate(arg)?.filter(|it| *it <= 0xff) else {
                    let why = format!("`{name}` with an immediate that is not a constant byte");
                    return Err(unsupported(why));
                };
                text = text.replace("{imm}", &immediate.to_string());
                continue;
            };
            let bytes = asm::x86_register_bytes(register).expect("a register of x86");
            if self.ty_layout(ty)?.size.bytes() != u64::from(bytes) {
                return Err(unsupported(format!(
                    "`{name}` of {ty:?}, which is not what its name says"
                )));
            }
            reads.push((*register, size));
            size += bytes;
        }
        let layout = self.ty_layout(dst_ty)?;
        let register_bytes = |it: &str| u64::from(asm::x86_register_bytes(it).expect("x86's"));
        let writes: Vec<(&str, u32)> = match lowered.results.as_slice() {
            [] if layout.is_zst() => Vec::new(),
            &[register] if layout.size.bytes() == register_bytes(register) => vec![(register, 0)],
            many => {
                let fields: Vec<Ty<'db>> = match dst_ty.kind() {
                    TyKind::Tuple(tys) => tys.iter().collect(),
                    TyKind::Adt(adt_def, subst) if let AdtId::StructId(id) = adt_def.def_id() => {
                        let field_types = self.db().field_types(id.into());
                        field_types
                            .iter()
                            .map(|(_, it)| {
                                let ty =
                                    it.ty().instantiate(self.fx.interner, subst).skip_norm_wip();
                                self.fx.normalize(ty)
                            })
                            .collect()
                    }
                    _ => Vec::new(),
                };
                let mut writes = Vec::new();
                for (i, (&register, &field)) in many.iter().zip(&fields).enumerate() {
                    if self.ty_layout(field)?.size.bytes() != register_bytes(register) {
                        break;
                    }
                    writes.push((register, layout.fields.offset(i).bytes() as u32));
                }
                if fields.len() != many.len() || writes.len() != many.len() {
                    return Err(unsupported(format!(
                        "`{name}` returning {dst_ty:?}, which is not what its name says"
                    )));
                }
                writes
            }
        };
        let text =
            asm::x86_function(&text, &reads, &writes, &lowered.clobbers).map_err(unsupported)?;
        self.assembly(text)
    }

    /// `sve_intrinsic`: the stub of an SVE intrinsic, by the kinds of its operands.
    fn sve_intrinsic(
        &mut self,
        name: &str,
        args: &[Operand],
        arg_tys: &[Ty<'db>],
        dst_ty: Ty<'db>,
    ) -> Result<(), String> {
        let mut kinds = Vec::with_capacity(args.len());
        for (arg, &ty) in args.iter().zip(arg_tys) {
            if let Some(shape) = self.fx.scalable(ty).map_err(told)? {
                kinds.push(sve::Arg::Vector(shape));
                continue;
            }
            let layout = self.ty_layout(ty)?;
            if let Some(shape) = sve::Shape::fixed(&layout) {
                kinds.push(sve::Arg::Fixed(shape));
                continue;
            }
            let BackendRepr::Scalar(_) = layout.backend_repr else {
                return Err(unsupported(format!(
                    "LLVM intrinsic `{name}` with an operand of {ty:?}"
                )));
            };
            let constant = self.sve_constant(arg, ty)?;
            let scalar = self.ty_scalar(ty)?;
            let bits = u64::from(scalar.ty.bits());
            kinds.push(match scalar.ty.is_float() {
                true => sve::Arg::Float(bits),
                false => sve::Arg::Int(bits, constant),
            });
        }
        let ret = if let Some(shape) = self.fx.scalable(dst_ty).map_err(told)? {
            sve::Ret::Vector(shape)
        } else if self.ty_layout(dst_ty)?.is_zst() {
            sve::Ret::Unit
        } else {
            let scalar = self.ty_scalar(dst_ty)?;
            let bits = u64::from(scalar.ty.bits());
            if scalar.ty.is_float() { sve::Ret::Float(bits) } else { sve::Ret::Int(bits) }
        };
        let text = sve::stub(name, &kinds, ret).map_err(unsupported)?;
        self.assembly(text)
    }

    /// `sve_constant`, of the walk's types.
    fn sve_constant(&mut self, operand: &Operand, ty: Ty<'db>) -> Result<Option<i128>, String> {
        let layout = self.ty_layout(ty)?;
        let BackendRepr::Scalar(scalar) = layout.backend_repr else { return Ok(None) };
        let size = layout.size;
        let signed = matches!(scalar.primitive(), Primitive::Int(_, true));
        let bits = match ty.kind() {
            TyKind::Int(_) | TyKind::Uint(_) => self.const_immediate(operand)?,
            TyKind::Adt(..) => {
                let bytes = match &operand.kind {
                    OperandKind::Constant { konst, .. } => match konst.as_ref().kind() {
                        ConstKind::Value(value) => {
                            let blob = self.recorded(|fx| fx.valtree_blob(ty, value.value))?;
                            (blob.relocs.is_empty() && blob.fn_relocs.is_empty())
                                .then_some(blob.bytes)
                        }
                        ConstKind::Unevaluated(named) => {
                            Some(self.recorded(|fx| fx.eval_const(named))?.memory.to_vec())
                        }
                        _ => None,
                    },
                    OperandKind::Allocation { allocation } => {
                        Some(allocation.as_ref().memory.to_vec())
                    }
                    // A const generic argument is moved to a local of its own first.
                    OperandKind::Copy(place) | OperandKind::Move(place)
                        if place.projection.as_slice().is_empty() =>
                    {
                        let Some(Rvalue::Use(from)) = self.fx.assigned_once(operand) else {
                            return Ok(None);
                        };
                        if self.operand_ty(from) != ty {
                            return Ok(None);
                        }
                        return self.sve_constant(from, ty);
                    }
                    _ => None,
                };
                bytes.filter(|it| it.len() >= size.bytes_usize()).map(|it| {
                    let mut all = [0; 16];
                    all[..size.bytes_usize()].copy_from_slice(&it[..size.bytes_usize()]);
                    u128::from_le_bytes(all)
                })
            }
            _ => None,
        };
        Ok(bits.map(|bits| {
            let shift = 128 - size.bits();
            if signed { ((bits << shift) as i128) >> shift } else { bits as i128 }
        }))
    }

    /// The routine Cranelift calls for `op` of a float of `ty` on x86-64.
    fn cranelift_routine(&mut self, op: Routine, ty: ir::Type) -> Result<(), String> {
        if !self.on_x86() {
            return Ok(());
        }
        self.libcall(routine(op, ty))
    }

    /// The compare-and-swap of compiler-builtins an atomic of `ty` is made of, where it is
    /// (`wide_atomics`).
    fn wide_atomic(&mut self, ty: Ty<'db>) -> Result<(), String> {
        let scalar = self.ty_scalar(ty)?;
        if self.fx.wide_atomics(scalar.ty).map_err(told)? {
            self.symbol("__aarch64_cas16_acq_rel")?;
        }
        Ok(())
    }

    /// `divide` of two integers of `scalar`.
    fn int_divide(&mut self, scalar: AbiScalar, div: bool) -> Result<(), String> {
        if scalar.ty != types::I128 {
            return Ok(());
        }
        self.symbol(divide_builtin(div, scalar.signed))
    }

    /// `wide_mul_overflow`.
    fn wide_mul(&mut self, signed: bool) -> Result<(), String> {
        self.symbol(if signed { "__rust_i128_mulo" } else { "__rust_u128_mulo" })
    }

    /// `branch_unless_equal` of two values of `ty`: `memcmp` of bytes, a soft float's `==`.
    /// `seen` are the types already looked at, which ask for the same wherever they are.
    fn equal_parts(&mut self, ty: Ty<'db>, seen: &mut FxHashSet<Ty<'db>>) -> Result<(), String> {
        let db = self.db();
        let ty = self.fx.normalize(ty);
        if !seen.insert(ty) {
            return Ok(());
        }
        match ty.kind() {
            TyKind::Ref(_, inner, _) | TyKind::Pat(inner, _) => self.equal_parts(inner, seen),
            TyKind::Str => self.symbol("memcmp"),
            TyKind::Slice(element) | TyKind::Array(element, _) => {
                if let TyKind::Array(_, len) = ty.kind()
                    && try_const_usize(db, len).is_none()
                {
                    return Err(unsupported("array with unevaluated length"));
                }
                match element.kind() {
                    TyKind::Int(_) | TyKind::Uint(_) | TyKind::Bool | TyKind::Char => {
                        self.symbol("memcmp")
                    }
                    _ => self.equal_parts(element, seen),
                }
            }
            TyKind::Tuple(tys) => {
                for field in tys.iter() {
                    self.equal_parts(field, seen)?;
                }
                Ok(())
            }
            TyKind::Adt(adt_def, subst) => {
                let layout = self.ty_layout(ty)?;
                let variants: Vec<VariantId> = match adt_def.def_id() {
                    AdtId::UnionId(_) => return Err(unsupported("comparison of a union")),
                    AdtId::StructId(id) => vec![id.into()],
                    AdtId::EnumId(id) => {
                        let all = id.enum_variants(db).variants.values().map(|&(it, _)| it);
                        match &layout.variants {
                            Variants::Empty => Vec::new(),
                            Variants::Single { index } => all
                                .filter(|variant| variant.index(db) == index.0)
                                .take(1)
                                .map(VariantId::from)
                                .collect(),
                            // Only the variants with fields that compare in parts are
                            // compared field by field.
                            Variants::Multiple { .. } => all
                                .filter(|&variant| {
                                    db.field_types(variant.into()).iter().next().is_some()
                                        && self.fx.fields_compare_in_parts(
                                            variant.into(),
                                            subst,
                                            &mut vec![ty],
                                        )
                                })
                                .map(VariantId::from)
                                .collect(),
                        }
                    }
                };
                for variant in variants {
                    for (_, field) in db.field_types(variant).iter() {
                        let field = field.ty().instantiate(self.fx.interner, subst).skip_norm_wip();
                        self.equal_parts(field, seen)?;
                    }
                }
                Ok(())
            }
            TyKind::RawPtr(pointee, _) if self.fx.is_unsized(pointee) => Ok(()),
            _ => {
                let scalar = self.ty_scalar(ty)?;
                if f16_f128::soft(scalar.ty) {
                    self.soft_binary(&BinOp::Eq, scalar.ty)?;
                }
                Ok(())
            }
        }
    }

    /// `float_cast` of `from` to `to`.
    fn scalar_cast(&mut self, from: AbiScalar, to: AbiScalar) -> Result<(), String> {
        if f16_f128::soft(from.ty) || f16_f128::soft(to.ty) {
            return self.soft_cast(from, to);
        }
        if from.ty == types::I128 || to.ty == types::I128 {
            let Some(name) = wide_int_float(from, to) else {
                return Err(unsupported(format!("cast from {} to {}", from.ty, to.ty)));
            };
            return self.symbol(name);
        }
        Ok(())
    }

    /// `soft_cast`.
    fn soft_cast(&mut self, from: AbiScalar, to: AbiScalar) -> Result<(), String> {
        let single = AbiScalar { ty: types::F32, signed: true };
        if from.ty == to.ty {
            return Ok(());
        }
        if self.on_x86() && (from.ty == types::F16 || to.ty == types::F16) {
            self.x86_soft_half()?;
        }
        match (from.ty, to.ty) {
            (types::F16, types::F32 | types::F64) => self.widen_half(to.ty),
            (types::F32 | types::F64, types::F16) => self.narrow_to_half(from.ty),
            (types::F16, types::F128) => self.symbol("__extendhftf2"),
            (types::F32, types::F128) => self.symbol("__extendsftf2"),
            (types::F64, types::F128) => self.symbol("__extenddftf2"),
            (types::F128, types::F16) => self.symbol("__trunctfhf2"),
            (types::F128, types::F32) => self.symbol("__trunctfsf2"),
            (types::F128, types::F64) => self.symbol("__trunctfdf2"),
            (types::F16, _) => {
                self.widen_half(types::F32)?;
                self.scalar_cast(single, to)
            }
            (types::I128, types::F16) if self.on_x86() => {
                self.symbol(if from.signed { "__floattihf" } else { "__floatuntihf" })
            }
            (_, types::F16) => {
                self.scalar_cast(from, single)?;
                self.narrow_to_half(types::F32)
            }
            // `quad_to_int`: the conversion, checked against the bounds by comparisons.
            (types::F128, _) => {
                self.soft_binary(&BinOp::Ge, types::F128)?;
                self.symbol(int_of_quad(to.ty.bits(), to.signed))?;
                self.soft_binary(&BinOp::Gt, types::F128)?;
                if to.signed {
                    self.soft_binary(&BinOp::Eq, types::F128)?;
                }
                Ok(())
            }
            (_, types::F128) => self.symbol(quad_of_int(from.ty.bits(), from.signed)),
            _ => Err(unsupported(format!("cast from {} to {}", from.ty, to.ty))),
        }
    }

    /// `float_binary_op` of two `f32`s or `f64`s.
    fn float_binary(&mut self, op: &BinOp, ty: ir::Type) -> Result<(), String> {
        match op {
            BinOp::Eq
            | BinOp::Ne
            | BinOp::Lt
            | BinOp::Le
            | BinOp::Gt
            | BinOp::Ge
            | BinOp::Add
            | BinOp::Sub
            | BinOp::Mul
            | BinOp::Div => Ok(()),
            BinOp::Rem => self.symbol(if ty == types::F32 { "fmodf" } else { "fmod" }),
            _ => Err(unsupported(format!("float operator {op:?}"))),
        }
    }

    /// `soft_binary_op` of two `f16`s or two `f128`s.
    fn soft_binary(&mut self, op: &BinOp, ty: ir::Type) -> Result<(), String> {
        if ty == types::F16 {
            let mnemonic = match op {
                BinOp::Add => Some("fadd"),
                BinOp::Sub => Some("fsub"),
                BinOp::Mul => Some("fmul"),
                BinOp::Div => Some("fdiv"),
                _ => None,
            };
            if let Some(mnemonic) = mnemonic.filter(|_| self.fx.half_precision()) {
                return self.half_instruction(&format!("{mnemonic} h0, h0, h1"));
            }
            if let Some(mnemonic) = mnemonic.filter(|_| self.x86_half_precision()) {
                let op = &mnemonic[1..];
                return self.half_instruction(&format!("v{op}sh xmm0, xmm0, xmm1"));
            }
            self.widen_half(types::F32)?;
            self.float_binary(op, types::F32)?;
            return match op {
                BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => Ok(()),
                _ => self.narrow_to_half(types::F32),
            };
        }
        let name = match op {
            BinOp::Add => "__addtf3",
            BinOp::Sub => "__subtf3",
            BinOp::Mul => "__multf3",
            BinOp::Div => "__divtf3",
            BinOp::Rem => return self.quad_libcall("fmod"),
            BinOp::Eq => "__eqtf2",
            BinOp::Ne => "__netf2",
            BinOp::Lt => "__lttf2",
            BinOp::Le => "__letf2",
            BinOp::Gt => "__gttf2",
            BinOp::Ge => "__getf2",
            _ => return Err(unsupported(format!("float operator {op:?}"))),
        };
        self.symbol(name)
    }

    /// `soft_unary`: `op` of an `f16` or an `f128`, the name of the intrinsic without its
    /// width.
    fn soft_unary(&mut self, op: &str, ty: ir::Type) -> Result<(), String> {
        if op == "fabs" {
            return Ok(());
        }
        if ty == types::F16 && self.fx.half_precision() {
            let mnemonic = match op {
                "sqrt" => "fsqrt",
                "floor" => "frintm",
                "ceil" => "frintp",
                "trunc" => "frintz",
                "round_ties_even" => "frintx",
                _ => "frinta",
            };
            return self.half_instruction(&format!("{mnemonic} h0, h0"));
        }
        if ty == types::F16 {
            self.widen_half(types::F32)?;
            let x86 = self.on_x86();
            match op {
                "sqrt" => (),
                "floor" if x86 => self.symbol("floorf")?,
                "ceil" if x86 => self.symbol("ceilf")?,
                "trunc" if x86 => self.symbol("truncf")?,
                "round_ties_even" if x86 => self.symbol("rintf")?,
                // AArch64's instructions.
                "floor" | "ceil" | "trunc" | "round_ties_even" => (),
                _ => self.symbol("roundf")?,
            }
            return self.narrow_to_half(types::F32);
        }
        let libm = match op {
            "sqrt" | "floor" | "ceil" | "trunc" => op,
            "round_ties_even" => "rint",
            _ => "round",
        };
        self.quad_libcall(libm)
    }

    /// `soft_fma`.
    fn soft_fma(&mut self, ty: ir::Type) -> Result<(), String> {
        if ty == types::F128 {
            return self.quad_libcall("fma");
        }
        if self.fx.half_precision() {
            return self.half_instruction("fmadd h0, h0, h1, h2");
        }
        self.widen_half(types::F64)?;
        self.cranelift_routine(Routine::Fma, types::F64)?;
        self.narrow_to_half(types::F64)
    }

    /// `soft_powi`.
    fn soft_powi(&mut self, ty: ir::Type) -> Result<(), String> {
        if ty == types::F128 {
            return self.symbol("__powitf2");
        }
        self.widen_half(types::F32)?;
        self.symbol("__powisf2")?;
        self.narrow_to_half(types::F32)
    }

    /// `widen_half` of an `f16` to `to`.
    fn widen_half(&mut self, to: ir::Type) -> Result<(), String> {
        if !self.on_x86() {
            return self.assembly(asm::fcvt(letter(to), 'h'));
        }
        self.x86_soft_half()?;
        self.symbol("__extendhfsf2")
    }

    /// `narrow_to_half` of an `f32` or `f64` of `from`.
    fn narrow_to_half(&mut self, from: ir::Type) -> Result<(), String> {
        if !self.on_x86() {
            return self.assembly(asm::fcvt('h', letter(from)));
        }
        self.x86_soft_half()?;
        self.symbol(if from == types::F32 { "__truncsfhf2" } else { "__truncdfhf2" })
    }

    /// `half_instruction`: the function of a half-precision instruction.
    fn half_instruction(&mut self, instruction: &str) -> Result<(), String> {
        let text = match self.on_x86() {
            true => asm::x86_half(instruction),
            false => asm::half(instruction),
        };
        self.assembly(text)
    }

    /// `quad_libcall`: C's function of `f128` named `base`, with the suffix of the target's.
    fn quad_libcall(&mut self, base: &str) -> Result<(), String> {
        let suffix = if self.on_x86() { "f128" } else { "l" };
        self.symbol(&format!("{base}{suffix}"))
    }

    fn x86_soft_half(&self) -> Result<(), String> {
        if self.fx.function_enables("f16c") {
            return Err(unsupported(
                "f16 in a function with F16C, which LLVM converts by its instructions",
            ));
        }
        Ok(())
    }

    fn x86_half_precision(&self) -> bool {
        self.on_x86() && self.fx.function_enables("avx512fp16")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn scalar(ty: ir::Type, signed: bool) -> AbiScalar {
        AbiScalar { ty, signed }
    }

    #[test]
    fn pairwise_neon_is_lowered_lane_by_lane() {
        assert!(neon_pairwise("umaxp.v16i8", 2));
        assert!(neon_pairwise("addp.v8i8", 2));
        assert!(neon_pairwise("addp.v2i64", 2));
        assert!(neon_pairwise("uaddlv.i32.v16i8", 1));
        assert!(!neon_pairwise("uaddlv.v16i8", 1));
        assert!(neon_pairwise("tbl1.v16i8", 2));
        assert!(!neon_pairwise("tbl1.v16i8", 3));
        assert!(neon_pairwise("tbx1.v8i8", 3));
        assert!(!neon_pairwise("tbx1.v8i8", 2));
        assert!(!neon_pairwise("tbl1.v8i16", 2));
        assert!(!neon_pairwise("sqdmulh.v2i64", 2));
        assert!(neon_pairwise("sqdmull.v2i64", 2));
        assert!(!neon_pairwise("sqdmull.v4i16", 2));
        // Float lanes and unknown operations go to `neon::lower`.
        assert!(!neon_pairwise("fmaxp.v4f32", 2));
        assert!(!neon_pairwise("sqshlu.v8i8", 2));
    }

    #[test]
    fn every_fold_of_an_fma_is_listed() {
        let texts = fma_texts(32, "ps");
        assert_eq!(texts.len(), 8);
        assert!(texts.contains(&"vmovaps ymm0, ymm2\nvfmadd213ps ymm0, ymm1, ymm3".to_owned()));
        assert!(texts.contains(&"vmovaps ymm0, ymm1\nvfnmsub213ps ymm0, ymm2, ymm3".to_owned()));
        let texts = fma_texts(4, "ss");
        assert!(texts.contains(&"vmovaps xmm0, xmm2\nvfmsub213ss xmm0, xmm1, xmm3".to_owned()));
        let texts = fma_texts(64, "pd");
        assert!(texts.iter().all(|it| it.starts_with("vmovaps zmm0, zmm")));
    }

    #[test]
    fn wide_integers_convert_by_compiler_builtins() {
        let (int, uint) = (scalar(types::I128, true), scalar(types::I128, false));
        let (single, double) = (scalar(types::F32, true), scalar(types::F64, true));
        assert_eq!(wide_int_float(single, int), Some("__fixsfti"));
        assert_eq!(wide_int_float(double, uint), Some("__fixunsdfti"));
        assert_eq!(wide_int_float(int, double), Some("__floattidf"));
        assert_eq!(wide_int_float(uint, single), Some("__floatuntisf"));
        assert_eq!(wide_int_float(int, uint), None);
        assert_eq!(divide_builtin(true, true), "__divti3");
        assert_eq!(divide_builtin(false, false), "__umodti3");
    }

    #[test]
    fn quads_convert_by_width() {
        assert_eq!(quad_of_int(8, true), "__floatsitf");
        assert_eq!(quad_of_int(64, false), "__floatunditf");
        assert_eq!(quad_of_int(128, true), "__floattitf");
        assert_eq!(int_of_quad(16, false), "__fixunstfsi");
        assert_eq!(int_of_quad(64, true), "__fixtfdi");
        assert_eq!(int_of_quad(128, false), "__fixunstfti");
    }

    #[test]
    fn routines_are_by_width() {
        assert_eq!(routine(Routine::Floor, types::F32), ir::LibCall::FloorF32);
        assert_eq!(routine(Routine::Nearest, types::F64), ir::LibCall::NearestF64);
        assert_eq!(routine(Routine::Fma, types::F64), ir::LibCall::FmaF64);
        assert_eq!(letter(types::F16), 'h');
        assert_eq!(letter(types::F64), 'd');
    }
}

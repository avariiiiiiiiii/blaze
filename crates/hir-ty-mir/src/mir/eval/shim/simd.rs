//! Shim implementation for simd intrinsics

use std::{
    cmp::Ordering,
    ops::{Add, Div, Mul, Rem, Sub},
};

use crate::consteval::try_const_usize;

use super::*;

/// What a lane of a vector holds, for the intrinsics that compute with lanes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lane {
    /// An integer of `bytes` bytes. A raw pointer is an unsigned one as wide as an address.
    Int {
        bytes: usize,
        signed: bool,
    },
    F32,
    F64,
}

impl Lane {
    fn size(self) -> usize {
        match self {
            Lane::Int { bytes, .. } => bytes,
            Lane::F32 => 4,
            Lane::F64 => 8,
        }
    }

    fn bits(self) -> u32 {
        self.size() as u32 * 8
    }

    /// The bits of the lane, as a mask of a 128-bit value.
    fn mask(self) -> u128 {
        u128::MAX >> (128 - self.bits())
    }

    /// The low bits of `value` as a value of the lane: an integer is extended to 128 bits by its
    /// signedness, a float is its bits.
    fn extend(self, value: u128) -> u128 {
        let shift = 128 - self.bits();
        match self {
            Lane::Int { signed: true, .. } => (((value << shift) as i128) >> shift) as u128,
            _ => (value << shift) >> shift,
        }
    }

    fn read(self, bytes: &[u8]) -> u128 {
        self.extend(u128::from_le_bytes(pad16(bytes, IsSigned::No)))
    }
}

/// A float lane, computed in its own width as IEEE 754 rounds it.
trait LaneFloat:
    Copy
    + PartialOrd
    + Add<Output = Self>
    + Sub<Output = Self>
    + Mul<Output = Self>
    + Div<Output = Self>
    + Rem<Output = Self>
{
    /// The bits of the quiet NaN with no payload and a clear sign. Every NaN result is this
    /// one, so that no result depends on the payload a machine would give it.
    const QUIET_NAN: u128;
    fn from_lane(bits: u128) -> Self;
    fn to_lane(self) -> u128;
    fn is_nan(self) -> bool;
    fn sqrt(self) -> Self;
    fn ceil(self) -> Self;
    fn floor(self) -> Self;
    fn round(self) -> Self;
    fn round_ties_even(self) -> Self;
    fn trunc(self) -> Self;
    fn mul_add(self, a: Self, b: Self) -> Self;
    fn min(self, other: Self) -> Self;
    fn max(self, other: Self) -> Self;
    fn from_i128(value: i128) -> Self;
    fn from_u128(value: u128) -> Self;

    /// The lane of a result.
    fn result(self) -> u128 {
        if self.is_nan() { Self::QUIET_NAN } else { self.to_lane() }
    }
}

macro_rules! lane_float {
    ($ty:ident, $bits:ident, $nan:expr) => {
        impl LaneFloat for $ty {
            const QUIET_NAN: u128 = $nan;
            fn from_lane(bits: u128) -> Self {
                $ty::from_bits(bits as $bits)
            }
            fn to_lane(self) -> u128 {
                u128::from(self.to_bits())
            }
            fn is_nan(self) -> bool {
                $ty::is_nan(self)
            }
            fn sqrt(self) -> Self {
                $ty::sqrt(self)
            }
            fn ceil(self) -> Self {
                $ty::ceil(self)
            }
            fn floor(self) -> Self {
                $ty::floor(self)
            }
            fn round(self) -> Self {
                $ty::round(self)
            }
            fn round_ties_even(self) -> Self {
                $ty::round_ties_even(self)
            }
            fn trunc(self) -> Self {
                $ty::trunc(self)
            }
            fn mul_add(self, a: Self, b: Self) -> Self {
                $ty::mul_add(self, a, b)
            }
            fn min(self, other: Self) -> Self {
                $ty::min(self, other)
            }
            fn max(self, other: Self) -> Self {
                $ty::max(self, other)
            }
            fn from_i128(value: i128) -> Self {
                value as $ty
            }
            fn from_u128(value: u128) -> Self {
                value as $ty
            }
        }
    };
}

lane_float!(f32, u32, 0x7fc0_0000);
lane_float!(f64, u64, 0x7ff8_0000_0000_0000);

fn simd_args<'db>(op: SimdOp) -> MirEvalError<'db> {
    MirEvalError::InternalError(format!("{} args are not provided", Intrinsic::Simd(op)).into())
}

fn check_lanes<'db>(op: SimdOp, expected: usize, got: usize) -> Result<'db, ()> {
    if expected == got {
        return Ok(());
    }
    Err(MirEvalError::InternalError(
        format!("{} of vectors of {expected} and {got} lanes", Intrinsic::Simd(op)).into(),
    ))
}

fn simd_ty_arg<'db>(
    generic_args: GenericArgs<'db>,
    index: usize,
    op: SimdOp,
) -> Result<'db, Ty<'db>> {
    generic_args.as_slice().get(index).and_then(|it| it.ty()).ok_or_else(|| {
        MirEvalError::InternalError(
            format!("{} has no type argument {index}", Intrinsic::Simd(op)).into(),
        )
    })
}

/// One lane of a lane-wise operation. A reduction passes the operation it folds with, or
/// itself for `simd_reduce_min` and `simd_reduce_max`, which no lane-wise operation is.
fn lane_op<'db>(op: SimdOp, lane: Lane, index: usize, args: &[u128]) -> Result<'db, u128> {
    let sign = 1u128 << (lane.bits() - 1);
    let value = match (lane, op) {
        (Lane::Int { .. }, _) => int_op(op, lane, index, args)?,
        // Rust defines negation and the absolute value of a float as operations on its sign
        // bit, which keep a NaN's payload.
        (_, SimdOp::Neg) => args[0] ^ sign,
        (_, SimdOp::Fabs) => args[0] & !sign,
        (Lane::F32, _) => float_op::<f32>(op, args)?,
        (Lane::F64, _) => float_op::<f64>(op, args)?,
    };
    Ok(lane.extend(value))
}

fn int_op<'db>(op: SimdOp, lane: Lane, index: usize, args: &[u128]) -> Result<'db, u128> {
    let signed = matches!(lane, Lane::Int { signed: true, .. });
    let bits = lane.bits();
    let mask = lane.mask();
    let ub = |what: String| {
        MirEvalError::UndefinedBehavior(format!(
            "{what} in `{}` in lane {index}",
            Intrinsic::Simd(op)
        ))
    };
    let less = |a: u128, b: u128| if signed { (a as i128) < (b as i128) } else { a < b };
    let (a, b) = (args[0], args.get(1).copied().unwrap_or(0));
    Ok(match op {
        SimdOp::Add => a.wrapping_add(b),
        SimdOp::Sub => a.wrapping_sub(b),
        SimdOp::Mul => a.wrapping_mul(b),
        SimdOp::And => a & b,
        SimdOp::Or => a | b,
        SimdOp::Xor => a ^ b,
        SimdOp::ReduceMin => {
            if less(b, a) {
                b
            } else {
                a
            }
        }
        SimdOp::ReduceMax => {
            if less(a, b) {
                b
            } else {
                a
            }
        }
        SimdOp::Div | SimdOp::Rem if b == 0 => return Err(ub("division by zero".to_owned())),
        SimdOp::Div | SimdOp::Rem if signed => {
            let (a, b) = (a as i128, b as i128);
            if b == -1 && a == i128::MIN >> (128 - bits) {
                let sign = if op == SimdOp::Div { "/" } else { "%" };
                return Err(ub(format!("overflow of {a} {sign} -1")));
            }
            (if op == SimdOp::Div { a / b } else { a % b }) as u128
        }
        SimdOp::Div => a / b,
        SimdOp::Rem => a % b,
        SimdOp::Shl | SimdOp::Shr => {
            let in_range = if signed {
                (0..i128::from(bits)).contains(&(b as i128))
            } else {
                b < u128::from(bits)
            };
            if !in_range {
                let amount = if signed { (b as i128).to_string() } else { b.to_string() };
                return Err(ub(format!("overflowing shift by {amount}")));
            }
            let b = b as u32;
            match (op, signed) {
                (SimdOp::Shl, _) => a << b,
                (_, true) => ((a as i128) >> b) as u128,
                _ => a >> b,
            }
        }
        SimdOp::FunnelShl | SimdOp::FunnelShr => {
            let (a, b, shift) = (a & mask, b & mask, args[2] & mask);
            if shift >= u128::from(bits) {
                return Err(ub(format!("overflowing shift by {shift}")));
            }
            let shift = shift as u32;
            let rest = bits - shift;
            if op == SimdOp::FunnelShl {
                (a << shift) | b.checked_shr(rest).unwrap_or(0)
            } else {
                a.checked_shl(rest).unwrap_or(0) | (b >> shift)
            }
        }
        SimdOp::CarrylessMul => {
            let (a, b) = (a & mask, b & mask);
            (0..bits).filter(|&i| (b >> i) & 1 == 1).fold(0, |product, i| product ^ (a << i))
        }
        SimdOp::SaturatingAdd | SimdOp::SaturatingSub => {
            let add = op == SimdOp::SaturatingAdd;
            if signed {
                let (min, max) = (i128::MIN >> (128 - bits), i128::MAX >> (128 - bits));
                let (a, b) = (a as i128, b as i128);
                let exact = if add { a.checked_add(b) } else { a.checked_sub(b) };
                exact.unwrap_or(if add == (b > 0) { max } else { min }).clamp(min, max) as u128
            } else if add {
                a.checked_add(b).map_or(mask, |it| it.min(mask))
            } else {
                a.saturating_sub(b)
            }
        }
        SimdOp::Neg => a.wrapping_neg(),
        SimdOp::Bswap => (a & mask).swap_bytes() >> (128 - bits),
        SimdOp::Bitreverse => (a & mask).reverse_bits() >> (128 - bits),
        SimdOp::Ctlz => u128::from((a & mask).leading_zeros() - (128 - bits)),
        SimdOp::Cttz => u128::from((a & mask).trailing_zeros().min(bits)),
        SimdOp::Ctpop => u128::from((a & mask).count_ones()),
        _ => not_supported!("`{}` of integer lanes", Intrinsic::Simd(op)),
    })
}

fn float_op<'db, F: LaneFloat>(op: SimdOp, args: &[u128]) -> Result<'db, u128> {
    let x = |i: usize| F::from_lane(args[i]);
    let value = match op {
        SimdOp::Add => x(0) + x(1),
        SimdOp::Sub => x(0) - x(1),
        SimdOp::Mul => x(0) * x(1),
        SimdOp::Div => x(0) / x(1),
        SimdOp::Rem => x(0) % x(1),
        SimdOp::MinimumNumberNsz | SimdOp::ReduceMin => x(0).min(x(1)),
        SimdOp::MaximumNumberNsz | SimdOp::ReduceMax => x(0).max(x(1)),
        SimdOp::Fsqrt => x(0).sqrt(),
        SimdOp::Ceil => x(0).ceil(),
        SimdOp::Floor => x(0).floor(),
        SimdOp::Round => x(0).round(),
        SimdOp::RoundTiesEven => x(0).round_ties_even(),
        SimdOp::Trunc => x(0).trunc(),
        SimdOp::Fma | SimdOp::RelaxedFma => x(0).mul_add(x(1), x(2)),
        _ => not_supported!("`{}` of float lanes", Intrinsic::Simd(op)),
    };
    Ok(value.result())
}

fn compare(lane: Lane, a: u128, b: u128) -> Option<Ordering> {
    match lane {
        Lane::Int { signed: true, .. } => Some((a as i128).cmp(&(b as i128))),
        Lane::Int { signed: false, .. } => Some(a.cmp(&b)),
        Lane::F32 => f32::from_lane(a).partial_cmp(&f32::from_lane(b)),
        Lane::F64 => f64::from_lane(a).partial_cmp(&f64::from_lane(b)),
    }
}

/// A lane of `simd_cast` or `simd_as`, which differ only in a float that does not fit the
/// integer it is cast to: `simd_as` saturates as `as` does, `simd_cast` has undefined behavior.
fn cast_lane<'db>(
    op: SimdOp,
    from: Lane,
    to: Lane,
    index: usize,
    value: u128,
) -> Result<'db, u128> {
    let float = match from {
        Lane::Int { signed, .. } => {
            return Ok(match to {
                Lane::Int { .. } => value,
                Lane::F32 => {
                    (if signed { f32::from_i128(value as i128) } else { f32::from_u128(value) })
                        .result()
                }
                Lane::F64 => {
                    (if signed { f64::from_i128(value as i128) } else { f64::from_u128(value) })
                        .result()
                }
            });
        }
        Lane::F32 => f64::from(f32::from_lane(value)),
        Lane::F64 => f64::from_lane(value),
    };
    Ok(match to {
        Lane::F32 => (float as f32).result(),
        Lane::F64 => float.result(),
        Lane::Int { signed, .. } => {
            let bits = to.bits();
            if op == SimdOp::Cast {
                let power = |bits: u32| 2f64.powi(bits as i32);
                let whole = float.trunc();
                let (low, high) =
                    if signed { (-power(bits - 1), power(bits - 1)) } else { (0.0, power(bits)) };
                if !(low <= whole && whole < high) {
                    return Err(MirEvalError::UndefinedBehavior(format!(
                        "`simd_cast` of {float} in lane {index}, which an integer of {bits} bits \
                         cannot hold"
                    )));
                }
            }
            if signed {
                (float as i128).clamp(i128::MIN >> (128 - bits), i128::MAX >> (128 - bits)) as u128
            } else {
                (float as u128).min(to.mask())
            }
        }
    })
}

impl<'db> Evaluator<'db> {
    /// The number of lanes of a vector type and the type of a lane.
    fn detect_simd_ty(&self, ty: Ty<'db>) -> Result<'db, (usize, Ty<'db>)> {
        let TyKind::Adt(adt_def, subst) = ty.kind() else {
            return Err(MirEvalError::InternalError("simd type which is not a struct".into()));
        };
        let AdtId::StructId(id) = adt_def.def_id() else {
            return Err(MirEvalError::InternalError("simd type which is not a struct".into()));
        };
        let struct_data = id.fields(self.db);
        let fields = struct_data.fields();
        let Some((first_field, _)) = fields.iter().next() else {
            not_supported!("simd type with no field");
        };
        let field_ty = self.db.field_types(id.into())[first_field]
            .ty()
            .instantiate(self.interner(), subst)
            .skip_norm_wip();
        if let TyKind::Array(lane, len) = field_ty.kind() {
            let Some(len) = try_const_usize(self.db, len) else {
                return Err(MirEvalError::InternalError(
                    "simd type with unevaluatable len param".into(),
                ));
            };
            return Ok((len as usize, lane));
        }
        Ok((fields.len(), field_ty))
    }

    fn simd_lane(&self, ty: Ty<'db>) -> Result<'db, Lane> {
        let int_bytes = |bits: Option<u64>| bits.map_or(self.ptr_size(), |it| it as usize / 8);
        Ok(match ty.kind() {
            TyKind::Int(it) => Lane::Int { bytes: int_bytes(it.bit_width()), signed: true },
            TyKind::Uint(it) => Lane::Int { bytes: int_bytes(it.bit_width()), signed: false },
            TyKind::Float(rustc_type_ir::FloatTy::F32) => Lane::F32,
            TyKind::Float(rustc_type_ir::FloatTy::F64) => Lane::F64,
            TyKind::RawPtr(..) => Lane::Int { bytes: self.ptr_size(), signed: false },
            _ => not_supported!("a simd lane of type {ty:?}"),
        })
    }

    fn simd_lanes(&self, ty: Ty<'db>) -> Result<'db, (usize, Lane)> {
        let (len, lane) = self.detect_simd_ty(ty)?;
        Ok((len, self.simd_lane(lane)?))
    }

    fn read_lanes(&self, arg: &IntervalAndTy<'db>) -> Result<'db, (Lane, Vec<u128>)> {
        let (len, lane) = self.simd_lanes(arg.ty)?;
        let Some(bytes) = arg.get(self)?.get(..len * lane.size()) else {
            return Err(MirEvalError::InternalError("simd vector smaller than its lanes".into()));
        };
        Ok((lane, bytes.chunks(lane.size()).map(|it| lane.read(it)).collect()))
    }

    fn write_lanes(
        &mut self,
        destination: Interval,
        lane: Lane,
        values: &[u128],
    ) -> Result<'db, ()> {
        let bytes: Vec<u8> =
            values.iter().flat_map(|it| it.to_le_bytes()[..lane.size()].to_vec()).collect();
        destination.write_from_bytes(self, &bytes)
    }

    /// The lanes of a vector of any lane type, as their bytes.
    fn lanes_of(
        &self,
        arg: &IntervalAndTy<'db>,
        locals: &Locals<'db>,
    ) -> Result<'db, Vec<Vec<u8>>> {
        let (len, lane) = self.detect_simd_ty(arg.ty)?;
        let size = self.size_of_sized(lane, locals, "a lane of a simd vector")?;
        let bytes = arg.get(self)?;
        let (true, Some(bytes)) = (size != 0, bytes.get(..len * size)) else {
            return Err(MirEvalError::InternalError("simd vector smaller than its lanes".into()));
        };
        Ok(bytes.chunks(size).map(<[u8]>::to_vec).collect())
    }

    /// The lanes of a mask, each of which must be all zeros (false) or all ones (true).
    fn read_mask(&self, arg: &IntervalAndTy<'db>, op: SimdOp) -> Result<'db, Vec<bool>> {
        let (lane, values) = self.read_lanes(arg)?;
        if !matches!(lane, Lane::Int { .. }) {
            not_supported!("`{}` with a mask of float lanes", Intrinsic::Simd(op));
        }
        values
            .iter()
            .enumerate()
            .map(|(i, &it)| match it & lane.mask() {
                0 => Ok(false),
                it if it == lane.mask() => Ok(true),
                _ => Err(MirEvalError::UndefinedBehavior(format!(
                    "lane {i} of the mask of `{}` is neither all zeros nor all ones",
                    Intrinsic::Simd(op)
                ))),
            })
            .collect()
    }

    /// The indices of `simd_shuffle`: an array of `u32` or a vector of them.
    fn shuffle_indices(&self, index: &IntervalAndTy<'db>) -> Result<'db, Vec<u32>> {
        let len = match index.ty.kind() {
            TyKind::Array(_, len) => match try_const_usize(self.db, len) {
                Some(it) => it as usize,
                None => {
                    return Err(MirEvalError::InternalError(
                        "simd_shuffle index with unevaluatable len".into(),
                    ));
                }
            },
            _ => self.detect_simd_ty(index.ty)?.0,
        };
        let Some(bytes) = index.get(self)?.get(..len * 4) else {
            return Err(MirEvalError::InternalError(
                "simd_shuffle index smaller than its len".into(),
            ));
        };
        Ok(bytes
            .chunks_exact(4)
            .map(|it| u32::from_le_bytes([it[0], it[1], it[2], it[3]]))
            .collect())
    }

    fn shuffle(
        &mut self,
        op: SimdOp,
        left: &IntervalAndTy<'db>,
        right: &IntervalAndTy<'db>,
        indices: &[u32],
        destination: Interval,
        locals: &Locals<'db>,
    ) -> Result<'db, ()> {
        let mut lanes = self.lanes_of(left, locals)?;
        lanes.extend(self.lanes_of(right, locals)?);
        let mut result = vec![];
        for &index in indices {
            let Some(lane) = lanes.get(index as usize) else {
                return Err(MirEvalError::UndefinedBehavior(format!(
                    "`{}` index {index} is out of bounds of two vectors of {} lanes",
                    Intrinsic::Simd(op),
                    lanes.len() / 2
                )));
            };
            result.extend_from_slice(lane);
        }
        destination.write_from_bytes(self, &result)
    }

    /// Fails unless `ptr` is aligned as the `SimdAlign` argument of a masked load or store asks.
    fn check_simd_align(
        &mut self,
        op: SimdOp,
        ptr: Address,
        vector: Ty<'db>,
        generic_args: GenericArgs<'db>,
        locals: &Locals<'db>,
    ) -> Result<'db, ()> {
        let intrinsic = Intrinsic::Simd(op);
        let Some(align) = generic_args.as_slice().get(3).and_then(|it| it.konst()) else {
            return Err(MirEvalError::InternalError(
                format!("{intrinsic} has no alignment argument").into(),
            ));
        };
        let align = self.allocate_const_in_heap(locals, align)?;
        let align = match align.get(self)? {
            // `SimdAlign::Unaligned`, `Element` and `Vector`.
            [0] => return Ok(()),
            [1] => {
                let (_, lane) = self.detect_simd_ty(vector)?;
                self.size_align_of_sized(lane, locals, "a lane of a simd vector")?.1
            }
            [2] => self.size_align_of_sized(vector, locals, "a simd vector")?.1,
            _ => {
                return Err(MirEvalError::InternalError(
                    format!("the alignment argument of {intrinsic} is not a `SimdAlign`").into(),
                ));
            }
        };
        if !ptr.to_usize().is_multiple_of(align) {
            return Err(MirEvalError::UndefinedBehavior(format!(
                "`{intrinsic}` of a pointer that is not aligned to {align} bytes"
            )));
        }
        Ok(())
    }

    pub(super) fn exec_simd_intrinsic(
        &mut self,
        op: SimdOp,
        args: &[IntervalAndTy<'db>],
        generic_args: GenericArgs<'db>,
        destination: Interval,
        locals: &Locals<'db>,
        _span: MirSpan,
    ) -> Result<'db, ()> {
        match op {
            SimdOp::And | SimdOp::Or | SimdOp::Xor => {
                let [left, right] = args else {
                    return Err(MirEvalError::InternalError(
                        "simd bit op args are not provided".into(),
                    ));
                };
                let result = left
                    .get(self)?
                    .iter()
                    .zip(right.get(self)?)
                    .map(|(&it, &y)| match op {
                        SimdOp::And => it & y,
                        SimdOp::Or => it | y,
                        _ => it ^ y,
                    })
                    .collect::<Vec<_>>();
                destination.write_from_bytes(self, &result)
            }
            SimdOp::Add
            | SimdOp::Sub
            | SimdOp::Mul
            | SimdOp::Div
            | SimdOp::Rem
            | SimdOp::Shl
            | SimdOp::Shr
            | SimdOp::SaturatingAdd
            | SimdOp::SaturatingSub
            | SimdOp::MinimumNumberNsz
            | SimdOp::MaximumNumberNsz
            | SimdOp::CarrylessMul
            | SimdOp::Neg
            | SimdOp::Fabs
            | SimdOp::Fsqrt
            | SimdOp::Ceil
            | SimdOp::Floor
            | SimdOp::Round
            | SimdOp::RoundTiesEven
            | SimdOp::Trunc
            | SimdOp::Bswap
            | SimdOp::Bitreverse
            | SimdOp::Ctlz
            | SimdOp::Cttz
            | SimdOp::Ctpop
            | SimdOp::Fma
            | SimdOp::RelaxedFma
            | SimdOp::FunnelShl
            | SimdOp::FunnelShr => {
                let arity = match op {
                    SimdOp::Neg
                    | SimdOp::Fabs
                    | SimdOp::Fsqrt
                    | SimdOp::Ceil
                    | SimdOp::Floor
                    | SimdOp::Round
                    | SimdOp::RoundTiesEven
                    | SimdOp::Trunc
                    | SimdOp::Bswap
                    | SimdOp::Bitreverse
                    | SimdOp::Ctlz
                    | SimdOp::Cttz
                    | SimdOp::Ctpop => 1,
                    SimdOp::Fma | SimdOp::RelaxedFma | SimdOp::FunnelShl | SimdOp::FunnelShr => 3,
                    _ => 2,
                };
                if args.len() != arity {
                    return Err(simd_args(op));
                }
                let operands =
                    args.iter().map(|it| self.read_lanes(it)).collect::<Result<'_, Vec<_>>>()?;
                let (lane, len) = (operands[0].0, operands[0].1.len());
                for (_, it) in &operands {
                    check_lanes(op, len, it.len())?;
                }
                let mut lane_args = [0u128; 3];
                let result = (0..len)
                    .map(|i| {
                        for (arg, (_, it)) in lane_args.iter_mut().zip(&operands) {
                            *arg = it[i];
                        }
                        lane_op(op, lane, i, &lane_args[..arity])
                    })
                    .collect::<Result<'_, Vec<_>>>()?;
                self.write_lanes(destination, lane, &result)
            }
            SimdOp::Fsin
            | SimdOp::Fcos
            | SimdOp::Fexp
            | SimdOp::Fexp2
            | SimdOp::Flog
            | SimdOp::Flog2
            | SimdOp::Flog10 => not_supported!(
                "`{}`: IEEE 754 does not define the result of a transcendental function \
                 to the last bit, so a constant cannot depend on it",
                Intrinsic::Simd(op)
            ),
            SimdOp::Eq | SimdOp::Ne | SimdOp::Lt | SimdOp::Le | SimdOp::Gt | SimdOp::Ge => {
                let [left, right] = args else {
                    return Err(simd_args(op));
                };
                let (lane, left) = self.read_lanes(left)?;
                let (_, right) = self.read_lanes(right)?;
                let (len, out) = self.simd_lanes(simd_ty_arg(generic_args, 1, op)?)?;
                check_lanes(op, left.len(), right.len())?;
                check_lanes(op, left.len(), len)?;
                let result = left
                    .iter()
                    .zip(&right)
                    .map(|(&l, &r)| {
                        let ordering = compare(lane, l, r);
                        let holds = match op {
                            SimdOp::Eq => ordering == Some(Ordering::Equal),
                            SimdOp::Ne => ordering != Some(Ordering::Equal),
                            SimdOp::Lt => ordering == Some(Ordering::Less),
                            SimdOp::Le => {
                                matches!(ordering, Some(Ordering::Less | Ordering::Equal))
                            }
                            SimdOp::Gt => ordering == Some(Ordering::Greater),
                            _ => matches!(ordering, Some(Ordering::Greater | Ordering::Equal)),
                        };
                        if holds { u128::MAX } else { 0 }
                    })
                    .collect::<Vec<_>>();
                self.write_lanes(destination, out, &result)
            }
            SimdOp::Cast | SimdOp::As => {
                let [vector] = args else {
                    return Err(simd_args(op));
                };
                let (from, values) = self.read_lanes(vector)?;
                let (len, to) = self.simd_lanes(simd_ty_arg(generic_args, 1, op)?)?;
                check_lanes(op, values.len(), len)?;
                let result = values
                    .iter()
                    .enumerate()
                    .map(|(i, &it)| cast_lane(op, from, to, i, it).map(|it| to.extend(it)))
                    .collect::<Result<'_, Vec<_>>>()?;
                self.write_lanes(destination, to, &result)
            }
            SimdOp::CastPtr | SimdOp::ExposeProvenance | SimdOp::WithExposedProvenance => {
                let [vector] = args else {
                    return Err(simd_args(op));
                };
                let bytes = vector.get(self)?.to_vec();
                if bytes.len() != destination.size {
                    return Err(MirEvalError::InternalError(
                        format!(
                            "{} of a vector of {} bytes to one of {}",
                            Intrinsic::Simd(op),
                            bytes.len(),
                            destination.size
                        )
                        .into(),
                    ));
                }
                destination.write_from_bytes(self, &bytes)
            }
            SimdOp::Select => {
                let [mask, yes, no] = args else {
                    return Err(simd_args(op));
                };
                let mask = self.read_mask(mask, op)?;
                let (yes, no) = (self.lanes_of(yes, locals)?, self.lanes_of(no, locals)?);
                check_lanes(op, mask.len(), yes.len())?;
                check_lanes(op, mask.len(), no.len())?;
                let result = mask
                    .iter()
                    .zip(yes.iter().zip(&no))
                    .flat_map(|(&mask, (yes, no))| (if mask { yes } else { no }).iter().copied())
                    .collect::<Vec<_>>();
                destination.write_from_bytes(self, &result)
            }
            SimdOp::SelectBitmask => {
                let [mask, yes, no] = args else {
                    return Err(simd_args(op));
                };
                // An unsigned integer or an array of `u8`, which on a little-endian target are
                // the same bits.
                let mask = mask.get(self)?;
                if mask.len() > 16 {
                    not_supported!("`simd_select_bitmask` with a mask of more than 128 bits");
                }
                let mask = u128::from_le_bytes(pad16(mask, IsSigned::No));
                let (yes, no) = (self.lanes_of(yes, locals)?, self.lanes_of(no, locals)?);
                check_lanes(op, yes.len(), no.len())?;
                if yes.len() > 128 {
                    not_supported!("`simd_select_bitmask` of more than 128 lanes");
                }
                let result = yes
                    .iter()
                    .zip(&no)
                    .enumerate()
                    .flat_map(|(i, (yes, no))| {
                        (if (mask >> i) & 1 == 1 { yes } else { no }).iter().copied()
                    })
                    .collect::<Vec<_>>();
                destination.write_from_bytes(self, &result)
            }
            SimdOp::Bitmask => {
                let [vector] = args else {
                    return Err(simd_args(op));
                };
                let mask = self.read_mask(vector, op)?;
                if mask.len() > 128 || destination.size > 16 {
                    not_supported!("`simd_bitmask` of more than 128 lanes");
                }
                let result = mask
                    .iter()
                    .enumerate()
                    .filter(|&(_, &it)| it)
                    .fold(0u128, |bits, (i, _)| bits | (1 << i));
                destination.write_from_bytes(self, &result.to_le_bytes()[..destination.size])
            }
            SimdOp::Extract | SimdOp::ExtractDyn | SimdOp::Insert | SimdOp::InsertDyn => {
                let (vector, index, value) = match args {
                    [vector, index] => (vector, index, None),
                    [vector, index, value] => (vector, index, Some(value)),
                    _ => return Err(simd_args(op)),
                };
                let mut lanes = self.lanes_of(vector, locals)?;
                let index = from_bytes!(u32, index.get(self)?) as usize;
                let len = lanes.len();
                let Some(lane) = lanes.get_mut(index) else {
                    return Err(MirEvalError::UndefinedBehavior(format!(
                        "`{}` index {index} is out of bounds of a vector of {len} lanes",
                        Intrinsic::Simd(op)
                    )));
                };
                match value {
                    None => {
                        let lane = lane.clone();
                        destination.write_from_bytes(self, &lane)
                    }
                    Some(value) => {
                        *lane = value.get(self)?.to_vec();
                        destination.write_from_bytes(self, &lanes.concat())
                    }
                }
            }
            SimdOp::ReduceAddOrdered
            | SimdOp::ReduceMulOrdered
            | SimdOp::ReduceAddUnordered
            | SimdOp::ReduceMulUnordered
            | SimdOp::ReduceAnd
            | SimdOp::ReduceOr
            | SimdOp::ReduceXor
            | SimdOp::ReduceMin
            | SimdOp::ReduceMax => {
                let (vector, init) = match args {
                    [vector] => (vector, None),
                    [vector, init] => (vector, Some(init)),
                    _ => return Err(simd_args(op)),
                };
                let (lane, values) = self.read_lanes(vector)?;
                let fold = match op {
                    SimdOp::ReduceAddOrdered | SimdOp::ReduceAddUnordered => SimdOp::Add,
                    SimdOp::ReduceMulOrdered | SimdOp::ReduceMulUnordered => SimdOp::Mul,
                    SimdOp::ReduceAnd => SimdOp::And,
                    SimdOp::ReduceOr => SimdOp::Or,
                    SimdOp::ReduceXor => SimdOp::Xor,
                    _ => op,
                };
                let mut values = values.into_iter();
                let mut result = match init {
                    Some(init) => lane.read(init.get(self)?),
                    None => values.next().ok_or_else(|| {
                        MirEvalError::InternalError(
                            format!("{} of a vector with no lanes", Intrinsic::Simd(op)).into(),
                        )
                    })?,
                };
                for (i, value) in values.enumerate() {
                    result = lane_op(fold, lane, i, &[result, value])?;
                }
                self.write_lanes(destination, lane, &[result])
            }
            SimdOp::ReduceAll | SimdOp::ReduceAny => {
                let [vector] = args else {
                    return Err(simd_args(op));
                };
                let mask = self.read_mask(vector, op)?;
                let result = if op == SimdOp::ReduceAll {
                    mask.iter().all(|&it| it)
                } else {
                    mask.iter().any(|&it| it)
                };
                destination.write_from_bytes(self, &[u8::from(result)])
            }
            SimdOp::Shuffle => {
                let [left, right, index] = args else {
                    return Err(simd_args(op));
                };
                let indices = self.shuffle_indices(index)?;
                self.shuffle(op, left, right, &indices, destination, locals)
            }
            SimdOp::Splat => {
                let [value] = args else {
                    return Err(MirEvalError::InternalError(
                        "simd_splat args are not provided".into(),
                    ));
                };
                let value = value.get(self)?;
                if value.is_empty() {
                    return Ok(());
                }
                let result = value.repeat(destination.size / value.len());
                destination.write_from_bytes(self, &result)
            }
            SimdOp::Gather | SimdOp::Scatter => {
                let [values, ptrs, mask] = args else {
                    return Err(simd_args(op));
                };
                let mut lanes = self.lanes_of(values, locals)?;
                let (_, ptrs) = self.read_lanes(ptrs)?;
                let mask = self.read_mask(mask, op)?;
                check_lanes(op, lanes.len(), ptrs.len())?;
                check_lanes(op, lanes.len(), mask.len())?;
                let gather = op == SimdOp::Gather;
                for ((lane, &ptr), &enabled) in lanes.iter_mut().zip(&ptrs).zip(&mask) {
                    if !enabled {
                        continue;
                    }
                    let addr = Address::from_usize(ptr as usize);
                    if gather {
                        *lane = self.read_memory(addr, lane.len())?.to_vec();
                    } else {
                        self.write_memory(addr, lane)?;
                    }
                }
                if gather {
                    destination.write_from_bytes(self, &lanes.concat())?;
                }
                Ok(())
            }
            SimdOp::MaskedLoad | SimdOp::MaskedStore => {
                let [mask, ptr, values] = args else {
                    return Err(simd_args(op));
                };
                let mask = self.read_mask(mask, op)?;
                let base = Address::from_bytes(ptr.get(self)?)?;
                let mut lanes = self.lanes_of(values, locals)?;
                check_lanes(op, lanes.len(), mask.len())?;
                self.check_simd_align(op, base, values.ty, generic_args, locals)?;
                let load = op == SimdOp::MaskedLoad;
                for (i, (lane, &enabled)) in lanes.iter_mut().zip(&mask).enumerate() {
                    if !enabled {
                        continue;
                    }
                    let addr = base.offset(i * lane.len());
                    if load {
                        *lane = self.read_memory(addr, lane.len())?.to_vec();
                    } else {
                        self.write_memory(addr, lane)?;
                    }
                }
                if load {
                    destination.write_from_bytes(self, &lanes.concat())?;
                }
                Ok(())
            }
            SimdOp::ArithOffset => {
                let [ptrs, offsets] = args else {
                    return Err(simd_args(op));
                };
                let (lane, values) = self.read_lanes(ptrs)?;
                let (_, counts) = self.read_lanes(offsets)?;
                check_lanes(op, values.len(), counts.len())?;
                let (_, ptr_ty) = self.detect_simd_ty(ptrs.ty)?;
                let TyKind::RawPtr(pointee, _) = ptr_ty.kind() else {
                    return Err(MirEvalError::InternalError(
                        "simd_arith_offset of a vector that is not of pointers".into(),
                    ));
                };
                let size =
                    self.size_of_sized(pointee, locals, "the pointee of simd_arith_offset")?;
                // The offsets are `isize` or `usize`, both counted as `isize`.
                let shift = 128 - lane.bits();
                let result = values
                    .iter()
                    .zip(&counts)
                    .map(|(&ptr, &count)| {
                        let count = ((count << shift) as i128) >> shift;
                        ptr.wrapping_add(count.wrapping_mul(size as i128) as u128)
                    })
                    .collect::<Vec<_>>();
                self.write_lanes(destination, lane, &result)
            }
        }
    }
}

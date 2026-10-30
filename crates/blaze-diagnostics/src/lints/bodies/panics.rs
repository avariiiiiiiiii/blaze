//! `arithmetic_overflow` and `unconditional_panic`: what rustc's known-panics lint finds, by
//! propagating the constants of a body's MIR after drop elaboration: an `Assert` it knows
//! fails, an arithmetic operation it knows overflows, a shift by as many bits as its left side
//! has or more, and a call with a `#[rustc_panics_when_zero]` const parameter of 0.
//!
//! Follows `compiler/rustc_mir_transform/src/known_panics_lint.rs` (`KnownPanicsLint::run_lint`:
//! the bodies it runs on; `ConstPropagator`: `check_unary_op`, `check_binary_op`,
//! `check_rvalue`, `check_assertion`, `eval_rvalue`, `visit_assign`, `visit_statement`,
//! `visit_terminator`, `visit_basic_block_data`; `Value::project`; `CanConstProp`),
//! `compiler/rustc_mir_transform/src/diagnostics.rs` (`AssertLint`, `ConstNIsZero`),
//! `compiler/rustc_middle/src/mir/terminator.rs` (`AssertKind`'s `Display`) and
//! `compiler/rustc_mir_transform/src/lib.rs` (the lint runs after `ElaborateDrops` and
//! `CleanupPostBorrowck`, which removes fake borrows) (MIT or Apache-2.0).
//!
//! rustc's interpreter gives the values; here [`scalar`] does, for what the lint can know: a
//! value of a primitive type, and of an enum whose layout is a scalar or a pair, the variant.
//! A constant is evaluated by const evaluation (`const_eval`, `anon_const_eval`). What is not
//! known here that rustc knows: `line!()` and `column!()`, whose values MIR leaves for the
//! code generator to place, and `f16` and `f128` arithmetic.

mod scalar;

use std::fmt;

use either::Either;
use hir::InFile;
use hir_def::{
    AdtId, CallableDefId, ConstParamId, DefWithBodyId, EnumVariantId, GenericDefId, GenericParamId, ItemContainerId,
    Lookup, VariantId, hir::ExprId, layout::BackendRepr, src::HasChildSource as _,
};
use hir_expand::{MacroDefKind, builtin::BuiltinFnLikeExpander};
use hir_ty::{
    InferBodyId, ParamEnvAndCrate,
    consteval::try_const_usize,
    db::{GeneralConstId, HirDatabase},
    method_resolution::lookup_impl_const,
    mir::{
        AggregateKind, AsmOperand, AssertKind, BasicBlockId, BinOp, BorrowKind, CastKind, LocalId, MirBody, MirSpan,
        Operand, OperandKind, PlaceTy, ProjectionElem, Rvalue, StatementKind, StoredPlace, Terminator, TerminatorKind,
        UnOp,
    },
    next_solver::{
        CallableIdWrapper, Const, ConstKind, DbInterner, GenericArgKind, ParamEnv, Ty, TyKind, TypingMode,
        infer::{DbInternerInferExt, InferCtxt},
    },
    utils::detect_variant_from_bytes,
};
use hir_ty_borrowck::NotChecked;
use rustc_hash::FxHashSet;
use rustc_type_ir::{
    FloatTy, IntTy, TypeVisitableExt as _, UintTy,
    inherent::{IntoKind as _, SliceLike as _},
};
use syntax::{
    AstNode as _, SyntaxNodePtr,
    ast::{self, HasAttrs as _, HasName as _},
};

use self::scalar::{Kind, Output, Scalar};
use super::BodyCx;
use crate::{
    cx::def_range,
    lower::{Found, Phase},
    model::{Diag, Span},
};

/// The known-panics lint of the body and of the closures in it, each closure after the body
/// it is in, in the order they are written. None of a body rustc builds no runtime MIR of
/// before it aborts, or whose type checking or borrow checking failed.
pub(super) fn lints(b: &BodyCx<'_, '_>) -> Vec<Found> {
    if b.tainted || !b.elaborated || borrowck_failed(b) {
        return Vec::new();
    }
    let db = b.db();
    let mut out = Vec::new();
    let closures = match runs_on(b) {
        true => {
            let Some(body) = b.runtime_mir() else { return Vec::new() };
            if body.coroutine.is_none() {
                Propagator::new(b, body, &mut out).run();
            }
            body.closures.clone()
        }
        false => b.mir().map(|it| it.closures.clone()).unwrap_or_default(),
    };
    let mut stack: Vec<_> = closures.into_iter().rev().collect();
    while let Some(closure) = stack.pop() {
        let Ok(body) = db.runtime_mir_body_for_closure(closure) else { continue };
        if body.coroutine.is_none() {
            Propagator::new(b, body, &mut out).run();
        }
        stack.extend(body.closures.iter().rev().copied());
    }
    out
}

/// `run_lint`'s kinds of body: functions, methods and associated constants. rustc leaves
/// other constants, statics and discriminants to const evaluation.
fn runs_on(b: &BodyCx<'_, '_>) -> bool {
    match b.owner {
        DefWithBodyId::FunctionId(_) => true,
        DefWithBodyId::ConstId(it) => {
            matches!(it.lookup(b.db()).container, ItemContainerId::ImplId(_) | ItemContainerId::TraitId(_))
        }
        DefWithBodyId::StaticId(_) | DefWithBodyId::VariantId(_) => false,
    }
}

/// rustc's `tainted_by_errors` from borrow checking the body and its closures, and from what
/// stops it before (const checking, a lowering error).
fn borrowck_failed(b: &BodyCx<'_, '_>) -> bool {
    let result = hir_ty_borrowck::borrowck(b.db(), b.owner);
    result.diagnostics.iter().any(|it| it.is_error())
        || result.not_checked.is_some_and(|it| it.refuses() && it != NotChecked::TypeErrors)
}

/// Which lint a finding is of.
#[derive(Clone, Copy)]
enum Lint {
    ArithmeticOverflow,
    UnconditionalPanic,
}

impl Lint {
    fn name(self) -> &'static str {
        match self {
            Lint::ArithmeticOverflow => "arithmetic_overflow",
            Lint::UnconditionalPanic => "unconditional_panic",
        }
    }

    fn message(self) -> &'static str {
        match self {
            Lint::ArithmeticOverflow => "this arithmetic operation will overflow",
            Lint::UnconditionalPanic => "this operation will panic at runtime",
        }
    }
}

/// rustc's `AssertKind` as the lint labels it, each value known or `_`.
enum Panic {
    BoundsCheck { len: Option<Scalar>, index: Option<Scalar> },
    Shl(Option<Scalar>),
    Shr(Option<Scalar>),
    Overflow { op: &'static str, l: Option<Scalar>, r: Option<Scalar> },
    OverflowNeg(Option<Scalar>),
    DivisionByZero(Option<Scalar>),
    RemainderByZero(Option<Scalar>),
}

impl Panic {
    /// `AssertKind::Overflow`, of an operation that can overflow.
    fn overflow(op: &BinOp, l: Option<Scalar>, r: Option<Scalar>) -> Option<Panic> {
        let op = match op {
            BinOp::Shl => return Some(Panic::Shl(r)),
            BinOp::Shr => return Some(Panic::Shr(r)),
            BinOp::Add | BinOp::AddWithOverflow => "+",
            BinOp::Sub | BinOp::SubWithOverflow => "-",
            BinOp::Mul | BinOp::MulWithOverflow => "*",
            BinOp::Div => "/",
            BinOp::Rem => "%",
            _ => return None,
        };
        Some(Panic::Overflow { op, l, r })
    }
}

impl fmt::Display for Panic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let int = |value: &Option<Scalar>, alternate: bool| match value {
            Some(it) => scalar::const_int(*it, alternate),
            None => "_".to_owned(),
        };
        match self {
            Panic::BoundsCheck { len, index } => write!(
                f,
                "index out of bounds: the length is {} but the index is {}",
                int(len, false),
                int(index, false)
            ),
            Panic::Shl(by) => write!(f, "attempt to shift left by `{}`, which would overflow", int(by, true)),
            Panic::Shr(by) => write!(f, "attempt to shift right by `{}`, which would overflow", int(by, true)),
            Panic::Overflow { op, l, r } => {
                write!(f, "attempt to compute `{} {op} {}`, which would overflow", int(l, true), int(r, true))
            }
            Panic::OverflowNeg(value) => write!(f, "attempt to negate `{}`, which would overflow", int(value, true)),
            Panic::DivisionByZero(value) => write!(f, "attempt to divide `{}` by zero", int(value, true)),
            Panic::RemainderByZero(value) => {
                write!(f, "attempt to calculate the remainder of `{}` with a divisor of zero", int(value, true))
            }
        }
    }
}

/// rustc's `Value`: what the propagation knows a local holds.
#[derive(Clone)]
enum Value<'db> {
    Immediate(Imm<'db>),
    /// The variant is `None` but of an enum.
    Aggregate { variant: Option<EnumVariantId>, fields: Vec<Value<'db>> },
    Uninit,
}

/// rustc's `ImmTy` as far as the lint reads one: a primitive value, or an enum whose layout
/// is a scalar or a pair, by its bytes.
#[derive(Clone)]
enum Imm<'db> {
    Scalar(Scalar),
    Enum { ty: Ty<'db>, bytes: Box<[u8]> },
}

impl<'db> Value<'db> {
    fn immediate(&self) -> Option<&Imm<'db>> {
        match self {
            Value::Immediate(it) => Some(it),
            _ => None,
        }
    }
}

/// rustc's `ConstPropMode`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    FullConstProp,
    OnlyInsideOwnBlock,
    NoPropagation,
}

/// How `CanConstProp` takes a use of a local.
enum Use {
    Store,
    Read,
    /// A borrow, a drop or a `yield`'s resume argument: what the propagation cannot follow.
    Escape,
}

/// rustc's `ConstPropagator` over one body.
struct Propagator<'a, 'b, 'db> {
    b: &'a BodyCx<'b, 'db>,
    db: &'db dyn HirDatabase,
    body: &'db MirBody<'db>,
    infcx: InferCtxt<'db>,
    env: ParamEnv<'db>,
    krate: base_db::Crate,
    pointer_size: Option<u8>,
    worklist: Vec<BasicBlockId>,
    visited: FxHashSet<BasicBlockId>,
    locals: Vec<Value<'db>>,
    modes: Vec<Mode>,
    written_only_inside_own_block: FxHashSet<LocalId>,
    uninit: Value<'db>,
    out: &'a mut Vec<Found>,
}

fn index(local: LocalId) -> usize {
    local.into_raw().into_u32() as usize
}

fn is_indirect(place: &StoredPlace) -> bool {
    place.projection.as_slice().iter().any(|it| matches!(it, ProjectionElem::Deref))
}

impl<'a, 'b, 'db> Propagator<'a, 'b, 'db> {
    fn new(b: &'a BodyCx<'b, 'db>, body: &'db MirBody<'db>, out: &'a mut Vec<Found>) -> Self {
        let db = b.db();
        let krate = b.cx.krate.base();
        let infcx = DbInterner::new_with(db, krate).infer_ctxt().build(TypingMode::non_body_analysis());
        let env = db.trait_environment(b.owner.generic_def(db));
        let pointer_size = db.target_data_layout(krate).ok().map(|it| it.pointer_size().bytes() as u8);
        let mut propagator = Propagator {
            b,
            db,
            body,
            infcx,
            env,
            krate,
            pointer_size,
            worklist: vec![body.start_block],
            visited: FxHashSet::default(),
            locals: vec![Value::Uninit; body.locals.len()],
            modes: Vec::new(),
            written_only_inside_own_block: FxHashSet::default(),
            uninit: Value::Uninit,
            out,
        };
        propagator.modes = propagator.can_const_prop();
        propagator
    }

    /// `visit_body`: the blocks from the start, the last found first, each once.
    fn run(&mut self) {
        while let Some(block) = self.worklist.pop() {
            if !self.visited.insert(block) {
                continue;
            }
            let data = &self.body.basic_blocks[block];
            for statement in &data.statements {
                match &statement.kind {
                    StatementKind::Assign(_, Rvalue::Ref(BorrowKind::Shallow, _)) => {}
                    StatementKind::Assign(place, rvalue) => self.assign(place, rvalue, statement.span),
                    StatementKind::StorageLive(local) | StatementKind::StorageDead(local) => self.remove_const(*local),
                    StatementKind::Deinit(place) if !is_indirect(place) => self.remove_const(place.local),
                    StatementKind::InlineAsm(asm) => {
                        let outputs = asm.operands.iter().filter_map(|it| match it {
                            AsmOperand::Reg { output, .. } => output.as_ref(),
                            _ => None,
                        });
                        for place in outputs.chain(&asm.taken) {
                            if !is_indirect(place) {
                                self.remove_const(place.local);
                            }
                        }
                    }
                    StatementKind::Deinit(_) | StatementKind::FakeRead(_) | StatementKind::Nop => {}
                }
            }
            if let Some(terminator) = &data.terminator {
                self.terminator(terminator);
            }
            for local in std::mem::take(&mut self.written_only_inside_own_block) {
                self.remove_const(local);
            }
        }
    }

    /// `CanConstProp::check`: a local of a union, of a type without a layout or of 1024 bytes
    /// or more is never known; one assigned more than once (a parameter is on entry) is known
    /// only in the block it is assigned in; one borrowed or dropped is never known.
    fn can_const_prop(&self) -> Vec<Mode> {
        let body = self.body;
        let mut modes: Vec<Mode> = body
            .locals
            .iter()
            .map(|(_, local)| {
                let ty = local.ty.as_ref();
                match !ty.is_union() && self.layout_size(ty).is_some_and(|it| it < 1024) {
                    true => Mode::FullConstProp,
                    false => Mode::NoPropagation,
                }
            })
            .collect();
        let mut assigned = vec![false; modes.len()];
        for &param in &body.param_locals {
            assigned[index(param)] = true;
        }
        let mut place = |place: &StoredPlace, mut use_: Use| {
            if let Some(ProjectionElem::Deref) = place.projection.as_slice().first() {
                use_ = Use::Read;
            }
            let at = index(place.local);
            match use_ {
                Use::Store if std::mem::replace(&mut assigned[at], true) => {
                    if modes[at] == Mode::FullConstProp {
                        modes[at] = Mode::OnlyInsideOwnBlock;
                    }
                }
                Use::Escape => modes[at] = Mode::NoPropagation,
                Use::Store | Use::Read => {}
            }
        };
        for (_, block) in body.basic_blocks.iter() {
            for statement in &block.statements {
                match &statement.kind {
                    StatementKind::Assign(_, Rvalue::Ref(BorrowKind::Shallow, _)) => {}
                    StatementKind::Assign(to, rvalue) => {
                        place(to, Use::Store);
                        if let Rvalue::Ref(_, borrowed) = rvalue {
                            place(borrowed, Use::Escape);
                        }
                    }
                    StatementKind::InlineAsm(asm) => {
                        for operand in asm.operands.iter() {
                            if let AsmOperand::Reg { output: Some(output), .. } = operand {
                                place(output, Use::Store);
                            }
                        }
                        if let Some(taken) = &asm.taken {
                            place(taken, Use::Store);
                        }
                    }
                    StatementKind::FakeRead(_)
                    | StatementKind::Deinit(_)
                    | StatementKind::StorageLive(_)
                    | StatementKind::StorageDead(_)
                    | StatementKind::Nop => {}
                }
            }
            match block.terminator.as_ref().map(|it| &it.kind) {
                Some(TerminatorKind::Drop { place: dropped, .. } | TerminatorKind::DropAndReplace { place: dropped, .. }) => {
                    place(dropped, Use::Escape)
                }
                Some(TerminatorKind::Call { destination, .. }) => place(destination, Use::Store),
                Some(TerminatorKind::Yield { resume_arg, .. }) => place(resume_arg, Use::Escape),
                _ => {}
            }
        }
        modes
    }

    fn remove_const(&mut self, local: LocalId) {
        self.locals[index(local)] = Value::Uninit;
        self.written_only_inside_own_block.remove(&local);
    }

    fn access_mut(&mut self, local: LocalId) -> Option<&mut Value<'db>> {
        match self.modes[index(local)] {
            Mode::NoPropagation => return None,
            Mode::OnlyInsideOwnBlock => {
                self.written_only_inside_own_block.insert(local);
            }
            Mode::FullConstProp => {}
        }
        Some(&mut self.locals[index(local)])
    }

    /// `visit_assign`.
    fn assign(&mut self, place: &StoredPlace, rvalue: &Rvalue, span: MirSpan) {
        if self.check_rvalue(rvalue, span).is_none() || is_indirect(place) {
            return;
        }
        if self.modes[index(place.local)] != Mode::NoPropagation && self.eval_rvalue(rvalue, place).is_none() {
            self.remove_const(place.local);
        }
    }

    /// `check_rvalue`: the lints of a unary or binary operation; `None` to keep the result
    /// unknown, as of a borrow (whose local is then unknown) or of what names a generic
    /// parameter.
    fn check_rvalue(&mut self, rvalue: &Rvalue, span: MirSpan) -> Option<()> {
        match rvalue {
            Rvalue::UnaryOp(op, arg) => self.check_unary_op(*op, arg, span)?,
            Rvalue::BinaryOp(op, left, right) => self.check_binary_op(op, left, right, span)?,
            Rvalue::Ref(_, place) => {
                self.remove_const(place.local);
                return None;
            }
            Rvalue::CheckedBinaryOp(..) => return None,
            Rvalue::ThreadLocalRef(never) | Rvalue::AddressOf(never) | Rvalue::NullaryOp(never) => match *never {},
            Rvalue::Use(_)
            | Rvalue::Repeat(..)
            | Rvalue::Len(_)
            | Rvalue::Cast(..)
            | Rvalue::Discriminant(_)
            | Rvalue::Aggregate(..)
            | Rvalue::CopyForDeref(_) => {}
        }
        match self.rvalue_has_param(rvalue) {
            true => None,
            false => Some(()),
        }
    }

    /// `check_unary_op`: `-x` of an integer that is its type's minimum.
    fn check_unary_op(&mut self, op: UnOp, arg: &Operand, span: MirSpan) -> Option<()> {
        let arg = self.eval_operand(arg)?;
        if let (UnOp::Neg, Imm::Scalar(arg)) = (op, arg)
            && arg.is_integral()
        {
            let zero = Scalar::new(0, arg.size, arg.kind);
            if scalar::overflows(&BinOp::SubWithOverflow, zero, arg)? {
                self.report(span, Lint::ArithmeticOverflow, Panic::OverflowNeg(Some(arg)));
                return None;
            }
        }
        Some(())
    }

    /// `check_binary_op`: a shift by the width of its left side or more, even when the left
    /// side is not known, and an addition, subtraction or multiplication that overflows.
    fn check_binary_op(&mut self, op: &BinOp, left: &Operand, right: &Operand, span: MirSpan) -> Option<()> {
        let r = self.eval_operand(right);
        let l = self.eval_operand(left);
        if let BinOp::Shl | BinOp::Shr = op {
            let r = r.clone()?;
            let left_bits = u128::from(self.scalar_kind(self.operand_ty(left)?)?.1) * 8;
            if let Imm::Scalar(by) = r
                && by.bits >= left_bits
            {
                self.report(span, Lint::ArithmeticOverflow, Panic::overflow(op, None, Some(by))?);
                return None;
            }
        }
        let op = match op {
            BinOp::Add => &BinOp::AddWithOverflow,
            BinOp::Sub => &BinOp::SubWithOverflow,
            BinOp::Mul => &BinOp::MulWithOverflow,
            op => op,
        };
        if let (Some(Imm::Scalar(l)), Some(Imm::Scalar(r))) = (l, r)
            && l.is_integral()
            && matches!(op, BinOp::AddWithOverflow | BinOp::SubWithOverflow | BinOp::MulWithOverflow)
            && scalar::overflows(op, l, r)?
        {
            self.report(span, Lint::ArithmeticOverflow, Panic::overflow(op, Some(l), Some(r))?);
            return None;
        }
        Some(())
    }

    /// `check_assertion`: an `Assert` whose condition is known not to be what it expects, of
    /// a bounds check or a division (the overflows are linted at their operation).
    fn check_assertion(&mut self, expected: bool, msg: &AssertKind, cond: &Operand, span: MirSpan) {
        let Some(Imm::Scalar(value)) = self.eval_operand(cond) else { return };
        if value.size == 1 && value.bits == u128::from(expected) {
            return;
        }
        if let OperandKind::Copy(place) | OperandKind::Move(place) = &cond.kind {
            self.remove_const(place.local);
        }
        let panic = match msg {
            AssertKind::DivisionByZero(op) => Panic::DivisionByZero(self.eval_to_int(op)),
            AssertKind::RemainderByZero(op) => Panic::RemainderByZero(self.eval_to_int(op)),
            AssertKind::Overflow(op @ (BinOp::Div | BinOp::Rem), l, r) => {
                let Some(panic) = Panic::overflow(op, self.eval_to_int(l), self.eval_to_int(r)) else { return };
                panic
            }
            AssertKind::BoundsCheck { len, index } => {
                Panic::BoundsCheck { len: self.eval_to_int(len), index: self.eval_to_int(index) }
            }
            AssertKind::Overflow(..) | AssertKind::OverflowNeg(_) => return,
        };
        self.report(span, Lint::UnconditionalPanic, panic);
    }

    fn eval_to_int(&mut self, op: &Operand) -> Option<Scalar> {
        match self.eval_operand(op)? {
            Imm::Scalar(it) => Some(it),
            Imm::Enum { .. } => None,
        }
    }

    /// `eval_rvalue`: the value an assignment to a whole local gives it, if known.
    fn eval_rvalue(&mut self, rvalue: &Rvalue, dest: &StoredPlace) -> Option<()> {
        if !dest.projection.is_empty() {
            return None;
        }
        let dest_ty = self.body.locals[dest.local].ty.as_ref();
        self.layout_size(dest_ty)?;
        let value = match rvalue {
            Rvalue::Use(operand) => Value::Immediate(self.eval_operand(operand)?),
            Rvalue::CopyForDeref(place) => Value::Immediate(self.eval_place(place)?),
            Rvalue::BinaryOp(op, left, right) => {
                let (Imm::Scalar(l), Imm::Scalar(r)) = (self.eval_operand(left)?, self.eval_operand(right)?) else {
                    return None;
                };
                match scalar::binary_op(op, l, r)? {
                    Output::One(value) => Value::Immediate(Imm::Scalar(value)),
                    Output::Pair(value, overflowed) => Value::Aggregate {
                        variant: None,
                        fields: vec![
                            Value::Immediate(Imm::Scalar(value)),
                            Value::Immediate(Imm::Scalar(Scalar::from_bool(overflowed))),
                        ],
                    },
                }
            }
            Rvalue::UnaryOp(op, operand) => {
                let Imm::Scalar(value) = self.eval_operand(operand)? else { return None };
                Value::Immediate(Imm::Scalar(scalar::unary_op(*op, value)?))
            }
            Rvalue::Aggregate(kind, operands) => Value::Aggregate {
                fields: operands.iter().map(|it| self.eval_operand(it).map_or(Value::Uninit, Value::Immediate)).collect(),
                variant: match kind {
                    AggregateKind::Adt(VariantId::EnumVariantId(it), _) => Some(*it),
                    _ => None,
                },
            },
            Rvalue::Cast(kind, operand, to) => {
                let Imm::Scalar(value) = self.eval_operand(operand)? else { return None };
                let (to, size) = self.scalar_kind(to.as_ref())?;
                let value = match kind {
                    CastKind::IntToInt | CastKind::IntToFloat => scalar::int_cast(value, to, size)?,
                    CastKind::FloatToFloat | CastKind::FloatToInt => scalar::float_cast(value, to, size)?,
                    _ => return None,
                };
                Value::Immediate(Imm::Scalar(value))
            }
            Rvalue::Discriminant(place) => {
                let variant = match self.get_const(place)? {
                    Value::Immediate(Imm::Scalar(_)) => None,
                    Value::Immediate(Imm::Enum { ty, bytes }) => Some(self.read_discriminant(*ty, bytes)?),
                    Value::Aggregate { variant, .. } => *variant,
                    Value::Uninit => return None,
                };
                let bits = match variant {
                    Some(it) => self.db.const_eval_discriminant(it).ok()? as u128,
                    None => 0,
                };
                let (kind, size) = self.scalar_kind(dest_ty)?;
                Value::Immediate(Imm::Scalar(Scalar::new(bits, size, kind)))
            }
            Rvalue::Repeat(..) | Rvalue::Ref(..) | Rvalue::Len(_) | Rvalue::CheckedBinaryOp(..) => return None,
            Rvalue::ThreadLocalRef(never) | Rvalue::AddressOf(never) | Rvalue::NullaryOp(never) => match *never {},
        };
        *self.access_mut(dest.local)? = value;
        Some(())
    }

    /// `read_discriminant` of an enum value held as its bytes.
    fn read_discriminant(&self, ty: Ty<'db>, bytes: &[u8]) -> Option<EnumVariantId> {
        let TyKind::Adt(def, _) = ty.kind() else { return None };
        let AdtId::EnumId(enum_) = def.def_id() else { return None };
        let layout = self.db.layout_of_ty(ty.store(), self.env_and_crate().store()).ok()?;
        if matches!(layout.variants, hir_def::layout::Variants::Empty) || layout.size.bytes() != bytes.len() as u64 {
            return None;
        }
        let target = self.db.target_data_layout(self.krate).ok()?;
        detect_variant_from_bytes(&layout, self.db, target, bytes, enum_).map(|(it, _)| it)
    }

    /// `visit_terminator`: an `Assert`'s check, a call's const parameters, and the blocks to
    /// go on with: of a `SwitchInt` on a known value, only the one it goes to.
    fn terminator(&mut self, terminator: &Terminator) {
        match &terminator.kind {
            TerminatorKind::Assert { cond, expected, msg, .. } => {
                self.check_assertion(*expected, msg, cond, terminator.span)
            }
            TerminatorKind::SwitchInt { discr, targets } => {
                if let Some(Imm::Scalar(value)) = self.eval_operand(discr) {
                    self.worklist.push(targets.target_for_value(value.bits));
                    return;
                }
            }
            TerminatorKind::Call { func, .. } => self.check_const_params(func, terminator.span),
            _ => {}
        }
        self.worklist.extend(terminator.kind.successors());
        if !matches!(terminator.kind, TerminatorKind::FalseUnwind { .. }) {
            self.worklist.extend(terminator.kind.unwind());
        }
    }

    /// `ConstNIsZero`: a call of a function with a `#[rustc_panics_when_zero]` const
    /// parameter, given 0.
    fn check_const_params(&mut self, func: &Operand, span: MirSpan) {
        let ty = match &func.kind {
            OperandKind::Constant { ty, .. } => ty.as_ref(),
            OperandKind::Allocation { allocation } => allocation.as_ref().ty,
            _ => return,
        };
        let TyKind::FnDef(CallableIdWrapper(callable), args) = ty.kind() else { return };
        let def: GenericDefId = match callable {
            CallableDefId::FunctionId(it) => it.into(),
            CallableDefId::StructId(it) => AdtId::from(it).into(),
            CallableDefId::EnumVariantId(it) => AdtId::from(it.lookup(self.db).parent).into(),
        };
        let generics = hir_ty::generics::generics(self.db, def);
        for ((param, _), arg) in generics.iter(false).zip(args.iter()) {
            let (GenericParamId::ConstParamId(param), GenericArgKind::Const(value)) = (param, arg.kind()) else {
                continue;
            };
            if try_const_usize(self.db, value) != Some(0) {
                continue;
            }
            let Some(name) = panics_when_zero(self.db, param) else { continue };
            self.emit(span, Lint::UnconditionalPanic, format!("const parameter `{name}` is zero"));
        }
    }

    /// `eval_operand`: the value of an operand, if known.
    fn eval_operand(&self, op: &Operand) -> Option<Imm<'db>> {
        match &op.kind {
            OperandKind::Copy(place) | OperandKind::Move(place) => self.eval_place(place),
            OperandKind::Constant { konst, ty } => self.eval_constant(konst.as_ref(), ty.as_ref()),
            OperandKind::Allocation { allocation } => {
                if let Some(MirSpan::ExprId(expr)) = op.span
                    && self.is_placed(expr)
                {
                    return None;
                }
                let allocation = allocation.as_ref();
                if allocation.ty.has_non_region_param() {
                    return None;
                }
                self.imm(allocation.ty, &allocation.memory)
            }
            OperandKind::Static(_) => None,
        }
    }

    /// `eval_constant`: a constant evaluated, unless it names a generic parameter.
    fn eval_constant(&self, konst: Const<'db>, ty: Ty<'db>) -> Option<Imm<'db>> {
        if konst.has_non_region_param() || ty.has_non_region_param() {
            return None;
        }
        let allocation = match konst.kind() {
            ConstKind::Value(value) => {
                let leaf = value.try_to_leaf()?;
                let (kind, size) = self.scalar_kind(ty)?;
                if u64::from(size) != leaf.size().bytes() {
                    return None;
                }
                return Some(Imm::Scalar(Scalar::new(leaf.to_bits_unchecked(), size, kind)));
            }
            ConstKind::Unevaluated(unevaluated) => match unevaluated.def.0 {
                GeneralConstId::ConstId(id) => {
                    let (id, args) = lookup_impl_const(&self.infcx, self.env, id, unevaluated.args);
                    self.db.const_eval(id, args, None).ok()?
                }
                GeneralConstId::AnonConstId(id) => self.db.anon_const_eval(id, unevaluated.args, None).ok()?,
                GeneralConstId::StaticId(_) => return None,
            },
            _ => return None,
        };
        self.imm(ty, &allocation.memory)
    }

    /// The value of `ty` whose bytes are `memory`, if it is one the lint reads.
    fn imm(&self, ty: Ty<'db>, memory: &[u8]) -> Option<Imm<'db>> {
        if let Some((kind, size)) = self.scalar_kind(ty) {
            if memory.len() != usize::from(size) {
                return None;
            }
            let mut bytes = [0; 16];
            bytes[..memory.len()].copy_from_slice(memory);
            return Some(Imm::Scalar(Scalar::new(u128::from_le_bytes(bytes), size, kind)));
        }
        let TyKind::Adt(def, _) = ty.kind() else { return None };
        if !def.is_enum() {
            return None;
        }
        let layout = self.db.layout_of_ty(ty.store(), self.env_and_crate().store()).ok()?;
        let pair = matches!(layout.backend_repr, BackendRepr::Scalar(_) | BackendRepr::ScalarPair { .. });
        (pair && layout.size.bytes() == memory.len() as u64).then(|| Imm::Enum { ty, bytes: memory.into() })
    }

    /// `eval_place`: the value of a place, if known and not an aggregate.
    fn eval_place(&self, place: &StoredPlace) -> Option<Imm<'db>> {
        self.get_const(place)?.immediate().cloned()
    }

    /// `get_const`: what is known of a place, through the fields and elements of aggregates.
    fn get_const(&self, place: &StoredPlace) -> Option<&Value<'db>> {
        let mut value = &self.locals[index(place.local)];
        for elem in place.projection.as_slice() {
            let Value::Aggregate { fields, .. } = value else { return None };
            let at = match elem {
                ProjectionElem::Field(field) => field.0 as usize,
                ProjectionElem::Index(local) => match self.locals[index(*local)].immediate()? {
                    Imm::Scalar(it) if Some(it.size) == self.pointer_size && u32::try_from(it.bits).is_ok() => {
                        it.bits as usize
                    }
                    _ => return None,
                },
                ProjectionElem::ConstantIndex { offset, from_end: false, .. } => usize::try_from(*offset).ok()?,
                _ => return None,
            };
            value = fields.get(at).unwrap_or(&self.uninit);
        }
        Some(value)
    }

    /// `rvalue.has_param()`: a type or constant in it, or the type of a field it reads, names
    /// a generic parameter.
    fn rvalue_has_param(&self, rvalue: &Rvalue) -> bool {
        let mut found = false;
        rvalue.for_each_operand(&mut |it| found |= self.operand_has_param(it));
        found
            || match rvalue {
                Rvalue::Repeat(_, count) => count.as_ref().has_non_region_param(),
                Rvalue::Ref(_, place) | Rvalue::Len(place) | Rvalue::Discriminant(place) | Rvalue::CopyForDeref(place) => {
                    self.place_has_param(place)
                }
                Rvalue::Cast(_, _, ty) => ty.as_ref().has_non_region_param(),
                Rvalue::Aggregate(kind, _) => match kind {
                    AggregateKind::Array(ty) | AggregateKind::Tuple(ty) | AggregateKind::Closure(ty) => {
                        ty.as_ref().has_non_region_param()
                    }
                    AggregateKind::Adt(_, args) => args.as_ref().has_non_region_param(),
                    AggregateKind::Union(..) => false,
                },
                _ => false,
            }
    }

    fn operand_has_param(&self, operand: &Operand) -> bool {
        match &operand.kind {
            OperandKind::Copy(place) | OperandKind::Move(place) => self.place_has_param(place),
            OperandKind::Constant { konst, ty } => {
                konst.as_ref().has_non_region_param() || ty.as_ref().has_non_region_param()
            }
            OperandKind::Allocation { allocation } => allocation.as_ref().ty.has_non_region_param(),
            OperandKind::Static(_) => false,
        }
    }

    /// A field projection of the place is of a type that names a generic parameter: rustc's
    /// `PlaceElem::Field` holds the field's type.
    fn place_has_param(&self, place: &StoredPlace) -> bool {
        let mut ty = PlaceTy::from_ty(self.body.locals[place.local].ty.as_ref());
        for elem in place.projection.as_slice() {
            ty = ty.multi_projection_ty(&self.infcx, self.env, std::slice::from_ref(elem));
            if let ProjectionElem::Field(_) = elem
                && ty.ty.has_non_region_param()
            {
                return true;
            }
        }
        false
    }

    fn operand_ty(&self, operand: &Operand) -> Option<Ty<'db>> {
        Some(match &operand.kind {
            OperandKind::Copy(place) | OperandKind::Move(place) => place.as_ref().ty(self.body, &self.infcx, self.env).ty,
            OperandKind::Constant { ty, .. } => ty.as_ref(),
            OperandKind::Allocation { allocation } => allocation.as_ref().ty,
            OperandKind::Static(_) => return None,
        })
    }

    /// What a value of `ty` is, with its size, when it is a primitive the lint computes with.
    fn scalar_kind(&self, ty: Ty<'db>) -> Option<(Kind, u8)> {
        let int = |signed: bool, pointer_sized: bool| Kind::Int { signed, pointer_sized };
        Some(match ty.kind() {
            TyKind::Bool => (Kind::Bool, 1),
            TyKind::Char => (Kind::Char, 4),
            TyKind::Int(it) => {
                let size = match it {
                    IntTy::I8 => 1,
                    IntTy::I16 => 2,
                    IntTy::I32 => 4,
                    IntTy::I64 => 8,
                    IntTy::I128 => 16,
                    IntTy::Isize => self.pointer_size?,
                };
                (int(true, it == IntTy::Isize), size)
            }
            TyKind::Uint(it) => {
                let size = match it {
                    UintTy::U8 => 1,
                    UintTy::U16 => 2,
                    UintTy::U32 => 4,
                    UintTy::U64 => 8,
                    UintTy::U128 => 16,
                    UintTy::Usize => self.pointer_size?,
                };
                (int(false, it == UintTy::Usize), size)
            }
            TyKind::Float(it) => {
                let size = match it {
                    FloatTy::F16 => 2,
                    FloatTy::F32 => 4,
                    FloatTy::F64 => 8,
                    FloatTy::F128 => 16,
                };
                (Kind::Float(it), size)
            }
            _ => return None,
        })
    }

    fn env_and_crate(&self) -> ParamEnvAndCrate<'db> {
        ParamEnvAndCrate { param_env: self.env, krate: self.krate }
    }

    fn layout_size(&self, ty: Ty<'db>) -> Option<u64> {
        let layout = self.db.layout_of_ty(ty.store(), self.env_and_crate().store()).ok()?;
        Some(layout.size.bytes())
    }

    /// A literal `line!()` or `column!()` expands to, whose value lowering leaves to the code
    /// generator.
    fn is_placed(&self, expr: ExprId) -> bool {
        let Ok(source) = self.b.source_map.expr_syntax(expr) else { return false };
        let Some(call) = source.file_id.macro_file() else { return false };
        matches!(
            call.loc(self.db).def.kind,
            MacroDefKind::BuiltIn(_, BuiltinFnLikeExpander::Line | BuiltinFnLikeExpander::Column)
        )
    }

    /// `report_assert_as_lint`.
    fn report(&mut self, span: MirSpan, lint: Lint, panic: Panic) {
        self.emit(span, lint, panic.to_string());
    }

    /// The lint at what `span` is of, labelled `label` there, once: none in a macro of
    /// another crate, as these lints are not reported in external macros.
    fn emit(&mut self, span: MirSpan, lint: Lint, label: String) {
        let (node, span) = self.location(span);
        if self.b.in_external_macro(node) {
            return;
        }
        let mut diag = Diag::lint(lint.name(), lint.message(), span.clone());
        diag.span_label(span, label);
        let found = self.b.lint(diag, Phase::Mir, node);
        if !self.out.iter().any(|it| it.diag == found.diag && it.lint_at == found.lint_at) {
            self.out.push(found);
        }
    }

    /// The node and span of what `span` is of; of what lowering made with no expression, the
    /// body's item.
    fn location(&self, span: MirSpan) -> (InFile<SyntaxNodePtr>, Span) {
        if let Some(node) = self.b.mir_node(span)
            && let Some(span) = self.b.mir_span(span)
        {
            return (node, span);
        }
        let owner = self.b.owner_node();
        let node = self.b.cx.syntax(owner);
        (owner, self.b.cx.span(InFile::new(owner.file_id, def_range(&node.value))))
    }
}

/// The name of `param` when it is marked `#[rustc_panics_when_zero]`.
fn panics_when_zero(db: &dyn HirDatabase, param: ConstParamId) -> Option<String> {
    let sources = param.parent().child_source(db);
    let Either::Left(ast::TypeOrConstParam::Const(node)) = sources.value.get(param.local_id())? else { return None };
    let marked = node.attrs().any(|attr| attr.path().is_some_and(|path| path.syntax().text() == "rustc_panics_when_zero"));
    marked.then(|| node.name().map(|it| it.text().to_string()))?
}

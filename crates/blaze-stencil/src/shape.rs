//! The stencil set: which stencils exist, each named by a [`Shape`] (operation, controlling
//! type, auxiliary code, where each operand comes from, where the result goes). One
//! definition read by the generator (`build.rs` includes this file with `#[path]`), the
//! patcher's selection and the tests; it uses nothing but `std`.
//!
//! [`each`] lists the whole set in one deterministic order; [`Shape::key`] packs a shape in
//! 28 bits, which the library's perfect index maps to its stencil.

use std::fmt;

/// Bumped with any change to what a shape means or which shapes exist: the library's header
/// carries it, and a library of another version is refused.
pub const VERSION: u32 = 1;

/// A value's type, as stencils see it: the types the IR builder makes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum Ty {
    I8,
    I16,
    I32,
    I64,
    I128,
    F16,
    F32,
    F64,
    F128,
    I8x16,
    I16x8,
    I32x4,
    I64x2,
    F32x4,
    F64x2,
}

impl Ty {
    pub const ALL: [Ty; 15] = [
        Ty::I8,
        Ty::I16,
        Ty::I32,
        Ty::I64,
        Ty::I128,
        Ty::F16,
        Ty::F32,
        Ty::F64,
        Ty::F128,
        Ty::I8x16,
        Ty::I16x8,
        Ty::I32x4,
        Ty::I64x2,
        Ty::F32x4,
        Ty::F64x2,
    ];

    pub const fn from_u8(it: u8) -> Option<Ty> {
        if (it as usize) < Ty::ALL.len() { Some(Ty::ALL[it as usize]) } else { None }
    }

    /// Bytes of the value.
    pub const fn bytes(self) -> u8 {
        match self {
            Ty::I8 => 1,
            Ty::I16 | Ty::F16 => 2,
            Ty::I32 | Ty::F32 => 4,
            Ty::I64 | Ty::F64 => 8,
            _ => 16,
        }
    }

    /// Bytes of its slot: 8, or 16 for what is wider than 64 bits.
    pub const fn slot(self) -> u8 {
        if self.bytes() > 8 { 16 } else { 8 }
    }

    /// Which accumulator holds it, if any: the integer one for integers up to 64 bits, the
    /// float one for `f32`, `f64` and vectors. `i128`, `f16` and `f128` are never held.
    pub const fn acc(self) -> Option<Acc> {
        match self {
            Ty::I8 | Ty::I16 | Ty::I32 | Ty::I64 => Some(Acc::Int),
            Ty::F32 | Ty::F64 => Some(Acc::Float),
            Ty::I8x16 | Ty::I16x8 | Ty::I32x4 | Ty::I64x2 | Ty::F32x4 | Ty::F64x2 => {
                Some(Acc::Float)
            }
            Ty::I128 | Ty::F16 | Ty::F128 => None,
        }
    }

    pub const fn is_int(self) -> bool {
        matches!(self, Ty::I8 | Ty::I16 | Ty::I32 | Ty::I64 | Ty::I128)
    }

    pub const fn is_float(self) -> bool {
        matches!(self, Ty::F16 | Ty::F32 | Ty::F64 | Ty::F128)
    }

    pub const fn is_vector(self) -> bool {
        matches!(self, Ty::I8x16 | Ty::I16x8 | Ty::I32x4 | Ty::I64x2 | Ty::F32x4 | Ty::F64x2)
    }

    /// The lane type of a vector, the type itself otherwise.
    pub const fn lane(self) -> Ty {
        match self {
            Ty::I8x16 => Ty::I8,
            Ty::I16x8 => Ty::I16,
            Ty::I32x4 => Ty::I32,
            Ty::I64x2 => Ty::I64,
            Ty::F32x4 => Ty::F32,
            Ty::F64x2 => Ty::F64,
            it => it,
        }
    }

    pub const fn lanes(self) -> u8 {
        if self.is_vector() { 16 / self.lane().bytes() } else { 1 }
    }

    /// Cranelift's name of the type, for errors and the generator's symbol names.
    pub const fn name(self) -> &'static str {
        match self {
            Ty::I8 => "i8",
            Ty::I16 => "i16",
            Ty::I32 => "i32",
            Ty::I64 => "i64",
            Ty::I128 => "i128",
            Ty::F16 => "f16",
            Ty::F32 => "f32",
            Ty::F64 => "f64",
            Ty::F128 => "f128",
            Ty::I8x16 => "i8x16",
            Ty::I16x8 => "i16x8",
            Ty::I32x4 => "i32x4",
            Ty::I64x2 => "i64x2",
            Ty::F32x4 => "f32x4",
            Ty::F64x2 => "f64x2",
        }
    }
}

/// A set of [`Ty`], one bit each by its number.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, Debug)]
pub struct TySet(pub u16);

impl TySet {
    pub const INTS: TySet = TySet::of(&[Ty::I8, Ty::I16, Ty::I32, Ty::I64]);
    pub const INTS_WIDE: TySet = TySet::of(&[Ty::I8, Ty::I16, Ty::I32, Ty::I64, Ty::I128]);
    pub const FLOATS: TySet = TySet::of(&[Ty::F32, Ty::F64]);
    pub const INT_VECTORS: TySet = TySet::of(&[Ty::I8x16, Ty::I16x8, Ty::I32x4, Ty::I64x2]);
    pub const FLOAT_VECTORS: TySet = TySet::of(&[Ty::F32x4, Ty::F64x2]);
    pub const ALL: TySet = TySet((1 << Ty::ALL.len()) - 1);

    pub const fn of(tys: &[Ty]) -> TySet {
        let mut bits = 0u16;
        let mut i = 0;
        while i < tys.len() {
            bits |= 1 << tys[i] as u16;
            i += 1;
        }
        TySet(bits)
    }

    pub const fn union(self, other: TySet) -> TySet {
        TySet(self.0 | other.0)
    }

    pub const fn contains(self, ty: Ty) -> bool {
        self.0 & (1 << ty as u16) != 0
    }

    pub fn iter(self) -> impl Iterator<Item = Ty> {
        Ty::ALL.into_iter().filter(move |it| self.contains(*it))
    }
}

/// The two accumulators: registers a result stays in for the next stencil
/// (stencil.md, "Shapes").
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Acc {
    Int,
    Float,
}

/// Where an operand comes from.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum Src {
    /// Not an operand of this op (the operand count is the op's).
    None,
    Slot,
    /// The accumulator of its type: the previous stencil's result.
    Acc,
    /// A 32-bit immediate, sign-extended to the operand's type: an `Imm(n)` hole.
    Imm,
}

impl Src {
    pub const fn from_u8(it: u8) -> Option<Src> {
        match it {
            0 => Some(Src::None),
            1 => Some(Src::Slot),
            2 => Some(Src::Acc),
            3 => Some(Src::Imm),
            _ => None,
        }
    }
}

/// Where result 0 goes; any other result always goes to its slot.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum Dst {
    /// No result (stores, branches, traps, fences).
    None,
    Slot,
    /// Only the accumulator: its one use is the next stencil.
    Acc,
    /// The accumulator and its slot: the next stencil is one of several uses.
    Both,
}

impl Dst {
    pub const fn from_u8(it: u8) -> Option<Dst> {
        match it {
            0 => Some(Dst::None),
            1 => Some(Dst::Slot),
            2 => Some(Dst::Acc),
            3 => Some(Dst::Both),
            _ => None,
        }
    }
}

/// An integer condition, in the order of Cranelift's `IntCC`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum IntCc {
    Equal,
    NotEqual,
    SignedLessThan,
    SignedGreaterThanOrEqual,
    SignedGreaterThan,
    SignedLessThanOrEqual,
    UnsignedLessThan,
    UnsignedGreaterThanOrEqual,
    UnsignedGreaterThan,
    UnsignedLessThanOrEqual,
}

impl IntCc {
    pub const COUNT: u8 = 10;
}

/// A float condition, in the order of Cranelift's `FloatCC`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum FloatCc {
    Ordered,
    Unordered,
    Equal,
    NotEqual,
    OrderedNotEqual,
    UnorderedOrEqual,
    LessThan,
    LessThanOrEqual,
    GreaterThan,
    GreaterThanOrEqual,
    UnorderedOrLessThan,
    UnorderedOrLessThanOrEqual,
    UnorderedOrGreaterThan,
    UnorderedOrGreaterThanOrEqual,
}

impl FloatCc {
    pub const COUNT: u8 = 14;
}

/// An atomic read-modify-write, in the order of Cranelift's `AtomicRmwOp`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum RmwOp {
    Add,
    Sub,
    And,
    Nand,
    Or,
    Xor,
    Xchg,
    Umin,
    Umax,
    Smin,
    Smax,
}

impl RmwOp {
    pub const COUNT: u8 = 11;
}

/// The operation of a stencil: Cranelift's opcodes that a stencil implements, with the
/// `_imm` forms folded into their base op (an [`Src::Imm`] operand), `select_spectre_guard`
/// into [`Op::Select`], `stack_load`/`stack_store`/`stack_addr` kept as frame-relative ops,
/// and the fusions [`Op::BrIcmp`] and [`Op::SelectIcmp`]. Opcodes not here are the glue's
/// (`arch::Glue`) or refused with an error naming them (`patch/select.rs`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum Op {
    // Moves and constants.
    /// A slot or accumulator to a slot or accumulator: block arguments, `iconcat`'s halves.
    Copy,
    /// `iconst` into a slot or accumulator (one no shape takes as an [`Src::Imm`]): `aux` 0
    /// when the value fits 32 bits sign-extended, 1 for an `Imm0` hole of all 64.
    Iconst,
    /// `f16const`, `f32const`, `f64const`: the bits as `Imm0`, `aux` as for [`Op::Iconst`].
    Fconst,
    /// `vconst`, `f128const`: the 16 bytes in the function's constant area (`Const`).
    Vconst,
    // Integer arithmetic.
    Iadd,
    Isub,
    Ineg,
    Iabs,
    Imul,
    Umulhi,
    Smulhi,
    Udiv,
    Sdiv,
    Urem,
    Srem,
    SqmulRoundSat,
    AvgRound,
    UaddSat,
    SaddSat,
    UsubSat,
    SsubSat,
    Smin,
    Umin,
    Smax,
    Umax,
    // Arithmetic with an overflow or carry result (result 1, in its slot).
    UaddOverflow,
    SaddOverflow,
    UsubOverflow,
    SsubOverflow,
    UmulOverflow,
    SmulOverflow,
    UaddOverflowCin,
    SaddOverflowCin,
    UsubOverflowBin,
    SsubOverflowBin,
    /// Adds, trapping on unsigned overflow (`Trap`).
    UaddOverflowTrap,
    // Bits.
    Band,
    Bor,
    Bxor,
    Bnot,
    BandNot,
    BorNot,
    BxorNot,
    Rotl,
    Rotr,
    Ishl,
    Ushr,
    Sshr,
    Bitrev,
    Clz,
    Cls,
    Ctz,
    Bswap,
    Popcnt,
    Bitselect,
    Bmask,
    // Comparisons and selects.
    /// `aux` is the [`IntCc`].
    Icmp,
    /// `aux` is the [`FloatCc`].
    Fcmp,
    /// `select c, x, y`: `aux` is the condition's [`Ty`].
    Select,
    /// `select (icmp cc a b), x, y`: `aux` is the [`IntCc`] in its low 4 bits and the
    /// compared [`Ty`] in its high 4.
    SelectIcmp,
    // Floats.
    Fadd,
    Fsub,
    Fmul,
    Fdiv,
    Sqrt,
    Fma,
    Fneg,
    Fabs,
    Fcopysign,
    Fmin,
    Fmax,
    Ceil,
    Floor,
    Trunc,
    Nearest,
    // Conversions: `aux` is the source [`Ty`] where it can be more than one.
    Bitcast,
    Ireduce,
    Uextend,
    Sextend,
    Fpromote,
    Fdemote,
    Fvdemote,
    FvpromoteLow,
    FcvtToUint,
    FcvtToSint,
    FcvtToUintSat,
    FcvtToSintSat,
    FcvtFromUint,
    FcvtFromSint,
    /// `i128` to its two `i64` halves (results 0 and 1).
    Isplit,
    /// Two `i64` halves to an `i128`.
    Iconcat,
    Snarrow,
    Unarrow,
    Uunarrow,
    SwidenLow,
    SwidenHigh,
    UwidenLow,
    UwidenHigh,
    IaddPairwise,
    /// `aux` is the scalar's [`Ty`]; `ty` the vector's.
    ScalarToVector,
    // Vectors.
    Splat,
    Swizzle,
    /// The 16-byte mask in the function's constant area (`Const`).
    Shuffle,
    /// `aux` is the lane.
    Insertlane,
    /// `aux` is the lane.
    Extractlane,
    VanyTrue,
    VallTrue,
    /// `aux` is the result's [`Ty`].
    VhighBits,
    Blendv,
    X86Pshufb,
    X86Pmulhrsw,
    X86Pmaddubsw,
    X86Cvtt2dq,
    // Memory: the access's offset is the `Offset` hole. `ty` is the value's type; for the
    // narrow loads and stores, the wide type (`uload8.i32`: `ty` is `i32`).
    Load,
    Store,
    Uload8,
    Sload8,
    Istore8,
    Uload16,
    Sload16,
    Istore16,
    Uload32,
    Sload32,
    Istore32,
    Uload8x8,
    Sload8x8,
    Uload16x4,
    Sload16x4,
    Uload32x2,
    Sload32x2,
    /// The frame register plus `Offset` (the stack slot's place plus the offset).
    StackAddr,
    StackLoad,
    StackStore,
    AtomicLoad,
    AtomicStore,
    /// `aux` is the [`RmwOp`].
    AtomicRmw,
    AtomicCas,
    Fence,
    // Control.
    /// To `Next`.
    Jump,
    /// To `Target` when the condition (of type `ty`) is not zero, else to `Next`.
    Brif,
    /// `brif (icmp cc a b)`: `aux` is the [`IntCc`]; `ty` the compared type.
    BrIcmp,
    /// To `Trap` when the condition is zero.
    Trapz,
    /// To `Trap` when the condition is not zero.
    Trapnz,
}

impl Op {
    /// The number of ops: every `Op as u8` is below it.
    pub const COUNT: u8 = Op::Trapnz as u8 + 1;

    pub fn from_u8(it: u8) -> Option<Op> {
        Op::all().nth(usize::from(it)).filter(|op| *op as u8 == it)
    }

    /// Every op, in the order of their numbers.
    pub fn all() -> impl Iterator<Item = Op> {
        (0..Op::COUNT).map(op_of)
    }

    /// What the op takes and gives, and which shapes of it exist.
    pub fn info(self) -> &'static Info {
        &INFO[self as usize]
    }
}

/// The op numbered `it`, which is below [`Op::COUNT`].
fn op_of(it: u8) -> Op {
    INFO[usize::from(it)].op
}

/// What `aux` means for an op, and so which values of it exist.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Aux {
    /// Always 0.
    None,
    /// An [`IntCc`].
    IntCc,
    /// A [`FloatCc`].
    FloatCc,
    /// An [`IntCc`] and the compared integer [`Ty`] ([`Op::SelectIcmp`]).
    IntCcTy,
    /// A lane of `ty`: below `ty.lanes()`.
    Lane,
    /// A source or condition [`Ty`], from the set given.
    Ty(TySet),
    /// An [`RmwOp`].
    Rmw,
}

/// What an op takes and gives, and which shapes of it exist.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Info {
    pub op: Op,
    /// Cranelift's name of the opcode it implements (the base opcode for the `_imm` forms).
    pub name: &'static str,
    /// Value operands, at most 3.
    pub args: u8,
    /// Results, at most 2.
    pub results: u8,
    /// The controlling types it has a stencil for.
    pub tys: TySet,
    pub aux: Aux,
    /// Bit `n`: operand `n` may be [`Src::Imm`] (where its type is an integer up to 64 bits).
    pub imm: u8,
    /// Bit `n`: operand `n` may be [`Src::Acc`] (where its type has an accumulator).
    pub acc: u8,
    /// Whether result 0 may go to an accumulator ([`Dst::Acc`], [`Dst::Both`]).
    pub dst_acc: bool,
    /// Whether it has a `Trap` hole.
    pub traps: bool,
    /// Whether it has a `Target` hole (a branch).
    pub branches: bool,
}

/// One [`Info`] per op, by its number: the generator child (`fastcompile-stencil-gen`) fills
/// in the table, each row from Cranelift's lowering rules of the opcode on x64 and AArch64
/// (which types it takes) and the IR builder's use of it.
static INFO: [Info; Op::Trapnz as usize + 1] = info_table();

const fn info_table() -> [Info; Op::Trapnz as usize + 1] {
    unimplemented!("fastcompile-stencil-gen: the info of every op")
}

/// One stencil's name: 4 bytes, packed by [`Shape::key`] into 28 bits.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Shape {
    pub op: Op,
    pub ty: Ty,
    pub aux: u8,
    pub srcs: [Src; 3],
    pub dst: Dst,
}

impl Shape {
    /// `op` in bits 20..28, `ty` 16..20, `aux` 8..16, the three sources 2 bits each from bit 2,
    /// `dst` in bits 0..2.
    pub const fn key(self) -> u32 {
        (self.op as u32) << 20
            | (self.ty as u32) << 16
            | (self.aux as u32) << 8
            | (self.srcs[0] as u32) << 6
            | (self.srcs[1] as u32) << 4
            | (self.srcs[2] as u32) << 2
            | self.dst as u32
    }

    pub fn from_key(key: u32) -> Option<Shape> {
        if key >> 28 != 0 {
            return None;
        }
        let field = |shift: u32, bits: u32| ((key >> shift) & ((1 << bits) - 1)) as u8;
        Some(Shape {
            op: Op::from_u8(field(20, 8))?,
            ty: Ty::from_u8(field(16, 4))?,
            aux: field(8, 8),
            srcs: [
                Src::from_u8(field(6, 2))?,
                Src::from_u8(field(4, 2))?,
                Src::from_u8(field(2, 2))?,
            ],
            dst: Dst::from_u8(field(0, 2))?,
        })
    }

    /// Whether the shape is one of the set: its op has the type, `aux` is in range, the
    /// sources and the destination are ones the op's [`Info`] allows for the type, and at
    /// most one source is an accumulator.
    pub fn exists(self) -> bool {
        unimplemented!("fastcompile-stencil-gen: the rules of the set, as `each` enumerates it")
    }
}

impl fmt::Display for Shape {
    /// The stencil's symbol in the generator's module, and its name in errors:
    /// `iadd.i64.0.sa.s` (op, type, aux, sources, destination).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let src = |it: Src| match it {
            Src::None => "",
            Src::Slot => "s",
            Src::Acc => "a",
            Src::Imm => "i",
        };
        let dst = match self.dst {
            Dst::None => "n",
            Dst::Slot => "s",
            Dst::Acc => "a",
            Dst::Both => "b",
        };
        write!(
            f,
            "{}.{}.{}.{}{}{}.{dst}",
            self.op.info().name,
            self.ty.name(),
            self.aux,
            src(self.srcs[0]),
            src(self.srcs[1]),
            src(self.srcs[2]),
        )
    }
}

/// Calls `f` with every shape of the set, in one order: by op number, then type number,
/// then `aux`, then sources and destination by their numbers. The generator makes one
/// stencil for each; [`Shape::exists`] holds exactly for these.
pub fn each(f: impl FnMut(Shape)) {
    let _ = f;
    unimplemented!("fastcompile-stencil-gen: the enumeration of the set")
}

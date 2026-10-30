//! What no stencil can be, per architecture ([`Glue`]): the prologue and epilogue, parameters
//! and returns, calls in the target's convention, landing pads, addresses of symbols and
//! thread-locals, jump tables, traps, the libcall operations, far-slot relays and the tier
//! counters. Each is written as Cranelift writes it for the same IR and flags
//! (docs/v2/plan/lanes/fastcompile/stencil.md, "The glue per architecture"), so a function of
//! this backend and one of Cranelift's call each other with no adapter.
//!
//! The patcher ([`crate::patch`]) drives it through a [`Cx`]: the buffer, the function, the
//! frame the glue laid out, and what the patcher decided (each value's [`Loc`], the labels of
//! blocks and edges, the unwind rows and call sites being recorded).

use cranelift_codegen::ir;

use crate::{
    Error, Target,
    buf::{Buf, Label},
    code::{self, Name, Rule},
    patch,
};

pub mod aarch64;
pub mod x64;

/// Where a value is, as the glue reads or writes it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum Loc {
    /// Never used: no slot, and nothing is stored.
    #[default]
    Dead,
    /// In its slot, this many bytes above the frame register.
    Slot(u32),
    /// An `iconst`, `f32const` or `f64const` (its bits) the patcher folded: no slot holds it,
    /// and the glue materializes it where it is used.
    Const(i64),
}

/// What the patcher laid out above the frame register: the value slots, then the stack
/// slots.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Area {
    /// Bytes, a multiple of 16.
    pub size: u32,
    /// The largest alignment a stack slot asks for, a power of two of at least 16: the frame
    /// register is aligned to it.
    pub align: u32,
}

/// One architecture's glue. `Self` is a marker type; every function is given the context.
pub trait Glue: Sized + 'static {
    const ARCH: blaze_codegen::Arch;
    /// The hardware number of the stencils' frame register (their convention's first
    /// argument), callee-saved in every C ABI of the architecture. The generator checks LLVM
    /// put the argument there.
    const FRAME: u8;
    /// The first frame offset stencils can't reach a slot of 8 and of 16 bytes at: operands
    /// at or above it are relayed through [`Cx::scratch`] by [`Glue::relay`].
    const NEAR: [u32; 2];
    /// The architecture's no-op, which padding is made of.
    const NOP: &'static [u8];
    /// The architecture's layout of one function's frame.
    type Frame: Default;

    /// Lays out the frame of `func`: the saved registers, the area above the frame register,
    /// the outgoing argument area of its largest call, the realignment and the probe.
    fn frame(func: &ir::Function, target: &Target, area: Area) -> Result<Self::Frame, Error>;

    /// Whether the glue writes `inst` (a call, a return, an address, a jump table, a trap,
    /// an operation Cranelift lowers to a libcall on the target's CPU baseline): every other
    /// instruction is a stencil's.
    fn takes(func: &ir::Function, target: &Target, inst: ir::Inst) -> bool;

    /// The most bytes [`Glue::inst`] writes for `inst`, which [`Glue::takes`].
    fn bound(func: &ir::Function, target: &Target, inst: ir::Inst) -> u32;

    /// The most bytes the prologue, one landing pad, one relay, one back-edge count and the
    /// trap stubs write, for the one reservation.
    fn bound_fixed(frame: &Self::Frame) -> u32;

    /// Writes the prologue: the frame, the saved registers, the probe, the frame register, the
    /// calls counter ([`crate::tier`]), and each entry block parameter stored to its [`Loc`].
    fn prologue(cx: &mut Cx<'_, Self>) -> Result<(), Error>;

    /// Writes `inst`, which [`Glue::takes`]. A `try_call`'s normal edge is written next by
    /// the patcher, so the glue falls through to it.
    fn inst(cx: &mut Cx<'_, Self>, inst: ir::Inst) -> Result<(), Error>;

    /// At a landing pad of `inst` (a `try_call`, at the label of one of its handler edges):
    /// stores the exception payload registers to [`Cx::payload`].
    fn landing(cx: &mut Cx<'_, Self>, inst: ir::Inst) -> Result<(), Error>;

    /// Writes the trap stub of `code`, at the label the patcher bound.
    fn trap(cx: &mut Cx<'_, Self>, code: ir::TrapCode);

    /// Adds one to the function's back-edge cell ([`crate::tier`]).
    fn count_back_edge(cx: &mut Cx<'_, Self>);

    /// Copies `bytes` (8 or 16) from the slot at `from` to the slot at `to`, either beyond
    /// [`Glue::NEAR`].
    fn relay(cx: &mut Cx<'_, Self>, from: u32, to: u32, bytes: u8);

    /// The function's unwind information, from the rows recorded ([`Cx::rows`]) once all of
    /// it is written.
    fn unwind(cx: &mut Cx<'_, Self>) -> Result<Option<code::Unwind>, Error>;

    /// The DWARF register named slots are found from (the frame pointer), and where the
    /// frame register points from it.
    fn slot_base(frame: &Self::Frame) -> (u16, i32);
}

/// What the glue is given to write one piece of a function.
pub struct Cx<'a, A: Glue> {
    pub buf: &'a mut Buf,
    pub func: &'a ir::Function,
    pub target: &'a Target,
    pub frame: &'a A::Frame,
    pub(crate) plan: &'a patch::Plan,
    pub(crate) side: &'a mut patch::Side,
}

impl<A: Glue> Cx<'_, A> {
    /// Where `value` is (its aliases resolved).
    pub fn loc(&self, value: ir::Value) -> Loc {
        self.plan.loc(self.func, value)
    }

    /// Where `inst`'s return value `k` goes: the result's place for a `call`, the place of
    /// `TryCallRet(k)` for a `try_call`.
    pub fn returned(&self, inst: ir::Inst, k: usize) -> Loc {
        self.plan.returned(inst, k)
    }

    /// Where a `try_call`'s exception payload value `k` goes (`TryCallExn(k)`).
    pub fn payload(&self, inst: ir::Inst, k: usize) -> Loc {
        self.plan.payload(inst, k)
    }

    pub fn block(&self, block: ir::Block) -> Label {
        self.plan.block(block)
    }

    /// The label of `inst`'s successor `i`, in the order of
    /// `InstructionData::branch_destination` (for a `try_call`: the normal return, then its
    /// handlers in order): the block's own label, or an edge's that moves its arguments
    /// first.
    pub fn edge(&self, inst: ir::Inst, i: usize) -> Label {
        self.plan.edge(inst, i)
    }

    /// The frame offset of a stack slot.
    pub fn stack_slot(&self, slot: ir::StackSlot) -> u32 {
        self.plan.stack_slot(slot)
    }

    /// Two 16-byte slots below [`Glue::NEAR`] that hold nothing across instructions.
    pub fn scratch(&self) -> [u32; 2] {
        self.plan.scratch()
    }

    /// The label of the trap stub of `code`, made on first use.
    pub fn trap(&mut self, code: ir::TrapCode) -> Label {
        self.side.trap(self.buf, code)
    }

    /// Records an unwind rule holding from the current offset.
    pub fn rule(&mut self, rule: Rule) {
        let at = self.buf.len();
        self.side.rule(at, rule);
    }

    /// Records an unwind rule holding from `at`.
    pub fn rule_at(&mut self, at: u32, rule: Rule) {
        self.side.rule(at, rule);
    }

    pub fn rows(&self) -> &[(u32, Rule)] {
        self.side.rows()
    }

    /// Records the call of `inst` returning at `ret`; its handlers are those of `inst`'s
    /// exception table, at its edges' labels.
    pub fn call_site(&mut self, ret: u32, inst: ir::Inst) {
        self.side.call_site(ret, inst);
    }

    /// The relocation name of an external name of the function.
    pub fn name(&self, name: &ir::ExternalName) -> Result<Name, Error> {
        match name {
            ir::ExternalName::User(at) => {
                let name = &self.func.params.user_named_funcs()[*at];
                Ok(Name::User { namespace: name.namespace, index: name.index })
            }
            ir::ExternalName::LibCall(call) => Ok(Name::LibCall(*call)),
            ir::ExternalName::KnownSymbol(symbol) => Ok(Name::Known(*symbol)),
            ir::ExternalName::TestCase(_) => {
                Err(Error::new("a relocation against a test case's name"))
            }
        }
    }
}

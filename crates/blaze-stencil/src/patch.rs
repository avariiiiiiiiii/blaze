//! From one function's Cranelift IR to its [`Code`], in one pass over the IR and one buffer
//! (docs/v2/plan/lanes/fastcompile/stencil.md, "What a compile is"): scan the values, give
//! each a slot, lay out the frame, reserve the buffer, then per block in layout order copy
//! each instruction's stencil and patch its holes (or have the glue write it), write the
//! edges' moves, the trap stubs and the constants, resolve the labels and hand back the
//! code.
//!
//! Every column is in [`Scratch`], indexed by the IR's entity numbers and cleared, not
//! freed, between functions: a compile allocates nothing per instruction.

use cranelift_codegen::ir;

use crate::{
    Error, Target,
    arch::{Glue, Loc},
    buf::{Buf, Label},
    code::{Code, Rule},
    table::Library,
};

/// One thread's columns and buffer, reused across compiles.
#[derive(Default)]
pub struct Scratch {
    pub(crate) buf: Buf,
    pub(crate) plan: Plan,
    pub(crate) side: Side,
}

impl Scratch {
    pub fn new() -> Scratch {
        Scratch::default()
    }

    /// Gives the scratch the allocations of a [`Code`] its consumer is done with, so the next
    /// compile writes into them.
    pub fn recycle(&mut self, code: Code) {
        let _ = code;
        unimplemented!("fastcompile-stencil-patch")
    }
}

/// What the patcher decided of a function before writing it: each value's place, the labels
/// of blocks and edges, the stack slots' offsets. The glue reads it through
/// [`crate::arch::Cx`].
#[derive(Default)]
pub(crate) struct Plan {}

impl Plan {
    pub(crate) fn loc(&self, func: &ir::Function, value: ir::Value) -> Loc {
        let _ = (func, value);
        unimplemented!("fastcompile-stencil-patch")
    }

    pub(crate) fn returned(&self, inst: ir::Inst, k: usize) -> Loc {
        let _ = (inst, k);
        unimplemented!("fastcompile-stencil-patch")
    }

    pub(crate) fn payload(&self, inst: ir::Inst, k: usize) -> Loc {
        let _ = (inst, k);
        unimplemented!("fastcompile-stencil-patch")
    }

    pub(crate) fn block(&self, block: ir::Block) -> Label {
        let _ = block;
        unimplemented!("fastcompile-stencil-patch")
    }

    pub(crate) fn edge(&self, inst: ir::Inst, i: usize) -> Label {
        let _ = (inst, i);
        unimplemented!("fastcompile-stencil-patch")
    }

    pub(crate) fn stack_slot(&self, slot: ir::StackSlot) -> u32 {
        let _ = slot;
        unimplemented!("fastcompile-stencil-patch")
    }

    pub(crate) fn scratch(&self) -> [u32; 2] {
        unimplemented!("fastcompile-stencil-patch")
    }
}

/// What is recorded while the function is written: unwind rows, call sites, trap stubs
/// wanted, lines.
#[derive(Default)]
pub(crate) struct Side {}

impl Side {
    pub(crate) fn rule(&mut self, at: u32, rule: Rule) {
        let _ = (at, rule);
        unimplemented!("fastcompile-stencil-patch")
    }

    pub(crate) fn rows(&self) -> &[(u32, Rule)] {
        unimplemented!("fastcompile-stencil-patch")
    }

    pub(crate) fn call_site(&mut self, ret: u32, inst: ir::Inst) {
        let _ = (ret, inst);
        unimplemented!("fastcompile-stencil-patch")
    }

    pub(crate) fn trap(&mut self, buf: &mut Buf, code: ir::TrapCode) -> Label {
        let _ = (buf, code);
        unimplemented!("fastcompile-stencil-patch")
    }
}

/// Compiles `func` for `target` with `A`'s glue and `library`'s stencils.
pub fn compile<A: Glue>(
    scratch: &mut Scratch,
    func: &ir::Function,
    target: &Target,
    library: &Library,
) -> Result<Code, Error> {
    let _ = (scratch, func, target, library);
    unimplemented!("fastcompile-stencil-patch")
}

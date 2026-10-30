//! AArch64's glue: AAPCS64 (Linux, Windows) and Apple's variant, written as Cranelift
//! 0.136.2's `isa/aarch64` writes the same IR for the same flags (stencil.md, "The glue per
//! architecture"). The stencils' frame register is `x20`; every function keeps `x29` as its
//! frame pointer; `x16` and `x17` are the glue's scratch, `x18` is never written.

use cranelift_codegen::ir;

use super::{Area, Cx, Glue};
use crate::{Error, Target, code};

/// AArch64.
pub struct Aarch64;

/// One function's frame on AArch64.
#[derive(Default)]
pub struct Frame {}

impl Glue for Aarch64 {
    const ARCH: blaze_codegen::Arch = blaze_codegen::Arch::Aarch64;
    /// `x20`.
    const FRAME: u8 = 20;
    /// `ldr`/`str` with an imm12 scaled by 8 and by 16.
    const NEAR: [u32; 2] = [4096 * 8, 4096 * 16];
    /// `nop`: `d503201f`.
    const NOP: &'static [u8] = &[0x1f, 0x20, 0x03, 0xd5];
    type Frame = Frame;

    fn frame(func: &ir::Function, target: &Target, area: Area) -> Result<Frame, Error> {
        let _ = (func, target, area);
        unimplemented!("fastcompile-stencil-a64")
    }

    fn takes(func: &ir::Function, target: &Target, inst: ir::Inst) -> bool {
        let _ = (func, target, inst);
        unimplemented!("fastcompile-stencil-a64")
    }

    fn bound(func: &ir::Function, target: &Target, inst: ir::Inst) -> u32 {
        let _ = (func, target, inst);
        unimplemented!("fastcompile-stencil-a64")
    }

    fn bound_fixed(frame: &Frame) -> u32 {
        let _ = frame;
        unimplemented!("fastcompile-stencil-a64")
    }

    fn prologue(cx: &mut Cx<'_, Aarch64>) -> Result<(), Error> {
        let _ = cx;
        unimplemented!("fastcompile-stencil-a64")
    }

    fn inst(cx: &mut Cx<'_, Aarch64>, inst: ir::Inst) -> Result<(), Error> {
        let _ = (cx, inst);
        unimplemented!("fastcompile-stencil-a64")
    }

    fn landing(cx: &mut Cx<'_, Aarch64>, inst: ir::Inst) -> Result<(), Error> {
        let _ = (cx, inst);
        unimplemented!("fastcompile-stencil-a64")
    }

    fn trap(cx: &mut Cx<'_, Aarch64>, code: ir::TrapCode) {
        let _ = (cx, code);
        unimplemented!("fastcompile-stencil-a64")
    }

    fn count_back_edge(cx: &mut Cx<'_, Aarch64>) {
        let _ = cx;
        unimplemented!("fastcompile-stencil-a64")
    }

    fn relay(cx: &mut Cx<'_, Aarch64>, from: u32, to: u32, bytes: u8) {
        let _ = (cx, from, to, bytes);
        unimplemented!("fastcompile-stencil-a64")
    }

    fn unwind(cx: &mut Cx<'_, Aarch64>) -> Result<Option<code::Unwind>, Error> {
        let _ = cx;
        unimplemented!("fastcompile-stencil-a64")
    }

    fn slot_base(frame: &Frame) -> (u16, i32) {
        let _ = frame;
        unimplemented!("fastcompile-stencil-a64")
    }
}

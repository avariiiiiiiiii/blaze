//! x86-64's glue: SysV (Linux, macOS) and Windows x64, written as Cranelift 0.136.2's
//! `isa/x64` writes the same IR for the same flags (stencil.md, "The glue per architecture").
//! The stencils' frame register is `r12`; every function keeps `rbp` as its frame pointer.

use cranelift_codegen::ir;

use super::{Area, Cx, Glue};
use crate::{Error, Target, code};

/// x86-64.
pub struct X64;

/// One function's frame on x86-64.
#[derive(Default)]
pub struct Frame {}

impl Glue for X64 {
    const ARCH: blaze_codegen::Arch = blaze_codegen::Arch::X86_64;
    /// `r12`.
    const FRAME: u8 = 12;
    /// Every slot is reached with a `disp32`.
    const NEAR: [u32; 2] = [i32::MAX as u32, i32::MAX as u32];
    const NOP: &'static [u8] = &[0x90];
    type Frame = Frame;

    fn frame(func: &ir::Function, target: &Target, area: Area) -> Result<Frame, Error> {
        let _ = (func, target, area);
        unimplemented!("fastcompile-stencil-x64")
    }

    fn takes(func: &ir::Function, target: &Target, inst: ir::Inst) -> bool {
        let _ = (func, target, inst);
        unimplemented!("fastcompile-stencil-x64")
    }

    fn bound(func: &ir::Function, target: &Target, inst: ir::Inst) -> u32 {
        let _ = (func, target, inst);
        unimplemented!("fastcompile-stencil-x64")
    }

    fn bound_fixed(frame: &Frame) -> u32 {
        let _ = frame;
        unimplemented!("fastcompile-stencil-x64")
    }

    fn prologue(cx: &mut Cx<'_, X64>) -> Result<(), Error> {
        let _ = cx;
        unimplemented!("fastcompile-stencil-x64")
    }

    fn inst(cx: &mut Cx<'_, X64>, inst: ir::Inst) -> Result<(), Error> {
        let _ = (cx, inst);
        unimplemented!("fastcompile-stencil-x64")
    }

    fn landing(cx: &mut Cx<'_, X64>, inst: ir::Inst) -> Result<(), Error> {
        let _ = (cx, inst);
        unimplemented!("fastcompile-stencil-x64")
    }

    fn trap(cx: &mut Cx<'_, X64>, code: ir::TrapCode) {
        let _ = (cx, code);
        unimplemented!("fastcompile-stencil-x64")
    }

    fn count_back_edge(cx: &mut Cx<'_, X64>) {
        let _ = cx;
        unimplemented!("fastcompile-stencil-x64")
    }

    fn relay(cx: &mut Cx<'_, X64>, from: u32, to: u32, bytes: u8) {
        let _ = (cx, from, to, bytes);
        unimplemented!("fastcompile-stencil-x64")
    }

    fn unwind(cx: &mut Cx<'_, X64>) -> Result<Option<code::Unwind>, Error> {
        let _ = cx;
        unimplemented!("fastcompile-stencil-x64")
    }

    fn slot_base(frame: &Frame) -> (u16, i32) {
        let _ = frame;
        unimplemented!("fastcompile-stencil-x64")
    }
}

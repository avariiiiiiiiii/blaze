//! One function's frame on AArch64, laid out and entered as Cranelift 0.136.2's
//! `AArch64MachineDeps` (`gen_prologue_frame_setup`, `gen_inline_probestack`,
//! `gen_clobber_save`, `gen_clobber_restore`, `gen_epilogue_frame_restore`) and
//! `Callee::gen_prologue` write it, for a function that clobbers every callee-saved
//! register (stencils may write all of them):
//!
//! ```text
//!   caller's sp ->  incoming stack arguments
//!                   x29, x30                       the frame record   <- x29
//!                   x19 .. x28, d8 .. d15          SAVES bytes
//!                   the area (value and stack slots)                   <- x20 + area.size
//!                   realignment                    align - 16, when align > 16
//!                   the StructReturn pointer       16, when the function has one
//!                   outgoing stack arguments                           <- sp
//! ```
//!
//! `x20` points at the area's bottom, so its first `NEAR` bytes (most of a function's slots)
//! are one `ldr`/`str` away.

use cranelift_codegen::ir::{self, ArgumentPurpose, GlobalValueData, InstructionData};

use super::{
    abi,
    inst::{self, FP, LR, SP, X16, X17},
};
use crate::{Error, Os, Target, TlsModel, arch::Area, buf::Buf, code::Rule};

/// Bytes of the registers every function saves: `x19` to `x28` and the low halves of `d8`
/// to `d15`, Cranelift's `compute_clobber_size` of all of them.
pub const SAVES: u32 = 10 * 8 + 8 * 8;

/// The pairs `gen_clobber_save` pushes, in its order: the X registers sorted and paired,
/// last pair first, then the D registers the same way.
pub const X_PAIRS: [(u8, u8); 5] = [(27, 28), (25, 26), (23, 24), (21, 22), (19, 20)];
pub const D_PAIRS: [(u8, u8); 4] = [(14, 15), (12, 13), (10, 11), (8, 9)];

/// DWARF numbers: `x0`..`x31` are 0..31, `v0`..`v31` are 64..95.
pub const DWARF_FP: u16 = 29;
pub const DWARF_LR: u16 = 30;
pub const DWARF_X20: u16 = 20;
const DWARF_V0: u16 = 64;

/// How the prologue touches the pages of a frame larger than one: Cranelift's
/// `gen_inline_probestack`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Probe {
    #[default]
    None,
    /// `count` (at most 3) steps of `guard` bytes, each touched, then `sp` put back.
    Unroll { count: u32, guard: u32 },
    /// A loop over `size` bytes in steps of `guard`.
    Loop { size: u32, guard: u32 },
}

/// Cranelift's `PROBE_MAX_UNROLL`.
const PROBE_MAX_UNROLL: u32 = 3;

/// One function's frame on AArch64.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Frame {
    /// Bytes from `sp` up to the saved registers: Cranelift's `fixed_frame_storage_size +
    /// outgoing_args_size`, which one `sub sp` allocates.
    pub stack: u32,
    /// The outgoing stack argument area at `sp`: the largest of the function's calls'.
    pub outgoing: u32,
    /// From `sp` to the frame register, before any realignment.
    pub base: u32,
    /// The frame register's alignment, a power of two of at least 16.
    pub align: u32,
    /// The StructReturn pointer's own slot, from `sp`: Cranelift returns it in `x8`.
    pub sret: Option<u32>,
    pub probe: Probe,
    /// Whether the function makes a call (Cranelift's `FunctionCalls::Regular`).
    pub calls: bool,
    /// The unwind information is Windows', not DWARF's.
    pub windows: bool,
}

impl Frame {
    /// The offset of `x20` from `x29` when it is not realigned.
    pub fn base_from_fp(&self) -> i32 {
        self.base as i32 - (self.stack + SAVES) as i32
    }
}

/// Lays out the frame of `func`, whose area above the frame register the patcher sized.
pub fn layout(func: &ir::Function, target: &Target, area: Area) -> Result<Frame, Error> {
    abi::check(func.signature.call_conv)?;
    if !area.align.is_power_of_two() || area.align < 16 || area.size % 16 != 0 {
        return Err(Error::new(format!(
            "an area of {} bytes aligned to {}: it must be 16-byte sized and aligned to a power \
             of two of at least 16",
            area.size, area.align
        )));
    }
    let mut outgoing = 0;
    let mut calls = false;
    for block in func.layout.blocks() {
        for inst in func.layout.block_insts(block) {
            if let Some(sig) = call_signature(func, inst) {
                let size = abi::stack_args(&func.dfg.signatures[sig])
                    .map_err(|e| Error::at(inst, e.what))?;
                outgoing = outgoing.max(size);
                calls = true;
            } else if makes_tls_call(func, target, inst) {
                calls = true;
            }
        }
    }
    let sret = func.signature.uses_special_param(ArgumentPurpose::StructReturn);
    let private = if sret { 16 } else { 0 };
    let pad = area.align - 16;
    let fixed = u64::from(private) + u64::from(pad) + u64::from(area.size);
    let stack = fixed + u64::from(outgoing);
    // Cranelift's `maximum_frame_size` is the limit of its `i32` offsets.
    if stack + u64::from(SAVES) + 16 > i32::MAX as u64 {
        return Err(Error::new(format!("a frame of {stack} bytes, past 2 GB")));
    }
    let stack = stack as u32;
    if target.os == Os::Windows && stack >= (1 << 24) * 16 {
        return Err(Error::new(format!(
            "a frame of {stack} bytes on Windows, past the 256 MB an `alloc_l` unwind code says"
        )));
    }
    let probe = if target.probe_stack {
        probe(SAVES + stack + if calls { 16 } else { 0 }, target.probe_log2)?
    } else {
        Probe::None
    };
    Ok(Frame {
        stack,
        outgoing,
        base: outgoing + private,
        align: area.align,
        sret: sret.then_some(outgoing),
        probe,
        calls,
        windows: target.os == Os::Windows,
    })
}

/// The signature of `inst` when it is a call that returns to the function.
pub fn call_signature(func: &ir::Function, inst: ir::Inst) -> Option<ir::SigRef> {
    match func.dfg.insts[inst] {
        InstructionData::Call { func_ref, .. } | InstructionData::TryCall { func_ref, .. } => {
            Some(func.dfg.ext_funcs[func_ref].signature)
        }
        InstructionData::CallIndirect { sig_ref, .. } => Some(sig_ref),
        InstructionData::TryCallIndirect { exception, .. } => {
            Some(func.dfg.exception_tables[exception].signature())
        }
        _ => None,
    }
}

/// Whether `inst` finds a thread-local's address with a call (ELF's TLS descriptor, Mach-O's
/// `tlv` thunk), as Cranelift's `ElfTlsGetAddr` and `MachOTlsGetAddr` are calls.
fn makes_tls_call(func: &ir::Function, target: &Target, inst: ir::Inst) -> bool {
    let InstructionData::UnaryGlobalValue { global_value, .. } = func.dfg.insts[inst] else {
        return false;
    };
    matches!(target.tls, TlsModel::ElfGd | TlsModel::MachO) && reaches_tls(func, global_value)
}

/// Whether `gv` is, or is computed from, a thread-local symbol.
pub fn reaches_tls(func: &ir::Function, mut gv: ir::GlobalValue) -> bool {
    loop {
        match func.global_values[gv] {
            GlobalValueData::Symbol { tls, .. } => return tls,
            GlobalValueData::Load { base, .. } | GlobalValueData::IAddImm { base, .. } => gv = base,
            GlobalValueData::VMContext | GlobalValueData::DynScaleTargetConst { .. } => {
                return false;
            }
        }
    }
}

/// The probe of a frame of `size` bytes below the frame record, pages of `1 << log2`.
fn probe(size: u32, log2: u8) -> Result<Probe, Error> {
    if log2 >= 31 {
        return Err(Error::new(format!("a probe interval of 2^{log2} bytes")));
    }
    let guard = 1u32 << log2;
    let count = size / guard;
    if count == 0 {
        Ok(Probe::None)
    } else if count <= PROBE_MAX_UNROLL {
        Ok(Probe::Unroll { count, guard })
    } else if inst::imm12(u64::from(guard)).is_none() {
        Err(Error::new(format!(
            "a probe interval of {guard} bytes, which no `sub` immediate encodes (Cranelift's \
             `StackProbeLoop` needs one)"
        )))
    } else {
        Ok(Probe::Loop { size, guard })
    }
}

/// The rows a prologue records, at most 24 of them, without allocating.
pub struct Rows {
    rows: [(u32, Rule); 24],
    len: usize,
}

impl Default for Rows {
    fn default() -> Rows {
        Rows { rows: [(0, Rule::RememberState); 24], len: 0 }
    }
}

impl Rows {
    fn push(&mut self, at: u32, rule: Rule) {
        self.rows[self.len] = (at, rule);
        self.len += 1;
    }

    pub fn as_slice(&self) -> &[(u32, Rule)] {
        &self.rows[..self.len]
    }
}

/// The most bytes [`enter`] writes.
pub fn enter_bound(frame: &Frame) -> u32 {
    let probe = match frame.probe {
        Probe::None => 0,
        Probe::Unroll { count, .. } => count * (inst::ADD_ANY_MAX + 4) + inst::ADD_ANY_MAX,
        Probe::Loop { .. } => 4 + 16 + 16,
    };
    8 + probe + 4 * (X_PAIRS.len() + D_PAIRS.len()) as u32 + inst::ADD_ANY_MAX
}

/// Writes the frame record, the probe, the saves and the frame's allocation, with the rows
/// Cranelift's `create_unwind_info_from_insts` makes of the same prologue: the CFA from `sp`
/// once the record is pushed, from `x29` once the saves begin, and each saved register.
pub fn enter(buf: &mut Buf, frame: &Frame, rows: &mut Rows) {
    buf.put4(inst::stp_pre(FP, LR, SP, -16));
    let at = buf.len();
    rows.push(at, Rule::CfaOffset(16));
    rows.push(at, Rule::Offset(DWARF_FP, -16));
    rows.push(at, Rule::Offset(DWARF_LR, -8));
    buf.put4(inst::MOV_FP_SP);
    write_probe(buf, frame.probe);
    rows.push(buf.len(), Rule::CfaRegister(DWARF_FP));
    let to_cfa = (16 + SAVES) as i32;
    let mut clobber = SAVES as i32;
    for (a, b) in X_PAIRS {
        buf.put4(inst::stp_pre(a, b, SP, -16));
        clobber -= 16;
        let at = buf.len();
        rows.push(at, Rule::Offset(u16::from(a), clobber - to_cfa));
        rows.push(at, Rule::Offset(u16::from(b), clobber + 8 - to_cfa));
    }
    for (a, b) in D_PAIRS {
        buf.put4(inst::stp_d_pre(a, b, SP, -16));
        clobber -= 16;
        let at = buf.len();
        rows.push(at, Rule::Offset(DWARF_V0 + u16::from(a), clobber - to_cfa));
        rows.push(at, Rule::Offset(DWARF_V0 + u16::from(b), clobber + 8 - to_cfa));
    }
    if frame.stack > 0 {
        inst::add_any(buf, SP, SP, u64::from(frame.stack), true, X16);
    }
}

fn write_probe(buf: &mut Buf, probe: Probe) {
    match probe {
        Probe::None => {}
        Probe::Unroll { count, guard } => {
            for _ in 0..count {
                inst::add_any(buf, SP, SP, u64::from(guard), true, X16);
                buf.put4(inst::STUR_WZR_SP);
            }
            inst::add_any(buf, SP, SP, u64::from(guard * count), false, X16);
        }
        Probe::Loop { size, guard } => {
            let (step, shift) = inst::imm12(u64::from(guard)).expect("checked by `probe`");
            inst::mov_imm(buf, X16, 0);
            inst::mov_imm(buf, X17, u64::from(size));
            buf.put4(inst::add_imm(X16, X16, step, shift, true));
            buf.put4(inst::str_wzr_sp_reg(X16));
            buf.put4(inst::cmn(X16, X17));
            buf.put4(inst::b_cond_to(inst::GT, -3));
        }
    }
}

/// The most bytes [`exit`] writes.
pub const EXIT_MAX: u32 = inst::ADD_ANY_MAX + 4 * (D_PAIRS.len() + X_PAIRS.len()) as u32 + 8;

/// Frees the frame, restores the saved registers and the frame record and returns, as
/// `gen_epilogue` does. Like Cranelift's, it records no rows: the CFA stays said from `x29`.
pub fn exit(buf: &mut Buf, frame: &Frame) {
    if frame.stack > 0 {
        inst::add_any(buf, SP, SP, u64::from(frame.stack), false, X16);
    }
    for (a, b) in D_PAIRS.iter().rev() {
        buf.put4(inst::ldp_d_post(*a, *b, SP, 16));
    }
    for (a, b) in X_PAIRS.iter().rev() {
        buf.put4(inst::ldp_post(*a, *b, SP, 16));
    }
    buf.put4(inst::ldp_post(FP, LR, SP, 16));
    buf.put4(inst::RET);
}

/// The most bytes [`set_base`] writes.
pub const BASE_MAX: u32 = inst::ADD_ANY_MAX + 4;

/// Points `x20` at the area: `sp + base`, rounded up to the area's alignment when it asks for
/// more than `sp`'s 16.
pub fn set_base(buf: &mut Buf, frame: &Frame) {
    if frame.align > 16 {
        let up = u64::from(frame.base) + u64::from(frame.align) - 1;
        inst::add_any(buf, inst::X20, SP, up, false, X16);
        buf.put4(inst::and_align(inst::X20, inst::X20, frame.align));
    } else {
        inst::add_any(buf, inst::X20, SP, u64::from(frame.base), false, X16);
    }
}

/// The Windows ARM64 unwind codes of the prologue [`enter`] writes, as Cranelift's
/// `winarm64::create_unwind_info_from_insts` makes them and `UnwindInfo::emit` writes them:
/// last code first, each big-endian, padded with zero bytes to whole words.
pub fn windows_codes(frame: &Frame) -> Vec<u8> {
    // alloc: once per function, the unwind information it hands back.
    let mut codes: Vec<u8> = Vec::with_capacity(32);
    if frame.stack > 0 {
        let units = frame.stack / 16;
        if frame.stack < 32 * 16 {
            codes.push(units as u8);
        } else if frame.stack < 2048 * 16 {
            codes.extend_from_slice(&((0b11000 << 11) | units as u16).to_be_bytes());
        } else {
            codes.extend_from_slice(&((0b1110_0000 << 24) | units).to_be_bytes());
        }
    }
    // Each pair pushed with `[sp, #-16]!`: `save_fregp_x`/`save_regp_x` of 16 bytes.
    for (a, _) in D_PAIRS.iter().rev() {
        let code = 0b1101_1010_0000_0000u16 | u16::from(a - 8) << 6 | (16 / 8 - 1);
        codes.extend_from_slice(&code.to_be_bytes());
    }
    for (a, _) in X_PAIRS.iter().rev() {
        let code = 0b1100_1100_0000_0000u16 | u16::from(a - 19) << 6 | (16 / 8 - 1);
        codes.extend_from_slice(&code.to_be_bytes());
    }
    // `set_fp`, then `save_fplr_x` of 16 bytes.
    codes.push(0b1110_0001);
    codes.push(0b1000_0000 | (16 / 8 - 1));
    let words = codes.len().div_ceil(4);
    codes.resize(words * 4, 0);
    codes
}

/// Whether the frame's slots are said from `x29` or, realigned, from `x20` itself.
pub fn slot_base(frame: &Frame) -> (u16, i32) {
    if frame.align > 16 { (DWARF_X20, 0) } else { (DWARF_FP, frame.base_from_fp()) }
}

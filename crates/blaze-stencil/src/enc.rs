//! How a hole's field is encoded, and patching one: the relocation forms LLVM leaves in
//! stencils and the glue writes, for both architectures. Shared with the generator
//! (`build.rs` includes this file with `#[path]`); it uses nothing but `std`.
//!
//! A value is written as ELF computes a relocation's: `S + A` for an absolute form, and
//! `S + A - P` for a PC-relative one, `P` being the field's offset (x86-64) or the
//! instruction's (AArch64, whose fields are in 4-byte instructions). The patcher computes
//! that value and [`write`] puts it in the field, refusing one that does not fit.

use std::fmt;

/// The encoding of a hole's or a fixup's field.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum Form {
    /// 1 byte, the value as an unsigned or signed 8-bit integer (`R_X86_64_8`, from an
    /// `!absolute_symbol` range below 256).
    Abs8,
    /// 4 bytes, zero-extended (`R_X86_64_32`).
    Abs32,
    /// 4 bytes, sign-extended (`R_X86_64_32S`): displacements and immediates.
    Abs32S,
    /// 8 bytes (`R_X86_64_64`, `movabs`).
    Abs64,
    /// 4 bytes, `S + A - P` as a signed 32-bit integer (`R_X86_64_PC32`, `PLT32`): x86-64
    /// `jmp`/`jcc`/RIP-relative operands, and the entries of a jump table on both.
    Rel32,
    /// AArch64 `movz`/`movk` imm16, bits 0..16 of the value, checked below 2^16
    /// (`R_AARCH64_MOVW_UABS_G0`).
    A64MovwG0,
    /// Bits 0..16, unchecked (`MOVW_UABS_G0_NC`).
    A64MovwG0Nc,
    /// Bits 16..32, checked below 2^32 (`MOVW_UABS_G1`).
    A64MovwG1,
    A64MovwG1Nc,
    /// Bits 32..48, checked below 2^48 (`MOVW_UABS_G2`).
    A64MovwG2,
    A64MovwG2Nc,
    /// Bits 48..64 (`MOVW_UABS_G3`).
    A64MovwG3,
    /// AArch64 `add` imm12 of an absolute value, checked below 4096 (on an absolute value the
    /// whole value must fit, unlike ELF's `_NC` page offset).
    A64AddLo12,
    /// AArch64 `ldr`/`str` imm12 of an absolute value scaled by the access size (1, 2, 4,
    /// 8, 16 bytes): checked to be a multiple of it and below 4096 times it.
    A64Ldst8Lo12,
    A64Ldst16Lo12,
    A64Ldst32Lo12,
    A64Ldst64Lo12,
    A64Ldst128Lo12,
    /// AArch64 `b`/`bl` imm26, `(S + A - P) / 4` (`JUMP26`, `CALL26`): ±128 MB.
    A64Jump26,
    /// AArch64 `b.cond`/`cbz`/`cbnz` imm19 (`CONDBR19`): ±1 MB.
    A64CondBr19,
    /// AArch64 `tbz`/`tbnz` imm14 (`TSTBR14`): ±32 KB.
    A64TestBr14,
    /// AArch64 `adr` immlo:immhi (`ADR_PREL_LO21`): ±1 MB.
    A64AdrLo21,
    /// AArch64 `ldr` (literal) imm19 (`LD_PREL_LO19`): ±1 MB, word-aligned.
    A64LdrLit19,
}

impl Form {
    pub const ALL: [Form; 23] = [
        Form::Abs8,
        Form::Abs32,
        Form::Abs32S,
        Form::Abs64,
        Form::Rel32,
        Form::A64MovwG0,
        Form::A64MovwG0Nc,
        Form::A64MovwG1,
        Form::A64MovwG1Nc,
        Form::A64MovwG2,
        Form::A64MovwG2Nc,
        Form::A64MovwG3,
        Form::A64AddLo12,
        Form::A64Ldst8Lo12,
        Form::A64Ldst16Lo12,
        Form::A64Ldst32Lo12,
        Form::A64Ldst64Lo12,
        Form::A64Ldst128Lo12,
        Form::A64Jump26,
        Form::A64CondBr19,
        Form::A64TestBr14,
        Form::A64AdrLo21,
        Form::A64LdrLit19,
    ];

    pub const fn from_u8(it: u8) -> Option<Form> {
        if (it as usize) < Form::ALL.len() { Some(Form::ALL[it as usize]) } else { None }
    }

    /// Whether the value is `S + A - P`.
    pub const fn pc_relative(self) -> bool {
        matches!(
            self,
            Form::Rel32
                | Form::A64Jump26
                | Form::A64CondBr19
                | Form::A64TestBr14
                | Form::A64AdrLo21
                | Form::A64LdrLit19
        )
    }

    /// Bytes of code the field is in, from its offset: the field itself on x86-64, the
    /// instruction on AArch64.
    pub const fn bytes(self) -> u8 {
        match self {
            Form::Abs8 => 1,
            Form::Abs64 => 8,
            _ => 4,
        }
    }
}

/// A value that does not fit the field of its form.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Overflow {
    pub form: Form,
    pub value: i64,
}

impl fmt::Display for Overflow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} does not fit a {:?} field", self.value, self.form)
    }
}

/// Writes `value` into the field of `form` at `code[at..]`, keeping the instruction's other
/// bits. `code[at..at + form.bytes()]` must be in bounds (the library's holes are checked
/// against their stencil's length when it is loaded).
pub fn write(form: Form, code: &mut [u8], at: usize, value: i64) -> Result<(), Overflow> {
    let _ = (form, code, at, value);
    unimplemented!("fastcompile-stencil-patch: every form's encoding and range check")
}

/// The value the field of `form` at `code[at..]` holds, as [`write`] would have been given
/// it (the inverse, for tests and the generator's checks). For a scaled or partial form it is
/// the part the field holds, shifted back into place.
pub fn read(form: Form, code: &[u8], at: usize) -> i64 {
    let _ = (form, code, at);
    unimplemented!("fastcompile-stencil-patch: every form's decoding")
}

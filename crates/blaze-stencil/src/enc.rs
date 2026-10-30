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
#[inline]
pub fn write(form: Form, code: &mut [u8], at: usize, value: i64) -> Result<(), Overflow> {
    let overflow = Overflow { form, value };
    match form {
        Form::Abs8 => {
            if !(-128..=255).contains(&value) {
                return Err(overflow);
            }
            code[at] = value as u8;
        }
        Form::Abs32 => {
            if !(0..=i64::from(u32::MAX)).contains(&value) {
                return Err(overflow);
            }
            put4(code, at, value as u32);
        }
        Form::Abs32S | Form::Rel32 => {
            if i32::try_from(value).is_err() {
                return Err(overflow);
            }
            put4(code, at, value as i32 as u32);
        }
        Form::Abs64 => code[at..at + 8].copy_from_slice(&value.to_le_bytes()),
        _ => {
            let insn = a64(form, get4(code, at), value).ok_or(overflow)?;
            put4(code, at, insn);
        }
    }
    Ok(())
}

/// Whether `value` fits the field of `form`: what [`write`] would refuse, without code.
#[inline]
pub fn check(form: Form, value: i64) -> Result<(), Overflow> {
    let mut field = [0u8; 8];
    write(form, &mut field, 0, value)
}

/// The value the field of `form` at `code[at..]` holds, as [`write`] would have been given
/// it (the inverse, for tests and the generator's checks). For a scaled or partial form it is
/// the part the field holds, shifted back into place. `Abs8` and `Abs32` read zero-extended.
pub fn read(form: Form, code: &[u8], at: usize) -> i64 {
    let insn = || get4(code, at);
    let movw = |shift: u32| (u64::from((insn() >> 5) & 0xffff) << shift) as i64;
    let imm12 = || i64::from((insn() >> 10) & 0xfff);
    let signed = |bits: u32, shift: u32| {
        let field = (insn() >> shift) & ((1 << bits) - 1);
        sign_extend(i64::from(field), bits) << 2
    };
    match form {
        Form::Abs8 => i64::from(code[at]),
        Form::Abs32 => i64::from(get4(code, at)),
        Form::Abs32S | Form::Rel32 => i64::from(get4(code, at) as i32),
        Form::Abs64 => {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&code[at..at + 8]);
            i64::from_le_bytes(bytes)
        }
        Form::A64MovwG0 | Form::A64MovwG0Nc => movw(0),
        Form::A64MovwG1 | Form::A64MovwG1Nc => movw(16),
        Form::A64MovwG2 | Form::A64MovwG2Nc => movw(32),
        Form::A64MovwG3 => movw(48),
        Form::A64AddLo12 => imm12(),
        Form::A64Ldst8Lo12 => imm12(),
        Form::A64Ldst16Lo12 => imm12() * 2,
        Form::A64Ldst32Lo12 => imm12() * 4,
        Form::A64Ldst64Lo12 => imm12() * 8,
        Form::A64Ldst128Lo12 => imm12() * 16,
        Form::A64Jump26 => signed(26, 0),
        Form::A64CondBr19 | Form::A64LdrLit19 => signed(19, 5),
        Form::A64TestBr14 => signed(14, 5),
        Form::A64AdrLo21 => {
            let insn = insn();
            let lo = i64::from((insn >> 29) & 3);
            let hi = i64::from((insn >> 5) & 0x7ffff);
            sign_extend(hi << 2 | lo, 21)
        }
    }
}

/// `insn` with the field of the AArch64 `form` set to `value`, or `None` when it does not fit.
#[inline]
fn a64(form: Form, insn: u32, value: i64) -> Option<u32> {
    let unsigned = value as u64;
    let movw = |chunk: u64| (insn & !(0xffff << 5)) | (((chunk & 0xffff) as u32) << 5);
    let imm12 = |field: u64| (insn & !(0xfff << 10)) | ((field as u32) << 10);
    let scaled = |size: i64| {
        (value % size == 0 && (0..4096 * size).contains(&value)).then(|| imm12((value / size) as u64))
    };
    let branch = |bits: u32, shift: u32| {
        let words = value >> 2;
        let half = 1i64 << (bits - 1);
        (value & 3 == 0 && (-half..half).contains(&words)).then(|| {
            let mask = (1u32 << bits) - 1;
            (insn & !(mask << shift)) | (((words as u32) & mask) << shift)
        })
    };
    match form {
        Form::A64MovwG0 => (unsigned < 1 << 16).then(|| movw(unsigned)),
        Form::A64MovwG0Nc => Some(movw(unsigned)),
        Form::A64MovwG1 => (unsigned < 1 << 32).then(|| movw(unsigned >> 16)),
        Form::A64MovwG1Nc => Some(movw(unsigned >> 16)),
        Form::A64MovwG2 => (unsigned < 1 << 48).then(|| movw(unsigned >> 32)),
        Form::A64MovwG2Nc => Some(movw(unsigned >> 32)),
        Form::A64MovwG3 => Some(movw(unsigned >> 48)),
        Form::A64AddLo12 => (unsigned < 4096).then(|| imm12(unsigned)),
        Form::A64Ldst8Lo12 => scaled(1),
        Form::A64Ldst16Lo12 => scaled(2),
        Form::A64Ldst32Lo12 => scaled(4),
        Form::A64Ldst64Lo12 => scaled(8),
        Form::A64Ldst128Lo12 => scaled(16),
        Form::A64Jump26 => branch(26, 0),
        Form::A64CondBr19 | Form::A64LdrLit19 => branch(19, 5),
        Form::A64TestBr14 => branch(14, 5),
        Form::A64AdrLo21 => ((-(1i64 << 20))..(1i64 << 20)).contains(&value).then(|| {
            let lo = (value & 3) as u32;
            let hi = ((value >> 2) & 0x7ffff) as u32;
            (insn & !((3 << 29) | (0x7ffff << 5))) | (lo << 29) | (hi << 5)
        }),
        Form::Abs8 | Form::Abs32 | Form::Abs32S | Form::Abs64 | Form::Rel32 => None,
    }
}

const fn sign_extend(value: i64, bits: u32) -> i64 {
    let shift = 64 - bits;
    (value << shift) >> shift
}

#[inline]
fn get4(code: &[u8], at: usize) -> u32 {
    let mut bytes = [0u8; 4];
    bytes.copy_from_slice(&code[at..at + 4]);
    u32::from_le_bytes(bytes)
}

#[inline]
fn put4(code: &mut [u8], at: usize, it: u32) {
    code[at..at + 4].copy_from_slice(&it.to_le_bytes());
}

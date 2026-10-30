//! Copy-and-patch codegen for dev runs (docs/v2/plan/design/compile-time.md, change 4; the
//! design is docs/v2/plan/lanes/fastcompile/stencil.md).
//!
//! When blaze is built, LLVM compiles one *stencil* per Cranelift IR operation, operand shape
//! and type ([`shape`]): a few instructions in continuation-passing style, each ending in a
//! tail jump to the next, with *holes* where a frame offset, a constant or a jump target goes.
//! The generator (`gen/`, run by `build.rs`) records each hole as the relocation LLVM made for
//! it, and lays the stencils out as one library per architecture variant ([`table`]).
//!
//! Compiling a function ([`compile`]) is then: pick the stencil of each instruction, copy it
//! into one buffer, and patch its holes ([`patch`]); what no stencil can be (the prologue,
//! calls in the target's convention, returns, jump tables, traps, addresses of symbols) is
//! written by the architecture's glue ([`arch`]) exactly as Cranelift's code does it, so code
//! of this backend and of Cranelift call each other freely. What a function needs that neither
//! has is an error naming the instruction, never a quiet compile by Cranelift.
//!
//! The input is the same Cranelift IR the Cranelift backend compiles (`hir-ty-codegen`'s IR
//! builder, after ABI lowering), so every MIR construct lowers once, by one set of rules, and
//! both backends agree on what a program means. The output ([`code::Code`]) is what Cranelift's
//! compile gives the codegen lane: bytes, relocations by Cranelift's names, unwind rows, call
//! sites with landing pads, lines and named slots.
//!
//! Tier 0 code counts its calls and loop back edges ([`tier`]); the daemon recompiles what the
//! counts say is hot with Cranelift or LLVM and patches it into the function's slot.

// Denied, not forbidden: the one unsafe module is the tests' `tests/exec.rs`, which maps code
// and calls it.
#![deny(unsafe_code)]

pub mod arch;
pub mod buf;
pub mod code;
pub mod enc;
pub mod patch;
pub mod shape;
pub mod table;
pub mod tier;

#[cfg(test)]
mod tests;

use std::{cell::RefCell, fmt};

use cranelift_codegen::ir;

pub use code::Code;

/// The platform a function is compiled for, as its glue needs it: everything besides the
/// architecture that changes the bytes of a prologue, a call or an address. Each field is
/// the setting `hir-ty-codegen/target.rs` gives Cranelift for the same target.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Target {
    pub arch: blaze_codegen::Arch,
    pub os: Os,
    /// The CPU baseline Cranelift's ISA is given: which stencil library the code is made of.
    pub cpu: Cpu,
    /// Position-independent code: addresses of what is not colocated go through the GOT.
    pub pic: bool,
    pub tls: TlsModel,
    /// Frames larger than a page touch each page on the way down (Cranelift's inline
    /// `probestack`), so a stack overflow faults in the guard page.
    pub probe_stack: bool,
    /// log2 of the probe interval: Cranelift's `probestack_size_log2`.
    pub probe_log2: u8,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Os {
    Linux,
    MacOs,
    Windows,
}

/// The CPU baseline of a target: `hir-ty-codegen`'s `Target::baseline_settings`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Cpu {
    /// The architecture's baseline: x86-64 (with `cmpxchg16b`, which only 128-bit
    /// compare-and-swap uses), or AArch64 generic.
    Baseline,
    /// `apple-m1`: AArch64 with LSE atomics (Cranelift's `has_lse`).
    AppleM1,
}

/// How a thread-local's address is found: Cranelift's `tls_model` setting.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum TlsModel {
    /// ELF general dynamic (`__tls_get_addr`, or TLS descriptors on AArch64).
    ElfGd,
    /// Mach-O's thread-local variable descriptors.
    MachO,
    /// COFF's `_tls_index` and the TEB.
    Coff,
}

/// Why a function did not compile: the instruction (when one is to blame) and what of it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Error {
    pub inst: Option<ir::Inst>,
    pub what: String,
}

impl Error {
    pub fn new(what: impl Into<String>) -> Error {
        Error { inst: None, what: what.into() }
    }

    pub fn at(inst: ir::Inst, what: impl Into<String>) -> Error {
        Error { inst: Some(inst), what: what.into() }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.inst {
            Some(inst) => write!(f, "copy-and-patch: {inst}: {}", self.what),
            None => write!(f, "copy-and-patch: {}", self.what),
        }
    }
}

impl std::error::Error for Error {}

thread_local! {
    /// Each compiling thread's columns and buffer, cleared between functions, never freed.
    static SCRATCH: RefCell<patch::Scratch> = RefCell::new(patch::Scratch::new());
}

/// Compiles one function of Cranelift IR, after ABI lowering, for `target`, in this thread's
/// scratch.
pub fn compile(func: &ir::Function, target: &Target) -> Result<Code, Error> {
    SCRATCH.with(|scratch| match scratch.try_borrow_mut() {
        Ok(mut scratch) => compile_with(&mut scratch, func, target),
        // A compile that compiles another on its thread: only a panic unwinding through
        // `compile_with` and caught above could, and that one gets fresh columns.
        Err(_) => compile_with(&mut patch::Scratch::new(), func, target),
    })
}

/// [`compile`] in the caller's scratch.
pub fn compile_with(
    scratch: &mut patch::Scratch,
    func: &ir::Function,
    target: &Target,
) -> Result<Code, Error> {
    let library = table::Library::get(table::Variant::of(target)?)?;
    match target.arch {
        blaze_codegen::Arch::X86_64 => {
            patch::compile::<arch::x64::X64>(scratch, func, target, library)
        }
        blaze_codegen::Arch::Aarch64 => {
            patch::compile::<arch::aarch64::Aarch64>(scratch, func, target, library)
        }
        blaze_codegen::Arch::Wasm32 => {
            Err(Error::new("wasm32 has no stencils: its code is written from the IR as a module"))
        }
    }
}

/// The identity of the stencil library `target`'s code is made of: the BLAKE3 hash of its
/// bytes, which codegen hashes into the `Settings` of every stencil unit
/// (`Toolchain::stencils`), so a change to any stencil is a change to every key.
pub fn library_id(target: &Target) -> Result<[u8; 32], Error> {
    Ok(table::Library::get(table::Variant::of(target)?)?.id())
}

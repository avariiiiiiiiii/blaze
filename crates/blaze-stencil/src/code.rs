//! What a compile gives: the function's machine code with everything codegen turns into a
//! `CompiledFunction`, in the terms Cranelift's compile gives them (`hir-ty-codegen`'s
//! `fragment::Code`), so one conversion serves both backends:
//!
//! - relocations by Cranelift's [`binemit::Reloc`] kinds and names ([`Name`] mirrors
//!   `fragment::RelocName`);
//! - unwind rows as `unwind.rs`'s `FrameRows`/`FrameRule` ([`Rule`]), or Windows'
//!   `UNWIND_INFO` codes;
//! - call sites with their exception handlers, for the LSDA (`lsda::write`);
//! - source locations from each offset where they change, for `LineRow::of_ranges`;
//! - where named stack slots are, for `SlotPlaces`.

use cranelift_codegen::{binemit, ir};

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Code {
    pub bytes: Vec<u8>,
    /// A power of two.
    pub align: u32,
    /// By offset.
    pub relocs: Vec<Reloc>,
    pub unwind: Option<Unwind>,
    /// Every call the function makes that an unwind can pass through, by return offset.
    pub calls: Vec<CallSite>,
    /// The handlers of `calls`, each call's a range of them.
    pub handlers: Vec<Handler>,
    /// From each code offset where it changes, the source location the code is of
    /// (`ir::Function::srcloc` of its instruction), by offset.
    pub lines: Vec<(u32, ir::SourceLoc)>,
    /// Where each stack slot is, from the frame pointer.
    pub slots: Slots,
}

/// A place in the code the linker fills with an address.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Reloc {
    pub at: u32,
    pub kind: binemit::Reloc,
    pub name: Name,
    pub addend: i64,
}

/// What a relocation points at, as `fragment::RelocName` names it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Name {
    /// A user name of the function's IR (`ir::UserExternalName`): namespace 0 functions and
    /// 1 data, as the IR builder declares them, any other one of its own.
    User { namespace: u32, index: u32 },
    LibCall(ir::LibCall),
    Known(ir::KnownSymbol),
    /// The function itself, this many bytes into its code.
    Own(u32),
    /// The function's own tier counter cells ([`crate::tier`]): `Target::Counter` of its
    /// instance, placed writable by the linker.
    Counter,
}

/// A call an unwind can pass through: where it returns to, and which of
/// [`Code::handlers`] it has (none: an unwind goes on to the caller).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CallSite {
    /// The offset of the instruction after the call.
    pub ret: u32,
    pub handlers: std::ops::Range<u32>,
}

/// Where an exception lands, as Cranelift's `FinalizedMachExceptionHandler` says it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Handler {
    /// An exception with this tag (`ir::ExceptionTag::as_u32`) lands at this offset.
    Tag { tag: u32, landing: u32 },
    /// Every exception lands at this offset.
    Default { landing: u32 },
}

/// A function's unwind information.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Unwind {
    /// DWARF CFI (ELF, Mach-O): the rules from each offset, after the rules every function
    /// starts with (`FrameRule::on_entry`), covering `len` bytes.
    Dwarf { len: u32, rows: Vec<(u32, Rule)> },
    /// Windows x64: the `UNWIND_INFO` without a handler, covering `len` bytes.
    Windows { len: u32, info: Vec<u8> },
}

/// A DWARF call frame instruction, as `hir-ty-codegen/unwind.rs`'s `FrameRule`: registers
/// by DWARF number, offsets in bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Rule {
    Cfa(u16, i32),
    CfaRegister(u16),
    CfaOffset(i32),
    Offset(u16, i32),
    Restore(u16),
    SameValue(u16),
    Undefined(u16),
    RememberState,
    RestoreState,
    ArgsSize(u32),
}

/// Where stack slots are, for a debugger.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Slots {
    /// The DWARF number of the register the offsets are from: the frame pointer.
    pub base: u16,
    /// Each stack slot's offset from `base`, by `ir::StackSlot` number.
    pub offsets: Vec<i32>,
}

//! Per-function codegen (docs/v2/codegen.md section 9, docs/v2/items.md section 6.4).
//!
//! The unit of codegen is a function [`Instance`]: a generic's code hash and the facts of its
//! type arguments it reads, so identical instances across crates, branches and projects
//! compile once, and identical normalized MIR anywhere is one compiled function. Each compiles
//! to a [`CompiledFunction`]: its machine code with its relocations, keyed by its
//! [`InstanceKey`], with debug info and panic locations in a side table under the same key.
//!
//! Dev: Cranelift per function. Release: LLVM per function, with the callees it inlined
//! recorded ([`CompiledFunction::inlined`]) as deep-hash edges.
//!
//! Code dedupe never merges statics or anything whose address the program can compare as
//! distinct: those are [`Target::Static`], keyed by which item they are, never by content.

use blaze_format::SpanRef;
use blaze_ids::{ContentId, Hasher};
use blaze_items::{CodeHash, ItemKey};
use serde::{Deserialize, Serialize};

/// A function instance: what is compiled.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Instance {
    /// The generic's (or the plain function's) code hash.
    pub code: CodeHash,
    /// The content ID of the facts of its type arguments its code reads (layouts, impls
    /// selected, constants): not the arguments' names.
    pub args: ContentId,
    /// The codegen settings that change its machine code.
    pub settings: Settings,
}

/// An instance's key: its content ID, the store's key of its blob.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct InstanceKey(pub ContentId);

pub const INSTANCE_DOMAIN: &str = "blaze v2 instance 1";

impl Instance {
    pub fn key(&self) -> InstanceKey {
        let mut hasher = Hasher::new(INSTANCE_DOMAIN);
        hasher.id(&self.code.0).id(&self.args).id(&self.settings.0);
        InstanceKey(hasher.finish())
    }
}

/// The content ID of everything that changes machine code besides the function: target,
/// backend and its version, opt-level, overflow checks, panic strategy, debug-info level,
/// the toolchain. Built by the codegen lane; any setting left out is a stale-code bug.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Settings(pub ContentId);

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Backend {
    Cranelift,
    Llvm { opt: u8 },
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Arch {
    X86_64,
    Aarch64,
    Wasm32,
}

/// One unit of codegen: an instance and how to compile it.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Unit {
    pub instance: Instance,
    pub backend: Backend,
    pub arch: Arch,
    /// The item it is an instance of, for its name where the program observes it (symbol,
    /// `#[no_mangle]`) and for diagnostics; never part of [`Instance::key`].
    pub item: ItemKey,
}

/// A function's machine code, relocatable: placed anywhere by the linker, which applies
/// `relocs`.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct CompiledFunction {
    pub key: InstanceKey,
    pub arch: Arch,
    /// A power of two.
    pub align: u32,
    pub code: Vec<u8>,
    pub relocs: Vec<Reloc>,
    /// Read-only data only it refers to, deduplicated by content: [`Target::Data`].
    pub data: Vec<Data>,
    /// The callees whose bodies it inlined: a change to one's code reruns it (release).
    pub inlined: Vec<CodeHash>,
    /// Where its debug info and panic locations are, in the side table.
    pub side: Option<ContentId>,
}

/// Read-only bytes a function refers to: string literals, constants, vtables of known types.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Data {
    pub id: ContentId,
    pub align: u32,
    pub bytes: Vec<u8>,
    pub relocs: Vec<Reloc>,
}

/// A place in code or data the linker fills with an address.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Reloc {
    /// From the start of the code or data it is in.
    pub at: u32,
    pub kind: RelocKind,
    pub target: Target,
    pub addend: i64,
}

/// How the address is written, as the object formats name them.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum RelocKind {
    /// The absolute address, 64 bits.
    Abs64,
    /// x86-64: 32 bits relative to the end of the field (`R_X86_64_PC32`, `PLT32`).
    PcRel32,
    /// x86-64: the GOT entry's address, relative (`R_X86_64_GOTPCREL`).
    GotPcRel32,
    /// AArch64 `BL`/`B`: 26 bits of words (`R_AARCH64_CALL26`).
    Call26,
    /// AArch64 `ADRP`: the page, 21 bits (`R_AARCH64_ADR_PREL_PG_HI21`).
    AdrpPage21,
    /// AArch64 `ADD`/`LDR`: the low 12 bits (`R_AARCH64_ADD_ABS_LO12_NC`).
    PageOffset12,
    /// AArch64 `LDR` from the GOT (`R_AARCH64_ADR_GOT_PAGE`, `LD64_GOT_LO12_NC`).
    GotPage21,
    GotPageOffset12,
    /// Thread-local: the offset in the thread's block.
    TlsOffset,
    /// wasm: a function or table index, a memory address.
    WasmFunction,
    WasmTable,
    WasmMemory,
}

/// What a relocation points at.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum Target {
    /// Another compiled function, called through its jump-table slot so that a patch of it
    /// changes one slot.
    Function(InstanceKey),
    /// The same, called directly: release code, or a callee known not to be patched.
    Direct(InstanceKey),
    /// A static: by which item and instance it is, never merged by content.
    Static { item: ItemKey, instance: ContentId },
    /// Read-only data, merged by content.
    Data(ContentId),
    /// A symbol outside blaze's code: libc, a C dependency, the system.
    Extern(String),
    /// A position the program observes (`line!()`, `#[track_caller]`'s `Location`): the
    /// span side table's entry, filled at link time, so a reformat compiles nothing.
    Position(SpanRef),
}

/// Why a unit did not compile: names the instance and the item.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CodegenError {
    pub unit: InstanceKey,
    pub item: ItemKey,
    pub what: String,
}

/// A backend: compiles one unit, given the MIR the front end built for it.
pub trait Compile: Sync {
    /// The front end's MIR of an instance, as the codegen lane reads it (`hir-ty-mir`).
    type Mir<'a>;
    fn compile(
        &self,
        unit: &Unit,
        mir: Self::Mir<'_>,
    ) -> Result<CompiledFunction, CodegenError>;
}

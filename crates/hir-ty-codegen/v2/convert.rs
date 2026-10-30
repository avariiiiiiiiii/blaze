//! The IR builder's compiled code ([`fragment::Code`] and its [`Requests`]) as v2 keeps it
//! ([`CompiledFunction`]): every relocation at a target of the build, and the function's own
//! data by content, each object pointing only at the ones before it.

use blaze_codegen::{
    Arch, CompiledFunction, Data, InstanceKey, Line, Reloc, RelocKind, Target,
};
use blaze_format::SpanRef;
use blaze_ids::ContentId;
use cranelift_codegen::{binemit, isa::TargetIsa};
use cranelift_module::{ModuleReloc, ModuleRelocTarget};

use super::places::{PERSONALITY, libcall_symbol};
use crate::{
    ConstBlob, DataIdentity,
    fragment::{self, DataRequest, FuncRequest, Requests},
};

/// What a data request is in the build.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum DataTarget {
    /// A static, a symbol, a position.
    Target(Target),
    /// The function's own, made of the request's blob.
    Own,
    /// A `&str` of this text: `file!()`.
    Text(String),
    /// A `core::panic::Location` of no place the program has a span of.
    Nowhere,
}

/// What each request of a function is, by its number.
pub(crate) struct Resolve<'a> {
    pub(crate) func: &'a dyn Fn(u32, &FuncRequest) -> Result<Target, String>,
    pub(crate) data: &'a dyn Fn(u32, &DataRequest) -> Result<DataTarget, String>,
    /// A symbol the backend calls on its own: a libcall, the personality routine.
    pub(crate) symbol: &'a dyn Fn(&str) -> Result<Target, String>,
}

/// The function `code` is, `requests` being what its relocations name and `spans` what its
/// line rows do.
pub(crate) fn function(
    key: InstanceKey,
    arch: Arch,
    isa: &dyn TargetIsa,
    code: &fragment::Code,
    requests: &Requests,
    resolve: &Resolve<'_>,
    spans: &[SpanRef],
    side: Option<ContentId>,
) -> Result<CompiledFunction, String> {
    let word = word(arch)?;
    let mut own = OwnData { data: Vec::new(), word };
    let datas = own.requests(requests, resolve)?;
    let mut relocs = Vec::with_capacity(code.relocs.len());
    for reloc in &code.relocs {
        let elf = code.elf_types.iter().find(|it| it.0 == reloc.offset).map(|it| it.1);
        let kind = match elf {
            Some(elf) => elf_kind(elf)?,
            None => reloc_kind(reloc.kind)?,
        };
        let target = reloc_target(reloc, requests, &datas, resolve)?;
        relocs.push(Reloc { at: reloc.offset, kind, target, addend: reloc.addend });
    }
    let unwind = match &code.unwind {
        Some(unwind) => {
            let personality = || (resolve.symbol)(PERSONALITY);
            let (unwind, lsda) = super::unwind::convert(isa, unwind, personality)?;
            if let Some(lsda) = lsda {
                own.push(lsda);
            }
            Some(unwind)
        }
        None => None,
    };
    let lines = code
        .lines
        .iter()
        .map(|row| match spans.get(row.span as usize) {
            Some(&span) => Ok(Line { at: row.offset, span }),
            None => Err(format!("a line row of statement {}, which the body has no span of", row.span)),
        })
        .collect::<Result<_, String>>()?;
    let align = u32::try_from(code.align).map_err(|e| e.to_string())?;
    if !align.is_power_of_two() {
        return Err(format!("code aligned to {align}, not a power of two"));
    }
    Ok(CompiledFunction {
        key,
        arch,
        align,
        code: code.bytes.clone(),
        relocs,
        data: own.data,
        inlined: Vec::new(),
        side,
        unwind,
        lines,
    })
}

fn word(arch: Arch) -> Result<usize, String> {
    match arch {
        Arch::X86_64 | Arch::Aarch64 => Ok(8),
        Arch::Wasm32 => Err("wasm: v2 has no relocations of wasm's data yet".to_owned()),
    }
}

/// A function's own data, in the order it is made: an object points only at the ones before.
struct OwnData {
    data: Vec<Data>,
    word: usize,
}

impl OwnData {
    fn push(&mut self, data: Data) -> Target {
        let target = Target::Data(data.id);
        if !self.data.iter().any(|it| it.id == data.id) {
            self.data.push(data);
        }
        target
    }

    /// The target of each data request, making the function's own as they come.
    fn requests(&mut self, requests: &Requests, resolve: &Resolve<'_>) -> Result<Vec<Target>, String> {
        let mut targets: Vec<Target> = Vec::with_capacity(requests.datas.len());
        for (index, request) in requests.datas.iter().enumerate() {
            let target = match (resolve.data)(index as u32, request)? {
                DataTarget::Target(target) => target,
                DataTarget::Own => {
                    let blob = own_blob(request)?;
                    let data = self.blob(blob, index, &targets, requests, resolve)?;
                    self.push(data)
                }
                DataTarget::Text(text) => self.text(text),
                DataTarget::Nowhere => self.nowhere(),
            };
            targets.push(target);
        }
        Ok(targets)
    }

    fn blob(
        &mut self,
        blob: &ConstBlob,
        index: usize,
        before: &[Target],
        requests: &Requests,
        resolve: &Resolve<'_>,
    ) -> Result<Data, String> {
        if !blob.type_ids.is_empty() {
            return Err("a constant with a `TypeId` the adapter did not fill".to_owned());
        }
        let mut relocs = Vec::with_capacity(blob.relocs.len() + blob.fn_relocs.len());
        for &(at, data, addend) in &blob.relocs {
            let Some(target) = before.get(data.as_u32() as usize) else {
                return Err(format!("data {index} points at data {}, not one before it", data.as_u32()));
            };
            relocs.push(Reloc { at, kind: RelocKind::Abs64, target: target.clone(), addend });
        }
        for &(at, func) in &blob.fn_relocs {
            let request = requests
                .funcs
                .get(func.as_u32() as usize)
                .ok_or_else(|| format!("data {index} points at function {}, never requested", func.as_u32()))?;
            let target = (resolve.func)(func.as_u32(), request)?;
            relocs.push(Reloc { at, kind: RelocKind::Abs64, target, addend: 0 });
        }
        relocs.sort_by_key(|it| it.at);
        let align = u32::try_from(blob.align).map_err(|e| e.to_string())?;
        Ok(Data::new(align, blob.bytes.clone(), relocs))
    }

    /// The bytes of `text`, and the `&str` of them.
    fn text(&mut self, text: String) -> Target {
        let len = text.len() as u64;
        let bytes = self.push(Data::new(1, text.into_bytes(), Vec::new()));
        let mut fat = vec![0; 2 * self.word];
        fat[self.word..].copy_from_slice(&len.to_le_bytes()[..self.word]);
        let pointer = Reloc { at: 0, kind: RelocKind::Abs64, target: bytes, addend: 0 };
        self.push(Data::new(self.word as u32, fat, vec![pointer]))
    }

    /// A `core::panic::Location` of no file, as v1 makes it: `<blaze>`, line 0, column 0.
    fn nowhere(&mut self) -> Target {
        let file = "<blaze>";
        let mut name = file.as_bytes().to_vec();
        // The name is followed by a NUL that its length leaves out.
        name.push(0);
        let name = self.push(Data::new(1, name, Vec::new()));
        let mut bytes = vec![0u8; 2 * self.word + 8];
        bytes[self.word..2 * self.word]
            .copy_from_slice(&(file.len() as u64).to_le_bytes()[..self.word]);
        let pointer = Reloc { at: 0, kind: RelocKind::Abs64, target: name, addend: 0 };
        self.push(Data::new(self.word as u32, bytes, vec![pointer]))
    }
}

/// The blob of a request that is the function's own data.
fn own_blob(request: &DataRequest) -> Result<&ConstBlob, String> {
    match request {
        DataRequest::Blob { identity: DataIdentity::Writable, .. } => {
            Err("data a function writes to, which is no constant".to_owned())
        }
        DataRequest::Blob { blob, .. } | DataRequest::Vtable { blob, .. } => Ok(blob),
        _ => Err("a data request that has no blob taken as the function's own".to_owned()),
    }
}

fn reloc_target(
    reloc: &ModuleReloc,
    requests: &Requests,
    datas: &[Target],
    resolve: &Resolve<'_>,
) -> Result<Target, String> {
    match &reloc.name {
        &ModuleRelocTarget::User { namespace: 0, index } => {
            let request = requests
                .funcs
                .get(index as usize)
                .ok_or_else(|| format!("a relocation at function {index}, never requested"))?;
            (resolve.func)(index, request)
        }
        &ModuleRelocTarget::User { namespace: 1, index } => datas
            .get(index as usize)
            .cloned()
            .ok_or_else(|| format!("a relocation at data {index}, never requested")),
        &ModuleRelocTarget::User { namespace: fragment::INLINED, .. } => {
            Err("a relocation at a reference of a body built in".to_owned())
        }
        ModuleRelocTarget::User { namespace, .. } => {
            Err(format!("a relocation in the unknown namespace {namespace}"))
        }
        &ModuleRelocTarget::LibCall(libcall) => (resolve.symbol)(&libcall_symbol(libcall)),
        ModuleRelocTarget::KnownSymbol(symbol) => {
            Err(format!("a relocation at the linker's {symbol:?}, which v2 has no target for"))
        }
        ModuleRelocTarget::FunctionOffset(_, offset) => {
            Err(format!("a relocation at offset {offset} of the function itself"))
        }
    }
}

/// The kind of a relocation as v2 names it.
pub(crate) fn reloc_kind(kind: binemit::Reloc) -> Result<RelocKind, String> {
    use binemit::Reloc as R;
    Ok(match kind {
        R::Abs8 => RelocKind::Abs64,
        R::X86PCRel4 | R::X86CallPCRel4 | R::X86CallPLTRel4 => RelocKind::PcRel32,
        R::X86GOTPCRel4 => RelocKind::GotPcRel32,
        R::Arm64Call => RelocKind::Call26,
        R::Aarch64AdrPrelPgHi21 => RelocKind::AdrpPage21,
        R::Aarch64AddAbsLo12Nc => RelocKind::PageOffset12,
        R::Aarch64AdrGotPage21 => RelocKind::GotPage21,
        R::Aarch64Ld64GotLo12Nc => RelocKind::GotPageOffset12,
        other => return Err(format!("a {other:?} relocation, which v2 has no kind for")),
    })
}

/// The kind of a relocation whose ELF type the producer said.
pub(crate) fn elf_kind(elf: u32) -> Result<RelocKind, String> {
    use cranelift_object::object::elf;
    Ok(match elf {
        elf::R_X86_64_64 | elf::R_AARCH64_ABS64 => RelocKind::Abs64,
        elf::R_X86_64_PC32 | elf::R_X86_64_PLT32 => RelocKind::PcRel32,
        elf::R_X86_64_GOTPCREL | elf::R_X86_64_GOTPCRELX | elf::R_X86_64_REX_GOTPCRELX => {
            RelocKind::GotPcRel32
        }
        elf::R_AARCH64_CALL26 | elf::R_AARCH64_JUMP26 => RelocKind::Call26,
        elf::R_AARCH64_ADR_PREL_PG_HI21 => RelocKind::AdrpPage21,
        elf::R_AARCH64_ADD_ABS_LO12_NC => RelocKind::PageOffset12,
        elf::R_AARCH64_ADR_GOT_PAGE => RelocKind::GotPage21,
        elf::R_AARCH64_LD64_GOT_LO12_NC => RelocKind::GotPageOffset12,
        other => return Err(format!("a relocation of ELF type {other}, which v2 has no kind for")),
    })
}

#[cfg(test)]
mod tests;

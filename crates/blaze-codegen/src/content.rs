//! Content IDs of what codegen makes: a data object's (what [`Target::Data`] names, so equal
//! bytes are one object) and a compiled function's code (what code dedupe compares).

use blaze_format::SpanRef;
use blaze_ids::{ContentId, Hasher};

use crate::{Arch, CompiledFunction, Data, Line, Reloc, RelocKind, Target, Unwind};

pub const DATA_DOMAIN: &str = "blaze v2 data 1";
pub const CODE_DOMAIN: &str = "blaze v2 code 1";

/// The content ID of a read-only data object: its alignment, bytes and relocations. Two
/// objects with one ID are one object.
pub fn data_id(align: u32, bytes: &[u8], relocs: &[Reloc]) -> ContentId {
    let mut hasher = Hasher::new(DATA_DOMAIN);
    hasher.update(&align.to_le_bytes()).part(bytes);
    relocs_into(&mut hasher, relocs);
    hasher.finish()
}

impl Data {
    /// A data object, its ID made from what it is.
    pub fn new(align: u32, bytes: Vec<u8>, relocs: Vec<Reloc>) -> Data {
        Data { id: data_id(align, &bytes, &relocs), align, bytes, relocs }
    }
}

/// The content ID of what a compiled function does: its architecture, alignment, bytes,
/// relocations, data, unwind information and lines. Not its key, what it inlined or its side
/// table: two functions with one code ID run the same instructions against the same
/// addresses, unwind alike and show the same lines in a backtrace.
pub fn code_id(function: &CompiledFunction) -> ContentId {
    let mut hasher = Hasher::new(CODE_DOMAIN);
    hasher.update(&[arch_tag(function.arch)]);
    hasher.update(&function.align.to_le_bytes()).part(&function.code);
    relocs_into(&mut hasher, &function.relocs);
    hasher.update(&(function.data.len() as u64).to_le_bytes());
    for data in &function.data {
        hasher.id(&data.id);
    }
    match &function.unwind {
        None => hasher.update(&[0]),
        Some(Unwind::Dwarf { code_align, data_align, return_address, cie, fde, personality, lsda }) => {
            hasher.update(&[1]).update(&code_align.to_le_bytes()).update(&data_align.to_le_bytes());
            hasher.update(&return_address.to_le_bytes()).part(cie).part(fde);
            optional_target_into(&mut hasher, personality.as_ref());
            optional_id_into(&mut hasher, lsda.as_ref())
        }
        Some(Unwind::Windows { info, handler, lsda }) => {
            hasher.update(&[2]).part(info);
            optional_target_into(&mut hasher, handler.as_ref());
            optional_id_into(&mut hasher, lsda.as_ref())
        }
    };
    lines_into(&mut hasher, &function.lines);
    hasher.finish()
}

fn optional_target_into<'h>(hasher: &'h mut Hasher, target: Option<&Target>) -> &'h mut Hasher {
    match target {
        Some(target) => {
            hasher.update(&[1]);
            target_into(hasher, target);
            hasher
        }
        None => hasher.update(&[0]),
    }
}

fn optional_id_into<'h>(hasher: &'h mut Hasher, id: Option<&ContentId>) -> &'h mut Hasher {
    match id {
        Some(id) => hasher.update(&[1]).id(id),
        None => hasher.update(&[0]),
    }
}

pub(crate) fn lines_into(hasher: &mut Hasher, lines: &[Line]) {
    hasher.update(&(lines.len() as u64).to_le_bytes());
    for Line { at, span: SpanRef { anchor, index } } in lines {
        hasher.update(&at.to_le_bytes()).id(anchor).update(&index.to_le_bytes());
    }
}

pub(crate) fn relocs_into(hasher: &mut Hasher, relocs: &[Reloc]) {
    hasher.update(&(relocs.len() as u64).to_le_bytes());
    for reloc in relocs {
        hasher.update(&reloc.at.to_le_bytes());
        hasher.update(&[kind_tag(reloc.kind)]);
        hasher.update(&reloc.addend.to_le_bytes());
        target_into(hasher, &reloc.target);
    }
}

pub(crate) fn target_into(hasher: &mut Hasher, target: &Target) {
    match target {
        Target::Function(key) => hasher.update(&[0]).id(&key.0),
        Target::Direct(key) => hasher.update(&[1]).id(&key.0),
        Target::Static { item, instance } => hasher.update(&[2]).id(&item.0).id(instance),
        Target::Data(id) => hasher.update(&[3]).id(id),
        Target::Extern(name) => hasher.update(&[4]).part(name.as_bytes()),
        Target::Position(SpanRef { anchor, index }) => {
            hasher.update(&[5]).id(anchor).update(&index.to_le_bytes())
        }
        Target::Weak(name) => hasher.update(&[6]).part(name.as_bytes()),
    };
}

pub const TARGET_DOMAIN: &str = "blaze v2 target 1";

/// A relocation target's content ID, to set targets apart without comparing them.
pub fn target_id(target: &Target) -> ContentId {
    let mut hasher = Hasher::new(TARGET_DOMAIN);
    target_into(&mut hasher, target);
    hasher.finish()
}

pub(crate) fn arch_tag(arch: Arch) -> u8 {
    match arch {
        Arch::X86_64 => 0,
        Arch::Aarch64 => 1,
        Arch::Wasm32 => 2,
    }
}

pub(crate) fn arch_of(tag: u8) -> Option<Arch> {
    Some(match tag {
        0 => Arch::X86_64,
        1 => Arch::Aarch64,
        2 => Arch::Wasm32,
        _ => return None,
    })
}

/// Every kind, by its tag ([`kind_tag`]); a new kind goes at the end.
const KINDS: [RelocKind; 26] = [
    RelocKind::Abs64,
    RelocKind::PcRel32,
    RelocKind::GotPcRel32,
    RelocKind::Call26,
    RelocKind::AdrpPage21,
    RelocKind::PageOffset12,
    RelocKind::GotPage21,
    RelocKind::GotPageOffset12,
    RelocKind::TlsOffset,
    RelocKind::WasmFunction,
    RelocKind::WasmTable,
    RelocKind::WasmMemory,
    RelocKind::TlsGd32,
    RelocKind::TlsGotTpOff32,
    RelocKind::TlvRel32,
    RelocKind::TlvPage21,
    RelocKind::TlvPageOffset12,
    RelocKind::TlsDescPage21,
    RelocKind::TlsDescLd64Lo12,
    RelocKind::TlsDescAddLo12,
    RelocKind::TlsDescCall,
    RelocKind::TlsIePage21,
    RelocKind::TlsIeLo12,
    RelocKind::TpHi12,
    RelocKind::TpLo12,
    RelocKind::SectionRel32,
];

pub(crate) fn kind_tag(kind: RelocKind) -> u8 {
    match kind {
        RelocKind::Abs64 => 0,
        RelocKind::PcRel32 => 1,
        RelocKind::GotPcRel32 => 2,
        RelocKind::Call26 => 3,
        RelocKind::AdrpPage21 => 4,
        RelocKind::PageOffset12 => 5,
        RelocKind::GotPage21 => 6,
        RelocKind::GotPageOffset12 => 7,
        RelocKind::TlsOffset => 8,
        RelocKind::WasmFunction => 9,
        RelocKind::WasmTable => 10,
        RelocKind::WasmMemory => 11,
        RelocKind::TlsGd32 => 12,
        RelocKind::TlsGotTpOff32 => 13,
        RelocKind::TlvRel32 => 14,
        RelocKind::TlvPage21 => 15,
        RelocKind::TlvPageOffset12 => 16,
        RelocKind::TlsDescPage21 => 17,
        RelocKind::TlsDescLd64Lo12 => 18,
        RelocKind::TlsDescAddLo12 => 19,
        RelocKind::TlsDescCall => 20,
        RelocKind::TlsIePage21 => 21,
        RelocKind::TlsIeLo12 => 22,
        RelocKind::TpHi12 => 23,
        RelocKind::TpLo12 => 24,
        RelocKind::SectionRel32 => 25,
    }
}

pub(crate) fn kind_of(tag: u8) -> Option<RelocKind> {
    KINDS.get(tag as usize).copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InstanceKey;

    fn function(code: &[u8], relocs: Vec<Reloc>) -> CompiledFunction {
        CompiledFunction {
            key: InstanceKey(ContentId([7; 32])),
            arch: Arch::X86_64,
            align: 16,
            code: code.to_vec(),
            relocs,
            data: Vec::new(),
            inlined: Vec::new(),
            side: None,
            unwind: None,
            lines: Vec::new(),
        }
    }

    #[test]
    fn unwinding_and_lines_are_the_code() {
        let a = function(&[0xc3], Vec::new());
        let mut b = a.clone();
        b.unwind = Some(Unwind::Dwarf {
            code_align: 1,
            data_align: -8,
            return_address: 16,
            cie: vec![0x0c, 7, 8],
            fde: Vec::new(),
            personality: None,
            lsda: None,
        });
        let mut c = a.clone();
        c.lines = vec![Line { at: 0, span: SpanRef { anchor: ContentId([1; 32]), index: 0 } }];
        assert_ne!(code_id(&a), code_id(&b));
        assert_ne!(code_id(&a), code_id(&c));
    }

    #[test]
    fn every_kind_has_its_own_tag() {
        for tag in 0..KINDS.len() as u8 {
            assert_eq!(kind_tag(kind_of(tag).unwrap()), tag);
        }
        assert_eq!(kind_of(KINDS.len() as u8), None);
        let mut tags: Vec<u8> = KINDS.iter().map(|it| kind_tag(*it)).collect();
        tags.dedup();
        assert_eq!(tags.len(), KINDS.len());
    }

    #[test]
    fn the_key_is_not_the_code() {
        let a = function(&[0xc3], Vec::new());
        let mut b = a.clone();
        b.key = InstanceKey(ContentId([8; 32]));
        assert_eq!(code_id(&a), code_id(&b));
    }

    #[test]
    fn a_relocation_target_is_the_code() {
        let reloc = |n| Reloc {
            at: 1,
            kind: RelocKind::PcRel32,
            target: Target::Function(InstanceKey(ContentId([n; 32]))),
            addend: -4,
        };
        let a = function(&[0xe8, 0, 0, 0, 0], vec![reloc(1)]);
        let b = function(&[0xe8, 0, 0, 0, 0], vec![reloc(2)]);
        assert_ne!(code_id(&a), code_id(&b));
    }

    #[test]
    fn data_ids_frame_their_parts() {
        assert_ne!(data_id(1, b"ab", &[]), data_id(2, b"ab", &[]));
        assert_ne!(data_id(1, b"ab", &[]), data_id(1, b"abc", &[]));
    }
}

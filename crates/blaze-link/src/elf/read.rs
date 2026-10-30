//! Reading ELF native inputs: relocatable objects, `ar` archives, shared objects, and the GNU
//! linker scripts glibc ships as `libc.so`.
//!
//! Sections and symbols of an object keep their ELF indexes, so a symbol's section, a group's
//! members and a relocation's symbol are the ELF numbers unchanged.

mod archive;
mod script;
mod shared;
#[cfg(test)]
mod tests;

use std::collections::hash_map::Entry;

use blaze_codegen::Arch;
use object::Endianness;
use rustc_hash::FxHashMap;
use object::elf;
use object::read::elf::{FileHeader as _, Rela as _, SectionHeader as _, SectionTable, Sym as _};

use crate::NativeKind;
use crate::model::{
    Apply, Binding, Data, Group, NativeFile, NativeFixup, Object, Section, SectionKind, Symbol,
    SymbolDef, SymbolKind,
};

type Header = elf::FileHeader64<Endianness>;

/// Every ELF input blaze links is little-endian (x86-64, AArch64); [`header`] checks it.
const ENDIAN: Endianness = Endianness::Little;

/// Reads `bytes`, the input `name` the link line takes as `kind`.
pub(super) fn read(
    arch: Arch,
    name: &str,
    bytes: &[u8],
    kind: &NativeKind,
) -> Result<NativeFile, String> {
    let mut copy = Vec::new();
    let bytes = aligned(bytes, &mut copy);
    if bytes.starts_with(&object::archive::THIN_MAGIC) {
        return Err(format!(
            "{name}: a thin archive, whose members are files outside it: blaze links only \
             archives that hold their members"
        ));
    }
    if bytes.starts_with(&object::archive::MAGIC) {
        return match kind {
            NativeKind::Archive { .. } => archive::read(arch, name, bytes).map(NativeFile::Archive),
            _ => Err(mismatch(name, "an archive", kind)),
        };
    }
    if bytes.starts_with(&elf::ELFMAG) {
        let header = header(arch, name, bytes)?;
        return match (header.e_type(ENDIAN), kind) {
            (elf::ET_REL, NativeKind::Object) => object(arch, name, bytes).map(NativeFile::Object),
            (elf::ET_DYN, NativeKind::Shared { .. }) => {
                shared::read(name, bytes).map(NativeFile::Shared)
            }
            (elf::ET_REL, _) => Err(mismatch(name, "a relocatable object (ET_REL)", kind)),
            (elf::ET_DYN, _) => Err(mismatch(name, "a shared object (ET_DYN)", kind)),
            (e_type, _) => Err(format!(
                "{name}: an ELF file of type {e_type}, neither a relocatable object (ET_REL) \
                 nor a shared object (ET_DYN)"
            )),
        };
    }
    match kind {
        NativeKind::Shared { .. } => script::read(name, bytes).map(NativeFile::Shared),
        _ => Err(format!("{name}: neither an ELF file nor an archive, given as {}", taken(kind))),
    }
}

/// `object`'s ELF reader takes its structures in place, so it needs them as aligned as its
/// `U64`: 8 bytes, or 1 with its `unaligned` feature, when nothing is ever copied. An archive
/// member is only 2-aligned. The copy is one allocation and one `memcpy`, nothing zeroed.
fn aligned<'a>(bytes: &'a [u8], copy: &'a mut Vec<u8>) -> &'a [u8] {
    let align = align_of::<Header>();
    if bytes.as_ptr().align_offset(align) == 0 {
        return bytes;
    }
    *copy = Vec::with_capacity(bytes.len() + align - 1);
    let start = copy.as_ptr().align_offset(align);
    copy.resize(start, 0);
    copy.extend_from_slice(bytes);
    &copy[start..]
}

fn taken(kind: &NativeKind) -> &'static str {
    match kind {
        NativeKind::Object => "an object",
        NativeKind::Archive { .. } => "an archive",
        NativeKind::Shared { .. } => "a shared library",
    }
}

fn mismatch(name: &str, what: &str, kind: &NativeKind) -> String {
    format!("{name}: {what}, but the link line gives it as {}", taken(kind))
}

/// The ELF header of `bytes`, checked to be a 64-bit little-endian file of the output's machine.
fn header<'data>(arch: Arch, name: &str, bytes: &'data [u8]) -> Result<&'data Header, String> {
    let header =
        Header::parse(bytes).map_err(|error| format!("{name}: not a 64-bit ELF file: {error}"))?;
    if !header.is_little_endian() {
        return Err(format!("{name}: a big-endian ELF file"));
    }
    let (machine, label) = match arch {
        Arch::X86_64 => (elf::EM_X86_64, "x86-64"),
        Arch::Aarch64 => (elf::EM_AARCH64, "AArch64"),
        Arch::Wasm32 => return Err(format!("{name}: an ELF input to a wasm32 link")),
    };
    let found = header.e_machine(ENDIAN);
    if found != machine {
        return Err(format!(
            "{name}: an ELF file for machine {found} ({}), but the output is {label}",
            machine_name(found)
        ));
    }
    Ok(header)
}

fn machine_name(machine: u16) -> &'static str {
    match machine {
        elf::EM_X86_64 => "x86-64",
        elf::EM_AARCH64 => "AArch64",
        elf::EM_386 => "i386",
        elf::EM_ARM => "ARM",
        elf::EM_RISCV => "RISC-V",
        _ => "another machine",
    }
}

fn utf8(name: &str, what: &str, bytes: &[u8]) -> Result<String, String> {
    std::str::from_utf8(bytes).map(str::to_owned).map_err(|_| {
        format!("{name}: {what} {} is not UTF-8", String::from_utf8_lossy(bytes))
    })
}

/// The last component of a path: what a shared library without a `DT_SONAME`, or a linker
/// script, is known by.
fn file_name(path: &str) -> String {
    path.rsplit_once('/').map_or(path, |(_, file)| file).to_owned()
}

/// A symbol's kind; an IFUNC is a function here, and [`object`] refuses one.
fn kind(st_type: u8) -> SymbolKind {
    match st_type {
        elf::STT_FUNC | elf::STT_GNU_IFUNC => SymbolKind::Function,
        elf::STT_OBJECT | elf::STT_COMMON => SymbolKind::Data,
        elf::STT_TLS => SymbolKind::Tls,
        elf::STT_SECTION => SymbolKind::Section,
        _ => SymbolKind::Other,
    }
}

/// The names of the defined global and weak symbols of the relocatable object `bytes`, in
/// symbol table order: what an archive without a symbol table is indexed by.
fn defined(arch: Arch, name: &str, bytes: &[u8]) -> Result<Vec<String>, String> {
    let mut copy = Vec::new();
    let data = aligned(bytes, &mut copy);
    if !data.starts_with(&elf::ELFMAG) {
        return Err(format!("{name}: an archive member that is not an ELF object"));
    }
    let header = header(arch, name, data)?;
    if header.e_type(ENDIAN) != elf::ET_REL {
        return Err(format!("{name}: an archive member that is not a relocatable object (ET_REL)"));
    }
    let error = |error: object::read::Error| format!("{name}: {error}");
    let sections = header.sections(ENDIAN, data).map_err(error)?;
    let symtab = sections.symbols(ENDIAN, data, elf::SHT_SYMTAB).map_err(error)?;
    let mut names = Vec::new();
    for (index, symbol) in symtab.enumerate() {
        if index.0 == 0 || symbol.st_shndx(ENDIAN) == elf::SHN_UNDEF {
            continue;
        }
        if matches!(symbol.st_bind(), elf::STB_GLOBAL | elf::STB_WEAK | elf::STB_GNU_UNIQUE) {
            let bytes = symtab.symbol_name(ENDIAN, symbol).map_err(error)?;
            names.push(utf8(name, "the symbol name", bytes)?);
        }
    }
    Ok(names)
}

/// Reads a relocatable object.
fn object(arch: Arch, name: &str, data: &[u8]) -> Result<Object, String> {
    let header = header(arch, name, data)?;
    let error = |error: object::read::Error| format!("{name}: {error}");
    let sections = header.sections(ENDIAN, data).map_err(error)?;
    let symtab = sections.symbols(ENDIAN, data, elf::SHT_SYMTAB).map_err(error)?;

    let mut object = Object::default();
    for (index, section) in sections.enumerate() {
        object.sections.push(self::section(arch, name, &sections, index.0, section, data)?);
    }

    // `st_size` of each ELF symbol, for `R_X86_64_SIZE*`.
    let mut sizes = Vec::with_capacity(symtab.len());
    for (index, symbol) in symtab.enumerate() {
        sizes.push(symbol.st_size(ENDIAN));
        if index.0 == 0 {
            object.symbols.push(Symbol {
                name: String::new(),
                def: SymbolDef::Absolute(0),
                binding: Binding::Local,
                kind: SymbolKind::Other,
                hidden: false,
            });
            continue;
        }
        let symbol_name =
            utf8(name, "the symbol name", symtab.symbol_name(ENDIAN, symbol).map_err(error)?)?;
        let def = match symbol.st_shndx(ENDIAN) {
            elf::SHN_UNDEF => SymbolDef::Undefined,
            elf::SHN_ABS => SymbolDef::Absolute(symbol.st_value(ENDIAN)),
            elf::SHN_COMMON => SymbolDef::Common {
                size: symbol.st_size(ENDIAN),
                align: symbol.st_value(ENDIAN),
            },
            shndx => {
                let section = if shndx == elf::SHN_XINDEX {
                    symtab
                        .symbol_section(ENDIAN, symbol, index)
                        .map_err(error)?
                        .ok_or_else(|| {
                            format!("{name}: symbol {symbol_name} has no extended section index")
                        })?
                        .0
                } else if shndx < elf::SHN_LORESERVE {
                    usize::from(shndx)
                } else {
                    return Err(format!(
                        "{name}: symbol {symbol_name} is in reserved section {shndx:#x}"
                    ));
                };
                if section >= object.sections.len() {
                    return Err(format!(
                        "{name}: symbol {symbol_name} is in section {section}, which the object \
                         does not have"
                    ));
                }
                SymbolDef::Section {
                    section: section as u32,
                    value: symbol.st_value(ENDIAN),
                    size: symbol.st_size(ENDIAN),
                }
            }
        };
        let binding = match symbol.st_bind() {
            elf::STB_LOCAL => Binding::Local,
            elf::STB_WEAK => Binding::Weak,
            elf::STB_GLOBAL | elf::STB_GNU_UNIQUE => Binding::Global,
            other => {
                return Err(format!("{name}: symbol {symbol_name} has binding {other}"));
            }
        };
        if symbol.st_type() == elf::STT_GNU_IFUNC {
            return Err(format!(
                "{name}: symbol {symbol_name} is an IFUNC (STT_GNU_IFUNC), which blaze-link does \
                 not link yet: it has no IRELATIVE relocation to resolve one at load"
            ));
        }
        object.symbols.push(Symbol {
            name: symbol_name,
            def,
            binding,
            kind: kind(symbol.st_type()),
            hidden: matches!(symbol.st_visibility(), elf::STV_HIDDEN | elf::STV_INTERNAL),
        });
    }

    for (index, section) in sections.enumerate() {
        let Some((flag, members)) = section.group(ENDIAN, data).map_err(error)? else {
            continue;
        };
        if flag & elf::GRP_COMDAT == 0 {
            continue;
        }
        let group = &object.sections[index.0].name;
        if section.sh_link(ENDIAN) as usize != symtab.section().0 {
            return Err(format!("{name}: group {group} names a symbol table other than .symtab"));
        }
        let signature = section.sh_info(ENDIAN) as usize;
        let Some(symbol) = object.symbols.get(signature).filter(|_| signature != 0) else {
            return Err(format!(
                "{name}: group {group}'s signature is symbol {signature}, which the object does \
                 not have"
            ));
        };
        let group_name = match symbol.def {
            SymbolDef::Section { section, .. } if symbol.kind == SymbolKind::Section => {
                object.sections[section as usize].name.clone()
            }
            _ => symbol.name.clone(),
        };
        let mut indexes = Vec::with_capacity(members.len());
        for member in members {
            let member = member.get(ENDIAN);
            if member as usize >= object.sections.len() || member == 0 {
                return Err(format!(
                    "{name}: group {group_name} names section {member}, which the object does \
                     not have"
                ));
            }
            indexes.push(member);
        }
        object.groups.push(Group { name: group_name, sections: indexes });
    }

    let mut added = Added::default();
    for (index, section) in sections.enumerate() {
        let sh_type = section.sh_type(ENDIAN);
        if !matches!(sh_type, elf::SHT_RELA | elf::SHT_REL | elf::SHT_CREL) {
            continue;
        }
        let relocations = &object.sections[index.0].name;
        let target = section.sh_info(ENDIAN) as usize;
        let Some(applied) = object.sections.get(target).filter(|_| target != 0) else {
            return Err(format!(
                "{name}: relocation section {relocations} applies to section {target}, which \
                 the object does not have"
            ));
        };
        if applied.what == SectionKind::Dropped {
            continue;
        }
        if sh_type != elf::SHT_RELA {
            return Err(format!(
                "{name}: section {} has {} relocations ({relocations}): x86-64 and AArch64 \
                 objects carry their addends (SHT_RELA)",
                applied.name,
                if sh_type == elf::SHT_REL { "SHT_REL" } else { "SHT_CREL" }
            ));
        }
        if section.sh_link(ENDIAN) as usize != symtab.section().0 {
            return Err(format!(
                "{name}: relocation section {relocations} names a symbol table other than .symtab"
            ));
        }
        let Some((relas, _)) = section.rela(ENDIAN, data).map_err(error)? else {
            continue;
        };
        // Taken out while the relocations are read, so the added symbols can be pushed and
        // an error can still name the section, without a copy of either.
        let mut fixups = std::mem::take(&mut object.sections[target].fixups);
        fixups.reserve_exact(relas.len());
        for rela in relas {
            let at = rela.r_offset(ENDIAN);
            let r_type = rela.r_type(ENDIAN, false);
            let symbol = rela.r_sym(ENDIAN, false);
            let addend = rela.r_addend(ENDIAN);
            let sections = &object.sections;
            let place = || {
                format!(
                    "{name}: section {}: relocation {} ({r_type}) at {at:#x}",
                    sections[target].name,
                    relocation_name(arch, r_type)
                )
            };
            if symbol as usize >= sizes.len() {
                return Err(format!("{}: symbol {symbol}, which the object does not have", place()));
            }
            let how = match arch {
                Arch::X86_64 => x86_64(r_type),
                Arch::Aarch64 => aarch64(r_type),
                Arch::Wasm32 => Err("an ELF relocation in a wasm32 link"),
            }
            .map_err(|why| format!("{}: {why}", place()))?;
            let fixup = |apply, symbol, minus| NativeFixup { at, apply, symbol, addend, minus };
            fixups.push(match how {
                How::Nothing => continue,
                How::To(apply) => fixup(apply, symbol, None),
                How::Module(apply) => fixup(apply, added.module(&mut object.symbols), None),
                How::Size(apply) => {
                    let size = added
                        .size(&mut object.symbols, &sizes, symbol)
                        .map_err(|why| format!("{}: {why}", place()))?;
                    fixup(apply, size, None)
                }
                How::GotBase(apply) => fixup(apply, added.got(&mut object.symbols), None),
                How::GotOff(apply) => {
                    let got = added.got(&mut object.symbols);
                    fixup(apply, symbol, Some(got))
                }
            });
        }
        object.sections[target].fixups = fixups;
    }
    Ok(object)
}

/// The section at ELF index `index`, without its fixups.
fn section(
    arch: Arch,
    name: &str,
    sections: &SectionTable<'_, Header>,
    index: usize,
    section: &elf::SectionHeader64<Endianness>,
    data: &[u8],
) -> Result<Section, String> {
    let sh_type = section.sh_type(ENDIAN);
    let flags = section.sh_flags(ENDIAN);
    let flag = |flag: u32| flags & u64::from(flag) != 0;
    let section_name = utf8(
        name,
        "the section name",
        sections.section_name(ENDIAN, section).map_err(|error| format!("{name}: {error}"))?,
    )?;
    let align = section.sh_addralign(ENDIAN).max(1);
    let dropped = index == 0
        || !flag(elf::SHF_ALLOC)
        || flag(elf::SHF_EXCLUDE)
        || matches!(
            sh_type,
            elf::SHT_GROUP
                | elf::SHT_SYMTAB
                | elf::SHT_SYMTAB_SHNDX
                | elf::SHT_STRTAB
                | elf::SHT_RELA
                | elf::SHT_REL
                | elf::SHT_NOTE
        )
        || section_name.starts_with(".note")
        || section_name == ".comment"
        || section_name == ".llvm_addrsig"
        || section_name.starts_with(".debug_");
    if dropped {
        return Ok(Section {
            name: section_name,
            what: SectionKind::Dropped,
            align,
            data: Data::Zero(0),
            fixups: Vec::new(),
            retain: false,
        });
    }
    if flag(elf::SHF_COMPRESSED) {
        return Err(format!(
            "{name}: section {section_name} is compressed (SHF_COMPRESSED), which a loaded \
             section may not be"
        ));
    }
    let what = if section_name == ".eh_frame"
        || (arch == Arch::X86_64 && sh_type == elf::SHT_X86_64_UNWIND)
    {
        SectionKind::EhFrame
    } else {
        let (class, rank) = super::section(
            &section_name,
            flag(elf::SHF_WRITE),
            flag(elf::SHF_EXECINSTR),
            flag(elf::SHF_TLS),
        )
        .map_err(|why| format!("{name}: section {section_name}: {why}"))?;
        SectionKind::Alloc(class, rank)
    };
    let data = if sh_type == elf::SHT_NOBITS {
        Data::Zero(section.sh_size(ENDIAN))
    } else {
        Data::Bytes(
            section
                .data(ENDIAN, data)
                .map_err(|error| format!("{name}: section {section_name}: {error}"))?
                .to_vec(),
        )
    };
    Ok(Section {
        name: section_name,
        what,
        align,
        data,
        fixups: Vec::new(),
        retain: flag(elf::SHF_GNU_RETAIN),
    })
}

/// What a relocation type becomes.
enum How {
    Nothing,
    To(Apply),
    /// Against the object's module symbol: offset 0 of its TLS block, whose GD pair is the
    /// module's (`R_X86_64_TLSLD`).
    Module(Apply),
    /// Against an absolute symbol of the target's size (`R_X86_64_SIZE*`).
    Size(Apply),
    /// Against `_GLOBAL_OFFSET_TABLE_` (`R_X86_64_GOTPC*`).
    GotBase(Apply),
    /// Against the target, minus `_GLOBAL_OFFSET_TABLE_` (`R_X86_64_GOTOFF64`).
    GotOff(Apply),
}

fn x86_64(r_type: u32) -> Result<How, &'static str> {
    Ok(match r_type {
        elf::R_X86_64_NONE => How::Nothing,
        elf::R_X86_64_64 => How::To(Apply::Abs64),
        elf::R_X86_64_32 => How::To(Apply::Abs32),
        elf::R_X86_64_32S => How::To(Apply::Abs32S),
        elf::R_X86_64_PC32 => How::To(Apply::Rel32),
        elf::R_X86_64_PC64 => How::To(Apply::Rel64),
        elf::R_X86_64_PLT32 => How::To(Apply::X86Branch32),
        elf::R_X86_64_GOTPCREL | elf::R_X86_64_GOTPCRELX | elf::R_X86_64_REX_GOTPCRELX => {
            How::To(Apply::X86GotPcRel32)
        }
        elf::R_X86_64_TPOFF32 => How::To(Apply::X86TpOff32),
        elf::R_X86_64_GOTTPOFF => How::To(Apply::X86GotTpOff32),
        elf::R_X86_64_TLSGD => How::To(Apply::X86TlsGd32),
        elf::R_X86_64_TLSLD => How::Module(Apply::X86TlsGd32),
        elf::R_X86_64_DTPOFF32 => How::To(Apply::DtpOff32),
        elf::R_X86_64_DTPOFF64 => How::To(Apply::DtpOff64),
        elf::R_X86_64_SIZE32 => How::Size(Apply::Abs32),
        elf::R_X86_64_SIZE64 => How::Size(Apply::Abs64),
        elf::R_X86_64_GOTPC32 => How::GotBase(Apply::Rel32),
        elf::R_X86_64_GOTPC64 => How::GotBase(Apply::Rel64),
        elf::R_X86_64_GOTOFF64 => How::GotOff(Apply::Delta64),
        elf::R_X86_64_8 | elf::R_X86_64_16 | elf::R_X86_64_PC8 | elf::R_X86_64_PC16 => {
            How::To(Apply::Native(r_type))
        }
        elf::R_X86_64_GOTPC32_TLSDESC | elf::R_X86_64_TLSDESC_CALL => {
            return Err("TLS descriptors (-mtls-dialect=gnu2) are not supported");
        }
        elf::R_X86_64_GOT32
        | elf::R_X86_64_GOT64
        | elf::R_X86_64_GOTPCREL64
        | elf::R_X86_64_GOTPLT64
        | elf::R_X86_64_PLTOFF64 => return Err("the large code model is not supported"),
        _ => return Err("not a relocation type blaze-link reads"),
    })
}

fn aarch64(r_type: u32) -> Result<How, &'static str> {
    Ok(How::To(match r_type {
        elf::R_AARCH64_NONE => return Ok(How::Nothing),
        elf::R_AARCH64_ABS64 => Apply::Abs64,
        elf::R_AARCH64_ABS32 => Apply::Abs32,
        elf::R_AARCH64_PREL64 => Apply::Rel64,
        elf::R_AARCH64_PREL32 => Apply::Rel32,
        elf::R_AARCH64_CALL26 => Apply::A64Call26,
        elf::R_AARCH64_JUMP26 => Apply::A64Jump26,
        elf::R_AARCH64_ADR_PREL_PG_HI21 => Apply::A64Page21,
        elf::R_AARCH64_ADR_PREL_LO21 => Apply::A64Adr21,
        elf::R_AARCH64_ADD_ABS_LO12_NC | elf::R_AARCH64_LDST8_ABS_LO12_NC => {
            Apply::A64Lo12 { shift: 0 }
        }
        elf::R_AARCH64_LDST16_ABS_LO12_NC => Apply::A64Lo12 { shift: 1 },
        elf::R_AARCH64_LDST32_ABS_LO12_NC => Apply::A64Lo12 { shift: 2 },
        elf::R_AARCH64_LDST64_ABS_LO12_NC => Apply::A64Lo12 { shift: 3 },
        elf::R_AARCH64_LDST128_ABS_LO12_NC => Apply::A64Lo12 { shift: 4 },
        elf::R_AARCH64_CONDBR19 => Apply::A64CondBr19,
        elf::R_AARCH64_TSTBR14 => Apply::A64TstBr14,
        elf::R_AARCH64_MOVW_UABS_G0 | elf::R_AARCH64_MOVW_UABS_G0_NC => Apply::A64MovW { group: 0 },
        elf::R_AARCH64_MOVW_UABS_G1 | elf::R_AARCH64_MOVW_UABS_G1_NC => Apply::A64MovW { group: 1 },
        elf::R_AARCH64_MOVW_UABS_G2 | elf::R_AARCH64_MOVW_UABS_G2_NC => Apply::A64MovW { group: 2 },
        elf::R_AARCH64_MOVW_UABS_G3 => Apply::A64MovW { group: 3 },
        elf::R_AARCH64_ADR_GOT_PAGE => Apply::A64GotPage21,
        elf::R_AARCH64_LD64_GOT_LO12_NC => Apply::A64GotLo12,
        elf::R_AARCH64_TLSLE_ADD_TPREL_HI12 => Apply::A64TpHi12,
        elf::R_AARCH64_TLSLE_ADD_TPREL_LO12 | elf::R_AARCH64_TLSLE_ADD_TPREL_LO12_NC => {
            Apply::A64TpLo12
        }
        elf::R_AARCH64_TLSIE_ADR_GOTTPREL_PAGE21 => Apply::A64TlsIePage21,
        elf::R_AARCH64_TLSIE_LD64_GOTTPREL_LO12_NC => Apply::A64TlsIeLo12,
        elf::R_AARCH64_TLSDESC_ADR_PAGE21 => Apply::A64TlsDescPage21,
        elf::R_AARCH64_TLSDESC_LD64_LO12 => Apply::A64TlsDescLo12,
        elf::R_AARCH64_TLSDESC_ADD_LO12 => Apply::A64TlsDescAdd12,
        elf::R_AARCH64_TLSDESC_CALL => Apply::A64TlsDescCall,
        elf::R_AARCH64_ABS16
        | elf::R_AARCH64_PREL16
        | elf::R_AARCH64_ADR_PREL_PG_HI21_NC
        | elf::R_AARCH64_LD_PREL_LO19
        | elf::R_AARCH64_MOVW_SABS_G0
        | elf::R_AARCH64_MOVW_SABS_G1
        | elf::R_AARCH64_MOVW_SABS_G2
        | elf::R_AARCH64_MOVW_PREL_G0
        | elf::R_AARCH64_MOVW_PREL_G0_NC
        | elf::R_AARCH64_MOVW_PREL_G1
        | elf::R_AARCH64_MOVW_PREL_G1_NC
        | elf::R_AARCH64_MOVW_PREL_G2
        | elf::R_AARCH64_MOVW_PREL_G2_NC
        | elf::R_AARCH64_MOVW_PREL_G3 => Apply::Native(r_type),
        _ => return Err("not a relocation type blaze-link reads"),
    }))
}

/// The local and undefined symbols an object's relocations add after its ELF ones.
#[derive(Default)]
struct Added {
    module: Option<u32>,
    got: Option<u32>,
    /// By the ELF symbol whose size each holds.
    sizes: FxHashMap<u32, u32>,
}

impl Added {
    fn push(symbols: &mut Vec<Symbol>, symbol: Symbol) -> u32 {
        symbols.push(symbol);
        (symbols.len() - 1) as u32
    }

    fn module(&mut self, symbols: &mut Vec<Symbol>) -> u32 {
        *self.module.get_or_insert_with(|| {
            Self::push(symbols, Symbol {
                name: String::new(),
                def: SymbolDef::Absolute(0),
                binding: Binding::Local,
                kind: SymbolKind::Tls,
                hidden: true,
            })
        })
    }

    fn got(&mut self, symbols: &mut Vec<Symbol>) -> u32 {
        *self.got.get_or_insert_with(|| {
            Self::push(symbols, Symbol {
                name: "_GLOBAL_OFFSET_TABLE_".to_owned(),
                def: SymbolDef::Undefined,
                binding: Binding::Global,
                kind: SymbolKind::Other,
                hidden: false,
            })
        })
    }

    /// A symbol whose value is the size of ELF symbol `target`, which must be this object's and
    /// final: a weak or common one may be replaced by a bigger one elsewhere.
    fn size(
        &mut self,
        symbols: &mut Vec<Symbol>,
        sizes: &[u64],
        target: u32,
    ) -> Result<u32, String> {
        let entry = match self.sizes.entry(target) {
            Entry::Occupied(it) => return Ok(*it.get()),
            Entry::Vacant(it) => it,
        };
        let of = &symbols[target as usize];
        match (of.def, of.binding) {
            (SymbolDef::Undefined, _) => {
                return Err(format!(
                    "the size of {}, which the object does not define",
                    of.name
                ));
            }
            (SymbolDef::Common { .. }, _) => {
                return Err(format!(
                    "the size of common symbol {}, which is known only once the link merges it",
                    of.name
                ));
            }
            (_, Binding::Weak) => {
                return Err(format!(
                    "the size of weak symbol {}, which another object may replace",
                    of.name
                ));
            }
            _ => {}
        }
        let symbol = Self::push(symbols, Symbol {
            name: String::new(),
            def: SymbolDef::Absolute(sizes[target as usize]),
            binding: Binding::Local,
            kind: SymbolKind::Other,
            hidden: true,
        });
        Ok(*entry.insert(symbol))
    }
}

macro_rules! names {
    ($($name:ident),* $(,)?) => {
        &[$((elf::$name, stringify!($name))),*]
    };
}

const X86_64_NAMES: &[(u32, &str)] = names![
    R_X86_64_NONE, R_X86_64_64, R_X86_64_PC32, R_X86_64_GOT32, R_X86_64_PLT32, R_X86_64_COPY,
    R_X86_64_GLOB_DAT, R_X86_64_JUMP_SLOT, R_X86_64_RELATIVE, R_X86_64_GOTPCREL, R_X86_64_32,
    R_X86_64_32S, R_X86_64_16, R_X86_64_PC16, R_X86_64_8, R_X86_64_PC8, R_X86_64_DTPMOD64,
    R_X86_64_DTPOFF64, R_X86_64_TPOFF64, R_X86_64_TLSGD, R_X86_64_TLSLD, R_X86_64_DTPOFF32,
    R_X86_64_GOTTPOFF, R_X86_64_TPOFF32, R_X86_64_PC64, R_X86_64_GOTOFF64, R_X86_64_GOTPC32,
    R_X86_64_GOT64, R_X86_64_GOTPCREL64, R_X86_64_GOTPC64, R_X86_64_GOTPLT64, R_X86_64_PLTOFF64,
    R_X86_64_SIZE32, R_X86_64_SIZE64, R_X86_64_GOTPC32_TLSDESC, R_X86_64_TLSDESC_CALL,
    R_X86_64_TLSDESC, R_X86_64_IRELATIVE, R_X86_64_RELATIVE64, R_X86_64_GOTPCRELX,
    R_X86_64_REX_GOTPCRELX,
];

const AARCH64_NAMES: &[(u32, &str)] = names![
    R_AARCH64_NONE, R_AARCH64_ABS64, R_AARCH64_ABS32, R_AARCH64_ABS16, R_AARCH64_PREL64,
    R_AARCH64_PREL32, R_AARCH64_PREL16, R_AARCH64_MOVW_UABS_G0, R_AARCH64_MOVW_UABS_G0_NC,
    R_AARCH64_MOVW_UABS_G1, R_AARCH64_MOVW_UABS_G1_NC, R_AARCH64_MOVW_UABS_G2,
    R_AARCH64_MOVW_UABS_G2_NC, R_AARCH64_MOVW_UABS_G3, R_AARCH64_MOVW_SABS_G0,
    R_AARCH64_MOVW_SABS_G1, R_AARCH64_MOVW_SABS_G2, R_AARCH64_LD_PREL_LO19,
    R_AARCH64_ADR_PREL_LO21, R_AARCH64_ADR_PREL_PG_HI21, R_AARCH64_ADR_PREL_PG_HI21_NC,
    R_AARCH64_ADD_ABS_LO12_NC, R_AARCH64_LDST8_ABS_LO12_NC, R_AARCH64_TSTBR14,
    R_AARCH64_CONDBR19, R_AARCH64_JUMP26, R_AARCH64_CALL26, R_AARCH64_LDST16_ABS_LO12_NC,
    R_AARCH64_LDST32_ABS_LO12_NC, R_AARCH64_LDST64_ABS_LO12_NC, R_AARCH64_MOVW_PREL_G0,
    R_AARCH64_MOVW_PREL_G0_NC, R_AARCH64_MOVW_PREL_G1, R_AARCH64_MOVW_PREL_G1_NC,
    R_AARCH64_MOVW_PREL_G2, R_AARCH64_MOVW_PREL_G2_NC, R_AARCH64_MOVW_PREL_G3,
    R_AARCH64_LDST128_ABS_LO12_NC, R_AARCH64_MOVW_GOTOFF_G0, R_AARCH64_MOVW_GOTOFF_G0_NC,
    R_AARCH64_MOVW_GOTOFF_G1, R_AARCH64_MOVW_GOTOFF_G1_NC, R_AARCH64_MOVW_GOTOFF_G2,
    R_AARCH64_MOVW_GOTOFF_G2_NC, R_AARCH64_MOVW_GOTOFF_G3, R_AARCH64_GOTREL64,
    R_AARCH64_GOTREL32, R_AARCH64_GOT_LD_PREL19, R_AARCH64_LD64_GOTOFF_LO15,
    R_AARCH64_ADR_GOT_PAGE, R_AARCH64_LD64_GOT_LO12_NC, R_AARCH64_LD64_GOTPAGE_LO15,
    R_AARCH64_TLSGD_ADR_PREL21, R_AARCH64_TLSGD_ADR_PAGE21, R_AARCH64_TLSGD_ADD_LO12_NC,
    R_AARCH64_TLSGD_MOVW_G1, R_AARCH64_TLSGD_MOVW_G0_NC, R_AARCH64_TLSLD_ADR_PREL21,
    R_AARCH64_TLSLD_ADR_PAGE21, R_AARCH64_TLSLD_ADD_LO12_NC, R_AARCH64_TLSLD_MOVW_G1,
    R_AARCH64_TLSLD_MOVW_G0_NC, R_AARCH64_TLSLD_LD_PREL19, R_AARCH64_TLSLD_MOVW_DTPREL_G2,
    R_AARCH64_TLSLD_MOVW_DTPREL_G1, R_AARCH64_TLSLD_MOVW_DTPREL_G1_NC,
    R_AARCH64_TLSLD_MOVW_DTPREL_G0, R_AARCH64_TLSLD_MOVW_DTPREL_G0_NC,
    R_AARCH64_TLSLD_ADD_DTPREL_HI12, R_AARCH64_TLSLD_ADD_DTPREL_LO12,
    R_AARCH64_TLSLD_ADD_DTPREL_LO12_NC, R_AARCH64_TLSLD_LDST8_DTPREL_LO12,
    R_AARCH64_TLSLD_LDST8_DTPREL_LO12_NC, R_AARCH64_TLSLD_LDST16_DTPREL_LO12,
    R_AARCH64_TLSLD_LDST16_DTPREL_LO12_NC, R_AARCH64_TLSLD_LDST32_DTPREL_LO12,
    R_AARCH64_TLSLD_LDST32_DTPREL_LO12_NC, R_AARCH64_TLSLD_LDST64_DTPREL_LO12,
    R_AARCH64_TLSLD_LDST64_DTPREL_LO12_NC, R_AARCH64_TLSIE_MOVW_GOTTPREL_G1,
    R_AARCH64_TLSIE_MOVW_GOTTPREL_G0_NC, R_AARCH64_TLSIE_ADR_GOTTPREL_PAGE21,
    R_AARCH64_TLSIE_LD64_GOTTPREL_LO12_NC, R_AARCH64_TLSIE_LD_GOTTPREL_PREL19,
    R_AARCH64_TLSLE_MOVW_TPREL_G2, R_AARCH64_TLSLE_MOVW_TPREL_G1,
    R_AARCH64_TLSLE_MOVW_TPREL_G1_NC, R_AARCH64_TLSLE_MOVW_TPREL_G0,
    R_AARCH64_TLSLE_MOVW_TPREL_G0_NC, R_AARCH64_TLSLE_ADD_TPREL_HI12,
    R_AARCH64_TLSLE_ADD_TPREL_LO12, R_AARCH64_TLSLE_ADD_TPREL_LO12_NC,
    R_AARCH64_TLSLE_LDST8_TPREL_LO12, R_AARCH64_TLSLE_LDST8_TPREL_LO12_NC,
    R_AARCH64_TLSLE_LDST16_TPREL_LO12, R_AARCH64_TLSLE_LDST16_TPREL_LO12_NC,
    R_AARCH64_TLSLE_LDST32_TPREL_LO12, R_AARCH64_TLSLE_LDST32_TPREL_LO12_NC,
    R_AARCH64_TLSLE_LDST64_TPREL_LO12, R_AARCH64_TLSLE_LDST64_TPREL_LO12_NC,
    R_AARCH64_TLSDESC_LD_PREL19, R_AARCH64_TLSDESC_ADR_PREL21, R_AARCH64_TLSDESC_ADR_PAGE21,
    R_AARCH64_TLSDESC_LD64_LO12, R_AARCH64_TLSDESC_ADD_LO12, R_AARCH64_TLSDESC_OFF_G1,
    R_AARCH64_TLSDESC_OFF_G0_NC, R_AARCH64_TLSDESC_LDR, R_AARCH64_TLSDESC_ADD,
    R_AARCH64_TLSDESC_CALL, R_AARCH64_TLSLE_LDST128_TPREL_LO12,
    R_AARCH64_TLSLE_LDST128_TPREL_LO12_NC, R_AARCH64_TLSLD_LDST128_DTPREL_LO12,
];

/// The name of relocation type `r_type` of `arch`, for errors.
fn relocation_name(arch: Arch, r_type: u32) -> &'static str {
    let names: &[(u32, &'static str)] = match arch {
        Arch::X86_64 => X86_64_NAMES,
        Arch::Aarch64 => AARCH64_NAMES,
        Arch::Wasm32 => &[],
    };
    names.iter().find(|&&(number, _)| number == r_type).map_or("an unknown type", |&(_, name)| name)
}

//! The native inputs of a PE link: COFF objects (`.obj`, and `/bigobj` ones), `.lib` archives
//! (the first and second linker members), and import libraries, whose short import members
//! (`IMPORT_OBJECT_HEADER`) and long ones (an object with `.idata$` sections) are read as one
//! `Shared` per DLL with no `__imp_` names: `__imp_X` is `Special::Got("X")`.

use std::collections::BTreeMap;

use object::LittleEndian as LE;
use object::pe;
use object::read::archive::{ArchiveFile, ArchiveKind};
use object::read::coff::{CoffHeader, ImageSymbol, SectionTable, SymbolTable};
use object::read::SymbolIndex;

use super::{OBJ_PDATA, Pe};
use crate::NativeKind;
use crate::model::{
    Apply, Archive, Binding, Class, Data, Directive, Group, Member, NativeFile, NativeFixup,
    Object, Section, SectionKind, Shared, SharedSymbol, Symbol, SymbolDef, SymbolKind, Writer,
};

/// The size of an archive member's header.
const MEMBER_HEADER: u64 = 60;

const BIGOBJ: [u8; 16] = [
    0xC7, 0xA1, 0xBA, 0xD1, 0xEE, 0xBA, 0xA9, 0x4B, 0xAF, 0x20, 0xFA, 0xF6, 0x6A, 0xA4, 0xDC, 0xB8,
];

pub(super) fn read(
    pe: &Pe,
    name: &str,
    bytes: &[u8],
    kind: &NativeKind,
) -> Result<NativeFile, String> {
    match kind {
        NativeKind::Object => member(pe, name, bytes),
        NativeKind::Archive { whole } => archive(pe, name, bytes, *whole).map(NativeFile::Archive),
        NativeKind::Shared { .. } => import_library(pe, name, bytes).map(NativeFile::Shared),
    }
}

/// `object`'s COFF and PE headers are read in place and need their alignment, which an
/// archive member (2-aligned) doesn't have: such bytes are copied to 8-aligned words.
pub(super) enum Aligned<'a> {
    Borrowed(&'a [u8]),
    Owned(Vec<u64>, usize),
}

impl Aligned<'_> {
    pub(super) fn new(bytes: &[u8]) -> Aligned<'_> {
        if (bytes.as_ptr() as usize).is_multiple_of(8) {
            return Aligned::Borrowed(bytes);
        }
        let mut words = vec![0u64; bytes.len().div_ceil(8)];
        object::pod::bytes_of_slice_mut(&mut words)[..bytes.len()].copy_from_slice(bytes);
        Aligned::Owned(words, bytes.len())
    }

    pub(super) fn bytes(&self) -> &[u8] {
        match self {
            Aligned::Borrowed(bytes) => bytes,
            Aligned::Owned(words, len) => &object::pod::bytes_of_slice(words)[..*len],
        }
    }
}

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// A relocatable input: an object, or an import member of an archive.
fn member(pe: &Pe, name: &str, bytes: &[u8]) -> Result<NativeFile, String> {
    let aligned = Aligned::new(bytes);
    let data = aligned.bytes();
    if u16_at(data, 0) == Some(pe::IMAGE_FILE_MACHINE_UNKNOWN) && u16_at(data, 2) == Some(0xffff)
    {
        let version = u16_at(data, 4).ok_or_else(|| format!("{name}: a truncated COFF header"))?;
        if version == 0 {
            return short_import(name, data).map(NativeFile::Shared);
        }
        if version >= 2 && data.get(12..28) == Some(&BIGOBJ[..]) {
            return coff::<pe::AnonObjectHeaderBigobj>(pe, name, data);
        }
        return Err(format!(
            "{name}: an anonymous COFF object (one built with /GL for link-time code generation, \
             or /clr): blaze links no LTCG or CLR objects"
        ));
    }
    coff::<pe::ImageFileHeader>(pe, name, data)
}

/// A short import member: one symbol of one DLL.
pub(super) fn short_import(name: &str, data: &[u8]) -> Result<Shared, String> {
    let short = || format!("{name}: a truncated import member");
    let machine = u16_at(data, 6).ok_or_else(short)?;
    if machine != pe::IMAGE_FILE_MACHINE_AMD64 {
        return Err(format!("{name}: an import member for machine {machine:#x}, not x86-64"));
    }
    let size = u32_at(data, 12).ok_or_else(short)? as usize;
    let ordinal_or_hint = u16_at(data, 16).ok_or_else(short)?;
    let types = u16_at(data, 18).ok_or_else(short)?;
    let strings = data.get(20..20 + size).ok_or_else(short)?;
    let mut strings = strings.split(|it| *it == 0);
    let symbol = lossy(strings.next().ok_or_else(short)?);
    let dll = lossy(strings.next().ok_or_else(short)?);
    let kind = match types & pe::IMPORT_OBJECT_TYPE_MASK {
        pe::IMPORT_OBJECT_CODE => SymbolKind::Function,
        pe::IMPORT_OBJECT_DATA | pe::IMPORT_OBJECT_CONST => SymbolKind::Data,
        other => return Err(format!("{name}: import type {other} of {symbol}, which no DLL has")),
    };
    let strip = |it: &str| -> String {
        it.strip_prefix(['?', '@', '_']).unwrap_or(it).to_owned()
    };
    let export = match (types >> pe::IMPORT_OBJECT_NAME_SHIFT) & pe::IMPORT_OBJECT_NAME_MASK {
        pe::IMPORT_OBJECT_ORDINAL => None,
        pe::IMPORT_OBJECT_NAME => Some(symbol.clone()),
        pe::IMPORT_OBJECT_NAME_NO_PREFIX => Some(strip(&symbol)),
        pe::IMPORT_OBJECT_NAME_UNDECORATE => {
            Some(strip(&symbol).split('@').next().unwrap_or_default().to_owned())
        }
        pe::IMPORT_OBJECT_NAME_EXPORTAS => Some(lossy(strings.next().ok_or_else(short)?)),
        other => {
            return Err(format!("{name}: import name type {other} of {symbol}, which blaze does not link"));
        }
    };
    let version = match export {
        None => format!("#{ordinal_or_hint}"),
        Some(export) => format!("{ordinal_or_hint} {export}"),
    };
    Ok(Shared {
        name: dll,
        symbols: vec![SharedSymbol { name: symbol, version: Some(version), kind, weak: false }],
        more: Vec::new(),
        inputs: Vec::new(),
    })
}

/// How the writer names an import in the DLL's export table, as `SharedSymbol::version` and
/// `Import::version` carry it on PE: `"<hint> <name>"` or `"#<ordinal>"`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum ImportName {
    Name { hint: u16, name: String },
    Ordinal(u16),
}

impl ImportName {
    pub(crate) fn parse(symbol: &str, version: Option<&str>) -> Result<ImportName, String> {
        let Some(version) = version else {
            return Ok(ImportName::Name { hint: 0, name: symbol.to_owned() });
        };
        let bad = || format!("the import {symbol} has no PE import name: {version:?}");
        if let Some(ordinal) = version.strip_prefix('#') {
            return ordinal.parse().map(ImportName::Ordinal).map_err(|_| bad());
        }
        let (hint, name) = version.split_once(' ').ok_or_else(bad)?;
        Ok(ImportName::Name { hint: hint.parse().map_err(|_| bad())?, name: name.to_owned() })
    }
}

fn archive(pe: &Pe, name: &str, bytes: &[u8], whole: bool) -> Result<Archive, String> {
    let file = ArchiveFile::parse(bytes).map_err(|e| format!("{name}: not an archive: {e}"))?;
    if file.is_thin() {
        return Err(format!("{name}: a thin archive, which link.exe does not read"));
    }
    match file.kind() {
        ArchiveKind::Coff | ArchiveKind::Gnu | ArchiveKind::Gnu64 | ArchiveKind::Unknown => {}
        other => return Err(format!("{name}: a {other:?} archive, not a COFF one")),
    }
    let mut members = Vec::new();
    let mut at = BTreeMap::new();
    for member in file.members() {
        let member = member.map_err(|e| format!("{name}: {e}"))?;
        let (offset, size) = member.file_range();
        at.insert(offset - MEMBER_HEADER, members.len() as u32);
        members.push(Member {
            name: lossy(member.name()),
            bytes: offset as usize..(offset + size) as usize,
        });
    }
    let mut index = Vec::new();
    if let Some(symbols) = file.symbols().map_err(|e| format!("{name}: {e}"))? {
        for symbol in symbols {
            let symbol = symbol.map_err(|e| format!("{name}: {e}"))?;
            let member = at.get(&symbol.offset().0).ok_or_else(|| {
                format!(
                    "{name}: the index names {} in a member at {:#x}, where none starts",
                    lossy(symbol.name()),
                    symbol.offset().0
                )
            })?;
            index.push((lossy(symbol.name()), *member));
        }
    }
    if index.is_empty() && !members.is_empty() && !whole {
        return Err(format!(
            "{name}: an archive with no symbol index, so no member of it can be taken for a \
             symbol (index it with lib.exe or llvm-ranlib)"
        ));
    }
    for (symbol, member) in &index {
        if !symbol.starts_with("__IMPORT_DESCRIPTOR_") {
            continue;
        }
        let member = &members[*member as usize];
        let dll = descriptor_dll(&format!("{name}({})", member.name), &bytes[member.bytes.clone()])?;
        pe.descriptors.lock().unwrap_or_else(|e| e.into_inner()).insert(symbol.clone(), dll);
    }
    Ok(Archive { members, index })
}

/// The DLL an import descriptor member names: its `.idata$6`.
fn descriptor_dll(name: &str, bytes: &[u8]) -> Result<String, String> {
    let aligned = Aligned::new(bytes);
    let data = aligned.bytes();
    let mut offset = 0;
    let header = pe::ImageFileHeader::parse(data, &mut offset)
        .map_err(|e| format!("{name}: an import descriptor that is not a COFF object: {e}"))?;
    let sections = header.sections(data, offset).map_err(|e| format!("{name}: {e}"))?;
    let symbols = header.symbols(data).map_err(|e| format!("{name}: {e}"))?;
    for section in sections.iter() {
        if section.name(symbols.strings()).ok() != Some(b".idata$6".as_slice()) {
            continue;
        }
        let bytes = section.coff_data(data).map_err(|()| format!("{name}: a truncated .idata$6"))?;
        let dll = bytes.split(|it| *it == 0).next().unwrap_or_default();
        if !dll.is_empty() {
            return Ok(lossy(dll));
        }
    }
    Err(format!("{name}: an import descriptor naming no DLL (no .idata$6)"))
}

/// An import library linked as a shared library: every member an import of some DLL.
fn import_library(pe: &Pe, name: &str, bytes: &[u8]) -> Result<Shared, String> {
    if !bytes.starts_with(b"!<arch>\n") {
        if bytes.starts_with(b"MZ") {
            return Err(format!(
                "{name}: a DLL or executable; a PE image links against the DLL's import \
                 library (.lib), as link.exe does"
            ));
        }
        return Err(format!("{name}: not an import library"));
    }
    let archive = archive(pe, name, bytes, true)?;
    let mut dlls: Vec<Shared> = Vec::new();
    for member in &archive.members {
        let member_name = format!("{name}({})", member.name);
        let mut shared = match self::member(pe, &member_name, &bytes[member.bytes.clone()])? {
            NativeFile::Shared(shared) => shared,
            NativeFile::Object(object) if object.sections.is_empty() => continue,
            NativeFile::Object(_) | NativeFile::Archive(_) => {
                return Err(format!(
                    "{member_name}: an object in a library linked as a DLL's import library; \
                     a library with objects is linked as an archive"
                ));
            }
        };
        match dlls.iter_mut().find(|it| it.name == shared.name) {
            Some(dll) => dll.symbols.append(&mut shared.symbols),
            None => dlls.push(shared),
        }
    }
    let mut dlls = dlls.into_iter();
    let mut first = dlls.next().ok_or_else(|| format!("{name}: an import library importing nothing"))?;
    first.more = dlls.collect();
    Ok(first)
}

/// The sections of a COFF object, by its section number less one.
struct Sections<'d> {
    table: SectionTable<'d>,
    names: Vec<String>,
}

fn coff<'d, C: CoffHeader>(pe: &Pe, name: &str, data: &'d [u8]) -> Result<NativeFile, String> {
    let mut offset = 0;
    let header = C::parse(data, &mut offset).map_err(|e| format!("{name}: not a COFF object: {e}"))?;
    match header.machine() {
        pe::IMAGE_FILE_MACHINE_AMD64 | pe::IMAGE_FILE_MACHINE_UNKNOWN => {}
        machine => {
            return Err(format!("{name}: a COFF object for machine {machine:#x}, not x86-64"));
        }
    }
    let table = header.sections(data, offset).map_err(|e| format!("{name}: {e}"))?;
    let symbols = header.symbols(data).map_err(|e| format!("{name}: {e}"))?;
    let strings = symbols.strings();
    let mut names = Vec::with_capacity(table.len());
    for section in table.iter() {
        let section_name = section.name(strings).map_err(|e| format!("{name}: {e}"))?;
        names.push(lossy(section_name));
    }
    let sections = Sections { table, names };
    if sections.names.iter().any(|it| it.starts_with(".idata$")) {
        return long_import(pe, name, data, &sections, &symbols);
    }
    Reader { pe, name, data, sections: &sections, symbols: &symbols }.object().map(NativeFile::Object)
}

struct Reader<'a, 'd, C: CoffHeader> {
    pe: &'a Pe,
    name: &'a str,
    data: &'d [u8],
    sections: &'a Sections<'d>,
    symbols: &'a SymbolTable<'d, &'d [u8], C>,
}

/// What a COFF symbol table entry is to the link.
struct Read {
    symbols: Vec<Symbol>,
    /// The object's symbol for each COFF symbol table index (aux records and symbols no
    /// relocation can name have none).
    map: Vec<Option<u32>>,
}

impl<'d, C: CoffHeader> Reader<'_, 'd, C> {
    fn err(&self, what: impl std::fmt::Display) -> String {
        format!("{}: {what}", self.name)
    }

    fn header(&self, index: usize) -> &'d pe::ImageSectionHeader {
        &self.sections.table.iter().as_slice()[index]
    }

    fn object(&self) -> Result<Object, String> {
        let mut sections = Vec::with_capacity(self.sections.names.len());
        let mut libraries = Vec::new();
        let mut directives = Vec::new();
        let mut alternates = Vec::new();
        for (index, section_name) in self.sections.names.iter().enumerate() {
            let header = self.header(index);
            let flags = header.characteristics.get(LE);
            if section_name == ".drectve" {
                let bytes = header.coff_data(self.data).map_err(|()| self.err("a truncated .drectve"))?;
                self.directives(bytes, &mut libraries, &mut directives, &mut alternates)?;
            }
            sections.push(self.section(section_name, header, flags)?);
        }
        let read = self.symbols()?;
        let groups = self.groups(&mut sections)?;
        for (index, header) in self.sections.table.iter().enumerate() {
            if sections[index].what == SectionKind::Dropped {
                continue;
            }
            sections[index].fixups = self.fixups(index, header, &sections[index], &read)?;
        }
        let mut symbols = read.symbols;
        for (from, to) in alternates {
            let target = match symbols.iter().position(|it| it.name == to && it.binding != Binding::Local) {
                Some(at) => at as u32,
                None => {
                    symbols.push(Symbol {
                        name: to,
                        def: SymbolDef::Undefined,
                        binding: Binding::Global,
                        kind: SymbolKind::Other,
                        hidden: true,
                    });
                    symbols.len() as u32 - 1
                }
            };
            symbols.push(Symbol {
                name: from,
                def: SymbolDef::Fallback { symbol: target, search: false },
                binding: Binding::Global,
                kind: SymbolKind::Other,
                hidden: true,
            });
        }
        Ok(Object { sections, symbols, groups, libraries, directives })
    }

    fn section(
        &self,
        section_name: &str,
        header: &pe::ImageSectionHeader,
        flags: u32,
    ) -> Result<Section, String> {
        let align = alignment(flags).ok_or_else(|| {
            self.err(format!("section {section_name} has alignment bits {:#x}", flags & pe::IMAGE_SCN_ALIGN_MASK))
        })?;
        let base = section_name.split('$').next().unwrap_or_default();
        let dropped = flags & (pe::IMAGE_SCN_LNK_REMOVE | pe::IMAGE_SCN_LNK_INFO) != 0
            || dropped_name(section_name);
        let zero = flags & pe::IMAGE_SCN_CNT_UNINITIALIZED_DATA != 0;
        let data = if dropped {
            Data::Bytes(Vec::new())
        } else if zero {
            Data::Zero(header.size_of_raw_data.get(LE).into())
        } else {
            let bytes = header
                .coff_data(self.data)
                .map_err(|()| self.err(format!("section {section_name} is past the end")))?;
            Data::Bytes(bytes.to_vec())
        };
        let what = if dropped {
            SectionKind::Dropped
        } else if base == OBJ_PDATA {
            SectionKind::Unwind(Class::Named { name: OBJ_PDATA.to_owned(), write: false, exec: false })
        } else if base == ".xdata" {
            SectionKind::Unwind(Class::Rodata)
        } else {
            let write = flags & pe::IMAGE_SCN_MEM_WRITE != 0;
            let exec = flags & (pe::IMAGE_SCN_MEM_EXECUTE | pe::IMAGE_SCN_CNT_CODE) != 0;
            let (class, rank) = self
                .pe
                .section(section_name, write, exec, base == ".tls")
                .map_err(|e| self.err(e))?;
            let class = match class {
                Class::Data if zero => Class::Bss,
                Class::Bss if !zero => Class::Data,
                class => class,
            };
            SectionKind::Alloc(class, rank)
        };
        let comdat = flags & pe::IMAGE_SCN_LNK_COMDAT != 0;
        Ok(Section {
            name: section_name.to_owned(),
            retain: what != SectionKind::Dropped && !comdat,
            what,
            align,
            data,
            fixups: Vec::new(),
        })
    }

    fn symbols(&self) -> Result<Read, String> {
        let strings = self.symbols.strings();
        let mut read = Read { symbols: Vec::new(), map: vec![None; self.symbols.len()] };
        let mut weak = Vec::new();
        let mut defined: Vec<Vec<(u64, usize)>> = vec![Vec::new(); self.sections.names.len()];
        for (index, symbol) in self.symbols.iter() {
            let class = symbol.storage_class();
            let number = symbol.section_number();
            let linkable = matches!(
                class,
                pe::IMAGE_SYM_CLASS_EXTERNAL
                    | pe::IMAGE_SYM_CLASS_STATIC
                    | pe::IMAGE_SYM_CLASS_LABEL
                    | pe::IMAGE_SYM_CLASS_WEAK_EXTERNAL
            );
            if !linkable || number == pe::IMAGE_SYM_DEBUG {
                continue;
            }
            let symbol_name = lossy(symbol.name(strings).map_err(|e| self.err(e))?);
            let value = u64::from(symbol.value());
            let function = symbol.derived_type() == pe::IMAGE_SYM_DTYPE_FUNCTION;
            let (def, kind) = if class == pe::IMAGE_SYM_CLASS_WEAK_EXTERNAL {
                if !symbol.has_aux_weak_external() {
                    return Err(self.err(format!("weak external {symbol_name} names no default")));
                }
                let aux = self.symbols.aux_weak_external(index).map_err(|e| self.err(e))?;
                weak.push((read.symbols.len(), aux.default_symbol(), aux.weak_search_type.get(LE)));
                (SymbolDef::Undefined, if function { SymbolKind::Function } else { SymbolKind::Other })
            } else if number > 0 {
                let section = number as usize - 1;
                if section >= self.sections.names.len() {
                    return Err(self.err(format!("symbol {symbol_name} is in section {number}, which there isn't")));
                }
                let flags = self.header(section).characteristics.get(LE);
                let kind = if symbol.has_aux_section() && value == 0 {
                    SymbolKind::Section
                } else if function || flags & (pe::IMAGE_SCN_MEM_EXECUTE | pe::IMAGE_SCN_CNT_CODE) != 0 {
                    SymbolKind::Function
                } else {
                    SymbolKind::Data
                };
                if kind != SymbolKind::Section {
                    defined[section].push((value, read.symbols.len()));
                }
                (SymbolDef::Section { section: section as u32, value, size: 0 }, kind)
            } else if number == pe::IMAGE_SYM_ABSOLUTE {
                (SymbolDef::Absolute(value), SymbolKind::Other)
            } else if number == pe::IMAGE_SYM_UNDEFINED {
                let kind = if function { SymbolKind::Function } else { SymbolKind::Other };
                if value != 0 && class == pe::IMAGE_SYM_CLASS_EXTERNAL {
                    let align = value.next_power_of_two().min(32);
                    (SymbolDef::Common { size: value, align }, SymbolKind::Data)
                } else {
                    (SymbolDef::Undefined, kind)
                }
            } else {
                return Err(self.err(format!("symbol {symbol_name} has section number {number}")));
            };
            let binding = match class {
                pe::IMAGE_SYM_CLASS_EXTERNAL => Binding::Global,
                pe::IMAGE_SYM_CLASS_WEAK_EXTERNAL => Binding::Weak,
                _ => Binding::Local,
            };
            read.map[index.0] = Some(read.symbols.len() as u32);
            read.symbols.push(Symbol { name: symbol_name, def, binding, kind, hidden: true });
        }
        for (section, symbols) in defined.iter_mut().enumerate() {
            let len = u64::from(self.header(section).size_of_raw_data.get(LE));
            symbols.sort();
            for (at, (value, symbol)) in symbols.iter().enumerate() {
                let end = symbols[at..].iter().map(|it| it.0).find(|it| it > value).unwrap_or(len);
                if let SymbolDef::Section { size, .. } = &mut read.symbols[*symbol].def {
                    *size = end.saturating_sub(*value);
                }
            }
        }
        for symbol in &mut read.symbols {
            if let (SymbolKind::Section, SymbolDef::Section { section, size, .. }) =
                (symbol.kind, &mut symbol.def)
            {
                *size = u64::from(self.header(*section as usize).size_of_raw_data.get(LE));
            }
        }
        for (symbol, default, search) in weak {
            let target = read.map.get(default.0).copied().flatten().ok_or_else(|| {
                self.err(format!("weak external {} defaults to symbol {}, which is not one", read.symbols[symbol].name, default.0))
            })?;
            let search = match search {
                pe::IMAGE_WEAK_EXTERN_SEARCH_NOLIBRARY => false,
                pe::IMAGE_WEAK_EXTERN_SEARCH_LIBRARY | pe::IMAGE_WEAK_EXTERN_SEARCH_ALIAS => true,
                other => {
                    return Err(self.err(format!(
                        "weak external {} has search type {other}, which blaze does not link",
                        read.symbols[symbol].name
                    )));
                }
            };
            read.symbols[symbol].def = SymbolDef::Fallback { symbol: target, search };
        }
        Ok(read)
    }

    /// The COMDAT groups, and which sections stay whatever refers to them.
    fn groups(&self, sections: &mut [Section]) -> Result<Vec<Group>, String> {
        let strings = self.symbols.strings();
        let count = sections.len();
        // The section symbol (with the selection) and the COMDAT symbol (naming the group):
        // the first and second symbols of a section.
        let mut first: Vec<Option<SymbolIndex>> = vec![None; count];
        let mut second: Vec<Option<SymbolIndex>> = vec![None; count];
        for (index, symbol) in self.symbols.iter() {
            let number = symbol.section_number();
            if number <= 0 || number as usize > count {
                continue;
            }
            let at = number as usize - 1;
            if first[at].is_none() {
                first[at] = Some(index);
            } else if second[at].is_none() {
                second[at] = Some(index);
            }
        }
        let mut groups: Vec<Group> = Vec::new();
        let mut group_of: Vec<Option<usize>> = vec![None; count];
        let mut associate = Vec::new();
        for index in 0..count {
            if self.header(index).characteristics.get(LE) & pe::IMAGE_SCN_LNK_COMDAT == 0
                || sections[index].what == SectionKind::Dropped
            {
                continue;
            }
            let section_name = &self.sections.names[index];
            let aux = first[index]
                .filter(|it| self.symbols.symbol(*it).is_ok_and(|it| it.has_aux_section()))
                .ok_or_else(|| self.err(format!("COMDAT section {section_name} has no section symbol")))?;
            let aux_symbol = self.symbols.aux_section(aux).map_err(|e| self.err(e))?;
            let mut parent = u32::from(aux_symbol.number.get(LE));
            if C::is_type_bigobj() {
                parent |= u32::from(aux_symbol.high_number.get(LE)) << 16;
            }
            let largest = match aux_symbol.selection {
                pe::IMAGE_COMDAT_SELECT_ASSOCIATIVE => {
                    associate.push((index, parent));
                    continue;
                }
                pe::IMAGE_COMDAT_SELECT_NODUPLICATES
                | pe::IMAGE_COMDAT_SELECT_ANY
                | pe::IMAGE_COMDAT_SELECT_SAME_SIZE
                | pe::IMAGE_COMDAT_SELECT_EXACT_MATCH => false,
                pe::IMAGE_COMDAT_SELECT_LARGEST => true,
                other => {
                    return Err(self.err(format!(
                        "COMDAT section {section_name} has selection {other}, which blaze does not link"
                    )));
                }
            };
            let leader = second[index]
                .and_then(|it| Some((it, self.symbols.symbol(it).ok()?)))
                .ok_or_else(|| self.err(format!("COMDAT section {section_name} has no COMDAT symbol")))?;
            let group_name = if leader.1.storage_class() == pe::IMAGE_SYM_CLASS_EXTERNAL {
                lossy(leader.1.name(strings).map_err(|e| self.err(e))?)
            } else {
                format!("\0{}\0{index}", self.name)
            };
            group_of[index] = Some(groups.len());
            groups.push(Group { name: group_name, sections: vec![index as u32], largest });
        }
        for (index, parent) in associate {
            let mut root = parent as usize;
            let mut steps = 0;
            loop {
                if root == 0 || root > count {
                    return Err(self.err(format!(
                        "associative COMDAT section {} names section {root}, which there isn't",
                        self.sections.names[index]
                    )));
                }
                match associated(self, root - 1) {
                    Some(next) if steps < count => {
                        root = next as usize;
                        steps += 1;
                    }
                    Some(_) => {
                        return Err(self.err(format!(
                            "associative COMDAT section {} is in a cycle",
                            self.sections.names[index]
                        )));
                    }
                    None => break,
                }
            }
            match group_of[root - 1] {
                Some(group) => groups[group].sections.push(index as u32),
                None if self.header(root - 1).characteristics.get(LE) & pe::IMAGE_SCN_LNK_COMDAT == 0 => {
                    sections[index].retain = true;
                }
                None => {}
            }
        }
        Ok(groups)
    }

    fn fixups(
        &self,
        index: usize,
        header: &pe::ImageSectionHeader,
        section: &Section,
        read: &Read,
    ) -> Result<Vec<NativeFixup>, String> {
        let relocations = header.coff_relocations(self.data).map_err(|e| self.err(e))?;
        let section_name = &self.sections.names[index];
        let bytes = match &section.data {
            Data::Bytes(bytes) => bytes.as_slice(),
            Data::Zero(_) if relocations.is_empty() => return Ok(Vec::new()),
            Data::Zero(_) => {
                return Err(self.err(format!("relocations in {section_name}, which has no bytes")));
            }
        };
        let mut fixups = Vec::with_capacity(relocations.len());
        for relocation in relocations {
            let at = relocation.virtual_address.get(LE) as usize;
            let typ = relocation.typ.get(LE);
            if typ == pe::IMAGE_REL_AMD64_ABSOLUTE {
                continue;
            }
            let index = relocation.symbol_table_index.get(LE) as usize;
            let symbol = read.map.get(index).copied().flatten().ok_or_else(|| {
                self.err(format!(
                    "a relocation in {section_name} at {at:#x} names symbol {index}, which is \
                     not one a relocation can name"
                ))
            })?;
            let target = &read.symbols[symbol as usize];
            let branch = matches!(target.kind, SymbolKind::Function)
                || matches!(target.def, SymbolDef::Undefined | SymbolDef::Fallback { .. });
            let short = || self.err(format!("a relocation in {section_name} at {at:#x} past its end"));
            let (apply, addend) = match typ {
                pe::IMAGE_REL_AMD64_ADDR64 => (Apply::Abs64, u64_at(bytes, at).ok_or_else(short)? as i64),
                pe::IMAGE_REL_AMD64_ADDR32 => {
                    (Apply::Abs32, i64::from(u32_at(bytes, at).ok_or_else(short)?))
                }
                pe::IMAGE_REL_AMD64_ADDR32NB => {
                    (Apply::ImageRel32, i64::from(u32_at(bytes, at).ok_or_else(short)? as i32))
                }
                pe::IMAGE_REL_AMD64_REL32..=pe::IMAGE_REL_AMD64_REL32_5 => {
                    let implicit = i64::from(u32_at(bytes, at).ok_or_else(short)? as i32);
                    let past = 4 + i64::from(typ - pe::IMAGE_REL_AMD64_REL32);
                    (if branch { Apply::X86Branch32 } else { Apply::Rel32 }, implicit - past)
                }
                pe::IMAGE_REL_AMD64_SECTION => {
                    (Apply::Section16, i64::from(u16_at(bytes, at).ok_or_else(short)?))
                }
                pe::IMAGE_REL_AMD64_SECREL => {
                    (Apply::SecRel32, i64::from(u32_at(bytes, at).ok_or_else(short)? as i32))
                }
                other => {
                    return Err(self.err(format!(
                        "relocation type {other:#x} ({}) in {section_name} at {at:#x} against {}, \
                         which blaze does not link",
                        relocation_name(other),
                        target.name
                    )));
                }
            };
            fixups.push(NativeFixup { at: at as u64, apply, symbol, addend, minus: None });
        }
        Ok(fixups)
    }

    fn directives(
        &self,
        bytes: &[u8],
        libraries: &mut Vec<String>,
        directives: &mut Vec<Directive>,
        alternates: &mut Vec<(String, String)>,
    ) -> Result<(), String> {
        let text = String::from_utf8_lossy(bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes));
        for word in words(&text) {
            let Some(option) = word.strip_prefix(['/', '-']) else {
                return Err(self.err(format!("the .drectve word {word:?} is not an option")));
            };
            let (option, arg) = match option.split_once(':') {
                Some((option, arg)) => (option.to_ascii_lowercase(), Some(arg)),
                None => (option.to_ascii_lowercase(), None),
            };
            let need = || self.err(format!("the .drectve option /{option} needs an argument"));
            match option.as_str() {
                "defaultlib" => {
                    let library = default_library(arg.ok_or_else(need)?);
                    if !libraries.contains(&library) {
                        libraries.push(library);
                    }
                }
                "include" => directives.push(Directive::Include(arg.ok_or_else(need)?.to_owned())),
                "alternatename" => {
                    let arg = arg.ok_or_else(need)?;
                    let (from, to) = arg.split_once('=').ok_or_else(|| {
                        self.err(format!("/ALTERNATENAME:{arg} is not from=to"))
                    })?;
                    alternates.push((from.to_owned(), to.to_owned()));
                }
                "export" => directives.push(self.export(arg.ok_or_else(need)?)?),
                "failifmismatch" => {
                    let arg = arg.ok_or_else(need)?;
                    let (key, value) = arg.split_once('=').ok_or_else(|| {
                        self.err(format!("/FAILIFMISMATCH:{arg} is not key=value"))
                    })?;
                    directives.push(Directive::Mismatch { key: key.to_owned(), value: value.to_owned() });
                }
                "nodefaultlib" => {
                    directives.push(Directive::NoDefaultLib(arg.map(default_library)));
                }
                // These change nothing a program observes: section merging (every section of
                // a protection is laid out together already), incremental-link padding, the
                // control-flow-guard tables of /guard:cf (off), a check that a library is not
                // linked.
                "merge" | "editandcontinue" | "guardsym" | "disallowlib" => {}
                _ => {
                    return Err(self.err(format!(
                        "the .drectve option {word}, which blaze does not link (it would change \
                         the image as link.exe makes it)"
                    )));
                }
            }
        }
        Ok(())
    }

    /// `/EXPORT:name[=internal][,@ordinal[,NONAME]][,DATA][,PRIVATE]`.
    fn export(&self, arg: &str) -> Result<Directive, String> {
        let mut parts = arg.split(',');
        let first = parts.next().unwrap_or_default();
        let (name, symbol) = first.split_once('=').unwrap_or((first, first));
        if name.is_empty() || symbol.contains('.') {
            return Err(self.err(format!("/EXPORT:{arg}: a forwarder or no name, which blaze does not link")));
        }
        let mut data = false;
        for part in parts {
            match part.to_ascii_uppercase().as_str() {
                "DATA" => data = true,
                "PRIVATE" => {}
                _ => {
                    return Err(self.err(format!(
                        "/EXPORT:{arg}: exports by ordinal (@n, NONAME), which blaze does not link"
                    )));
                }
            }
        }
        Ok(Directive::Export { name: name.to_owned(), symbol: symbol.to_owned(), data })
    }
}

/// The section number an associative COMDAT section `index` names, if it is one.
fn associated<C: CoffHeader>(reader: &Reader<'_, '_, C>, index: usize) -> Option<u32> {
    if reader.header(index).characteristics.get(LE) & pe::IMAGE_SCN_LNK_COMDAT == 0 {
        return None;
    }
    let number = index as i32 + 1;
    let (symbol, _) = reader
        .symbols
        .iter()
        .find(|(_, it)| it.section_number() == number && it.has_aux_section())?;
    let aux = reader.symbols.aux_section(symbol).ok()?;
    if aux.selection != pe::IMAGE_COMDAT_SELECT_ASSOCIATIVE {
        return None;
    }
    let mut parent = u32::from(aux.number.get(LE));
    if C::is_type_bigobj() {
        parent |= u32::from(aux.high_number.get(LE)) << 16;
    }
    Some(parent)
}

/// A COFF section's alignment: 16 when it states none, as the format says.
pub(super) fn alignment(flags: u32) -> Option<u64> {
    match (flags & pe::IMAGE_SCN_ALIGN_MASK) >> 20 {
        0 => Some(16),
        bits @ 1..=14 => Some(1 << (bits - 1)),
        _ => None,
    }
}

/// Sections no image keeps: debug info (CodeView and DWARF; no PDB is written), the
/// control-flow-guard tables of `/guard:cf` (off), LLVM's address-significance and call-graph
/// tables, embedded bitcode, and the volatile-metadata table link.exe discards.
fn dropped_name(name: &str) -> bool {
    let base = name.split('$').next().unwrap_or_default();
    name.starts_with(".debug")
        || matches!(
            base,
            ".gfids" | ".giats" | ".gljmp" | ".gehcont" | ".sxdata" | ".voltbl" | ".chks64"
        )
        || matches!(name, ".llvm_addrsig" | ".llvm.call-graph-profile" | ".llvmbc" | ".llvmcmd")
}

/// A `/DEFAULTLIB` name as link.exe takes it: `.lib` added when it has no extension.
fn default_library(name: &str) -> String {
    let file = name.rsplit(['/', '\\']).next().unwrap_or(name);
    if file.contains('.') { name.to_owned() } else { format!("{name}.lib") }
}

/// The words of a `.drectve`: split at spaces outside quotes, the quotes removed.
pub(super) fn words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    let mut any = false;
    for c in text.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            c if c.is_whitespace() && !quoted => {
                if any {
                    words.push(std::mem::take(&mut word));
                    any = false;
                }
            }
            '\0' if !quoted => {}
            c => {
                word.push(c);
                any = true;
            }
        }
    }
    if any {
        words.push(word);
    }
    words
}

fn relocation_name(typ: u16) -> &'static str {
    match typ {
        pe::IMAGE_REL_AMD64_SECREL7 => "SECREL7",
        pe::IMAGE_REL_AMD64_TOKEN => "TOKEN",
        pe::IMAGE_REL_AMD64_SREL32 => "SREL32",
        pe::IMAGE_REL_AMD64_PAIR => "PAIR",
        pe::IMAGE_REL_AMD64_SSPAN32 => "SSPAN32",
        pe::IMAGE_REL_AMD64_EHANDLER => "EHANDLER",
        pe::IMAGE_REL_AMD64_IMPORT_BR => "IMPORT_BR",
        pe::IMAGE_REL_AMD64_IMPORT_CALL => "IMPORT_CALL",
        pe::IMAGE_REL_AMD64_CFG_BR..=pe::IMAGE_REL_AMD64_INDIR_CALL => "a control-flow-guard one",
        pe::IMAGE_REL_AMD64_INDIR_BR_SWITCHTABLE_FIRST..=pe::IMAGE_REL_AMD64_INDIR_BR_SWITCHTABLE_LAST => {
            "a switch-table one"
        }
        _ => "unknown",
    }
}

/// A long-format import member: an object with `.idata$` sections. One importing a function
/// or datum is read as that import; the descriptor, null descriptor and null thunk members
/// hold the import tables the writer makes itself, so they are read as empty objects.
fn long_import<'d, C: CoffHeader>(
    pe: &Pe,
    name: &str,
    data: &'d [u8],
    sections: &Sections<'d>,
    symbols: &SymbolTable<'d, &'d [u8], C>,
) -> Result<NativeFile, String> {
    let strings = symbols.strings();
    let mut imp = None;
    let mut descriptor = None;
    let mut defined_in_code = Vec::new();
    for (index, symbol) in symbols.iter() {
        let symbol_name = lossy(symbol.name(strings).map_err(|e| format!("{name}: {e}"))?);
        let number = symbol.section_number();
        let external = symbol.storage_class() == pe::IMAGE_SYM_CLASS_EXTERNAL;
        if !external {
            continue;
        }
        if number > 0 {
            if symbol_name.starts_with("__IMPORT_DESCRIPTOR_")
                || symbol_name == "__NULL_IMPORT_DESCRIPTOR"
                || symbol_name.ends_with("_NULL_THUNK_DATA")
            {
                return Ok(NativeFile::Object(Object::default()));
            }
            let section = number as usize - 1;
            let section_name = sections.names.get(section).map(String::as_str).unwrap_or_default();
            if let Some(stripped) = symbol_name.strip_prefix("__imp_") {
                if section_name.starts_with(".idata$5") {
                    imp = Some((stripped.to_owned(), section, index));
                }
            } else if let Some(header) = sections.table.iter().nth(section) {
                let flags = header.characteristics.get(LE);
                if flags & (pe::IMAGE_SCN_MEM_EXECUTE | pe::IMAGE_SCN_CNT_CODE) != 0 {
                    defined_in_code.push(symbol_name);
                }
            }
        } else if number == pe::IMAGE_SYM_UNDEFINED && symbol_name.starts_with("__IMPORT_DESCRIPTOR_") {
            descriptor = Some(symbol_name);
        }
    }
    let Some((symbol, section, _)) = imp else {
        return Err(format!(
            "{name}: an object with .idata$ sections that defines no __imp_ symbol in .idata$5: \
             the import tables are the linker's"
        ));
    };
    let descriptor = descriptor.ok_or_else(|| {
        format!("{name}: the import member of {symbol} names no __IMPORT_DESCRIPTOR_ (a GNU dlltool library?)")
    })?;
    let dll = pe
        .descriptors
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&descriptor)
        .cloned()
        .ok_or_else(|| format!("{name}: no archive read so far has {descriptor}, the DLL {symbol} is from"))?;
    let header = sections.table.iter().nth(section).ok_or_else(|| format!("{name}: no .idata$5"))?;
    let thunk = header.coff_data(data).map_err(|()| format!("{name}: a truncated .idata$5"))?;
    let relocations = header.coff_relocations(data).map_err(|e| format!("{name}: {e}"))?;
    let version = match relocations.first() {
        Some(relocation) => {
            let target = symbols
                .symbol(SymbolIndex(relocation.symbol_table_index.get(LE) as usize))
                .map_err(|e| format!("{name}: {e}"))?;
            let number = target.section_number();
            let names = (number > 0)
                .then(|| sections.table.iter().nth(number as usize - 1))
                .flatten()
                .ok_or_else(|| format!("{name}: the import name of {symbol} is in no section"))?;
            let bytes = names.coff_data(data).map_err(|()| format!("{name}: a truncated .idata$6"))?;
            let at = target.value() as usize + u64_at(thunk, 0).unwrap_or(0) as usize;
            let hint = u16_at(bytes, at).ok_or_else(|| format!("{name}: a truncated .idata$6"))?;
            let export = bytes[at + 2..].split(|it| *it == 0).next().unwrap_or_default();
            format!("{hint} {}", lossy(export))
        }
        None => {
            let value = u64_at(thunk, 0).ok_or_else(|| format!("{name}: a truncated .idata$5"))?;
            if value & pe::IMAGE_ORDINAL_FLAG64 == 0 {
                return Err(format!("{name}: the import of {symbol} has neither a name nor an ordinal"));
            }
            format!("#{}", value as u16)
        }
    };
    let kind = if defined_in_code.contains(&symbol) { SymbolKind::Function } else { SymbolKind::Data };
    Ok(NativeFile::Shared(Shared {
        name: dll,
        symbols: vec![SharedSymbol { name: symbol, version: Some(version), kind, weak: false }],
        more: Vec::new(),
        inputs: Vec::new(),
    }))
}

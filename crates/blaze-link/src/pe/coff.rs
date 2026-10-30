//! The native inputs of a PE link: COFF objects (`.obj`, and `/bigobj` ones), `.lib` archives
//! (the first and second linker members), and import libraries, whose short import members
//! (`IMPORT_OBJECT_HEADER`) and long ones (an object with `.idata$` sections) are read as one
//! `Shared` per DLL with no `__imp_` names: `__imp_X` is `Special::Got("X")`.
//!
//! Cost: one pass over an object's section headers and one over its symbol table, with section
//! names and `.drectve` words borrowed from the input; a symbol's object index is a `u32` column
//! by COFF index. A section's bytes are a range of the input (`Data::Input`), never copied; every
//! name is borrowed from the input and pushed once into the file's one `Names` arena (sized up
//! front from the string table); relocations and COMDAT members are runs of one column per
//! object. What is still made is each import's `"<hint> <name>"` (its `Shared::versions` entry).
//! A short import member is read in place, never copied for alignment.

use std::borrow::Cow;
use std::fmt::{self, Write as _};
use std::sync::Mutex;

use blaze_ids::radix;
use object::LittleEndian as LE;
use object::pe;
use object::read::SymbolIndex;
use object::read::archive::{ArchiveFile, ArchiveKind};
use object::read::coff::{CoffHeader, ImageSymbol, SectionTable, SymbolTable};
use rustc_hash::FxHashMap;

use super::{OBJ_PDATA, Pe};
use crate::NativeKind;
use crate::model::{
    Apply, Archive, Binding, Class, Data, Directive, Group, Member, Name, Names, NativeFile, NativeFixup, Object,
    Section, SectionKind, Shared, SharedSymbol, Symbol, SymbolDef, SymbolKind, Writer,
};

/// The size of an archive member's header.
const MEMBER_HEADER: u64 = 60;

const BIGOBJ: [u8; 16] = [
    0xC7, 0xA1, 0xBA, 0xD1, 0xEE, 0xBA, 0xA9, 0x4B, 0xAF, 0x20, 0xFA, 0xF6, 0x6A, 0xA4, 0xDC, 0xB8,
];

/// No object symbol: a COFF index that is an aux record or a symbol no relocation can name.
const NONE: u32 = u32::MAX;

/// The DLL of each `__IMPORT_DESCRIPTOR_<lib>`, by that name.
pub(crate) type Descriptors = FxHashMap<String, String>;

/// What errors call an input: a file, or a member of an archive (`lib(member)`). Formatted only
/// when an error is.
#[derive(Clone, Copy)]
pub(super) struct Input<'a> {
    file: &'a str,
    member: Option<&'a str>,
}

impl fmt::Display for Input<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.member {
            Some(member) => write!(f, "{}({member})", self.file),
            None => f.write_str(self.file),
        }
    }
}

/// Where a long import member finds the DLL its `__IMPORT_DESCRIPTOR_` names: the descriptors
/// of every archive read so far (a member the core reads on its own), or those of the import
/// library being read.
#[derive(Clone, Copy)]
enum Dlls<'a> {
    Read(&'a Mutex<Descriptors>),
    Library(&'a Descriptors),
}

impl<'a> Dlls<'a> {
    fn get(self, descriptor: &str) -> Option<Cow<'a, str>> {
        match self {
            Dlls::Read(map) => {
                map.lock().unwrap_or_else(|e| e.into_inner()).get(descriptor).cloned().map(Cow::Owned)
            }
            Dlls::Library(map) => map.get(descriptor).map(|it| Cow::Borrowed(it.as_str())),
        }
    }
}

/// One import of a DLL as an import member names it: the symbol, borrowed from the member
/// when it can be, and how the DLL's export table names it (`"<hint> <name>"` or
/// `"#<ordinal>"`, which PE carries as the symbol's version).
struct Imported<'a> {
    name: Cow<'a, str>,
    version: String,
    kind: SymbolKind,
}

/// A relocatable input as read: an object, or one import of a DLL.
enum Parsed<'a> {
    Object(Object),
    Import(Cow<'a, str>, Imported<'a>),
}

fn shared(dll: String) -> Shared {
    Shared {
        name: dll,
        symbols: Vec::new(),
        names: Names::default(),
        versions: Vec::new(),
        more: Vec::new(),
        inputs: Vec::new(),
    }
}

/// Adds an import to its DLL: the name pushed into the DLL's names, the export name as a
/// version of its own (each import's differs).
fn add(shared: &mut Shared, input: Input<'_>, import: Imported<'_>) -> Result<(), String> {
    let version = u16::try_from(shared.versions.len()).map_err(|_| {
        format!("{input}: more than 65,536 imports of {}, more than a DLL can export", shared.name)
    })?;
    let name = shared.names.push(&import.name).map_err(|e| format!("{input}: {e}"))?;
    shared.versions.push(import.version);
    shared.symbols.push(SharedSymbol { name, version: Some(version), kind: import.kind, weak: false });
    Ok(())
}

pub(super) fn read(pe: &Pe, name: &str, bytes: &[u8], kind: &NativeKind) -> Result<NativeFile, String> {
    let input = Input { file: name, member: None };
    match kind {
        NativeKind::Object => match member(pe, Dlls::Read(&pe.descriptors), input, bytes)? {
            Parsed::Object(object) => Ok(NativeFile::Object(object)),
            Parsed::Import(dll, import) => {
                let mut shared = shared(dll.into_owned());
                add(&mut shared, input, import)?;
                Ok(NativeFile::Shared(shared))
            }
        },
        NativeKind::Archive { whole } => {
            let (archive, descriptors) = archive(input, bytes, *whole)?;
            if !descriptors.is_empty() {
                pe.descriptors.lock().unwrap_or_else(|e| e.into_inner()).extend(descriptors);
            }
            Ok(NativeFile::Archive(archive))
        }
        NativeKind::Shared { .. } => import_library(pe, input, bytes).map(NativeFile::Shared),
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

/// A relocatable input: an object, or an import member of an archive. Only a COFF object is
/// aligned (copied when it isn't); an import header is read where it is.
fn member<'a>(pe: &Pe, dlls: Dlls<'a>, name: Input<'_>, bytes: &'a [u8]) -> Result<Parsed<'a>, String> {
    if u16_at(bytes, 0) == Some(pe::IMAGE_FILE_MACHINE_UNKNOWN) && u16_at(bytes, 2) == Some(0xffff) {
        let version = u16_at(bytes, 4).ok_or_else(|| format!("{name}: a truncated COFF header"))?;
        if version == 0 {
            let (dll, symbol) = short_import(name, bytes)?;
            return Ok(Parsed::Import(dll, symbol));
        }
        if version >= 2 && bytes.get(12..28) == Some(&BIGOBJ[..]) {
            let aligned = Aligned::new(bytes);
            return coff::<pe::AnonObjectHeaderBigobj>(pe, dlls, name, aligned.bytes());
        }
        return Err(format!(
            "{name}: an anonymous COFF object (one built with /GL for link-time code generation, \
             or /clr): blaze links no LTCG or CLR objects"
        ));
    }
    let aligned = Aligned::new(bytes);
    coff::<pe::ImageFileHeader>(pe, dlls, name, aligned.bytes())
}

fn strip(name: &str) -> &str {
    name.strip_prefix(['?', '@', '_']).unwrap_or(name)
}

/// A short import member: one symbol of one DLL, the symbol's and the DLL's names borrowed from it.
fn short_import<'d>(name: Input<'_>, data: &'d [u8]) -> Result<(Cow<'d, str>, Imported<'d>), String> {
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
    let symbol = String::from_utf8_lossy(strings.next().ok_or_else(short)?);
    let dll = String::from_utf8_lossy(strings.next().ok_or_else(short)?);
    let kind = match types & pe::IMPORT_OBJECT_TYPE_MASK {
        pe::IMPORT_OBJECT_CODE => SymbolKind::Function,
        pe::IMPORT_OBJECT_DATA | pe::IMPORT_OBJECT_CONST => SymbolKind::Data,
        other => return Err(format!("{name}: import type {other} of {symbol}, which no DLL has")),
    };
    let export_as: Cow<'_, str>;
    let export = match (types >> pe::IMPORT_OBJECT_NAME_SHIFT) & pe::IMPORT_OBJECT_NAME_MASK {
        pe::IMPORT_OBJECT_ORDINAL => None,
        pe::IMPORT_OBJECT_NAME => Some(&*symbol),
        pe::IMPORT_OBJECT_NAME_NO_PREFIX => Some(strip(&symbol)),
        pe::IMPORT_OBJECT_NAME_UNDECORATE => Some(strip(&symbol).split('@').next().unwrap_or_default()),
        pe::IMPORT_OBJECT_NAME_EXPORTAS => {
            export_as = String::from_utf8_lossy(strings.next().ok_or_else(short)?);
            Some(&*export_as)
        }
        other => {
            return Err(format!("{name}: import name type {other} of {symbol}, which blaze does not link"));
        }
    };
    let version = match export {
        None => format!("#{ordinal_or_hint}"),
        Some(export) => format!("{ordinal_or_hint} {export}"),
    };
    Ok((dll, Imported { name: symbol, version, kind }))
}

/// How the writer names an import in the DLL's export table, as `SharedSymbol::version` and
/// `Import::version` carry it on PE: `"<hint> <name>"` or `"#<ordinal>"`. Borrows the name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ImportName<'a> {
    Name { hint: u16, name: &'a str },
    Ordinal(u16),
}

impl<'a> ImportName<'a> {
    pub(crate) fn parse(symbol: &'a str, version: Option<&'a str>) -> Result<ImportName<'a>, String> {
        let Some(version) = version else {
            return Ok(ImportName::Name { hint: 0, name: symbol });
        };
        let bad = || format!("the import {symbol} has no PE import name: {version:?}");
        if let Some(ordinal) = version.strip_prefix('#') {
            return ordinal.parse().map(ImportName::Ordinal).map_err(|_| bad());
        }
        let (hint, name) = version.split_once(' ').ok_or_else(bad)?;
        Ok(ImportName::Name { hint: hint.parse().map_err(|_| bad())?, name })
    }
}

/// An archive's members and index, and the DLL each import descriptor member in it names.
fn archive(name: Input<'_>, bytes: &[u8], whole: bool) -> Result<(Archive, Descriptors), String> {
    let file = ArchiveFile::parse(bytes).map_err(|e| format!("{name}: not an archive: {e}"))?;
    if file.is_thin() {
        return Err(format!("{name}: a thin archive, which link.exe does not read"));
    }
    match file.kind() {
        ArchiveKind::Coff | ArchiveKind::Gnu | ArchiveKind::Gnu64 | ArchiveKind::Unknown => {}
        other => return Err(format!("{name}: a {other:?} archive, not a COFF one")),
    }
    let mut members = Vec::new();
    let mut names = Names::default();
    // Each member's header offset, ascending as the members are in the file.
    let mut starts = Vec::new();
    for member in file.members() {
        let member = member.map_err(|e| format!("{name}: {e}"))?;
        let (offset, size) = member.file_range();
        starts.push(offset - MEMBER_HEADER);
        let member_name = String::from_utf8_lossy(member.name());
        let member_name = names.push(&member_name).map_err(|e| format!("{name}: {e}"))?;
        members.push(Member { name: member_name, bytes: offset as usize..(offset + size) as usize });
    }
    let mut index = Vec::new();
    let mut descriptors = Descriptors::default();
    if let Some(symbols) = file.symbols().map_err(|e| format!("{name}: {e}"))? {
        for symbol in symbols {
            let symbol = symbol.map_err(|e| format!("{name}: {e}"))?;
            let offset = symbol.offset().0;
            let symbol_name = String::from_utf8_lossy(symbol.name());
            let member = starts.binary_search(&offset).map_err(|_| {
                format!("{name}: the index names {symbol_name} in a member at {offset:#x}, where none starts")
            })? as u32;
            if symbol_name.starts_with("__IMPORT_DESCRIPTOR_") && !descriptors.contains_key(&*symbol_name) {
                let it = &members[member as usize];
                let input = Input { file: name.file, member: Some(names.at(it.name)) };
                let dll = descriptor_dll(input, &bytes[it.bytes.clone()])?;
                descriptors.insert((*symbol_name).to_owned(), dll);
            }
            index.push((names.push(&symbol_name).map_err(|e| format!("{name}: {e}"))?, member));
        }
    }
    if index.is_empty() && !members.is_empty() && !whole {
        return Err(format!(
            "{name}: an archive with no symbol index, so no member of it can be taken for a \
             symbol (index it with lib.exe or llvm-ranlib)"
        ));
    }
    Ok((Archive { members, index, names }, descriptors))
}

/// The DLL an import descriptor member names: its `.idata$6`.
fn descriptor_dll(name: Input<'_>, bytes: &[u8]) -> Result<String, String> {
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

/// An import library linked as a shared library: every member an import of some DLL. The
/// DLLs are found by name in a hash index (an umbrella library such as `windows.lib` has
/// thousands of members of hundreds of DLLs), and a DLL's name is copied once.
fn import_library(pe: &Pe, name: Input<'_>, bytes: &[u8]) -> Result<Shared, String> {
    if !bytes.starts_with(b"!<arch>\n") {
        if bytes.starts_with(b"MZ") {
            return Err(format!(
                "{name}: a DLL or executable; a PE image links against the DLL's import \
                 library (.lib), as link.exe does"
            ));
        }
        return Err(format!("{name}: not an import library"));
    }
    let (archive, descriptors) = archive(name, bytes, true)?;
    let mut dlls: Vec<Shared> = Vec::new();
    let mut of: FxHashMap<Cow<'_, str>, u32> = FxHashMap::default();
    for it in &archive.members {
        let input = Input { file: name.file, member: Some(archive.name(it.name)) };
        let (dll, import) = match member(pe, Dlls::Library(&descriptors), input, &bytes[it.bytes.clone()])? {
            Parsed::Import(dll, import) => (dll, import),
            Parsed::Object(object) if object.sections.is_empty() => continue,
            Parsed::Object(_) => {
                return Err(format!(
                    "{input}: an object in a library linked as a DLL's import library; \
                     a library with objects is linked as an archive"
                ));
            }
        };
        let at = match of.get(&*dll) {
            Some(&at) => at as usize,
            None => {
                of.insert(dll.clone(), dlls.len() as u32);
                dlls.push(shared(dll.into_owned()));
                dlls.len() - 1
            }
        };
        add(&mut dlls[at], input, import)?;
    }
    let mut dlls = dlls.into_iter();
    let mut first = dlls.next().ok_or_else(|| format!("{name}: an import library importing nothing"))?;
    first.more = dlls.collect();
    Ok(first)
}

/// The sections of a COFF object, by its section number less one, their names borrowed.
struct Sections<'d> {
    table: SectionTable<'d>,
    names: Vec<Cow<'d, str>>,
}

fn coff<'a, C: CoffHeader>(pe: &Pe, dlls: Dlls<'a>, name: Input<'_>, data: &[u8]) -> Result<Parsed<'a>, String> {
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
    let mut import = false;
    for section in table.iter() {
        let section_name = section.name(strings).map_err(|e| format!("{name}: {e}"))?;
        import |= section_name.starts_with(b".idata$");
        names.push(String::from_utf8_lossy(section_name));
    }
    let sections = Sections { table, names };
    if import {
        return long_import(dlls, name, data, &sections, &symbols);
    }
    // Every name is at most 8 bytes in its record or one string of the string table, whose
    // length is the word after the symbol table; never more than the input holds.
    let strings_at = header.pointer_to_symbol_table() as usize
        + symbols.len() * std::mem::size_of::<C::ImageSymbolBytes>();
    let string_bytes = u32_at(data, strings_at).unwrap_or(0) as usize;
    let capacity = (string_bytes + 8 * (symbols.len() + sections.names.len())).min(data.len());
    Reader { pe, name, data, sections: &sections, symbols: &symbols, names: capacity }.object().map(Parsed::Object)
}

struct Reader<'a, 'd, C: CoffHeader> {
    pe: &'a Pe,
    name: Input<'a>,
    data: &'d [u8],
    sections: &'a Sections<'d>,
    symbols: &'a SymbolTable<'d, &'d [u8], C>,
    /// What the object's `Names` is allocated with.
    names: usize,
}

/// What a COFF symbol table is to the link, from one pass over it.
struct Read {
    symbols: Vec<Symbol>,
    /// The object's symbol for each COFF symbol table index, [`NONE`] for aux records and
    /// symbols no relocation can name: 4 bytes per COFF symbol, looked up by index.
    map: Vec<u32>,
    /// Per section: its first and second symbols (a COMDAT's section symbol, with the
    /// selection, and its COMDAT symbol, naming the group), and its first symbol with a
    /// section definition record (what an associative section's parent is found by).
    first: Vec<Option<SymbolIndex>>,
    second: Vec<Option<SymbolIndex>>,
    aux: Vec<Option<SymbolIndex>>,
}

impl<'d, C: CoffHeader> Reader<'_, 'd, C> {
    fn err(&self, what: impl fmt::Display) -> String {
        format!("{}: {what}", self.name)
    }

    fn header(&self, index: usize) -> &'d pe::ImageSectionHeader {
        &self.sections.table.iter().as_slice()[index]
    }

    fn object(&self) -> Result<Object, String> {
        let mut object = Object {
            sections: Vec::with_capacity(self.sections.names.len()),
            names: Names::with_capacity(self.names),
            ..Object::default()
        };
        // `/ALTERNATENAME` pairs `(from, to)`, their names pushed as the `.drectve` is read.
        let mut alternates = Vec::new();
        for (index, section_name) in self.sections.names.iter().enumerate() {
            let section_name: &str = section_name;
            let header = self.header(index);
            let flags = header.characteristics.get(LE);
            if section_name == ".drectve" {
                let bytes = header.coff_data(self.data).map_err(|()| self.err("a truncated .drectve"))?;
                self.directives(
                    bytes,
                    &mut object.names,
                    &mut object.libraries,
                    &mut object.directives,
                    &mut alternates,
                )?;
            }
            let section = self.section(&mut object.names, section_name, header, flags)?;
            object.sections.push(section);
        }
        let read = self.symbols(&mut object.names)?;
        self.groups(&mut object, &read)?;
        for (index, header) in self.sections.table.iter().enumerate() {
            if object.sections[index].what == SectionKind::Dropped {
                continue;
            }
            let start = self.run(object.fixups.len())?;
            self.fixups(index, header, &object.sections[index].data, &read, &object.names, &mut object.fixups)?;
            object.sections[index].fixups = start..self.run(object.fixups.len())?;
        }
        object.symbols = read.symbols;
        object.symbols.reserve(alternates.len() * 2);
        for (from, to) in alternates {
            let names = &object.names;
            let found = object.symbols.iter().position(|it| {
                it.binding != Binding::Local && names.at(it.name) == names.at(to)
            });
            let target = match found {
                Some(at) => at as u32,
                None => {
                    object.symbols.push(Symbol {
                        name: to,
                        def: SymbolDef::Undefined,
                        binding: Binding::Global,
                        kind: SymbolKind::Other,
                        hidden: true,
                    });
                    object.symbols.len() as u32 - 1
                }
            };
            object.symbols.push(Symbol {
                name: from,
                def: SymbolDef::Fallback { symbol: target, search: false },
                binding: Binding::Global,
                kind: SymbolKind::Other,
                hidden: true,
            });
        }
        Ok(object)
    }

    /// The end of a run of one of the object's columns, as the contract's `u32` ranges hold it.
    fn run(&self, len: usize) -> Result<u32, String> {
        u32::try_from(len).map_err(|_| self.err("more than 4 Gi relocations or COMDAT members in one object"))
    }

    fn section(
        &self,
        names: &mut Names,
        section_name: &str,
        header: &pe::ImageSectionHeader,
        flags: u32,
    ) -> Result<Section, String> {
        let align = alignment(flags).ok_or_else(|| {
            self.err(format!("section {section_name} has alignment bits {:#x}", flags & pe::IMAGE_SCN_ALIGN_MASK))
        })?;
        let base = section_name.split('$').next().unwrap_or_default();
        let dropped = flags & (pe::IMAGE_SCN_LNK_REMOVE | pe::IMAGE_SCN_LNK_INFO) != 0 || dropped_name(section_name);
        let zero = flags & pe::IMAGE_SCN_CNT_UNINITIALIZED_DATA != 0;
        let data = if dropped {
            Data::Bytes(Vec::new())
        } else if zero {
            Data::Zero(header.size_of_raw_data.get(LE).into())
        } else {
            header
                .coff_data(self.data)
                .map_err(|()| self.err(format!("section {section_name} is past the end")))?;
            let start = u64::from(header.pointer_to_raw_data.get(LE));
            Data::Input(start..start + u64::from(header.size_of_raw_data.get(LE)))
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
            let (class, rank) =
                self.pe.section(section_name, write, exec, base == ".tls").map_err(|e| self.err(e))?;
            let class = match class {
                Class::Data if zero => Class::Bss,
                Class::Bss if !zero => Class::Data,
                class => class,
            };
            SectionKind::Alloc(class, rank)
        };
        let comdat = flags & pe::IMAGE_SCN_LNK_COMDAT != 0;
        Ok(Section {
            name: names.push(section_name).map_err(|e| self.err(e))?,
            retain: what != SectionKind::Dropped && !comdat,
            what,
            align,
            data,
            fixups: 0..0,
        })
    }

    fn symbols(&self, names: &mut Names) -> Result<Read, String> {
        let strings = self.symbols.strings();
        let count = self.sections.names.len();
        let mut read = Read {
            symbols: Vec::with_capacity(self.symbols.len()),
            map: vec![NONE; self.symbols.len()],
            first: vec![None; count],
            second: vec![None; count],
            aux: vec![None; count],
        };
        let mut weak = Vec::new();
        // `(section, value, symbol)` of every symbol defined in a section but the section's
        // own, sorted once to size each by the next one.
        let mut defined: Vec<(u32, u64, u32)> = Vec::with_capacity(self.symbols.len());
        for (index, symbol) in self.symbols.iter() {
            let class = symbol.storage_class();
            let number = symbol.section_number();
            if number > 0 && number as usize <= count {
                let at = number as usize - 1;
                if read.first[at].is_none() {
                    read.first[at] = Some(index);
                } else if read.second[at].is_none() {
                    read.second[at] = Some(index);
                }
                if read.aux[at].is_none() && symbol.has_aux_section() {
                    read.aux[at] = Some(index);
                }
            }
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
            let symbol_name = String::from_utf8_lossy(symbol.name(strings).map_err(|e| self.err(e))?);
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
                if section >= count {
                    return Err(self.err(format!("symbol {symbol_name} is in section {number}, which there isn't")));
                }
                let header = self.header(section);
                let flags = header.characteristics.get(LE);
                let (kind, size) = if symbol.has_aux_section() && value == 0 {
                    (SymbolKind::Section, u64::from(header.size_of_raw_data.get(LE)))
                } else if function || flags & (pe::IMAGE_SCN_MEM_EXECUTE | pe::IMAGE_SCN_CNT_CODE) != 0 {
                    (SymbolKind::Function, 0)
                } else {
                    (SymbolKind::Data, 0)
                };
                if kind != SymbolKind::Section {
                    defined.push((section as u32, value, read.symbols.len() as u32));
                }
                (SymbolDef::Section { section: section as u32, value, size }, kind)
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
            let name = names.push(&symbol_name).map_err(|e| self.err(e))?;
            read.map[index.0] = read.symbols.len() as u32;
            read.symbols.push(Symbol { name, def, binding, kind, hidden: true });
        }
        // A symbol's size runs to the next greater value in its section, or the section's end.
        radix::sort_copy_by_key(&mut defined, |&(section, value, symbol)| {
            (u128::from(section) << 96) | (u128::from(value) << 32) | u128::from(symbol)
        });
        for run in defined.chunk_by(|a, b| a.0 == b.0) {
            let mut end = u64::from(self.header(run[0].0 as usize).size_of_raw_data.get(LE));
            let mut at = None;
            for &(_, value, symbol) in run.iter().rev() {
                if at != Some(value) {
                    if let Some(next) = at {
                        end = next;
                    }
                    at = Some(value);
                }
                if let SymbolDef::Section { size, .. } = &mut read.symbols[symbol as usize].def {
                    *size = end.saturating_sub(value);
                }
            }
        }
        for (symbol, default, search) in weak {
            let target = read.map.get(default.0).copied().filter(|it| *it != NONE).ok_or_else(|| {
                self.err(format!(
                    "weak external {} defaults to symbol {}, which is not one",
                    names.at(read.symbols[symbol].name),
                    default.0
                ))
            })?;
            let search = match search {
                pe::IMAGE_WEAK_EXTERN_SEARCH_NOLIBRARY => false,
                pe::IMAGE_WEAK_EXTERN_SEARCH_LIBRARY | pe::IMAGE_WEAK_EXTERN_SEARCH_ALIAS => true,
                other => {
                    return Err(self.err(format!(
                        "weak external {} has search type {other}, which blaze does not link",
                        names.at(read.symbols[symbol].name)
                    )));
                }
            };
            read.symbols[symbol].def = SymbolDef::Fallback { symbol: target, search };
        }
        Ok(read)
    }

    /// The selection and parent section number of section `index`'s definition record.
    fn selection(&self, read: &Read, index: usize) -> Option<(u8, u32)> {
        let aux = self.symbols.aux_section(read.aux[index]?).ok()?;
        let mut parent = u32::from(aux.number.get(LE));
        if C::is_type_bigobj() {
            parent |= u32::from(aux.high_number.get(LE)) << 16;
        }
        Some((aux.selection, parent))
    }

    /// The section number an associative COMDAT section `index` names, if it is one.
    fn associated(&self, read: &Read, index: usize) -> Option<u32> {
        if self.header(index).characteristics.get(LE) & pe::IMAGE_SCN_LNK_COMDAT == 0 {
            return None;
        }
        match self.selection(read, index)? {
            (pe::IMAGE_COMDAT_SELECT_ASSOCIATIVE, parent) => Some(parent),
            _ => None,
        }
    }

    /// The COMDAT groups with their sections in `object.grouped`, and which sections stay
    /// whatever refers to them.
    fn groups(&self, object: &mut Object, read: &Read) -> Result<(), String> {
        let strings = self.symbols.strings();
        let count = object.sections.len();
        // Each section's group, [`NONE`] for none: 4 bytes per section.
        let mut group_of = vec![NONE; count];
        // `(group, section)`: each group's own section, then the associatives that join it;
        // sorted once by group (stably, so the group's own section stays first) into runs.
        let mut members: Vec<(u32, u32)> = Vec::new();
        let mut associate = Vec::new();
        // A local COMDAT's group name, made in one reused buffer.
        let mut local = String::new();
        for index in 0..count {
            if self.header(index).characteristics.get(LE) & pe::IMAGE_SCN_LNK_COMDAT == 0
                || object.sections[index].what == SectionKind::Dropped
            {
                continue;
            }
            let section_name = &self.sections.names[index];
            let aux = read.first[index]
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
            let leader = read.second[index]
                .and_then(|it| self.symbols.symbol(it).ok())
                .ok_or_else(|| self.err(format!("COMDAT section {section_name} has no COMDAT symbol")))?;
            let pushed = if leader.storage_class() == pe::IMAGE_SYM_CLASS_EXTERNAL {
                object.names.push(&String::from_utf8_lossy(leader.name(strings).map_err(|e| self.err(e))?))
            } else {
                local.clear();
                write!(local, "\0{}\0{index}", self.name).map_err(|e| self.err(e))?;
                object.names.push(&local)
            };
            let name = pushed.map_err(|e| self.err(e))?;
            let group = object.groups.len() as u32;
            group_of[index] = group;
            members.push((group, index as u32));
            object.groups.push(Group { name, sections: 0..0, largest });
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
                match self.associated(read, root - 1) {
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
                NONE if self.header(root - 1).characteristics.get(LE) & pe::IMAGE_SCN_LNK_COMDAT == 0 => {
                    object.sections[index].retain = true;
                }
                NONE => {}
                group => members.push((group, index as u32)),
            }
        }
        radix::sort_copy_by_key(&mut members, |it| it.0);
        object.grouped.reserve(members.len());
        for run in members.chunk_by(|a, b| a.0 == b.0) {
            let start = self.run(object.grouped.len())?;
            object.grouped.extend(run.iter().map(|it| it.1));
            object.groups[run[0].0 as usize].sections = start..self.run(object.grouped.len())?;
        }
        Ok(())
    }

    /// Appends the section's relocations to `fixups` (the object's column): a `match` on the
    /// type per relocation, the implicit addend read from the section's bytes in the input, no
    /// allocation but the column's growth.
    fn fixups(
        &self,
        index: usize,
        header: &pe::ImageSectionHeader,
        data: &Data,
        read: &Read,
        names: &Names,
        fixups: &mut Vec<NativeFixup>,
    ) -> Result<(), String> {
        let relocations = header.coff_relocations(self.data).map_err(|e| self.err(e))?;
        let section_name = &self.sections.names[index];
        let bytes = match data.bytes(self.data).map_err(|e| self.err(format!("section {section_name}: {e}")))? {
            Some(bytes) => bytes,
            None if relocations.is_empty() => return Ok(()),
            None => {
                return Err(self.err(format!("relocations in {section_name}, which has no bytes")));
            }
        };
        fixups.reserve(relocations.len());
        for relocation in relocations {
            let at = relocation.virtual_address.get(LE) as usize;
            let typ = relocation.typ.get(LE);
            if typ == pe::IMAGE_REL_AMD64_ABSOLUTE {
                continue;
            }
            let index = relocation.symbol_table_index.get(LE) as usize;
            let symbol = read.map.get(index).copied().filter(|it| *it != NONE).ok_or_else(|| {
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
                pe::IMAGE_REL_AMD64_ADDR32 => (Apply::Abs32, i64::from(u32_at(bytes, at).ok_or_else(short)?)),
                pe::IMAGE_REL_AMD64_ADDR32NB => {
                    (Apply::ImageRel32, i64::from(u32_at(bytes, at).ok_or_else(short)? as i32))
                }
                pe::IMAGE_REL_AMD64_REL32..=pe::IMAGE_REL_AMD64_REL32_5 => {
                    let implicit = i64::from(u32_at(bytes, at).ok_or_else(short)? as i32);
                    let past = 4 + i64::from(typ - pe::IMAGE_REL_AMD64_REL32);
                    (if branch { Apply::X86Branch32 } else { Apply::Rel32 }, implicit - past)
                }
                pe::IMAGE_REL_AMD64_SECTION => (Apply::Section16, i64::from(u16_at(bytes, at).ok_or_else(short)?)),
                pe::IMAGE_REL_AMD64_SECREL => {
                    (Apply::SecRel32, i64::from(u32_at(bytes, at).ok_or_else(short)? as i32))
                }
                other => {
                    return Err(self.err(format!(
                        "relocation type {other:#x} ({}) in {section_name} at {at:#x} against {}, \
                         which blaze does not link",
                        relocation_name(other),
                        names.at(target.name)
                    )));
                }
            };
            fixups.push(NativeFixup { at: at as u64, apply, symbol, addend, minus: None });
        }
        Ok(())
    }

    fn directives(
        &self,
        bytes: &[u8],
        names: &mut Names,
        libraries: &mut Vec<String>,
        directives: &mut Vec<Directive>,
        alternates: &mut Vec<(Name, Name)>,
    ) -> Result<(), String> {
        let text = String::from_utf8_lossy(bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes));
        for word in words(&text) {
            let Some(option) = word.strip_prefix(['/', '-']) else {
                return Err(self.err(format!("the .drectve word {word:?} is not an option")));
            };
            let (option, arg) = match option.split_once(':') {
                Some((option, arg)) => (option, Some(arg)),
                None => (option, None),
            };
            // The option's name lowercased on the stack: no known one is longer.
            let mut buf = [0u8; 16];
            let key = match buf.get_mut(..option.len()) {
                Some(key) => {
                    key.copy_from_slice(option.as_bytes());
                    key.make_ascii_lowercase();
                    &*key
                }
                None => &b""[..],
            };
            let need = || self.err(format!("the .drectve option /{option} needs an argument"));
            match key {
                b"defaultlib" => {
                    let library = default_library(arg.ok_or_else(need)?);
                    if !libraries.contains(&library) {
                        libraries.push(library);
                    }
                }
                b"include" => directives.push(Directive::Include(arg.ok_or_else(need)?.to_owned())),
                b"alternatename" => {
                    let arg = arg.ok_or_else(need)?;
                    let (from, to) =
                        arg.split_once('=').ok_or_else(|| self.err(format!("/ALTERNATENAME:{arg} is not from=to")))?;
                    let from = names.push(from).map_err(|e| self.err(e))?;
                    alternates.push((from, names.push(to).map_err(|e| self.err(e))?));
                }
                b"export" => directives.push(self.export(arg.ok_or_else(need)?)?),
                b"failifmismatch" => {
                    let arg = arg.ok_or_else(need)?;
                    let (key, value) = arg
                        .split_once('=')
                        .ok_or_else(|| self.err(format!("/FAILIFMISMATCH:{arg} is not key=value")))?;
                    directives.push(Directive::Mismatch { key: key.to_owned(), value: value.to_owned() });
                }
                b"nodefaultlib" => {
                    directives.push(Directive::NoDefaultLib(arg.map(default_library)));
                }
                // These change nothing a program observes: section merging (every section of
                // a protection is laid out together already), incremental-link padding, the
                // control-flow-guard tables of /guard:cf (off), a check that a library is not
                // linked.
                b"merge" | b"editandcontinue" | b"guardsym" | b"disallowlib" => {}
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
            if part.eq_ignore_ascii_case("DATA") {
                data = true;
            } else if !part.eq_ignore_ascii_case("PRIVATE") {
                return Err(self.err(format!(
                    "/EXPORT:{arg}: exports by ordinal (@n, NONAME), which blaze does not link"
                )));
            }
        }
        Ok(Directive::Export { name: name.to_owned(), symbol: symbol.to_owned(), data })
    }
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
        || matches!(base, ".gfids" | ".giats" | ".gljmp" | ".gehcont" | ".sxdata" | ".voltbl" | ".chks64")
        || matches!(name, ".llvm_addrsig" | ".llvm.call-graph-profile" | ".llvmbc" | ".llvmcmd")
}

/// A `/DEFAULTLIB` name as link.exe takes it: `.lib` added when it has no extension.
fn default_library(name: &str) -> String {
    let file = name.rsplit(['/', '\\']).next().unwrap_or(name);
    if file.contains('.') { name.to_owned() } else { format!("{name}.lib") }
}

/// The words of a `.drectve`: split at spaces outside quotes, the quotes (and NULs outside
/// them) removed. A word with neither is borrowed from `text`.
pub(super) fn words(text: &str) -> Vec<Cow<'_, str>> {
    let mut words = Vec::new();
    let mut start = None;
    let mut owned: Option<String> = None;
    let mut quoted = false;
    for (at, c) in text.char_indices() {
        let kept = match c {
            '"' => {
                quoted = !quoted;
                false
            }
            c if c.is_whitespace() && !quoted => {
                if let Some(from) = start.take() {
                    words.push(owned.take().map_or(Cow::Borrowed(&text[from..at]), Cow::Owned));
                }
                continue;
            }
            '\0' if !quoted && start.is_none() => continue,
            '\0' if !quoted => false,
            _ => true,
        };
        let from = *start.get_or_insert(at);
        match (&mut owned, kept) {
            (Some(word), true) => word.push(c),
            (None, false) => owned = Some(text[from..at].to_owned()),
            (Some(_), false) | (None, true) => {}
        }
    }
    if let Some(from) = start {
        words.push(owned.map_or(Cow::Borrowed(&text[from..]), Cow::Owned));
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
/// hold the import tables the writer makes itself, so they are read as empty objects. Names
/// are compared as the input's bytes; only the import's own is copied.
fn long_import<'a, 'd, C: CoffHeader>(
    dlls: Dlls<'a>,
    name: Input<'_>,
    data: &'d [u8],
    sections: &Sections<'d>,
    symbols: &SymbolTable<'d, &'d [u8], C>,
) -> Result<Parsed<'a>, String> {
    let strings = symbols.strings();
    let headers = sections.table.iter().as_slice();
    let mut imp: Option<(&'d [u8], usize)> = None;
    let mut descriptor: Option<&'d [u8]> = None;
    let mut defined_in_code: Vec<&'d [u8]> = Vec::new();
    for (_, symbol) in symbols.iter() {
        if symbol.storage_class() != pe::IMAGE_SYM_CLASS_EXTERNAL {
            continue;
        }
        let symbol_name = symbol.name(strings).map_err(|e| format!("{name}: {e}"))?;
        let number = symbol.section_number();
        if number > 0 {
            if symbol_name.starts_with(b"__IMPORT_DESCRIPTOR_")
                || symbol_name == b"__NULL_IMPORT_DESCRIPTOR"
                || symbol_name.ends_with(b"_NULL_THUNK_DATA")
            {
                return Ok(Parsed::Object(Object::default()));
            }
            let section = number as usize - 1;
            if let Some(stripped) = symbol_name.strip_prefix(b"__imp_") {
                if sections.names.get(section).is_some_and(|it| it.starts_with(".idata$5")) {
                    imp = Some((stripped, section));
                }
            } else if let Some(header) = headers.get(section) {
                let flags = header.characteristics.get(LE);
                if flags & (pe::IMAGE_SCN_MEM_EXECUTE | pe::IMAGE_SCN_CNT_CODE) != 0 {
                    defined_in_code.push(symbol_name);
                }
            }
        } else if number == pe::IMAGE_SYM_UNDEFINED && symbol_name.starts_with(b"__IMPORT_DESCRIPTOR_") {
            descriptor = Some(symbol_name);
        }
    }
    let Some((stripped, section)) = imp else {
        return Err(format!(
            "{name}: an object with .idata$ sections that defines no __imp_ symbol in .idata$5: \
             the import tables are the linker's"
        ));
    };
    let symbol = lossy(stripped);
    let descriptor = String::from_utf8_lossy(descriptor.ok_or_else(|| {
        format!("{name}: the import member of {symbol} names no __IMPORT_DESCRIPTOR_ (a GNU dlltool library?)")
    })?);
    let dll = dlls
        .get(&descriptor)
        .ok_or_else(|| format!("{name}: no archive read so far has {descriptor}, the DLL {symbol} is from"))?;
    let header = headers.get(section).ok_or_else(|| format!("{name}: no .idata$5"))?;
    let thunk = header.coff_data(data).map_err(|()| format!("{name}: a truncated .idata$5"))?;
    let relocations = header.coff_relocations(data).map_err(|e| format!("{name}: {e}"))?;
    let version = match relocations.first() {
        Some(relocation) => {
            let target = symbols
                .symbol(SymbolIndex(relocation.symbol_table_index.get(LE) as usize))
                .map_err(|e| format!("{name}: {e}"))?;
            let number = target.section_number();
            let names = (number > 0)
                .then(|| headers.get(number as usize - 1))
                .flatten()
                .ok_or_else(|| format!("{name}: the import name of {symbol} is in no section"))?;
            let bytes = names.coff_data(data).map_err(|()| format!("{name}: a truncated .idata$6"))?;
            let at = target.value() as usize + u64_at(thunk, 0).unwrap_or(0) as usize;
            let hint = u16_at(bytes, at).ok_or_else(|| format!("{name}: a truncated .idata$6"))?;
            let export = bytes[at + 2..].split(|it| *it == 0).next().unwrap_or_default();
            format!("{hint} {}", String::from_utf8_lossy(export))
        }
        None => {
            let value = u64_at(thunk, 0).ok_or_else(|| format!("{name}: a truncated .idata$5"))?;
            if value & pe::IMAGE_ORDINAL_FLAG64 == 0 {
                return Err(format!("{name}: the import of {symbol} has neither a name nor an ordinal"));
            }
            format!("#{}", value as u16)
        }
    };
    let kind = if defined_in_code.contains(&stripped) { SymbolKind::Function } else { SymbolKind::Data };
    Ok(Parsed::Import(dll, Imported { name: Cow::Owned(symbol), version, kind }))
}

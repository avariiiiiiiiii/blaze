//! Reads wasm objects as LLVM writes them (wasi-libc's members, `crt1-command.o`, objects of
//! C dependencies) and archives of them, into the contract's [`Object`] and [`Archive`].
//!
//! An object's function becomes a [`Class::Text`] piece in the form blaze's codegen gives its
//! own ([`Code`]: the header, then the body), so the writer reads every code piece one way.
//! Its data segments become pieces by their names (`.rodata.*`, `.data.*`, `.bss.*`, or a
//! named section for `__start_`/`__stop_`), its custom sections custom-section pieces, its
//! init functions [`Class::InitArray`] pieces of their priority holding the function's slot.
//! The format: <https://github.com/WebAssembly/tool-conventions/blob/main/Linking.md>.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use blaze_codegen::wasm::{Code, Import, Signature, ValType};

use super::reader::{self, Reader};
use super::reloc::{self, Kind};
use crate::model::{
    Apply, Archive, Binding, Class, Data, Group, Member, NativeFile, NativeFixup, Object, Section,
    SectionKind, Symbol, SymbolDef, SymbolKind,
};
use crate::output::NativeKind;

mod id {
    pub(super) const CUSTOM: u8 = 0;
    pub(super) const TYPE: u8 = 1;
    pub(super) const IMPORT: u8 = 2;
    pub(super) const FUNCTION: u8 = 3;
    pub(super) const TABLE: u8 = 4;
    pub(super) const MEMORY: u8 = 5;
    pub(super) const GLOBAL: u8 = 6;
    pub(super) const EXPORT: u8 = 7;
    pub(super) const START: u8 = 8;
    pub(super) const ELEMENT: u8 = 9;
    pub(super) const CODE: u8 = 10;
    pub(super) const DATA: u8 = 11;
    pub(super) const DATA_COUNT: u8 = 12;
    pub(super) const TAG: u8 = 13;
}

/// The subsections of the `linking` section.
const SEGMENT_INFO: u8 = 5;
const INIT_FUNCS: u8 = 6;
const COMDAT_INFO: u8 = 7;
const SYMBOL_TABLE: u8 = 8;
const LINKING_VERSION: u32 = 2;

const SYM_FUNCTION: u8 = 0;
const SYM_DATA: u8 = 1;
const SYM_GLOBAL: u8 = 2;
const SYM_SECTION: u8 = 3;
const SYM_TAG: u8 = 4;
const SYM_TABLE: u8 = 5;

const BINDING_WEAK: u32 = 0x1;
const BINDING_LOCAL: u32 = 0x2;
const VISIBILITY_HIDDEN: u32 = 0x4;
const UNDEFINED: u32 = 0x10;
const EXPORTED: u32 = 0x20;
const EXPLICIT_NAME: u32 = 0x40;
const NO_STRIP: u32 = 0x80;
const TLS: u32 = 0x100;
const ABSOLUTE: u32 = 0x200;

const SEGMENT_TLS: u32 = 0x2;
const SEGMENT_RETAIN: u32 = 0x4;

const COMDAT_DATA: u8 = 0;
const COMDAT_FUNCTION: u8 = 1;
const COMDAT_SECTION: u8 = 5;

pub(super) fn read(name: &str, bytes: &[u8], kind: &NativeKind) -> Result<NativeFile, String> {
    match kind {
        NativeKind::Object => {
            object(bytes).map(NativeFile::Object).map_err(|e| format!("{name}: {e}"))
        }
        NativeKind::Archive { whole } => {
            archive(bytes, *whole).map(NativeFile::Archive).map_err(|e| format!("{name}: {e}"))
        }
        NativeKind::Shared { .. } => Err(format!(
            "{name}: a shared library, and a wasm32 module links none (wasm-ld takes them only \
             with -pie or -shared, which rustc's wasm targets don't use)"
        )),
    }
}

fn archive(bytes: &[u8], whole: bool) -> Result<Archive, String> {
    use ::object::read::archive::ArchiveFile;

    let file = ArchiveFile::parse(bytes).map_err(|e| format!("an archive: {e}"))?;
    if file.is_thin() {
        return Err("a thin archive, whose members are other files".to_owned());
    }
    let mut members = Vec::new();
    let mut by_offset = BTreeMap::new();
    for member in file.members() {
        let member = member.map_err(|e| format!("an archive member: {e}"))?;
        let (offset, size) = member.file_range();
        let start = usize::try_from(offset).map_err(|_| "an archive past the address space")?;
        let end = usize::try_from(offset + size).map_err(|_| "an archive past the address space")?;
        by_offset.insert(offset, members.len() as u32);
        let name = String::from_utf8_lossy(member.name()).into_owned();
        members.push(Member { name, bytes: start..end });
    }
    let mut index = Vec::new();
    match file.symbols().map_err(|e| format!("the archive's index: {e}"))? {
        Some(symbols) => {
            for symbol in symbols {
                let symbol = symbol.map_err(|e| format!("the archive's index: {e}"))?;
                let name = std::str::from_utf8(symbol.name())
                    .map_err(|_| "a name in the archive's index that is not UTF-8")?;
                let member = file.member(symbol.offset()).map_err(|e| {
                    format!("the archive's index names `{name}` in no member: {e}")
                })?;
                let member = by_offset
                    .get(&member.file_range().0)
                    .ok_or_else(|| format!("the archive's index names `{name}` in no member"))?;
                index.push((name.to_owned(), *member));
            }
        }
        None if members.is_empty() || whole => {}
        None => {
            return Err("an archive without an index of its symbols (run `llvm-ranlib` on it)"
                .to_owned());
        }
    }
    Ok(Archive { members, index })
}

struct Imported<'a> {
    module: &'a str,
    field: &'a str,
    ty: u32,
}

struct Custom<'a> {
    /// Its index among the module's sections, which relocations and symbols name it by.
    index: usize,
    name: &'a str,
    /// Its contents after the name, and where they start in the file.
    payload: Range<usize>,
}

/// The sections of an object, read.
struct Parsed<'a> {
    sections: Vec<reader::Section<'a>>,
    types: Vec<Signature>,
    imported: Vec<Imported<'a>>,
    imported_globals: Vec<&'a str>,
    imported_tables: Vec<&'a str>,
    /// The type of each function with a body.
    functions: Vec<u32>,
    /// Export names by function index.
    exports: BTreeMap<u32, &'a str>,
    code: Option<usize>,
    /// Where each body is in the file, after its size.
    bodies: Vec<Range<usize>>,
    data: Option<usize>,
    /// Where each segment's bytes are in the file.
    segments: Vec<Range<usize>>,
    customs: Vec<Custom<'a>>,
    linking: Option<&'a [u8]>,
    relocs: Vec<(&'a str, &'a [u8])>,
}

fn parse(bytes: &[u8]) -> Result<Parsed<'_>, String> {
    let sections = reader::sections(bytes)?;
    let mut parsed = Parsed {
        sections: Vec::new(),
        types: Vec::new(),
        imported: Vec::new(),
        imported_globals: Vec::new(),
        imported_tables: Vec::new(),
        functions: Vec::new(),
        exports: BTreeMap::new(),
        code: None,
        bodies: Vec::new(),
        data: None,
        segments: Vec::new(),
        customs: Vec::new(),
        linking: None,
        relocs: Vec::new(),
    };
    for (index, section) in sections.iter().enumerate() {
        let mut r = Reader::new(section.contents);
        let at = section.at;
        let what = |e: String| format!("section {index} (id {}): {e}", section.id);
        let mut one = || -> Result<(), String> {
            match section.id {
                id::CUSTOM => {
                    let name = r.name()?;
                    let payload = at + r.at()..section.range().end;
                    let contents = r.rest();
                    if name == "linking" {
                        parsed.linking = Some(contents);
                    } else if name.starts_with("reloc.") {
                        parsed.relocs.push((name, contents));
                    } else {
                        parsed.customs.push(Custom { index, name, payload });
                    }
                }
                id::TYPE => {
                    for _ in 0..r.count()? {
                        parsed.types.push(signature(&mut r)?);
                    }
                }
                id::IMPORT => {
                    for _ in 0..r.count()? {
                        let module = r.name()?;
                        let field = r.name()?;
                        match r.u8()? {
                            0 => parsed.imported.push(Imported { module, field, ty: r.u32()? }),
                            1 => {
                                r.u8()?;
                                limits(&mut r)?;
                                parsed.imported_tables.push(field);
                            }
                            2 => limits(&mut r)?,
                            3 => {
                                r.u8()?;
                                r.u8()?;
                                parsed.imported_globals.push(field);
                            }
                            4 => {
                                return Err(format!(
                                    "the tag import {module}.{field}: wasm exception handling \
                                     is not linked"
                                ));
                            }
                            kind => return Err(format!("the import {module}.{field} of kind {kind}")),
                        }
                    }
                }
                id::FUNCTION => {
                    for _ in 0..r.count()? {
                        parsed.functions.push(r.u32()?);
                    }
                }
                id::EXPORT => {
                    for _ in 0..r.count()? {
                        let name = r.name()?;
                        match (r.u8()?, r.u32()?) {
                            (0, function) => {
                                parsed.exports.insert(function, name);
                            }
                            (kind, _) => {
                                return Err(format!(
                                    "the export `{name}` of kind {kind}: an object exports only \
                                     functions"
                                ));
                            }
                        }
                    }
                }
                id::CODE => {
                    parsed.code = Some(index);
                    for _ in 0..r.count()? {
                        let len = r.u32()? as usize;
                        let start = at + r.at();
                        r.bytes(len)?;
                        parsed.bodies.push(start..start + len);
                    }
                }
                id::DATA => {
                    parsed.data = Some(index);
                    for _ in 0..r.count()? {
                        match r.u32()? {
                            0 => {}
                            1 => {
                                return Err("a passive data segment (shared memory)".to_owned());
                            }
                            2 => match r.u32()? {
                                0 => {}
                                memory => {
                                    return Err(format!("a data segment of memory {memory}"));
                                }
                            },
                            flags => return Err(format!("a data segment of flags {flags}")),
                        }
                        r.i32_const()?;
                        let len = r.u32()? as usize;
                        let start = at + r.at();
                        r.bytes(len)?;
                        parsed.segments.push(start..start + len);
                    }
                }
                id::DATA_COUNT => {
                    r.u32()?;
                }
                id::TABLE | id::MEMORY | id::GLOBAL | id::START | id::ELEMENT | id::TAG => {
                    return Err(format!(
                        "a {} section, which LLVM's objects don't have: linked module or \
                         position-independent code?",
                        match section.id {
                            id::TABLE => "table",
                            id::MEMORY => "memory",
                            id::GLOBAL => "global",
                            id::START => "start",
                            id::ELEMENT => "element",
                            _ => "tag",
                        }
                    ));
                }
                other => return Err(format!("a section of the unknown id {other}")),
            }
            if !r.done() {
                return Err("bytes after its contents".to_owned());
            }
            Ok(())
        };
        one().map_err(what)?;
    }
    if parsed.bodies.len() != parsed.functions.len() {
        return Err(format!(
            "{} function types and {} bodies",
            parsed.functions.len(),
            parsed.bodies.len()
        ));
    }
    parsed.sections = sections;
    Ok(parsed)
}

fn signature(r: &mut Reader<'_>) -> Result<Signature, String> {
    match r.u8()? {
        0x60 => {}
        form => {
            return Err(format!("a type of form {form:#04x} (GC types are not linked)"));
        }
    }
    let mut lists = [Vec::new(), Vec::new()];
    for list in &mut lists {
        for _ in 0..r.count()? {
            let byte = r.u8()?;
            list.push(ValType::from_byte(byte).ok_or_else(|| format!("the value type {byte:#04x}"))?);
        }
    }
    let [params, results] = lists;
    Ok(Signature { params, results })
}

fn limits(r: &mut Reader<'_>) -> Result<(), String> {
    let flags = r.u8()?;
    if flags > 7 {
        return Err(format!("limits of flags {flags:#x}"));
    }
    r.u64()?;
    if flags & 1 != 0 {
        r.u64()?;
    }
    Ok(())
}

struct WasmSymbol<'a> {
    kind: u8,
    flags: u32,
    /// A function's, global's, table's or section's index.
    index: u32,
    name: Option<&'a str>,
    /// A defined data symbol's segment, offset and size.
    data: Option<(u32, u64, u64)>,
}

struct SegmentInfo<'a> {
    name: &'a str,
    align: u32,
    flags: u32,
}

#[derive(Default)]
struct Linking<'a> {
    symbols: Vec<WasmSymbol<'a>>,
    segments: Vec<SegmentInfo<'a>>,
    inits: Vec<(u32, u32)>,
    comdats: Vec<(&'a str, Vec<(u8, u32)>)>,
}

fn linking<'a>(contents: &'a [u8]) -> Result<Linking<'a>, String> {
    let mut r = Reader::new(contents);
    match r.u32()? {
        LINKING_VERSION => {}
        version => return Err(format!("a linking section of version {version}")),
    }
    let mut linking = Linking::default();
    while !r.done() {
        let id = r.u8()?;
        let len = r.u32()? as usize;
        let mut s = Reader::new(r.bytes(len)?);
        match id {
            SEGMENT_INFO => {
                for _ in 0..s.count()? {
                    let name = s.name()?;
                    let align = s.u32()?;
                    let flags = s.u32()?;
                    linking.segments.push(SegmentInfo { name, align, flags });
                }
            }
            INIT_FUNCS => {
                for _ in 0..s.count()? {
                    let priority = s.u32()?;
                    linking.inits.push((priority, s.u32()?));
                }
            }
            COMDAT_INFO => {
                for _ in 0..s.count()? {
                    let name = s.name()?;
                    match s.u32()? {
                        0 => {}
                        flags => return Err(format!("the COMDAT `{name}` of flags {flags}")),
                    }
                    let mut entries = Vec::new();
                    for _ in 0..s.count()? {
                        let kind = s.u8()?;
                        entries.push((kind, s.u32()?));
                    }
                    linking.comdats.push((name, entries));
                }
            }
            SYMBOL_TABLE => {
                for _ in 0..s.count()? {
                    linking.symbols.push(symbol(&mut s)?);
                }
            }
            other => return Err(format!("the linking subsection {other}")),
        }
        if !s.done() {
            return Err(format!("bytes after the linking subsection {id}"));
        }
    }
    Ok(linking)
}

fn symbol<'a>(r: &mut Reader<'a>) -> Result<WasmSymbol<'a>, String> {
    let kind = r.u8()?;
    let flags = r.u32()?;
    let mut symbol = WasmSymbol { kind, flags, index: 0, name: None, data: None };
    match kind {
        SYM_FUNCTION | SYM_GLOBAL | SYM_TAG | SYM_TABLE => {
            symbol.index = r.u32()?;
            if flags & UNDEFINED == 0 || flags & EXPLICIT_NAME != 0 {
                symbol.name = Some(r.name()?);
            }
        }
        SYM_DATA => {
            symbol.name = Some(r.name()?);
            if flags & UNDEFINED == 0 {
                let segment = r.u32()?;
                let offset = r.u64()?;
                symbol.data = Some((segment, offset, r.u64()?));
            }
        }
        SYM_SECTION => symbol.index = r.u32()?,
        other => return Err(format!("a symbol of the unknown kind {other}")),
    }
    Ok(symbol)
}

struct Reloc {
    r_type: u32,
    /// From the start of the section's contents (after its id and size).
    offset: usize,
    index: u32,
    addend: i64,
}

fn relocs(contents: &[u8]) -> Result<(usize, Vec<Reloc>), String> {
    let mut r = Reader::new(contents);
    let section = r.u32()? as usize;
    let mut relocs = Vec::new();
    for _ in 0..r.count()? {
        let r_type = r.u32()?;
        let offset = r.u32()? as usize;
        let index = r.u32()?;
        let addend = if reloc::has_addend(r_type) { r.i64()? } else { 0 };
        relocs.push(Reloc { r_type, offset, index, addend });
    }
    if !r.done() {
        return Err("bytes after the relocations".to_owned());
    }
    Ok((section, relocs))
}

/// What a data segment's name places it in, as wasm-ld names output segments.
fn segment_class(name: &str, flags: u32) -> Result<Class, String> {
    let is = |prefix: &str| {
        name.strip_prefix(prefix).is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
    };
    if flags & SEGMENT_TLS != 0 || is(".tdata") || is(".tbss") {
        return Err(format!(
            "the thread-local data segment `{name}`: wasm32 without shared memory has no TLS \
             (LLVM makes thread-locals plain data there)"
        ));
    }
    Ok(if is(".rodata") {
        Class::Rodata
    } else if is(".data") {
        Class::Data
    } else if is(".bss") {
        Class::Bss
    } else {
        super::named_data(name)
    })
}

/// The bytes a relocation of `width` bytes at `at` in the file falls in: the index of the
/// range among `ranges` (sorted, disjoint) and the offset in it.
fn within(ranges: &[Range<usize>], at: usize, width: usize) -> Option<(usize, usize)> {
    let after = ranges.partition_point(|it| it.start <= at);
    let index = after.checked_sub(1)?;
    let range = &ranges[index];
    (at + width <= range.end).then_some((index, at - range.start))
}

fn width(r_type: u32) -> usize {
    match r_type {
        reloc::TABLE_INDEX_I32
        | reloc::MEMORY_ADDR_I32
        | reloc::GLOBAL_INDEX_I32
        | reloc::FUNCTION_INDEX_I32
        | reloc::FUNCTION_OFFSET_I32
        | reloc::SECTION_OFFSET_I32 => 4,
        _ => super::leb::PADDED,
    }
}

fn object(bytes: &[u8]) -> Result<Object, String> {
    let parsed = parse(bytes)?;
    let linking = linking(parsed.linking.ok_or(
        "no linking section: not a relocatable object (a linked module can't be linked again)",
    )?)?;
    if linking.segments.len() != parsed.segments.len() {
        return Err(format!(
            "{} data segments and {} of them in the linking section",
            parsed.segments.len(),
            linking.segments.len()
        ));
    }
    let imported = parsed.imported.len() as u32;

    // The relocations of each section, by the section's index in the module.
    let mut by_section: BTreeMap<usize, Vec<Reloc>> = BTreeMap::new();
    for &(name, contents) in &parsed.relocs {
        let (section, relocs) = relocs(contents).map_err(|e| format!("{name}: {e}"))?;
        if section >= parsed.sections.len() {
            return Err(format!("{name} is of section {section}, which there is not"));
        }
        by_section.entry(section).or_default().extend(relocs);
    }
    let start_of = |section: usize| parsed.sections[section].at;
    let named = |r: &Reloc| reloc::name(r.r_type).unwrap_or("an unknown relocation");

    let mut sections = Vec::new();
    let mut retain = BTreeSet::new();

    // Functions.
    let mut function_names: BTreeMap<u32, &str> = BTreeMap::new();
    let mut exported: BTreeMap<u32, &str> = BTreeMap::new();
    for symbol in &linking.symbols {
        if symbol.kind == SYM_FUNCTION && symbol.flags & UNDEFINED == 0 {
            if let Some(name) = symbol.name {
                function_names.entry(symbol.index).or_insert(name);
                if symbol.flags & EXPORTED != 0 {
                    exported.entry(symbol.index).or_insert(name);
                }
            }
        }
    }
    // Each with its offset from the start of the body.
    let mut code_relocs: Vec<Vec<(usize, &Reloc)>> = vec![Vec::new(); parsed.bodies.len()];
    if let Some(code) = parsed.code {
        for r in by_section.get(&code).into_iter().flatten() {
            let at = start_of(code) + r.offset;
            let (body, offset) = within(&parsed.bodies, at, width(r.r_type)).ok_or_else(|| {
                format!("{} at {:#x} of the code section, in no function body", named(r), r.offset)
            })?;
            code_relocs[body].push((offset, r));
        }
    }
    let mut function_sections = Vec::new();
    for (body, range) in parsed.bodies.iter().enumerate() {
        let index = imported + body as u32;
        let ty = parsed.functions[body];
        let ty = parsed.types.get(ty as usize).ok_or_else(|| format!("no type {ty}"))?.clone();
        let mut code = Code {
            ty,
            export: parsed.exports.get(&index).or(exported.get(&index)).map(|it| (*it).to_owned()),
            ..Code::default()
        };
        let mut fixups = Vec::new();
        for &(at, r) in &code_relocs[body] {
            match reloc::kind(r.r_type)? {
                Kind::Applied => fixups.push(NativeFixup {
                    at: at as u64,
                    apply: Apply::Native(r.r_type),
                    symbol: r.index,
                    addend: r.addend,
                    minus: None,
                }),
                Kind::Type => {
                    let ty = parsed
                        .types
                        .get(r.index as usize)
                        .ok_or_else(|| format!("{}: no type {}", named(r), r.index))?;
                    let which = match code.types.iter().position(|it| it == ty) {
                        Some(which) => which,
                        None => {
                            code.types.push(ty.clone());
                            code.types.len() - 1
                        }
                    };
                    code.type_fixups.push((at as u32, which as u32));
                }
                Kind::Debug => {
                    return Err(format!("{} in the code of function {index}", named(r)));
                }
            }
        }
        let (data, start) = code.encode(&bytes[range.clone()]);
        for fixup in &mut fixups {
            fixup.at += u64::from(start);
        }
        if code.export.is_some() {
            retain.insert(sections.len());
        }
        let name = match function_names.get(&index) {
            Some(name) => format!(".text.{name}"),
            None => format!(".text.{index}"),
        };
        function_sections.push(sections.len() as u32);
        sections.push(Section {
            name,
            what: SectionKind::Alloc(Class::Text, 0),
            align: 1,
            data: Data::Bytes(data),
            fixups,
            retain: false,
        });
    }

    // Data segments.
    let mut segment_fixups: Vec<Vec<NativeFixup>> = vec![Vec::new(); parsed.segments.len()];
    if let Some(data) = parsed.data {
        for r in by_section.get(&data).into_iter().flatten() {
            let at = start_of(data) + r.offset;
            let (segment, offset) = within(&parsed.segments, at, width(r.r_type)).ok_or_else(|| {
                format!("{} at {:#x} of the data section, in no segment", named(r), r.offset)
            })?;
            match reloc::kind(r.r_type)? {
                Kind::Applied => segment_fixups[segment].push(NativeFixup {
                    at: offset as u64,
                    apply: Apply::Native(r.r_type),
                    symbol: r.index,
                    addend: r.addend,
                    minus: None,
                }),
                Kind::Type | Kind::Debug => {
                    return Err(format!("{} in the data segment {segment}", named(r)));
                }
            }
        }
    }
    let mut segment_sections = Vec::new();
    for (segment, (range, info)) in parsed.segments.iter().zip(&linking.segments).enumerate() {
        let class = segment_class(info.name, info.flags)?;
        let contents = &bytes[range.clone()];
        let data = match class {
            Class::Bss if contents.iter().all(|&it| it == 0) => Data::Zero(contents.len() as u64),
            Class::Bss => {
                return Err(format!("the zeroed segment `{}` holds bytes other than 0", info.name));
            }
            _ => Data::Bytes(contents.to_vec()),
        };
        if info.align >= 32 {
            return Err(format!("the segment `{}` aligned to 2^{}", info.name, info.align));
        }
        if info.flags & SEGMENT_RETAIN != 0 {
            retain.insert(sections.len());
        }
        segment_sections.push(sections.len() as u32);
        sections.push(Section {
            name: info.name.to_owned(),
            what: SectionKind::Alloc(class, 0),
            align: 1 << info.align,
            data,
            fixups: std::mem::take(&mut segment_fixups[segment]),
            retain: false,
        });
    }

    // Custom sections.
    let mut custom_sections = BTreeMap::new();
    for custom in &parsed.customs {
        let dropped = custom.name.starts_with(".debug")
            || matches!(custom.name, "name" | "external_debug_info" | "sourceMappingURL");
        if custom.name.starts_with("dylink") {
            return Err(format!(
                "the `{}` section of a position-independent object, which a wasm32 module \
                 doesn't link",
                custom.name
            ));
        }
        let mut fixups = Vec::new();
        for r in by_section.get(&custom.index).into_iter().flatten() {
            let at = start_of(custom.index) + r.offset;
            let offset = at
                .checked_sub(custom.payload.start)
                .filter(|&it| it + width(r.r_type) <= custom.payload.len())
                .ok_or_else(|| format!("{} outside the section `{}`", named(r), custom.name))?;
            match (reloc::kind(r.r_type)?, dropped) {
                (Kind::Applied, _) | (Kind::Debug, true) => fixups.push(NativeFixup {
                    at: offset as u64,
                    apply: Apply::Native(r.r_type),
                    symbol: r.index,
                    addend: r.addend,
                    minus: None,
                }),
                (Kind::Type | Kind::Debug, _) => {
                    return Err(format!("{} in the section `{}`", named(r), custom.name));
                }
            }
        }
        custom_sections.insert(custom.index, sections.len() as u32);
        if !dropped {
            retain.insert(sections.len());
        }
        sections.push(Section {
            name: custom.name.to_owned(),
            what: match dropped {
                true => SectionKind::Dropped,
                false => SectionKind::Alloc(super::custom(custom.name), 0),
            },
            align: 1,
            data: Data::Bytes(bytes[custom.payload.clone()].to_vec()),
            fixups,
            retain: false,
        });
    }
    let function_section = |index: u32| {
        index
            .checked_sub(imported)
            .and_then(|body| function_sections.get(body as usize).copied())
            .ok_or_else(|| format!("function {index}, which the object doesn't define"))
    };

    // Symbols.
    let mut symbols = Vec::with_capacity(linking.symbols.len());
    for (at, s) in linking.symbols.iter().enumerate() {
        let binding = if s.flags & BINDING_LOCAL != 0 {
            Binding::Local
        } else if s.flags & BINDING_WEAK != 0 {
            Binding::Weak
        } else {
            Binding::Global
        };
        let hidden = s.flags & VISIBILITY_HIDDEN != 0;
        let undefined = s.flags & UNDEFINED != 0;
        let unnamed = || format!("symbol {at} has no name");
        let symbol = match s.kind {
            SYM_FUNCTION if undefined => {
                let import = parsed
                    .imported
                    .get(s.index as usize)
                    .ok_or_else(|| format!("symbol {at}: no imported function {}", s.index))?;
                let ty = parsed
                    .types
                    .get(import.ty as usize)
                    .ok_or_else(|| format!("symbol {at}: no type {}", import.ty))?;
                Symbol {
                    name: s.name.unwrap_or(import.field).to_owned(),
                    def: SymbolDef::Undefined,
                    binding,
                    kind: SymbolKind::Function,
                    hidden,
                    import: Some(Import {
                        module: import.module.to_owned(),
                        field: import.field.to_owned(),
                        ty: ty.clone(),
                    }),
                }
            }
            SYM_FUNCTION => {
                let section = function_section(s.index).map_err(|e| format!("symbol {at}: {e}"))?;
                if s.flags & (EXPORTED | NO_STRIP) != 0 {
                    retain.insert(section as usize);
                }
                Symbol {
                    name: s.name.ok_or_else(unnamed)?.to_owned(),
                    def: SymbolDef::Section {
                        section,
                        value: 0,
                        size: sections[section as usize].data.len(),
                    },
                    binding,
                    kind: SymbolKind::Function,
                    hidden,
                    import: None,
                }
            }
            SYM_DATA => {
                let name = s.name.ok_or_else(unnamed)?;
                if s.flags & TLS != 0 {
                    return Err(format!(
                        "the thread-local `{name}`: wasm32 without shared memory has no TLS"
                    ));
                }
                if s.flags & EXPORTED != 0 {
                    return Err(format!("`{name}` is data and exported: only functions are"));
                }
                let def = match s.data {
                    None => SymbolDef::Undefined,
                    Some((_, offset, _)) if s.flags & ABSOLUTE != 0 => SymbolDef::Absolute(offset),
                    Some((segment, value, size)) => {
                        let section = *segment_sections
                            .get(segment as usize)
                            .ok_or_else(|| format!("`{name}` is in no segment {segment}"))?;
                        let len = sections[section as usize].data.len();
                        if value.checked_add(size).is_none_or(|end| end > len) {
                            return Err(format!("`{name}` reaches past its segment"));
                        }
                        if s.flags & NO_STRIP != 0 {
                            retain.insert(section as usize);
                        }
                        SymbolDef::Section { section, value, size }
                    }
                };
                let kind = SymbolKind::Data;
                Symbol { name: name.to_owned(), def, binding, kind, hidden, import: None }
            }
            SYM_GLOBAL | SYM_TABLE if undefined => {
                let fields = match s.kind {
                    SYM_GLOBAL => &parsed.imported_globals,
                    _ => &parsed.imported_tables,
                };
                let name = s.name.or_else(|| fields.get(s.index as usize).copied());
                Symbol {
                    name: name.ok_or_else(unnamed)?.to_owned(),
                    def: SymbolDef::Undefined,
                    binding,
                    kind: SymbolKind::Other,
                    hidden,
                    import: None,
                }
            }
            SYM_GLOBAL | SYM_TABLE => {
                return Err(format!(
                    "symbol {at} defines a {}: the module's globals and table are the linker's",
                    if s.kind == SYM_GLOBAL { "global" } else { "table" }
                ));
            }
            SYM_SECTION => {
                let section = *custom_sections
                    .get(&(s.index as usize))
                    .ok_or_else(|| format!("symbol {at} of section {}, no custom one", s.index))?;
                Symbol {
                    name: sections[section as usize].name.clone(),
                    def: SymbolDef::Section { section, value: 0, size: 0 },
                    binding: Binding::Local,
                    kind: SymbolKind::Section,
                    hidden: false,
                    import: None,
                }
            }
            SYM_TAG => {
                return Err(format!(
                    "symbol {at} is an exception tag: wasm exception handling is not linked"
                ));
            }
            other => return Err(format!("symbol {at} of the unknown kind {other}")),
        };
        symbols.push(symbol);
    }

    // Init functions: each a pointer in the init array, called by `__wasm_call_ctors`.
    for &(priority, symbol) in &linking.inits {
        match linking.symbols.get(symbol as usize) {
            Some(it) if it.kind == SYM_FUNCTION => {}
            _ => return Err(format!("the init function {symbol} is no function's symbol")),
        }
        sections.push(Section {
            name: format!(".init_array.{priority}"),
            what: SectionKind::Alloc(Class::InitArray, priority),
            align: 4,
            data: Data::Bytes(vec![0; 4]),
            fixups: vec![NativeFixup {
                at: 0,
                apply: Apply::Native(reloc::TABLE_INDEX_I32),
                symbol,
                addend: 0,
                minus: None,
            }],
            retain: false,
        });
    }

    let mut groups = Vec::new();
    for (name, entries) in &linking.comdats {
        let mut members = Vec::new();
        for &(kind, index) in entries {
            members.push(match kind {
                COMDAT_DATA => *segment_sections
                    .get(index as usize)
                    .ok_or_else(|| format!("the COMDAT `{name}` has no segment {index}"))?,
                COMDAT_FUNCTION => {
                    function_section(index).map_err(|e| format!("the COMDAT `{name}`: {e}"))?
                }
                COMDAT_SECTION => *custom_sections
                    .get(&(index as usize))
                    .ok_or_else(|| format!("the COMDAT `{name}` has no custom section {index}"))?,
                other => return Err(format!("the COMDAT `{name}` holds a thing of kind {other}")),
            });
        }
        groups.push(Group { name: (*name).to_owned(), sections: members });
    }

    for section in retain {
        sections[section].retain = true;
    }
    for fixup in sections.iter().flat_map(|it| &it.fixups) {
        if fixup.symbol as usize >= symbols.len() {
            return Err(format!("a relocation against symbol {}, which there is not", fixup.symbol));
        }
    }
    Ok(Object { sections, symbols, groups, libraries: Vec::new() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wasm::leb::{custom, name, section, sleb, uleb};

    const SP: u32 = 3;

    fn vec_of(count: usize, entries: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        uleb(&mut out, count as u64);
        out.extend(entries);
        out
    }

    fn subsection(out: &mut Vec<u8>, id: u8, contents: &[u8]) {
        out.push(id);
        uleb(out, contents.len() as u64);
        out.extend(contents);
    }

    /// The body of `_start`: the places of its relocations, from the start of the body.
    const START_BODY: [u8; 38] = [
        0x00, // no locals
        0x23, 0x80, 0x80, 0x80, 0x80, 0x00, // global.get __stack_pointer (2)
        0x1a, // drop
        0x41, 0x80, 0x80, 0x80, 0x80, 0x00, // i32.const msg+2 (9)
        0x41, 0x80, 0x80, 0x80, 0x80, 0x00, // i32.const slot of init (15)
        0x11, 0x80, 0x80, 0x80, 0x80, 0x00, // call_indirect type (21)
        0x80, 0x80, 0x80, 0x80, 0x00, // table number (26)
        0x10, 0x80, 0x80, 0x80, 0x80, 0x00, // call fd_write (32)
        0x0b,
    ];

    /// Relocations: type, offset in the section, symbol or type, addend.
    fn reloc_section(out: &mut Vec<u8>, name_: &str, section_index: u32, relocs: &[(u8, u32, u32, i64)]) {
        let mut contents = Vec::new();
        uleb(&mut contents, section_index.into());
        uleb(&mut contents, relocs.len() as u64);
        for &(r_type, offset, index, addend) in relocs {
            contents.push(r_type);
            uleb(&mut contents, offset.into());
            uleb(&mut contents, index.into());
            if reloc::has_addend(r_type.into()) {
                sleb(&mut contents, addend);
            }
        }
        custom(out, name_, &contents);
    }

    /// A wasm object as clang writes one, with every kind of thing the reader takes.
    fn object_bytes(code_relocs: &[(u8, u32, u32, i64)], extra_symbol: Option<&[u8]>) -> Vec<u8> {
        let mut out = reader::MAGIC.to_vec();
        // Types: 0 is (i32 i32 i32 i32) -> i32, 1 is () -> (), 2 is (i32) -> ().
        section(&mut out, 1, &vec_of(3, &[0x60, 4, 0x7f, 0x7f, 0x7f, 0x7f, 1, 0x7f, 0x60, 0, 0, 0x60, 1, 0x7f, 0]));
        let mut imports = Vec::new();
        name(&mut imports, "env");
        name(&mut imports, "__linear_memory");
        imports.extend([2, 0, 1]);
        name(&mut imports, "env");
        name(&mut imports, "__stack_pointer");
        imports.extend([3, 0x7f, 1]);
        name(&mut imports, "wasi_snapshot_preview1");
        name(&mut imports, "fd_write");
        imports.extend([0, 0]);
        name(&mut imports, "env");
        name(&mut imports, "__indirect_function_table");
        imports.extend([1, 0x70, 0, 0]);
        section(&mut out, 2, &vec_of(4, &imports));
        section(&mut out, 3, &vec_of(2, &[1, 1]));
        let mut exports = Vec::new();
        name(&mut exports, "_start");
        exports.extend([0, 1]);
        section(&mut out, 7, &vec_of(1, &exports));
        // Code: the count, then each body's size and the body; `_start` starts at 2.
        let mut code = vec![2, START_BODY.len() as u8];
        code.extend(START_BODY);
        code.extend([2, 0x00, 0x0b]);
        section(&mut out, 10, &code);
        // Data: four segments, whose bytes are at 6, 15, 28 and 37 of the section.
        let mut data = vec![4];
        data.extend([0, 0x41, 0, 0x0b, 4]);
        data.extend(b"hi!\0");
        data.extend([0, 0x41, 4, 0x0b, 8]);
        data.extend([0; 8]);
        data.extend([0, 0x41, 12, 0x0b, 4]);
        data.extend([0; 4]);
        data.extend([0, 0x41, 16, 0x0b, 4]);
        data.extend([9; 4]);
        section(&mut out, 11, &data);
        let mut producers = vec![1];
        name(&mut producers, "language");
        producers.push(1);
        name(&mut producers, "C");
        name(&mut producers, "");
        custom(&mut out, "producers", &producers);
        custom(&mut out, ".debug_info", &[1, 2, 3, 4]);

        let mut linking = Vec::new();
        uleb(&mut linking, LINKING_VERSION.into());
        let mut symbols = Vec::new();
        let mut count = 0;
        let mut sym = |bytes: &[u8]| {
            symbols.extend(bytes);
            count += 1;
        };
        let mut entry = vec![SYM_FUNCTION, EXPORTED as u8, 1];
        name(&mut entry, "_start");
        sym(&entry);
        let mut entry = vec![SYM_FUNCTION, BINDING_LOCAL as u8, 2];
        name(&mut entry, "init");
        sym(&entry);
        let mut entry = vec![SYM_FUNCTION, (UNDEFINED | EXPLICIT_NAME) as u8, 0];
        name(&mut entry, "__imported_wasi_snapshot_preview1_fd_write");
        sym(&entry);
        sym(&[SYM_GLOBAL, UNDEFINED as u8, 0]);
        let mut entry = vec![SYM_DATA, 0];
        name(&mut entry, "msg");
        entry.extend([0, 0, 4]);
        sym(&entry);
        let mut entry = vec![SYM_DATA, VISIBILITY_HIDDEN as u8];
        name(&mut entry, "buf");
        entry.extend([1, 0, 8]);
        sym(&entry);
        let mut entry = vec![SYM_DATA, 0];
        name(&mut entry, "ptr");
        entry.extend([2, 0, 4]);
        sym(&entry);
        let mut entry = vec![SYM_DATA, UNDEFINED as u8];
        name(&mut entry, "extern_data");
        sym(&entry);
        sym(&[SYM_TABLE, UNDEFINED as u8, 0]);
        sym(&[SYM_SECTION, BINDING_LOCAL as u8, 7]);
        let mut entry = vec![SYM_DATA, 0x80, 0x01];
        name(&mut entry, "items");
        entry.extend([3, 0, 4]);
        sym(&entry);
        if let Some(extra) = extra_symbol {
            sym(extra);
        }
        let mut segments = Vec::new();
        for (segment, align) in [(".rodata.msg", 0), (".bss.buf", 3), (".data.ptr", 2), ("my_items", 2)] {
            name(&mut segments, segment);
            segments.extend([align, 0]);
        }
        subsection(&mut linking, SEGMENT_INFO, &vec_of(4, &segments));
        subsection(&mut linking, INIT_FUNCS, &vec_of(1, &[101, 1]));
        let mut comdat = Vec::new();
        name(&mut comdat, "grp");
        comdat.extend([0, 2, COMDAT_FUNCTION, 2, COMDAT_DATA, 2]);
        subsection(&mut linking, COMDAT_INFO, &vec_of(1, &comdat));
        subsection(&mut linking, SYMBOL_TABLE, &vec_of(count, &symbols));
        custom(&mut out, "linking", &linking);

        reloc_section(&mut out, "reloc.CODE", 4, code_relocs);
        reloc_section(&mut out, "reloc.DATA", 5, &[(2, 28, 1, 0)]);
        // `.debug_info`'s contents start with its name: 12 bytes.
        reloc_section(&mut out, "reloc..debug_info", 7, &[(9, 12, 9, 0)]);
        out
    }

    fn code_relocs() -> Vec<(u8, u32, u32, i64)> {
        vec![(7, 4, SP, 0), (4, 11, 4, 2), (1, 17, 1, 0), (6, 23, 2, 0), (20, 28, 8, 0), (0, 34, 2, 0)]
    }

    fn read_object(bytes: &[u8]) -> Result<Object, String> {
        match read("crt1.o", bytes, &NativeKind::Object)? {
            NativeFile::Object(object) => Ok(object),
            _ => panic!("not an object"),
        }
    }

    #[test]
    fn reads_an_object() {
        let object = read_object(&object_bytes(&code_relocs(), None)).unwrap();
        let names: Vec<&str> = object.sections.iter().map(|it| it.name.as_str()).collect();
        assert_eq!(
            names,
            [
                ".text._start",
                ".text.init",
                ".rodata.msg",
                ".bss.buf",
                ".data.ptr",
                "my_items",
                "producers",
                ".debug_info",
                ".init_array.101",
            ]
        );
        let kinds: Vec<&SectionKind> = object.sections.iter().map(|it| &it.what).collect();
        assert_eq!(kinds[0], &SectionKind::Alloc(Class::Text, 0));
        assert_eq!(kinds[2], &SectionKind::Alloc(Class::Rodata, 0));
        assert_eq!(kinds[3], &SectionKind::Alloc(Class::Bss, 0));
        assert_eq!(kinds[4], &SectionKind::Alloc(Class::Data, 0));
        assert_eq!(kinds[5], &SectionKind::Alloc(crate::wasm::named_data("my_items"), 0));
        assert_eq!(kinds[6], &SectionKind::Alloc(crate::wasm::custom("producers"), 0));
        assert_eq!(kinds[7], &SectionKind::Dropped);
        assert_eq!(kinds[8], &SectionKind::Alloc(Class::InitArray, 101));
        assert_eq!(object.sections[3].data, Data::Zero(8));
        assert_eq!(object.sections[3].align, 8);
        assert_eq!(object.sections[2].data, Data::Bytes(b"hi!\0".to_vec()));
        let retained: Vec<bool> = object.sections.iter().map(|it| it.retain).collect();
        assert_eq!(retained, [true, false, false, false, false, true, true, false, false]);

        // `_start` is in the form codegen gives functions: header, then body.
        let Data::Bytes(start) = &object.sections[0].data else { panic!("no bytes") };
        let (code, body) = Code::decode(start).unwrap();
        assert_eq!(&start[body..], &START_BODY);
        assert_eq!(code.ty, Signature::default());
        assert_eq!(code.types, [Signature { params: vec![ValType::I32], results: vec![] }]);
        assert_eq!(code.type_fixups, [(21, 0)]);
        assert_eq!(code.export.as_deref(), Some("_start"));
        let fixups: Vec<(u64, Apply, u32, i64)> = object.sections[0]
            .fixups
            .iter()
            .map(|it| (it.at - body as u64, it.apply, it.symbol, it.addend))
            .collect();
        assert_eq!(
            fixups,
            [
                (2, Apply::Native(reloc::GLOBAL_INDEX_LEB), SP, 0),
                (9, Apply::Native(reloc::MEMORY_ADDR_SLEB), 4, 2),
                (15, Apply::Native(reloc::TABLE_INDEX_SLEB), 1, 0),
                (26, Apply::Native(reloc::TABLE_NUMBER_LEB), 8, 0),
                (32, Apply::Native(reloc::FUNCTION_INDEX_LEB), 2, 0),
            ]
        );
        assert_eq!(
            object.sections[4].fixups,
            [NativeFixup {
                at: 0,
                apply: Apply::Native(reloc::TABLE_INDEX_I32),
                symbol: 1,
                addend: 0,
                minus: None
            }]
        );
        assert_eq!(object.sections[8].fixups[0].symbol, 1);
        assert_eq!(object.sections[7].fixups[0].apply, Apply::Native(reloc::SECTION_OFFSET_I32));

        let symbol = |name: &str| object.symbols.iter().find(|it| it.name == name).unwrap();
        assert_eq!(object.symbols.len(), 11);
        assert_eq!(
            symbol("_start").def,
            SymbolDef::Section { section: 0, value: 0, size: start.len() as u64 }
        );
        assert_eq!(symbol("init").binding, Binding::Local);
        let fd_write = symbol("__imported_wasi_snapshot_preview1_fd_write");
        assert_eq!(fd_write.def, SymbolDef::Undefined);
        let import = fd_write.import.as_ref().unwrap();
        assert_eq!((import.module.as_str(), import.field.as_str()), ("wasi_snapshot_preview1", "fd_write"));
        assert_eq!(import.ty.params.len(), 4);
        assert_eq!(import.ty.results, [ValType::I32]);
        assert_eq!(object.symbols[SP as usize].name, "__stack_pointer");
        assert_eq!(object.symbols[SP as usize].kind, SymbolKind::Other);
        assert_eq!(object.symbols[8].name, "__indirect_function_table");
        assert_eq!(symbol("msg").def, SymbolDef::Section { section: 2, value: 0, size: 4 });
        assert!(symbol("buf").hidden);
        assert_eq!(symbol("extern_data").def, SymbolDef::Undefined);
        assert_eq!(object.symbols[9].kind, SymbolKind::Section);
        assert_eq!(object.symbols[9].def, SymbolDef::Section { section: 7, value: 0, size: 0 });
        assert_eq!(object.groups, [Group { name: "grp".to_owned(), sections: vec![1, 4] }]);
    }

    #[test]
    fn refuses_what_it_does_not_know() {
        let mut relocs = code_relocs();
        relocs[1].0 = 99;
        let error = read_object(&object_bytes(&relocs, None)).unwrap_err();
        assert!(error.contains("crt1.o") && error.contains("99"), "{error}");

        let mut relocs = code_relocs();
        relocs[1].0 = 21;
        let error = read_object(&object_bytes(&relocs, None)).unwrap_err();
        assert!(error.contains("R_WASM_MEMORY_ADDR_TLS_SLEB"), "{error}");

        let mut relocs = code_relocs();
        relocs[0].1 = 1;
        let error = read_object(&object_bytes(&relocs, None)).unwrap_err();
        assert!(error.contains("no function body"), "{error}");

        let error = read_object(&object_bytes(&code_relocs(), Some(&[SYM_TAG, 0, 0, 1, b't'])))
            .unwrap_err();
        assert!(error.contains("exception tag"), "{error}");

        let error = read_object(&object_bytes(&code_relocs(), Some(&[9, 0]))).unwrap_err();
        assert!(error.contains("unknown kind 9"), "{error}");

        let mut linked = reader::MAGIC.to_vec();
        section(&mut linked, 1, &[0]);
        let error = read_object(&linked).unwrap_err();
        assert!(error.contains("no linking section"), "{error}");

        assert!(read("libc.so", &linked, &NativeKind::Shared { as_needed: true }).is_err());
    }

    fn header(name: &str, size: usize) -> Vec<u8> {
        let mut out = format!("{name:<16}{:<12}{:<6}{:<6}{:<8}{size:<10}", 0, 0, 0, 644).into_bytes();
        out.extend(b"`\n");
        assert_eq!(out.len(), 60);
        out
    }

    fn archive_bytes(index: bool) -> Vec<u8> {
        let member = reader::MAGIC.to_vec();
        let names = b"_start\0helper\0";
        let table_len = 4 + 4 * 2 + names.len();
        let first = 8 + if index { 60 + table_len } else { 0 };
        let second = first + 60 + member.len();
        let mut out = b"!<arch>\n".to_vec();
        if index {
            out.extend(header("/", table_len));
            out.extend(2u32.to_be_bytes());
            out.extend((first as u32).to_be_bytes());
            out.extend((second as u32).to_be_bytes());
            out.extend(names);
        }
        out.extend(header("a.o/", member.len()));
        out.extend(&member);
        out.extend(header("b.o/", member.len()));
        out.extend(&member);
        out
    }

    #[test]
    fn reads_an_archive() {
        let bytes = archive_bytes(true);
        let NativeFile::Archive(archive) =
            read("libc.a", &bytes, &NativeKind::Archive { whole: false }).unwrap()
        else {
            panic!("not an archive")
        };
        let names: Vec<&str> = archive.members.iter().map(|it| it.name.as_str()).collect();
        assert_eq!(names, ["a.o", "b.o"]);
        assert_eq!(&bytes[archive.members[1].bytes.clone()], reader::MAGIC);
        assert_eq!(archive.index, [("_start".to_owned(), 0), ("helper".to_owned(), 1)]);
    }

    #[test]
    fn an_archive_without_an_index() {
        let bytes = archive_bytes(false);
        let error = read("libx.a", &bytes, &NativeKind::Archive { whole: false }).err().unwrap();
        assert!(error.contains("libx.a") && error.contains("index"), "{error}");
        assert!(read("libx.a", &bytes, &NativeKind::Archive { whole: true }).is_ok());
    }
}

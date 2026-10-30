//! The tables a PE loader reads, made from the placed image, and the headers around it.
//!
//! The import directory (descriptors, lookup tables, the hint/name table, DLL names; the
//! import address table is the core's GOT, laid out per DLL), the export directory, the debug
//! directory with its CodeView entry, the TLS directory and `_tls_index` when the CRT defines
//! none, the exception directory (`.pdata`: codegen's functions and the objects' entries,
//! sorted), the base relocations; then the DOS header and stub, the PE and optional headers
//! and the section table.

use std::collections::{BTreeMap, BTreeSet};

use object::pe;

use super::coff::ImportName;
use super::{
    DEBUG, EDATA, IDATA, OBJ_PDATA, PAGE, PDATA, Pe, RELOC, RSRC, Segment, TLS_DIR, TLS_INDEX, error, place,
    zeroed,
};
use crate::model::{Class, Counts, Export, File, Holds, Placed, Prot, Region, SymbolKind};
use crate::{LinkError, OutputKind};

/// Room for the image's file name, which the export directory and the CodeView entry hold
/// and which is not known when the synthetic regions are sized.
const NAME_ROOM: u64 = 260;
const DESCRIPTOR: u64 = 20;
const EXPORT_DIRECTORY: u64 = 40;
const TLS_DIRECTORY: u64 = 40;
const DEBUG_DIRECTORY: u64 = 28;
/// `RSDS`, the GUID and the age, before the PDB's name.
const CODEVIEW: u64 = 24;
const RUNTIME_FUNCTION: u64 = 12;
/// Where the PE signature is: after the DOS header and its program.
const NT: usize = 0x80;
const FILE_HEADER: usize = 20;
const OPTIONAL: usize = 240;
const SECTION_HEADER: usize = 40;
const DIRECTORIES: usize = 16;
/// link.exe's defaults: `/STACK` and `/HEAP` reserve 1 MiB and commit a page.
const STACK_RESERVE: u64 = 0x10_0000;
const HEAP_RESERVE: u64 = 0x10_0000;
const COMMIT: u64 = 0x1000;
/// link.exe 14's version, and the Windows version its x64 images ask for (Vista, 6.0).
const LINKER_VERSION: u8 = 14;
const WINDOWS_VERSION: u16 = 6;

/// The MS-DOS program link.exe puts after the DOS header.
const DOS_PROGRAM: &[u8] =
    b"\x0e\x1f\xba\x0e\x00\xb4\x09\xcd\x21\xb8\x01\x4c\xcd\x21This program cannot be run in DOS mode.\r\r\n$";

#[derive(Default)]
pub(super) struct Buf(pub(super) Vec<u8>);

impl Buf {
    pub(super) fn u8(&mut self, value: u8) {
        self.0.push(value);
    }

    pub(super) fn u16(&mut self, value: u16) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }

    pub(super) fn u32(&mut self, value: u32) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }

    pub(super) fn u64(&mut self, value: u64) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }

    pub(super) fn bytes(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }

    /// The bytes and a NUL.
    pub(super) fn string(&mut self, text: &str) {
        self.bytes(text.as_bytes());
        self.u8(0);
    }

    pub(super) fn align(&mut self, to: usize) {
        self.0.resize(self.0.len().next_multiple_of(to), 0);
    }

    pub(super) fn len(&self) -> u64 {
        self.0.len() as u64
    }
}

fn rva(placed: &Placed<'_>, addr: u64, what: &str) -> Result<u32, LinkError> {
    addr.checked_sub(placed.plan.base)
        .and_then(|it| u32::try_from(it).ok())
        .ok_or_else(|| error(format!("{what} at {addr:#x} is outside the image")))
}

/// The time stamp of the headers and directories: from the image's ID, so the same image is
/// the same bytes (link.exe `/Brepro`).
fn stamp(placed: &Placed<'_>) -> u32 {
    let id = placed.id;
    u32::from_le_bytes([id[0], id[1], id[2], id[3]])
}

/// The bytes of the image at `addr`.
fn bytes_at<'p>(placed: &'p Placed<'_>, addr: u64, len: u64, what: &str) -> Result<&'p [u8], LinkError> {
    let index = placed.regions.partition_point(|it| it.addr <= addr);
    index
        .checked_sub(1)
        .and_then(|index| {
            let offset = usize::try_from(addr - placed.regions[index].addr).ok()?;
            placed.bytes(index).get(offset..offset.checked_add(usize::try_from(len).ok()?)?)
        })
        .ok_or_else(|| error(format!("{what} at {addr:#x} is not in the image's bytes")))
}

fn u32_of(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn u64_of(bytes: &[u8], at: usize) -> u64 {
    u64::from(u32_of(bytes, at)) | u64::from(u32_of(bytes, at + 4)) << 32
}

/// What the synthetic regions may need, from the counts alone.
pub(super) fn synthetic(counts: &Counts) -> Vec<(Class, u64)> {
    let mut regions = Vec::new();
    if counts.libraries > 0 {
        let descriptors = ((counts.libraries + 1) * DESCRIPTOR).next_multiple_of(8);
        let lookup = (counts.imports + counts.libraries) * 8;
        // A hint, the name (no longer than its `"<hint> <name>"` version), a NUL and a pad.
        let hint_names = counts.imports * 4 + counts.import_names;
        let dll_names = counts.library_names + counts.libraries;
        regions.push((Class::Synthetic(IDATA), descriptors + lookup + hint_names + dll_names));
    }
    if counts.exports > 0 {
        let tables = counts.exports * 10;
        let names = counts.export_names + counts.exports + NAME_ROOM + 1;
        regions.push((Class::Synthetic(EDATA), EXPORT_DIRECTORY + tables + names));
    }
    regions.push((Class::Synthetic(DEBUG), DEBUG_DIRECTORY + CODEVIEW + NAME_ROOM + 1));
    // The directory and a callback list holding only its terminator.
    regions.push((Class::Synthetic(TLS_DIR), TLS_DIRECTORY + 8));
    regions.push((Class::Synthetic(TLS_INDEX), 8));
    if counts.functions + counts.unwind > 0 {
        regions.push((Class::Synthetic(PDATA), counts.functions * RUNTIME_FUNCTION + counts.unwind));
    }
    // Every rebased address and the four of a TLS directory the writer makes; a block (its
    // header and a pad) per page that has one.
    let entries = counts.absolute + 4;
    let blocks = entries.min(counts.image / PAGE + 64);
    regions.push((Class::Synthetic(RELOC), entries * 2 + blocks * 10));
    regions
}

pub(super) fn synthesize(placed: &Placed<'_>) -> Result<Vec<(Class, Vec<u8>)>, LinkError> {
    Ok(Tables::of(placed)?.regions)
}

/// The synthetic regions' bytes and the data directories pointing into the image.
pub(super) struct Tables {
    pub(super) regions: Vec<(Class, Vec<u8>)>,
    /// RVA and size, by `IMAGE_DIRECTORY_ENTRY_*`.
    pub(super) directories: [(u32, u32); DIRECTORIES],
}

impl Tables {
    pub(super) fn of(placed: &Placed<'_>) -> Result<Tables, LinkError> {
        let mut tables = Tables { regions: Vec::new(), directories: [(0, 0); DIRECTORIES] };
        if let Some(bind) = placed.binds.first() {
            let name = placed.imports.get(bind.import as usize).map_or("?", |it| it.name.as_str());
            return Err(error(format!(
                "a bind of `{name}` at {:#x}: a PE loader fills only the import address table, \
                 so an import is reached through its cell or its thunk",
                bind.addr
            )));
        }
        for cell in placed.got {
            match &cell.holds {
                Holds::Local(_) | Holds::Import(_) => {}
                other => {
                    return Err(error(format!(
                        "the GOT cell at {:#x} holds {other:?}: Windows reaches thread-locals \
                         through `_tls_index` and `SECREL`, never a GOT cell",
                        cell.addr
                    )));
                }
            }
        }
        for import in placed.imports {
            if import.library.is_none() {
                return Err(error(format!(
                    "`{}` is imported from no DLL: PE has no weak or unresolved imports \
                     (link.exe: LNK2019)",
                    import.name
                )));
            }
            if import.kind == SymbolKind::Tls {
                return Err(error(format!(
                    "the thread-local `{}` is imported: a DLL exports no thread-locals",
                    import.name
                )));
            }
        }

        match placed.region(&Class::Synthetic(IDATA)) {
            Some((_, region)) => {
                let (bytes, import, iat) = imports(placed, region)?;
                tables.directories[pe::IMAGE_DIRECTORY_ENTRY_IMPORT] = import;
                tables.directories[pe::IMAGE_DIRECTORY_ENTRY_IAT] = iat;
                tables.push(placed, IDATA, bytes)?;
            }
            None if placed.got.iter().any(|it| matches!(it.holds, Holds::Import(_))) => {
                return Err(error("an image with imports and no import directory region".into()));
            }
            None => {}
        }

        match placed.region(&Class::Synthetic(EDATA)) {
            Some((_, region)) => {
                let bytes = exports(placed, region)?;
                tables.directories[pe::IMAGE_DIRECTORY_ENTRY_EXPORT] =
                    (rva(placed, region.addr, "the export directory")?, bytes.len() as u32);
                tables.push(placed, EDATA, bytes)?;
            }
            None if !placed.exports.is_empty() => {
                return Err(error("an image with exports and no export directory region".into()));
            }
            None => {}
        }

        if let Some((_, region)) = placed.region(&Class::Synthetic(DEBUG)) {
            let bytes = debug(placed, region)?;
            tables.directories[pe::IMAGE_DIRECTORY_ENTRY_DEBUG] =
                (rva(placed, region.addr, "the debug directory")?, DEBUG_DIRECTORY as u32);
            tables.push(placed, DEBUG, bytes)?;
        }

        let mut extra = Vec::new();
        tables.tls(placed, &mut extra)?;
        if placed.region(&Class::Synthetic(TLS_INDEX)).is_some() {
            tables.push(placed, TLS_INDEX, vec![0; 8])?;
        }

        match placed.region(&Class::Synthetic(PDATA)) {
            Some((_, region)) => {
                let bytes = pdata(placed)?;
                if !bytes.is_empty() {
                    tables.directories[pe::IMAGE_DIRECTORY_ENTRY_EXCEPTION] =
                        (rva(placed, region.addr, "the exception directory")?, bytes.len() as u32);
                }
                tables.push(placed, PDATA, bytes)?;
            }
            None if placed.functions.iter().any(|it| it.windows.is_some()) => {
                return Err(error("an image with unwind info and no `.pdata` region".into()));
            }
            None => {}
        }

        if let Some(addr) = (placed.symbol)("_load_config_used") {
            let size = u32_of(bytes_at(placed, addr, 4, "the load configuration `_load_config_used`")?, 0);
            tables.directories[pe::IMAGE_DIRECTORY_ENTRY_LOAD_CONFIG] =
                (rva(placed, addr, "the load configuration")?, size);
        }

        let resources = || placed.regions.iter().filter(|it| matches!(&it.class, Class::Named { name, .. } if name == RSRC));
        if let (Some(start), Some(end)) =
            (resources().map(|it| it.addr).min(), resources().map(|it| it.addr + it.size).max())
        {
            tables.directories[pe::IMAGE_DIRECTORY_ENTRY_RESOURCE] =
                (rva(placed, start, "the resource directory")?, (end - start) as u32);
        }

        match placed.region(&Class::Synthetic(RELOC)) {
            Some((_, region)) => {
                let bytes = relocations(placed, &extra)?;
                if !bytes.is_empty() {
                    tables.directories[pe::IMAGE_DIRECTORY_ENTRY_BASERELOC] =
                        (rva(placed, region.addr, "the base relocations")?, bytes.len() as u32);
                }
                tables.push(placed, RELOC, bytes)?;
            }
            None if !placed.rebase.is_empty() || !extra.is_empty() => {
                return Err(error("an image with rebased addresses and no `.reloc` region".into()));
            }
            None => {}
        }
        Ok(tables)
    }

    fn push(&mut self, placed: &Placed<'_>, name: &'static str, bytes: Vec<u8>) -> Result<(), LinkError> {
        let class = Class::Synthetic(name);
        let (_, region) =
            placed.region(&class).ok_or_else(|| error(format!("the {name} region was not placed")))?;
        if !region.addr.is_multiple_of(8) {
            return Err(error(format!("the {name} region at {:#x} is not 8-aligned", region.addr)));
        }
        let room = region.capacity.max(region.size);
        if bytes.len() as u64 > room {
            return Err(error(format!(
                "{name} needs {} bytes, more than the {room} of its region at {:#x}",
                bytes.len(),
                region.addr
            )));
        }
        self.regions.push((class, bytes));
        Ok(())
    }

    /// The TLS directory: the CRT's `_tls_used` when it defines one (the template must be
    /// the range it states), else the writer's own for an image with thread-locals.
    fn tls(&mut self, placed: &Placed<'_>, extra: &mut Vec<u64>) -> Result<(), LinkError> {
        let ours = placed.region(&Class::Synthetic(TLS_DIR)).map(|(_, it)| it.addr);
        let used = (placed.symbol)("_tls_used");
        let mut bytes = Buf::default();
        match used {
            Some(addr) if Some(addr) != ours => {
                let directory = bytes_at(placed, addr, TLS_DIRECTORY, "the CRT's TLS directory `_tls_used`")?;
                let start = u64_of(directory, 0);
                let end = u64_of(directory, 8);
                let zero_fill = u64::from(u32_of(directory, 32));
                if let Some(tls) = placed.tls
                    && (tls.addr != start || tls.addr + tls.size > end + zero_fill)
                {
                    return Err(error(format!(
                        "the thread-local template {:#x}..{:#x} is not the range {start:#x}..{:#x} \
                         the CRT's `_tls_used` states (between `_tls_start` and `_tls_end`)",
                        tls.addr,
                        tls.addr + tls.size,
                        end + zero_fill
                    )));
                }
                self.directories[pe::IMAGE_DIRECTORY_ENTRY_TLS] =
                    (rva(placed, addr, "`_tls_used`")?, TLS_DIRECTORY as u32);
            }
            _ if used.is_some() || placed.tls.is_some() => {
                let addr = ours.ok_or_else(|| error("an image with thread-locals and no TLS directory region".into()))?;
                let index = (placed.symbol)("_tls_index")
                    .or_else(|| placed.region(&Class::Synthetic(TLS_INDEX)).map(|(_, it)| it.addr))
                    .ok_or_else(|| error("an image with thread-locals and no `_tls_index`".into()))?;
                let callbacks = (placed.symbol)("__xl_a").map_or(addr + TLS_DIRECTORY, |it| it + 8);
                let (start, end, zero_fill, align) = match placed.tls {
                    Some(tls) => (tls.addr, tls.addr + tls.init, tls.size - tls.init, tls.align),
                    None => (0, 0, 0, 1),
                };
                let zero_fill = u32::try_from(zero_fill)
                    .map_err(|_| error(format!("{zero_fill:#x} bytes of zeroed thread-locals")))?;
                bytes.u64(start);
                bytes.u64(end);
                bytes.u64(index);
                bytes.u64(callbacks);
                bytes.u32(zero_fill);
                bytes.u32(alignment(align).map_err(error)?);
                bytes.u64(0);
                if start != 0 {
                    extra.extend([addr, addr + 8]);
                }
                extra.extend([addr + 16, addr + 24]);
                self.directories[pe::IMAGE_DIRECTORY_ENTRY_TLS] =
                    (rva(placed, addr, "the TLS directory")?, TLS_DIRECTORY as u32);
            }
            _ => {}
        }
        if ours.is_some() {
            self.push(placed, TLS_DIR, bytes.0)?;
        }
        Ok(())
    }
}

/// `IMAGE_SCN_ALIGN_*` of an alignment.
pub(super) fn alignment(align: u64) -> Result<u32, String> {
    let align = align.max(1);
    if !align.is_power_of_two() || align > 8192 {
        return Err(format!("an alignment of {align}, which a PE section can't state"));
    }
    Ok((align.trailing_zeros() + 1) << 20)
}

/// The import directory, the lookup tables, the hint/name table and the DLL names, and the
/// import and IAT directories. Each DLL's IAT cells are the core's GOT cells of its imports,
/// consecutive and followed by a zero cell.
fn imports(placed: &Placed<'_>, region: &Region) -> Result<(Vec<u8>, (u32, u32), (u32, u32)), LinkError> {
    let taken: BTreeSet<u64> = placed.got.iter().map(|it| it.addr).collect();
    let mut groups: BTreeMap<u32, Vec<(u64, u32)>> = BTreeMap::new();
    for cell in placed.got {
        let Holds::Import(import) = cell.holds else { continue };
        let it = placed.imports.get(import as usize).ok_or_else(|| {
            error(format!("the GOT cell at {:#x} holds import {import}, which there isn't", cell.addr))
        })?;
        let library = it.library.ok_or_else(|| error(format!("`{}` is imported from no DLL", it.name)))?;
        if placed.libraries.get(library as usize).is_none() {
            return Err(error(format!("`{}` is imported from library {library}, which there isn't", it.name)));
        }
        groups.entry(library).or_default().push((cell.addr, import));
    }
    let mut groups: Vec<(u32, Vec<(u64, u32)>)> = groups.into_iter().collect();
    for (library, cells) in &mut groups {
        cells.sort_unstable();
        let dll = &placed.libraries[*library as usize].name;
        for pair in cells.windows(2) {
            if pair[1].0 != pair[0].0 + 8 {
                return Err(error(format!(
                    "the import address table of {dll} is not consecutive: cells at {:#x} and {:#x}",
                    pair[0].0, pair[1].0
                )));
            }
        }
        let after = cells[cells.len() - 1].0 + 8;
        if taken.contains(&after) {
            return Err(error(format!(
                "the import address table of {dll} has no zero cell after it: {after:#x} is a GOT cell"
            )));
        }
    }
    if groups.is_empty() {
        return Ok((Vec::new(), (0, 0), (0, 0)));
    }

    let base = region.addr;
    let descriptors = ((groups.len() as u64 + 1) * DESCRIPTOR).next_multiple_of(8);
    let lookup: u64 = groups.iter().map(|(_, cells)| (cells.len() as u64 + 1) * 8).sum();
    let hint_start = descriptors + lookup;
    let mut hints = Buf::default();
    let mut thunks = Vec::with_capacity(groups.len());
    for (_, cells) in &groups {
        let mut list = Vec::with_capacity(cells.len());
        for &(_, import) in cells {
            let it = &placed.imports[import as usize];
            match ImportName::parse(&it.name, it.version.as_deref()).map_err(error)? {
                ImportName::Ordinal(ordinal) => list.push(pe::IMAGE_ORDINAL_FLAG64 | u64::from(ordinal)),
                ImportName::Name { hint, name } => {
                    let at = rva(placed, base + hint_start + hints.len(), "a hint/name entry")?;
                    list.push(u64::from(at));
                    hints.u16(hint);
                    hints.string(&name);
                    hints.align(2);
                }
            }
        }
        thunks.push(list);
    }
    let names_start = hint_start + hints.len();
    let mut names = Buf::default();
    let mut name_at = Vec::with_capacity(groups.len());
    for (library, _) in &groups {
        name_at.push(names_start + names.len());
        names.string(&placed.libraries[*library as usize].name);
    }

    let mut out = Buf::default();
    let mut lookup_at = descriptors;
    for ((_, cells), name) in groups.iter().zip(&name_at) {
        out.u32(rva(placed, base + lookup_at, "an import lookup table")?);
        out.u32(0);
        out.u32(0);
        out.u32(rva(placed, base + name, "a DLL name")?);
        out.u32(rva(placed, cells[0].0, "an import address table")?);
        lookup_at += (cells.len() as u64 + 1) * 8;
    }
    out.bytes(&[0; DESCRIPTOR as usize]);
    out.align(8);
    for list in thunks {
        for thunk in list {
            out.u64(thunk);
        }
        out.u64(0);
    }
    out.bytes(&hints.0);
    out.bytes(&names.0);

    let import = (rva(placed, base, "the import directory")?, ((groups.len() as u64 + 1) * DESCRIPTOR) as u32);
    let first = groups.iter().map(|(_, cells)| cells[0].0).min().unwrap_or(0);
    let last = groups.iter().map(|(_, cells)| cells[cells.len() - 1].0).max().unwrap_or(0);
    let iat = (rva(placed, first, "the import address table")?, (last + 16 - first) as u32);
    Ok((out.0, import, iat))
}

/// The export directory: every export by name, the names sorted (the loader searches them
/// by bisection), ordinals from 1 in that order.
fn exports(placed: &Placed<'_>, region: &Region) -> Result<Vec<u8>, LinkError> {
    let mut exports: Vec<&Export> = placed.exports.iter().collect();
    exports.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
    for pair in exports.windows(2) {
        if pair[0].name == pair[1].name {
            return Err(error(format!("`{}` is exported twice", pair[0].name)));
        }
    }
    let count = u16::try_from(exports.len())
        .map_err(|_| error(format!("{} exports, more than a PE export table's 65535", exports.len())))?;
    let n = u64::from(count);
    let base = region.addr;
    let functions = EXPORT_DIRECTORY;
    let name_pointers = functions + 4 * n;
    let ordinals = name_pointers + 4 * n;
    let strings_start = ordinals + 2 * n;
    let mut strings = Buf::default();
    let mut name_at = Vec::with_capacity(exports.len());
    for export in &exports {
        if export.name.is_empty() {
            return Err(error(format!("an export with no name at {:#x}", export.addr)));
        }
        name_at.push(strings_start + strings.len());
        strings.string(&export.name);
    }
    let image_name = strings_start + strings.len();
    strings.string(placed.name);

    let mut out = Buf::default();
    out.u32(0);
    out.u32(stamp(placed));
    out.u16(0);
    out.u16(0);
    out.u32(rva(placed, base + image_name, "the export directory's name")?);
    out.u32(1);
    out.u32(count.into());
    out.u32(count.into());
    out.u32(rva(placed, base + functions, "the export address table")?);
    out.u32(rva(placed, base + name_pointers, "the export name table")?);
    out.u32(rva(placed, base + ordinals, "the export ordinal table")?);
    for export in &exports {
        if export.kind == SymbolKind::Tls {
            return Err(error(format!("the thread-local `{}` is exported: a DLL exports no thread-locals", export.name)));
        }
        out.u32(rva(placed, export.addr, &format!("the export `{}`", export.name))?);
    }
    for at in &name_at {
        out.u32(rva(placed, base + at, "an export name")?);
    }
    for ordinal in 0..count {
        out.u16(ordinal);
    }
    out.bytes(&strings.0);
    Ok(out.0)
}

/// The PDB a debugger looks for: the image's name with `.pdb` for its extension.
pub(super) fn pdb_name(name: &str) -> String {
    let stem = match name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => name,
    };
    format!("{stem}.pdb")
}

/// The debug directory: one CodeView entry naming the PDB by the image's ID. No PDB is
/// written.
fn debug(placed: &Placed<'_>, region: &Region) -> Result<Vec<u8>, LinkError> {
    let pdb = pdb_name(placed.name);
    let data = region.addr + DEBUG_DIRECTORY;
    let size = CODEVIEW as u32 + pdb.len() as u32 + 1;
    let at = rva(placed, data, "the CodeView entry")?;
    let mut out = Buf::default();
    out.u32(0);
    out.u32(stamp(placed));
    out.u16(0);
    out.u16(0);
    out.u32(pe::IMAGE_DEBUG_TYPE_CODEVIEW);
    out.u32(size);
    out.u32(at);
    // A region's file offset is its RVA.
    out.u32(at);
    out.bytes(b"RSDS");
    out.bytes(&placed.id);
    out.u32(1);
    out.string(&pdb);
    Ok(out.0)
}

/// `RUNTIME_FUNCTION`s: one per function codegen gave Windows unwind info, and the native
/// objects' `.pdata` entries (those of dropped functions read 0), sorted by start.
fn pdata(placed: &Placed<'_>) -> Result<Vec<u8>, LinkError> {
    let mut entries = Vec::new();
    for function in placed.functions {
        let Some(windows) = function.windows else { continue };
        let what = function.name.as_deref().unwrap_or("a function");
        entries.push((
            rva(placed, function.addr, what)?,
            rva(placed, function.addr + function.size, what)?,
            rva(placed, windows.info, &format!("the unwind info of {what}"))?,
        ));
    }
    for (index, region) in placed.regions.iter().enumerate() {
        if !matches!(&region.class, Class::Named { name, .. } if name == OBJ_PDATA) {
            continue;
        }
        let bytes = placed.bytes(index);
        if !bytes.len().is_multiple_of(RUNTIME_FUNCTION as usize) {
            return Err(error(format!(
                "the objects' .pdata at {:#x} is {} bytes, not whole RUNTIME_FUNCTIONs",
                region.addr,
                bytes.len()
            )));
        }
        for entry in bytes.chunks_exact(RUNTIME_FUNCTION as usize) {
            let begin = u32_of(entry, 0);
            if begin != 0 {
                entries.push((begin, u32_of(entry, 4), u32_of(entry, 8)));
            }
        }
    }
    entries.sort_unstable();
    for pair in entries.windows(2) {
        if pair[1].0 < pair[0].1 {
            return Err(error(format!(
                "the unwind entries of RVAs {:#x}..{:#x} and {:#x}..{:#x} overlap",
                pair[0].0, pair[0].1, pair[1].0, pair[1].1
            )));
        }
    }
    let mut out = Buf::default();
    for (begin, end, info) in entries {
        out.u32(begin);
        out.u32(end);
        out.u32(info);
    }
    Ok(out.0)
}

/// The base relocations: a block per 4 KiB page, a `DIR64` entry per rebased address in it,
/// padded to 4 bytes.
fn relocations(placed: &Placed<'_>, extra: &[u64]) -> Result<Vec<u8>, LinkError> {
    let mut at: Vec<u64> = placed.rebase.iter().chain(extra).copied().collect();
    at.sort_unstable();
    at.dedup();
    for &addr in &at {
        let index = placed.regions.partition_point(|it| it.addr <= addr);
        let region = index
            .checked_sub(1)
            .map(|it| &placed.regions[it])
            .filter(|it| addr + 8 <= it.addr + it.capacity.max(it.size));
        match region {
            Some(region) if zeroed(&region.class) => {
                return Err(error(format!(
                    "a rebased address at {addr:#x} in {:?}, which is not in the file",
                    region.class
                )));
            }
            Some(_) => {}
            None => return Err(error(format!("a rebased address at {addr:#x} in no region"))),
        }
    }
    let mut out = Buf::default();
    let mut next = 0;
    while next < at.len() {
        let page = at[next] & !(PAGE - 1);
        let block = out.0.len();
        out.u32(rva(placed, page, "a rebased page")?);
        out.u32(0);
        while next < at.len() && at[next] & !(PAGE - 1) == page {
            out.u16((pe::IMAGE_REL_BASED_DIR64 << 12) | (at[next] & (PAGE - 1)) as u16);
            next += 1;
        }
        out.align(4);
        let size = (out.0.len() - block) as u32;
        out.0[block + 4..block + 8].copy_from_slice(&size.to_le_bytes());
    }
    Ok(out.0)
}

/// One section of the image: a run of regions of one protection.
pub(super) struct Out {
    pub(super) name: [u8; 8],
    pub(super) start: u64,
    pub(super) end: u64,
    /// The end of its bytes in the file: zeroed regions at its end are not.
    pub(super) file_end: u64,
    pub(super) prot: Prot,
    pub(super) discardable: bool,
}

impl Out {
    fn raw(&self) -> u64 {
        if self.file_end > self.start { (self.file_end - self.start).next_multiple_of(PAGE) } else { 0 }
    }

    fn characteristics(&self) -> u32 {
        let mut flags = if self.prot.exec {
            pe::IMAGE_SCN_CNT_CODE | pe::IMAGE_SCN_MEM_EXECUTE
        } else if self.raw() > 0 {
            pe::IMAGE_SCN_CNT_INITIALIZED_DATA
        } else {
            pe::IMAGE_SCN_CNT_UNINITIALIZED_DATA
        };
        if self.prot.read {
            flags |= pe::IMAGE_SCN_MEM_READ;
        }
        if self.prot.write {
            flags |= pe::IMAGE_SCN_MEM_WRITE;
        }
        if self.discardable {
            flags |= pe::IMAGE_SCN_MEM_DISCARDABLE;
        }
        flags
    }
}

fn default_prot(segment: Segment) -> Prot {
    match segment {
        Segment::Text => Prot::RX,
        Segment::Data | Segment::Tls => Prot::RW,
        Segment::Rdata | Segment::Pdata | Segment::Rsrc | Segment::Reloc => Prot::R,
    }
}

fn section_name(class: &Class, segment: Segment, prot: Prot) -> [u8; 8] {
    let name = match class {
        Class::Named { name, .. } if prot != default_prot(segment) => name.as_bytes(),
        _ => segment.name(),
    };
    let mut out = [0; 8];
    for (to, byte) in out.iter_mut().zip(name) {
        *to = *byte;
    }
    out
}

/// The image's sections: a new one at each page-aligned change of section or protection. A
/// section whose regions the core placed right after another's of the same protection (no
/// page boundary between them, as when `.tls` is absent and `.reloc` follows `.pdata`)
/// joins it: the loader maps both alike, and the data directories find the tables in it.
pub(super) fn sections(placed: &Placed<'_>) -> Result<Vec<Out>, LinkError> {
    let mut sections: Vec<(Segment, Out)> = Vec::new();
    for region in placed.regions {
        if region.class == Class::ThreadVars {
            return Err(error(format!(
                "Mach-O thread-local descriptors at {:#x} in a PE image",
                region.addr
            )));
        }
        let end = region.addr + region.capacity.max(region.size);
        if end == region.addr {
            continue;
        }
        let segment = place(&region.class).0;
        let in_file = !zeroed(&region.class);
        let aligned = region.addr.is_multiple_of(PAGE);
        let extend = match sections.last() {
            Some((_, last)) if region.addr < last.end => {
                return Err(error(format!(
                    "{:?} at {:#x} overlaps the section before it, which ends at {:#x}",
                    region.class, region.addr, last.end
                )));
            }
            Some((last_segment, last)) => last.prot == region.prot && (*last_segment == segment || !aligned),
            None => false,
        };
        if let Some((_, last)) = sections.last_mut().filter(|_| extend) {
            last.end = end;
            if in_file {
                last.file_end = end;
            }
            continue;
        }
        if !aligned {
            return Err(error(format!(
                "{:?} at {:#x} begins a section off a {PAGE:#x} page boundary",
                region.class, region.addr
            )));
        }
        sections.push((
            segment,
            Out {
                name: section_name(&region.class, segment, region.prot),
                start: region.addr,
                end,
                file_end: if in_file { end } else { region.addr },
                prot: region.prot,
                discardable: segment == Segment::Reloc,
            },
        ));
    }
    match sections.first() {
        None => return Err(error("an image with no regions".into())),
        Some((_, first)) if first.start < placed.plan.base + PAGE => {
            return Err(error(format!("the first section at {:#x} is inside the headers", first.start)));
        }
        Some(_) => {}
    }
    Ok(sections.into_iter().map(|(_, it)| it).collect())
}

/// The headers: the DOS header and program, the PE header, the optional header with the data
/// directories, the section table.
pub(super) fn file(pe: &Pe, placed: &Placed<'_>) -> Result<File, LinkError> {
    let tables = Tables::of(placed)?;
    let sections = sections(placed)?;
    let base = placed.plan.base;
    let headers = NT + 4 + FILE_HEADER + OPTIONAL + SECTION_HEADER * sections.len();
    if headers as u64 > PAGE {
        return Err(error(format!("{} sections, more than the headers' page has room for", sections.len())));
    }
    let last = sections.last().map_or(base + PAGE, |it| it.end);
    let size_of_image = u32::try_from((last - base).next_multiple_of(PAGE))
        .map_err(|_| error(format!("an image of {:#x} bytes, more than PE's 4 GiB", last - base)))?;
    let entry = match &pe.output.kind {
        OutputKind::Executable { entry } => {
            let addr = placed.entry.ok_or_else(|| error(format!("the executable's entry `{entry}` is not placed")))?;
            rva(placed, addr, &format!("the entry `{entry}`"))?
        }
        OutputKind::Shared { .. } => match placed.entry {
            Some(addr) => rva(placed, addr, "the DLL's entry")?,
            None => 0,
        },
    };
    let mut code = 0u64;
    let mut initialized = 0u64;
    let mut uninitialized = 0u64;
    for section in &sections {
        match (section.prot.exec, section.raw()) {
            (true, raw) => code += raw,
            (false, 0) => uninitialized += (section.end - section.start).next_multiple_of(PAGE),
            (false, raw) => initialized += raw,
        }
    }
    let size = |value: u64, what: &str| {
        u32::try_from(value).map_err(|_| error(format!("{value:#x} bytes of {what}, more than PE's 4 GiB")))
    };
    let base_of_code = match sections.iter().find(|it| it.prot.exec) {
        Some(section) => rva(placed, section.start, "the code")?,
        None => 0,
    };
    let dll = pe.dll();

    let mut out = Buf::default();
    out.u16(pe::IMAGE_DOS_SIGNATURE);
    for field in [0x90, 3, 0, 4, 0, 0xffff, 0, 0xb8, 0, 0, 0, 0x40, 0] {
        out.u16(field);
    }
    out.bytes(&[0; 32]);
    out.u32(NT as u32);
    out.bytes(DOS_PROGRAM);
    out.0.resize(NT, 0);

    out.u32(pe::IMAGE_NT_SIGNATURE);
    out.u16(pe::IMAGE_FILE_MACHINE_AMD64);
    out.u16(sections.len() as u16);
    out.u32(stamp(placed));
    out.u32(0);
    out.u32(0);
    out.u16(OPTIONAL as u16);
    let mut characteristics = pe::IMAGE_FILE_EXECUTABLE_IMAGE | pe::IMAGE_FILE_LARGE_ADDRESS_AWARE;
    if dll {
        characteristics |= pe::IMAGE_FILE_DLL;
    }
    out.u16(characteristics);

    out.u16(pe::IMAGE_NT_OPTIONAL_HDR64_MAGIC);
    out.u8(LINKER_VERSION);
    out.u8(0);
    out.u32(size(code, "code")?);
    out.u32(size(initialized, "data")?);
    out.u32(size(uninitialized, "zeroed data")?);
    out.u32(entry);
    out.u32(base_of_code);
    out.u64(base);
    out.u32(PAGE as u32);
    // A region's file offset is its RVA, so the file is aligned as memory is.
    out.u32(PAGE as u32);
    out.u16(WINDOWS_VERSION);
    out.u16(0);
    out.u16(0);
    out.u16(0);
    out.u16(WINDOWS_VERSION);
    out.u16(0);
    out.u32(0);
    out.u32(size_of_image);
    out.u32(PAGE as u32);
    out.u32(0);
    out.u16(if pe.output.windows_subsystem {
        pe::IMAGE_SUBSYSTEM_WINDOWS_GUI
    } else {
        pe::IMAGE_SUBSYSTEM_WINDOWS_CUI
    });
    let mut dll_characteristics = pe::IMAGE_DLLCHARACTERISTICS_HIGH_ENTROPY_VA
        | pe::IMAGE_DLLCHARACTERISTICS_DYNAMIC_BASE
        | pe::IMAGE_DLLCHARACTERISTICS_NX_COMPAT;
    if !dll {
        dll_characteristics |= pe::IMAGE_DLLCHARACTERISTICS_TERMINAL_SERVER_AWARE;
    }
    out.u16(dll_characteristics);
    out.u64(pe.output.stack.unwrap_or(STACK_RESERVE));
    out.u64(COMMIT);
    out.u64(HEAP_RESERVE);
    out.u64(COMMIT);
    out.u32(0);
    out.u32(DIRECTORIES as u32);
    for (at, len) in tables.directories {
        out.u32(at);
        out.u32(len);
    }

    let mut len = PAGE;
    for section in &sections {
        let start = rva(placed, section.start, "a section")?;
        let raw = section.raw();
        out.bytes(&section.name);
        out.u32(size(section.end - section.start, "a section")?);
        out.u32(start);
        out.u32(size(raw, "a section")?);
        out.u32(if raw > 0 { start } else { 0 });
        out.u32(0);
        out.u32(0);
        out.u16(0);
        out.u16(0);
        out.u32(section.characteristics());
        if raw > 0 {
            len = len.max(u64::from(start) + raw);
        }
    }
    Ok(File { len, parts: vec![(0, out.0)] })
}

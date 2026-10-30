//! The tables a PE loader reads, made from the placed image, and the headers around it.
//!
//! The import directory (descriptors, lookup tables, the hint/name table, DLL names; the
//! import address table is the core's GOT, laid out per DLL), the export directory, the debug
//! directory with its CodeView entry, the TLS directory when the CRT defines none, the
//! exception directory (`.pdata`: codegen's functions and the objects' entries, sorted), the
//! base relocations; then the DOS header and stub, the PE and optional headers and the section
//! table.
//!
//! Cost, per link (full or incremental: the core asks again after every patch): `.reloc` is one
//! pass over `Placed::rebase` (sorted by the core; sorted here only if it isn't), 2 bytes per
//! address, with a region cursor rather than a search per address, cut at page boundaries into
//! pieces of 64Ki addresses written in parallel; `.pdata` is one pass over `Placed::functions`
//! merged with the objects' entries, 12 bytes each. The two run in parallel with each other.
//! The import and export tables are linear in imports and exports, plus a sort of each; each
//! import's name is parsed once and borrowed, and nothing in them allocates per entry. `file`
//! doesn't rebuild any table: it reads each directory's size from the bytes `synthesize`
//! produced, which the core put in the regions and sized them to, so it costs the number of
//! regions plus GOT cells.

use std::borrow::Cow;
use std::ops::Range;

use object::pe;
use rayon::prelude::*;

use super::coff::ImportName;
use super::{
    DEBUG, EDATA, IDATA, OBJ_PDATA, PAGE, PDATA, Pe, RELOC, RSRC, Segment, TLS_DIR, TLS_INDEX, error, place,
    zeroed,
};
use crate::model::{Class, Counts, Export, File, Function, Holds, Placed, Prot, Region, SymbolKind};
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
const RUNTIME_FUNCTION: usize = 12;
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

/// A little-endian byte writer over one buffer sized up front.
#[derive(Default)]
pub(super) struct Buf(pub(super) Vec<u8>);

impl Buf {
    pub(super) fn with_capacity(capacity: usize) -> Buf {
        Buf(Vec::with_capacity(capacity))
    }

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

fn rva_of(placed: &Placed<'_>, addr: u64) -> Option<u32> {
    addr.checked_sub(placed.plan.base).and_then(|it| u32::try_from(it).ok())
}

fn rva(placed: &Placed<'_>, addr: u64, what: &str) -> Result<u32, LinkError> {
    Image::of(placed).rva(addr, what)
}

/// What the tables made on other threads read of the placed image: `Placed` itself is not
/// `Sync` (its `symbol` is a `dyn Fn`).
#[derive(Clone, Copy)]
pub(super) struct Image<'a> {
    base: u64,
    regions: &'a [Region],
    contents: &'a [&'a [u8]],
    rebase: &'a [u64],
    functions: &'a [Function],
}

impl<'a> Image<'a> {
    pub(super) fn of(placed: &Placed<'a>) -> Image<'a> {
        Image {
            base: placed.plan.base,
            regions: placed.regions,
            contents: placed.contents,
            rebase: placed.rebase,
            functions: placed.functions,
        }
    }

    fn rva_of(self, addr: u64) -> Option<u32> {
        addr.checked_sub(self.base).and_then(|it| u32::try_from(it).ok())
    }

    fn rva(self, addr: u64, what: &str) -> Result<u32, LinkError> {
        self.rva_of(addr).ok_or_else(|| error(format!("{what} at {addr:#x} is outside the image")))
    }
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
    u64::from(u32_of(bytes, at)) | (u64::from(u32_of(bytes, at + 4)) << 32)
}

/// What the synthetic regions may need, from the counts alone.
pub(super) fn synthetic(counts: &Counts) -> Vec<(Class, u64)> {
    let mut regions = Vec::with_capacity(7);
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
        regions.push((Class::Synthetic(PDATA), counts.functions * RUNTIME_FUNCTION as u64 + counts.unwind));
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

/// The synthetic regions' bytes.
pub(super) struct Tables {
    pub(super) regions: Vec<(Class, Vec<u8>)>,
}

impl Tables {
    pub(super) fn of(placed: &Placed<'_>) -> Result<Tables, LinkError> {
        let mut tables = Tables { regions: Vec::with_capacity(7) };
        if let Some(bind) = placed.binds.first() {
            let name = placed.imports.get(bind.import as usize).map_or("?", |it| it.name.as_str());
            return Err(error(format!(
                "a bind of `{name}` at {:#x}: a PE loader fills only the import address table, \
                 so an import is reached through its cell or its thunk",
                bind.addr
            )));
        }
        if let Some(cell) = placed.got.iter().find(|it| !matches!(it.holds, Holds::Local(_) | Holds::Import(_))) {
            return Err(error(format!(
                "the GOT cell at {:#x} holds {:?}: Windows reaches thread-locals through \
                 `_tls_index` and `SECREL`, never a GOT cell",
                cell.addr, cell.holds
            )));
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
            Some((_, region)) => tables.push(placed, IDATA, imports(placed, region)?)?,
            None if placed.got.iter().any(|it| matches!(it.holds, Holds::Import(_))) => {
                return Err(error("an image with imports and no import directory region".into()));
            }
            None => {}
        }
        match placed.region(&Class::Synthetic(EDATA)) {
            Some((_, region)) => tables.push(placed, EDATA, exports(placed, region)?)?,
            None if !placed.exports.is_empty() => {
                return Err(error("an image with exports and no export directory region".into()));
            }
            None => {}
        }
        if let Some((_, region)) = placed.region(&Class::Synthetic(DEBUG)) {
            tables.push(placed, DEBUG, debug(placed, region)?)?;
        }

        let mut extra = Vec::with_capacity(4);
        let tls = match tls_directory(placed)? {
            TlsDirectory::Ours(addr) => own_tls(placed, addr, &mut extra)?,
            TlsDirectory::Crt(_) | TlsDirectory::None => Vec::new(),
        };
        if placed.region(&Class::Synthetic(TLS_DIR)).is_some() {
            tables.push(placed, TLS_DIR, tls)?;
        }
        if placed.region(&Class::Synthetic(TLS_INDEX)).is_some() {
            tables.push(placed, TLS_INDEX, vec![0; 8])?;
        }

        let image = Image::of(placed);
        let (pdata, relocations) = rayon::join(|| pdata(image), || relocations(image, &extra, RELOC_PIECE));
        match placed.region(&Class::Synthetic(PDATA)) {
            Some(_) => tables.push(placed, PDATA, pdata?)?,
            None if placed.functions.iter().any(|it| it.windows.is_some()) => {
                return Err(error("an image with unwind info and no `.pdata` region".into()));
            }
            None => {}
        }
        match placed.region(&Class::Synthetic(RELOC)) {
            Some(_) => tables.push(placed, RELOC, relocations?)?,
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
}

/// Which TLS directory the image has.
enum TlsDirectory {
    None,
    /// The CRT's `_tls_used`, at this address.
    Crt(u64),
    /// The writer's own, in its region at this address.
    Ours(u64),
}

/// The CRT's `_tls_used` when it defines one (the template must be the range it states), else
/// the writer's own for an image with thread-locals or one that names `_tls_used`.
fn tls_directory(placed: &Placed<'_>) -> Result<TlsDirectory, LinkError> {
    let ours = placed.region(&Class::Synthetic(TLS_DIR)).map(|(_, it)| it.addr);
    let used = (placed.symbol)("_tls_used");
    match used {
        Some(addr) if Some(addr) != ours => {
            let directory = bytes_at(placed, addr, TLS_DIRECTORY, "the CRT's TLS directory `_tls_used`")?;
            let start = u64_of(directory, 0);
            let end = u64_of(directory, 8);
            // The CRT's range runs from `_tls_start` (`.tls`, first) to `_tls_end` (`.tls$ZZZ`,
            // whose own byte is past it); every other thread-local ranks between the two. The
            // template's growth room and that byte are past `end`.
            let placed_end = placed
                .regions
                .iter()
                .filter(|it| matches!(it.class, Class::Tdata | Class::Tbss))
                .map(|it| it.addr + it.size)
                .max();
            if let (Some(tls), Some(placed_end)) = (placed.tls, placed_end)
                && (tls.addr != start || end < start || end > placed_end)
            {
                return Err(error(format!(
                    "the thread-locals placed at {:#x}..{placed_end:#x} don't hold the range \
                     {start:#x}..{end:#x} the CRT's `_tls_used` states (between `_tls_start` and \
                     `_tls_end`)",
                    tls.addr
                )));
            }
            Ok(TlsDirectory::Crt(addr))
        }
        _ if used.is_some() || placed.tls.is_some() => ours
            .map(TlsDirectory::Ours)
            .ok_or_else(|| error("an image with thread-locals and no TLS directory region".into())),
        _ => Ok(TlsDirectory::None),
    }
}

/// The writer's TLS directory at `addr`, and its four addresses the loader rebases.
fn own_tls(placed: &Placed<'_>, addr: u64, extra: &mut Vec<u64>) -> Result<Vec<u8>, LinkError> {
    let index = (placed.symbol)("_tls_index")
        .or_else(|| placed.region(&Class::Synthetic(TLS_INDEX)).map(|(_, it)| it.addr))
        .ok_or_else(|| error("an image with thread-locals and no `_tls_index`".into()))?;
    let callbacks = (placed.symbol)("__xl_a").map_or(addr + TLS_DIRECTORY, |it| it + 8);
    let (start, end, zero_fill, align) = match placed.tls {
        Some(tls) => (tls.addr, tls.addr + tls.init, tls.size - tls.init, tls.align),
        None => (0, 0, 0, 1),
    };
    let zero_fill =
        u32::try_from(zero_fill).map_err(|_| error(format!("{zero_fill:#x} bytes of zeroed thread-locals")))?;
    let mut bytes = Buf::with_capacity((TLS_DIRECTORY + 8) as usize);
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
    Ok(bytes.0)
}

/// `IMAGE_SCN_ALIGN_*` of an alignment.
pub(super) fn alignment(align: u64) -> Result<u32, String> {
    let align = align.max(1);
    if !align.is_power_of_two() || align > 8192 {
        return Err(format!("an alignment of {align}, which a PE section can't state"));
    }
    Ok((align.trailing_zeros() + 1) << 20)
}

/// An import cell, `(library, cell, import)`: 16 bytes per import, sorted once.
type Cell = (u32, u64, u32);
const _: () = assert!(size_of::<Cell>() == 16);
/// A `RUNTIME_FUNCTION`, `(begin, end, info)` RVAs: 12 bytes per function, as in the file.
type Entry = (u32, u32, u32);
const _: () = assert!(size_of::<Entry>() == RUNTIME_FUNCTION);

/// The GOT's import cells sorted by library then cell: each DLL's IAT.
fn iat(placed: &Placed<'_>) -> Result<Vec<Cell>, LinkError> {
    let mut cells = Vec::with_capacity(placed.imports.len());
    for cell in placed.got {
        let Holds::Import(import) = cell.holds else { continue };
        let it = placed.imports.get(import as usize).ok_or_else(|| {
            error(format!("the GOT cell at {:#x} holds import {import}, which there isn't", cell.addr))
        })?;
        let library = it.library.ok_or_else(|| error(format!("`{}` is imported from no DLL", it.name)))?;
        if placed.libraries.get(library as usize).is_none() {
            return Err(error(format!("`{}` is imported from library {library}, which there isn't", it.name)));
        }
        cells.push((library, cell.addr, import));
    }
    cells.sort_unstable();
    Ok(cells)
}

/// The import directory, the lookup tables, the hint/name table and the DLL names. Each DLL's
/// IAT cells are the core's GOT cells of its imports, consecutive and followed by a zero cell.
fn imports(placed: &Placed<'_>, region: &Region) -> Result<Vec<u8>, LinkError> {
    let cells = iat(placed)?;
    if cells.is_empty() {
        return Ok(Vec::new());
    }
    let mut taken: Vec<u64> = placed.got.iter().map(|it| it.addr).collect();
    taken.sort_unstable();
    // Each DLL's run of `cells` (and of `names`, which is parallel to it).
    let mut groups: Vec<Range<usize>> = Vec::with_capacity(placed.libraries.len());
    for group in cells.chunk_by(|a, b| a.0 == b.0) {
        let start = groups.last().map_or(0, |it| it.end);
        groups.push(start..start + group.len());
    }
    let names = cells
        .iter()
        .map(|&(_, _, import)| {
            let it = &placed.imports[import as usize];
            ImportName::parse(&it.name, it.version.as_deref()).map_err(error)
        })
        .collect::<Result<Vec<_>, _>>()?;
    for group in &groups {
        let group = &cells[group.clone()];
        let dll = &placed.libraries[group[0].0 as usize].name;
        for pair in group.windows(2) {
            if pair[1].1 != pair[0].1 + 8 {
                return Err(error(format!(
                    "the import address table of {dll} is not consecutive: cells at {:#x} and {:#x}",
                    pair[0].1, pair[1].1
                )));
            }
        }
        let after = group[group.len() - 1].1 + 8;
        if taken.binary_search(&after).is_ok() {
            return Err(error(format!(
                "the import address table of {dll} has no zero cell after it: {after:#x} is a GOT cell"
            )));
        }
    }

    let base = region.addr;
    let descriptors = ((groups.len() as u64 + 1) * DESCRIPTOR).next_multiple_of(8);
    let lookup = (cells.len() as u64 + groups.len() as u64) * 8;
    let hint_start = descriptors + lookup;
    let mut out = Buf::with_capacity(region.capacity.max(region.size) as usize);
    let mut lookup_at = descriptors;
    // The hint/name entries and DLL names are written after the lookup tables, so their
    // offsets are computed first: the names' bytes are known from the imports.
    let hint_names: u64 = names
        .iter()
        .map(|it| match it {
            ImportName::Name { name, .. } => (2 + name.len() as u64 + 1).next_multiple_of(2),
            ImportName::Ordinal(_) => 0,
        })
        .sum();
    let mut name_at = hint_start + hint_names;
    for group in &groups {
        let first = cells[group.start];
        let dll = &placed.libraries[first.0 as usize].name;
        out.u32(rva(placed, base + lookup_at, "an import lookup table")?);
        out.u32(0);
        out.u32(0);
        out.u32(rva(placed, base + name_at, "a DLL name")?);
        out.u32(rva(placed, first.1, "an import address table")?);
        lookup_at += (group.len() as u64 + 1) * 8;
        name_at += dll.len() as u64 + 1;
    }
    out.bytes(&[0; DESCRIPTOR as usize]);
    out.align(8);
    let mut hint_at = hint_start;
    for group in &groups {
        for name in &names[group.clone()] {
            match *name {
                ImportName::Ordinal(ordinal) => out.u64(pe::IMAGE_ORDINAL_FLAG64 | u64::from(ordinal)),
                ImportName::Name { name, .. } => {
                    out.u64(u64::from(rva(placed, base + hint_at, "a hint/name entry")?));
                    hint_at += (2 + name.len() as u64 + 1).next_multiple_of(2);
                }
            }
        }
        out.u64(0);
    }
    for name in &names {
        if let ImportName::Name { hint, name } = *name {
            out.u16(hint);
            out.string(name);
            out.align(2);
        }
    }
    for group in &groups {
        out.string(&placed.libraries[cells[group.start].0 as usize].name);
    }
    Ok(out.0)
}

/// The export directory: every export by name, the names sorted (the loader searches them
/// by bisection), ordinals from 1 in that order.
fn exports(placed: &Placed<'_>, region: &Region) -> Result<Vec<u8>, LinkError> {
    let mut exports: Vec<&Export> = placed.exports.iter().collect();
    blaze_ids::names::sort_by_name(&mut exports, |it| it.name.as_bytes());
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
    let strings = ordinals + 2 * n;
    let names: u64 = exports.iter().map(|it| it.name.len() as u64 + 1).sum();
    let image_name = strings + names;

    let mut out = Buf::with_capacity((image_name + placed.name.len() as u64 + 1) as usize);
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
        let at = rva_of(placed, export.addr)
            .ok_or_else(|| error(format!("the export `{}` at {:#x} is outside the image", export.name, export.addr)))?;
        out.u32(at);
    }
    let mut at = strings;
    for export in &exports {
        if export.name.is_empty() {
            return Err(error(format!("an export with no name at {:#x}", export.addr)));
        }
        out.u32(rva(placed, base + at, "an export name")?);
        at += export.name.len() as u64 + 1;
    }
    for ordinal in 0..count {
        out.u16(ordinal);
    }
    for export in &exports {
        out.string(&export.name);
    }
    out.string(placed.name);
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
    let mut out = Buf::with_capacity((DEBUG_DIRECTORY + u64::from(size)) as usize);
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

/// `RUNTIME_FUNCTION`s: one per function codegen gave Windows unwind info (in address order, as
/// the core gives them), merged with the native objects' `.pdata` entries (those of dropped
/// functions read 0), sorted by start; an overlap is an error.
fn pdata(image: Image<'_>) -> Result<Vec<u8>, LinkError> {
    let mut ours: Vec<Entry> = Vec::with_capacity(image.functions.len());
    for function in image.functions {
        let Some(windows) = function.windows else { continue };
        let entry = (|| {
            Some((image.rva_of(function.addr)?, image.rva_of(function.addr + function.size)?, image.rva_of(windows.info)?))
        })();
        ours.push(entry.ok_or_else(|| {
            let name = function.name.as_deref().unwrap_or("a function");
            error(format!("{name} at {:#x} or its unwind info is outside the image", function.addr))
        })?);
    }
    if !ours.is_sorted() {
        ours.sort_unstable();
    }
    let mut native: Vec<Entry> = Vec::new();
    for (index, region) in image.regions.iter().enumerate() {
        if !matches!(&region.class, Class::Named { name, .. } if name == OBJ_PDATA) {
            continue;
        }
        let bytes = image.contents[index];
        if !bytes.len().is_multiple_of(RUNTIME_FUNCTION) {
            return Err(error(format!(
                "the objects' .pdata at {:#x} is {} bytes, not whole RUNTIME_FUNCTIONs",
                region.addr,
                bytes.len()
            )));
        }
        native.reserve(bytes.len() / RUNTIME_FUNCTION);
        for entry in bytes.chunks_exact(RUNTIME_FUNCTION) {
            let begin = u32_of(entry, 0);
            if begin != 0 {
                native.push((begin, u32_of(entry, 4), u32_of(entry, 8)));
            }
        }
    }
    if !native.is_sorted() {
        native.sort_unstable();
    }
    let mut out = Vec::with_capacity((ours.len() + native.len()) * RUNTIME_FUNCTION);
    let (mut a, mut b) = (ours.iter().peekable(), native.iter().peekable());
    let mut last: Option<Entry> = None;
    loop {
        let entry = match (a.peek(), b.peek()) {
            (Some(x), Some(y)) if y < x => b.next(),
            (Some(_), _) => a.next(),
            (None, _) => b.next(),
        };
        let Some(&(begin, end, info)) = entry else { break };
        if let Some((before, before_end, _)) = last
            && begin < before_end
        {
            return Err(error(format!(
                "the unwind entries of RVAs {before:#x}..{before_end:#x} and {begin:#x}..{end:#x} overlap"
            )));
        }
        last = Some((begin, end, info));
        out.extend_from_slice(&begin.to_le_bytes());
        out.extend_from_slice(&end.to_le_bytes());
        out.extend_from_slice(&info.to_le_bytes());
    }
    Ok(out)
}

/// Rebased addresses per piece of `.reloc` written on one thread: below this a thread costs
/// more than the 2 bytes per address it would write.
const RELOC_PIECE: usize = 1 << 16;

fn page_of(addr: u64) -> u64 {
    addr & !(PAGE - 1)
}

/// The base relocations: a block per 4 KiB page, a `DIR64` entry per rebased address in it,
/// padded to 4 bytes. The sorted addresses are cut at page boundaries into pieces of about
/// `piece` addresses, each written on its own thread and joined in order: a block never spans
/// two pieces, so the bytes are the same whatever the number of threads or the piece size.
pub(super) fn relocations(image: Image<'_>, extra: &[u64], piece: usize) -> Result<Vec<u8>, LinkError> {
    let rebase: Cow<'_, [u64]> = if image.rebase.is_sorted() {
        Cow::Borrowed(image.rebase)
    } else {
        let mut sorted = image.rebase.to_vec();
        sorted.sort_unstable();
        Cow::Owned(sorted)
    };
    let mut extra = extra.to_vec();
    extra.sort_unstable();
    let piece = piece.max(1);
    // The first address of each piece but the first: the first of a page.
    let mut cuts = Vec::with_capacity(rebase.len() / piece + 1);
    let mut at = piece;
    while at < rebase.len() {
        let page = page_of(rebase[at]);
        let mut cut = rebase.partition_point(|it| page_of(*it) < page);
        if cut <= cuts.last().copied().unwrap_or(0) {
            cut = rebase.partition_point(|it| page_of(*it) <= page);
        }
        if cut >= rebase.len() {
            break;
        }
        cuts.push(cut);
        at = cut + piece;
    }
    if cuts.is_empty() {
        let mut out = Vec::new();
        blocks(image, &rebase, &extra, &mut out)?;
        return Ok(out);
    }
    // Each piece's addresses and the extra ones in its pages.
    let mut pieces = Vec::with_capacity(cuts.len() + 1);
    let (mut from, mut extra_from) = (0, 0);
    for &cut in &cuts {
        let page = page_of(rebase[cut]);
        let extra_to = extra.partition_point(|it| page_of(*it) < page);
        pieces.push((&rebase[from..cut], &extra[extra_from..extra_to]));
        (from, extra_from) = (cut, extra_to);
    }
    pieces.push((&rebase[from..], &extra[extra_from..]));
    let written: Vec<Result<Vec<u8>, LinkError>> = pieces
        .par_iter()
        .map(|&(rebase, extra)| {
            let mut out = Vec::new();
            blocks(image, rebase, extra, &mut out).map(|()| out)
        })
        .collect();
    let mut out = Vec::with_capacity(written.iter().map(|it| it.as_ref().map_or(0, Vec::len)).sum());
    for piece in written {
        out.extend_from_slice(&piece?);
    }
    Ok(out)
}

/// The blocks of sorted rebased addresses merged with sorted `extra`, appended to `out`: one
/// pass, with a cursor over the regions checking each lies in a region with bytes in the file.
fn blocks(image: Image<'_>, rebase: &[u64], extra: &[u64], out: &mut Vec<u8>) -> Result<(), LinkError> {
    let regions = image.regions;
    let count = rebase.len() + extra.len();
    let lowest = rebase.first().into_iter().chain(extra.first()).min();
    let highest = rebase.last().into_iter().chain(extra.last()).max();
    let (Some(&lowest), Some(&highest)) = (lowest, highest) else {
        return Ok(());
    };
    let pages = ((highest - lowest) / PAGE + 1) as usize;
    out.reserve(count * 2 + pages.min(count) * 12);
    let (mut i, mut j) = (0, 0);
    let mut region = 0;
    let mut last = None;
    let mut block: Option<(u64, usize)> = None;
    loop {
        let addr = match (rebase.get(i), extra.get(j)) {
            (Some(&a), Some(&b)) if b < a => {
                j += 1;
                b
            }
            (Some(&a), _) => {
                i += 1;
                a
            }
            (None, Some(&b)) => {
                j += 1;
                b
            }
            (None, None) => break,
        };
        if last == Some(addr) {
            continue;
        }
        last = Some(addr);
        while region < regions.len() && regions[region].addr + regions[region].capacity.max(regions[region].size) < addr + 8 {
            region += 1;
        }
        match regions.get(region) {
            Some(it) if it.addr <= addr && zeroed(&it.class) => {
                return Err(error(format!("a rebased address at {addr:#x} in {:?}, which is not in the file", it.class)));
            }
            Some(it) if it.addr <= addr => {}
            _ => return Err(error(format!("a rebased address at {addr:#x} in no region"))),
        }
        let page = page_of(addr);
        if block.map(|it| it.0) != Some(page) {
            if let Some((_, start)) = block {
                close(out, start);
            }
            block = Some((page, out.len()));
            out.extend_from_slice(&image.rva(page, "a rebased page")?.to_le_bytes());
            out.extend_from_slice(&[0; 4]);
        }
        out.extend_from_slice(&((pe::IMAGE_REL_BASED_DIR64 << 12) | (addr & (PAGE - 1)) as u16).to_le_bytes());
    }
    if let Some((_, start)) = block {
        close(out, start);
    }
    Ok(())
}

/// Ends the block at `start`: a pad entry to 4 bytes, and its size.
fn close(out: &mut Vec<u8>, start: usize) {
    if !out.len().is_multiple_of(4) {
        out.extend_from_slice(&[0; 2]);
    }
    let size = (out.len() - start) as u32;
    out[start + 4..start + 8].copy_from_slice(&size.to_le_bytes());
}

/// The data directories, RVA and size by `IMAGE_DIRECTORY_ENTRY_*`, read from the tables
/// `synthesize` made (the core sized each synthetic region to its bytes) rather than made again.
fn directories(placed: &Placed<'_>) -> Result<[(u32, u32); DIRECTORIES], LinkError> {
    let mut directories = [(0, 0); DIRECTORIES];
    let filled = |name: &'static str| -> Result<Option<(usize, u32, u32)>, LinkError> {
        match placed.region(&Class::Synthetic(name)) {
            Some((index, region)) if region.size > 0 => {
                let size = u32::try_from(region.size).map_err(|_| error(format!("a {name} of {:#x} bytes", region.size)))?;
                Ok(Some((index, rva(placed, region.addr, name)?, size)))
            }
            _ => Ok(None),
        }
    };
    if let Some((index, at, _)) = filled(IDATA)? {
        let count = placed
            .bytes(index)
            .chunks_exact(DESCRIPTOR as usize)
            .position(|it| it.iter().all(|byte| *byte == 0))
            .ok_or_else(|| error("the import directory has no null descriptor: its bytes are not in place".into()))?;
        directories[pe::IMAGE_DIRECTORY_ENTRY_IMPORT] = (at, (count as u32 + 1) * DESCRIPTOR as u32);
        let bounds = placed
            .got
            .iter()
            .filter(|it| matches!(it.holds, Holds::Import(_)))
            .fold(None, |bounds: Option<(u64, u64)>, it| match bounds {
                Some((first, last)) => Some((first.min(it.addr), last.max(it.addr))),
                None => Some((it.addr, it.addr)),
            });
        if let Some((first, last)) = bounds {
            directories[pe::IMAGE_DIRECTORY_ENTRY_IAT] =
                (rva(placed, first, "the import address table")?, (last + 16 - first) as u32);
        }
    }
    if let Some((_, at, size)) = filled(EDATA)? {
        directories[pe::IMAGE_DIRECTORY_ENTRY_EXPORT] = (at, size);
    }
    if let Some((index, at, _)) = filled(DEBUG)? {
        if placed.bytes(index).get(12..16) != Some(&pe::IMAGE_DEBUG_TYPE_CODEVIEW.to_le_bytes()[..]) {
            return Err(error("the debug directory's bytes are not in place: `file` before `synthesize`".into()));
        }
        directories[pe::IMAGE_DIRECTORY_ENTRY_DEBUG] = (at, DEBUG_DIRECTORY as u32);
    }
    match tls_directory(placed)? {
        TlsDirectory::Crt(addr) | TlsDirectory::Ours(addr) => {
            directories[pe::IMAGE_DIRECTORY_ENTRY_TLS] = (rva(placed, addr, "the TLS directory")?, TLS_DIRECTORY as u32);
        }
        TlsDirectory::None => {}
    }
    if let Some((_, at, size)) = filled(PDATA)? {
        directories[pe::IMAGE_DIRECTORY_ENTRY_EXCEPTION] = (at, size);
    }
    if let Some((_, at, size)) = filled(RELOC)? {
        directories[pe::IMAGE_DIRECTORY_ENTRY_BASERELOC] = (at, size);
    }
    if let Some(addr) = (placed.symbol)("_load_config_used") {
        let size = u32_of(bytes_at(placed, addr, 4, "the load configuration `_load_config_used`")?, 0);
        directories[pe::IMAGE_DIRECTORY_ENTRY_LOAD_CONFIG] = (rva(placed, addr, "the load configuration")?, size);
    }
    let resources = || placed.regions.iter().filter(|it| matches!(&it.class, Class::Named { name, .. } if name == RSRC));
    if let (Some(start), Some(end)) =
        (resources().map(|it| it.addr).min(), resources().map(|it| it.addr + it.size).max())
    {
        directories[pe::IMAGE_DIRECTORY_ENTRY_RESOURCE] =
            (rva(placed, start, "the resource directory")?, (end - start) as u32);
    }
    Ok(directories)
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
    let mut sections: Vec<(Segment, Out)> = Vec::with_capacity(8);
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
    let directories = directories(placed)?;
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
            rva(placed, addr, entry)?
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

    let mut out = Buf::with_capacity(headers);
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
    for (at, len) in directories {
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

//! The PE headers and tables blaze's loader reads, from bytes it does not trust: every offset
//! and length is checked against the image's bytes, and nothing here panics or writes. The
//! loader and the tests' reference model walk the tables through these same walkers.

use crate::Error;

const MACHINE_AMD64: u16 = 0x8664;
const PE32_PLUS: u16 = 0x20b;
const FILE_RELOCS_STRIPPED: u16 = 0x0001;
const FILE_EXECUTABLE: u16 = 0x0002;
const FILE_DLL: u16 = 0x2000;
const SCN_CNT_CODE: u32 = 0x20;
const SCN_MEM_EXECUTE: u32 = 0x2000_0000;
const SCN_MEM_WRITE: u32 = 0x8000_0000;
/// The page sections start on and are protected by.
pub(super) const PAGE: u64 = 0x1000;
/// The most sections the Windows loader takes.
const MOST_SECTIONS: usize = 96;
const SECTION_HEADER: u64 = 40;
const OPTIONAL_FIXED: u64 = 112;

const DIR_IMPORT: usize = 1;
const DIR_EXCEPTION: usize = 3;
const DIR_BASERELOC: usize = 5;
const DIR_TLS: usize = 9;
const DIR_DELAY_IMPORT: usize = 13;
const DIR_CLR: usize = 14;

const REL_ABSOLUTE: u16 = 0;
const REL_DIR64: u16 = 10;
pub(super) const RUNTIME_FUNCTION: u64 = 12;
const TLS_DIRECTORY: u64 = 40;
const IMPORT_DESCRIPTOR: u64 = 20;
const MOST_IMPORTED: u64 = 4096;
const MOST_CALLBACKS: usize = 1024;

pub(super) fn bad(what: impl std::fmt::Display) -> Error {
    Error::new(format!("the program's PE image: {what}"))
}

fn array<const N: usize>(bytes: &[u8], at: u64) -> Option<[u8; N]> {
    let at = usize::try_from(at).ok()?;
    bytes.get(at..at.checked_add(N)?)?.try_into().ok()
}

pub(super) fn u16_at(bytes: &[u8], at: u64) -> Option<u16> {
    array(bytes, at).map(u16::from_le_bytes)
}

pub(super) fn u32_at(bytes: &[u8], at: u64) -> Option<u32> {
    array(bytes, at).map(u32::from_le_bytes)
}

pub(super) fn u64_at(bytes: &[u8], at: u64) -> Option<u64> {
    array(bytes, at).map(u64::from_le_bytes)
}

/// Reads of the headers, each failing with what it read and where.
struct Fields<'a>(&'a [u8]);

impl Fields<'_> {
    fn short(&self, what: &str, at: u64) -> Error {
        bad(format!("{what} at {at:#x} is past the end of the image ({:#x} bytes)", self.0.len()))
    }

    fn u16(&self, at: u64, what: &str) -> Result<u16, Error> {
        u16_at(self.0, at).ok_or_else(|| self.short(what, at))
    }

    fn u32(&self, at: u64, what: &str) -> Result<u32, Error> {
        u32_at(self.0, at).ok_or_else(|| self.short(what, at))
    }

    fn u64(&self, at: u64, what: &str) -> Result<u64, Error> {
        u64_at(self.0, at).ok_or_else(|| self.short(what, at))
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(super) struct Directory {
    pub(super) rva: u64,
    pub(super) size: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Section {
    name: [u8; 8],
    pub(super) rva: u64,
    /// Its length in memory, a multiple of the section alignment.
    pub(super) extent: u64,
    /// The bytes of it in memory (`VirtualSize`, or `SizeOfRawData` where that is 0).
    pub(super) virtual_len: u64,
    /// The bytes of it in the file, which start at `rva` as well.
    pub(super) raw_len: u64,
    characteristics: u32,
}

impl Section {
    pub(super) fn code(&self) -> bool {
        self.characteristics & (SCN_CNT_CODE | SCN_MEM_EXECUTE) != 0
    }

    pub(super) fn writable(&self) -> bool {
        self.characteristics & SCN_MEM_WRITE != 0
    }

    pub(super) fn end(&self) -> u64 {
        self.rva + self.extent
    }

    pub(super) fn name(&self) -> String {
        let len = self.name.iter().position(|it| *it == 0).unwrap_or(self.name.len());
        String::from_utf8_lossy(&self.name[..len]).into_owned()
    }
}

/// The headers of a PE32+ x86-64 executable whose file is laid out as it is in memory.
#[derive(Debug)]
pub(super) struct Headers<'a> {
    /// The image's bytes, at least `size_of_image` of them.
    pub(super) bytes: &'a [u8],
    pub(super) image_base: u64,
    pub(super) size_of_image: u64,
    pub(super) size_of_headers: u64,
    /// The RVA of the entry, in a section of code.
    pub(super) entry: u64,
    pub(super) stack_reserve: u64,
    /// Whether it can be loaded anywhere but `image_base`.
    pub(super) relocatable: bool,
    /// In address order, none overlapping, none empty.
    pub(super) sections: Vec<Section>,
    directories: [Directory; 16],
}

pub(super) fn parse(bytes: &[u8]) -> Result<Headers<'_>, Error> {
    let fields = Fields(bytes);
    if bytes.get(..2) != Some(b"MZ") {
        return Err(bad("no DOS header (MZ)"));
    }
    let pe = u64::from(fields.u32(0x3c, "the PE header's offset")?);
    if array::<4>(bytes, pe) != Some(*b"PE\0\0") {
        return Err(bad(format!("no PE signature at {pe:#x}")));
    }
    let coff = pe + 4;
    let machine = fields.u16(coff, "the machine")?;
    if machine != MACHINE_AMD64 {
        return Err(bad(format!("machine {machine:#06x}, where blaze's loader runs x86-64 (0x8664) only")));
    }
    let count = usize::from(fields.u16(coff + 2, "the count of sections")?);
    let optional_len = u64::from(fields.u16(coff + 16, "the optional header's size")?);
    let flags = fields.u16(coff + 18, "the image's characteristics")?;
    if flags & FILE_EXECUTABLE == 0 || flags & FILE_DLL != 0 {
        return Err(bad(format!("characteristics {flags:#06x}: blaze's loader runs executables, not objects or DLLs")));
    }
    let optional = coff + 20;
    let magic = fields.u16(optional, "the optional header's magic")?;
    if magic != PE32_PLUS || optional_len < OPTIONAL_FIXED {
        return Err(bad(format!("an optional header of magic {magic:#x} and {optional_len} bytes, not PE32+")));
    }
    let entry = u64::from(fields.u32(optional + 16, "the entry")?);
    let image_base = fields.u64(optional + 24, "the image base")?;
    let align = u64::from(fields.u32(optional + 32, "the section alignment")?);
    let size_of_image = u64::from(fields.u32(optional + 56, "the image's size")?);
    let size_of_headers = u64::from(fields.u32(optional + 60, "the headers' size")?);
    let stack_reserve = fields.u64(optional + 72, "the stack reserve")?;
    let listed = fields.u32(optional + 108, "the count of data directories")?;
    if !align.is_power_of_two() || align < PAGE {
        return Err(bad(format!("a section alignment of {align:#x}, not a power of two of a page or more")));
    }
    if !image_base.is_multiple_of(64 << 10) {
        return Err(bad(format!("an image base of {image_base:#x}, not on 64 KiB")));
    }
    if size_of_image > bytes.len() as u64 || size_of_headers > size_of_image {
        return Err(bad(format!(
            "an image of {size_of_image:#x} bytes with {size_of_headers:#x} of headers, in {:#x} bytes",
            bytes.len()
        )));
    }

    let mut directories = [Directory::default(); 16];
    let listed = (listed as usize).min(directories.len());
    if optional_len < OPTIONAL_FIXED + 8 * listed as u64 {
        return Err(bad(format!("{listed} data directories in an optional header of {optional_len} bytes")));
    }
    for (index, directory) in directories.iter_mut().enumerate().take(listed) {
        let at = optional + OPTIONAL_FIXED + 8 * index as u64;
        *directory = Directory {
            rva: u64::from(fields.u32(at, "a data directory")?),
            size: u64::from(fields.u32(at + 4, "a data directory")?),
        };
    }
    if directories[DIR_DELAY_IMPORT].rva != 0 {
        return Err(bad("delay-load imports, which blaze's loader does not resolve (blaze-link writes none)"));
    }
    if directories[DIR_CLR].rva != 0 {
        return Err(bad("a .NET header: blaze's loader runs native code only"));
    }

    if count == 0 || count > MOST_SECTIONS {
        return Err(bad(format!("{count} sections")));
    }
    let table = optional + optional_len;
    if table + SECTION_HEADER * count as u64 > size_of_headers {
        return Err(bad(format!("a section table at {table:#x} of {count} sections past the headers' {size_of_headers:#x} bytes")));
    }
    let mut sections = Vec::with_capacity(count);
    let mut end = size_of_headers;
    for index in 0..count as u64 {
        let at = table + SECTION_HEADER * index;
        let name = array::<8>(bytes, at).ok_or_else(|| fields.short("a section's name", at))?;
        let virtual_size = u64::from(fields.u32(at + 8, "a section's size")?);
        let rva = u64::from(fields.u32(at + 12, "a section's address")?);
        let raw_len = u64::from(fields.u32(at + 16, "a section's raw size")?);
        let raw_at = u64::from(fields.u32(at + 20, "a section's raw data")?);
        let characteristics = fields.u32(at + 36, "a section's characteristics")?;
        let virtual_len = if virtual_size == 0 { raw_len } else { virtual_size };
        let section = Section {
            name,
            rva,
            extent: virtual_len.next_multiple_of(align),
            virtual_len,
            raw_len,
            characteristics,
        };
        if virtual_len == 0 {
            continue;
        }
        let label = section.name();
        if !rva.is_multiple_of(align) || rva < end {
            return Err(bad(format!(
                "section {label} at RVA {rva:#x}: not on the section alignment {align:#x}, or over what ends at {end:#x}"
            )));
        }
        if raw_len != 0 && raw_at != rva {
            return Err(bad(format!(
                "section {label} has its bytes at file offset {raw_at:#x} and its address at RVA {rva:#x}: blaze's \
                 loader maps the image from one memory object as the file is laid out, so blaze-link must place \
                 each section's bytes at its RVA (PointerToRawData == VirtualAddress)"
            )));
        }
        if raw_len > section.extent || section.end() > size_of_image {
            return Err(bad(format!(
                "section {label} of {raw_len:#x} bytes in the file and {:#x} in memory at RVA {rva:#x}, in an image \
                 of {size_of_image:#x}",
                section.extent
            )));
        }
        if section.code() && section.writable() {
            return Err(bad(format!("section {label} is both writable and executable")));
        }
        if section.code() && virtual_len > raw_len {
            let tail = &bytes[(rva + raw_len) as usize..(rva + virtual_len) as usize];
            if tail.iter().any(|it| *it != 0) {
                return Err(bad(format!(
                    "section {label} of code has bytes that are not zero past its raw data, in memory it zero-fills"
                )));
            }
        }
        end = section.end();
        sections.push(section);
    }
    let headers = Headers {
        bytes,
        image_base,
        size_of_image,
        size_of_headers,
        entry,
        stack_reserve,
        relocatable: flags & FILE_RELOCS_STRIPPED == 0,
        sections,
        directories,
    };
    if headers.code_at(entry).is_none() {
        return Err(bad(format!("an entry at RVA {entry:#x}, which is in no section of code")));
    }
    Ok(headers)
}

impl<'a> Headers<'a> {
    fn directory(&self, index: usize) -> Option<Directory> {
        let directory = self.directories[index];
        (directory.rva != 0 && directory.size != 0).then_some(directory)
    }

    /// The section all of `rva..rva + len` is in.
    pub(super) fn section_of(&self, rva: u64, len: u64) -> Option<&Section> {
        let index = self.sections.partition_point(|it| it.rva <= rva).checked_sub(1)?;
        let section = &self.sections[index];
        (rva.checked_add(len)? <= section.end()).then_some(section)
    }

    pub(super) fn code_at(&self, rva: u64) -> Option<&Section> {
        self.section_of(rva, 1).filter(|it| it.code())
    }

    /// The section of data all of `rva..rva + len` is in, or an error naming `what`.
    pub(super) fn data(&self, rva: u64, len: u64, what: &str) -> Result<&Section, Error> {
        match self.section_of(rva, len) {
            Some(section) if !section.code() => Ok(section),
            Some(section) => Err(bad(format!(
                "{what} at RVA {rva:#x} is in section {} of code: blaze's loader shares code with the daemon and \
                 never writes it",
                section.name()
            ))),
            None => Err(bad(format!("{what} at RVA {rva:#x}, {len} bytes, is in no section"))),
        }
    }

    /// Where the bytes at `rva` that the file holds end: the end of the headers or of the raw
    /// data of the section `rva` is in.
    fn raw_end(&self, rva: u64) -> Option<u64> {
        if rva < self.size_of_headers {
            return Some(self.size_of_headers);
        }
        let section = self.section_of(rva, 1)?;
        let end = section.rva + section.raw_len;
        (rva < end).then_some(end)
    }

    /// The `len` bytes the file holds at `rva`.
    pub(super) fn raw(&self, rva: u64, len: u64) -> Option<&'a [u8]> {
        let end = rva.checked_add(len)?;
        if len != 0 && end > self.raw_end(rva)? {
            return None;
        }
        self.bytes.get(usize::try_from(rva).ok()?..usize::try_from(end).ok()?)
    }

    /// The string at `rva`, of at most `most` bytes before its NUL, which the image holds
    /// right after it.
    fn c_str(&self, rva: u64, most: usize) -> Option<&'a [u8]> {
        let end = self.raw_end(rva)?;
        let bytes = self.raw(rva, end - rva)?;
        let len = bytes.iter().take(most + 1).position(|it| *it == 0)?;
        Some(&bytes[..len])
    }

    /// The RVA of the address `va` as the image states it (based at `image_base`).
    fn rva(&self, va: u64, what: &str) -> Result<u64, Error> {
        va.checked_sub(self.image_base)
            .filter(|it| *it < self.size_of_image)
            .ok_or_else(|| bad(format!("{what} at {va:#x}, outside the image at {:#x}", self.image_base)))
    }

    /// Where the view of code ends: headers and code before it, only data from it on. Views
    /// are whole granules, so the first section that is not code must start on one.
    pub(super) fn code_end(&self, granule: u64) -> Result<u64, Error> {
        let Some(last) = self.sections.iter().rev().find(|it| it.code()) else {
            return Err(bad("no section of code"));
        };
        let end = last.end();
        if let Some(data) = self.sections.iter().find(|it| !it.code() && it.rva < end) {
            return Err(bad(format!(
                "section {} at RVA {:#x} is data among the code, which ends at {end:#x}: blaze's loader maps code \
                 shared and data as private copies, so blaze-link must put every section of code before any other",
                data.name(),
                data.rva
            )));
        }
        let code_end = end.next_multiple_of(granule);
        if let Some(data) = self.sections.iter().find(|it| !it.code() && it.rva < code_end) {
            return Err(bad(format!(
                "section {} starts at RVA {:#x}, in the {granule:#x}-byte granule where the code ends ({end:#x}): \
                 blaze's loader maps views of whole granules, code shared and data private, so blaze-link must \
                 start the first section that is not code on a {granule:#x}-byte boundary",
                data.name(),
                data.rva
            )));
        }
        Ok(code_end)
    }
}

/// The RVAs the base relocations rebase, in table order.
pub(super) struct Relocations<'a> {
    table: &'a [u8],
    at: usize,
    page: u64,
    entries: &'a [u8],
}

pub(super) fn relocations<'a>(headers: &Headers<'a>) -> Result<Relocations<'a>, Error> {
    let table = match headers.directory(DIR_BASERELOC) {
        None => &[][..],
        Some(directory) => headers
            .raw(directory.rva, directory.size)
            .ok_or_else(|| bad(format!("base relocations at RVA {:#x} not in the image's bytes", directory.rva)))?,
    };
    Ok(Relocations { table, at: 0, page: 0, entries: &[] })
}

impl Relocations<'_> {
    fn fail(&mut self, what: String) -> Option<Result<u64, Error>> {
        self.at = self.table.len();
        self.entries = &[];
        Some(Err(bad(what)))
    }
}

impl Iterator for Relocations<'_> {
    type Item = Result<u64, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some((entry, rest)) = self.entries.split_first_chunk::<2>() {
                self.entries = rest;
                let entry = u16::from_le_bytes(*entry);
                match entry >> 12 {
                    REL_ABSOLUTE => continue,
                    REL_DIR64 => return Some(Ok(self.page + u64::from(entry & 0xfff))),
                    kind => {
                        let page = self.page;
                        return self.fail(format!(
                            "a base relocation of type {kind} in the page at RVA {page:#x}: x86-64 images rebase \
                             with DIR64 (10) only"
                        ));
                    }
                }
            }
            if self.at >= self.table.len() {
                return None;
            }
            let block = &self.table[self.at..];
            let (Some(page), Some(size)) = (u32_at(block, 0), u32_at(block, 4)) else {
                let at = self.at;
                return self.fail(format!("a base relocation block cut short at {at:#x} of the table"));
            };
            let size = size as usize;
            if size < 8 || !size.is_multiple_of(2) || size > block.len() {
                let at = self.at;
                return self.fail(format!("a base relocation block of {size} bytes at {at:#x} of the table"));
            }
            self.page = u64::from(page);
            self.entries = &block[8..size];
            self.at += size;
        }
    }
}

/// A library the image imports from, with its tables.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Library<'a> {
    /// Its name, which a NUL follows in the image's bytes.
    pub(super) name: &'a [u8],
    lookup: u64,
    slots: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Symbol<'a> {
    /// A name, which a NUL follows in the image's bytes.
    Name(&'a [u8]),
    Ordinal(u16),
}

pub(super) fn imports<'a>(headers: &Headers<'a>) -> Result<Vec<Library<'a>>, Error> {
    let Some(directory) = headers.directory(DIR_IMPORT) else { return Ok(Vec::new()) };
    let mut out = Vec::new();
    for index in 0..MOST_IMPORTED {
        let at = directory.rva + IMPORT_DESCRIPTOR * index;
        let descriptor = headers
            .raw(at, IMPORT_DESCRIPTOR)
            .ok_or_else(|| bad(format!("import descriptor {index} at RVA {at:#x} is not in the image's bytes")))?;
        if descriptor.iter().all(|it| *it == 0) {
            return Ok(out);
        }
        let field = |offset| u64::from(u32_at(descriptor, offset).unwrap_or(0));
        let (lookup, name, slots) = (field(0), field(12), field(16));
        let name = headers
            .c_str(name, 256)
            .filter(|it| !it.is_empty())
            .ok_or_else(|| bad(format!("import descriptor {index} names its library at RVA {name:#x}, not a string")))?;
        if slots == 0 {
            return Err(bad(format!(
                "the imports of {} have no address table",
                String::from_utf8_lossy(name)
            )));
        }
        out.push(Library { name, lookup: if lookup != 0 { lookup } else { slots }, slots });
    }
    Err(bad(format!("more than {MOST_IMPORTED} import descriptors")))
}

impl<'a> Library<'a> {
    /// Each slot of its address table (an RVA in data) and the symbol that goes there.
    pub(super) fn symbols<'h>(&self, headers: &'h Headers<'a>) -> Symbols<'a, 'h> {
        Symbols { headers, library: *self, index: 0, done: false }
    }
}

pub(super) struct Symbols<'a, 'h> {
    headers: &'h Headers<'a>,
    library: Library<'a>,
    index: u64,
    done: bool,
}

impl<'a> Symbols<'a, '_> {
    fn step(&self) -> Result<Option<(u64, Symbol<'a>)>, Error> {
        let library = || String::from_utf8_lossy(self.library.name);
        let at = self.library.lookup + 8 * self.index;
        let thunk = self.headers.raw(at, 8).and_then(|it| u64_at(it, 0)).ok_or_else(|| {
            bad(format!("the import lookup table of {} runs past the image's bytes at RVA {at:#x}", library()))
        })?;
        if thunk == 0 {
            return Ok(None);
        }
        let slot = self.library.slots + 8 * self.index;
        self.headers.data(slot, 8, "an import address slot")?;
        let symbol = if thunk & (1 << 63) != 0 {
            if thunk & 0x7fff_ffff_ffff_0000 != 0 {
                return Err(bad(format!("an import by ordinal from {} of {thunk:#x}", library())));
            }
            Symbol::Ordinal(thunk as u16)
        } else {
            let name = (thunk >> 31 == 0)
                .then(|| self.headers.c_str(thunk + 2, 4096))
                .flatten()
                .filter(|it| !it.is_empty())
                .ok_or_else(|| bad(format!("an import from {} names its symbol at {thunk:#x}, not a string", library())))?;
            Symbol::Name(name)
        };
        Ok(Some((slot, symbol)))
    }
}

impl<'a> Iterator for Symbols<'a, '_> {
    type Item = Result<(u64, Symbol<'a>), Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let step = self.step();
        match step {
            Ok(Some(_)) => self.index += 1,
            Ok(None) | Err(_) => self.done = true,
        }
        step.transpose()
    }
}

/// The image's thread-local storage, every address an RVA.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct Tls {
    /// The template: `init` bytes at `start`, then `zero` zero bytes.
    pub(super) start: u64,
    pub(super) init: u64,
    pub(super) zero: u64,
    /// Where the image's `_tls_index` is, the 4 bytes the loader writes its index to.
    pub(super) index: u64,
    pub(super) callbacks: Vec<u64>,
    pub(super) align: u64,
}

impl Tls {
    pub(super) fn len(&self) -> u64 {
        self.init + self.zero
    }
}

pub(super) fn tls(headers: &Headers<'_>) -> Result<Option<Tls>, Error> {
    let Some(directory) = headers.directory(DIR_TLS) else { return Ok(None) };
    let fields = headers
        .raw(directory.rva, TLS_DIRECTORY)
        .filter(|_| directory.size >= TLS_DIRECTORY)
        .ok_or_else(|| bad(format!("a TLS directory at RVA {:#x} of {} bytes", directory.rva, directory.size)))?;
    let field = |at| u64_at(fields, at).unwrap_or(0);
    let (start_va, end_va, index_va, callbacks_va) = (field(0), field(8), field(16), field(24));
    let zero = u64::from(u32_at(fields, 32).unwrap_or(0));
    let characteristics = u32_at(fields, 36).unwrap_or(0);
    let (start, init) = match (start_va, end_va) {
        (0, 0) => (0, 0),
        _ => {
            let start = headers.rva(start_va, "the thread-local template")?;
            let init = end_va
                .checked_sub(start_va)
                .ok_or_else(|| bad(format!("a thread-local template from {start_va:#x} to {end_va:#x}")))?;
            if init != 0 {
                headers.data(start, init, "the thread-local template")?;
                headers
                    .raw(start, init)
                    .ok_or_else(|| bad("the thread-local template is not all in the image's bytes"))?;
            }
            (start, init)
        }
    };
    let index = headers.rva(index_va, "the TLS index")?;
    headers.data(index, 4, "the TLS index")?;
    let mut callbacks = Vec::new();
    if callbacks_va != 0 {
        let mut at = headers.rva(callbacks_va, "the TLS callbacks")?;
        loop {
            let va = headers
                .raw(at, 8)
                .and_then(|it| u64_at(it, 0))
                .ok_or_else(|| bad(format!("the TLS callbacks run past the image's bytes at RVA {at:#x}")))?;
            if va == 0 {
                break;
            }
            let rva = headers.rva(va, "a TLS callback")?;
            if headers.code_at(rva).is_none() {
                return Err(bad(format!("a TLS callback at RVA {rva:#x}, which is in no section of code")));
            }
            if callbacks.len() == MOST_CALLBACKS {
                return Err(bad(format!("more than {MOST_CALLBACKS} TLS callbacks")));
            }
            callbacks.push(rva);
            at += 8;
        }
    }
    let align = match (characteristics >> 20) & 0xf {
        0 => 1,
        power => 1u64 << (power - 1),
    };
    Ok(Some(Tls { start, init, zero, index, callbacks, align }))
}

/// The image's table of functions for unwinding: its RVA and count of entries, each checked
/// to be in order and in the code before `code_end`.
pub(super) fn functions(headers: &Headers<'_>, code_end: u64) -> Result<Option<(u64, u32)>, Error> {
    let Some(directory) = headers.directory(DIR_EXCEPTION) else { return Ok(None) };
    let (at, size) = (directory.rva, directory.size);
    if !size.is_multiple_of(RUNTIME_FUNCTION) || !at.is_multiple_of(4) {
        return Err(bad(format!("a function table at RVA {at:#x} of {size} bytes")));
    }
    headers.data(at, size, "the function table")?;
    let table = headers.raw(at, size).ok_or_else(|| bad("the function table is not in the image's bytes"))?;
    let mut last = 0;
    for (index, entry) in table.chunks_exact(RUNTIME_FUNCTION as usize).enumerate() {
        let field = |offset| u64::from(u32_at(entry, offset).unwrap_or(0));
        let (begin, end, unwind) = (field(0), field(4), field(8));
        if begin < last || end <= begin || end > code_end || (unwind & !1) >= headers.size_of_image {
            return Err(bad(format!(
                "function table entry {index}: {begin:#x}..{end:#x} with unwind information at {unwind:#x}, not in \
                 order in the code before {code_end:#x}"
            )));
        }
        last = end;
    }
    Ok(Some((at, (size / RUNTIME_FUNCTION) as u32)))
}

/// Where the rest of a command line starts after its first argument and the blanks after it,
/// as the C runtime reads `argv[0]`: up to the closing quote when it starts with one (no
/// escapes), else up to a space or tab.
pub(super) fn after_first<T: Copy + Into<u32>>(line: &[T]) -> usize {
    let (quote, space, tab) = (u32::from(b'"'), u32::from(b' '), u32::from(b'\t'));
    let at = |index: usize| line[index].into();
    let mut index = 0;
    if !line.is_empty() && at(0) == quote {
        index = 1;
        while index < line.len() && at(index) != quote {
            index += 1;
        }
        index = (index + 1).min(line.len());
    } else {
        while index < line.len() && at(index) != space && at(index) != tab {
            index += 1;
        }
    }
    while index < line.len() && (at(index) == space || at(index) == tab) {
        index += 1;
    }
    index
}

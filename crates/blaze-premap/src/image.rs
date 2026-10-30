//! A dependency image as the store keeps it: one value, little-endian, fixed-layout records,
//! read in place ([`View`]) once [`View::parse`] has checked every offset, length, name and
//! address against the bytes and the key. The segments' bytes sit at page-aligned offsets,
//! so the memory object every run maps is filled with one copy per segment.
//!
//! ```text
//! 0    magic "BZPREMAP", version u32, format u8, arch u8, 2 zero bytes
//! 16   key (32 bytes)
//! 48   base u64, page u64
//! 64   directory: SECTIONS x (offset u64, length u64)
//! 224  the sections, each 64-byte aligned; segment bytes page-aligned, last
//! ```

use blaze_codegen::Arch;
use blaze_link::Format;

use crate::PremapError;
use crate::key::{ImageKey, arch_tag, format_tag};
use crate::prelink::{Bind, BindKind, InitEntry, Prelinked, Prot, Tls};
use crate::table::{self, Def, SymbolId, TableView};

pub const MAGIC: [u8; 8] = *b"BZPREMAP";
pub const VERSION: u32 = 1;

const SEGMENTS: usize = 0;
const TABLE: usize = 1;
const BINDS: usize = 2;
const LIBRARIES: usize = 3;
const INIT: usize = 4;
const FINI: usize = 5;
const TLS: usize = 6;
const UNWIND: usize = 7;
const STRINGS: usize = 8;
const BYTES: usize = 9;
const SECTIONS: usize = 10;

const DIRECTORY: usize = 64;
const HEADER: usize = DIRECTORY + SECTIONS * 16;

/// addr, size, offset of its bytes, their length, prot, 7 zero bytes.
const SEGMENT: usize = 40;
/// place, addend, name offset u32, name length u32, kind, weak, 6 zero bytes.
const BIND: usize = 32;
/// name offset u32, name length u32, order u32, as_needed u8, 3 zero bytes.
const LIBRARY: usize = 16;
/// rank u32, order 3 x u32, tie (64), at u64, count u32, 4 zero bytes.
const ENTRY: usize = 96;
const TLS_RECORD: usize = 32;
const UNWIND_RECORD: usize = 16;

const _: () = assert!(HEADER == 224);
const _: () = assert!(4 + 12 + 64 + 8 + 4 + 4 == ENTRY);

/// What an image is of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Meta {
    pub key: ImageKey,
    pub format: Format,
    pub arch: Arch,
    pub base: u64,
    pub page: u64,
}

/// `linked` and its table `entries`, as one value.
pub(crate) fn encode(meta: &Meta, linked: &Prelinked, entries: &[(SymbolId, Def)]) -> Result<Vec<u8>, PremapError> {
    let table = table::build(entries)?;
    let mut strings = Vec::new();
    let mut string = |text: &str| -> Result<(u32, u32), PremapError> {
        let at = u32::try_from(strings.len()).map_err(|_| PremapError::new("the image's names pass 4 GiB".to_owned()))?;
        let len = u32::try_from(text.len()).map_err(|_| PremapError::new("a name of 4 GiB".to_owned()))?;
        strings.extend_from_slice(text.as_bytes());
        Ok((at, len))
    };

    let mut binds = Vec::with_capacity(linked.binds.len() * BIND);
    for Bind { place, name, addend, kind, weak } in &linked.binds {
        let (at, len) = string(name)?;
        binds.extend_from_slice(&place.to_le_bytes());
        binds.extend_from_slice(&addend.to_le_bytes());
        binds.extend_from_slice(&at.to_le_bytes());
        binds.extend_from_slice(&len.to_le_bytes());
        binds.push(match kind {
            BindKind::Addr => 1,
            BindKind::TpOff => 2,
        });
        binds.push(*weak as u8);
        binds.extend_from_slice(&[0; 6]);
    }
    let mut libraries = Vec::with_capacity(linked.libraries.len() * LIBRARY);
    for library in &linked.libraries {
        let (at, len) = string(&library.name)?;
        libraries.extend_from_slice(&at.to_le_bytes());
        libraries.extend_from_slice(&len.to_le_bytes());
        libraries.extend_from_slice(&library.order.to_le_bytes());
        libraries.push(library.as_needed as u8);
        libraries.extend_from_slice(&[0; 3]);
    }
    let entries_of = |list: &[InitEntry]| {
        let mut out = Vec::with_capacity(list.len() * ENTRY);
        for entry in list {
            out.extend_from_slice(&entry.rank.to_le_bytes());
            for order in entry.order {
                out.extend_from_slice(&order.to_le_bytes());
            }
            out.extend_from_slice(&entry.tie);
            out.extend_from_slice(&entry.at.to_le_bytes());
            out.extend_from_slice(&entry.count.to_le_bytes());
            out.extend_from_slice(&[0; 4]);
        }
        out
    };
    let init = entries_of(&linked.init);
    let fini = entries_of(&linked.fini);
    let mut tls = Vec::new();
    if let Some(Tls { template, init, size, align }) = linked.tls {
        for it in [template, init, size, align] {
            tls.extend_from_slice(&it.to_le_bytes());
        }
    }
    let mut unwind = Vec::with_capacity(linked.unwind.len() * UNWIND_RECORD);
    for (addr, len) in &linked.unwind {
        unwind.extend_from_slice(&addr.to_le_bytes());
        unwind.extend_from_slice(&len.to_le_bytes());
    }

    let small: [&[u8]; STRINGS + 1] = [&[], &table, &binds, &libraries, &init, &fini, &tls, &unwind, &strings];
    let mut at = HEADER;
    let mut directory = [(0u64, 0u64); SECTIONS];
    // The segment records come first, and their size is known before the bytes' offsets are.
    directory[SEGMENTS] = (at as u64, (linked.segments.len() * SEGMENT) as u64);
    at = align(at + linked.segments.len() * SEGMENT, 64);
    for section in TABLE..=STRINGS {
        directory[section] = (at as u64, small[section].len() as u64);
        at = align(at + small[section].len(), 64);
    }
    let page = meta.page as usize;
    let bytes_from = align(at, page);
    let mut placed = Vec::with_capacity(linked.segments.len());
    let mut end = bytes_from;
    for segment in &linked.segments {
        placed.push(end);
        end = align(end + segment.bytes.len(), page);
    }
    directory[BYTES] = (bytes_from as u64, (end - bytes_from) as u64);

    let mut out = Vec::with_capacity(end);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&[format_tag(meta.format), arch_tag(meta.arch), 0, 0]);
    out.extend_from_slice(&meta.key.0.0);
    out.extend_from_slice(&meta.base.to_le_bytes());
    out.extend_from_slice(&meta.page.to_le_bytes());
    for (offset, len) in directory {
        out.extend_from_slice(&offset.to_le_bytes());
        out.extend_from_slice(&len.to_le_bytes());
    }
    debug_assert_eq!(out.len(), HEADER);
    for (segment, offset) in linked.segments.iter().zip(&placed) {
        out.extend_from_slice(&segment.addr.to_le_bytes());
        out.extend_from_slice(&segment.size.to_le_bytes());
        out.extend_from_slice(&(*offset as u64).to_le_bytes());
        out.extend_from_slice(&(segment.bytes.len() as u64).to_le_bytes());
        out.push(prot_tag(segment.prot));
        out.extend_from_slice(&[0; 7]);
    }
    for section in TABLE..=STRINGS {
        out.resize(directory[section].0 as usize, 0);
        out.extend_from_slice(small[section]);
    }
    for (segment, offset) in linked.segments.iter().zip(&placed) {
        out.resize(*offset, 0);
        out.extend_from_slice(&segment.bytes);
    }
    out.resize(end, 0);
    Ok(out)
}

fn align(at: usize, to: usize) -> usize {
    at.div_ceil(to) * to
}

/// A segment read in place.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SegmentRef<'a> {
    pub addr: u64,
    pub size: u64,
    pub prot: Prot,
    /// Its bytes, then zeroes to `size`.
    pub bytes: &'a [u8],
    /// Where `bytes` start in the image's value: page-aligned.
    pub offset: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BindRef<'a> {
    pub place: u64,
    pub name: &'a str,
    pub addend: i64,
    pub kind: BindKind,
    pub weak: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LibraryRef<'a> {
    pub name: &'a str,
    pub order: u32,
    pub as_needed: bool,
}

/// An image, checked, read in place.
#[derive(Clone, Copy)]
pub struct View<'a> {
    bytes: &'a [u8],
    meta: Meta,
    directory: [(usize, usize); SECTIONS],
    table: TableView<'a>,
}

impl<'a> View<'a> {
    /// Checks `bytes` are an image of `key` and that every reference in them stays inside
    /// them; a store value that is not is refused, naming what is wrong.
    pub fn parse(bytes: &'a [u8], key: ImageKey) -> Result<View<'a>, PremapError> {
        let bad = |what: String| Err(PremapError::new(format!("the dependency image {:?} {what}", key.0)));
        if bytes.len() < HEADER || bytes[..8] != MAGIC {
            return bad("is not one: no header".to_owned());
        }
        if u32_at(bytes, 8) != VERSION {
            return bad(format!("is of version {}, and this blaze reads {VERSION}", u32_at(bytes, 8)));
        }
        let (Some(format), Some(arch)) = (format_of(bytes[12]), arch_of(bytes[13])) else {
            return bad("names no format or target there is".to_owned());
        };
        if bytes[14..16] != [0, 0] {
            return bad("has bytes where its header has none".to_owned());
        }
        if bytes[16..48] != key.0.0 {
            return bad("is under another key than its own".to_owned());
        }
        let (base, page) = (u64_at(bytes, 48), u64_at(bytes, 56));
        if !page.is_power_of_two() || page < 4096 || base % page != 0 {
            return bad(format!("has a page of {page} and a base of {base:#x}"));
        }
        let mut directory = [(0usize, 0usize); SECTIONS];
        for (section, slot) in directory.iter_mut().enumerate() {
            let offset = u64_at(bytes, DIRECTORY + section * 16);
            let len = u64_at(bytes, DIRECTORY + section * 16 + 8);
            let end = offset.checked_add(len).filter(|end| *end <= bytes.len() as u64);
            if end.is_none() || offset < HEADER as u64 {
                return bad(format!("has section {section} outside its bytes"));
            }
            *slot = (offset as usize, len as usize);
        }
        for (section, record) in [
            (SEGMENTS, SEGMENT),
            (BINDS, BIND),
            (LIBRARIES, LIBRARY),
            (INIT, ENTRY),
            (FINI, ENTRY),
            (UNWIND, UNWIND_RECORD),
        ] {
            if directory[section].1 % record != 0 {
                return bad(format!("has section {section} of no whole records"));
            }
        }
        if directory[TLS].1 != 0 && directory[TLS].1 != TLS_RECORD {
            return bad("has a thread-local record of another size".to_owned());
        }
        let table = TableView::parse(&bytes[directory[TABLE].0..][..directory[TABLE].1])?;
        let meta = Meta { key, format, arch, base, page };
        let view = View { bytes, meta, directory, table };

        let mut end = base;
        let (from, len) = directory[BYTES];
        for at in 0..directory[SEGMENTS].1 / SEGMENT {
            let record = view.record(SEGMENTS, SEGMENT, at);
            let (addr, size, offset, file) = (u64_at(record, 0), u64_at(record, 8), u64_at(record, 16), u64_at(record, 24));
            if prot_of(record[32]).is_none() || record[33..40] != [0; 7] {
                return bad(format!("has segment {at} of no protection there is"));
            }
            if addr % page != 0 || addr < end || size == 0 || file > size {
                return bad(format!("has segment {at} at {addr:#x} out of order or of no size"));
            }
            let inside = offset >= from as u64
                && offset % page == 0
                && offset.checked_add(file).is_some_and(|it| it <= (from + len) as u64);
            if !inside {
                return bad(format!("has segment {at}'s bytes outside its bytes"));
            }
            end = match addr.checked_add(size) {
                Some(end) => end,
                None => return bad(format!("has segment {at} past the end of the address space")),
            };
        }
        let strings = view.section(STRINGS);
        let name = |at: u32, len: u32| -> Option<&str> {
            let range = strings.get(at as usize..(at as usize).checked_add(len as usize)?)?;
            std::str::from_utf8(range).ok()
        };
        for at in 0..directory[BINDS].1 / BIND {
            let record = view.record(BINDS, BIND, at);
            let place = u64_at(record, 0);
            if name(u32_at(record, 16), u32_at(record, 20)).is_none()
                || !matches!(record[24], 1 | 2)
                || record[25] > 1
                || record[26..32] != [0; 6]
            {
                return bad(format!("has bind {at} that is not one"));
            }
            if place % 8 != 0 || !view.inside(place, 8, true) {
                return bad(format!("binds a cell at {place:#x}, outside its writable segments"));
            }
        }
        for at in 0..directory[LIBRARIES].1 / LIBRARY {
            let record = view.record(LIBRARIES, LIBRARY, at);
            if name(u32_at(record, 0), u32_at(record, 4)).is_none() || record[12] > 1 || record[13..16] != [0; 3] {
                return bad(format!("has library {at} that is not one"));
            }
        }
        for section in [INIT, FINI] {
            for at in 0..directory[section].1 / ENTRY {
                let record = view.record(section, ENTRY, at);
                let (cell, count) = (u64_at(record, 80), u32_at(record, 88));
                if record[92..96] != [0; 4] || cell % 8 != 0 || !view.inside(cell, u64::from(count) * 8, false) {
                    return bad(format!("has an init or fini piece at {cell:#x} outside its segments"));
                }
            }
        }
        if let Some(tls) = view.tls() {
            if tls.init > tls.size || !tls.align.is_power_of_two() || (tls.init > 0 && !view.inside(tls.template, tls.init, false)) {
                return bad("has a thread-local template that is not one".to_owned());
            }
        }
        for (addr, len) in view.unwind() {
            if !view.inside(addr, len, false) {
                return bad(format!("has call frame information at {addr:#x} outside its segments"));
            }
        }
        Ok(view)
    }

    pub fn meta(&self) -> Meta {
        self.meta
    }

    /// The image's value, whole.
    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    pub fn table(&self) -> TableView<'a> {
        self.table
    }

    /// One past the image's last byte in the address space.
    pub fn end(&self) -> u64 {
        self.segments().last().map_or(self.meta.base, |it| it.addr + it.size)
    }

    pub fn segments(&self) -> impl Iterator<Item = SegmentRef<'a>> + 'a {
        let view = *self;
        (0..self.directory[SEGMENTS].1 / SEGMENT).map(move |at| {
            let record = view.record(SEGMENTS, SEGMENT, at);
            let (offset, len) = (u64_at(record, 16), u64_at(record, 24));
            SegmentRef {
                addr: u64_at(record, 0),
                size: u64_at(record, 8),
                prot: prot_of(record[32]).expect("checked by parse"),
                bytes: &view.bytes[offset as usize..(offset + len) as usize],
                offset,
            }
        })
    }

    pub fn binds(&self) -> impl Iterator<Item = BindRef<'a>> + 'a {
        let view = *self;
        (0..self.directory[BINDS].1 / BIND).map(move |at| {
            let record = view.record(BINDS, BIND, at);
            BindRef {
                place: u64_at(record, 0),
                addend: u64_at(record, 8) as i64,
                name: view.string(u32_at(record, 16), u32_at(record, 20)),
                kind: if record[24] == 1 { BindKind::Addr } else { BindKind::TpOff },
                weak: record[25] == 1,
            }
        })
    }

    pub fn libraries(&self) -> impl Iterator<Item = LibraryRef<'a>> + 'a {
        let view = *self;
        (0..self.directory[LIBRARIES].1 / LIBRARY).map(move |at| {
            let record = view.record(LIBRARIES, LIBRARY, at);
            LibraryRef {
                name: view.string(u32_at(record, 0), u32_at(record, 4)),
                order: u32_at(record, 8),
                as_needed: record[12] == 1,
            }
        })
    }

    pub fn init(&self) -> impl Iterator<Item = InitEntry> + 'a {
        self.entries(INIT)
    }

    pub fn fini(&self) -> impl Iterator<Item = InitEntry> + 'a {
        self.entries(FINI)
    }

    pub fn tls(&self) -> Option<Tls> {
        let section = self.section(TLS);
        (!section.is_empty()).then(|| Tls {
            template: u64_at(section, 0),
            init: u64_at(section, 8),
            size: u64_at(section, 16),
            align: u64_at(section, 24),
        })
    }

    pub fn unwind(&self) -> impl Iterator<Item = (u64, u64)> + 'a {
        let view = *self;
        (0..self.directory[UNWIND].1 / UNWIND_RECORD).map(move |at| {
            let record = view.record(UNWIND, UNWIND_RECORD, at);
            (u64_at(record, 0), u64_at(record, 8))
        })
    }

    fn entries(&self, section: usize) -> impl Iterator<Item = InitEntry> + 'a {
        let view = *self;
        (0..self.directory[section].1 / ENTRY).map(move |at| {
            let record = view.record(section, ENTRY, at);
            InitEntry {
                rank: u32_at(record, 0),
                order: [u32_at(record, 4), u32_at(record, 8), u32_at(record, 12)],
                tie: record[16..80].try_into().expect("64 bytes"),
                at: u64_at(record, 80),
                count: u32_at(record, 88),
            }
        })
    }

    fn section(&self, section: usize) -> &'a [u8] {
        let (offset, len) = self.directory[section];
        &self.bytes[offset..offset + len]
    }

    fn record(&self, section: usize, size: usize, at: usize) -> &'a [u8] {
        &self.section(section)[at * size..(at + 1) * size]
    }

    fn string(&self, at: u32, len: u32) -> &'a str {
        let strings = self.section(STRINGS);
        std::str::from_utf8(&strings[at as usize..at as usize + len as usize]).expect("checked by parse")
    }

    /// Whether `[addr, addr + len)` is inside one segment (a writable one when `writable`).
    fn inside(&self, addr: u64, len: u64, writable: bool) -> bool {
        self.segments().any(|it| {
            addr >= it.addr && addr.checked_add(len).is_some_and(|end| end <= it.addr + it.size) && (!writable || it.prot == Prot::Data)
        })
    }

    /// The symbol `id` is, if the image defines it.
    pub fn get(&self, id: &SymbolId) -> Option<Def> {
        self.table.get(id)
    }
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4 bytes"))
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"))
}

fn prot_tag(prot: Prot) -> u8 {
    match prot {
        Prot::Code => 1,
        Prot::ReadOnly => 2,
        Prot::Data => 3,
    }
}

fn prot_of(tag: u8) -> Option<Prot> {
    match tag {
        1 => Some(Prot::Code),
        2 => Some(Prot::ReadOnly),
        3 => Some(Prot::Data),
        _ => None,
    }
}

fn format_of(tag: u8) -> Option<Format> {
    [Format::Elf, Format::MachO, Format::Pe, Format::Wasm].into_iter().find(|it| format_tag(*it) == tag)
}

fn arch_of(tag: u8) -> Option<Arch> {
    [Arch::X86_64, Arch::Aarch64, Arch::Wasm32].into_iter().find(|it| arch_tag(*it) == tag)
}

//! `__unwind_info`: the table libunwind and dyld (`_dyld_find_unwind_sections`) look a pc up
//! in, in ld64's format (`<mach-o/compact_unwind_encoding.h>`), version 1, with regular
//! second-level pages only.
//!
//! Every function with an FDE in `__eh_frame` gets the DWARF-mode encoding pointing at it;
//! every `__compact_unwind` entry gets its own encoding, with its personality and LSDA;
//! every other function, and the first address after a covered range where nothing starts,
//! gets encoding 0.

use std::collections::BTreeMap;

use blaze_codegen::Arch;
use gimli::read::{BaseAddresses, CieOrFde, EhFrame, UnwindSection};

use crate::LinkError;
use crate::model::{Class, Placed, Region};

const VERSION: u32 = 1;
const HEADER: u64 = 28;
const COMMON_MAX: usize = 127;
const PERSONALITY_MAX: usize = 3;
const PAGE: u64 = 4096;
const PAGE_HEADER: u64 = 8;
const PAGE_ENTRY: u64 = 8;
const PAGE_ENTRIES: usize = ((PAGE - PAGE_HEADER) / PAGE_ENTRY) as usize;
const SECOND_LEVEL_REGULAR: u32 = 2;
const INDEX_ENTRY: u64 = 12;
const LSDA_ENTRY: u64 = 8;
const COMPACT_ENTRY: u64 = 32;

const MODE_MASK: u32 = 0x0F00_0000;
const DWARF_OFFSET_MASK: u32 = 0x00FF_FFFF;
const HAS_LSDA: u32 = 0x4000_0000;
const PERSONALITY_MASK: u32 = 0x3000_0000;
const PERSONALITY_SHIFT: u32 = 28;
const ARM64_MODE_DWARF: u32 = 0x0300_0000;
const X86_64_MODE_DWARF: u32 = 0x0400_0000;

/// The most bytes `__unwind_info` can take for `functions` functions and `compact` bytes of
/// `__compact_unwind` input (32 bytes an entry): the region is sized by it before placement.
pub(super) fn bound(functions: u64, compact: u64) -> u64 {
    let compact = compact.div_ceil(COMPACT_ENTRY);
    let entries = 2 * (functions + compact);
    let pages = entries.div_ceil(PAGE_ENTRIES as u64);
    HEADER
        + 4 * COMMON_MAX as u64
        + 4 * PERSONALITY_MAX as u64
        + INDEX_ENTRY * (pages + 1)
        + LSDA_ENTRY * compact
        + PAGE_HEADER * pages
        + PAGE_ENTRY * entries
}

/// `__unwind_info` for `placed`, whose `__TEXT` starts at `placed.plan.base`.
pub(super) fn build(placed: &Placed<'_>, arch: Arch) -> Result<Vec<u8>, LinkError> {
    let dwarf_mode = match arch {
        Arch::Aarch64 => ARM64_MODE_DWARF,
        Arch::X86_64 => X86_64_MODE_DWARF,
        Arch::Wasm32 => {
            return Err(error("__unwind_info: wasm32 has no compact unwind encoding".into()));
        }
    };
    let base = placed.plan.base;
    let fdes = fdes(placed)?;

    let mut entries = Vec::new();
    let mut ends = Vec::new();
    let mut personalities: Vec<(Held, u32)> = Vec::new();
    let mut compact_bytes = 0u64;
    let class = super::compact_unwind();
    for (index, region) in placed.regions.iter().enumerate().filter(|(_, it)| it.class == class) {
        let bytes = region_bytes(placed, index, region, "__compact_unwind")?;
        if bytes.len() as u64 % COMPACT_ENTRY != 0 {
            return Err(error(format!(
                "__compact_unwind at {:#x}: {} bytes, not a whole number of 32-byte entries",
                region.addr,
                bytes.len()
            )));
        }
        compact_bytes += bytes.len() as u64;
        let binds: BTreeMap<u64, Held> = placed
            .binds
            .iter()
            .filter(|it| (region.addr..region.addr + region.size).contains(&it.addr))
            .map(|it| (it.addr, Held::Import { import: it.import, addend: it.addend }))
            .collect();
        for (n, entry) in bytes.chunks_exact(COMPACT_ENTRY as usize).enumerate() {
            let function = u64_at(entry, 0);
            if function == 0 {
                continue;
            }
            let length = u64::from(u32_at(entry, 8));
            let encoding = u32_at(entry, 12);
            let personality = u64_at(entry, 16);
            let lsda = u64_at(entry, 24);
            let end = function.checked_add(length).ok_or_else(|| {
                error(format!(
                    "__compact_unwind: {} has length {length:#x}, past the end of the address \
                     space",
                    named(placed, function)
                ))
            })?;
            let kind = if encoding & MODE_MASK == dwarf_mode {
                let Some(fde) = fdes.get(&function) else {
                    return Err(error(format!(
                        "__compact_unwind: the entry of {} says DWARF mode ({encoding:#010x}), \
                         but no FDE in __eh_frame starts there",
                        named(placed, function)
                    )));
                };
                Kind::Dwarf { fde: fde.offset }
            } else {
                let mut encoding = encoding & !(PERSONALITY_MASK | HAS_LSDA);
                let cell = region.addr + COMPACT_ENTRY * n as u64 + 16;
                let held = match binds.get(&cell) {
                    Some(bind) => Some(*bind),
                    None => (personality != 0).then_some(Held::Address(personality)),
                };
                if let Some(held) = held {
                    let index = match personalities.iter().position(|(it, _)| *it == held) {
                        Some(index) => index,
                        None => {
                            if personalities.len() == PERSONALITY_MAX {
                                return Err(error(format!(
                                    "__unwind_info: {} has a fourth distinct personality \
                                     (held at {cell:#x}); the format has room for \
                                     {PERSONALITY_MAX}",
                                    named(placed, function)
                                )));
                            }
                            let cell = offset(base, cell, "a personality's cell")?;
                            personalities.push((held, cell));
                            personalities.len() - 1
                        }
                    };
                    encoding |= ((index as u32) + 1) << PERSONALITY_SHIFT;
                }
                let lsda = (lsda != 0).then_some(lsda);
                if lsda.is_some() {
                    encoding |= HAS_LSDA;
                }
                Kind::Compact { encoding, lsda }
            };
            entries.push(Entry { addr: function, rank: 0, kind });
            ends.push(end);
        }
    }
    for (&start, fde) in &fdes {
        entries.push(Entry { addr: start, rank: 1, kind: Kind::Dwarf { fde: fde.offset } });
        ends.push(fde.end);
    }
    let mut last = 0u64;
    for function in placed.functions {
        entries.push(Entry { addr: function.addr, rank: 2, kind: Kind::None });
        last = last.max(function.addr.saturating_add(function.size));
    }
    for &end in &ends {
        entries.push(Entry { addr: end, rank: 3, kind: Kind::None });
        last = last.max(end);
    }
    entries.sort_by_key(|it| (it.addr, it.rank));
    entries.dedup_by_key(|it| it.addr);
    if let Some(entry) = entries.last() {
        last = last.max(entry.addr);
    }

    let mut rows: Vec<Row> = Vec::with_capacity(entries.len());
    for entry in &entries {
        let (encoding, lsda) = match entry.kind {
            Kind::Compact { encoding, lsda } => (encoding, lsda),
            Kind::Dwarf { fde } => {
                let Some(fde) = u32::try_from(fde).ok().filter(|it| *it <= DWARF_OFFSET_MASK)
                else {
                    return Err(error(format!(
                        "__unwind_info: the FDE of {} is at {fde:#x} in __eh_frame, past the 24 \
                         bits DWARF mode has for it",
                        named(placed, entry.addr)
                    )));
                };
                (dwarf_mode | fde, None)
            }
            Kind::None => (0, None),
        };
        if let Some(previous) = rows.last()
            && previous.encoding == encoding
            && previous.lsda.is_none()
            && lsda.is_none()
        {
            continue;
        }
        let lsda = match lsda {
            Some(lsda) => Some(offset(base, lsda, "an LSDA")?),
            None => None,
        };
        rows.push(Row { function: offset(base, entry.addr, "a function")?, encoding, lsda });
    }
    let sentinel = offset(base, last, "the end of the last function")?;

    let mut counts: BTreeMap<u32, u64> = BTreeMap::new();
    for row in &rows {
        *counts.entry(row.encoding).or_default() += 1;
    }
    let mut common: Vec<(u32, u64)> = counts.into_iter().collect();
    common.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    common.truncate(COMMON_MAX);

    let pages: Vec<&[Row]> = rows.chunks(PAGE_ENTRIES).collect();
    let lsdas: Vec<(u32, u32)> =
        rows.iter().filter_map(|row| row.lsda.map(|lsda| (row.function, lsda))).collect();
    let common_at = HEADER;
    let personalities_at = common_at + 4 * common.len() as u64;
    let index_at = personalities_at + 4 * personalities.len() as u64;
    let lsda_at = index_at + INDEX_ENTRY * (pages.len() as u64 + 1);
    let pages_at = lsda_at + LSDA_ENTRY * lsdas.len() as u64;
    let len = pages_at
        + pages.iter().map(|it| PAGE_HEADER + PAGE_ENTRY * it.len() as u64).sum::<u64>();
    let limit = bound(placed.functions.len() as u64, compact_bytes);
    if len > limit {
        return Err(error(format!(
            "__unwind_info: {} entries take {len} bytes, past the {limit} sized for {} functions \
             and {} compact unwind entries: an FDE in __eh_frame starts where no placed \
             function does",
            rows.len(),
            placed.functions.len(),
            compact_bytes / COMPACT_ENTRY
        )));
    }
    if len > u64::from(u32::MAX) {
        return Err(error(format!(
            "__unwind_info: {len} bytes, past the 32-bit offsets the format has"
        )));
    }
    let small = |value: u64| value as u32;

    let mut out = Vec::with_capacity(len as usize);
    for value in [
        VERSION,
        small(common_at),
        common.len() as u32,
        small(personalities_at),
        personalities.len() as u32,
        small(index_at),
        pages.len() as u32 + 1,
    ] {
        out.extend_from_slice(&value.to_le_bytes());
    }
    for (encoding, _) in &common {
        out.extend_from_slice(&encoding.to_le_bytes());
    }
    for (_, cell) in &personalities {
        out.extend_from_slice(&cell.to_le_bytes());
    }
    let mut page_at = pages_at;
    let mut lsda_index = 0usize;
    for page in &pages {
        let first = page[0].function;
        while lsdas.get(lsda_index).is_some_and(|it| it.0 < first) {
            lsda_index += 1;
        }
        for value in [first, small(page_at), small(lsda_at + LSDA_ENTRY * lsda_index as u64)] {
            out.extend_from_slice(&value.to_le_bytes());
        }
        page_at += PAGE_HEADER + PAGE_ENTRY * page.len() as u64;
    }
    for value in [sentinel, 0, small(lsda_at + LSDA_ENTRY * lsdas.len() as u64)] {
        out.extend_from_slice(&value.to_le_bytes());
    }
    for (function, lsda) in &lsdas {
        out.extend_from_slice(&function.to_le_bytes());
        out.extend_from_slice(&lsda.to_le_bytes());
    }
    for page in &pages {
        out.extend_from_slice(&SECOND_LEVEL_REGULAR.to_le_bytes());
        out.extend_from_slice(&(PAGE_HEADER as u16).to_le_bytes());
        out.extend_from_slice(&(page.len() as u16).to_le_bytes());
        for row in *page {
            out.extend_from_slice(&row.function.to_le_bytes());
            out.extend_from_slice(&row.encoding.to_le_bytes());
        }
    }
    debug_assert_eq!(out.len() as u64, len);
    Ok(out)
}

/// What a personality cell holds: the loader slides an address of the image, or binds an
/// import.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Held {
    Address(u64),
    Import { import: u32, addend: i64 },
}

/// A start address and what covers it; of one address the lowest `rank` wins: a compact
/// entry (0), an FDE (1), a function without either (2), the end of a covered range (3).
struct Entry {
    addr: u64,
    rank: u8,
    kind: Kind,
}

#[derive(Clone, Copy)]
enum Kind {
    Compact { encoding: u32, lsda: Option<u64> },
    /// The FDE's offset in `__eh_frame`.
    Dwarf { fde: u64 },
    None,
}

/// A second-level entry, offsets from the image's base.
struct Row {
    function: u32,
    encoding: u32,
    lsda: Option<u32>,
}

struct Fde {
    offset: u64,
    end: u64,
}

/// Every FDE of `__eh_frame`, by the start of the function it covers; of two at one address,
/// the first.
fn fdes(placed: &Placed<'_>) -> Result<BTreeMap<u64, Fde>, LinkError> {
    let mut fdes = BTreeMap::new();
    let Some((index, region)) = placed.region(&Class::EhFrame) else {
        return Ok(fdes);
    };
    let bytes = region_bytes(placed, index, region, "__eh_frame")?;
    let mut section = EhFrame::new(bytes, gimli::LittleEndian);
    section.set_address_size(8);
    let bases = BaseAddresses::default().set_eh_frame(region.addr);
    let bad = |at: String, err: gimli::Error| {
        error(format!("__eh_frame at {:#x}: {at}: {err}", region.addr))
    };
    let mut entries = section.entries(&bases);
    while let Some(entry) = entries.next().map_err(|err| bad("reading an entry".into(), err))? {
        let CieOrFde::Fde(partial) = entry else {
            continue;
        };
        let at = partial.offset() as u64;
        let fde = partial
            .parse(|section, bases, offset| section.cie_from_offset(bases, offset))
            .map_err(|err| bad(format!("the FDE at offset {at:#x}"), err))?;
        let start = fde.initial_address();
        if start == 0 {
            continue;
        }
        let end = start.checked_add(fde.len()).ok_or_else(|| {
            error(format!(
                "__eh_frame at {:#x}: the FDE at offset {at:#x} covers past the end of the \
                 address space",
                region.addr
            ))
        })?;
        fdes.entry(start).or_insert(Fde { offset: at, end });
    }
    Ok(fdes)
}

fn region_bytes<'p>(
    placed: &'p Placed<'_>,
    index: usize,
    region: &Region,
    name: &str,
) -> Result<&'p [u8], LinkError> {
    let bytes = placed.bytes(index);
    usize::try_from(region.size).ok().and_then(|size| bytes.get(..size)).ok_or_else(|| {
        error(format!(
            "{name} at {:#x}: {} bytes placed, {} in its contents",
            region.addr,
            region.size,
            bytes.len()
        ))
    })
}

/// `addr`'s offset from the image's first byte, which every offset in the table is.
fn offset(base: u64, addr: u64, what: &str) -> Result<u32, LinkError> {
    addr.checked_sub(base).and_then(|it| u32::try_from(it).ok()).ok_or_else(|| {
        error(format!(
            "__unwind_info: {what} at {addr:#x} is not within 4 GiB above the image's base {base:#x}"
        ))
    })
}

fn named(placed: &Placed<'_>, addr: u64) -> String {
    match placed.functions.iter().find(|it| it.addr == addr).and_then(|it| it.name.as_deref()) {
        Some(name) => format!("`{name}` at {addr:#x}"),
        None => format!("the function at {addr:#x}"),
    }
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4 bytes"))
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"))
}

fn error(what: String) -> LinkError {
    LinkError { function: None, what }
}

#[cfg(test)]
mod tests {
    use blaze_codegen::Arch;
    use gimli::write::{Address, CommonInformationEntry, EhFrame, EndianVec, FrameDescriptionEntry, FrameTable};

    use super::{ARM64_MODE_DWARF, HAS_LSDA, PERSONALITY_MASK, X86_64_MODE_DWARF, bound, build, u32_at};
    use crate::model::{Bind, Class, Function, Placed, Plan, Prot, Region};
    use crate::output::{Debug, Format, Output, OutputKind};

    const BASE: u64 = 0x1_0000_0000;
    const TEXT: u64 = BASE + 0x4000;
    const EH_FRAME: u64 = BASE + 0x8000;
    const COMPACT: u64 = BASE + 0xc000;
    const LSDA: u64 = BASE + 0x9000;
    const PERSONALITY: u64 = TEXT + 0x800;
    const FRAME: u32 = 0x0400_0000;
    const FRAMELESS: u32 = 0x0200_1000;

    fn output(arch: Arch) -> Output {
        Output {
            format: Format::MachO,
            arch,
            kind: OutputKind::Executable { entry: "main".into() },
            interpreter: None,
            platform: None,
            incremental: false,
            debug: Debug::None,
            live: None,
            tags: 0,
            stack: None,
        }
    }

    fn plan() -> Plan {
        Plan {
            base: BASE,
            headers: 0x4000,
            page: 0x4000,
            indexed_code: false,
            got_by_library: false,
            tls_descriptors: true,
        }
    }

    fn region(class: Class, addr: u64, size: u64) -> Region {
        Region { class, addr, capacity: size, size, prot: Prot::R, align: 8 }
    }

    fn function(addr: u64, size: u64, name: &str) -> Function {
        Function { addr, size, fde: None, windows: None, name: Some(name.into()) }
    }

    /// An `__eh_frame` at [`EH_FRAME`] of one CIE and an FDE for each range, its addresses
    /// pc-relative; and each FDE's offset in it.
    fn eh_frame(ranges: &[(u64, u32)]) -> (Vec<u8>, Vec<u64>) {
        let encoding = gimli::Encoding { address_size: 8, format: gimli::Format::Dwarf32, version: 1 };
        let mut cie = CommonInformationEntry::new(encoding, 4, -8, gimli::Register(30));
        cie.fde_address_encoding = gimli::DW_EH_PE_pcrel | gimli::DW_EH_PE_sdata4;
        let mut table = FrameTable::default();
        let cie = table.add_cie(cie);
        for &(addr, len) in ranges {
            table.add_fde(cie, FrameDescriptionEntry::new(Address::Constant(addr.wrapping_sub(EH_FRAME)), len));
        }
        let mut section = EhFrame(EndianVec::new(gimli::LittleEndian));
        table.write_eh_frame(&mut section).unwrap();
        let bytes = section.0.into_vec();
        let mut offsets = Vec::new();
        let mut at = 0;
        while at < bytes.len() {
            let length = u32_at(&bytes, at) as usize;
            if u32_at(&bytes, at + 4) != 0 {
                offsets.push(at as u64);
            }
            at += 4 + length;
        }
        (bytes, offsets)
    }

    fn compact(entries: &[(u64, u32, u32, u64, u64)]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for &(function, length, encoding, personality, lsda) in entries {
            bytes.extend_from_slice(&function.to_le_bytes());
            bytes.extend_from_slice(&length.to_le_bytes());
            bytes.extend_from_slice(&encoding.to_le_bytes());
            bytes.extend_from_slice(&personality.to_le_bytes());
            bytes.extend_from_slice(&lsda.to_le_bytes());
        }
        bytes
    }

    /// The table as libunwind reads it.
    struct Table<'a> {
        bytes: &'a [u8],
    }

    impl Table<'_> {
        fn word(&self, at: u32) -> u32 {
            u32_at(self.bytes, at as usize)
        }

        fn half(&self, at: u32) -> u16 {
            u16::from_le_bytes(self.bytes[at as usize..at as usize + 2].try_into().unwrap())
        }

        fn personalities(&self) -> Vec<u32> {
            (0..self.word(16)).map(|i| self.word(self.word(12) + 4 * i)).collect()
        }

        fn common(&self) -> Vec<u32> {
            (0..self.word(8)).map(|i| self.word(self.word(4) + 4 * i)).collect()
        }

        /// Each first-level entry: function offset, page offset, LSDA index offset.
        fn index(&self) -> Vec<(u32, u32, u32)> {
            let at = self.word(20);
            (0..self.word(24))
                .map(|i| (self.word(at + 12 * i), self.word(at + 12 * i + 4), self.word(at + 12 * i + 8)))
                .collect()
        }

        /// Every second-level entry, in order.
        fn entries(&self) -> Vec<(u32, u32)> {
            let index = self.index();
            let mut entries = Vec::new();
            for &(_, page, _) in &index[..index.len() - 1] {
                assert_eq!(self.word(page), 2, "a regular page");
                let start = page + u32::from(self.half(page + 4));
                for i in 0..u32::from(self.half(page + 6)) {
                    entries.push((self.word(start + 8 * i), self.word(start + 8 * i + 4)));
                }
            }
            entries
        }

        /// The encoding libunwind finds for `pc`, an offset from the base, and the LSDA's
        /// offset when the encoding has one.
        fn lookup(&self, pc: u32) -> Option<(u32, Option<u32>)> {
            let index = self.index();
            let page = index.windows(2).position(|it| it[0].0 <= pc && pc < it[1].0)?;
            let (_, at, _) = index[page];
            let start = at + u32::from(self.half(at + 4));
            let count = u32::from(self.half(at + 6));
            let entries: Vec<(u32, u32)> =
                (0..count).map(|i| (self.word(start + 8 * i), self.word(start + 8 * i + 4))).collect();
            let found = entries.iter().rposition(|it| it.0 <= pc)?;
            let (function, encoding) = entries[found];
            if encoding & HAS_LSDA == 0 {
                return Some((encoding, None));
            }
            let lsdas = (index[page].2..index[page + 1].2)
                .step_by(8)
                .map(|at| (self.word(at), self.word(at + 4)))
                .collect::<Vec<_>>();
            let lsda = lsdas.iter().find(|it| it.0 == function).map(|it| it.1);
            assert!(lsda.is_some(), "an LSDA for the function at {function:#x}");
            Some((encoding, lsda))
        }
    }

    fn off(addr: u64) -> u32 {
        (addr - BASE) as u32
    }

    #[test]
    fn each_function_gets_its_encoding() {
        let f0 = TEXT;
        let f1 = TEXT + 0x40;
        let f2 = TEXT + 0x80;
        let f3 = TEXT + 0xc0;
        let f4 = TEXT + 0xd0;
        let f5 = TEXT + 0x100;
        let f6 = TEXT + 0x110;
        let f7 = TEXT + 0x200;
        let (eh, fde) = eh_frame(&[(f0, 0x40), (f2, 0x20), (f1, 0x40), (f4, 0x30)]);
        let cu = compact(&[
            (f1, 0x40, FRAME, PERSONALITY, LSDA),
            (0, 0x40, FRAME, PERSONALITY, LSDA + 0x40),
            (f4, 0x30, ARM64_MODE_DWARF, 0, 0),
            (f5, 0x10, FRAMELESS, PERSONALITY, 0),
            (f6, 0x10, FRAMELESS, PERSONALITY, 0),
        ]);
        let regions = [
            region(Class::Text, TEXT, 0x1000),
            region(Class::EhFrame, EH_FRAME, eh.len() as u64),
            region(super::super::compact_unwind(), COMPACT, cu.len() as u64),
        ];
        let contents: [&[u8]; 3] = [&[], &eh, &cu];
        let functions = [
            function(f0, 0x40, "f0"),
            function(f1, 0x40, "f1"),
            function(f2, 0x20, "f2"),
            function(f3, 0x10, "f3"),
            function(f4, 0x30, "f4"),
            function(f5, 0x10, "f5"),
            function(f6, 0x10, "f6"),
            function(f7, 0x10, "f7"),
        ];
        let output = output(Arch::Aarch64);
        let plan = plan();
        let placed = Placed {
            output: &output,
            plan: &plan,
            regions: &regions,
            contents: &contents,
            libraries: &[],
            imports: &[],
            got: &[],
            rebase: &[COMPACT + 16, COMPACT + 32 * 3 + 16, COMPACT + 32 * 4 + 16],
            binds: &[],
            exports: &[],
            functions: &functions,
            entry: None,
            tls: None,
            id: [0; 16],
            debug: &[],
        };
        let bytes = build(&placed, Arch::Aarch64).unwrap();
        assert!(bytes.len() as u64 <= bound(functions.len() as u64, cu.len() as u64));
        let table = Table { bytes: &bytes };
        assert_eq!(table.word(0), 1, "version 1");

        assert_eq!(table.personalities(), vec![off(COMPACT + 16)], "one personality, by its cell");
        let personality = 1 << 28;
        let dwarf = |n: usize| ARM64_MODE_DWARF | fde[n] as u32;

        assert_eq!(table.lookup(off(f0)), Some((dwarf(0), None)));
        assert_eq!(table.lookup(off(f0) + 0x3c), Some((dwarf(0), None)));
        assert_eq!(
            table.lookup(off(f1)),
            Some((FRAME | personality | HAS_LSDA, Some(off(LSDA)))),
            "the compact entry wins over the FDE of the same function"
        );
        assert_eq!(table.lookup(off(f2)), Some((dwarf(1), None)));
        assert_eq!(table.lookup(off(f2) + 0x20), Some((0, None)), "the end marker after f2");
        assert_eq!(table.lookup(off(f3)), Some((0, None)));
        assert_eq!(table.lookup(off(f4)), Some((dwarf(3), None)), "a DWARF-mode compact entry");
        assert_eq!(table.lookup(off(f5)), Some((FRAMELESS | personality, None)));
        assert_eq!(table.lookup(off(f6) + 4), Some((FRAMELESS | personality, None)));
        assert_eq!(table.lookup(off(f6) + 0x10), Some((0, None)), "the end marker after f6");
        assert_eq!(table.lookup(off(f7)), Some((0, None)));
        assert_eq!(table.lookup(off(f7) + 0x10), None, "past the sentinel");

        let entries = table.entries();
        assert!(entries.windows(2).all(|it| it[0].0 < it[1].0), "sorted, no duplicates");
        assert!(
            entries.windows(2).all(|it| it[0].1 != it[1].1 || it[1].1 & HAS_LSDA != 0),
            "consecutive entries with one encoding merged"
        );
        assert!(!entries.iter().any(|it| it.0 == off(f6)), "f6 merged into f5");
        assert!(entries.iter().all(|it| it.0 != 0), "the dead-stripped entry left out");
        assert!(entries.iter().all(|it| it.1 & PERSONALITY_MASK != 2 << 28));

        let index = table.index();
        assert_eq!(index.len(), 2, "one page and the sentinel");
        assert_eq!(index[0].0, off(f0));
        let sentinel = index[1];
        assert_eq!(sentinel.0, off(f7) + 0x10, "the sentinel is the end of the last function");
        assert_eq!(sentinel.1, 0);
        assert_eq!(sentinel.2 - index[0].2, 8, "one LSDA entry");
        assert_eq!(table.word(index[0].2), off(f1));
        assert_eq!(table.word(index[0].2 + 4), off(LSDA));

        let common = table.common();
        assert!(common.len() <= 127);
        assert_eq!(common[0], 0, "encoding 0 is the most frequent");
    }

    #[test]
    fn x86_64_dwarf_mode() {
        let (eh, fde) = eh_frame(&[(TEXT, 0x10), (TEXT + 0x10, 0x10)]);
        let regions = [region(Class::Text, TEXT, 0x20), region(Class::EhFrame, EH_FRAME, eh.len() as u64)];
        let contents: [&[u8]; 2] = [&[], &eh];
        let functions = [function(TEXT, 0x10, "a"), function(TEXT + 0x10, 0x10, "b")];
        let output = output(Arch::X86_64);
        let plan = plan();
        let placed = placed(&output, &plan, &regions, &contents, &functions, &[]);
        let bytes = build(&placed, Arch::X86_64).unwrap();
        assert!(bytes.len() as u64 <= bound(2, 0));
        let table = Table { bytes: &bytes };
        assert_eq!(table.lookup(off(TEXT)), Some((X86_64_MODE_DWARF | fde[0] as u32, None)));
        assert_eq!(table.lookup(off(TEXT) + 0x18), Some((X86_64_MODE_DWARF | fde[1] as u32, None)));
        assert_eq!(table.lookup(off(TEXT) + 0x20), None);
        assert!(table.personalities().is_empty());
    }

    #[test]
    fn many_functions_span_pages() {
        let functions: Vec<Function> =
            (0..2000).map(|i| function(TEXT + 0x10 * i, 0x10, "f")).collect();
        let entries: Vec<(u64, u32, u32, u64, u64)> = (0..2000)
            .map(|i| (TEXT + 0x10 * i, 0x10, FRAMELESS + (i as u32 % 2) * 0x1000, 0, if i % 3 == 0 { LSDA + i } else { 0 }))
            .collect();
        let cu = compact(&entries);
        let regions = [region(Class::Text, TEXT, 0x10 * 2000), region(super::super::compact_unwind(), COMPACT, cu.len() as u64)];
        let contents: [&[u8]; 2] = [&[], &cu];
        let output = output(Arch::Aarch64);
        let plan = plan();
        let placed = placed(&output, &plan, &regions, &contents, &functions, &[]);
        let bytes = build(&placed, Arch::Aarch64).unwrap();
        assert!(bytes.len() as u64 <= bound(functions.len() as u64, cu.len() as u64));
        let table = Table { bytes: &bytes };
        assert!(table.index().len() > 2, "more than one page");
        for i in (0..2000).step_by(7) {
            let lsda = (i % 3 == 0).then(|| off(LSDA + i));
            let encoding = FRAMELESS + (i as u32 % 2) * 0x1000 | if lsda.is_some() { HAS_LSDA } else { 0 };
            assert_eq!(table.lookup(off(TEXT + 0x10 * i) + 4), Some((encoding, lsda)), "function {i}");
        }
    }

    #[test]
    fn a_bound_personality_is_its_import() {
        let cu = compact(&[(TEXT, 0x10, FRAME, 0, 0), (TEXT + 0x10, 0x10, FRAME, 0, 0)]);
        let regions = [region(Class::Text, TEXT, 0x20), region(super::super::compact_unwind(), COMPACT, cu.len() as u64)];
        let contents: [&[u8]; 2] = [&[], &cu];
        let functions = [function(TEXT, 0x10, "a"), function(TEXT + 0x10, 0x10, "b")];
        let binds = [
            Bind { addr: COMPACT + 16, import: 3, addend: 0 },
            Bind { addr: COMPACT + 32 + 16, import: 3, addend: 0 },
        ];
        let output = output(Arch::Aarch64);
        let plan = plan();
        let placed = placed(&output, &plan, &regions, &contents, &functions, &binds);
        let bytes = build(&placed, Arch::Aarch64).unwrap();
        let table = Table { bytes: &bytes };
        assert_eq!(table.personalities(), vec![off(COMPACT + 16)]);
        assert_eq!(table.lookup(off(TEXT + 0x10)), Some((FRAME | 1 << 28, None)));
    }

    #[test]
    fn four_personalities_are_an_error() {
        let cu = compact(&[
            (TEXT, 0x10, FRAME, PERSONALITY, 0),
            (TEXT + 0x10, 0x10, FRAME, PERSONALITY + 0x10, 0),
            (TEXT + 0x20, 0x10, FRAME, PERSONALITY + 0x20, 0),
            (TEXT + 0x30, 0x10, FRAME, PERSONALITY + 0x30, 0),
        ]);
        let regions = [region(Class::Text, TEXT, 0x40), region(super::super::compact_unwind(), COMPACT, cu.len() as u64)];
        let contents: [&[u8]; 2] = [&[], &cu];
        let functions: Vec<Function> = (0..4).map(|i| function(TEXT + 0x10 * i, 0x10, "f")).collect();
        let output = output(Arch::Aarch64);
        let plan = plan();
        let placed = placed(&output, &plan, &regions, &contents, &functions, &[]);
        let error = build(&placed, Arch::Aarch64).unwrap_err();
        assert!(error.what.contains("fourth distinct personality"), "{}", error.what);
        assert!(error.what.contains(&format!("{:#x}", TEXT + 0x30)), "{}", error.what);
    }

    #[test]
    fn dwarf_mode_without_an_fde_is_an_error() {
        let (eh, _) = eh_frame(&[(TEXT, 0x10)]);
        let cu = compact(&[(TEXT + 0x10, 0x10, ARM64_MODE_DWARF, 0, 0)]);
        let regions = [
            region(Class::Text, TEXT, 0x20),
            region(Class::EhFrame, EH_FRAME, eh.len() as u64),
            region(super::super::compact_unwind(), COMPACT, cu.len() as u64),
        ];
        let contents: [&[u8]; 3] = [&[], &eh, &cu];
        let functions = [function(TEXT, 0x10, "a"), function(TEXT + 0x10, 0x10, "b")];
        let output = output(Arch::Aarch64);
        let plan = plan();
        let placed = placed(&output, &plan, &regions, &contents, &functions, &[]);
        let error = build(&placed, Arch::Aarch64).unwrap_err();
        assert!(error.what.contains("DWARF mode"), "{}", error.what);
        assert!(error.what.contains(&format!("`b` at {:#x}", TEXT + 0x10)), "{}", error.what);
    }

    #[test]
    fn nothing_to_unwind() {
        let output = output(Arch::Aarch64);
        let plan = plan();
        let placed = placed(&output, &plan, &[], &[], &[], &[]);
        let bytes = build(&placed, Arch::Aarch64).unwrap();
        assert!(bytes.len() as u64 <= bound(0, 0));
        let table = Table { bytes: &bytes };
        assert_eq!(table.index().len(), 1, "the sentinel alone");
        assert!(table.entries().is_empty());
    }

    fn placed<'a>(
        output: &'a Output,
        plan: &'a Plan,
        regions: &'a [Region],
        contents: &'a [&'a [u8]],
        functions: &'a [Function],
        binds: &'a [Bind],
    ) -> Placed<'a> {
        Placed {
            output,
            plan,
            regions,
            contents,
            libraries: &[],
            imports: &[],
            got: &[],
            rebase: &[],
            binds,
            exports: &[],
            functions,
            entry: None,
            tls: None,
            id: [0; 16],
            debug: &[],
        }
    }
}

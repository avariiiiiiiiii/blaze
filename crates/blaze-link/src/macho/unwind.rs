//! `__unwind_info`: the table libunwind and dyld (`_dyld_find_unwind_sections`) look a pc up
//! in, in ld64's format (`<mach-o/compact_unwind_encoding.h>`), version 1.
//!
//! Every function with an FDE in `__eh_frame` gets the DWARF-mode encoding pointing at it;
//! every `__compact_unwind` entry gets its own encoding, with its personality and LSDA;
//! every other function, and the first address after a covered range where nothing starts,
//! gets encoding 0.
//!
//! Built as lld's `UnwindInfoSection` builds it: the FDEs from one scan of `__eh_frame`, then
//! one merge of four sorted columns (compact entries, FDEs, functions, ends of covered ranges)
//! into the entries; a second-level page is compressed (4-byte entries indexing the common
//! encodings or the page's own, as ld64 writes) unless a regular page holds more entries.

#![forbid(unsafe_code)]

use std::borrow::Cow;

use blaze_codegen::Arch;

use crate::LinkError;
use crate::model::{Bind, Class, Function, Placed, Region};

const VERSION: u32 = 1;
const HEADER: u64 = 28;
const COMMON_MAX: usize = 127;
/// Encoding indexes of a compressed entry are 8 bits: the common ones, then the page's own.
const ENCODINGS_MAX: usize = 256;
const PERSONALITY_MAX: usize = 3;
const PAGE: u64 = 4096;
const PAGE_HEADER: u64 = 8;
const PAGE_ENTRY: u64 = 8;
const PAGE_ENTRIES: usize = ((PAGE - PAGE_HEADER) / PAGE_ENTRY) as usize;
const COMPRESSED_HEADER: u64 = 12;
const COMPRESSED_ENTRY: u64 = 4;
const COMPRESSED_OFFSET_MASK: u32 = 0x00FF_FFFF;
const SECOND_LEVEL_REGULAR: u32 = 2;
const SECOND_LEVEL_COMPRESSED: u32 = 3;
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

const DW_EH_PE_OMIT: u8 = 0xff;
const DW_EH_PE_PCREL: u8 = 0x10;

/// The most bytes `__unwind_info` can take for `functions` functions, `fdes` FDEs in
/// `__eh_frame` (a native object's may be of a function no other entry covers) and `compact`
/// bytes of `__compact_unwind` input (32 bytes an entry): the region is sized by it before
/// placement.
///
/// Counted in regular pages: every page but the last holds at least a regular page's 511
/// entries in at most its 4096 bytes, and a compressed last page of `n` entries takes at most
/// `12 + 8n` bytes, 4 more than a regular one.
pub(super) fn bound(functions: u64, fdes: u64, compact: u64) -> u64 {
    let compact = compact.div_ceil(COMPACT_ENTRY);
    let entries = 2 * (functions + fdes + compact);
    let pages = entries.div_ceil(PAGE_ENTRIES as u64);
    HEADER
        + 4 * COMMON_MAX as u64
        + 4 * PERSONALITY_MAX as u64
        + INDEX_ENTRY * (pages + 1)
        + LSDA_ENTRY * compact
        + PAGE_HEADER * pages
        + PAGE_ENTRY * entries
        + (COMPRESSED_HEADER - PAGE_HEADER)
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
    let Compact { mut covered, mut ends, personalities, bytes: compact_bytes } =
        compact(placed, dwarf_mode, &fdes)?;
    ends.extend(fdes.iter().map(|it| it.end));
    if !covered.is_sorted_by_key(|it| it.addr) {
        covered.sort_by_key(|it| it.addr);
    }
    if !ends.is_sorted() {
        ends.sort_unstable();
    }
    let Rows { rows, lsdas, last } = rows(placed, dwarf_mode, &covered, &fdes, &ends)?;
    let sentinel = offset(base, last, "the end of the last function")?;
    if rows.len() as u64 > u64::from(u32::MAX) / COMPRESSED_ENTRY {
        return Err(error(format!(
            "__unwind_info: {} entries, past the 32-bit offsets the format has",
            rows.len()
        )));
    }

    let mut encodings = Encodings::new();
    for row in rows.iter().filter(|it| it.encoding & MODE_MASK != dwarf_mode) {
        encodings.count(row.encoding);
    }
    let common = encodings.common();
    let pages = encodings.pages(&rows, common.len(), dwarf_mode);

    let common_at = HEADER;
    let personalities_at = common_at + 4 * common.len() as u64;
    let index_at = personalities_at + 4 * personalities.len() as u64;
    let lsda_at = index_at + INDEX_ENTRY * (pages.len() as u64 + 1);
    let pages_at = lsda_at + LSDA_ENTRY * lsdas.len() as u64;
    let len = pages_at + pages.iter().map(Page::len).sum::<u64>();
    let limit = bound(placed.functions.len() as u64, fdes.len() as u64, compact_bytes);
    if len > limit {
        return Err(error(format!(
            "__unwind_info: {} entries take {len} bytes, past the {limit} sized for {} functions, \
             {} FDEs and {} compact unwind entries",
            rows.len(),
            placed.functions.len(),
            fdes.len(),
            compact_bytes / COMPACT_ENTRY
        )));
    }
    if len > u64::from(u32::MAX) {
        return Err(error(format!(
            "__unwind_info: {len} bytes, past the 32-bit offsets the format has"
        )));
    }
    let small = |value: u64| value as u32;

    let mut bytes = vec![0; len as usize];
    let mut out = Out { bytes: &mut bytes, at: 0 };
    for value in [
        VERSION,
        small(common_at),
        common.len() as u32,
        small(personalities_at),
        personalities.len() as u32,
        small(index_at),
        pages.len() as u32 + 1,
    ] {
        out.u32(value);
    }
    for &encoding in &common {
        out.u32(encoding);
    }
    for &(_, cell) in &personalities {
        out.u32(cell);
    }
    let mut page_at = pages_at;
    let mut lsda_index = 0usize;
    for page in &pages {
        let first = rows[page.first].function;
        while lsdas.get(lsda_index).is_some_and(|it| it.0 < first) {
            lsda_index += 1;
        }
        out.u32(first);
        out.u32(small(page_at));
        out.u32(small(lsda_at + LSDA_ENTRY * lsda_index as u64));
        page_at += page.len();
    }
    out.u32(sentinel);
    out.u32(0);
    out.u32(small(lsda_at + LSDA_ENTRY * lsdas.len() as u64));
    for &(function, lsda) in &lsdas {
        out.u32(function);
        out.u32(lsda);
    }
    for page in &pages {
        let rows = &rows[page.first..page.first + page.count];
        if !page.compressed {
            out.u32(SECOND_LEVEL_REGULAR);
            out.u16(PAGE_HEADER as u16);
            out.u16(rows.len() as u16);
            for row in rows {
                out.u32(row.function);
                out.u32(row.encoding);
            }
            continue;
        }
        let entries = COMPRESSED_ENTRY as usize * rows.len();
        out.u32(SECOND_LEVEL_COMPRESSED);
        out.u16(COMPRESSED_HEADER as u16);
        out.u16(rows.len() as u16);
        out.u16((COMPRESSED_HEADER as usize + entries) as u16);
        out.u16(page.locals as u16);
        let locals_at = out.at + entries;
        let first = rows[0].function;
        let mut next = common.len();
        encodings.stamp += 1;
        for row in rows {
            let dwarf = row.encoding & MODE_MASK == dwarf_mode;
            let (index, new) = encodings.index(row.encoding, dwarf, next);
            if new {
                out.put(locals_at + 4 * (next - common.len()), row.encoding);
                next += 1;
            }
            out.u32((row.function - first) | (u32::from(index) << 24));
        }
        debug_assert_eq!(next - common.len(), page.locals);
        out.at = locals_at + 4 * page.locals;
    }
    debug_assert_eq!(out.at as u64, len);
    Ok(bytes)
}

/// What a personality cell holds: the loader slides an address of the image, or binds an
/// import.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Held {
    Address(u64),
    Import { import: u32, addend: i64 },
}

/// A start a `__compact_unwind` entry covers: 24 bytes, a column sorted by `addr`.
#[derive(Clone, Copy)]
struct Covered {
    addr: u64,
    kind: Kind,
}

const _: () = assert!(size_of::<Covered>() == 24);

#[derive(Clone, Copy)]
enum Kind {
    /// The encoding with its personality index and `UNWIND_HAS_LSDA`, and the LSDA's address
    /// (0: none).
    Compact { encoding: u32, lsda: u64 },
    /// The FDE's offset in `__eh_frame`.
    Dwarf { fde: u64 },
}

/// An FDE of `__eh_frame`: 24 bytes, a column sorted by `addr`, one per address.
#[derive(Clone, Copy, Debug)]
struct Fde {
    addr: u64,
    end: u64,
    offset: u64,
}

const _: () = assert!(size_of::<Fde>() == 24);

/// A second-level entry, offsets from the image's base: 8 bytes.
#[derive(Clone, Copy)]
struct Row {
    function: u32,
    encoding: u32,
}

const _: () = assert!(size_of::<Row>() == 8);

/// What the `__compact_unwind` regions give: their starts in input order (sorted by the
/// caller), the ends of their ranges, the personalities in order of first use and their
/// cells' offsets, and how many bytes they had.
struct Compact {
    covered: Vec<Covered>,
    ends: Vec<u64>,
    personalities: Vec<(Held, u32)>,
    bytes: u64,
}

fn compact(placed: &Placed<'_>, dwarf_mode: u32, fdes: &[Fde]) -> Result<Compact, LinkError> {
    let base = placed.plan.base;
    let binds: Cow<'_, [Bind]> = if placed.binds.is_sorted_by_key(|it| it.addr) {
        Cow::Borrowed(placed.binds)
    } else {
        let mut binds = placed.binds.to_vec();
        binds.sort_by_key(|it| it.addr);
        Cow::Owned(binds)
    };
    let mut covered = Vec::new();
    let mut ends = Vec::with_capacity(fdes.len());
    let mut personalities: Vec<(Held, u32)> = Vec::with_capacity(PERSONALITY_MAX);
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
        covered.reserve(bytes.len() / COMPACT_ENTRY as usize);
        ends.reserve(bytes.len() / COMPACT_ENTRY as usize);
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
                let at = fdes.partition_point(|it| it.addr < function);
                let Some(fde) = fdes.get(at).filter(|it| it.addr == function) else {
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
                let upto = binds.partition_point(|it| it.addr <= cell);
                let bind = upto.checked_sub(1).map(|at| binds[at]).filter(|it| it.addr == cell);
                let held = match bind {
                    Some(bind) => Some(Held::Import { import: bind.import, addend: bind.addend }),
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
                if lsda != 0 {
                    encoding |= HAS_LSDA;
                }
                Kind::Compact { encoding, lsda }
            };
            covered.push(Covered { addr: function, kind });
            ends.push(end);
        }
    }
    Ok(Compact { covered, ends, personalities, bytes: compact_bytes })
}

/// Function starts in address order: [`Placed::functions`] as the core gives them, or their
/// addresses sorted here when they are not.
enum Starts<'a> {
    Placed(&'a [Function]),
    Sorted(Vec<u64>),
}

impl Starts<'_> {
    fn get(&self, index: usize) -> Option<u64> {
        match self {
            Starts::Placed(functions) => functions.get(index).map(|it| it.addr),
            Starts::Sorted(starts) => starts.get(index).copied(),
        }
    }
}

/// The second-level entries in address order, the LSDA index (function and LSDA offsets),
/// and the end of the last function.
struct Rows {
    rows: Vec<Row>,
    lsdas: Vec<(u32, u32)>,
    last: u64,
}

/// One merge of the sorted columns: of one address the compact entry wins, then the FDE, then
/// a function without either (encoding 0), then the end of a covered range (encoding 0); an
/// entry with the encoding of the one before and neither with an LSDA adds nothing (as ld64).
fn rows(
    placed: &Placed<'_>,
    dwarf_mode: u32,
    covered: &[Covered],
    fdes: &[Fde],
    ends: &[u64],
) -> Result<Rows, LinkError> {
    let base = placed.plan.base;
    let mut last = base;
    let mut sorted = true;
    let mut previous = 0;
    for function in placed.functions {
        last = last.max(function.addr.saturating_add(function.size));
        sorted &= previous <= function.addr;
        previous = function.addr;
    }
    let starts = if sorted {
        Starts::Placed(placed.functions)
    } else {
        let mut starts: Vec<u64> = placed.functions.iter().map(|it| it.addr).collect();
        starts.sort_unstable();
        Starts::Sorted(starts)
    };

    let mut rows: Vec<Row> =
        Vec::with_capacity(placed.functions.len() + covered.len() + fdes.len() + ends.len());
    let mut lsdas = Vec::new();
    let (mut c, mut f, mut s, mut e) = (0, 0, 0, 0);
    let mut lsda_last = false;
    loop {
        let heads = [
            covered.get(c).map(|it| it.addr),
            fdes.get(f).map(|it| it.addr),
            starts.get(s),
            ends.get(e).copied(),
        ];
        let Some(at) = heads.into_iter().flatten().min() else {
            break;
        };
        let kind = if heads[0] == Some(at) {
            Some(covered[c].kind)
        } else if heads[1] == Some(at) {
            Some(Kind::Dwarf { fde: fdes[f].offset })
        } else {
            None
        };
        while covered.get(c).is_some_and(|it| it.addr == at) {
            c += 1;
        }
        if heads[1] == Some(at) {
            f += 1;
        }
        while starts.get(s) == Some(at) {
            s += 1;
        }
        while ends.get(e) == Some(&at) {
            e += 1;
        }
        last = last.max(at);
        let (encoding, lsda) = match kind {
            Some(Kind::Compact { encoding, lsda }) => (encoding, lsda),
            Some(Kind::Dwarf { fde }) => {
                let Some(fde) = u32::try_from(fde).ok().filter(|it| *it <= DWARF_OFFSET_MASK)
                else {
                    return Err(error(format!(
                        "__unwind_info: the FDE of {} is at {fde:#x} in __eh_frame, past the 24 \
                         bits DWARF mode has for it",
                        named(placed, at)
                    )));
                };
                (dwarf_mode | fde, 0)
            }
            None => (0, 0),
        };
        if rows.last().is_some_and(|it| it.encoding == encoding) && !lsda_last && lsda == 0 {
            continue;
        }
        let lsda_offset = match lsda {
            0 => None,
            lsda => Some(offset(base, lsda, "an LSDA")?),
        };
        let function = offset(base, at, "a function")?;
        if let Some(lsda) = lsda_offset {
            lsdas.push((function, lsda));
        }
        rows.push(Row { function, encoding });
        lsda_last = lsda_offset.is_some();
    }
    Ok(Rows { rows, lsdas, last })
}

const NO_COMMON: u8 = u8::MAX;

/// A distinct encoding: 16 bytes.
#[derive(Clone, Copy)]
struct Slot {
    encoding: u32,
    count: u32,
    /// The page being laid out ([`Encodings::stamp`]) that gave it an index of its own, and
    /// that index.
    page: u32,
    local: u8,
    common: u8,
    used: bool,
}

const _: () = assert!(size_of::<Slot>() == 16);

const EMPTY: Slot =
    Slot { encoding: 0, count: 0, page: 0, local: 0, common: NO_COMMON, used: false };

/// The distinct encodings but DWARF mode's, open-addressed with Fibonacci hashing and at most
/// half full: one multiply and about one probe a lookup. A DWARF-mode encoding names its FDE,
/// so it is of one entry: never common, new to its page, never looked up.
struct Encodings {
    slots: Vec<Slot>,
    len: usize,
    shift: u32,
    /// The page being laid out; a slot's `page` equal to it says its `local` is of this page.
    stamp: u32,
}

impl Encodings {
    fn new() -> Self {
        Self { slots: vec![EMPTY; 64], len: 0, shift: 32 - 6, stamp: 0 }
    }

    /// The slot of `encoding`, or the empty one it goes in.
    fn find(&self, encoding: u32) -> usize {
        let mask = self.slots.len() - 1;
        let mut at = (encoding.wrapping_mul(0x9E37_79B9) >> self.shift) as usize;
        while self.slots[at].used && self.slots[at].encoding != encoding {
            at = (at + 1) & mask;
        }
        at
    }

    fn count(&mut self, encoding: u32) {
        let at = self.find(encoding);
        let slot = &mut self.slots[at];
        if slot.used {
            slot.count += 1;
            return;
        }
        *slot = Slot { encoding, count: 1, used: true, ..EMPTY };
        self.len += 1;
        if 2 * self.len > self.slots.len() {
            let grown = vec![EMPTY; 2 * self.slots.len()];
            let old = std::mem::replace(&mut self.slots, grown);
            self.shift -= 1;
            for slot in old.into_iter().filter(|it| it.used) {
                let at = self.find(slot.encoding);
                self.slots[at] = slot;
            }
        }
    }

    /// The common encodings: of those of more than one entry the most frequent, ties by value,
    /// at most 127. One of a single entry is no smaller common than the page's own.
    fn common(&mut self) -> Vec<u32> {
        let mut common: Vec<(u32, u32)> = self
            .slots
            .iter()
            .filter(|it| it.used && it.count > 1)
            .map(|it| (it.count, it.encoding))
            .collect();
        common.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        common.truncate(COMMON_MAX);
        common
            .into_iter()
            .enumerate()
            .map(|(index, (_, encoding))| {
                let at = self.find(encoding);
                self.slots[at].common = index as u8;
                encoding
            })
            .collect()
    }

    /// `encoding`'s index in the compressed page being laid out, and whether it is new to the
    /// page (then it takes `next`).
    fn index(&mut self, encoding: u32, dwarf: bool, next: usize) -> (u8, bool) {
        if dwarf {
            return (next as u8, true);
        }
        let stamp = self.stamp;
        let at = self.find(encoding);
        let slot = &mut self.slots[at];
        debug_assert!(slot.used, "every encoding but DWARF mode's is counted");
        if slot.common != NO_COMMON {
            return (slot.common, false);
        }
        if slot.page == stamp {
            return (slot.local, false);
        }
        slot.page = stamp;
        slot.local = next as u8;
        (slot.local, true)
    }

    /// The second-level pages, as lld splits them: as many entries as a compressed page holds
    /// (4096 bytes, function offsets within 24 bits of its first, 256 encoding indexes); a
    /// regular page instead where that is fewer than a regular page's 511 and more follow.
    fn pages(&mut self, rows: &[Row], common: usize, dwarf_mode: u32) -> Vec<Page> {
        let mut pages = Vec::with_capacity(rows.len() / PAGE_ENTRIES + 1);
        let mut i = 0;
        while i < rows.len() {
            let first = i;
            let limit = u64::from(rows[i].function) + u64::from(COMPRESSED_OFFSET_MASK);
            let mut words = (PAGE - COMPRESSED_HEADER) / 4;
            let mut next = common;
            self.stamp += 1;
            while words >= 1 && i < rows.len() {
                let row = rows[i];
                if u64::from(row.function) >= limit {
                    break;
                }
                let dwarf = row.encoding & MODE_MASK == dwarf_mode;
                let (_, new) = self.index(row.encoding, dwarf, next);
                let cost = 1 + u64::from(new);
                if words < cost || (new && next == ENCODINGS_MAX) {
                    break;
                }
                words -= cost;
                next += usize::from(new);
                i += 1;
            }
            let count = i - first;
            if i < rows.len() && count < PAGE_ENTRIES {
                let count = PAGE_ENTRIES.min(rows.len() - first);
                i = first + count;
                pages.push(Page { first, count, compressed: false, locals: 0 });
            } else {
                pages.push(Page { first, count, compressed: true, locals: next - common });
            }
        }
        pages
    }
}

/// A second-level page: its rows, and of a compressed one how many encodings of its own.
struct Page {
    first: usize,
    count: usize,
    compressed: bool,
    locals: usize,
}

impl Page {
    fn len(&self) -> u64 {
        if self.compressed {
            COMPRESSED_HEADER + COMPRESSED_ENTRY * self.count as u64 + 4 * self.locals as u64
        } else {
            PAGE_HEADER + PAGE_ENTRY * self.count as u64
        }
    }
}

/// The table's bytes, sized exactly before anything is written, and where the next goes.
struct Out<'a> {
    bytes: &'a mut [u8],
    at: usize,
}

impl Out<'_> {
    fn put(&mut self, at: usize, value: u32) {
        self.bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn u32(&mut self, value: u32) {
        self.put(self.at, value);
        self.at += 4;
    }

    fn u16(&mut self, value: u16) {
        self.bytes[self.at..self.at + 2].copy_from_slice(&value.to_le_bytes());
        self.at += 2;
    }
}

/// Every FDE of `__eh_frame`, sorted by the start of the function it covers; of two at one
/// address, the first. One pass over the records, reading of a CIE only its FDE pointer
/// encoding and of an FDE only `pc_begin` and `pc_range`, under gimli's rules for both (CIE
/// versions 1, 3 and 4; `z`, `L`, `P`, `R`, `S` augmentations; pointers absolute or relative
/// to where they are, indirection ignored); an FDE at 0 was of a dead function.
fn fdes(placed: &Placed<'_>) -> Result<Vec<Fde>, LinkError> {
    let Some((index, region)) = placed.region(&Class::EhFrame) else {
        return Ok(Vec::new());
    };
    let bytes = region_bytes(placed, index, region, "__eh_frame")?;
    let bad = |at: usize, what: &str| {
        error(format!("__eh_frame at {:#x}: the entry at offset {at:#x}: {what}", region.addr))
    };
    let mut fdes = Vec::with_capacity(bytes.len() / 32);
    let mut cies: Vec<(usize, u8)> = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let record = at;
        let mut reader = Reader { bytes, at };
        let length = match reader.u32().map_err(|it| bad(record, it))? {
            0 => break,
            0xffff_fff0..=0xffff_fffe => return Err(bad(record, "a reserved initial length")),
            0xffff_ffff => reader.u64().map_err(|it| bad(record, it))?,
            length => u64::from(length),
        };
        let body = reader.at;
        let end = usize::try_from(length)
            .ok()
            .and_then(|it| body.checked_add(it))
            .filter(|it| *it <= bytes.len())
            .ok_or_else(|| bad(record, "longer than the section"))?;
        let mut reader = Reader { bytes: &bytes[..end], at: body };
        let id = reader.u32().map_err(|it| bad(record, it))?;
        at = end;
        if id == 0 {
            let encoding = cie_encoding(&mut reader, region.addr).map_err(|it| bad(record, it))?;
            cies.push((record, encoding));
            continue;
        }
        let Some(cie) = body.checked_sub(id as usize) else {
            return Err(bad(record, "its CIE pointer is before the section"));
        };
        let Ok(found) = cies.binary_search_by_key(&cie, |it| it.0) else {
            return Err(bad(record, "its CIE pointer names no CIE"));
        };
        let encoding = cies[found].1;
        let start = reader.pointer(encoding, region.addr).map_err(|it| bad(record, it))?;
        let len = reader.value(encoding).map_err(|it| bad(record, it))?;
        if start == 0 {
            continue;
        }
        let end = start.checked_add(len).ok_or_else(|| {
            error(format!(
                "__eh_frame at {:#x}: the FDE at offset {record:#x} covers past the end of the \
                 address space",
                region.addr
            ))
        })?;
        fdes.push(Fde { addr: start, end, offset: record as u64 });
    }
    if !fdes.is_sorted_by_key(|it| it.addr) {
        fdes.sort_by_key(|it| it.addr);
    }
    fdes.dedup_by_key(|it| it.addr);
    Ok(fdes)
}

/// The FDE pointer encoding of the CIE `reader` is in (past its ID), its other fields checked
/// as gimli checks them.
fn cie_encoding(reader: &mut Reader<'_>, section: u64) -> Result<u8, &'static str> {
    const UNKNOWN: &str = "a CIE of an unknown augmentation";
    let version = reader.u8()?;
    if !matches!(version, 1 | 3 | 4) {
        return Err("a CIE of an unknown version");
    }
    let augmentation = reader.cstr()?;
    reader.uleb()?;
    reader.sleb()?;
    if version == 1 {
        reader.u8()?;
    } else {
        reader.uleb()?;
    }
    let mut encoding = 0;
    let mut data: Option<Reader<'_>> = None;
    for (index, &ch) in augmentation.iter().enumerate() {
        match ch {
            b'z' if index == 0 => {
                let len = reader.uleb()?;
                let (bytes, start) = (reader.bytes, reader.at);
                let end = usize::try_from(len)
                    .ok()
                    .and_then(|it| start.checked_add(it))
                    .filter(|it| *it <= bytes.len())
                    .ok_or("augmentation data longer than its CIE")?;
                data = Some(Reader { bytes: &bytes[..end], at: start });
                reader.at = end;
            }
            b'L' => {
                let data = data.as_mut().ok_or(UNKNOWN)?;
                valid(data.u8()?)?;
            }
            b'P' => {
                let data = data.as_mut().ok_or(UNKNOWN)?;
                let personality = valid(data.u8()?)?;
                data.pointer(personality, section)?;
            }
            b'R' => {
                let data = data.as_mut().ok_or(UNKNOWN)?;
                encoding = valid(data.u8()?)?;
            }
            b'S' => {}
            _ => return Err(UNKNOWN),
        }
    }
    Ok(encoding)
}

/// `encoding` if it is a `DW_EH_PE_*` pointer encoding.
fn valid(encoding: u8) -> Result<u8, &'static str> {
    let format = matches!(encoding & 0x0f, 0x00..=0x04 | 0x09..=0x0c);
    if encoding == DW_EH_PE_OMIT || (format && encoding & 0x70 <= 0x50) {
        Ok(encoding)
    } else {
        Err("an unknown pointer encoding")
    }
}

/// A cursor over `__eh_frame`, little endian; `at` is from the section's start.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N], &'static str> {
        let bytes = self.bytes.get(self.at..self.at + N).ok_or("cut short")?;
        self.at += N;
        Ok(bytes.try_into().expect("N bytes"))
    }

    fn u8(&mut self) -> Result<u8, &'static str> {
        Ok(self.take::<1>()?[0])
    }

    fn u32(&mut self) -> Result<u32, &'static str> {
        Ok(u32::from_le_bytes(self.take()?))
    }

    fn u64(&mut self) -> Result<u64, &'static str> {
        Ok(u64::from_le_bytes(self.take()?))
    }

    fn uleb(&mut self) -> Result<u64, &'static str> {
        let mut value = 0u64;
        let mut shift = 0;
        loop {
            let byte = self.u8()?;
            if shift == 63 && byte > 1 {
                return Err("an unsigned LEB128 past 64 bits");
            }
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
            shift += 7;
        }
    }

    fn sleb(&mut self) -> Result<i64, &'static str> {
        let mut value = 0i64;
        let mut shift = 0;
        loop {
            let byte = self.u8()?;
            if shift == 63 && byte != 0 && byte != 0x7f {
                return Err("a signed LEB128 past 64 bits");
            }
            value |= i64::from(byte & 0x7f) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                if shift < 64 && byte & 0x40 != 0 {
                    value |= !0 << shift;
                }
                return Ok(value);
            }
        }
    }

    fn cstr(&mut self) -> Result<&'a [u8], &'static str> {
        let rest = &self.bytes[self.at..];
        let len = rest.iter().position(|it| *it == 0).ok_or("a string without its end")?;
        self.at += len + 1;
        Ok(&rest[..len])
    }

    /// A value in `encoding`'s format, signed ones sign-extended.
    fn value(&mut self, encoding: u8) -> Result<u64, &'static str> {
        Ok(match encoding & 0x0f {
            0x00 | 0x04 | 0x0c => self.u64()?,
            0x01 => self.uleb()?,
            0x02 => u64::from(u16::from_le_bytes(self.take()?)),
            0x03 => u64::from(self.u32()?),
            0x09 => self.sleb()? as u64,
            0x0a => i16::from_le_bytes(self.take()?) as u64,
            0x0b => i32::from_le_bytes(self.take()?) as u64,
            _ => return Err("an unknown pointer encoding"),
        })
    }

    /// A pointer in `encoding` with only `__eh_frame`'s address (`section`) known: absolute or
    /// relative to where it is.
    fn pointer(&mut self, encoding: u8, section: u64) -> Result<u64, &'static str> {
        if encoding == DW_EH_PE_OMIT {
            return Err("a pointer whose encoding says it is omitted");
        }
        let base = match encoding & 0x70 {
            0x00 => 0,
            DW_EH_PE_PCREL => section.wrapping_add(self.at as u64),
            _ => return Err("a pointer relative to a base other than where it is"),
        };
        Ok(base.wrapping_add(self.value(encoding)?))
    }
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
            "__unwind_info: {what} at {addr:#x} is not within 4 GiB above the image's base \
             {base:#x}"
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
    use std::collections::BTreeMap;

    use blaze_codegen::Arch;
    use gimli::read::{BaseAddresses, CieOrFde, UnwindSection};
    use gimli::write::{
        Address, CommonInformationEntry, EhFrame, EndianVec, FrameDescriptionEntry, FrameTable,
    };

    use super::{
        ARM64_MODE_DWARF, COMMON_MAX, COMPACT_ENTRY, DWARF_OFFSET_MASK, HAS_LSDA, HEADER,
        Held, INDEX_ENTRY, LSDA_ENTRY, MODE_MASK, PAGE_ENTRY, PAGE_HEADER, PERSONALITY_MASK,
        PERSONALITY_MAX, PERSONALITY_SHIFT, SECOND_LEVEL_REGULAR, VERSION, X86_64_MODE_DWARF,
        bound, build, error, named, offset, region_bytes, u32_at, u64_at,
    };
    use crate::LinkError;
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

    fn compact_region(bytes: &[u8]) -> Region {
        region(super::super::compact_unwind(), COMPACT, bytes.len() as u64)
    }

    fn function(addr: u64, size: u64, name: &str) -> Function {
        Function { addr, size, fde: None, windows: None, name: Some(name.into()) }
    }

    fn no_symbol(_: &str) -> Option<u64> {
        None
    }

    /// An `__eh_frame` at [`EH_FRAME`] of one CIE and an FDE for each range, its addresses
    /// pc-relative; and each FDE's offset in it.
    fn eh_frame(ranges: &[(u64, u32)]) -> (Vec<u8>, Vec<u64>) {
        let encoding =
            gimli::Encoding { address_size: 8, format: gimli::Format::Dwarf32, version: 1 };
        let mut cie = CommonInformationEntry::new(encoding, 4, -8, gimli::Register(30));
        cie.fde_address_encoding = gimli::DW_EH_PE_pcrel | gimli::DW_EH_PE_sdata4;
        let mut table = FrameTable::default();
        let cie = table.add_cie(cie);
        for &(addr, len) in ranges {
            let start = Address::Constant(addr.wrapping_sub(EH_FRAME));
            table.add_fde(cie, FrameDescriptionEntry::new(start, len));
        }
        let mut section = EhFrame(EndianVec::new(gimli::LittleEndian));
        table.write_eh_frame(&mut section).unwrap();
        let bytes = section.0.into_vec();
        let offsets = fde_offsets(&bytes);
        (bytes, offsets)
    }

    /// The offset of each FDE in `bytes`, in order.
    fn fde_offsets(bytes: &[u8]) -> Vec<u64> {
        let mut offsets = Vec::new();
        let mut at = 0;
        while at < bytes.len() {
            let length = u32_at(bytes, at) as usize;
            if u32_at(bytes, at + 4) != 0 {
                offsets.push(at as u64);
            }
            at += 4 + length;
        }
        offsets
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

    /// The table as libunwind reads it, both kinds of second-level page.
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

        /// The `i`th first-level entry: function offset, page offset, LSDA index offset.
        fn first_level(&self, i: u32) -> (u32, u32, u32) {
            let at = self.word(20) + 12 * i;
            (self.word(at), self.word(at + 4), self.word(at + 8))
        }

        fn index(&self) -> Vec<(u32, u32, u32)> {
            (0..self.word(24)).map(|i| self.first_level(i)).collect()
        }

        /// The kind of each second-level page.
        fn kinds(&self) -> Vec<u32> {
            (0..self.word(24) - 1).map(|i| self.word(self.first_level(i).1)).collect()
        }

        fn count(&self, page: u32) -> u32 {
            u32::from(self.half(page + 6))
        }

        /// The `i`th entry of the page at `page`, whose first-level function offset is
        /// `first`: its function offset and encoding.
        fn entry(&self, page: u32, first: u32, i: u32) -> (u32, u32) {
            let start = page + u32::from(self.half(page + 4));
            match self.word(page) {
                2 => (self.word(start + 8 * i), self.word(start + 8 * i + 4)),
                3 => {
                    let entry = self.word(start + 4 * i);
                    let index = entry >> 24;
                    let common = self.word(8);
                    let encoding = if index < common {
                        self.word(self.word(4) + 4 * index)
                    } else {
                        let local = index - common;
                        assert!(local < u32::from(self.half(page + 10)), "a local index in range");
                        self.word(page + u32::from(self.half(page + 8)) + 4 * local)
                    };
                    (first + (entry & 0x00ff_ffff), encoding)
                }
                kind => panic!("a second-level page of kind {kind}"),
            }
        }

        /// Every second-level entry, in order.
        fn entries(&self) -> Vec<(u32, u32)> {
            let mut entries = Vec::new();
            for i in 0..self.word(24) - 1 {
                let (first, page, _) = self.first_level(i);
                assert_eq!(self.entry(page, first, 0).0, first, "a page starts at its index entry");
                for n in 0..self.count(page) {
                    entries.push(self.entry(page, first, n));
                }
            }
            entries
        }

        /// The LSDA index: function and LSDA offsets.
        fn lsdas(&self) -> Vec<(u32, u32)> {
            let (from, to) = (self.first_level(0).2, self.first_level(self.word(24) - 1).2);
            (from..to).step_by(8).map(|at| (self.word(at), self.word(at + 4))).collect()
        }

        /// What libunwind finds for `pc`, an offset from the base: the entry's function, its
        /// encoding and, when the encoding has one, the LSDA's offset.
        fn find(&self, pc: u32) -> Option<(u32, u32, Option<u32>)> {
            let pages = self.word(24).checked_sub(1)?;
            let page = (0..pages)
                .find(|&i| self.first_level(i).0 <= pc && pc < self.first_level(i + 1).0)?;
            let (first, at, lsda_from) = self.first_level(page);
            let lsda_to = self.first_level(page + 1).2;
            let (mut low, mut high) = (0, self.count(at));
            while low < high {
                let middle = (low + high) / 2;
                if self.entry(at, first, middle).0 <= pc {
                    low = middle + 1;
                } else {
                    high = middle;
                }
            }
            let (function, encoding) = self.entry(at, first, low.checked_sub(1)?);
            if encoding & HAS_LSDA == 0 {
                return Some((function, encoding, None));
            }
            let lsda = (lsda_from..lsda_to)
                .step_by(8)
                .find(|&at| self.word(at) == function)
                .map(|at| self.word(at + 4));
            assert!(lsda.is_some(), "an LSDA for the function at {function:#x}");
            Some((function, encoding, lsda))
        }

        fn lookup(&self, pc: u32) -> Option<(u32, Option<u32>)> {
            self.find(pc).map(|(_, encoding, lsda)| (encoding, lsda))
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
            compact_region(&cu),
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
            name: "test",
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
            init: None,
            fini: None,
            tls: None,
            id: [0; 16],
            debug: &[],
            symbol: &no_symbol,
        };
        let bytes = build(&placed, Arch::Aarch64).unwrap();
        assert!(bytes.len() as u64 <= bound(functions.len() as u64, 8, cu.len() as u64));
        let table = Table { bytes: &bytes };
        assert_eq!(table.word(0), 1, "version 1");
        assert_eq!(table.kinds(), vec![3], "one compressed page");

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
        assert!(
            entries.iter().all(|it| matches!(it.1 & PERSONALITY_MASK, 0 | 0x1000_0000)),
            "one personality, index 1"
        );

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
        let regions =
            [region(Class::Text, TEXT, 0x20), region(Class::EhFrame, EH_FRAME, eh.len() as u64)];
        let contents: [&[u8]; 2] = [&[], &eh];
        let functions = [function(TEXT, 0x10, "a"), function(TEXT + 0x10, 0x10, "b")];
        let output = output(Arch::X86_64);
        let plan = plan();
        let placed = placed(&output, &plan, &regions, &contents, &functions, &[]);
        let bytes = build(&placed, Arch::X86_64).unwrap();
        assert!(bytes.len() as u64 <= bound(2, 2, 0));
        let table = Table { bytes: &bytes };
        let dwarf = |n: usize| Some((X86_64_MODE_DWARF | fde[n] as u32, None));
        assert_eq!(table.lookup(off(TEXT)), dwarf(0));
        assert_eq!(table.lookup(off(TEXT) + 0x18), dwarf(1));
        assert_eq!(table.lookup(off(TEXT) + 0x20), None);
        assert!(table.personalities().is_empty());
        assert!(table.common().is_empty(), "DWARF-mode encodings are of one entry each");
    }

    #[test]
    fn many_functions_span_pages() {
        let functions: Vec<Function> =
            (0..2000).map(|i| function(TEXT + 0x10 * i, 0x10, "f")).collect();
        let encoding = |i: u64| FRAMELESS + (i as u32 % 2) * 0x1000;
        let lsda = |i: u64| if i % 3 == 0 { LSDA + i } else { 0 };
        let entries: Vec<(u64, u32, u32, u64, u64)> =
            (0..2000).map(|i| (TEXT + 0x10 * i, 0x10, encoding(i), 0, lsda(i))).collect();
        let cu = compact(&entries);
        let regions = [region(Class::Text, TEXT, 0x10 * 2000), compact_region(&cu)];
        let contents: [&[u8]; 2] = [&[], &cu];
        let output = output(Arch::Aarch64);
        let plan = plan();
        let placed = placed(&output, &plan, &regions, &contents, &functions, &[]);
        let bytes = build(&placed, Arch::Aarch64).unwrap();
        assert!(bytes.len() as u64 <= bound(functions.len() as u64, 0, cu.len() as u64));
        let table = Table { bytes: &bytes };
        assert!(table.index().len() > 2, "more than one page");
        assert!(table.kinds().iter().all(|it| *it == 3), "every page compressed");
        assert_eq!(table.common().len(), 4, "two encodings, each with and without an LSDA");
        assert!(bytes.len() < 2001 * 4 + 2000 / 3 * 8 + 256, "4 bytes an entry: {}", bytes.len());
        for i in (0..2000).step_by(7) {
            let lsda = (i % 3 == 0).then(|| off(LSDA + i));
            let encoding = encoding(i) | if lsda.is_some() { HAS_LSDA } else { 0 };
            let found = table.lookup(off(TEXT + 0x10 * i) + 4);
            assert_eq!(found, Some((encoding, lsda)), "function {i}");
        }
    }

    #[test]
    fn a_page_past_24_bits_of_function_offsets_ends() {
        let far = TEXT + 0x0100_0000;
        let functions = [function(TEXT, 0x10, "near"), function(far, 0x10, "far")];
        let cu = compact(&[(TEXT, 0x10, FRAME, 0, 0), (far, 0x10, FRAMELESS, 0, 0)]);
        let regions = [compact_region(&cu)];
        let contents: [&[u8]; 1] = [&cu];
        let output = output(Arch::Aarch64);
        let plan = plan();
        let placed = placed(&output, &plan, &regions, &contents, &functions, &[]);
        let bytes = build(&placed, Arch::Aarch64).unwrap();
        let table = Table { bytes: &bytes };
        assert_eq!(table.kinds(), vec![2], "regular, as a compressed page would hold 2 entries");
        assert_eq!(table.lookup(off(TEXT) + 4), Some((FRAME, None)));
        assert_eq!(table.lookup(off(TEXT) + 0x10), Some((0, None)));
        assert_eq!(table.lookup(off(far) + 4), Some((FRAMELESS, None)));
    }

    #[test]
    fn a_bound_personality_is_its_import() {
        let cu = compact(&[(TEXT, 0x10, FRAME, 0, 0), (TEXT + 0x10, 0x10, FRAME, 0, 0)]);
        let regions = [region(Class::Text, TEXT, 0x20), compact_region(&cu)];
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
        assert_eq!(table.lookup(off(TEXT + 0x10)), Some((FRAME | (1 << 28), None)));
    }

    #[test]
    fn four_personalities_are_an_error() {
        let cu = compact(&[
            (TEXT, 0x10, FRAME, PERSONALITY, 0),
            (TEXT + 0x10, 0x10, FRAME, PERSONALITY + 0x10, 0),
            (TEXT + 0x20, 0x10, FRAME, PERSONALITY + 0x20, 0),
            (TEXT + 0x30, 0x10, FRAME, PERSONALITY + 0x30, 0),
        ]);
        let regions = [region(Class::Text, TEXT, 0x40), compact_region(&cu)];
        let contents: [&[u8]; 2] = [&[], &cu];
        let functions: Vec<Function> =
            (0..4).map(|i| function(TEXT + 0x10 * i, 0x10, "f")).collect();
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
            compact_region(&cu),
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
        assert!(bytes.len() as u64 <= bound(0, 0, 0));
        let table = Table { bytes: &bytes };
        assert_eq!(table.index().len(), 1, "the sentinel alone");
        assert!(table.entries().is_empty());
    }

    /// The bytes of a CIE or FDE whose body is `body`.
    fn record(body: &[u8]) -> Vec<u8> {
        let mut record = (body.len() as u32).to_le_bytes().to_vec();
        record.extend_from_slice(body);
        record
    }

    /// An FDE appended to `eh` naming the CIE at `cie`, its body after the CIE pointer `rest`.
    fn push_fde(eh: &mut Vec<u8>, cie: usize, rest: &[u8]) {
    let pointer = (eh.len() + 4 - cie) as u32;
    let parts: [&[u8]; 2] = [&pointer.to_le_bytes(), rest];
    let record = record(&parts.concat());
        eh.extend_from_slice(&record);
    }

    fn eh_placed<'a>(
        output: &'a Output,
        plan: &'a Plan,
        regions: &'a [Region],
        contents: &'a [&'a [u8]],
    ) -> Placed<'a> {
        placed(output, plan, regions, contents, &[], &[])
    }

    fn scanned(placed: &Placed<'_>) -> Result<Vec<(u64, u64, u64)>, LinkError> {
        Ok(super::fdes(placed)?.iter().map(|it| (it.addr, it.offset, it.end)).collect())
    }

    fn parsed(placed: &Placed<'_>) -> Result<Vec<(u64, u64, u64)>, LinkError> {
        Ok(gimli_fdes(placed)?.into_iter().map(|(addr, (at, end))| (addr, at, end)).collect())
    }

    #[test]
    fn hand_built_records_scan_as_gimli_parses_them() {
        let mut eh = Vec::new();
        let plain = eh.len();
        eh.extend_from_slice(&record(&[0, 0, 0, 0, 1, 0, 1, 0x78, 30]));
        push_fde(&mut eh, plain, &[TEXT.to_le_bytes(), 0x10u64.to_le_bytes()].concat());
        push_fde(&mut eh, plain, &[0u64.to_le_bytes(), 0x10u64.to_le_bytes()].concat());
        let udata4 = eh.len();
        let cie: [&[u8]; 3] = [&[0, 0, 0, 0, 3], b"zR\0", &[1, 0x78, 30, 1, 0x03]];
        eh.extend_from_slice(&record(&cie.concat()));
        let fde: [&[u8]; 3] = [&0x5000u32.to_le_bytes(), &0x20u32.to_le_bytes(), &[0]];
        push_fde(&mut eh, udata4, &fde.concat());
        let full = eh.len();
        let cie: [&[u8]; 3] =
            [&[0, 0, 0, 0, 1], b"zPLRS\0", &[1, 0x78, 30, 7, 0x9b, 0, 0, 0, 0, 0x1b, 0x1b]];
        eh.extend_from_slice(&record(&cie.concat()));
        let field = EH_FRAME + eh.len() as u64 + 8;
        let pc = (TEXT + 0x40).wrapping_sub(field) as u32;
        let fde: [&[u8]; 3] = [&pc.to_le_bytes(), &0x30u32.to_le_bytes(), &[4, 0, 0, 0, 0]];
        push_fde(&mut eh, full, &fde.concat());
        push_fde(&mut eh, plain, &[TEXT.to_le_bytes(), 0x80u64.to_le_bytes()].concat());

        let output = output(Arch::Aarch64);
        let plan = plan();
        let regions = [region(Class::EhFrame, EH_FRAME, eh.len() as u64)];
        let contents: [&[u8]; 1] = [&eh];
        let placed = eh_placed(&output, &plan, &regions, &contents);
        let fdes = scanned(&placed).unwrap();
        assert_eq!(fdes, parsed(&placed).unwrap());
        let starts: Vec<u64> = fdes.iter().map(|it| it.0).collect();
        assert_eq!(starts, vec![0x5000, TEXT, TEXT + 0x40], "sorted, the dead FDE left out");
        assert_eq!(fdes[1].2, TEXT + 0x10, "of two FDEs at one address the first");

        let mut bad = eh.clone();
        push_fde(&mut bad, plain + 1, &[0; 16]);
        let contents: [&[u8]; 1] = [&bad];
        let regions = [region(Class::EhFrame, EH_FRAME, bad.len() as u64)];
        let placed = eh_placed(&output, &plan, &regions, &contents);
        let error = super::fdes(&placed).unwrap_err();
        assert!(error.what.contains("names no CIE"), "{}", error.what);
        assert!(gimli_fdes(&placed).is_err());

        let version = record(&[0, 0, 0, 0, 2, 0, 1, 0x78, 30]);
        let contents: [&[u8]; 1] = [&version];
        let regions = [region(Class::EhFrame, EH_FRAME, version.len() as u64)];
        let placed = eh_placed(&output, &plan, &regions, &contents);
        let error = super::fdes(&placed).unwrap_err();
        assert!(error.what.contains("unknown version"), "{}", error.what);
        assert!(gimli_fdes(&placed).is_err());
    }

    /// SplitMix64.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }

        fn shuffle<T>(&mut self, items: &mut [T]) {
            for i in (1..items.len()).rev() {
                let j = self.below(i as u64 + 1) as usize;
                items.swap(i, j);
            }
        }
    }

    /// A seeded image: functions with FDEs (of two CIEs, one with a personality and LSDA),
    /// compact entries (dead ones, DWARF-mode ones, duplicates, up to 3 or 4 personalities
    /// held by address or by a bind, LSDAs), both out of address order, gaps past 24 bits.
    struct Case {
        arch: Arch,
        functions: Vec<Function>,
        fdes: u64,
        eh: Vec<u8>,
        cu: Vec<u8>,
        binds: Vec<Bind>,
    }

    fn case(seed: u64) -> Case {
        let mut rng = Rng(seed);
        let (arch, dwarf) = match seed % 2 {
            0 => (Arch::Aarch64, ARM64_MODE_DWARF),
            _ => (Arch::X86_64, X86_64_MODE_DWARF),
        };
        let n = match seed % 8 {
            0 | 4 => 2000 + rng.below(2000),
            _ => 1 + rng.below(300),
        };
        let distinct = match seed % 8 {
            0 => 300,
            4 => 2,
            _ => [2, 30, 300][rng.below(3) as usize],
        };
        let held = if seed % 9 == 0 { 4 } else { 1 + rng.below(3) };
        let mut functions = Vec::with_capacity(n as usize);
        let mut frames: Vec<(u64, u32)> = Vec::new();
        let mut entries: Vec<(u64, u32, u32, u64, u64)> = Vec::new();
        let mut addr = TEXT;
        for i in 0..n {
            if rng.below(300) == 0 {
                addr += 0x0100_0000 + 4 * rng.below(0x400);
            }
            let size = 4 * (1 + rng.below(48));
            functions.push(function(addr, size, "f"));
            let short = |rng: &mut Rng| if rng.below(8) == 0 { size - 4 } else { size };
            let covered = rng.below(10);
            if covered < 4 || covered == 8 {
                frames.push((addr, short(&mut rng) as u32));
            }
            if (4..9).contains(&covered) {
                let encoding = if covered == 8 && rng.below(2) == 0 {
                    dwarf
                } else {
                    let k = rng.below(distinct) as u32;
                    (if k % 2 == 0 { 0x0100_0000 } else { 0x0200_0000 }) | (k << 4)
                };
                let personality = rng.below(held + 1);
                let lsda = if rng.below(4) == 0 { LSDA + 8 * i } else { 0 };
                entries.push((addr, short(&mut rng) as u32, encoding, personality, lsda));
            }
            if rng.below(40) == 0 {
                frames.push((addr, size as u32));
            }
            if rng.below(40) == 0 {
                entries.push((addr, size as u32, 0x0200_0000 | 0xff0, held, 0));
            }
            if rng.below(30) == 0 {
                entries.push((0, size as u32, 0x0100_0000, 0, LSDA));
            }
            addr += size + 4 * rng.below(3);
        }
        frames.push((addr + 0x40, 0x20));
        rng.shuffle(&mut frames);
        rng.shuffle(&mut entries);
        if seed % 7 == 3 {
            rng.shuffle(&mut functions);
        }

        let encoding =
            gimli::Encoding { address_size: 8, format: gimli::Format::Dwarf32, version: 1 };
        let pcrel = gimli::DW_EH_PE_pcrel | gimli::DW_EH_PE_sdata4;
        let mut table = FrameTable::default();
        let mut plain = CommonInformationEntry::new(encoding, 4, -8, gimli::Register(30));
        plain.fde_address_encoding = pcrel;
        let plain = table.add_cie(plain);
        let mut full = CommonInformationEntry::new(encoding, 4, -8, gimli::Register(30));
        full.fde_address_encoding = pcrel;
        full.lsda_encoding = Some(pcrel);
        full.personality = Some((
            gimli::DW_EH_PE_indirect | pcrel,
            Address::Constant(PERSONALITY.wrapping_sub(EH_FRAME)),
        ));
        let full = table.add_cie(full);
        for &(addr, len) in &frames {
            let start = Address::Constant(addr.wrapping_sub(EH_FRAME));
            let mut fde = FrameDescriptionEntry::new(start, len);
            if rng.below(3) == 0 {
                fde.lsda = Some(Address::Constant(LSDA.wrapping_sub(EH_FRAME)));
                table.add_fde(full, fde);
            } else {
                table.add_fde(plain, fde);
            }
        }
        let mut section = EhFrame(EndianVec::new(gimli::LittleEndian));
        table.write_eh_frame(&mut section).unwrap();

        let mut cu = Vec::with_capacity(entries.len() * COMPACT_ENTRY as usize);
        let mut binds = Vec::new();
        for (n, &(function, length, encoding, personality, lsda)) in entries.iter().enumerate() {
            let cell = match personality {
                k if k == held => 0,
                k if k % 2 == 0 => PERSONALITY + 0x10 * k,
                k => {
                    let addr = COMPACT + 32 * n as u64 + 16;
                    binds.push(Bind { addr, import: k as u32, addend: 0 });
                    0
                }
            };
            cu.extend_from_slice(&compact(&[(function, length, encoding, cell, lsda)]));
        }
        if seed % 3 == 0 {
            binds.reverse();
        }
        Case { arch, functions, fdes: frames.len() as u64, eh: section.0.into_vec(), cu, binds }
    }

    #[test]
    fn the_fde_scan_reads_what_gimli_reads() {
        for seed in 0..16 {
            let case = case(seed);
            let output = output(case.arch);
            let plan = plan();
            let regions = [region(Class::EhFrame, EH_FRAME, case.eh.len() as u64)];
            let contents: [&[u8]; 1] = [&case.eh];
            let placed = eh_placed(&output, &plan, &regions, &contents);
            assert_eq!(scanned(&placed).unwrap(), parsed(&placed).unwrap(), "seed {seed}");
        }
    }

    #[test]
    fn the_new_tables_answer_as_the_regular_page_builder_did() {
        let (mut compressed, mut regular_pages, mut errors) = (0, 0, 0);
        for seed in 0..48 {
            let case = case(seed);
            let output = output(case.arch);
            let plan = plan();
            let regions = [
                region(Class::EhFrame, EH_FRAME, case.eh.len() as u64),
                compact_region(&case.cu),
            ];
            let contents: [&[u8]; 2] = [&case.eh, &case.cu];
            let placed = placed(&output, &plan, &regions, &contents, &case.functions, &case.binds);
            let new = build(&placed, case.arch).map_err(|it| it.what);
            let old = regular(&placed, case.arch).map_err(|it| it.what);
            let (new, old) = match (new, old) {
                (Ok(new), Ok(old)) => (new, old),
                (Err(new), Err(old)) => {
                    assert_eq!(new, old, "seed {seed}: the same error");
                    errors += 1;
                    continue;
                }
                (new, old) => panic!("seed {seed}: {:?} where it was {:?}", new.err(), old.err()),
            };
            assert_eq!(build(&placed, case.arch).unwrap(), new, "seed {seed}: deterministic");
            let limit = bound(case.functions.len() as u64, case.fdes, case.cu.len() as u64);
            assert!(new.len() as u64 <= limit, "seed {seed}: {} past {limit}", new.len());
            assert!(new.len() <= old.len() + 4, "seed {seed}: {} against {}", new.len(), old.len());

            let (new, old) = (Table { bytes: &new }, Table { bytes: &old });
            let entries = old.entries();
            assert_eq!(new.entries(), entries, "seed {seed}: the same entries");
            assert_eq!(new.personalities(), old.personalities(), "seed {seed}");
            assert_eq!(new.lsdas(), old.lsdas(), "seed {seed}: the same LSDA index");
            let sentinel = old.index().last().unwrap().0;
            assert_eq!(new.index().last().unwrap().0, sentinel, "seed {seed}: the sentinel");
            let boundaries = entries.iter().map(|it| it.0).chain([sentinel]);
            for start in boundaries {
                for pc in [start.wrapping_sub(1), start, start + 1, start + 2] {
                    assert_eq!(new.find(pc), old.find(pc), "seed {seed}: pc {pc:#x}");
                }
            }
            assert_eq!(new.find(0), old.find(0), "seed {seed}: before every function");
            for kind in new.kinds() {
                match kind {
                    3 => compressed += 1,
                    _ => regular_pages += 1,
                }
            }
        }
        assert!(compressed > 0, "some pages compressed");
        assert!(regular_pages > 0, "some pages regular, where a compressed one held fewer");
        assert!(errors < 48, "most seeds build");
    }

    /// The FDEs as the builder this one replaced read them: gimli's full parse, a map by start.
    fn gimli_fdes(placed: &Placed<'_>) -> Result<BTreeMap<u64, (u64, u64)>, LinkError> {
        let mut fdes = BTreeMap::new();
        let Some((index, region)) = placed.region(&Class::EhFrame) else {
            return Ok(fdes);
        };
        let bytes = region_bytes(placed, index, region, "__eh_frame")?;
        let mut section = gimli::read::EhFrame::new(bytes, gimli::LittleEndian);
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
                error(format!("__eh_frame at {:#x}: the FDE at {at:#x} overflows", region.addr))
            })?;
            fdes.entry(start).or_insert((at, end));
        }
        Ok(fdes)
    }

    enum OldKind {
        Compact { encoding: u32, lsda: Option<u64> },
        Dwarf { fde: u64 },
        None,
    }

    struct OldEntry {
        addr: u64,
        rank: u8,
        kind: OldKind,
    }

    struct OldRow {
        function: u32,
        encoding: u32,
        lsda: Option<u32>,
    }

    /// The builder this one replaced, as it was: gimli's FDE map, one sort of every start,
    /// regular pages only. The differential test holds `build` to its answers.
    fn regular(placed: &Placed<'_>, arch: Arch) -> Result<Vec<u8>, LinkError> {
        const PAGE_ENTRIES: usize = 511;
        let dwarf_mode = match arch {
            Arch::Aarch64 => ARM64_MODE_DWARF,
            Arch::X86_64 => X86_64_MODE_DWARF,
            Arch::Wasm32 => return Err(error("wasm32".into())),
        };
        let base = placed.plan.base;
        let fdes = gimli_fdes(placed)?;

        let mut entries = Vec::new();
        let mut ends = Vec::new();
        let mut personalities: Vec<(Held, u32)> = Vec::new();
        let class = super::super::compact_unwind();
        let regions = placed.regions.iter().enumerate().filter(|(_, it)| it.class == class);
        for (index, region) in regions {
            let bytes = region_bytes(placed, index, region, "__compact_unwind")?;
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
                let end = function + u64::from(u32_at(entry, 8));
                let encoding = u32_at(entry, 12);
                let personality = u64_at(entry, 16);
                let lsda = u64_at(entry, 24);
                let kind = if encoding & MODE_MASK == dwarf_mode {
                    let Some(&(fde, _)) = fdes.get(&function) else {
                        return Err(error(format!(
                            "__compact_unwind: the entry of {} says DWARF mode ({encoding:#010x}), \
                             but no FDE in __eh_frame starts there",
                            named(placed, function)
                        )));
                    };
                    OldKind::Dwarf { fde }
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
                    OldKind::Compact { encoding, lsda }
                };
                entries.push(OldEntry { addr: function, rank: 0, kind });
                ends.push(end);
            }
        }
        for (&start, &(fde, end)) in &fdes {
            entries.push(OldEntry { addr: start, rank: 1, kind: OldKind::Dwarf { fde } });
            ends.push(end);
        }
        let mut last = base;
        for function in placed.functions {
            entries.push(OldEntry { addr: function.addr, rank: 2, kind: OldKind::None });
            last = last.max(function.addr.saturating_add(function.size));
        }
        for &end in &ends {
            entries.push(OldEntry { addr: end, rank: 3, kind: OldKind::None });
            last = last.max(end);
        }
        entries.sort_by_key(|it| (it.addr, it.rank));
        entries.dedup_by_key(|it| it.addr);
        if let Some(entry) = entries.last() {
            last = last.max(entry.addr);
        }

        let mut rows: Vec<OldRow> = Vec::with_capacity(entries.len());
        for entry in &entries {
            let (encoding, lsda) = match entry.kind {
                OldKind::Compact { encoding, lsda } => (encoding, lsda),
                OldKind::Dwarf { fde } => {
                    let Some(fde) = u32::try_from(fde).ok().filter(|it| *it <= DWARF_OFFSET_MASK)
                    else {
                        return Err(error(format!(
                            "__unwind_info: the FDE of {} is at {fde:#x} in __eh_frame, past the \
                             24 bits DWARF mode has for it",
                            named(placed, entry.addr)
                        )));
                    };
                    (dwarf_mode | fde, None)
                }
                OldKind::None => (0, None),
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
            rows.push(OldRow { function: offset(base, entry.addr, "a function")?, encoding, lsda });
        }
        let sentinel = offset(base, last, "the end of the last function")?;

        let mut counts: BTreeMap<u32, u64> = BTreeMap::new();
        for row in &rows {
            *counts.entry(row.encoding).or_default() += 1;
        }
        let mut common: Vec<(u32, u64)> = counts.into_iter().collect();
        common.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        common.truncate(COMMON_MAX);

        let pages: Vec<&[OldRow]> = rows.chunks(PAGE_ENTRIES).collect();
        let lsdas: Vec<(u32, u32)> =
            rows.iter().filter_map(|row| row.lsda.map(|lsda| (row.function, lsda))).collect();
        let common_at = HEADER;
        let personalities_at = common_at + 4 * common.len() as u64;
        let index_at = personalities_at + 4 * personalities.len() as u64;
        let lsda_at = index_at + INDEX_ENTRY * (pages.len() as u64 + 1);
        let pages_at = lsda_at + LSDA_ENTRY * lsdas.len() as u64;
        let small = |value: u64| value as u32;

        let mut out = Vec::new();
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
        Ok(out)
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
            name: "test",
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
            init: None,
            fini: None,
            tls: None,
            id: [0; 16],
            debug: &[],
            symbol: &no_symbol,
        }
    }
}

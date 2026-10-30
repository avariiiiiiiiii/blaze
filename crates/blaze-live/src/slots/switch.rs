//! Switching an image's jump table between generations, apart from how a platform writes
//! its memory ([`Memory`]).
//!
//! A patch is checked whole, viewed in place in its message's bytes, with scratch buffers the
//! switch keeps from patch to patch: once they have grown, taking a patch allocates nothing.
//! Its cells go into one of [`POOL`] tables the switch owns, the one the cell left longest
//! ago, brought to the new generation by storing only the slots that differ: undoing the
//! generations it held down to where they meet the new one's, then redoing those up to it,
//! from the [`Log`] of every generation's cells. One release store then points the cell at
//! it. A rollback is the same towards an older generation, and only the cell's store when a
//! table holds that generation still, or when it is the one the program started at (the
//! loader's table).
//!
//! What a patch may write over:
//! - Code goes only into the patch code region, and never over code of a generation whose
//!   unwind entries were registered or that the cell may have pointed at ([`Lasting`]): a
//!   thread can still be in that code however long ago the program left it, rolled back or
//!   not.
//! - Nothing goes over an unwind entry once registered, nor over the zero length ending a
//!   registered run: the unwinder reads them from then on.
//! - Data goes outside the patch code region, the table cell and the loader's table; the
//!   platform checks that it is in the image.
//! - Data is written in place with atomic stores ([`Writes::store`]): one relaxed store per
//!   naturally aligned piece of at most 8 bytes, each holding its final, rebased value, so no
//!   write races another thread's load of that piece. A thread reading a static wider than
//!   8 bytes while `poll` rewrites it can see part of it old and part new, as with any static
//!   another thread changes; data no code reaches before the cell's store (a new static, a
//!   new unwind entry) is seen whole.

use std::{
    ops::Range,
    sync::atomic::{AtomicU64, Ordering},
};

use super::{Outcome, hook};
use crate::patch::{
    ARCH_AARCH64, ARCH_X86_64, Answer, DESCRIPTOR, DESCRIPTOR_VERSION, Descriptor, MessageView,
    PatchView,
};

/// How many generations are kept to roll back to, as Relax keeps.
pub(crate) const KEPT: usize = 64;
/// How many tables of its own a switch writes generations into, reused oldest first. A
/// table is written while the cell points at another, and a thread that loaded the cell
/// before it left this one reads each slot whole, of the generation it was in or the one
/// it is brought to, both code that is written and never written again ([`Lasting`]).
pub(crate) const POOL: usize = 4;
const MOST_SLOTS: u64 = 1 << 26;
const MOST_DIR: u64 = 4096;

/// [`Descriptor::arch`] of what this program runs on; none matches elsewhere.
pub(crate) const HOST_ARCH: u32 = if cfg!(target_arch = "x86_64") {
    ARCH_X86_64
} else if cfg!(target_arch = "aarch64") {
    ARCH_AARCH64
} else {
    u32::MAX
};

/// What an image's descriptor says, with its slide. Addresses are as linked unless a method
/// slides them.
#[derive(Clone, Debug)]
pub(crate) struct Layout {
    pub(crate) id: [u8; 16],
    /// The generation the image was linked at.
    pub(crate) generation: u64,
    pub(crate) slide: u64,
    /// The cell holding the current table's address.
    pub(crate) table: u64,
    pub(crate) slots: u64,
    pub(crate) code: Range<u64>,
    pub(crate) data: Range<u64>,
    pub(crate) dir: [u64; 2],
}

impl Layout {
    /// The layout of the descriptor found in memory at `at`.
    pub(crate) fn new(descriptor: &Descriptor, at: u64) -> Result<Layout, String> {
        if descriptor.version != DESCRIPTOR_VERSION {
            return Err(format!(
                "{DESCRIPTOR} is of version {}, where this blaze-live reads {DESCRIPTOR_VERSION}",
                descriptor.version
            ));
        }
        if descriptor.arch != HOST_ARCH {
            return Err(format!(
                "{DESCRIPTOR} is for architecture {}, where this program runs on {}",
                descriptor.arch,
                std::env::consts::ARCH
            ));
        }
        let [code_start, code_end] = descriptor.patch_code;
        let [data_start, data_end] = descriptor.patch_data;
        if code_start > code_end || data_start > data_end {
            return Err(format!(
                "{DESCRIPTOR} has a patch region that ends before it starts: code {code_start:#x}..\
                 {code_end:#x}, data {data_start:#x}..{data_end:#x}"
            ));
        }
        if descriptor.slots > MOST_SLOTS {
            return Err(format!("{DESCRIPTOR} has {} slots", descriptor.slots));
        }
        if descriptor.dir[1] > MOST_DIR {
            return Err(format!("{DESCRIPTOR} names a directory {} bytes long", descriptor.dir[1]));
        }
        let slide = at.wrapping_sub(descriptor.this);
        if !descriptor.table.wrapping_add(slide).is_multiple_of(8) {
            return Err(format!(
                "{DESCRIPTOR}'s table cell at {:#x} is not aligned",
                descriptor.table
            ));
        }
        Ok(Layout {
            id: descriptor.id,
            generation: descriptor.generation,
            slide,
            table: descriptor.table,
            slots: descriptor.slots,
            code: code_start..code_end,
            data: data_start..data_end,
            dir: descriptor.dir,
        })
    }

    pub(crate) fn slid(&self, address: u64) -> u64 {
        address.wrapping_add(self.slide)
    }

    /// Where the patch code region is in memory.
    pub(crate) fn code_in_memory(&self) -> Range<u64> {
        self.slid(self.code.start)..self.slid(self.code.end)
    }

    /// Where the table cell is in memory.
    pub(crate) fn cell(&self) -> u64 {
        self.slid(self.table)
    }
}

/// How the platform's unwinder takes DWARF call frame information.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Dwarf {
    /// `__register_frame` of a run of entries ending in a zero length (libgcc, and LLVM's
    /// libunwind on ELF).
    Sections,
    /// `__register_frame` of one FDE (Apple's libunwind).
    Entries,
    /// None: the platform unwinds by other tables.
    Not,
}

/// One write of a patch: `len` bytes of its message from `from`, to `at` in memory. 24 bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Span {
    pub(crate) at: u64,
    pub(super) from: usize,
    pub(super) len: usize,
}

const _: () = assert!(size_of::<Span>() == 24);

impl Span {
    pub(crate) fn end(&self) -> u64 {
        self.at + self.len as u64
    }

    pub(crate) fn range(&self) -> Range<u64> {
        self.at..self.end()
    }

    #[cfg(test)]
    pub(crate) fn new(at: u64, from: usize, len: usize) -> Span {
        Span { at, from, len }
    }
}

/// What a platform writes, every address in memory: spans of the message's bytes sorted by
/// address, none empty and none overlapping, and the addresses within them of 8-byte fields
/// holding an image address, to add the slide to (sorted, each inside one span).
pub(crate) struct Writes<'a> {
    pub(super) message: &'a [u8],
    pub(crate) spans: &'a [Span],
    pub(super) rebase: &'a [u64],
    pub(super) slide: u64,
}

impl<'a> Writes<'a> {
    #[cfg(test)]
    pub(crate) fn new(
        message: &'a [u8],
        spans: &'a [Span],
        rebase: &'a [u64],
        slide: u64,
    ) -> Writes<'a> {
        Writes { message, spans, rebase, slide }
    }

    pub(crate) fn bytes(&self, span: &Span) -> &'a [u8] {
        &self.message[span.from..span.from + span.len]
    }
}

/// What a platform does to the memory of a running image. Every address is in memory
/// (slid). The switch has checked that code is in the patch code region, that data is
/// outside it, the cell and the loader's table, and that neither is over lasting memory
/// (the module's rules). Whether data is in the image is the platform's to check. The
/// platform is also where the switch reads the image: the cell and the loader's table.
pub(crate) trait Memory {
    const DWARF: Dwarf;
    /// Whether the unwinder takes Windows `RUNTIME_FUNCTION`s.
    const SEH: bool;
    /// What the table cell at `cell` (8-aligned, `Layout::new` checked) holds, by one
    /// acquire load.
    fn cell(&self, cell: u64) -> u64;
    /// Checks that `range` is in the image, as the platform checks data.
    fn in_image(&self, range: &Range<u64>) -> Result<(), String>;
    /// The image's bytes at `range`, which the platform checks are in the image and
    /// readable: read-only data the linker wrote, such as the directory's name.
    fn bytes(&self, range: &Range<u64>) -> Result<&'static [u8], String>;
    /// The loader's table, `slots` addresses at `at`, which [`Switch::new`] checked is
    /// 8-aligned and in the image. Nothing writes it: the switch stores only to the cell and
    /// its own tables, and refuses data over it.
    fn loaders(&self, at: u64, slots: u64) -> &[u64];
    /// Writes into the patch code region and makes it runnable again.
    fn code(&mut self, writes: &Writes<'_>) -> Result<(), String>;
    /// Writes data anywhere in the image with [`Writes::store`], making read-only pages
    /// writable for the write and back.
    fn data(&mut self, writes: &Writes<'_>) -> Result<(), String>;
    /// Registers with the unwinder what [`Memory::DWARF`] says it takes, at each address.
    fn frames(&mut self, starts: &[u64]) -> Result<(), String>;
    /// Adds `RUNTIME_FUNCTION`s (begin, end, unwind information).
    fn functions(&mut self, functions: &[(u64, u64, u64)]) -> Result<(), String>;
    /// Stores `table` in the table cell `cell` with one release store. An error means the
    /// cell may or may not hold `table`.
    fn publish(&mut self, cell: u64, table: u64) -> Result<(), String>;
}

/// Every generation the program ran, by index (0 the one it started at), in columns: 29 bytes
/// each, 20 per cell its patch set and 16 per write of code. Lookups by index are direct.
struct Log {
    number: Vec<u64>,
    /// The generation it was applied on; its own index for the first.
    parent: Vec<u32>,
    depth: Vec<u32>,
    /// Whether it is kept to roll back to.
    kept: Vec<bool>,
    /// Generation `g`'s cells are `cells[g]..cells[g + 1]` of `slot`, `new` and `old`.
    cells: Vec<u32>,
    slot: Vec<u32>,
    /// A slot's target in memory in the generation, and in its parent.
    new: Vec<u64>,
    old: Vec<u64>,
    /// Generation `g`'s code is `code_at[g]..code_at[g + 1]` of `code`, in memory.
    code_at: Vec<u32>,
    code: Vec<Range<u64>>,
}

impl Log {
    fn new(number: u64) -> Log {
        Log {
            number: vec![number],
            parent: vec![0],
            depth: vec![0],
            kept: vec![true],
            cells: vec![0, 0],
            slot: Vec::new(),
            new: Vec::new(),
            old: Vec::new(),
            code_at: vec![0, 0],
            code: Vec::new(),
        }
    }

    fn cells(&self, generation: u32) -> Range<usize> {
        let g = generation as usize;
        self.cells[g] as usize..self.cells[g + 1] as usize
    }

    fn code(&self, generation: u32) -> &[Range<u64>] {
        let g = generation as usize;
        &self.code[self.code_at[g] as usize..self.code_at[g + 1] as usize]
    }

    /// Ends a generation whose cells and code were pushed after the last one's; its index.
    fn finish(&mut self, number: u64, parent: u32) -> u32 {
        let generation = self.number.len() as u32;
        self.number.push(number);
        self.parent.push(parent);
        self.depth.push(self.depth[parent as usize] + 1);
        self.kept.push(false);
        self.cells.push(self.slot.len() as u32);
        self.code_at.push(self.code.len() as u32);
        generation
    }
}

/// What a lasting range holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Holds {
    Code,
    Unwind,
}

/// Memory that never changes again, in memory, sorted and none overlapping: the code of
/// every generation whose unwind entries were registered or that the cell may have pointed
/// at (a thread can be in it however long ago the program left it), and every unwind entry
/// registered (the unwinder reads it from then on, and not every platform can take one
/// back). Each range is of one generation, by number.
#[derive(Default)]
struct Lasting {
    ranges: Vec<(Range<u64>, u64, Holds)>,
}

impl Lasting {
    /// The first lasting range `range` overlaps: a binary search.
    fn hit(&self, range: &Range<u64>) -> Option<&(Range<u64>, u64, Holds)> {
        let at = self.ranges.partition_point(|it| it.0.end <= range.start);
        self.ranges.get(at).filter(|it| it.0.start < range.end)
    }

    /// Adds `range` of generation `number`, merged with what it overlaps: only ranges of the
    /// same generation can, since a patch over a lasting range is refused.
    fn add(&mut self, range: Range<u64>, number: u64, holds: Holds) {
        if range.is_empty() {
            return;
        }
        let at = self.ranges.partition_point(|it| it.0.end <= range.start);
        let (mut merged, mut holds, mut end) = (range, holds, at);
        while let Some((next, _, other)) = self.ranges.get(end).filter(|it| it.0.start < merged.end)
        {
            merged = merged.start.min(next.start)..merged.end.max(next.end);
            if *other == Holds::Code {
                holds = Holds::Code;
            }
            end += 1;
        }
        self.ranges.splice(at..end, [(merged, number, holds)]);
    }
}

/// Why a write of `kind` over `hit` is refused, in linked addresses.
fn over(kind: &str, hit: &(Range<u64>, u64, Holds), slide: u64) -> String {
    let (range, number, holds) = hit;
    let (start, end) = (range.start.wrapping_sub(slide), range.end.wrapping_sub(slide));
    match holds {
        Holds::Code => format!(
            "{kind} over generation {number}'s code at {start:#x}..{end:#x}, which the program \
             can be running"
        ),
        Holds::Unwind => format!(
            "{kind} over generation {number}'s unwind entries at {start:#x}..{end:#x}, which \
             the unwinder reads"
        ),
    }
}

/// A table of the switch's own: the generation whose slots it holds, and the count of
/// publishes when the cell last pointed at it.
struct Pooled {
    table: Box<[AtomicU64]>,
    holds: u32,
    at: u64,
}

/// Buffers a patch is checked and written with, kept from one to the next.
#[derive(Default)]
struct Scratch {
    code: Vec<Span>,
    data: Vec<Span>,
    rebase: Vec<u64>,
    slots: Vec<u32>,
    fdes: Vec<(u64, u64)>,
    starts: Vec<u64>,
    /// The zero length ending each run of entries registered, for `Dwarf::Sections`.
    ends: Vec<u64>,
    functions: Vec<(u64, u64, u64)>,
    /// Each RUNTIME_FUNCTION's unwind information.
    unwinds: Vec<Range<u64>>,
    path: Vec<u32>,
}

/// The generations of a running image and which one its table cell points at.
pub(crate) struct Switch<M: Memory> {
    pub(crate) memory: M,
    layout: Layout,
    /// The generation the cell points at.
    current: u32,
    log: Log,
    /// The generations kept, the oldest applied first.
    kept: Vec<u32>,
    /// The loader's table, which holds generation 0.
    image: u64,
    pool: Vec<Pooled>,
    /// The table the cell points at: the image's, or one of the pool.
    on: Option<usize>,
    published: u64,
    lasting: Lasting,
    scratch: Scratch,
}

impl<M: Memory> Switch<M> {
    pub(crate) fn new(memory: M, layout: Layout) -> Result<Switch<M>, String> {
        let cell = layout.cell();
        memory.in_image(&(cell..cell + 8))?;
        let table = memory.cell(cell);
        if table == 0 || !table.is_multiple_of(8) {
            return Err(format!("the jump table cell at {cell:#x} holds {table:#x}"));
        }
        let end = layout.slots.checked_mul(8).and_then(|len| table.checked_add(len));
        let end =
            end.ok_or_else(|| format!("a jump table at {table:#x} past the end of memory"))?;
        memory.in_image(&(table..end))?;
        let log = Log::new(layout.generation);
        Ok(Switch {
            memory,
            layout,
            current: 0,
            log,
            kept: vec![0],
            image: table,
            pool: Vec::with_capacity(POOL),
            on: None,
            published: 0,
            lasting: Lasting::default(),
            scratch: Scratch::default(),
        })
    }

    pub(crate) fn current(&self) -> u64 {
        self.log.number[self.current as usize]
    }

    /// Takes one message: what became of it, and what to answer the linker, if anything.
    pub(crate) fn take(&mut self, message: &MessageView<'_>) -> (Outcome, Option<Answer>) {
        match *message {
            MessageView::Patch(patch) => {
                let generation = patch.generation;
                let mut scratch = std::mem::take(&mut self.scratch);
                let applied = self.apply(patch, &mut scratch);
                self.scratch = scratch;
                match applied {
                    Ok(()) => (
                        Outcome::Applied {
                            generation,
                            functions: patch.functions,
                            statics: patch.statics,
                        },
                        Some(Answer::Applied { generation }),
                    ),
                    Err(why) => declined(generation, why),
                }
            }
            MessageView::RollBack { id, to } => {
                if let Err(why) = self.ours(&id) {
                    return declined(to, why);
                }
                let from = self.current();
                match self.roll_back(to) {
                    Ok(()) => {
                        let why = "the linker took it back".to_owned();
                        (
                            Outcome::RolledBack { generation: from, to, why: why.clone() },
                            Some(Answer::RolledBack { generation: from, to, why }),
                        )
                    }
                    Err(why) => declined(to, why),
                }
            }
            MessageView::Restart { id, generation, why } => match self.ours(&id) {
                Ok(()) => (Outcome::Restart { generation, why: why.to_owned() }, None),
                Err(other) => declined(generation, other),
            },
        }
    }

    /// [`Switch::take`] of an owned message, as the linker writes it.
    #[cfg(test)]
    pub(crate) fn handle(&mut self, message: crate::patch::Message) -> (Outcome, Option<Answer>) {
        let bytes = message.encode();
        let view = crate::patch::Message::view(&bytes).expect("an encoded message reads back");
        self.take(&view)
    }

    fn ours(&self, id: &[u8; 16]) -> Result<(), String> {
        if *id == self.layout.id {
            Ok(())
        } else {
            Err(format!("for image {}, where this program runs {}", hex(id), hex(&self.layout.id)))
        }
    }

    /// The kept generation numbered `number`.
    fn find(&self, number: u64) -> Option<u32> {
        self.kept.iter().copied().find(|&it| self.log.number[it as usize] == number)
    }

    /// The current generation and the kept ones it was applied on, down to the first or the
    /// first no longer kept.
    fn lineage(&self) -> impl Iterator<Item = u32> + '_ {
        let mut at = Some(self.current);
        std::iter::from_fn(move || {
            let generation = at.filter(|it| self.log.kept[*it as usize])?;
            let parent = self.log.parent[generation as usize];
            at = (parent != generation).then_some(parent);
            Some(generation)
        })
    }

    /// Checks the whole patch, then writes it and switches the table to it. A patch that
    /// fails a check leaves the program as it was.
    fn apply(&mut self, patch: PatchView<'_>, s: &mut Scratch) -> Result<(), String> {
        self.ours(&patch.id)?;
        let current = self.current();
        if patch.base != current {
            return Err(format!(
                "a patch on generation {}, where this program is at {current}",
                patch.base
            ));
        }
        if patch.generation <= patch.base {
            return Err(format!(
                "generation {} on base {}: a generation counts up from its base",
                patch.generation, patch.base
            ));
        }
        if self.find(patch.generation).is_some() {
            return Err(format!("generation {} was applied already", patch.generation));
        }
        let layout = &self.layout;
        let slide = layout.slide;
        let message = patch.message;
        s.code.clear();
        for (at, bytes) in patch.code {
            let range = span(at, bytes.len())?;
            if !inside(&range, &layout.code) {
                return Err(format!(
                    "code at {:#x}..{:#x}, outside the patch code region {:#x}..{:#x}",
                    range.start, range.end, layout.code.start, layout.code.end
                ));
            }
            if !bytes.is_empty() {
                let placed = place(message, at, bytes, slide)?;
                if let Some(hit) = self.lasting.hit(&placed.range()) {
                    return Err(over("code", hit, slide));
                }
                s.code.push(placed);
            }
        }
        let cell = layout.table..layout.table.saturating_add(8);
        let loaders = self.image..self.image.saturating_add(layout.slots.saturating_mul(8));
        s.data.clear();
        for (at, bytes) in patch.data {
            let range = span(at, bytes.len())?;
            if overlap(&range, &layout.code) {
                return Err(format!(
                    "data at {:#x}..{:#x}, in the patch code region",
                    range.start, range.end
                ));
            }
            if overlap(&range, &cell) {
                return Err(format!("data over the jump table's cell at {:#x}", cell.start));
            }
            if !bytes.is_empty() {
                let placed = place(message, at, bytes, slide)?;
                if overlap(&placed.range(), &loaders) {
                    return Err(format!(
                        "data over the loader's jump table at {:#x}",
                        loaders.start.wrapping_sub(slide)
                    ));
                }
                if let Some(hit) = self.lasting.hit(&placed.range()) {
                    return Err(over("data", hit, slide));
                }
                s.data.push(placed);
            }
        }
        // Code is inside the code region and data outside it, so only writes of one kind can
        // overlap.
        for spans in [&mut s.code, &mut s.data] {
            spans.sort_unstable_by_key(|it| it.at);
            if let Some(pair) = spans.windows(2).find(|it| it[1].at < it[0].end()) {
                return Err(format!("two writes at {:#x}", pair[1].at.wrapping_sub(slide)));
            }
        }
        s.slots.clear();
        for (slot, _) in patch.cells() {
            if u64::from(slot) >= layout.slots {
                return Err(format!("slot {slot}, where the table has {}", layout.slots));
            }
            s.slots.push(slot);
        }
        s.slots.sort_unstable();
        if let Some(pair) = s.slots.windows(2).find(|it| it[0] == it[1]) {
            return Err(format!("slot {} set twice", pair[0]));
        }
        s.rebase.clear();
        s.rebase.extend(patch.rebase().map(|it| it.wrapping_add(slide)));
        s.rebase.sort_unstable();
        if let Some(pair) = s.rebase.windows(2).find(|it| it[0] == it[1]) {
            return Err(format!("the address at {:#x} rebased twice", pair[0].wrapping_sub(slide)));
        }
        for &at in &s.rebase {
            if find(message, &s.code, at, 8).is_none() && find(message, &s.data, at, 8).is_none() {
                return Err(format!(
                    "an address to rebase at {:#x}, in none of the patch's writes",
                    at.wrapping_sub(slide)
                ));
            }
        }
        let (fdes, starts, ends) = (&mut s.fdes, &mut s.starts, &mut s.ends);
        frames::<M>(patch, message, &s.code, &s.data, slide, fdes, starts, ends)?;
        if patch.windows().len() != 0 && !M::SEH {
            return Err("RUNTIME_FUNCTIONs, which this platform's unwinder doesn't take".to_owned());
        }
        s.functions.clear();
        s.unwinds.clear();
        for (begin, end, unwind) in patch.windows() {
            let (slid_begin, slid_unwind) = (begin.wrapping_add(slide), unwind.wrapping_add(slide));
            if begin >= end || find(message, &s.code, slid_begin, end - begin).is_none() {
                return Err(format!("a RUNTIME_FUNCTION of {begin:#x}..{end:#x}, not in the code"));
            }
            let written = |len: u64| {
                find(message, &s.code, slid_unwind, len)
                    .or_else(|| find(message, &s.data, slid_unwind, len))
            };
            let Some(header) = written(4) else {
                return Err(format!("unwind information at {unwind:#x}, in none of the writes"));
            };
            let len = unwind_len(header);
            if written(len).is_none() {
                return Err(format!(
                    "unwind information at {unwind:#x} runs past the patch's writes ({len} bytes)"
                ));
            }
            s.functions.push((slid_begin, end.wrapping_add(slide), slid_unwind));
            s.unwinds.push(slid_unwind..slid_unwind + len);
        }
        let cells = patch.cells();
        if u32::try_from(self.log.slot.len() + cells.len()).is_err()
            || u32::try_from(self.log.code.len() + s.code.len()).is_err()
            || u32::try_from(self.log.number.len()).is_err()
        {
            return Err("more generations, cells or code than this program counts".to_owned());
        }

        let rebase = &s.rebase;
        self.memory.code(&Writes { message, spans: &s.code, rebase, slide })?;
        self.memory.data(&Writes { message, spans: &s.data, rebase, slide })?;
        // From here the unwinder or a thread may read what this generation wrote, whatever
        // becomes of the rest.
        let number = patch.generation;
        for span in &s.code {
            self.lasting.add(span.range(), number, Holds::Code);
        }
        for &(at, len) in &s.fdes {
            self.lasting.add(at..at + len, number, Holds::Unwind);
        }
        for &at in &s.ends {
            self.lasting.add(at..at + 4, number, Holds::Unwind);
        }
        for unwind in &s.unwinds {
            self.lasting.add(unwind.clone(), number, Holds::Unwind);
        }
        self.memory.frames(&s.starts)?;
        self.memory.functions(&s.functions)?;

        for (slot, target) in cells {
            let old = self.value(slot);
            self.log.slot.push(slot);
            self.log.new.push(target.wrapping_add(slide));
            self.log.old.push(old);
        }
        self.log.code.extend(s.code.iter().map(Span::range));
        let generation = self.log.finish(patch.generation, self.current);
        let at = self.standby(generation, &mut s.path);
        self.publish(Some(at))?;
        self.log.kept[generation as usize] = true;
        self.kept.push(generation);
        self.current = generation;
        while self.kept.len() > KEPT {
            let Some(oldest) = self.kept.iter().position(|it| *it != self.current) else {
                break;
            };
            let evicted = self.kept.remove(oldest);
            self.log.kept[evicted as usize] = false;
        }
        Ok(())
    }

    /// The loader's table, which the cell pointed at when the program started.
    fn image(&self) -> &[u64] {
        self.memory.loaders(self.image, self.layout.slots)
    }

    /// The target of `slot` in the table the cell points at.
    fn value(&self, slot: u32) -> u64 {
        match self.on {
            None => self.image()[slot as usize],
            Some(at) => self.pool[at].table[slot as usize].load(Ordering::Relaxed),
        }
    }

    /// A table of the pool brought to `target`: one holding it already, a new one while the
    /// pool isn't full, else the one the cell left longest ago.
    fn standby(&mut self, target: u32, path: &mut Vec<u32>) -> usize {
        if let Some(at) = self.pool.iter().position(|it| it.holds == target) {
            return at;
        }
        let at = if self.pool.len() < POOL {
            let table: Box<[AtomicU64]> = match self.on {
                None => self.image().iter().map(|it| AtomicU64::new(*it)).collect(),
                Some(on) => {
                    let table = &self.pool[on].table;
                    table.iter().map(|it| AtomicU64::new(it.load(Ordering::Relaxed))).collect()
                }
            };
            self.pool.push(Pooled { table, holds: self.current, at: self.published });
            self.pool.len() - 1
        } else {
            (0..self.pool.len())
                .filter(|it| Some(*it) != self.on)
                .min_by_key(|it| self.pool[*it].at)
                .expect("a pool of more than one table")
        };
        self.retarget(at, target, path);
        at
    }

    /// Brings the pool's table `at` from the generation it holds to `to`: the generations
    /// from the one it holds down to where the two meet are undone, the newest first, and
    /// those from there up to `to` redone, the oldest first. Each slot is one relaxed store,
    /// which the release store of the cell publishes.
    fn retarget(&mut self, at: usize, to: u32, path: &mut Vec<u32>) {
        let log = &self.log;
        let table = &self.pool[at].table;
        let undo = |generation: u32| {
            for cell in log.cells(generation) {
                table[log.slot[cell] as usize].store(log.old[cell], Ordering::Relaxed);
            }
        };
        let depth = |generation: u32| log.depth[generation as usize];
        let parent = |generation: u32| log.parent[generation as usize];
        let (mut from, mut onto) = (self.pool[at].holds, to);
        path.clear();
        while depth(from) > depth(onto) {
            undo(from);
            from = parent(from);
        }
        while depth(onto) > depth(from) {
            path.push(onto);
            onto = parent(onto);
        }
        while from != onto {
            undo(from);
            from = parent(from);
            path.push(onto);
            onto = parent(onto);
        }
        for &generation in path.iter().rev() {
            for cell in log.cells(generation) {
                table[log.slot[cell] as usize].store(log.new[cell], Ordering::Relaxed);
            }
        }
        self.pool[at].holds = to;
    }

    /// Points the cell at the image's table (`None`) or the pool's table `on`.
    fn publish(&mut self, on: Option<usize>) -> Result<(), String> {
        let table = match on {
            None => self.image,
            Some(at) => self.pool[at].table.as_ptr() as u64,
        };
        let stored = self.memory.publish(self.layout.cell(), table);
        self.published += 1;
        if let Some(at) = on {
            // After a failed store the cell may point at it: as the one used last, it is the
            // last of the pool to be written again.
            self.pool[at].at = self.published;
        }
        stored?;
        self.on = on;
        Ok(())
    }

    /// Points the cell at the table of `to`, a kept generation.
    pub(crate) fn roll_back(&mut self, to: u64) -> Result<(), String> {
        let Some(target) = self.find(to) else {
            let kept: Vec<String> =
                self.kept.iter().map(|it| self.log.number[*it as usize].to_string()).collect();
            return Err(format!("generation {to} is not kept; kept are {}", kept.join(", ")));
        };
        if target == self.current {
            return Ok(());
        }
        let on = if target == 0 {
            None
        } else {
            let mut path = std::mem::take(&mut self.scratch.path);
            let at = self.standby(target, &mut path);
            self.scratch.path = path;
            Some(at)
        };
        self.publish(on)?;
        self.current = target;
        Ok(())
    }

    /// The newest generation of the current lineage with code at one of `pcs` (in memory),
    /// and its base, when that is kept to go back to.
    pub(crate) fn tie(&self, pcs: &[u64]) -> Option<(u64, u64)> {
        let log = &self.log;
        self.lineage()
            .filter(|it| {
                let parent = log.parent[*it as usize];
                parent != *it && log.kept[parent as usize]
            })
            .filter(|it| log.code(*it).iter().any(|range| pcs.iter().any(|pc| range.contains(pc))))
            .map(|it| (log.number[it as usize], log.number[log.parent[it as usize] as usize]))
            .max()
    }

    /// Runs `f`; a panic in the code of a generation rolls that generation back to its base
    /// and is told to `answer` and returned, any other is resumed as it was.
    pub(crate) fn guard<R>(
        &mut self,
        f: impl FnOnce() -> R,
        answer: &mut dyn FnMut(&Answer),
    ) -> Result<R, Outcome> {
        let (payload, panic) = match hook::catch(f) {
            Ok(result) => return Ok(result),
            Err(caught) => caught,
        };
        let tied = panic.as_ref().and_then(|it| self.tie(&it.pcs));
        let (Some(panic), Some((generation, to))) = (panic, tied) else {
            std::panic::resume_unwind(payload);
        };
        if let Err(why) = self.roll_back(to) {
            let why = format!("{}; rolling back to generation {to}: {why}", panic.why);
            answer(&Answer::Declined { generation, why });
            std::panic::resume_unwind(payload);
        }
        drop(payload);
        answer(&Answer::RolledBack { generation, to, why: panic.why.clone() });
        Err(Outcome::RolledBack { generation, to, why: panic.why })
    }

    #[cfg(test)]
    pub(crate) fn kept(&self) -> Vec<u64> {
        self.kept.iter().map(|it| self.log.number[*it as usize]).collect()
    }

    /// How many tables of its own the switch has.
    #[cfg(test)]
    pub(crate) fn pooled(&self) -> usize {
        self.pool.len()
    }

    /// The address of the table of the generation numbered `number`, if one holds it.
    #[cfg(test)]
    pub(crate) fn table_of(&self, number: u64) -> Option<u64> {
        let generation = self.find(number)?;
        if generation == 0 {
            return Some(self.image);
        }
        let at = self.pool.iter().position(|it| it.holds == generation)?;
        Some(self.pool[at].table.as_ptr() as u64)
    }
}

impl<M: Memory> Drop for Switch<M> {
    fn drop(&mut self) {
        // The cell points at one of them and calls keep going through it: none is freed.
        for pooled in self.pool.drain(..) {
            Box::leak(pooled.table);
        }
    }
}

fn declined(generation: u64, why: String) -> (Outcome, Option<Answer>) {
    (Outcome::Declined { generation, why: why.clone() }, Some(Answer::Declined { generation, why }))
}

/// The span of `bytes`, a slice of `message`, written at `at` as linked.
fn place(message: &[u8], at: u64, bytes: &[u8], slide: u64) -> Result<Span, String> {
    let slid = at.wrapping_add(slide);
    if slid.checked_add(bytes.len() as u64).is_none() {
        return Err(format!("a write at {at:#x} of {} bytes, past the end of memory", bytes.len()));
    }
    let from = bytes.as_ptr() as usize - message.as_ptr() as usize;
    Ok(Span { at: slid, from, len: bytes.len() })
}

/// Checks the FDEs of `patch` against the bytes it writes (`code` and `data`) and puts in
/// `starts` what to register at, in memory: each FDE, or the start of each run of entries
/// that ends in a zero length, and in `ends` where each such zero length is.
#[allow(clippy::too_many_arguments)]
fn frames<M: Memory>(
    patch: PatchView<'_>,
    message: &[u8],
    code: &[Span],
    data: &[Span],
    slide: u64,
    fdes: &mut Vec<(u64, u64)>,
    starts: &mut Vec<u64>,
    ends: &mut Vec<u64>,
) -> Result<(), String> {
    fdes.clear();
    starts.clear();
    ends.clear();
    if patch.frames().len() == 0 {
        return Ok(());
    }
    if M::DWARF == Dwarf::Not {
        return Err("DWARF unwind entries, which this platform's unwinder doesn't take".to_owned());
    }
    let linked = |at: u64| at.wrapping_sub(slide);
    let bytes =
        |at: u64, len: u64| find(message, code, at, len).or_else(|| find(message, data, at, len));
    let length = |at: u64| -> Result<u32, String> {
        let field = bytes(at, 4).ok_or_else(|| {
            format!("unwind entries reach {:#x}, in none of the patch's writes", linked(at))
        })?;
        Ok(le32(field))
    };
    fdes.extend(patch.frames().map(|(at, len)| (at.wrapping_add(slide), len)));
    fdes.sort_unstable();
    if let Some(pair) = fdes.windows(2).find(|it| it[1].0 < it[0].0.saturating_add(it[0].1)) {
        return Err(format!("two unwind entries at {:#x}", linked(pair[1].0)));
    }
    for &(at, len) in fdes.iter() {
        let field = length(at)?;
        if field == 0 || field == u32::MAX || u64::from(field) + 4 != len {
            return Err(format!(
                "the unwind entry at {:#x} is {len} bytes by the patch, and its length says {}",
                linked(at),
                u64::from(field) + 4
            ));
        }
        if bytes(at, len).is_none() {
            return Err(format!(
                "the unwind entry at {:#x} runs past the patch's writes",
                linked(at)
            ));
        }
    }
    match M::DWARF {
        Dwarf::Entries | Dwarf::Not => starts.extend(fdes.iter().map(|it| it.0)),
        Dwarf::Sections => {
            let mut next = 0;
            while next < fdes.len() {
                let start = fdes[next].0;
                let mut at = start;
                loop {
                    let field = length(at)?;
                    if field == 0 {
                        ends.push(at);
                        break;
                    }
                    if field == u32::MAX {
                        return Err(format!("a 64-bit unwind entry at {:#x}", linked(at)));
                    }
                    if next < fdes.len() && fdes[next].0 == at {
                        next += 1;
                    } else if next < fdes.len() && fdes[next].0 < at {
                        return Err(format!(
                            "the unwind entry at {:#x} is not where the entries before it end",
                            linked(fdes[next].0)
                        ));
                    }
                    at = at.checked_add(4 + u64::from(field)).ok_or_else(|| {
                        format!("unwind entries from {:#x} overflow", linked(start))
                    })?;
                }
                starts.push(start);
            }
        }
    }
    Ok(())
}

/// The `len` bytes at `at` of whichever of `spans` (sorted) has all of them: a binary search.
fn find<'m>(message: &'m [u8], spans: &[Span], at: u64, len: u64) -> Option<&'m [u8]> {
    let span = spans.get(spans.partition_point(|it| it.end() <= at))?;
    let offset = at.checked_sub(span.at)?;
    let end = offset.checked_add(len)?;
    (end <= span.len as u64)
        .then(|| &message[span.from + offset as usize..span.from + end as usize])
}

/// The length of the UNWIND_INFO whose first 4 bytes are `header`: the fixed part, the unwind
/// codes (2 bytes each, an even count), then the handler's address or the chained
/// RUNTIME_FUNCTION. Not counted: a handler's data after its address, whose length only the
/// handler knows.
fn unwind_len(header: &[u8]) -> u64 {
    const EHANDLER: u8 = 1;
    const UHANDLER: u8 = 2;
    const CHAININFO: u8 = 4;
    let flags = header[0] >> 3;
    let codes = u64::from(header[2]).next_multiple_of(2);
    let after = if flags & CHAININFO != 0 {
        12
    } else if flags & (EHANDLER | UHANDLER) != 0 {
        4
    } else {
        0
    };
    4 + 2 * codes + after
}

fn span(at: u64, len: usize) -> Result<Range<u64>, String> {
    at.checked_add(len as u64)
        .map(|end| at..end)
        .ok_or_else(|| format!("a write at {at:#x} of {len} bytes, past the end of memory"))
}

fn inside(range: &Range<u64>, region: &Range<u64>) -> bool {
    range.start >= region.start && range.end <= region.end
}

fn overlap(a: &Range<u64>, b: &Range<u64>) -> bool {
    a.start < b.end && b.start < a.end
}

fn le32(bytes: &[u8]) -> u32 {
    let mut value = [0; 4];
    value.copy_from_slice(&bytes[..4]);
    u32::from_le_bytes(value)
}

pub(super) fn le64(bytes: &[u8]) -> u64 {
    let mut value = [0; 8];
    value.copy_from_slice(&bytes[..8]);
    u64::from_le_bytes(value)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|it| format!("{it:02x}")).collect()
}

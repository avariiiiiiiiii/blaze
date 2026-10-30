//! Switching an image's jump table between generations, apart from how a platform writes
//! its memory ([`Memory`]).

use std::{
    collections::{HashSet, VecDeque},
    ops::Range,
    sync::atomic::{AtomicU64, Ordering},
};

use super::{Outcome, hook};
use crate::patch::{
    ARCH_AARCH64, ARCH_X86_64, Answer, DESCRIPTOR, DESCRIPTOR_VERSION, Descriptor, Message, Patch,
};

/// How many generations are kept to roll back to, as Relax keeps.
pub(crate) const KEPT: usize = 64;
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

/// What a platform does to the memory of a running image. Every address is in memory
/// (slid), and the switch has checked each is where the descriptor allows.
pub(crate) trait Memory {
    const DWARF: Dwarf;
    /// Whether the unwinder takes Windows `RUNTIME_FUNCTION`s.
    const SEH: bool;
    /// Writes into the patch code region and makes it runnable again.
    fn code(&mut self, writes: &[(u64, Vec<u8>)]) -> Result<(), String>;
    /// Writes data anywhere in the image, making read-only pages writable for the write and
    /// back.
    fn data(&mut self, writes: &[(u64, Vec<u8>)]) -> Result<(), String>;
    /// Registers with the unwinder what [`Memory::DWARF`] says it takes, at each address.
    fn frames(&mut self, starts: &[u64]) -> Result<(), String>;
    /// Adds `RUNTIME_FUNCTION`s (begin, end, unwind information).
    fn functions(&mut self, functions: &[(u64, u64, u64)]) -> Result<(), String>;
    /// Stores `table` in the table cell `cell` with one release store. An error means the
    /// cell may or may not hold `table`.
    fn publish(&mut self, cell: u64, table: u64) -> Result<(), String>;
}

enum Table {
    /// The table the loader slid, in the image.
    Image(u64),
    Own(Box<[u64]>),
}

impl Table {
    fn address(&self) -> u64 {
        match self {
            Table::Image(address) => *address,
            Table::Own(table) => table.as_ptr() as u64,
        }
    }
}

struct Generation {
    number: u64,
    /// The generation it was applied on; its own number for the one the program started at.
    base: u64,
    table: Table,
    /// The code it added, as linked.
    code: Vec<Range<u64>>,
}

/// The generations of a running image and which one its table cell points at.
pub(crate) struct Switch<M: Memory> {
    pub(crate) memory: M,
    layout: Layout,
    current: u64,
    /// The oldest applied first.
    kept: Vec<Generation>,
    /// Tables no generation has any more, with the count of tables published when they were
    /// let go of: each is freed once [`KEPT`] more have been.
    retired: VecDeque<(u64, Box<[u64]>)>,
    published: u64,
}

impl<M: Memory> Switch<M> {
    pub(crate) fn new(memory: M, layout: Layout) -> Result<Switch<M>, String> {
        let cell = layout.cell();
        // SAFETY: the cell is the descriptor's, 8-aligned (`Layout::new` checked) and in the
        // image's data, mapped as long as the image is. Only this switch stores to it.
        let table = unsafe { AtomicU64::from_ptr(cell as *mut u64) }.load(Ordering::Acquire);
        if table == 0 || !table.is_multiple_of(8) {
            return Err(format!("the jump table cell at {cell:#x} holds {table:#x}"));
        }
        let start = layout.generation;
        Ok(Switch {
            memory,
            layout,
            current: start,
            kept: vec![Generation {
                number: start,
                base: start,
                table: Table::Image(table),
                code: Vec::new(),
            }],
            retired: VecDeque::new(),
            published: 0,
        })
    }

    pub(crate) fn current(&self) -> u64 {
        self.current
    }

    /// Takes one message: what became of it, and what to answer the linker, if anything.
    pub(crate) fn handle(&mut self, message: Message) -> (Outcome, Option<Answer>) {
        match message {
            Message::Patch(patch) => {
                let generation = patch.generation;
                match self.apply(&patch) {
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
            Message::RollBack { id, to } => {
                if let Err(why) = self.ours(&id) {
                    return declined(to, why);
                }
                let from = self.current;
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
            Message::Restart { id, generation, why } => match self.ours(&id) {
                Ok(()) => (Outcome::Restart { generation, why }, None),
                Err(other) => declined(generation, other),
            },
        }
    }

    fn ours(&self, id: &[u8; 16]) -> Result<(), String> {
        if *id == self.layout.id {
            Ok(())
        } else {
            Err(format!("for image {}, where this program runs {}", hex(id), hex(&self.layout.id)))
        }
    }

    fn find(&self, number: u64) -> Option<&Generation> {
        self.kept.iter().find(|it| it.number == number)
    }

    /// The current generation and the kept ones it was applied on, down to the first or the
    /// first no longer kept.
    fn lineage(&self) -> Vec<u64> {
        let mut lineage = Vec::new();
        let mut at = self.current;
        while let Some(generation) = self.find(at) {
            lineage.push(at);
            if generation.base >= generation.number {
                break;
            }
            at = generation.base;
        }
        lineage
    }

    /// Checks the whole patch, then writes it and switches the table to it. A patch that
    /// fails a check leaves the program as it was.
    fn apply(&mut self, patch: &Patch) -> Result<(), String> {
        self.ours(&patch.id)?;
        if patch.base != self.current {
            return Err(format!(
                "a patch on generation {}, where this program is at {}",
                patch.base, self.current
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
        let code_ranges: Vec<Range<u64>> = patch
            .code
            .iter()
            .map(|(at, bytes)| span(*at, bytes.len()))
            .collect::<Result<_, _>>()?;
        for range in &code_ranges {
            if !inside(range, &layout.code) {
                return Err(format!(
                    "code at {:#x}..{:#x}, outside the patch code region {:#x}..{:#x}",
                    range.start, range.end, layout.code.start, layout.code.end
                ));
            }
        }
        let data_ranges: Vec<Range<u64>> = patch
            .data
            .iter()
            .map(|(at, bytes)| span(*at, bytes.len()))
            .collect::<Result<_, _>>()?;
        let cell = layout.table..layout.table + 8;
        for range in &data_ranges {
            if overlap(range, &layout.code) {
                return Err(format!(
                    "data at {:#x}..{:#x}, in the patch code region",
                    range.start, range.end
                ));
            }
            if overlap(range, &cell) {
                return Err(format!("data over the jump table's cell at {:#x}", cell.start));
            }
        }
        let mut all: Vec<Range<u64>> =
            code_ranges.iter().chain(&data_ranges).filter(|it| !it.is_empty()).cloned().collect();
        all.sort_by_key(|it| it.start);
        for pair in all.windows(2) {
            if pair[1].start < pair[0].end {
                return Err(format!("two writes at {:#x}", pair[1].start));
            }
        }
        let lineage = self.lineage();
        for generation in self.kept.iter().filter(|it| lineage.contains(&it.number)) {
            for range in &generation.code {
                if code_ranges.iter().any(|it| overlap(it, range)) {
                    return Err(format!(
                        "code over generation {}'s at {:#x}..{:#x}, which the program can be \
                         running",
                        generation.number, range.start, range.end
                    ));
                }
            }
        }
        let mut slots = HashSet::new();
        for &(slot, _) in &patch.cells {
            if u64::from(slot) >= layout.slots {
                return Err(format!("slot {slot}, where the table has {}", layout.slots));
            }
            if !slots.insert(slot) {
                return Err(format!("slot {slot} set twice"));
            }
        }

        let mut code = patch.code.clone();
        let mut data = patch.data.clone();
        let mut rebased = HashSet::new();
        for &at in &patch.rebase {
            if !rebased.insert(at) {
                return Err(format!("the address at {at:#x} rebased twice"));
            }
            let field = match find_mut(&mut code, at, 8) {
                Some(field) => field,
                None => find_mut(&mut data, at, 8).ok_or_else(|| {
                    format!("an address to rebase at {at:#x}, in none of the patch's writes")
                })?,
            };
            let mut value = [0; 8];
            value.copy_from_slice(field);
            let value = u64::from_le_bytes(value).wrapping_add(layout.slide);
            field.copy_from_slice(&value.to_le_bytes());
        }
        let frames = frames::<M>(patch, &code, &data)?;
        if !patch.windows.is_empty() && !M::SEH {
            return Err("RUNTIME_FUNCTIONs, which this platform's unwinder doesn't take".to_owned());
        }
        for &(begin, end, unwind) in &patch.windows {
            if begin >= end || !code_ranges.iter().any(|it| inside(&(begin..end), it)) {
                return Err(format!("a RUNTIME_FUNCTION of {begin:#x}..{end:#x}, not in the code"));
            }
            if !all.iter().any(|it| inside(&(unwind..unwind.saturating_add(4)), it)) {
                return Err(format!("unwind information at {unwind:#x}, in none of the writes"));
            }
        }

        // What was rolled back and this overwrites can't be gone back to, nor what was
        // applied on it.
        let mut dropped: Vec<u64> = self
            .kept
            .iter()
            .filter(|it| !lineage.contains(&it.number))
            .filter(|it| it.code.iter().any(|old| code_ranges.iter().any(|new| overlap(new, old))))
            .map(|it| it.number)
            .collect();
        loop {
            let more: Vec<u64> = self
                .kept
                .iter()
                .filter(|it| it.base < it.number && dropped.contains(&it.base))
                .filter(|it| !dropped.contains(&it.number))
                .map(|it| it.number)
                .collect();
            if more.is_empty() {
                break;
            }
            dropped.extend(more);
        }
        for number in dropped {
            self.forget(number);
        }

        let slide = self.layout.slide;
        let slid = |writes: Vec<(u64, Vec<u8>)>| -> Vec<(u64, Vec<u8>)> {
            writes.into_iter().map(|(at, bytes)| (at.wrapping_add(slide), bytes)).collect()
        };
        self.memory.code(&slid(code))?;
        self.memory.data(&slid(data))?;
        let frames: Vec<u64> = frames.iter().map(|it| it.wrapping_add(slide)).collect();
        self.memory.frames(&frames)?;
        let functions: Vec<(u64, u64, u64)> = patch
            .windows
            .iter()
            .map(|&(begin, end, unwind)| {
                (begin.wrapping_add(slide), end.wrapping_add(slide), unwind.wrapping_add(slide))
            })
            .collect();
        self.memory.functions(&functions)?;

        let mut table: Box<[u64]> = self.entries(self.current).into();
        for &(slot, target) in &patch.cells {
            table[slot as usize] = target.wrapping_add(slide);
        }
        if let Err(why) = self.publish(table.as_ptr() as u64) {
            // The cell may point at it: it is never freed.
            Box::leak(table);
            return Err(why);
        }
        self.kept.push(Generation {
            number: patch.generation,
            base: patch.base,
            table: Table::Own(table),
            code: code_ranges,
        });
        self.current = patch.generation;
        while self.kept.len() > KEPT {
            let Some(oldest) = self.kept.iter().position(|it| it.number != self.current) else {
                break;
            };
            let generation = self.kept.remove(oldest);
            self.retire(generation.table);
        }
        Ok(())
    }

    /// The entries of the table of a kept generation.
    fn entries(&self, number: u64) -> &[u64] {
        let slots = self.layout.slots as usize;
        match self.find(number).map(|it| &it.table) {
            Some(Table::Own(table)) => table,
            // SAFETY: the loader's table, which the cell pointed at when the program started:
            // `slots` code addresses (the descriptor's count), 8-aligned (`Switch::new`
            // checked), in the image, mapped as long as it is. Nothing writes it.
            Some(Table::Image(address)) => unsafe {
                std::slice::from_raw_parts(*address as *const u64, slots)
            },
            None => &[],
        }
    }

    /// Points the cell at the table of `to`, a kept generation.
    pub(crate) fn roll_back(&mut self, to: u64) -> Result<(), String> {
        let Some(table) = self.find(to).map(|it| it.table.address()) else {
            let kept: Vec<String> = self.kept.iter().map(|it| it.number.to_string()).collect();
            return Err(format!("generation {to} is not kept; kept are {}", kept.join(", ")));
        };
        if to == self.current {
            return Ok(());
        }
        self.publish(table)?;
        self.current = to;
        Ok(())
    }

    fn publish(&mut self, table: u64) -> Result<(), String> {
        self.memory.publish(self.layout.cell(), table)?;
        self.published += 1;
        while self.retired.front().is_some_and(|(at, _)| self.published >= at + KEPT as u64) {
            // SAFETY of the free: a call loads the cell and then one slot of the table it
            // found, a few instructions apart. This table left the cell at least `KEPT`
            // publishes ago, each in a separate `poll` or `guard` of the program's.
            self.retired.pop_front();
        }
        Ok(())
    }

    fn forget(&mut self, number: u64) {
        if let Some(at) = self.kept.iter().position(|it| it.number == number) {
            let generation = self.kept.remove(at);
            self.retire(generation.table);
        }
    }

    fn retire(&mut self, table: Table) {
        if let Table::Own(table) = table {
            self.retired.push_back((self.published, table));
        }
    }

    /// The newest generation of the current lineage with code at one of `pcs` (in memory),
    /// and its base, when that is kept to go back to.
    pub(crate) fn tie(&self, pcs: &[u64]) -> Option<(u64, u64)> {
        let slide = self.layout.slide;
        self.lineage()
            .into_iter()
            .filter_map(|number| self.find(number))
            .filter(|it| it.base < it.number && self.find(it.base).is_some())
            .filter(|it| {
                let unslid = |pc: &u64| pc.wrapping_sub(slide);
                it.code.iter().any(|range| pcs.iter().any(|pc| range.contains(&unslid(pc))))
            })
            .map(|it| (it.number, it.base))
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
        self.kept.iter().map(|it| it.number).collect()
    }

    #[cfg(test)]
    pub(crate) fn retired(&self) -> usize {
        self.retired.len()
    }
}

impl<M: Memory> Drop for Switch<M> {
    fn drop(&mut self) {
        // The cell points at one of them and calls keep going through it: none is freed.
        for generation in self.kept.drain(..) {
            if let Table::Own(table) = generation.table {
                Box::leak(table);
            }
        }
        for (_, table) in self.retired.drain(..) {
            Box::leak(table);
        }
    }
}

fn declined(generation: u64, why: String) -> (Outcome, Option<Answer>) {
    (Outcome::Declined { generation, why: why.clone() }, Some(Answer::Declined { generation, why }))
}

/// Checks the FDEs of `patch` against the bytes it writes (`code` and `data`, as linked) and
/// says what to register at: each FDE, or the start of each run of entries that ends in a
/// zero length.
fn frames<M: Memory>(
    patch: &Patch,
    code: &[(u64, Vec<u8>)],
    data: &[(u64, Vec<u8>)],
) -> Result<Vec<u64>, String> {
    if patch.frames.is_empty() {
        return Ok(Vec::new());
    }
    if M::DWARF == Dwarf::Not {
        return Err("DWARF unwind entries, which this platform's unwinder doesn't take".to_owned());
    }
    let bytes = |at: u64, len: u64| find(code, at, len).or_else(|| find(data, at, len));
    let length = |at: u64| -> Result<u32, String> {
        let field = bytes(at, 4).ok_or_else(|| {
            format!("unwind entries reach {at:#x}, in none of the patch's writes")
        })?;
        let mut value = [0; 4];
        value.copy_from_slice(field);
        Ok(u32::from_le_bytes(value))
    };
    let mut fdes = patch.frames.clone();
    fdes.sort_unstable();
    for pair in fdes.windows(2) {
        if pair[1].0 < pair[0].0.saturating_add(pair[0].1) {
            return Err(format!("two unwind entries at {:#x}", pair[1].0));
        }
    }
    for &(at, len) in &fdes {
        let field = length(at)?;
        if field == 0 || field == u32::MAX || u64::from(field) + 4 != len {
            return Err(format!(
                "the unwind entry at {at:#x} is {len} bytes by the patch, and its length says {}",
                u64::from(field) + 4
            ));
        }
        if bytes(at, len).is_none() {
            return Err(format!("the unwind entry at {at:#x} runs past the patch's writes"));
        }
    }
    match M::DWARF {
        Dwarf::Entries | Dwarf::Not => Ok(fdes.iter().map(|it| it.0).collect()),
        Dwarf::Sections => {
            let mut starts = Vec::new();
            let mut next = 0;
            while next < fdes.len() {
                let start = fdes[next].0;
                let mut at = start;
                loop {
                    let field = length(at)?;
                    if field == 0 {
                        break;
                    }
                    if field == u32::MAX {
                        return Err(format!("a 64-bit unwind entry at {at:#x}"));
                    }
                    if next < fdes.len() && fdes[next].0 == at {
                        next += 1;
                    } else if next < fdes.len() && fdes[next].0 < at {
                        return Err(format!(
                            "the unwind entry at {:#x} is not where the entries before it end",
                            fdes[next].0
                        ));
                    }
                    at = at
                        .checked_add(4 + u64::from(field))
                        .ok_or_else(|| format!("unwind entries from {start:#x} overflow"))?;
                }
                starts.push(start);
            }
            Ok(starts)
        }
    }
}

/// The `len` bytes at `at` of whichever of `writes` has all of them.
fn find(writes: &[(u64, Vec<u8>)], at: u64, len: u64) -> Option<&[u8]> {
    writes.iter().find_map(|(start, bytes)| {
        let offset = at.checked_sub(*start)?;
        let end = offset.checked_add(len)?;
        (end <= bytes.len() as u64).then(|| &bytes[offset as usize..end as usize])
    })
}

fn find_mut(writes: &mut [(u64, Vec<u8>)], at: u64, len: u64) -> Option<&mut [u8]> {
    writes.iter_mut().find_map(|(start, bytes)| {
        let offset = at.checked_sub(*start)?;
        let end = offset.checked_add(len)?;
        (end <= bytes.len() as u64).then(|| &mut bytes[offset as usize..end as usize])
    })
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

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|it| format!("{it:02x}")).collect()
}

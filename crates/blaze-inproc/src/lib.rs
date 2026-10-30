//! Dev runs and tests with no binary on disk (docs/v2/plan/design/compile-time.md change 6,
//! docs/v2/plan/lanes/fastcompile/inproc.md).
//!
//! The daemon loads code straight into the process it runs. A [`CodeMemory`] is one OS memory
//! object (Linux `memfd_create`, macOS a Mach memory entry, Windows a pagefile-backed section)
//! mapped twice: writable in the daemon, executable in every run process. Changing code is a
//! write into the daemon's view, [`CodeMemory::publish`] (the instruction-cache maintenance
//! the architecture asks for), and one aligned atomic store of the word that makes it
//! reachable ([`CodeMemory::store_word`]): no `mprotect`, no link, no file.
//!
//! Replaced code stays valid while a thread may be in it. [`CodeMemory::retire`] puts a range
//! behind a fence ([`fence`]): it is reused only once every thread of every run process has
//! passed a quiescent point after the store that unlinked it with no frame of its stack in
//! the range, and every live thread is known to the fence.
//!
//! A run process starts from blaze's own loader (the `blaze-inproc-run` binary, [`load`]),
//! which the daemon spawns through `blaze-core`'s `Effect::Spawn` (the `host` feature,
//! `host`). It is handed the memory over its [`channel::Channel`], maps the program's segments
//! from it, links the program to the system's libraries, sets up thread-local storage and
//! jumps in. The program's runtime ([`runtime`]) reports quiescent points and speaks to the
//! daemon over the same channel in fixed-layout little-endian frames ([`frame`]).
//!
//! The crate knows nothing of stubs, functions or dependency graphs: `blaze-lazy` and
//! `blaze-premap` build those on it.
//!
//! Linux on x86-64 and AArch64, macOS on AArch64 and Windows on x86-64. Elsewhere every
//! constructor stops with an [`Error`] naming the platform.

pub mod channel;
pub mod fence;
pub mod frame;
#[cfg(feature = "host")]
pub mod host;
pub mod load;
pub mod runtime;

mod alloc;
mod sys;

#[cfg(test)]
mod tests;

use std::fmt;

use fence::{Fences, Grace};

/// Why something of this crate could not be done, and on which platform.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Error {
    pub what: String,
}

impl Error {
    pub(crate) fn new(what: impl Into<String>) -> Error {
        Error { what: what.into() }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "blaze-inproc on {}-{}: {}",
            std::env::consts::OS,
            std::env::consts::ARCH,
            self.what
        )
    }
}

impl std::error::Error for Error {}

/// The object format of a program image a run process loads.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Format {
    Elf,
    MachO,
    Pe,
}

impl Format {
    pub(crate) fn tag(self) -> u8 {
        match self {
            Format::Elf => 1,
            Format::MachO => 2,
            Format::Pe => 3,
        }
    }

    pub(crate) fn from_tag(tag: u8) -> Option<Format> {
        match tag {
            1 => Some(Format::Elf),
            2 => Some(Format::MachO),
            3 => Some(Format::Pe),
            _ => None,
        }
    }

    /// The format of programs on this platform.
    pub fn host() -> Option<Format> {
        if cfg!(target_os = "linux") {
            Some(Format::Elf)
        } else if cfg!(target_os = "macos") {
            Some(Format::MachO)
        } else if cfg!(windows) {
            Some(Format::Pe)
        } else {
            None
        }
    }
}

/// How a range of a memory object is mapped in a run process.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Access {
    /// Readable and executable, shared with the daemon's writable view: code.
    Execute,
    /// Readable, shared: read-only data the daemon may still change.
    Read,
    /// Readable and writable, a private copy made on first write: a program's data. What the
    /// daemon writes later shows only in pages the process has not written.
    Copy,
    /// Readable and writable, shared both ways: the fence's control words.
    Shared,
}

impl Access {
    pub(crate) fn tag(self) -> u8 {
        match self {
            Access::Execute => 1,
            Access::Read => 2,
            Access::Copy => 3,
            Access::Shared => 4,
        }
    }

    pub(crate) fn from_tag(tag: u8) -> Option<Access> {
        match tag {
            1 => Some(Access::Execute),
            2 => Some(Access::Read),
            3 => Some(Access::Copy),
            4 => Some(Access::Shared),
            _ => None,
        }
    }
}

/// A range of a [`CodeMemory`], by offset in its object.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct CodeRange {
    pub offset: u64,
    pub len: u64,
}

impl CodeRange {
    pub fn end(&self) -> u64 {
        self.offset + self.len
    }

    pub fn contains(&self, offset: u64) -> bool {
        offset >= self.offset && offset < self.end()
    }
}

/// A range of a memory object a run process maps at a fixed address before the program's
/// own image: `blaze-premap`'s dependency image, mapped the same in every run.
#[derive(Clone, Copy)]
pub struct FixedMap<'a> {
    pub memory: &'a CodeMemory,
    pub range: CodeRange,
    /// Where in the run process; a multiple of [`CodeMemory::page`]. Taken already is an error
    /// naming the address, never another address.
    pub at: u64,
    pub access: Access,
}

/// What the loader keeps aside in every thread's static thread-local block for a program's
/// thread-locals (ELF and PE; Mach-O's are reached through descriptors the loader fills).
/// The linker places the program's thread-locals at `bias + offset`: from the thread
/// pointer on ELF, from the start of the module's block on PE.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct TlsReserve {
    pub bias: i64,
    pub size: u64,
    pub align: u64,
}

impl TlsReserve {
    /// As `blaze-inproc-run --reserve` prints it: `bias size align` in decimal.
    pub fn decode(text: &[u8]) -> Option<TlsReserve> {
        let text = std::str::from_utf8(text).ok()?;
        let mut words = text.split_ascii_whitespace();
        let reserve = TlsReserve {
            bias: words.next()?.parse().ok()?,
            size: words.next()?.parse().ok()?,
            align: words.next()?.parse().ok()?,
        };
        (words.next().is_none() && reserve.align.is_power_of_two()).then_some(reserve)
    }

    pub fn encode(&self) -> String {
        format!("{} {} {}\n", self.bias, self.size, self.align)
    }
}

/// The run processes a code memory is mapped in, and where.
struct Process {
    pid: u32,
    /// Its threads, as the OS listed them at the last [`CodeMemory::collect`]; reused.
    threads: Vec<u64>,
}

/// One memory object of code, writable here and executable in each run process it is mapped
/// in, at the same address in each ([`CodeMemory::base`]).
///
/// Its first `image` bytes are the program image as the linker lays out its file (a file
/// offset is an offset here); the `arena` after them is handed out by
/// [`CodeMemory::reserve`] for code placed while the program runs.
pub struct CodeMemory {
    object: sys::Object,
    view: sys::View,
    control: sys::Object,
    fences: Fences<sys::View>,
    image: u64,
    arena: alloc::Arena,
    /// Ranges retired behind a fence: with their grace once their entry is in the control
    /// table, waiting for room in it before.
    limbo: Vec<(Option<(Grace, u32)>, CodeRange)>,
    base: Option<u64>,
    processes: Vec<Process>,
    page: u64,
}

impl CodeMemory {
    /// A new memory object of `image + arena` bytes (each rounded up to [`CodeMemory::page`]),
    /// zero, mapped writable here. Pages take memory only once written.
    pub fn new(name: &str, image: u64, arena: u64) -> Result<CodeMemory, Error> {
        let page = sys::page();
        let image = image.checked_next_multiple_of(page).ok_or_else(|| Error::new("image size overflows"))?;
        let arena = arena.checked_next_multiple_of(page).ok_or_else(|| Error::new("arena size overflows"))?;
        let len = image.checked_add(arena).ok_or_else(|| Error::new("code memory size overflows"))?;
        if len == 0 {
            return Err(Error::new("a code memory of no bytes"));
        }
        sys::check_icache()?;
        let object = sys::Object::create(name, len)?;
        let view = object.map_here(Access::Shared)?;
        let control_len = fence::CONTROL_BYTES.next_multiple_of(page);
        let control = sys::Object::create(&format!("{name}.fence"), control_len)?;
        let control_view = control.map_here(Access::Shared)?;
        let fences = Fences::new(control_view);
        Ok(CodeMemory {
            object,
            view,
            control,
            fences,
            image,
            arena: alloc::Arena::new(image, arena),
            limbo: Vec::new(),
            base: None,
            processes: Vec::new(),
            page,
        })
    }

    /// The whole object's length.
    pub fn len(&self) -> u64 {
        self.view.len() as u64
    }

    pub fn is_empty(&self) -> bool {
        self.view.len() == 0
    }

    /// The image part's length: the linker writes `0..image_len()`.
    pub fn image_len(&self) -> u64 {
        self.image
    }

    /// The granule of a mapping: offsets and addresses of a [`FixedMap`] are multiples of it.
    pub fn page(&self) -> u64 {
        self.page
    }

    /// Writes `bytes` at `offset`: code no thread can reach yet (the image before its run,
    /// a reserved range before its slot is stored). Call [`CodeMemory::publish`] after.
    pub fn write(&mut self, offset: u64, bytes: &[u8]) -> Result<(), Error> {
        let range = self.checked(offset, bytes.len() as u64)?;
        self.view.bytes_mut(range)?.copy_from_slice(bytes);
        Ok(())
    }

    /// Sets `len` bytes at `offset` to `byte` (a trap, to fill what a retired range held).
    pub fn fill(&mut self, offset: u64, len: u64, byte: u8) -> Result<(), Error> {
        let range = self.checked(offset, len)?;
        self.view.bytes_mut(range)?.fill(byte);
        Ok(())
    }

    /// The bytes at `range`, as the daemon's view has them.
    pub fn bytes(&self, range: CodeRange) -> Result<&[u8], Error> {
        let range = self.checked(range.offset, range.len)?;
        self.view.bytes(range)
    }

    /// Makes what was written at `range` the code a thread runs once it reaches it: the
    /// data cache cleaned and the instruction cache invalidated to the point of unification
    /// on AArch64, nothing on x86-64 (each thread serializes at its next quiescent point).
    /// Call it after the writes and before the store that makes the range reachable.
    pub fn publish(&self, range: CodeRange) -> Result<(), Error> {
        let range = self.checked(range.offset, range.len)?;
        self.view.publish(range);
        Ok(())
    }

    /// Stores `value` in the aligned 8-byte word at `offset` with one release store: a slot,
    /// or the target word a stub loads. A thread that loads it sees the old value or the new,
    /// whole.
    pub fn store_word(&mut self, offset: u64, value: u64) -> Result<(), Error> {
        self.checked(offset, 8)?;
        if !offset.is_multiple_of(8) {
            return Err(Error::new(format!("a word store at {offset:#x}, which is not 8-byte aligned")));
        }
        self.view.word(offset as usize).store(value, std::sync::atomic::Ordering::Release);
        Ok(())
    }

    /// The aligned 8-byte word at `offset`.
    pub fn load_word(&self, offset: u64) -> Result<u64, Error> {
        self.checked(offset, 8)?;
        if !offset.is_multiple_of(8) {
            return Err(Error::new(format!("a word load at {offset:#x}, which is not 8-byte aligned")));
        }
        Ok(self.view.word(offset as usize).load(std::sync::atomic::Ordering::Acquire))
    }

    /// Stores the AArch64 instruction `instruction` at the aligned `offset` with one 4-byte
    /// store, then publishes its line: for a `B` or a `NOP`, which the architecture lets one
    /// thread change while another runs it. x86-64 code changes through target words
    /// ([`CodeMemory::store_word`]): there this is an error.
    pub fn store_instruction(&mut self, offset: u64, instruction: u32) -> Result<(), Error> {
        if !cfg!(target_arch = "aarch64") {
            return Err(Error::new(
                "an instruction store: x86-64 code changes through the target words its stubs load",
            ));
        }
        if !is_concurrently_modifiable(instruction) {
            return Err(Error::new(format!(
                "the instruction {instruction:#010x} at {offset:#x} is not one AArch64 lets a thread \
                 change while another runs it (B, BL, NOP, BRK, ISB, SVC)"
            )));
        }
        let range = self.checked(offset, 4)?;
        if !offset.is_multiple_of(4) {
            return Err(Error::new(format!("an instruction at {offset:#x}, which is not 4-byte aligned")));
        }
        self.view.word32(offset as usize).store(instruction, std::sync::atomic::Ordering::Release);
        self.view.publish(range);
        Ok(())
    }

    /// A range of the arena of `len` bytes aligned to `align` (a power of two), for code placed
    /// while the program runs. Full is an error saying how much is free and how much the
    /// fences still hold.
    pub fn reserve(&mut self, len: u64, align: u64) -> Result<CodeRange, Error> {
        if len == 0 || !align.is_power_of_two() {
            return Err(Error::new(format!("a reservation of {len} bytes aligned to {align}")));
        }
        if let Some(range) = self.arena.take(len, align) {
            return Ok(range);
        }
        self.collect()?;
        self.arena.take(len, align).ok_or_else(|| {
            Error::new(format!(
                "the code memory's arena is full: {len} bytes asked, {} free in pieces of at most {}, \
                 {} held by fences ({} ranges) until every thread of {} passes a quiescent point",
                self.arena.free(),
                self.arena.largest(),
                self.held(),
                self.limbo.len(),
                self.describe_processes(),
            ))
        })
    }

    /// Gives back a reserved range nothing could ever have reached (its slot was never
    /// stored): free at once.
    pub fn unreserve(&mut self, range: CodeRange) -> Result<(), Error> {
        self.arena_range(range)?;
        self.arena.give(range);
        Ok(())
    }

    /// Retires `range` of the arena, unlinked by stores already made: it is reused once every
    /// thread that could be in it has left it ([`CodeMemory::collect`]).
    pub fn retire(&mut self, range: CodeRange) -> Result<(), Error> {
        self.arena_range(range)?;
        if self.base.is_none() || self.processes.is_empty() {
            self.arena.give(range);
            return Ok(());
        }
        self.limbo.push((None, range));
        self.post();
        Ok(())
    }

    /// Frees every retired range whose fence has passed, and posts waiting ones to the
    /// control table. Says how many ranges it freed.
    pub fn collect(&mut self) -> Result<usize, Error> {
        if self.limbo.is_empty() {
            return Ok(0);
        }
        self.post();
        let births = self.fences.births();
        for process in &mut self.processes {
            sys::threads(process.pid, &mut process.threads)?;
        }
        let lists: Vec<(u32, &[u64])> =
            self.processes.iter().map(|it| (it.pid, it.threads.as_slice())).collect();
        let oldest = self.fences.oldest(births, &lists);
        let Some(oldest) = oldest else { return Ok(0) };
        let mut freed = 0;
        let mut index = 0;
        while index < self.limbo.len() {
            match self.limbo[index] {
                (Some((grace, entry)), range) if grace.passed_by(oldest) => {
                    self.fences.clear(entry);
                    self.arena.give(range);
                    self.limbo.swap_remove(index);
                    freed += 1;
                }
                _ => index += 1,
            }
        }
        if freed != 0 {
            self.post();
        }
        Ok(freed)
    }

    /// The bytes retired and not yet free.
    pub fn held(&self) -> u64 {
        self.limbo.iter().map(|(_, range)| range.len).sum()
    }

    /// Where offset 0 is in every run process, once one has started.
    pub fn base(&self) -> Option<u64> {
        self.base
    }

    /// The run-process address of `offset`.
    pub fn address(&self, offset: u64) -> Option<u64> {
        (offset < self.len()).then(|| self.base.map(|base| base + offset)).flatten()
    }

    /// The offset of the run-process address `address`.
    pub fn offset_of(&self, address: u64) -> Option<u64> {
        let offset = address.checked_sub(self.base?)?;
        (offset < self.len()).then_some(offset)
    }

    /// Records that the run process `pid` mapped the memory at `base`. Every process maps it at
    /// one base: the first sets it, and a later one elsewhere is an error.
    pub fn attach(&mut self, pid: u32, base: u64) -> Result<(), Error> {
        match self.base {
            Some(at) if at != base => {
                return Err(Error::new(format!(
                    "process {pid} mapped the code memory at {base:#x}, where the others have it at {at:#x}"
                )));
            }
            _ => self.base = Some(base),
        }
        if !self.processes.iter().any(|it| it.pid == pid) {
            self.processes.push(Process { pid, threads: Vec::new() });
        }
        Ok(())
    }

    /// Records that the run process `pid` exited: its threads hold no fence any more.
    pub fn exited(&mut self, pid: u32) {
        self.processes.retain(|it| it.pid != pid);
        self.fences.forget(pid);
        if self.processes.is_empty() {
            for (entry, range) in std::mem::take(&mut self.limbo) {
                if let Some((_, entry)) = entry {
                    self.fences.clear(entry);
                }
                self.arena.give(range);
            }
        }
    }

    pub(crate) fn object(&self) -> &sys::Object {
        &self.object
    }

    pub(crate) fn control(&self) -> &sys::Object {
        &self.control
    }

    /// Posts what waits in limbo to the control table, under one grace.
    fn post(&mut self) {
        let Some(base) = self.base else { return };
        let mut posted = false;
        for (entry, range) in &mut self.limbo {
            if entry.is_some() {
                continue;
            }
            let Some(slot) = self.fences.post(base + range.offset, base + range.end()) else { break };
            *entry = Some((Grace::NONE, slot));
            posted = true;
        }
        if posted {
            let grace = self.fences.start();
            for (entry, _) in &mut self.limbo {
                if let Some((g, _)) = entry
                    && *g == Grace::NONE
                {
                    *g = grace;
                }
            }
        }
    }

    fn describe_processes(&self) -> String {
        let pids: Vec<String> = self.processes.iter().map(|it| it.pid.to_string()).collect();
        format!("processes {}", pids.join(", "))
    }

    fn checked(&self, offset: u64, len: u64) -> Result<std::ops::Range<usize>, Error> {
        match offset.checked_add(len) {
            Some(end) if end <= self.len() => Ok(offset as usize..end as usize),
            _ => Err(Error::new(format!(
                "{len} bytes at {offset:#x} of a code memory of {:#x} bytes",
                self.len()
            ))),
        }
    }

    fn arena_range(&self, range: CodeRange) -> Result<(), Error> {
        if range.offset < self.image || range.end() > self.len() || range.len == 0 {
            return Err(Error::new(format!(
                "{:#x}..{:#x} is not in the arena {:#x}..{:#x}",
                range.offset,
                range.end(),
                self.image,
                self.len()
            )));
        }
        Ok(())
    }
}

/// Whether AArch64 lets one thread change `instruction` while another may run it (Arm ARM
/// B2.2.5, "concurrent modification and execution of instructions").
fn is_concurrently_modifiable(instruction: u32) -> bool {
    let b_or_bl = instruction & 0x7c00_0000 == 0x1400_0000;
    let nop = instruction == 0xd503_201f;
    let isb = instruction == 0xd503_3fdf;
    let brk = instruction & 0xffe0_001f == 0xd420_0000;
    let svc = instruction & 0xffe0_001f == 0xd400_0001;
    b_or_bl || nop || isb || brk || svc
}

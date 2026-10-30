//! What the v1 runtime (`linux`, `macos`, `windows`) decides before it maps or writes
//! anything, with no unsafe and tested on every host: where a patch goes in the room, that
//! every write is in the image, which unwind entries the unwinder is given, and the writes a
//! patch makes over the image.
//!
//! Those writes (a jump over each function's entry, the statics, what an earlier patch wrote
//! put back) are made with every other thread of the program stopped ([`World`]): each
//! platform stops every thread but the caller and reads where each is. None is written while
//! a thread is strictly inside the bytes of one, since it would resume in the middle of a
//! jump; the threads are let go and stopped again, [`TRIES`] times, before the patch is
//! refused naming the function. A thread stopped at a function's first byte runs the whole
//! jump when it resumes, and one anywhere else never runs the bytes written. Nothing the
//! stopped threads may hold, the allocator's lock above all, is taken while they are
//! stopped: whatever allocates is done before, the originals are read into room reserved
//! for them, and a write that fails says so without formatting ([`Broke`]).
//!
//! What the runtime can't see from the pc alone is a return address or a branch target
//! inside the bytes a jump covers. On AArch64, where every instruction is four bytes, a call
//! among them is found in the original words and the patch refused ([`Overwrites::calls`]);
//! that no branch targets them, and on x86-64 that no call returns into them, is the linker's
//! to make sure of when it picks the functions it redirects (`v2/requests/link.md`).

#![forbid(unsafe_code)]

use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    time::Duration,
};

use crate::format::{Code, Process, Write};

/// How many times the threads are stopped for one patch before it is refused because one
/// is always inside the bytes it writes.
pub(crate) const TRIES: u32 = 20;
/// How long the threads run between two tries.
pub(crate) const BETWEEN: Duration = Duration::from_micros(500);
/// The most threads a program may run for the platform to stop them.
pub(crate) const MOST_THREADS: usize = 4096;
/// How long the threads have to stop before the patch is refused.
pub(crate) const STOP_WITHIN: Duration = Duration::from_millis(500);
/// A `RUNTIME_FUNCTION`: where a function starts, ends, and has its unwind information.
pub(crate) const FUNCTION: usize = 12;

/// Why a patch was not applied.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Failed {
    /// Nothing the program runs was written: it runs on as it was.
    Declined(String),
    /// Some writes over the image were made and the rest could not be: the program is half
    /// patched and has to be started again.
    Restart(String),
}

impl From<String> for Failed {
    fn from(why: String) -> Failed {
        Failed::Declined(why)
    }
}

impl From<&str> for Failed {
    fn from(why: &str) -> Failed {
        Failed::Declined(why.to_owned())
    }
}

/// Takes the room `code` is laid out in, `page`-aligned whole pages within
/// `process.next..process.end`, and moves `next` past it before anything is mapped there:
/// whatever becomes of the patch, no later one is laid out over memory a jump or the
/// unwinder may reach. Where the patch ends.
pub(crate) fn claim(process: &mut Process, code: &Code, page: u64) -> Result<u64, String> {
    let text = code.text.len() as u64;
    let len = text
        .checked_add(code.data.len() as u64)
        .and_then(|it| it.checked_next_multiple_of(page))
        .ok_or("a patch larger than memory")?;
    let end = code.base.checked_add(len).ok_or_else(|| {
        format!("a patch at {:#x} of {len:#x} bytes, past the end of memory", code.base)
    })?;
    if code.base < process.next || end > process.end {
        return Err(format!(
            "laid out at {:#x}..{end:#x}, where this program has no room ({:#x}..{:#x}): build \
             again",
            code.base, process.next, process.end
        ));
    }
    if !(code.base.is_multiple_of(page) && text.is_multiple_of(page)) {
        return Err(format!("a patch at {:#x} whose code is not whole pages", code.base));
    }
    process.next = end;
    Ok(end)
}

/// Checks that each write, at `slide` from where the patch says, is in `image`, in memory.
pub(crate) fn in_image(writes: &[Write], slide: u64, image: &Range<u64>) -> Result<(), String> {
    for write in writes {
        let start = write.address.checked_add(slide);
        let end = start.and_then(|it| it.checked_add(write.bytes.len() as u64));
        match (start, end) {
            (Some(start), Some(end)) if start >= image.start && end <= image.end => {}
            _ => {
                return Err(format!(
                    "a write at {:#x} of {} bytes, outside the executable at {:#x}..{:#x}",
                    write.address.wrapping_add(slide),
                    write.bytes.len(),
                    image.start,
                    image.end
                ));
            }
        }
    }
    Ok(())
}

/// Adds `slide` to the address stored at `at` of `bytes`.
pub(crate) fn slide_at(bytes: &mut [u8], at: u64, slide: u64) -> Result<(), String> {
    let field = usize::try_from(at)
        .ok()
        .and_then(|at| bytes.get_mut(at..at.checked_add(8)?))
        .ok_or_else(|| format!("an address at {at:#x}, past the end of the patch"))?;
    let value =
        u64::from_le_bytes(field.try_into().map_err(|_| "an address shorter than 8 bytes")?);
    field.copy_from_slice(&value.wrapping_add(slide).to_le_bytes());
    Ok(())
}

/// The unwind section at `frames` (offset and length in `text`), checked inside `text`.
fn section(text: &[u8], (start, len): (u64, u64)) -> Result<(usize, usize), String> {
    usize::try_from(start)
        .ok()
        .zip(usize::try_from(len).ok())
        .and_then(|(start, len)| Some((start, start.checked_add(len)?)))
        .filter(|&(_, end)| end <= text.len())
        .ok_or_else(|| {
            format!("unwind tables at {start:#x} of {len:#x} bytes, past the end of the patch")
        })
}

/// The length field of the entry at `at` in `text`, and, for one that is not the zero
/// length ending a run, where it ends, at most `end`.
fn entry(text: &[u8], at: usize, end: usize) -> Result<(u32, usize), String> {
    let length = text
        .get(at..at.checked_add(4).ok_or("an unwind entry past the end of memory")?)
        .ok_or_else(|| format!("an unwind entry at {at:#x}, past the end of the patch"))?;
    let length = u32::from_le_bytes(length.try_into().map_err(|_| "a short unwind length")?);
    if length == 0 {
        return Ok((0, at + 4));
    }
    if length == u32::MAX {
        return Err("64-bit unwind entries, which the patch format has none of".to_owned());
    }
    let after = (at + 4)
        .checked_add(length as usize)
        .filter(|&after| after <= end)
        .ok_or_else(|| format!("the unwind entry at {at:#x} ends past its section"))?;
    Ok((length, after))
}

/// Where the run of CIEs and FDEs at `frames` starts in `text`, for the unwinders that take
/// one run ending in a zero length (libgcc's `__register_frame`, and LLVM's libunwind on
/// ELF): checked to end in a zero length within the section or in the four bytes right after
/// it, inside `text`. None for a patch with no unwind tables.
pub(crate) fn frame_run(text: &[u8], frames: (u64, u64)) -> Result<Option<usize>, String> {
    if frames.1 == 0 {
        return Ok(None);
    }
    let (start, end) = section(text, frames)?;
    let mut at = start;
    loop {
        if at > end {
            return Err("unwind entries with no zero length ending them in the patch".to_owned());
        }
        let (length, after) = entry(text, at, end)?;
        if length == 0 {
            return Ok(Some(start));
        }
        at = after;
    }
}

/// Where each FDE at `frames` starts in `text`, for Apple's unwinder, which takes one FDE at
/// a time and reads its CIE through the FDE's pointer: each checked to end inside the
/// section, and its CIE pointer to name a CIE of the section before it.
pub(crate) fn fdes(text: &[u8], frames: (u64, u64)) -> Result<Vec<usize>, String> {
    let (start, end) = section(text, frames)?;
    let (mut cies, mut fdes) = (Vec::new(), Vec::new());
    let mut at = start;
    while at + 8 <= end {
        let (length, after) = entry(text, at, end)?;
        if length == 0 {
            break;
        }
        if length < 4 {
            return Err(format!("the unwind entry at {at:#x} is {length} bytes"));
        }
        let pointer = u32::from_le_bytes(text[at + 4..at + 8].try_into().expect("4 bytes"));
        if pointer == 0 {
            cies.push(at);
        } else {
            let cie = (at + 4).checked_sub(pointer as usize);
            if !cie.is_some_and(|cie| cie >= start && cies.contains(&cie)) {
                return Err(format!("the FDE at {at:#x} names no CIE of the patch"));
            }
            fdes.push(at);
        }
        at = after;
    }
    Ok(fdes)
}

/// How many `RUNTIME_FUNCTION`s the table at `functions` (offset and length in `memory`, the
/// patch's code and data, mapped at `patch` from where the image is loaded) has: checked to
/// be whole entries, 4-aligned, inside `memory`, and each to name a function and unwind
/// information inside the patch, for the unwinder reads them from where the image is loaded.
pub(crate) fn functions(memory: &[u8], functions: (u64, u64), patch: u64) -> Result<u32, String> {
    if functions.1 == 0 {
        return Ok(0);
    }
    let (start, end) = section(memory, functions)?;
    if !(end - start).is_multiple_of(FUNCTION) || !start.is_multiple_of(4) {
        return Err(format!("a table of functions at {start:#x} that is not whole entries"));
    }
    let inside =
        patch..patch.checked_add(memory.len() as u64).ok_or("a patch past the end of memory")?;
    let u32_at =
        |at: usize| u64::from(u32::from_le_bytes(memory[at..at + 4].try_into().expect("4 bytes")));
    for at in (start..end).step_by(FUNCTION) {
        let (begin, until, unwind) = (u32_at(at), u32_at(at + 4), u32_at(at + 8));
        let named = begin < until
            && inside.contains(&begin)
            && until <= inside.end
            && inside.contains(&unwind)
            && unwind + 4 <= inside.end;
        if !named {
            return Err(format!(
                "the RUNTIME_FUNCTION at {at:#x} names {begin:#x}..{until:#x} and {unwind:#x}, \
                 outside the patch at {:#x}..{:#x}",
                inside.start, inside.end
            ));
        }
    }
    u32::try_from((end - start) / FUNCTION).map_err(|_| "too many RUNTIME_FUNCTIONs".to_owned())
}

/// One write over the image, in memory.
pub(crate) struct Step {
    pub(crate) address: u64,
    pub(crate) bytes: Vec<u8>,
    /// A jump over a function's entry, rather than a static or something put back.
    pub(crate) jump: bool,
    /// How much of what was there first is known once this is read: what the original lacks
    /// up to it is read just before the writes.
    read_to: usize,
    /// How long the patch's write is, kept as what was last written here; none for one put
    /// back as it first was.
    keeps: Option<usize>,
}

/// The writes a patch makes over the image, each in it ([`in_image`]), and their ranges by
/// where they start with the furthest end of any up to each, to find a pc inside one.
pub(crate) struct Plan {
    pub(crate) steps: Vec<Step>,
    reach: Vec<(u64, u64, u64)>,
}

impl Plan {
    fn new(steps: Vec<Step>) -> Plan {
        let mut ranges: Vec<(u64, u64)> =
            steps.iter().map(|it| (it.address, it.address + it.bytes.len() as u64)).collect();
        ranges.sort_unstable();
        let mut reach = Vec::with_capacity(ranges.len());
        let (mut furthest, mut of) = (0, 0);
        for (start, end) in ranges {
            if end > furthest {
                (furthest, of) = (end, start);
            }
            reach.push((start, furthest, of));
        }
        Plan { steps, reach }
    }

    /// The first of `pcs` strictly inside a write, with where that write starts: a thread
    /// there would resume in the middle of what is written. It allocates nothing.
    pub(crate) fn inside(&self, pcs: &[u64]) -> Option<(u64, u64)> {
        pcs.iter().find_map(|&pc| {
            let before = self.reach.partition_point(|it| it.0 < pc).checked_sub(1)?;
            let (_, furthest, of) = self.reach[before];
            (pc < furthest).then_some((pc, of))
        })
    }
}

/// What the patches wrote over the image: what was there first, as long as the longest write
/// at each address, and how long the last patch's write there was.
#[derive(Default)]
pub(crate) struct Overwrites {
    originals: HashMap<u64, Vec<u8>>,
    written: HashMap<u64, usize>,
}

impl Overwrites {
    /// The writes `code` makes, `slide` from where it says (checked in the image by
    /// [`in_image`]): first what the last patch wrote and this one does not, put back as it
    /// first was, then the patch's own, each with what a longer write there before left
    /// behind put back after it. Reserves the room what is read of the originals takes.
    pub(crate) fn plan(&mut self, code: &Code, slide: u64) -> Result<Plan, String> {
        let kept: HashSet<u64> =
            code.writes.iter().map(|it| it.address.wrapping_add(slide)).collect();
        let mut undone: Vec<u64> =
            self.written.keys().copied().filter(|it| !kept.contains(it)).collect();
        undone.sort_unstable();
        let mut steps = Vec::with_capacity(undone.len() + code.writes.len());
        for address in undone {
            let original = &self.originals[&address];
            let (bytes, read_to) = (original.clone(), original.len());
            steps.push(Step { address, bytes, jump: false, read_to, keeps: None });
        }
        for (nth, write) in code.writes.iter().enumerate() {
            let mut bytes = write.bytes.clone();
            for &pointer in &write.pointers {
                slide_at(&mut bytes, pointer, slide)?;
            }
            let address = write.address.wrapping_add(slide);
            let original = self.originals.entry(address).or_default();
            let read_to = original.len().max(bytes.len());
            original.reserve(read_to - original.len());
            let last = self.written.get(&address).copied().unwrap_or(0);
            if last > bytes.len() {
                bytes.extend_from_slice(&original[bytes.len()..last]);
            }
            let jump = (nth as u64) < code.redirected;
            steps.push(Step { address, bytes, jump, read_to, keeps: Some(write.bytes.len()) });
        }
        Ok(Plan::new(steps))
    }

    /// Reads what the originals lack for `plan` by `read(address, into)`, called only with
    /// the bytes of a step. It allocates nothing: [`Overwrites::plan`] reserved the room.
    pub(crate) fn read(&mut self, plan: &Plan, mut read: impl FnMut(u64, &mut [u8])) {
        for step in &plan.steps {
            let Some(original) = self.originals.get_mut(&step.address) else { continue };
            let have = original.len();
            if have < step.read_to {
                original.resize(step.read_to, 0);
                read(step.address + have as u64, &mut original[have..]);
            }
        }
    }

    /// The first jump of `plan`, after [`Overwrites::read`], over a function whose
    /// instructions before the last word the jump covers have a call (`bl`, `blr` and the
    /// authenticating `blr`s): a thread in what it called would return into the middle of
    /// the jump. For AArch64, where every instruction is a four-byte word.
    pub(crate) fn calls(&self, plan: &Plan) -> Option<u64> {
        let call = |word: u32| {
            word & 0xfc00_0000 == 0x9400_0000
                || word & 0xffff_fc1f == 0xd63f_0000
                || word & 0xffff_f81f == 0xd63f_081f
                || word & 0xffff_f800 == 0xd73f_0800
        };
        plan.steps.iter().filter(|it| it.jump).find_map(|step| {
            let original = self.originals.get(&step.address)?;
            let words = original.get(..step.bytes.len())?.chunks_exact(4);
            let before_last = (step.bytes.len() / 4).saturating_sub(1);
            words
                .take(before_last)
                .any(|it| call(u32::from_le_bytes(it.try_into().expect("4 bytes"))))
                .then_some(step.address)
        })
    }

    /// Keeps what the first `done` writes of `plan` leave: put back, or written.
    pub(crate) fn record(&mut self, plan: &Plan, done: usize) {
        for step in &plan.steps[..done] {
            match step.keeps {
                Some(len) => self.written.insert(step.address, len),
                None => self.written.remove(&step.address),
            };
        }
    }
}

/// The program's threads but the caller, which a platform stops.
pub(crate) trait World {
    /// Stops every other thread of the process, calls `f` with where each is (its program
    /// counter), and lets them go on. `f` must not allocate, nor take a lock a thread may
    /// hold. An error says why they could not all be stopped, with all of them let go.
    fn stopped<T>(&mut self, f: impl FnOnce(&[u64]) -> T) -> Result<T, String>;
}

/// A write that failed with the threads stopped, said without allocating.
pub(crate) struct Broke {
    pub(crate) what: &'static str,
    pub(crate) at: u64,
    /// The platform's error code.
    pub(crate) code: i64,
    /// Whether the bytes were written before it failed (giving the protection back did).
    pub(crate) written: bool,
}

enum Round {
    Done,
    Inside(u64, u64),
    Calls(u64),
    Broke(usize, Broke),
}

/// Makes `plan`'s writes with every other thread stopped and none inside them, reading the
/// originals first. `read` reads the image at the bytes of a step and `write(n, step)` makes
/// step `n`; both are called only with the threads stopped, and must not allocate. `calls`
/// refuses a jump over a function whose first words call ([`Overwrites::calls`]).
pub(crate) fn write_stopped(
    world: &mut impl World,
    overwrites: &mut Overwrites,
    plan: &Plan,
    calls: bool,
    mut read: impl FnMut(u64, &mut [u8]),
    mut write: impl FnMut(usize, &Step) -> Result<(), Broke>,
) -> Result<(), Failed> {
    if plan.steps.is_empty() {
        return Ok(());
    }
    let mut inside = (0, 0);
    for tried in 0..TRIES {
        if tried != 0 {
            std::thread::sleep(BETWEEN);
        }
        let round = world.stopped(|pcs| {
            if let Some((pc, at)) = plan.inside(pcs) {
                return Round::Inside(pc, at);
            }
            overwrites.read(plan, &mut read);
            if calls && let Some(at) = overwrites.calls(plan) {
                return Round::Calls(at);
            }
            for (n, step) in plan.steps.iter().enumerate() {
                if let Err(broke) = write(n, step) {
                    return Round::Broke(n, broke);
                }
            }
            Round::Done
        })?;
        match round {
            Round::Done => {
                overwrites.record(plan, plan.steps.len());
                return Ok(());
            }
            Round::Inside(pc, at) => inside = (pc, at),
            Round::Calls(at) => {
                return Err(Failed::Declined(format!(
                    "the function at {at:#x} calls within the bytes its jump covers: a thread in \
                     what it calls would return into the middle of the jump"
                )));
            }
            Round::Broke(n, broke) => {
                let done = n + usize::from(broke.written);
                overwrites.record(plan, done);
                let why = format!(
                    "{} at {:#x}: error {}, after {done} of {} writes",
                    broke.what,
                    broke.at,
                    broke.code,
                    plan.steps.len()
                );
                return Err(if done == 0 { Failed::Declined(why) } else { Failed::Restart(why) });
            }
        }
    }
    let (pc, at) = inside;
    Err(Failed::Declined(format!(
        "a thread was at {pc:#x}, inside the bytes written over the function at {at:#x}, each of \
         {TRIES} times the threads were stopped: apply the patch again"
    )))
}

#[cfg(test)]
mod tests;

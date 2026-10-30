//! Writing a running ELF image on Linux (x86-64 and AArch64).
//!
//! At `start` the patch code region is moved onto a memory file mapped twice: read and run
//! where the image has it, and read and write at an address of its own. A patch's code is
//! written through the second, straight from its message, with no system call and no change
//! of protection, so no TLB shootdown and no moment a page of the region can't run: what JITs
//! that keep code W^X by mapping it twice do. The image's other mappings and their
//! protections are read from /proc/self/maps once, at start; a data write to read-only pages
//! makes each run of them writable once, writes and protects it again.

use std::{
    ffi::{CString, c_char, c_void},
    ops::Range,
    os::unix::ffi::OsStrExt,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

pub(super) use super::unix::{Map, backtrace, descriptor};
use super::{
    Events,
    switch::{Dwarf, Layout, Memory, Writes},
    unix::__register_frame,
};
use crate::patch::MESSAGE_PREFIX;

#[repr(C)]
#[allow(dead_code)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

unsafe extern "C" {
    fn mmap(
        addr: *mut c_void,
        len: usize,
        prot: i32,
        flags: i32,
        fd: i32,
        offset: i64,
    ) -> *mut c_void;
    fn munmap(addr: *mut c_void, len: usize) -> i32;
    fn mprotect(addr: *mut c_void, len: usize, prot: i32) -> i32;
    fn mremap(old: *mut c_void, old_len: usize, new_len: usize, flags: i32, ...) -> *mut c_void;
    fn sysconf(name: i32) -> i64;
    fn memfd_create(name: *const c_char, flags: u32) -> i32;
    fn ftruncate(fd: i32, len: i64) -> i32;
    fn close(fd: i32) -> i32;
    fn inotify_init1(flags: i32) -> i32;
    fn inotify_add_watch(fd: i32, path: *const c_char, mask: u32) -> i32;
    fn poll(fds: *mut PollFd, count: u64, timeout: i32) -> i32;
    fn read(fd: i32, buffer: *mut c_void, len: usize) -> isize;
}

const PROT_READ: i32 = 1;
const PROT_WRITE: i32 = 2;
const PROT_EXEC: i32 = 4;
const MAP_SHARED: i32 = 1;
const MAP_FAILED: *mut c_void = !0usize as *mut c_void;
const MREMAP_MAYMOVE: i32 = 1;
const MREMAP_FIXED: i32 = 2;
const SC_PAGESIZE: i32 = 30;
const MFD_CLOEXEC: u32 = 1;
const IN_NONBLOCK: i32 = 0o4000;
const IN_CLOEXEC: i32 = 0o2000000;
const IN_CLOSE_WRITE: u32 = 0x8;
const IN_MOVED_TO: u32 = 0x80;
const IN_Q_OVERFLOW: u32 = 0x4000;
const IN_IGNORED: u32 = 0x8000;
const WATCHED: u32 = IN_CLOSE_WRITE | IN_MOVED_TO;
const POLLIN: i16 = 1;
/// How long the watching thread blocks before it looks whether to stop.
const WAKE_EVERY_MS: i32 = 1000;

pub(super) struct Os {
    page: u64,
    /// The protection of the table cell's page.
    cell: i32,
    /// The patch code region in memory, and where it is mapped writable.
    code: Range<u64>,
    alias: u64,
    maps: Maps,
}

impl Os {
    pub(super) fn new(layout: &Layout) -> Result<Os, String> {
        // SAFETY: it takes no pointer.
        let page = unsafe { sysconf(SC_PAGESIZE) };
        let page =
            u64::try_from(page).ok().filter(|it| it.is_power_of_two()).ok_or("no page size")?;
        let code = layout.code_in_memory();
        if !code.start.is_multiple_of(page) || !code.end.is_multiple_of(page) {
            return Err(format!(
                "the patch code region {:#x}..{:#x} is not whole pages of {page:#x}",
                code.start, code.end
            ));
        }
        let maps = Maps::read()?;
        let alias = if code.is_empty() {
            0
        } else {
            maps.each(&code, |piece, protection| {
                if protection & PROT_READ == 0 {
                    return Err(format!("the patch code at {:#x} can't be read", piece.start));
                }
                Ok(())
            })?;
            map_twice(&code)?
        };
        let at = layout.cell();
        let mut cell = 0;
        maps.each(&(at..at + 8), |_, protection| {
            cell = protection;
            Ok(())
        })?;
        Ok(Os { page, cell, code, alias, maps })
    }

    fn pages(&self, range: &Range<u64>) -> Range<u64> {
        range.start & !(self.page - 1)..range.end.next_multiple_of(self.page)
    }

    /// Writes what `writes` has within `pages`, of mappings with `protection`, which isn't
    /// writable: made writable for the write, then given it back.
    fn protected(&self, writes: &Writes<'_>, pages: &Range<u64>, protection: i32) -> Result<(), String> {
        let (start, len) = (pages.start as *mut c_void, (pages.end - pages.start) as usize);
        // SAFETY: the pages are of the image's mappings with `protection` (read at start),
        // made writable for the write and given it back; nothing else of the program's
        // writes the image's data a patch writes while `poll` writes it.
        unsafe {
            if mprotect(start, len, protection | PROT_WRITE) != 0 {
                return Err(format!(
                    "making {:#x} writable: {}",
                    pages.start,
                    std::io::Error::last_os_error()
                ));
            }
            writes.put(pages, pages.start as *mut u8);
            if mprotect(start, len, protection) != 0 {
                return Err(format!(
                    "protecting {:#x} again: {}",
                    pages.start,
                    std::io::Error::last_os_error()
                ));
            }
        }
        if protection & PROT_EXEC != 0 {
            flush(std::iter::once(pages.clone()), 0);
        }
        Ok(())
    }
}

/// Puts a memory file under `code`, with the bytes it has, mapped read and run there; the
/// address it is mapped read and write at.
fn map_twice(code: &Range<u64>) -> Result<u64, String> {
    let len = (code.end - code.start) as usize;
    let error = |what: &str| format!("{what} for the patch code: {}", std::io::Error::last_os_error());
    // SAFETY: the name ends in a zero.
    let fd = unsafe { memfd_create(c"blaze-live-code".as_ptr(), MFD_CLOEXEC) };
    if fd < 0 {
        return Err(error("making a memory file"));
    }
    let mapped = (|| {
        // SAFETY: `fd` is this function's file.
        if unsafe { ftruncate(fd, len as i64) } != 0 {
            return Err(error("sizing a memory file"));
        }
        // SAFETY: a new shared mapping of the file, anywhere.
        let alias = unsafe {
            mmap(std::ptr::null_mut(), len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0)
        };
        if alias == MAP_FAILED {
            return Err(error("mapping a memory file"));
        }
        // SAFETY: `code` is mapped and readable (the caller checked), `alias` is `len` bytes
        // of new memory; they don't overlap.
        unsafe { std::ptr::copy_nonoverlapping(code.start as *const u8, alias as *mut u8, len) };
        // SAFETY: as for `alias`.
        let view =
            unsafe { mmap(std::ptr::null_mut(), len, PROT_READ | PROT_EXEC, MAP_SHARED, fd, 0) };
        if view == MAP_FAILED {
            let error = error("mapping a memory file runnable");
            // SAFETY: this function's mapping, which nothing refers to.
            unsafe { munmap(alias, len) };
            return Err(error);
        }
        let to = code.start as *mut c_void;
        // SAFETY: `view` is this function's mapping of the bytes the region has; moving it
        // over the region replaces it in one step, so a thread running code in it runs the
        // same bytes before and after.
        let moved = unsafe { mremap(view, len, len, MREMAP_MAYMOVE | MREMAP_FIXED, to) };
        if moved != to {
            let error = error("moving a memory file over the region");
            // SAFETY: this function's mappings, which nothing refers to.
            unsafe {
                munmap(view, len);
                munmap(alias, len);
            }
            return Err(error);
        }
        Ok(alias as u64)
    })();
    // SAFETY: the mappings keep the file; its descriptor is this function's.
    unsafe { close(fd) };
    mapped
}

impl Memory for Os {
    const DWARF: Dwarf = Dwarf::Sections;
    const SEH: bool = false;

    fn code(&mut self, writes: &Writes<'_>) -> Result<(), String> {
        let (Some(first), Some(last)) = (writes.spans.first(), writes.spans.last()) else {
            return Ok(());
        };
        let hull = first.at..last.end();
        let to = self.alias + (hull.start - self.code.start);
        // SAFETY: the spans are in the patch code region (the switch checked), which `alias`
        // maps writable at the same offsets for as long as the program runs. The bytes
        // written are where no kept generation's code is, so no thread runs them.
        unsafe { writes.put(&hull, to as *mut u8) };
        flush(writes.spans.iter().map(|it| it.range()), self.alias.wrapping_sub(self.code.start));
        Ok(())
    }

    fn data(&mut self, writes: &Writes<'_>) -> Result<(), String> {
        let mut run: Option<(Range<u64>, i32)> = None;
        for span in writes.spans {
            self.maps.each(&span.range(), |piece, protection| {
                if protection & PROT_WRITE != 0 {
                    // SAFETY: the piece is mapped writable (read at start); the image's data a
                    // patch writes is written by nothing else of the program's while `poll`
                    // writes it.
                    unsafe { writes.put(&piece, piece.start as *mut u8) };
                    return Ok(());
                }
                let pages = self.pages(&piece);
                match &mut run {
                    Some((at, same)) if *same == protection && pages.start <= at.end => {
                        at.end = at.end.max(pages.end);
                    }
                    _ => {
                        if let Some((done, protection)) = run.replace((pages, protection)) {
                            self.protected(writes, &done, protection)?;
                        }
                    }
                }
                Ok(())
            })?;
        }
        if let Some((done, protection)) = run {
            self.protected(writes, &done, protection)?;
        }
        Ok(())
    }

    fn frames(&mut self, starts: &[u64]) -> Result<(), String> {
        for &start in starts {
            // SAFETY: a run of entries in the patch's writes ending in a zero length (the
            // switch checked), which stays mapped: the unwinder reads it from now on.
            unsafe { __register_frame(start as *const u8) };
        }
        Ok(())
    }

    fn functions(&mut self, functions: &[(u64, u64, u64)]) -> Result<(), String> {
        if functions.is_empty() { Ok(()) } else { Err("RUNTIME_FUNCTIONs on Linux".to_owned()) }
    }

    fn publish(&mut self, cell: u64, table: u64) -> Result<(), String> {
        let store = || {
            // SAFETY: the cell is 8-aligned in the image's data, and writable here.
            unsafe { AtomicU64::from_ptr(cell as *mut u64) }.store(table, Ordering::Release);
        };
        if self.cell & PROT_WRITE != 0 {
            store();
            return Ok(());
        }
        let pages = self.pages(&(cell..cell + 8));
        let (start, len) = (pages.start as *mut c_void, (pages.end - pages.start) as usize);
        // SAFETY: the cell's page is the image's, mapped; it is writable only for the store.
        if unsafe { mprotect(start, len, self.cell | PROT_WRITE) } != 0 {
            return Err(format!(
                "making the table cell at {cell:#x} writable: {}",
                std::io::Error::last_os_error()
            ));
        }
        store();
        // SAFETY: as above.
        if unsafe { mprotect(start, len, self.cell) } != 0 {
            return Err(format!(
                "protecting the table cell at {cell:#x} again: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }
}

/// The mappings of the process, by address, with their protections.
struct Maps(Vec<(u64, u64, i32)>);

impl Maps {
    fn read() -> Result<Maps, String> {
        let text = std::fs::read_to_string("/proc/self/maps")
            .map_err(|it| format!("reading /proc/self/maps: {it}"))?;
        let mut maps = Vec::new();
        for line in text.lines() {
            let mut fields = line.split_whitespace();
            let (Some(range), Some(permissions)) = (fields.next(), fields.next()) else {
                continue;
            };
            let Some((start, end)) = range.split_once('-') else { continue };
            let (Ok(start), Ok(end)) =
                (u64::from_str_radix(start, 16), u64::from_str_radix(end, 16))
            else {
                return Err(format!("/proc/self/maps has a line {line:?}"));
            };
            let permissions = permissions.as_bytes();
            let protection = (if permissions.first() == Some(&b'r') { PROT_READ } else { 0 })
                | (if permissions.get(1) == Some(&b'w') { PROT_WRITE } else { 0 })
                | (if permissions.get(2) == Some(&b'x') { PROT_EXEC } else { 0 });
            maps.push((start, end, protection));
        }
        maps.sort_unstable();
        Ok(Maps(maps))
    }

    /// Calls `f` with `range` in pieces of one mapping each, with their protections, from a
    /// binary search for the first; an error names the first address nothing maps.
    fn each(
        &self,
        range: &Range<u64>,
        mut f: impl FnMut(Range<u64>, i32) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut at = range.start;
        let first = self.0.partition_point(|it| it.1 <= at);
        for &(start, end, protection) in &self.0[first..] {
            if at >= range.end || start > at {
                break;
            }
            let piece = at..end.min(range.end);
            at = piece.end;
            f(piece, protection)?;
        }
        if at < range.end {
            return Err(format!("nothing is mapped at {at:#x}"));
        }
        Ok(())
    }
}

/// Watches `dir` for message files on a thread of its own, telling `events` of each.
pub(super) fn watch(dir: &Path, events: Arc<Events>) -> Result<(), String> {
    let path = CString::new(dir.as_os_str().as_bytes())
        .map_err(|_| format!("{}: a directory with a zero byte", dir.display()))?;
    let error = |what: &str| format!("{what} {}: {}", dir.display(), std::io::Error::last_os_error());
    // SAFETY: it takes no pointer.
    let fd = unsafe { inotify_init1(IN_NONBLOCK | IN_CLOEXEC) };
    if fd < 0 {
        return Err(error("watching"));
    }
    // SAFETY: the path ends in a zero.
    if unsafe { inotify_add_watch(fd, path.as_ptr(), WATCHED) } < 0 {
        let error = error("watching");
        // SAFETY: this function's descriptor.
        unsafe { close(fd) };
        return Err(error);
    }
    let watching = move || {
        let mut buffer = [0u8; 4096];
        let mut watched = true;
        while !events.stopped() {
            let mut ready = PollFd { fd, events: POLLIN, revents: 0 };
            // SAFETY: one `pollfd`.
            if unsafe { poll(&mut ready, 1, WAKE_EVERY_MS) } <= 0 {
                // A directory removed and made again is watched again.
                // SAFETY: the path ends in a zero.
                if !watched && unsafe { inotify_add_watch(fd, path.as_ptr(), WATCHED) } >= 0 {
                    watched = true;
                    events.tell();
                }
                continue;
            }
            loop {
                // SAFETY: `buffer` is writable for its length.
                let read = unsafe { read(fd, buffer.as_mut_ptr() as *mut c_void, buffer.len()) };
                let Ok(read) = usize::try_from(read) else { break };
                if read == 0 {
                    break;
                }
                let (message, ended) = names(&buffer[..read]);
                watched &= !ended;
                if message {
                    events.tell();
                }
            }
        }
        // SAFETY: the thread's own descriptor.
        unsafe { close(fd) };
    };
    std::thread::Builder::new()
        .name("blaze-live-slots".to_owned())
        .spawn(watching)
        .map(drop)
        .map_err(|it| format!("starting the thread that watches for patches: {it}"))
}

/// Whether the inotify events in `bytes` may have brought a message file, and whether the
/// watch ended (the directory went).
fn names(bytes: &[u8]) -> (bool, bool) {
    let (mut message, mut ended) = (false, false);
    let mut at = 0;
    while at + 16 <= bytes.len() {
        let field = |offset: usize| {
            let mut value = [0; 4];
            value.copy_from_slice(&bytes[at + offset..at + offset + 4]);
            u32::from_ne_bytes(value)
        };
        let (mask, len) = (field(4), field(12) as usize);
        let name = bytes.get(at + 16..at + 16 + len).unwrap_or(&[]);
        message |= mask & IN_Q_OVERFLOW != 0 || name.starts_with(MESSAGE_PREFIX.as_bytes());
        if mask & IN_IGNORED != 0 {
            message = true;
            ended = true;
        }
        at += 16 + len;
    }
    (message, ended)
}

/// Makes what was written at each of `ranges`, through the addresses `delta` from them, what
/// runs there: every data cache line cleaned, one barrier, every instruction cache line
/// invalidated, one barrier.
#[cfg(target_arch = "aarch64")]
fn flush(ranges: impl Iterator<Item = Range<u64>> + Clone, delta: u64) {
    use std::arch::asm;
    let ctr: u64;
    // SAFETY: CTR_EL0 is readable at EL0 on Linux, which traps and emulates it where not.
    unsafe { asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack)) };
    let data_line = 4u64 << ((ctr >> 16) & 0xf);
    let code_line = 4u64 << (ctr & 0xf);
    // SAFETY: cleaning and invalidating caches by address of memory that is mapped changes
    // no memory, as __clear_cache does.
    unsafe {
        for range in ranges.clone() {
            let (start, end) = (range.start.wrapping_add(delta), range.end.wrapping_add(delta));
            let mut at = start & !(data_line - 1);
            while at < end {
                asm!("dc cvau, {}", in(reg) at, options(nostack));
                at += data_line;
            }
        }
        asm!("dsb ish", options(nostack));
        for range in ranges {
            let mut at = range.start & !(code_line - 1);
            while at < range.end {
                asm!("ic ivau, {}", in(reg) at, options(nostack));
                at += code_line;
            }
        }
        asm!("dsb ish", "isb", options(nostack));
    }
}

/// x86 keeps instruction fetch coherent with stores, through any mapping of the page.
#[cfg(target_arch = "x86_64")]
fn flush(_: impl Iterator<Item = Range<u64>> + Clone, _: u64) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(mask: u32, name: &[u8]) -> Vec<u8> {
        let len = name.len().next_multiple_of(4) + 4;
        let mut bytes = Vec::new();
        for field in [1u32, mask, 0, len as u32] {
            bytes.extend_from_slice(&field.to_ne_bytes());
        }
        bytes.extend_from_slice(name);
        bytes.resize(16 + len, 0);
        bytes
    }

    #[test]
    fn only_message_files_and_lost_events_wake_the_program() {
        let answer = [event(IN_CLOSE_WRITE, b"answer.7"), event(IN_MOVED_TO, b"answer")].concat();
        assert_eq!(names(&answer), (false, false));
        let message = [event(IN_MOVED_TO, b"answer"), event(IN_MOVED_TO, b"message-00000001")];
        assert_eq!(names(&message.concat()), (true, false));
        assert_eq!(names(&event(IN_Q_OVERFLOW, b"")), (true, false));
        assert_eq!(names(&event(IN_IGNORED, b"")), (true, true));
        assert_eq!(names(&[0; 7]), (false, false));
    }

    #[test]
    fn maps_are_found_by_binary_search_in_pieces() {
        let maps = Maps(vec![(0x1000, 0x3000, PROT_READ), (0x3000, 0x4000, PROT_READ | PROT_WRITE)]);
        let mut pieces = Vec::new();
        maps.each(&(0x2ff8..0x3008), |piece, protection| {
            pieces.push((piece, protection));
            Ok(())
        })
        .expect("mapped");
        assert_eq!(pieces, vec![(0x2ff8..0x3000, PROT_READ), (0x3000..0x3008, PROT_READ | PROT_WRITE)]);
        assert!(maps.each(&(0x3ff8..0x4008), |_, _| Ok(())).unwrap_err().contains("0x4000"));
        assert!(maps.each(&(0x800..0x1008), |_, _| Ok(())).unwrap_err().contains("0x800"));
        let mine = Maps::read().expect("this process's maps");
        let local = std::hint::black_box(0u64);
        let stack = &local as *const u64 as u64;
        mine.each(&(stack..stack + 8), |_, protection| {
            assert_eq!(protection & (PROT_READ | PROT_WRITE), PROT_READ | PROT_WRITE);
            Ok(())
        })
        .expect("the stack is mapped");
    }
}

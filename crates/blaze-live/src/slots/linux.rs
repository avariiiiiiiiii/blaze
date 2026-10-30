//! Writing a running ELF image on Linux (x86-64 and AArch64).
//!
//! The patch code region is moved onto anonymous memory at `start`, so it can be made
//! writable whatever the file under it is: a copy is made elsewhere, made runnable, and moved
//! over the region in one `mremap`, so code there that runs never sees it missing. Between
//! writes it is read and run only; a write makes its pages writable too for the write.

use std::{
    ffi::c_void,
    ops::Range,
    sync::atomic::{AtomicU64, Ordering},
};

pub(super) use super::unix::{backtrace, descriptor};
use super::{
    switch::{Dwarf, Layout, Memory},
    unix::__register_frame,
};

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
}

const PROT_READ: i32 = 1;
const PROT_WRITE: i32 = 2;
const PROT_EXEC: i32 = 4;
const MAP_PRIVATE: i32 = 2;
const MAP_ANONYMOUS: i32 = 0x20;
const MAP_FAILED: *mut c_void = !0usize as *mut c_void;
const MREMAP_MAYMOVE: i32 = 1;
const MREMAP_FIXED: i32 = 2;
const SC_PAGESIZE: i32 = 30;

pub(super) struct Os {
    page: u64,
    /// The protection of the table cell's page.
    cell: i32,
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
        if !code.is_empty() {
            for (piece, protection) in maps.pieces(&code)? {
                if protection & PROT_READ == 0 {
                    return Err(format!("the patch code at {:#x} can't be read", piece.start));
                }
            }
            move_to_anonymous(&code)?;
        }
        let cell = layout.cell();
        let cell = maps.pieces(&(cell..cell + 8))?.first().map_or(0, |it| it.1);
        Ok(Os { page, cell })
    }

    fn pages(&self, range: &Range<u64>) -> Range<u64> {
        range.start & !(self.page - 1)..range.end.next_multiple_of(self.page)
    }
}

/// Puts anonymous memory with the same bytes, read and run only, under `code`.
fn move_to_anonymous(code: &Range<u64>) -> Result<(), String> {
    let len = (code.end - code.start) as usize;
    // SAFETY: a new mapping anywhere, of no file.
    let fresh = unsafe {
        mmap(std::ptr::null_mut(), len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0)
    };
    if fresh == MAP_FAILED {
        return Err(format!(
            "mapping a copy of the patch code: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: `code` is mapped and readable (checked by the caller), `fresh` is `len` bytes
    // of new memory; they don't overlap.
    unsafe { std::ptr::copy_nonoverlapping(code.start as *const u8, fresh as *mut u8, len) };
    // SAFETY: `fresh` is this function's mapping; moving it replaces the region in one step,
    // so a thread running code in it runs the same bytes before and after.
    let moved = unsafe {
        if mprotect(fresh, len, PROT_READ | PROT_EXEC) != 0 {
            Err(std::io::Error::last_os_error())
        } else {
            let to = code.start as *mut c_void;
            let moved = mremap(fresh, len, len, MREMAP_MAYMOVE | MREMAP_FIXED, to);
            if moved == to { Ok(()) } else { Err(std::io::Error::last_os_error()) }
        }
    };
    if let Err(error) = moved {
        // SAFETY: still this function's mapping, which nothing refers to.
        unsafe { munmap(fresh, len) };
        return Err(format!("moving the patch code onto anonymous memory: {error}"));
    }
    Ok(())
}

impl Memory for Os {
    const DWARF: Dwarf = Dwarf::Sections;
    const SEH: bool = false;

    fn code(&mut self, writes: &[(u64, Vec<u8>)]) -> Result<(), String> {
        for (at, bytes) in writes.iter().filter(|it| !it.1.is_empty()) {
            let range = *at..at + bytes.len() as u64;
            let pages = self.pages(&range);
            let (start, len) = (pages.start as *mut c_void, (pages.end - pages.start) as usize);
            // SAFETY: the pages are of the patch code region, made anonymous by `Os::new`.
            // They stay runnable while written, for a thread running an older generation's
            // code on them; the bytes written are where no kept generation's code is.
            unsafe {
                if mprotect(start, len, PROT_READ | PROT_WRITE | PROT_EXEC) != 0 {
                    return Err(format!(
                        "making the patch code at {at:#x} writable: {}",
                        std::io::Error::last_os_error()
                    ));
                }
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), *at as *mut u8, bytes.len());
                if mprotect(start, len, PROT_READ | PROT_EXEC) != 0 {
                    return Err(format!(
                        "making the patch code at {at:#x} runnable: {}",
                        std::io::Error::last_os_error()
                    ));
                }
            }
            flush(&range);
        }
        Ok(())
    }

    fn data(&mut self, writes: &[(u64, Vec<u8>)]) -> Result<(), String> {
        let maps = Maps::read()?;
        for (at, bytes) in writes.iter().filter(|it| !it.1.is_empty()) {
            for (piece, protection) in maps.pieces(&(*at..at + bytes.len() as u64))? {
                let from = &bytes[(piece.start - at) as usize..(piece.end - at) as usize];
                // SAFETY: the piece is mapped (by /proc/self/maps); the image's data it is in
                // is written by nothing else of the program's while `poll` writes it (a
                // static's new value, a panic location's record, a new static).
                unsafe { self.write(&piece, protection, from)? };
            }
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

impl Os {
    /// Writes `bytes` over `piece`, all of one mapping with `protection`.
    ///
    /// # Safety
    ///
    /// `piece` is mapped and nothing else reads or writes it while it is written.
    unsafe fn write(
        &self,
        piece: &Range<u64>,
        protection: i32,
        bytes: &[u8],
    ) -> Result<(), String> {
        let copy = || {
            // SAFETY: the caller's: mapped, and writable when this runs.
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), piece.start as *mut u8, bytes.len())
            }
        };
        if protection & PROT_WRITE != 0 {
            copy();
            return Ok(());
        }
        let pages = self.pages(piece);
        let (start, len) = (pages.start as *mut c_void, (pages.end - pages.start) as usize);
        // SAFETY: the pages are of one mapping (mappings are whole pages), made writable for
        // the write and given their protection back.
        unsafe {
            if mprotect(start, len, protection | PROT_WRITE) != 0 {
                return Err(format!(
                    "making {:#x} writable: {}",
                    piece.start,
                    std::io::Error::last_os_error()
                ));
            }
            copy();
            if mprotect(start, len, protection) != 0 {
                return Err(format!(
                    "protecting {:#x} again: {}",
                    piece.start,
                    std::io::Error::last_os_error()
                ));
            }
        }
        if protection & PROT_EXEC != 0 {
            flush(piece);
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

    /// `range` in pieces of one mapping each, with their protections; an error names the
    /// first address of it nothing maps.
    fn pieces(&self, range: &Range<u64>) -> Result<Vec<(Range<u64>, i32)>, String> {
        let mut pieces = Vec::new();
        let mut at = range.start;
        for &(start, end, protection) in &self.0 {
            if at >= range.end {
                break;
            }
            if end <= at {
                continue;
            }
            if start > at {
                break;
            }
            let piece = at..end.min(range.end);
            at = piece.end;
            pieces.push((piece, protection));
        }
        if at < range.end {
            return Err(format!("nothing is mapped at {at:#x}"));
        }
        Ok(pieces)
    }
}

/// Makes what was written at `range` what runs there.
#[cfg(target_arch = "aarch64")]
fn flush(range: &Range<u64>) {
    use std::arch::asm;
    let ctr: u64;
    // SAFETY: CTR_EL0 is readable at EL0 on Linux, which traps and emulates it where not.
    unsafe { asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack)) };
    let data_line = 4u64 << ((ctr >> 16) & 0xf);
    let code_line = 4u64 << (ctr & 0xf);
    // SAFETY: cleaning and invalidating caches by address of memory that is mapped changes
    // no memory, as __clear_cache does.
    unsafe {
        let mut at = range.start & !(data_line - 1);
        while at < range.end {
            asm!("dc cvau, {}", in(reg) at, options(nostack));
            at += data_line;
        }
        asm!("dsb ish", options(nostack));
        let mut at = range.start & !(code_line - 1);
        while at < range.end {
            asm!("ic ivau, {}", in(reg) at, options(nostack));
            at += code_line;
        }
        asm!("dsb ish", "isb", options(nostack));
    }
}

#[cfg(target_arch = "x86_64")]
fn flush(_: &Range<u64>) {}

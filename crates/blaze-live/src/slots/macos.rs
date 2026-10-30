//! Writing a running Mach-O image on macOS AArch64.
//!
//! As `crate::macos` writes code: a page that is not writable is never made writable, since
//! a page of code that is writable is not runnable and a thread running on it would fault.
//! A copy of its pages is written, given their protection, and mapped over them in one
//! `mach_vm_remap`, then the instruction cache is invalidated (`sys_icache_invalidate`).
//! `pthread_jit_write_protect_np` only switches pages mapped `MAP_JIT`, which can't be mapped
//! at a fixed address such as the image's patch code region, so it has no part here.

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
    fn mach_task_self() -> u32;
    fn mach_vm_allocate(task: u32, address: *mut u64, size: u64, flags: i32) -> i32;
    fn mach_vm_deallocate(task: u32, address: u64, size: u64) -> i32;
    fn mach_vm_protect(task: u32, address: u64, size: u64, set_max: i32, protection: i32) -> i32;
    fn mach_vm_remap(
        target: u32,
        address: *mut u64,
        size: u64,
        mask: u64,
        flags: i32,
        source_task: u32,
        source: u64,
        copy: i32,
        protection: *mut i32,
        max_protection: *mut i32,
        inheritance: u32,
    ) -> i32;
    fn mach_vm_region(
        task: u32,
        address: *mut u64,
        size: *mut u64,
        flavor: i32,
        info: *mut i32,
        count: *mut u32,
        object: *mut u32,
    ) -> i32;
    fn sys_icache_invalidate(start: *mut c_void, len: usize);
}

const WRITE: i32 = 2;
const EXECUTE: i32 = 4;
const PAGE: u64 = 0x4000;
const ANYWHERE: i32 = 1;
const FIXED: i32 = 0;
const OVERWRITE: i32 = 0x4000;
const INHERIT_NONE: u32 = 2;
const BASIC_INFO_64: i32 = 9;

pub(super) struct Os;

impl Os {
    pub(super) fn new(layout: &Layout) -> Result<Os, String> {
        let code = layout.code_in_memory();
        if !code.start.is_multiple_of(PAGE) || !code.end.is_multiple_of(PAGE) {
            return Err(format!(
                "the patch code region {:#x}..{:#x} is not whole pages of {PAGE:#x}",
                code.start, code.end
            ));
        }
        let mut at = code.start;
        while at < code.end {
            at = region_at(at)?.0.end;
        }
        region_at(layout.cell())?;
        Ok(Os)
    }
}

impl Memory for Os {
    const DWARF: Dwarf = Dwarf::Entries;
    const SEH: bool = false;

    fn code(&mut self, writes: &[(u64, Vec<u8>)]) -> Result<(), String> {
        let mut writes: Vec<&(u64, Vec<u8>)> =
            writes.iter().filter(|it| !it.1.is_empty()).collect();
        writes.sort_by_key(|it| it.0);
        // The writes in runs of pages, each swapped once.
        let mut runs: Vec<(Range<u64>, Vec<&(u64, Vec<u8>)>)> = Vec::new();
        for write in writes {
            let pages = pages(&(write.0..write.0 + write.1.len() as u64));
            match runs.last_mut() {
                Some((run, within)) if pages.start <= run.end => {
                    run.end = run.end.max(pages.end);
                    within.push(write);
                }
                _ => runs.push((pages, vec![write])),
            }
        }
        for (run, within) in runs {
            let mut at = run.start;
            while at < run.end {
                let (region, protection) = region_at(at)?;
                let piece = at..region.end.min(run.end);
                let parts: Vec<(u64, &[u8])> = within
                    .iter()
                    .filter_map(|(start, bytes)| clip(*start, bytes, &piece))
                    .collect();
                // SAFETY: the pages are the patch code region's, mapped (`Os::new` checked);
                // the bytes written are where no kept generation's code is, and the rest of
                // the pages is copied as it is.
                unsafe { swap(&piece, &parts, protection)? };
                at = piece.end;
            }
        }
        Ok(())
    }

    fn data(&mut self, writes: &[(u64, Vec<u8>)]) -> Result<(), String> {
        for (start, bytes) in writes.iter().filter(|it| !it.1.is_empty()) {
            let end = start + bytes.len() as u64;
            let mut at = *start;
            while at < end {
                let (region, protection) = region_at(at)?;
                let piece = at..region.end.min(end);
                let from = &bytes[(piece.start - start) as usize..(piece.end - start) as usize];
                if protection & WRITE != 0 {
                    // SAFETY: mapped and writable; nothing else of the program's writes the
                    // image's data a patch writes while `poll` writes it.
                    unsafe {
                        std::ptr::copy_nonoverlapping(from.as_ptr(), at as *mut u8, from.len())
                    };
                } else {
                    let pages = pages(&piece);
                    // SAFETY: read-only pages of the image, which nothing writes; the copy
                    // mapped over them has the rest of their bytes as they are.
                    unsafe { swap(&pages, &[(piece.start, from)], protection)? };
                }
                at = piece.end;
            }
        }
        Ok(())
    }

    fn frames(&mut self, starts: &[u64]) -> Result<(), String> {
        for &start in starts {
            // SAFETY: an FDE in the patch's writes (the switch checked), which stay mapped:
            // the unwinder reads it from now on.
            unsafe { __register_frame(start as *const u8) };
        }
        Ok(())
    }

    fn functions(&mut self, functions: &[(u64, u64, u64)]) -> Result<(), String> {
        if functions.is_empty() { Ok(()) } else { Err("RUNTIME_FUNCTIONs on macOS".to_owned()) }
    }

    fn publish(&mut self, cell: u64, table: u64) -> Result<(), String> {
        let (_, protection) = region_at(cell)?;
        if protection & WRITE != 0 {
            // SAFETY: the cell is 8-aligned in the image's data, and writable.
            unsafe { AtomicU64::from_ptr(cell as *mut u64) }.store(table, Ordering::Release);
            return Ok(());
        }
        // A page mapped over the cell's in one step is a store a reader sees all of or none.
        let bytes = table.to_le_bytes();
        // SAFETY: the cell's page is read-only data of the image, which nothing else writes.
        unsafe { swap(&pages(&(cell..cell + 8)), &[(cell, &bytes[..])], protection) }
    }
}

fn pages(range: &Range<u64>) -> Range<u64> {
    range.start & !(PAGE - 1)..range.end.next_multiple_of(PAGE)
}

/// The part of the write of `bytes` at `start` that is in `piece`.
fn clip<'a>(start: u64, bytes: &'a [u8], piece: &Range<u64>) -> Option<(u64, &'a [u8])> {
    let from = start.max(piece.start);
    let to = (start + bytes.len() as u64).min(piece.end);
    (from < to).then(|| (from, &bytes[(from - start) as usize..(to - start) as usize]))
}

fn check(code: i32) -> Result<(), String> {
    if code == 0 { Ok(()) } else { Err(format!("kern_return {code}")) }
}

/// The mapped region at `address` and its protection.
fn region_at(address: u64) -> Result<(Range<u64>, i32), String> {
    // SAFETY: it takes no pointer.
    let task = unsafe { mach_task_self() };
    let (mut start, mut size, mut object) = (address, 0u64, 0u32);
    let mut info = [0i32; 9];
    let mut count = info.len() as u32;
    // SAFETY: `info` is as large as the basic information of a region, in `int`s.
    unsafe {
        check(mach_vm_region(
            task,
            &mut start,
            &mut size,
            BASIC_INFO_64,
            info.as_mut_ptr(),
            &mut count,
            &mut object,
        ))
    }
    .map_err(|it| format!("reading the protection at {address:#x}: {it}"))?;
    if start > address || start.saturating_add(size) <= address {
        return Err(format!("nothing is mapped at {address:#x}"));
    }
    Ok((start..start + size, info[0]))
}

/// Writes `parts` within the whole pages `pages`, of one region with `protection`, by
/// mapping a written copy of them over them.
///
/// # Safety
///
/// `pages` are mapped, and nothing writes them while this runs.
unsafe fn swap(pages: &Range<u64>, parts: &[(u64, &[u8])], protection: i32) -> Result<(), String> {
    let len = pages.end - pages.start;
    // SAFETY: it takes no pointer.
    let task = unsafe { mach_task_self() };
    let mut scratch = 0u64;
    // SAFETY: a new allocation anywhere; `pages` are readable (the caller's), the parts are
    // within them, and the scratch is freed once mapped over them.
    unsafe {
        check(mach_vm_allocate(task, &mut scratch, len, ANYWHERE))
            .map_err(|it| format!("allocating a copy of {:#x}: {it}", pages.start))?;
        std::ptr::copy_nonoverlapping(pages.start as *const u8, scratch as *mut u8, len as usize);
        for &(at, bytes) in parts {
            let to = (scratch + (at - pages.start)) as *mut u8;
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), to, bytes.len());
        }
        let swapped = (|| {
            check(mach_vm_protect(task, scratch, len, 0, protection))?;
            let (mut current, mut max, mut target) = (0, 0, pages.start);
            check(mach_vm_remap(
                task,
                &mut target,
                len,
                0,
                FIXED | OVERWRITE,
                task,
                scratch,
                0,
                &mut current,
                &mut max,
                INHERIT_NONE,
            ))
        })();
        mach_vm_deallocate(task, scratch, len);
        swapped.map_err(|it| format!("writing at {:#x}: {it}", pages.start))?;
        if protection & EXECUTE != 0 {
            sys_icache_invalidate(pages.start as *mut c_void, len as usize);
        }
    }
    Ok(())
}

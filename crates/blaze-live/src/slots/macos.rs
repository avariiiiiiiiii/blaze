//! Writing a running Mach-O image on macOS AArch64.
//!
//! As `crate::macos` writes code: a page that is not writable is never made writable, since
//! a page of code that is writable is not runnable and a thread running on it would fault.
//! A copy of its pages is written, given their protection, and mapped over them in one
//! `mach_vm_remap`, then the instruction cache is invalidated (`sys_icache_invalidate`).
//! `pthread_jit_write_protect_np` only switches pages mapped `MAP_JIT`, which can't be mapped
//! at a fixed address (XNU refuses `MAP_JIT | MAP_FIXED`) such as the image's patch code
//! region, where the linker resolved the patch's references, so it has no part here.
//!
//! The code region's protection and the table cell's are read once, at start. A patch's code
//! is one remap per run of pages it writes (runs closer than [`MERGE`] are one, the pages
//! between copied as they are), its bytes copied straight from the message into the copy.
//! Data in writable pages is written in place with atomic stores ([`Writes::store`]), since
//! other threads may read it meanwhile.

#![allow(unsafe_code)]

use std::{
    ffi::{CString, c_char, c_int, c_void},
    ops::Range,
    os::unix::ffi::OsStrExt,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

pub(super) use super::unix::{GRAIN, Reserved, backtrace, descriptor, in_image_of};
use super::{
    Events,
    switch::{Dwarf, Layout, Memory, Writes},
    unix::{__register_frame, Segments, load_cell, loaders},
};

#[repr(C)]
#[allow(dead_code)]
struct Kevent {
    ident: usize,
    filter: i16,
    flags: u16,
    fflags: u32,
    data: isize,
    udata: *mut c_void,
}

#[repr(C)]
struct Timespec {
    sec: i64,
    nsec: i64,
}

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
    fn open(path: *const c_char, flags: c_int, ...) -> c_int;
    fn close(fd: c_int) -> c_int;
    fn kqueue() -> c_int;
    fn kevent(
        kq: c_int,
        changes: *const Kevent,
        change_count: c_int,
        events: *mut Kevent,
        event_count: c_int,
        timeout: *const Timespec,
    ) -> c_int;
}

const READ: i32 = 1;
const WRITE: i32 = 2;
const EXECUTE: i32 = 4;
const PAGE: u64 = 0x4000;
/// Runs of pages written this close together are remapped as one.
const MERGE: u64 = 4 * PAGE;
const ANYWHERE: i32 = 1;
const FIXED: i32 = 0;
const OVERWRITE: i32 = 0x4000;
const INHERIT_NONE: u32 = 2;
const BASIC_INFO_64: i32 = 9;
const O_EVTONLY: c_int = 0x8000;
const O_CLOEXEC: c_int = 0x0100_0000;
const EVFILT_VNODE: i16 = -4;
const EV_ADD: u16 = 0x1;
const EV_CLEAR: u16 = 0x20;
const NOTE_DELETE: u32 = 0x1;
const NOTE_WRITE: u32 = 0x2;
const NOTE_RENAME: u32 = 0x20;
/// How long the watching thread blocks before it looks whether to stop.
const WAKE_EVERY: Timespec = Timespec { sec: 1, nsec: 0 };

pub(super) struct Os {
    /// The patch code region in memory, and its protection.
    code: Range<u64>,
    protection: i32,
    /// The protection of the table cell's region.
    cell: i32,
    /// Where the image is mapped, which every data write is checked to be in.
    segments: Segments,
}

impl Os {
    pub(super) fn new(layout: &Layout) -> Result<Os, String> {
        let code = layout.code_in_memory();
        if !code.start.is_multiple_of(PAGE) || !code.end.is_multiple_of(PAGE) {
            // alloc: error path
            return Err(format!(
                "the patch code region {:#x}..{:#x} is not whole pages of {PAGE:#x}",
                code.start, code.end
            ));
        }
        let mut protection = None;
        let mut at = code.start;
        while at < code.end {
            let (region, this) = region_at(at)?;
            match protection {
                Some(other) if other != this => {
                    // alloc: error path
                    return Err(format!(
                        "the patch code region has pages of protection {other:#x} and, at \
                         {at:#x}, {this:#x}"
                    ));
                }
                _ => protection = Some(this),
            }
            at = region.end;
        }
        let (_, cell) = region_at(layout.cell())?;
        let segments = Segments::of(layout.cell())?;
        Ok(Os { code, protection: protection.unwrap_or(READ | EXECUTE), cell, segments })
    }

    /// Where the image is mapped, from its first segment to its last, in memory.
    pub(super) fn image(&self) -> Range<u64> {
        self.segments.hull()
    }
}

impl Memory for Os {
    const DWARF: Dwarf = Dwarf::Entries;
    const SEH: bool = false;

    fn cell(&self, cell: u64) -> u64 {
        load_cell(cell)
    }

    fn in_image(&self, range: &Range<u64>) -> Result<(), String> {
        self.segments.check(range)?;
        let mut at = range.start;
        while at < range.end {
            let (region, protection) = region_at(at)?;
            if protection & READ == 0 {
                // alloc: error path
                return Err(format!("{at:#x}, in pages with no access"));
            }
            at = region.end;
        }
        Ok(())
    }

    fn bytes(&self, range: &Range<u64>) -> Result<&'static [u8], String> {
        self.in_image(range)?;
        super::unix::bytes(range, self.segments.base())
    }

    fn loaders(&self, at: u64, slots: u64) -> &[AtomicU64] {
        loaders(at, slots)
    }

    fn code(&mut self, writes: &Writes<'_>) -> Result<(), String> {
        let mut run: Option<Range<u64>> = None;
        for span in writes.spans {
            let pages = pages(&span.range());
            match &mut run {
                Some(at) if pages.start <= at.end + MERGE => at.end = at.end.max(pages.end),
                _ => {
                    if let Some(done) = run.replace(pages) {
                        self.swap_code(writes, &done)?;
                    }
                }
            }
        }
        if let Some(done) = run {
            self.swap_code(writes, &done)?;
        }
        Ok(())
    }

    fn data(&mut self, writes: &Writes<'_>) -> Result<(), String> {
        let mut region: Option<(Range<u64>, i32)> = None;
        let mut run: Option<(Range<u64>, i32)> = None;
        for span in writes.spans {
            self.segments.check(&span.range())?;
            let mut at = span.at;
            while at < span.end() {
                let (found, protection) = match &region {
                    Some((found, protection)) if found.contains(&at) => {
                        // clone: a Range<u64> of two words; Range isn't Copy
                        (found.clone(), *protection)
                    }
                    _ => {
                        let looked = region_at(at)?;
                        // clone: as above
                        region = Some(looked.clone());
                        looked
                    }
                };
                let piece = at..found.end.min(span.end());
                at = piece.end;
                if protection & READ == 0 {
                    // alloc: error path
                    return Err(format!("data at {:#x}, in pages with no access", piece.start));
                }
                if protection & WRITE != 0 {
                    // SAFETY: the image's (checked above), mapped and writable, and stays
                    // mapped; other threads may use the bytes meanwhile, which `store` allows.
                    unsafe { writes.store(&piece, piece.start as *mut u8) };
                    continue;
                }
                let pages = pages(&piece);
                match &mut run {
                    Some((pending, same)) if *same == protection && pages.start <= pending.end => {
                        pending.end = pending.end.max(pages.end);
                    }
                    _ => {
                        if let Some((done, protection)) = run.replace((pages, protection)) {
                            // SAFETY: read-only pages of the image (checked above), which nothing
                            // writes; the copy mapped over them in one step has the rest of
                            // their bytes as they are, so a reader sees the old page or the new.
                            unsafe { swap(&done, protection, |copy| writes.fill(&done, copy))? };
                        }
                    }
                }
            }
        }
        if let Some((done, protection)) = run {
            // SAFETY: as above.
            unsafe { swap(&done, protection, |copy| writes.fill(&done, copy))? };
        }
        Ok(())
    }

    fn frames(&mut self, starts: &[u64]) -> Result<(), String> {
        for &start in starts {
            // SAFETY: an FDE in the patch's writes (the switch checked), mapped and never
            // written again (the switch's `Lasting`): the unwinder reads it from now on, and
            // nothing takes it back.
            unsafe { __register_frame(start as *const u8) };
        }
        Ok(())
    }

    fn functions(&mut self, functions: &[(u64, u64, u64)]) -> Result<(), String> {
        // alloc: error path
        if functions.is_empty() { Ok(()) } else { Err("RUNTIME_FUNCTIONs on macOS".to_owned()) }
    }

    fn publish(&mut self, cell: u64, table: u64) -> Result<(), String> {
        if self.cell & WRITE != 0 {
            // SAFETY: the cell is 8-aligned in the image's data, and writable.
            unsafe { AtomicU64::from_ptr(cell as *mut u64) }.store(table, Ordering::Release);
            return Ok(());
        }
        // A page mapped over the cell's in one step is a store a reader sees all of or none.
        let pages = pages(&(cell..cell + 8));
        let bytes = table.to_le_bytes();
        let at = (cell - pages.start) as usize;
        // SAFETY: the cell's page is read-only data of the image, which nothing else writes.
        unsafe { swap(&pages, self.cell, |copy| copy[at..at + 8].copy_from_slice(&bytes)) }
    }
}

impl Os {
    fn swap_code(&self, writes: &Writes<'_>, run: &Range<u64>) -> Result<(), String> {
        debug_assert!(run.start >= self.code.start && run.end <= self.code.end);
        // SAFETY: the pages are the patch code region's, mapped with one protection
        // (`Os::new` checked, and every swap keeps it). The bytes written are over no code
        // the cell ever pointed at nor any registered unwind entry (the switch's `Lasting`),
        // and the rest of the pages is copied as it is, so a thread running there runs the
        // same bytes before and after.
        unsafe { swap(run, self.protection, |copy| writes.fill(run, copy)) }
    }
}

fn pages(range: &Range<u64>) -> Range<u64> {
    range.start & !(PAGE - 1)..range.end.next_multiple_of(PAGE)
}

fn check(code: i32) -> Result<(), String> {
    // alloc: error path
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
    check(unsafe {
        mach_vm_region(
            task,
            &mut start,
            &mut size,
            BASIC_INFO_64,
            info.as_mut_ptr(),
            &mut count,
            &mut object,
        )
    })
    // alloc: error path
    .map_err(|it| format!("reading the protection at {address:#x}: {it}"))?;
    if start > address || start.saturating_add(size) <= address {
        // alloc: error path
        return Err(format!("nothing is mapped at {address:#x}"));
    }
    Ok((start..start + size, info[0]))
}

/// Writes the whole pages `pages`, mapped with `protection`, by mapping a copy of them over
/// them: `fill` writes the copy, which stands for `pages`.
///
/// # Safety
///
/// `pages` are whole mapped and readable pages of the image that nothing writes while this
/// runs.
unsafe fn swap(
    pages: &Range<u64>,
    protection: i32,
    fill: impl FnOnce(&mut [u8]),
) -> Result<(), String> {
    let len = pages.end - pages.start;
    // SAFETY: it takes no pointer.
    let task = unsafe { mach_task_self() };
    let mut scratch = 0u64;
    // SAFETY: a new allocation anywhere, written to `scratch`.
    check(unsafe { mach_vm_allocate(task, &mut scratch, len, ANYWHERE) })
        // alloc: error path
        .map_err(|it| format!("allocating a copy of {:#x}: {it}", pages.start))?;
    {
        // SAFETY: `len` bytes allocated above, zeroed, which only this function knows of.
        let copy = unsafe { std::slice::from_raw_parts_mut(scratch as *mut u8, len as usize) };
        // SAFETY: `pages` are mapped and readable, and nothing writes them (the caller's).
        let now = unsafe { std::slice::from_raw_parts(pages.start as *const u8, len as usize) };
        copy.copy_from_slice(now);
        fill(copy);
    }
    let swapped = (|| {
        // SAFETY: the allocation above, of `len` bytes.
        check(unsafe { mach_vm_protect(task, scratch, len, 0, protection) })?;
        let (mut current, mut max, mut target) = (0, 0, pages.start);
        // SAFETY: maps the copy over `pages` in one step, so a reader sees the old bytes or
        // the new; whole pages (the caller's), with the protection they had.
        check(unsafe {
            mach_vm_remap(
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
            )
        })
    })();
    // SAFETY: the allocation above, which no slice borrows now; the remap shares its pages.
    unsafe { mach_vm_deallocate(task, scratch, len) };
    // alloc: error path
    swapped.map_err(|it| format!("writing at {:#x}: {it}", pages.start))?;
    if protection & EXECUTE != 0 {
        // SAFETY: the pages were just mapped; invalidating changes no memory.
        unsafe { sys_icache_invalidate(pages.start as *mut c_void, len as usize) };
    }
    Ok(())
}

/// Opens `path` for events only and adds it to `kq`: its descriptor.
fn register(kq: c_int, path: &CString) -> Option<c_int> {
    // SAFETY: the path ends in a zero.
    let fd = unsafe { open(path.as_ptr(), O_EVTONLY | O_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    let change = Kevent {
        ident: fd as usize,
        filter: EVFILT_VNODE,
        flags: EV_ADD | EV_CLEAR,
        fflags: NOTE_WRITE | NOTE_DELETE | NOTE_RENAME,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    // SAFETY: one change, no events asked for.
    if unsafe { kevent(kq, &change, 1, std::ptr::null_mut(), 0, std::ptr::null()) } < 0 {
        // SAFETY: this function's descriptor.
        unsafe { close(fd) };
        return None;
    }
    Some(fd)
}

/// Watches `dir` on a thread of its own: each change to its entries (a message renamed into
/// place) is told to `events`.
pub(super) fn watch(dir: &Path, events: Arc<Events>) -> Result<(), String> {
    let path = CString::new(dir.as_os_str().as_bytes())
        // alloc: error path
        .map_err(|_| format!("{}: a directory with a zero byte", dir.display()))?;
    // alloc: error path
    let error = || format!("watching {}: {}", dir.display(), std::io::Error::last_os_error());
    // SAFETY: it takes no pointer.
    let kq = unsafe { kqueue() };
    if kq < 0 {
        return Err(error());
    }
    let Some(fd) = register(kq, &path) else {
        let error = error();
        // SAFETY: this function's queue.
        unsafe { close(kq) };
        return Err(error);
    };
    let watching = move || {
        let mut watched = Some(fd);
        while !events.stopped() {
            if watched.is_none() {
                // A directory removed or moved and made again is watched again.
                watched = register(kq, &path);
                if watched.is_some() {
                    events.tell();
                }
            }
            let mut event = Kevent {
                ident: 0,
                filter: 0,
                flags: 0,
                fflags: 0,
                data: 0,
                udata: std::ptr::null_mut(),
            };
            // SAFETY: room for one event; the timeout is a whole `timespec`.
            if unsafe { kevent(kq, std::ptr::null(), 0, &mut event, 1, &WAKE_EVERY) } > 0 {
                events.tell();
                if event.fflags & (NOTE_DELETE | NOTE_RENAME) != 0 {
                    if let Some(fd) = watched.take() {
                        // SAFETY: the thread's own descriptor; closing it removes its event.
                        unsafe { close(fd) };
                    }
                }
            }
        }
        if let Some(fd) = watched {
            // SAFETY: the thread's own descriptor, closed once.
            unsafe { close(fd) };
        }
        // SAFETY: as above.
        unsafe { close(kq) };
    };
    std::thread::Builder::new()
        // alloc: setup, the thread's name, once
        .name("blaze-live-slots".to_owned())
        .spawn(watching)
        .map(drop)
        // alloc: error path
        .map_err(|it| format!("starting the thread that watches for patches: {it}"))
}

//! Writing a running PE image on Windows x86-64.
//!
//! Pages are made writable for a write with `VirtualProtect` and given their protection
//! back, once per run of pages of one protection that a patch writes; code stays runnable
//! while written, for a thread running an older generation's code on the same pages.
//! Unwinding goes by `RUNTIME_FUNCTION`s, added with `RtlAddFunctionTable`. Data is written
//! with atomic stores ([`Writes::store`]), since other threads may read it meanwhile.

#![allow(unsafe_code)]

use std::{
    ffi::c_void,
    ops::Range,
    os::windows::ffi::OsStrExt,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use super::{
    Events, image,
    switch::{Dwarf, Layout, Memory, Writes},
};
use crate::patch::{DESCRIPTOR, Descriptor};

#[repr(C)]
#[allow(dead_code)]
struct MemoryBasicInformation {
    base: *mut c_void,
    allocation_base: *mut c_void,
    allocation_protect: u32,
    partition: u16,
    size: usize,
    state: u32,
    protect: u32,
    kind: u32,
}

#[repr(C)]
#[allow(dead_code)]
struct RuntimeFunction {
    begin: u32,
    end: u32,
    unwind: u32,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetModuleHandleExW(flags: u32, name: *const u16, module: *mut *mut c_void) -> i32;
    fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
    fn GetCurrentProcess() -> *mut c_void;
    fn VirtualProtect(address: *mut c_void, size: usize, protect: u32, old: *mut u32) -> i32;
    fn VirtualQuery(address: *const c_void, info: *mut MemoryBasicInformation, len: usize)
    -> usize;
    fn FlushInstructionCache(process: *mut c_void, address: *const c_void, size: usize) -> i32;
    fn RtlAddFunctionTable(table: *const RuntimeFunction, count: u32, base: u64) -> u8;
    fn RtlCaptureStackBackTrace(
        skip: u32,
        count: u32,
        frames: *mut *mut c_void,
        hash: *mut u32,
    ) -> u16;
    fn FindFirstChangeNotificationW(path: *const u16, subtree: i32, filter: u32) -> *mut c_void;
    fn FindNextChangeNotification(handle: *mut c_void) -> i32;
    fn FindCloseChangeNotification(handle: *mut c_void) -> i32;
    fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
}

const FROM_ADDRESS: u32 = 0x4;
const UNCHANGED_REFCOUNT: u32 = 0x2;
const MEM_COMMIT: u32 = 0x1000;
const PAGE_NOACCESS: u32 = 0x01;
const PAGE_GUARD: u32 = 0x100;
const PAGE_READWRITE: u32 = 0x04;
const PAGE_WRITECOPY: u32 = 0x08;
const PAGE_EXECUTE_READWRITE: u32 = 0x40;
const PAGE_EXECUTE_WRITECOPY: u32 = 0x80;
const EXECUTABLE: u32 = 0xf0;
const WRITABLE: u32 =
    PAGE_READWRITE | PAGE_WRITECOPY | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY;
const FILE_NOTIFY_CHANGE_FILE_NAME: u32 = 0x1;
const FILE_NOTIFY_CHANGE_LAST_WRITE: u32 = 0x10;
const INVALID_HANDLE_VALUE: *mut c_void = !0usize as *mut c_void;
const WAIT_OBJECT_0: u32 = 0;
/// How long the watching thread blocks before it looks whether to stop.
const WAKE_EVERY_MS: u32 = 1000;
const PAGE: u64 = 0x1000;
/// What `RtlCaptureStackBackTrace` takes at most on every Windows.
const FRAMES: usize = 62;

/// Where the image that has `address` is loaded.
fn module_of(address: *const c_void) -> Result<*mut c_void, String> {
    let mut module = std::ptr::null_mut();
    // SAFETY: with FROM_ADDRESS the name is an address in the module; the count of
    // references is left alone, and the image stays loaded as long as its code runs.
    let found =
        unsafe { GetModuleHandleExW(FROM_ADDRESS | UNCHANGED_REFCOUNT, address as _, &mut module) };
    if found == 0 || module.is_null() {
        // alloc: error path
        return Err(format!("no module has {address:?}: {}", std::io::Error::last_os_error()));
    }
    Ok(module)
}

/// Where the descriptor of the module this code is in is, in memory, and its bytes.
pub(super) fn descriptor() -> Result<(u64, &'static [u8]), String> {
    let own = descriptor as fn() -> Result<(u64, &'static [u8]), String> as *const c_void;
    let module = module_of(own)?;
    // alloc: setup, the descriptor's name, looked up once at start
    let name: Vec<u8> = DESCRIPTOR.bytes().chain([0]).collect();
    // SAFETY: the module is loaded and the name ends in a zero.
    let found = unsafe { GetProcAddress(module, name.as_ptr()) };
    if found.is_null() {
        // alloc: error path
        return Err(format!(
            "the module of blaze-live's code exports no {DESCRIPTOR}: blaze-link links it for \
             live patches"
        ));
    }
    let at = found as u64;
    let end =
        at.checked_add(Descriptor::SIZE as u64).ok_or("a descriptor past the end of memory")?;
    Ok((at, readable(&(at..end), module as u64)?))
}

/// The image's bytes at `range`, checked to be committed, readable and of the image at
/// `base`.
fn readable(range: &Range<u64>, base: u64) -> Result<&'static [u8], String> {
    regions(range, |piece, region| {
        if region.allocation != base {
            // alloc: error path
            return Err(format!("{:#x}, outside the image at {base:#x}", piece.start));
        }
        let protect = region.protect & 0xff;
        if protect == PAGE_NOACCESS || region.protect & PAGE_GUARD != 0 {
            // alloc: error path
            return Err(format!("{:#x}, in pages with no access", piece.start));
        }
        Ok(())
    })?;
    // SAFETY: `range` is committed, readable memory of the image (checked above), which stays
    // loaded as long as its code, this, runs; it is data the linker wrote, which no patch
    // writes over (the switch refuses data over the descriptor's cell and table, and the
    // descriptor and the directory name are read-only data).
    Ok(unsafe { image::view(range.start, (range.end - range.start) as usize) })
}

/// Pushes the code address of each frame of this thread's stack, newest first.
pub(super) fn backtrace(pcs: &mut Vec<u64>) {
    let mut frames = [std::ptr::null_mut(); FRAMES];
    // SAFETY: `frames` has room for `FRAMES`; no hash is asked for.
    let count = unsafe {
        RtlCaptureStackBackTrace(0, FRAMES as u32, frames.as_mut_ptr(), std::ptr::null_mut())
    };
    let count = usize::from(count).min(FRAMES);
    pcs.extend(frames[..count].iter().map(|it| (*it as u64).wrapping_sub(1)));
}

/// Watches `dir` on a thread of its own: each change of its file names (a message renamed
/// into place) is told to `events`.
pub(super) fn watch(dir: &Path, events: Arc<Events>) -> Result<(), String> {
    // alloc: setup, the directory's name, watched from start
    let path: Vec<u16> = dir.as_os_str().encode_wide().chain([0]).collect();
    let first = |path: &[u16]| {
        let filter = FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_LAST_WRITE;
        // SAFETY: the path ends in a zero.
        let handle = unsafe { FindFirstChangeNotificationW(path.as_ptr(), 0, filter) };
        (handle != INVALID_HANDLE_VALUE).then_some(handle as usize)
    };
    let Some(handle) = first(&path) else {
        // alloc: error path
        return Err(format!("watching {}: {}", dir.display(), std::io::Error::last_os_error()));
    };
    // A handle is an address the thread is given to use alone.
    let watching = move || {
        let mut watched = Some(handle);
        while !events.stopped() {
            let Some(handle) = watched else {
                // A directory removed and made again is watched again.
                std::thread::sleep(std::time::Duration::from_millis(u64::from(WAKE_EVERY_MS)));
                watched = first(&path);
                if watched.is_some() {
                    events.tell();
                }
                continue;
            };
            let handle = handle as *mut c_void;
            // SAFETY: the thread's own notification handle.
            if unsafe { WaitForSingleObject(handle, WAKE_EVERY_MS) } == WAIT_OBJECT_0 {
                events.tell();
                // SAFETY: as above.
                if unsafe { FindNextChangeNotification(handle) } == 0 {
                    // SAFETY: as above; one that can't go on is closed, once.
                    unsafe { FindCloseChangeNotification(handle) };
                    watched = None;
                }
            }
        }
        if let Some(handle) = watched {
            // SAFETY: as above, closed once.
            unsafe { FindCloseChangeNotification(handle as *mut c_void) };
        }
    };
    std::thread::Builder::new()
        // alloc: setup, the thread's name, once
        .name("blaze-live-slots".to_owned())
        .spawn(watching)
        .map(drop)
        // alloc: error path
        .map_err(|it| format!("starting the thread that watches for patches: {it}"))
}

pub(super) struct Os {
    /// Where the image is loaded, which `RUNTIME_FUNCTION`s are relative to.
    base: u64,
}

impl Os {
    pub(super) fn new(layout: &Layout) -> Result<Os, String> {
        let base = module_of(layout.cell() as *const c_void)? as u64;
        let code = layout.code_in_memory();
        if !code.is_empty() {
            regions(&code, |_, _| Ok(()))?;
        }
        Ok(Os { base })
    }

    fn write(&mut self, writes: &Writes<'_>, kind: Kind) -> Result<(), String> {
        let base = self.base;
        let mut run: Option<(Range<u64>, Region)> = None;
        let mut known = None;
        for span in writes.spans {
            regions_from(&mut known, &span.range(), |piece, region| {
                if region.allocation != base {
                    // alloc: error path
                    return Err(format!(
                        "a write at {:#x}..{:#x}, outside the image at {base:#x}",
                        piece.start, piece.end
                    ));
                }
                let protect = region.protect & 0xff;
                if protect == PAGE_NOACCESS || region.protect & PAGE_GUARD != 0 {
                    // alloc: error path
                    return Err(format!("a write at {:#x}, in pages with no access", piece.start));
                }
                if protect & WRITABLE != 0 {
                    // SAFETY: the piece is the image's, committed and writable (`regions` read
                    // it), and stays committed.
                    unsafe { kind.write(writes, &piece) };
                    if protect & EXECUTABLE != 0 {
                        flush(&piece);
                    }
                    return Ok(());
                }
                let pages = piece.start & !(PAGE - 1)..piece.end.next_multiple_of(PAGE);
                // One `VirtualProtect` pair covers pages of one allocation with one protection.
                match &mut run {
                    Some((pending, same)) if *same == region && pages.start <= pending.end => {
                        pending.end = pending.end.max(pages.end);
                    }
                    _ => {
                        if let Some((done, region)) = run.replace((pages, region)) {
                            protected(writes, &done, region.protect & 0xff, kind)?;
                        }
                    }
                }
                Ok(())
            })?;
        }
        if let Some((done, region)) = run {
            protected(writes, &done, region.protect & 0xff, kind)?;
        }
        Ok(())
    }
}

/// What a write is of, which says how it may be written.
#[derive(Clone, Copy)]
enum Kind {
    /// Patch code: over no code the cell ever pointed at nor any registered unwind entry
    /// (the switch's `Lasting`), so no thread runs or reads the bytes, and plain copies do.
    Code,
    /// The image's data, which other threads may read or write meanwhile: atomic stores.
    Data,
}

impl Kind {
    /// Writes what `writes` has within `range`.
    ///
    /// # Safety
    ///
    /// `range` is committed memory of the image, writable while this runs.
    unsafe fn write(self, writes: &Writes<'_>, range: &Range<u64>) {
        match self {
            // SAFETY: the caller's; no thread uses the bytes (`Kind::Code`).
            Kind::Code => unsafe { writes.put(range, range.start as *mut u8) },
            // SAFETY: the caller's; `store` allows other threads to use the bytes.
            Kind::Data => unsafe { writes.store(range, range.start as *mut u8) },
        }
    }
}

impl Memory for Os {
    const DWARF: Dwarf = Dwarf::Not;
    const SEH: bool = true;

    fn cell(&self, cell: u64) -> u64 {
        // SAFETY: the cell is the descriptor's, 8-aligned (`Layout::new` checked), checked to
        // be in the image by `Switch::new` before the first load, and only ever accessed
        // atomically.
        unsafe { image::cell(cell) }.load(Ordering::Acquire)
    }

    fn in_image(&self, range: &Range<u64>) -> Result<(), String> {
        readable(range, self.base).map(drop)
    }

    fn bytes(&self, range: &Range<u64>) -> Result<&'static [u8], String> {
        readable(range, self.base)
    }

    fn loaders(&self, at: u64, slots: u64) -> &[u64] {
        // SAFETY: `Switch::new` checked `at..at + slots * 8` is in the image and `at` 8-aligned
        // before the first call; nothing writes it (the switch refuses data over it).
        unsafe { image::words(at, slots as usize) }
    }

    fn code(&mut self, writes: &Writes<'_>) -> Result<(), String> {
        self.write(writes, Kind::Code)
    }

    fn data(&mut self, writes: &Writes<'_>) -> Result<(), String> {
        self.write(writes, Kind::Data)
    }

    fn frames(&mut self, starts: &[u64]) -> Result<(), String> {
        // alloc: error path
        if starts.is_empty() { Ok(()) } else { Err("DWARF unwind entries on Windows".to_owned()) }
    }

    fn functions(&mut self, functions: &[(u64, u64, u64)]) -> Result<(), String> {
        if functions.is_empty() {
            return Ok(());
        }
        let relative = |address: u64| {
            address.checked_sub(self.base).and_then(|it| u32::try_from(it).ok()).ok_or_else(|| {
                // alloc: error path
                format!("{address:#x}, out of reach of the image at {:#x}", self.base)
            })
        };
        let mut table = functions
            .iter()
            .map(|&(begin, end, unwind)| {
                Ok(RuntimeFunction {
                    begin: relative(begin)?,
                    end: relative(end)?,
                    unwind: relative(unwind)?,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        table.sort_by_key(|it| it.begin);
        let count = u32::try_from(table.len()).map_err(|_| "too many RUNTIME_FUNCTIONs")?;
        // The unwinder reads the table from now on, and the code and unwind information it
        // names are never written again (the switch's `Lasting`): it is never freed.
        let table: &'static [RuntimeFunction] = Box::leak(table.into_boxed_slice());
        // SAFETY: `count` entries, sorted, relative to `base`, alive forever.
        if unsafe { RtlAddFunctionTable(table.as_ptr(), count, self.base) } == 0 {
            // alloc: error path
            return Err("the unwinder refused the patch's RUNTIME_FUNCTIONs".to_owned());
        }
        Ok(())
    }

    fn publish(&mut self, cell: u64, table: u64) -> Result<(), String> {
        let range = cell..cell + 8;
        let (mut protect, mut count) = (0, 0);
        regions(&range, |_, region| {
            protect = region.protect & 0xff;
            count += 1;
            Ok(())
        })?;
        if count != 1 {
            // alloc: error path
            return Err(format!("the table cell at {cell:#x} spans two regions"));
        }
        let store = || {
            // SAFETY: the cell is 8-aligned in the image's data, and writable when this runs.
            unsafe { AtomicU64::from_ptr(cell as *mut u64) }.store(table, Ordering::Release);
        };
        if protect & WRITABLE != 0 {
            store();
            return Ok(());
        }
        // SAFETY: the cell is the image's, committed; writable for the store alone.
        unsafe { writable(&range, protect, store) }
    }
}

/// What a run of pages written with one `VirtualProtect` pair has the same of.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Region {
    allocation: u64,
    /// With its modifiers (`PAGE_GUARD` and the rest).
    protect: u32,
}

/// Calls `f` with `range` in pieces of one region each; an error names the first address
/// of it nothing is committed at.
fn regions(
    range: &Range<u64>,
    f: impl FnMut(Range<u64>, Region) -> Result<(), String>,
) -> Result<(), String> {
    regions_from(&mut None, range, f)
}

/// [`regions`], starting from `known`, the region last queried, and leaving there the last
/// one it queries: spans sorted by address in one region cost one `VirtualQuery` in all. A
/// region's protection is the same after a write as before ([`writable`] gives it back), so
/// what was queried stays true while a patch is written.
fn regions_from(
    known: &mut Option<(Range<u64>, Region)>,
    range: &Range<u64>,
    mut f: impl FnMut(Range<u64>, Region) -> Result<(), String>,
) -> Result<(), String> {
    let mut at = range.start;
    while at < range.end {
        if let Some((region, of)) = known.as_ref().filter(|it| it.0.contains(&at)) {
            let (piece, of) = (at..region.end.min(range.end), *of);
            at = piece.end;
            f(piece, of)?;
            continue;
        }
        let mut info = MemoryBasicInformation {
            base: std::ptr::null_mut(),
            allocation_base: std::ptr::null_mut(),
            allocation_protect: 0,
            partition: 0,
            size: 0,
            state: 0,
            protect: 0,
            kind: 0,
        };
        let len = size_of::<MemoryBasicInformation>();
        // SAFETY: `info` is a whole MEMORY_BASIC_INFORMATION.
        if unsafe { VirtualQuery(at as *const c_void, &mut info, len) } != len {
            // alloc: error path
            return Err(format!("querying {at:#x}: {}", std::io::Error::last_os_error()));
        }
        let end = (info.base as u64).saturating_add(info.size as u64);
        if info.state != MEM_COMMIT || end <= at {
            // alloc: error path
            return Err(format!("nothing is committed at {at:#x}"));
        }
        let region = Region { allocation: info.allocation_base as u64, protect: info.protect };
        *known = Some((info.base as u64..end, region));
        let piece = at..end.min(range.end);
        at = piece.end;
        f(piece, region)?;
    }
    Ok(())
}

/// Writes what `writes` has within `pages`, committed memory of the image with `protect`,
/// which isn't writable: made writable once for the write, then given it back.
fn protected(
    writes: &Writes<'_>,
    pages: &Range<u64>,
    protect: u32,
    kind: Kind,
) -> Result<(), String> {
    let put = || {
        // SAFETY: the pages are the image's, committed and writable while this runs
        // (`writable`).
        unsafe { kind.write(writes, pages) }
    };
    // SAFETY: the pages are the image's, committed with `protect` (`regions` read it).
    unsafe { writable(pages, protect, put)? };
    if protect & EXECUTABLE != 0 {
        flush(pages);
    }
    Ok(())
}

fn flush(range: &Range<u64>) {
    // SAFETY: it takes no pointer; the handle stands for this process and needs no closing.
    let process = unsafe { GetCurrentProcess() };
    let (start, len) = (range.start as *const c_void, (range.end - range.start) as usize);
    // SAFETY: the range was just written; flushing changes no memory.
    unsafe { FlushInstructionCache(process, start, len) };
}

/// Runs `f` with `range`, of regions with `protect`, made writable.
///
/// # Safety
///
/// `range` is committed memory of the image that nothing else changes the protection of.
unsafe fn writable(range: &Range<u64>, protect: u32, f: impl FnOnce()) -> Result<(), String> {
    let wanted = if protect & EXECUTABLE != 0 { PAGE_EXECUTE_READWRITE } else { PAGE_READWRITE };
    let (address, len) = (range.start as *mut c_void, (range.end - range.start) as usize);
    let mut old = 0;
    // SAFETY: the caller's.
    if unsafe { VirtualProtect(address, len, wanted, &mut old) } == 0 {
        // alloc: error path
        return Err(format!(
            "making {:#x} writable: {}",
            range.start,
            std::io::Error::last_os_error()
        ));
    }
    f();
    let mut was = 0;
    // SAFETY: as above; the regions had one protection, which they get back.
    if unsafe { VirtualProtect(address, len, old, &mut was) } == 0 {
        // alloc: error path
        return Err(format!(
            "protecting {:#x} again: {}",
            range.start,
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

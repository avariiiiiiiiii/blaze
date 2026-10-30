//! Writing a running PE image on Windows x86-64.
//!
//! Pages are made writable for a write with `VirtualProtect` and given their protection
//! back, once per run of pages of one protection that a patch writes; code stays runnable
//! while written, for a thread running an older generation's code on the same pages.
//! Unwinding goes by `RUNTIME_FUNCTION`s, added with `RtlAddFunctionTable`.

use std::{
    ffi::c_void,
    fs::File,
    ops::Range,
    os::windows::{ffi::OsStrExt, io::AsRawHandle},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use super::{
    Events,
    switch::{Dwarf, Layout, Memory, Writes},
};
use crate::patch::DESCRIPTOR;

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
    fn CreateFileMappingW(
        file: *mut c_void,
        attributes: *const c_void,
        protect: u32,
        size_high: u32,
        size_low: u32,
        name: *const u16,
    ) -> *mut c_void;
    fn MapViewOfFile(
        mapping: *mut c_void,
        access: u32,
        offset_high: u32,
        offset_low: u32,
        len: usize,
    ) -> *mut c_void;
    fn UnmapViewOfFile(address: *const c_void) -> i32;
    fn CloseHandle(handle: *mut c_void) -> i32;
    fn FindFirstChangeNotificationW(path: *const u16, subtree: i32, filter: u32) -> *mut c_void;
    fn FindNextChangeNotification(handle: *mut c_void) -> i32;
    fn FindCloseChangeNotification(handle: *mut c_void) -> i32;
    fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
}

const FROM_ADDRESS: u32 = 0x4;
const UNCHANGED_REFCOUNT: u32 = 0x2;
const MEM_COMMIT: u32 = 0x1000;
const PAGE_READONLY: u32 = 0x02;
const PAGE_READWRITE: u32 = 0x04;
const PAGE_WRITECOPY: u32 = 0x08;
const PAGE_EXECUTE_READWRITE: u32 = 0x40;
const PAGE_EXECUTE_WRITECOPY: u32 = 0x80;
const EXECUTABLE: u32 = 0xf0;
const WRITABLE: u32 =
    PAGE_READWRITE | PAGE_WRITECOPY | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY;
const FILE_MAP_READ: u32 = 0x4;
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
        return Err(format!("no module has {address:?}: {}", std::io::Error::last_os_error()));
    }
    Ok(module)
}

/// Where the descriptor of the module this code is in is, in memory.
pub(super) fn descriptor() -> Result<u64, String> {
    let own = descriptor as fn() -> Result<u64, String> as *const c_void;
    let module = module_of(own)?;
    let name: Vec<u8> = DESCRIPTOR.bytes().chain([0]).collect();
    // SAFETY: the module is loaded and the name ends in a zero.
    let found = unsafe { GetProcAddress(module, name.as_ptr()) };
    if found.is_null() {
        return Err(format!(
            "the module of blaze-live's code exports no {DESCRIPTOR}: blaze-link links it for \
             live patches"
        ));
    }
    Ok(found as u64)
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

/// A message file mapped read-only, its bytes read in place and unmapped when dropped.
pub(super) struct Map {
    at: *mut c_void,
    len: usize,
}

impl Map {
    /// Maps the `len` bytes of `file` (its length when opened).
    pub(super) fn new(file: &File, len: u64) -> Result<Map, String> {
        let len = usize::try_from(len).map_err(|_| format!("a message of {len} bytes"))?;
        if len == 0 {
            return Ok(Map { at: std::ptr::null_mut(), len });
        }
        let error = |what: &str| format!("{what} the message: {}", std::io::Error::last_os_error());
        // SAFETY: a read-only mapping of an open file, of its whole length, unnamed.
        let mapping = unsafe {
            CreateFileMappingW(
                file.as_raw_handle(),
                std::ptr::null(),
                PAGE_READONLY,
                0,
                0,
                std::ptr::null(),
            )
        };
        if mapping.is_null() {
            return Err(error("mapping"));
        }
        // SAFETY: a view of the mapping made above, which the view keeps once its handle is
        // closed.
        let at = unsafe { MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, len) };
        let viewed = if at.is_null() { Err(error("viewing")) } else { Ok(Map { at, len }) };
        // SAFETY: this function's handle.
        unsafe { CloseHandle(mapping) };
        viewed
    }

    pub(super) fn bytes(&self) -> &[u8] {
        if self.len == 0 {
            return &[];
        }
        // SAFETY: `len` bytes viewed readable at `at` until drop. The linker writes a message
        // under another name and renames it into place, so nothing writes the file's bytes.
        unsafe { std::slice::from_raw_parts(self.at as *const u8, self.len) }
    }
}

impl Drop for Map {
    fn drop(&mut self) {
        if self.len != 0 {
            // SAFETY: this map's own view, which nothing borrows once it is dropped.
            unsafe { UnmapViewOfFile(self.at) };
        }
    }
}

/// Watches `dir` on a thread of its own: each change of its file names (a message renamed
/// into place) is told to `events`.
pub(super) fn watch(dir: &Path, events: Arc<Events>) -> Result<(), String> {
    let path: Vec<u16> = dir.as_os_str().encode_wide().chain([0]).collect();
    let first = |path: &[u16]| {
        let filter = FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_LAST_WRITE;
        // SAFETY: the path ends in a zero.
        let handle = unsafe { FindFirstChangeNotificationW(path.as_ptr(), 0, filter) };
        (handle != INVALID_HANDLE_VALUE).then_some(handle as usize)
    };
    let Some(handle) = first(&path) else {
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
                // SAFETY: as above; one that can't go on is closed, once.
                if unsafe { FindNextChangeNotification(handle) } == 0 {
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
        .name("blaze-live-slots".to_owned())
        .spawn(watching)
        .map(drop)
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
}

impl Memory for Os {
    const DWARF: Dwarf = Dwarf::Not;
    const SEH: bool = true;

    fn code(&mut self, writes: &Writes<'_>) -> Result<(), String> {
        self.data(writes)
    }

    fn data(&mut self, writes: &Writes<'_>) -> Result<(), String> {
        let mut run: Option<(Range<u64>, Region)> = None;
        for span in writes.spans {
            regions(&span.range(), |piece, region| {
                let protect = region.protect & 0xff;
                if protect & WRITABLE != 0 {
                    // SAFETY: the piece is committed and writable (`regions` read it); the
                    // patch code bytes written are where no kept generation's code is, and the
                    // data nothing else of the program's writes while `poll` writes it.
                    unsafe { writes.put(&piece, piece.start as *mut u8) };
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
                            protected(writes, &done, region.protect & 0xff)?;
                        }
                    }
                }
                Ok(())
            })?;
        }
        if let Some((done, region)) = run {
            protected(writes, &done, region.protect & 0xff)?;
        }
        Ok(())
    }

    fn frames(&mut self, starts: &[u64]) -> Result<(), String> {
        if starts.is_empty() { Ok(()) } else { Err("DWARF unwind entries on Windows".to_owned()) }
    }

    fn functions(&mut self, functions: &[(u64, u64, u64)]) -> Result<(), String> {
        if functions.is_empty() {
            return Ok(());
        }
        let relative = |address: u64| {
            address.checked_sub(self.base).and_then(|it| u32::try_from(it).ok()).ok_or_else(|| {
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
        // The unwinder reads the table from now on, for as long as the code is mapped, which
        // is as long as the program runs: it is never freed.
        let table: &'static [RuntimeFunction] = Box::leak(table.into_boxed_slice());
        // SAFETY: `count` entries, sorted, relative to `base`, alive forever.
        if unsafe { RtlAddFunctionTable(table.as_ptr(), count, self.base) } == 0 {
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
    mut f: impl FnMut(Range<u64>, Region) -> Result<(), String>,
) -> Result<(), String> {
    let mut at = range.start;
    while at < range.end {
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
            return Err(format!("querying {at:#x}: {}", std::io::Error::last_os_error()));
        }
        let end = (info.base as u64).saturating_add(info.size as u64);
        if info.state != MEM_COMMIT || end <= at {
            return Err(format!("nothing is committed at {at:#x}"));
        }
        let piece = at..end.min(range.end);
        at = piece.end;
        f(piece, Region { allocation: info.allocation_base as u64, protect: info.protect })?;
    }
    Ok(())
}

/// Writes what `writes` has within `pages`, committed memory of the image with `protect`,
/// which isn't writable: made writable once for the write, then given it back.
fn protected(writes: &Writes<'_>, pages: &Range<u64>, protect: u32) -> Result<(), String> {
    // SAFETY: the pages are the image's, committed with `protect` (`regions` read it), and
    // nothing else of the program's writes them while `poll` does.
    unsafe { writable(pages, protect, || writes.put(pages, pages.start as *mut u8))? };
    if protect & EXECUTABLE != 0 {
        flush(pages);
    }
    Ok(())
}

fn flush(range: &Range<u64>) {
    // SAFETY: the range was just written.
    unsafe {
        FlushInstructionCache(
            GetCurrentProcess(),
            range.start as *const c_void,
            (range.end - range.start) as usize,
        )
    };
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
        return Err(format!(
            "protecting {:#x} again: {}",
            range.start,
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

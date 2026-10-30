//! Writing a running PE image on Windows x86-64.
//!
//! Pages are made writable for a write with `VirtualProtect` and given their protection
//! back; code stays runnable while written, for a thread running an older generation's code
//! on the same pages. Unwinding goes by `RUNTIME_FUNCTION`s, added with
//! `RtlAddFunctionTable`.

use std::{
    ffi::c_void,
    ops::Range,
    sync::atomic::{AtomicU64, Ordering},
};

use super::switch::{Dwarf, Layout, Memory};
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
}

const FROM_ADDRESS: u32 = 0x4;
const UNCHANGED_REFCOUNT: u32 = 0x2;
const MEM_COMMIT: u32 = 0x1000;
const PAGE_READWRITE: u32 = 0x04;
const PAGE_WRITECOPY: u32 = 0x08;
const PAGE_EXECUTE_READWRITE: u32 = 0x40;
const PAGE_EXECUTE_WRITECOPY: u32 = 0x80;
const EXECUTABLE: u32 = 0xf0;
const WRITABLE: u32 =
    PAGE_READWRITE | PAGE_WRITECOPY | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY;
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
        return Err(format!(
            "no module has {address:?}: {}",
            std::io::Error::last_os_error()
        ));
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

pub(super) struct Os {
    /// Where the image is loaded, which `RUNTIME_FUNCTION`s are relative to.
    base: u64,
}

impl Os {
    pub(super) fn new(layout: &Layout) -> Result<Os, String> {
        let base = module_of(layout.cell() as *const c_void)? as u64;
        let code = layout.code_in_memory();
        if !code.is_empty() {
            regions(&code)?;
        }
        Ok(Os { base })
    }
}

impl Memory for Os {
    const DWARF: Dwarf = Dwarf::Not;
    const SEH: bool = true;

    fn code(&mut self, writes: &[(u64, Vec<u8>)]) -> Result<(), String> {
        self.data(writes)
    }

    fn data(&mut self, writes: &[(u64, Vec<u8>)]) -> Result<(), String> {
        for (start, bytes) in writes.iter().filter(|it| !it.1.is_empty()) {
            let range = *start..start + bytes.len() as u64;
            for (piece, protect) in regions(&range)? {
                let from = &bytes[(piece.start - start) as usize..(piece.end - start) as usize];
                // SAFETY: the piece is committed (`regions` checked) and of the image; the
                // patch code bytes written are where no kept generation's code is, and the
                // data nothing else of the program's writes while `poll` writes it.
                unsafe { write(&piece, protect, from)? };
            }
        }
        Ok(())
    }

    fn frames(&mut self, starts: &[u64]) -> Result<(), String> {
        if starts.is_empty() {
            Ok(())
        } else {
            Err("DWARF unwind entries on Windows".to_owned())
        }
    }

    fn functions(&mut self, functions: &[(u64, u64, u64)]) -> Result<(), String> {
        if functions.is_empty() {
            return Ok(());
        }
        let relative = |address: u64| {
            address
                .checked_sub(self.base)
                .and_then(|it| u32::try_from(it).ok())
                .ok_or_else(|| format!("{address:#x}, out of reach of the image at {:#x}", self.base))
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
        let pieces = regions(&range)?;
        let [(_, protect)] = pieces.as_slice() else {
            return Err(format!("the table cell at {cell:#x} spans two regions"));
        };
        let protect = *protect;
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

/// `range` in pieces of one region each, with their protections; an error names the first
/// address of it nothing is committed at.
fn regions(range: &Range<u64>) -> Result<Vec<(Range<u64>, u32)>, String> {
    let mut pieces = Vec::new();
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
        pieces.push((piece, info.protect & 0xff));
    }
    Ok(pieces)
}

/// Runs `f` with `range`, of one region with `protect`, made writable.
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
    // SAFETY: as above; one region had one protection, which it gets back.
    if unsafe { VirtualProtect(address, len, old, &mut was) } == 0 {
        return Err(format!(
            "protecting {:#x} again: {}",
            range.start,
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

/// Writes `bytes` over `piece`, of one region with `protect`.
///
/// # Safety
///
/// `piece` is committed memory of the image, and nothing else reads or writes the bytes
/// while they are written.
unsafe fn write(piece: &Range<u64>, protect: u32, bytes: &[u8]) -> Result<(), String> {
    let copy = || {
        // SAFETY: the caller's; writable when this runs.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), piece.start as *mut u8, bytes.len()) }
    };
    if protect & WRITABLE != 0 {
        copy();
    } else {
        // SAFETY: the caller's.
        unsafe { writable(piece, protect, copy)? };
    }
    if protect & EXECUTABLE != 0 {
        // SAFETY: the range was just written.
        unsafe {
            FlushInstructionCache(
                GetCurrentProcess(),
                piece.start as *const c_void,
                bytes.len(),
            )
        };
    }
    Ok(())
}

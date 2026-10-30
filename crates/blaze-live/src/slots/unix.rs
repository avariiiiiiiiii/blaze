//! What Linux and macOS share: finding the descriptor with `dlsym`, walking a stack with
//! `_Unwind_Backtrace`, the unwinder's `__register_frame`, and reading a message file in
//! place from a mapping of it.

use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    fs::File,
    os::fd::AsRawFd,
};

use super::hook::FRAMES;
use crate::patch::DESCRIPTOR;

#[repr(C)]
#[allow(dead_code)]
struct DlInfo {
    fname: *const c_char,
    fbase: *mut c_void,
    sname: *const c_char,
    saddr: *mut c_void,
}

unsafe extern "C" {
    fn dladdr(address: *const c_void, info: *mut DlInfo) -> c_int;
    fn dlopen(path: *const c_char, flags: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
    fn dlclose(handle: *mut c_void) -> c_int;
    fn _Unwind_Backtrace(
        trace: extern "C" fn(*mut c_void, *mut c_void) -> c_int,
        data: *mut c_void,
    ) -> c_int;
    fn _Unwind_GetIP(context: *mut c_void) -> usize;
    pub(super) fn __register_frame(frame: *const u8);
    fn mmap(
        addr: *mut c_void,
        len: usize,
        prot: c_int,
        flags: c_int,
        fd: c_int,
        offset: i64,
    ) -> *mut c_void;
    fn munmap(addr: *mut c_void, len: usize) -> c_int;
}

const PROT_READ: c_int = 1;
const MAP_PRIVATE: c_int = 2;
/// Linux faults every page of the mapping in within the one call.
#[cfg(target_os = "linux")]
const MAP_POPULATE: c_int = 0x8000;
#[cfg(target_os = "macos")]
const MAP_POPULATE: c_int = 0;
const MAP_FAILED: *mut c_void = !0usize as *mut c_void;

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
        // SAFETY: a new private, read-only mapping of an open file, anywhere.
        let at = unsafe {
            mmap(
                std::ptr::null_mut(),
                len,
                PROT_READ,
                MAP_PRIVATE | MAP_POPULATE,
                file.as_raw_fd(),
                0,
            )
        };
        if at == MAP_FAILED {
            return Err(format!("mapping the message: {}", std::io::Error::last_os_error()));
        }
        Ok(Map { at, len })
    }

    pub(super) fn bytes(&self) -> &[u8] {
        if self.len == 0 {
            return &[];
        }
        // SAFETY: `len` bytes mapped readable at `at` until drop. The linker writes a message
        // under another name and renames it into place, so nothing writes the file's bytes.
        unsafe { std::slice::from_raw_parts(self.at as *const u8, self.len) }
    }
}

impl Drop for Map {
    fn drop(&mut self) {
        if self.len != 0 {
            // SAFETY: this map's own mapping, which nothing borrows once it is dropped.
            unsafe { munmap(self.at, self.len) };
        }
    }
}

const RTLD_LAZY: c_int = 1;
#[cfg(target_os = "linux")]
const RTLD_NOLOAD: c_int = 4;
#[cfg(target_os = "macos")]
const RTLD_NOLOAD: c_int = 0x10;
const URC_NO_REASON: c_int = 0;
const URC_END_OF_STACK: c_int = 5;

/// The image that has `address`: where it starts and its path as the loader has it.
fn image_of(address: *const c_void) -> Option<(u64, *const c_char)> {
    let mut info = DlInfo {
        fname: std::ptr::null(),
        fbase: std::ptr::null_mut(),
        sname: std::ptr::null(),
        saddr: std::ptr::null_mut(),
    };
    // SAFETY: `info` is a whole `Dl_info` for dladdr to fill.
    (unsafe { dladdr(address, &mut info) } != 0).then_some((info.fbase as u64, info.fname))
}

/// The descriptor of the image `base` from `handle`, if that is where `handle` finds it.
fn lookup(handle: *mut c_void, name: &CStr, base: u64) -> Option<u64> {
    if handle.is_null() {
        return None;
    }
    // SAFETY: the handle is open, the name ends in a zero.
    let found = unsafe { dlsym(handle, name.as_ptr()) };
    (!found.is_null() && image_of(found)?.0 == base).then_some(found as u64)
}

/// Where the descriptor of the image this code is in is, in memory.
pub(super) fn descriptor() -> Result<u64, String> {
    let name = CString::new(DESCRIPTOR).map_err(|_| "a descriptor name with a zero")?;
    let own = descriptor as fn() -> Result<u64, String> as *const c_void;
    let (base, path) = image_of(own).ok_or("the loader knows no image of blaze-live's own code")?;
    // SAFETY: a null path asks for the program's handle, which is never unloaded.
    let program = unsafe { dlopen(std::ptr::null(), RTLD_LAZY) };
    if let Some(found) = lookup(program, &name, base) {
        return Ok(found);
    }
    // A shared library that links blaze-live is found by its own path.
    // SAFETY: the loader's path of a loaded image ends in a zero and lives as long as it does.
    let path =
        (!path.is_null()).then(|| unsafe { CStr::from_ptr(path) }).filter(|it| !it.is_empty());
    if let Some(path) = path {
        // SAFETY: NOLOAD opens nothing new; the handle is closed below.
        let handle = unsafe { dlopen(path.as_ptr(), RTLD_LAZY | RTLD_NOLOAD) };
        let found = lookup(handle, &name, base);
        if !handle.is_null() {
            // SAFETY: opened above; the image stays loaded, as it was before.
            unsafe { dlclose(handle) };
        }
        if let Some(found) = found {
            return Ok(found);
        }
    }
    let path =
        path.map_or_else(|| "the program".to_owned(), |it| it.to_string_lossy().into_owned());
    Err(format!("{path} exports no {DESCRIPTOR}: blaze-link links it for live patches"))
}

/// Pushes the code address of each frame of this thread's stack, newest first.
pub(super) fn backtrace(pcs: &mut Vec<u64>) {
    extern "C" fn each(context: *mut c_void, data: *mut c_void) -> c_int {
        // SAFETY: `data` is the vector `backtrace` passed, borrowed for the walk.
        let pcs = unsafe { &mut *(data as *mut Vec<u64>) };
        // SAFETY: the unwinder's context of this step of the walk.
        let ip = unsafe { _Unwind_GetIP(context) } as u64;
        if ip != 0 {
            pcs.push(ip - 1);
        }
        if pcs.len() >= FRAMES { URC_END_OF_STACK } else { URC_NO_REASON }
    }
    // SAFETY: `each` only pushes to the vector it is given.
    unsafe { _Unwind_Backtrace(each, pcs as *mut Vec<u64> as *mut c_void) };
}

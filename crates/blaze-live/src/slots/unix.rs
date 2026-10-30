//! What Linux and macOS share: finding the descriptor with `dlsym`, walking a stack with
//! `_Unwind_Backtrace`, and the unwinder's `__register_frame`.

#![allow(unsafe_code)]

use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    ops::Range,
};

use super::{hook::FRAMES, image};
use crate::patch::{DESCRIPTOR, Descriptor};

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

/// Where the image that has `address` starts, for the platforms' check that a patch's data
/// is in the image it was linked for.
pub(super) fn image_base(address: u64) -> Option<u64> {
    image_of(address as *const c_void).map(|it| it.0)
}

/// Checks that the first and the last byte of `range` are in the image at `base`. The loader
/// maps an image as one run of addresses; a gap within it is refused by the platform's walk
/// of the mappings (unmapped, or mapped with no access).
pub(super) fn in_image(range: &Range<u64>, base: u64) -> Result<(), String> {
    let last = range.end.saturating_sub(1).max(range.start);
    if image_base(range.start) == Some(base) && image_base(last) == Some(base) {
        Ok(())
    } else {
        Err(outside(range, base))
    }
}

fn outside(range: &Range<u64>, base: u64) -> String {
    format!("data at {:#x}..{:#x}, outside the image at {base:#x}", range.start, range.end)
}

/// The address ranges the image the loader put at `base` is mapped in, sorted: those the
/// loader's `dladdr` finds an address of the image in (ELF `PT_LOAD` segments, Mach-O
/// segments but `__PAGEZERO`), read once at start. [`Segments::check`] is [`in_image`] by
/// binary search, where `dladdr` walks the loader's list of images and their symbols for
/// each address.
pub(super) struct Segments {
    base: u64,
    ranges: Vec<Range<u64>>,
}

impl Segments {
    fn new(base: u64, mut ranges: Vec<Range<u64>>) -> Result<Segments, String> {
        ranges.retain(|it| !it.is_empty());
        ranges.sort_unstable_by_key(|it| it.start);
        if ranges.is_empty() {
            return Err(format!("the image at {base:#x} has no segments"));
        }
        Ok(Segments { base, ranges })
    }

    /// Whether `at` is in a segment.
    fn has(&self, at: u64) -> bool {
        let next = self.ranges.partition_point(|it| it.end <= at);
        self.ranges.get(next).is_some_and(|it| it.start <= at)
    }

    /// Checks that the first and the last byte of `range` are in the image, as [`in_image`].
    pub(super) fn check(&self, range: &Range<u64>) -> Result<(), String> {
        let last = range.end.saturating_sub(1).max(range.start);
        if self.has(range.start) && self.has(last) {
            Ok(())
        } else {
            Err(outside(range, self.base))
        }
    }

    /// Where the image is loaded.
    pub(super) fn base(&self) -> u64 {
        self.base
    }

    /// From the start of the first segment to the end of the last, in memory.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(super) fn hull(&self) -> Range<u64> {
        let first = self.ranges.first().map_or(0, |it| it.start);
        first..self.ranges.last().map_or(first, |it| it.end)
    }

    /// The segments of the image that has `address`.
    #[cfg(target_os = "linux")]
    pub(super) fn of(address: u64) -> Result<Segments, String> {
        /// The first fields of `dl_phdr_info`, which the loader passes by pointer.
        #[repr(C)]
        #[allow(dead_code)]
        struct Info {
            addr: u64,
            name: *const c_char,
            phdr: *const Phdr,
            phnum: u16,
        }
        /// `Elf64_Phdr`.
        #[repr(C)]
        #[allow(dead_code)]
        struct Phdr {
            kind: u32,
            flags: u32,
            offset: u64,
            vaddr: u64,
            paddr: u64,
            filesz: u64,
            memsz: u64,
            align: u64,
        }
        const PT_LOAD: u32 = 1;
        unsafe extern "C" {
            fn dl_iterate_phdr(
                each: extern "C" fn(*mut Info, usize, *mut c_void) -> c_int,
                data: *mut c_void,
            ) -> c_int;
        }
        struct Found {
            address: u64,
            ranges: Vec<Range<u64>>,
        }
        extern "C" fn each(info: *mut Info, _: usize, data: *mut c_void) -> c_int {
            // SAFETY: `data` is the `Found` `of` passed, borrowed for the walk.
            let found = unsafe { &mut *(data as *mut Found) };
            // SAFETY: the loader's `dl_phdr_info` of one object, valid during this call; its
            // first four fields are `Info`'s.
            let info = unsafe { &*info };
            if info.phdr.is_null() {
                return 0;
            }
            // SAFETY: `phdr` is the object's array of `phnum` program headers, mapped as long
            // as the object is loaded, which it is during this call.
            let headers = unsafe { std::slice::from_raw_parts(info.phdr, usize::from(info.phnum)) };
            let loads = headers.iter().filter(|it| it.kind == PT_LOAD).map(|it| {
                let start = info.addr.wrapping_add(it.vaddr);
                start..start.saturating_add(it.memsz)
            });
            if !loads.clone().any(|it| it.contains(&found.address)) {
                return 0;
            }
            found.ranges.extend(loads);
            1
        }
        let base = image_base(address)
            .ok_or_else(|| format!("no image has the address {address:#x}"))?;
        let mut found = Found { address, ranges: Vec::with_capacity(16) };
        // SAFETY: `each` only reads the loader's headers and pushes to the `Found` it is given.
        unsafe { dl_iterate_phdr(each, &mut found as *mut Found as *mut c_void) };
        Segments::new(base, found.ranges)
    }

    /// The segments of the image that has `address`, from its Mach-O header.
    #[cfg(target_os = "macos")]
    pub(super) fn of(address: u64) -> Result<Segments, String> {
        const MH_MAGIC_64: u32 = 0xfeed_facf;
        const LC_SEGMENT_64: u32 = 0x19;
        const HEADER: usize = 32;
        const SEGMENT: usize = 72;
        let base = image_base(address)
            .ok_or_else(|| format!("no image has the address {address:#x}"))?;
        let u32_at = |bytes: &[u8], at: usize| {
            u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4 bytes"))
        };
        let u64_at = |bytes: &[u8], at: usize| {
            u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"))
        };
        // SAFETY: `base` is where dyld mapped the image's Mach-O header (dladdr's `dli_fbase`),
        // the start of its `__TEXT`, read-only for as long as the image is loaded.
        let header = unsafe { image::view(base, HEADER) };
        if u32_at(header, 0) != MH_MAGIC_64 {
            return Err(format!("the image at {base:#x} is not a 64-bit Mach-O image"));
        }
        let (count, len) = (u32_at(header, 16), u32_at(header, 20) as usize);
        // SAFETY: the load commands follow the header in the same mapped `__TEXT`, `len`
        // bytes by the header, which dyld checked when it loaded the image.
        let commands = unsafe { image::view(base + HEADER as u64, len) };
        let (mut at, mut segments, mut slide) = (0usize, Vec::new(), None);
        for _ in 0..count {
            if at + 8 > commands.len() {
                break;
            }
            let (kind, size) = (u32_at(commands, at), u32_at(commands, at + 4) as usize);
            if size < 8 || at + size > commands.len() {
                return Err(format!("the image at {base:#x} has a load command past its end"));
            }
            if kind == LC_SEGMENT_64 && size >= SEGMENT {
                let (vmaddr, vmsize) = (u64_at(commands, at + 24), u64_at(commands, at + 32));
                let (fileoff, filesize) = (u64_at(commands, at + 40), u64_at(commands, at + 48));
                let initprot = u32_at(commands, at + 60);
                if fileoff == 0 && filesize != 0 {
                    slide = Some(base.wrapping_sub(vmaddr));
                }
                if initprot != 0 {
                    segments.push((vmaddr, vmsize));
                }
            }
            at += size;
        }
        let slide = slide.ok_or_else(|| format!("the image at {base:#x} maps no header"))?;
        let ranges = segments
            .into_iter()
            .map(|(vmaddr, vmsize)| {
                let start = vmaddr.wrapping_add(slide);
                start..start.saturating_add(vmsize)
            })
            .collect();
        Segments::new(base, ranges)
    }
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

/// The image's bytes at `range`, checked to be in the image at `base`; the caller checks
/// they are readable (the platform's walk of its mappings) or knows (the descriptor, which
/// the loader found by name).
pub(super) fn bytes(range: &Range<u64>, base: u64) -> Result<&'static [u8], String> {
    in_image(range, base)?;
    // SAFETY: `range` is in the image at `base` (checked above), which stays loaded as long as
    // this code, the image's own, runs; what is read is data the linker wrote, which no patch
    // writes over (the switch refuses data over the descriptor's cell and table, and the
    // descriptor and the directory name are in read-only data).
    Ok(unsafe { image::view(range.start, (range.end - range.start) as usize) })
}

/// Where the descriptor of the image this code is in is, in memory, and its bytes.
pub(super) fn descriptor() -> Result<(u64, &'static [u8]), String> {
    let at = find()?;
    let base = image_base(at).ok_or_else(|| format!("no image has {DESCRIPTOR} at {at:#x}"))?;
    let end =
        at.checked_add(Descriptor::SIZE as u64).ok_or("a descriptor past the end of memory")?;
    Ok((at, bytes(&(at..end), base)?))
}

fn find() -> Result<u64, String> {
    let name = CString::new(DESCRIPTOR).map_err(|_| "a descriptor name with a zero")?;
    let own = descriptor as fn() -> Result<(u64, &'static [u8]), String> as *const c_void;
    let (base, path) = image_of(own).ok_or("the loader knows no image of blaze-live's own code")?;
    // SAFETY: a null path asks for the program's handle, which is never unloaded.
    let program = unsafe { dlopen(std::ptr::null(), RTLD_LAZY) };
    if let Some(found) = lookup(program, &name, base) {
        return Ok(found);
    }
    // A shared library that links blaze-live is found by its own path.
    let path = (!path.is_null())
        .then(|| {
            // SAFETY: the loader's path of a loaded image ends in a zero and lives as long as
            // it does.
            unsafe { CStr::from_ptr(path) }
        })
        .filter(|it| !it.is_empty());
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

/// What the table cell at `cell` holds, by one acquire load.
pub(super) fn load_cell(cell: u64) -> u64 {
    // SAFETY: the cell is the descriptor's, 8-aligned (`Layout::new` checked), checked to be
    // in the image by `Switch::new` before the first load, and only ever accessed atomically.
    unsafe { image::cell(cell) }.load(std::sync::atomic::Ordering::Acquire)
}

/// The loader's table at `at`, `slots` addresses.
pub(super) fn loaders(at: u64, slots: u64) -> &'static [u64] {
    // SAFETY: `Switch::new` checked `at..at + slots * 8` is in the image and `at` 8-aligned
    // before the first call; nothing writes it (the switch refuses data over it).
    unsafe { image::words(at, slots as usize) }
}

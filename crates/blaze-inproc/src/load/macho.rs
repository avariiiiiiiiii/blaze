//! blaze's loader on macOS (AArch64): maps a Mach-O image as blaze-link lays it out (an arm64
//! `MH_EXECUTE`, position independent, each segment at a file offset equal to its address from
//! the first one's) from the image object, links it to the system's libraries with `dlopen`
//! and `dlsym`, sets up its thread-locals and unwind tables, and runs its initializers and
//! `main` in this process: what dyld and `start` would have done for it as a file, with no file.
//!
//! The image's bytes are untrusted: [`parse`] reads them and [`fixup`] runs the rebase and bind
//! opcodes, both safe and the same code the tests run over plain buffers. [`tlv`] is the
//! thread-locals' thunk; this file maps, calls the system and jumps.
//!
//! Code is mapped shared with the daemon's writable view, so a patch is a store there; data is
//! a private copy, `__DATA_CONST` made read-only once fixed up; the code memory's arena after
//! the image is mapped executable at the same offsets. One mapping per segment and one pass
//! over each opcode table, each symbol looked up once.
//!
//! What dyld does that this does not, each an error: chained fixups, weak definitions
//! coalesced across images, interposing, Objective-C and Swift metadata (their runtimes learn
//! images from dyld alone), thread-locals imported from a library. The process's own image as
//! dyld and `_dyld_get_image_header(0)` see it is the loader's, as is `current_exe()`.

mod fixup;
mod parse;
#[cfg(test)]
mod tests;
mod tlv;

use std::ffi::{CStr, CString, OsString, c_char, c_int, c_void};
use std::os::unix::ffi::OsStringExt;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicPtr, Ordering};

use fixup::{Imports, Memory, Segment};
use parse::{
    Dylib, MachO, S_INIT_FUNC_OFFSETS, S_MOD_INIT_FUNC_POINTERS, S_MOD_TERM_FUNC_POINTERS, S_THREAD_LOCAL_REGULAR,
    S_THREAD_LOCAL_VARIABLES, S_THREAD_LOCAL_ZEROFILL, Section,
};

use super::{Image, Program};
use crate::channel::Channel;
use crate::{Access, Error, TlsReserve, sys};

unsafe extern "C" {
    fn _NSGetArgc() -> *mut c_int;
    fn _NSGetArgv() -> *mut *mut *mut c_char;
    fn _NSGetEnviron() -> *mut *mut *mut c_char;
    fn _NSGetProgname() -> *mut *const c_char;
}

/// Mach-O's thread-locals are reached through descriptors the loader fills, not a reserve.
pub(crate) fn reserve() -> Result<TlsReserve, Error> {
    Ok(TlsReserve::default())
}

pub(crate) fn load(image: Image<'_>) -> Result<Box<dyn Program>, Error> {
    let Image { object, bytes, image_len, base, reserve: _, channel } = image;
    Ok(Box::new(link(object, channel, bytes, image_len, base)?))
}

fn dlerror() -> String {
    // SAFETY: dlerror returns null or a NUL-terminated message valid until the next dl call
    // on this thread; it is copied at once.
    let message = unsafe { libc::dlerror() };
    if message.is_null() {
        return "no reason given".to_owned();
    }
    // SAFETY: as above.
    unsafe { CStr::from_ptr(message) }.to_string_lossy().into_owned()
}

/// The image's libraries, opened, and lookups in them by `dlsym`.
struct Dl {
    /// By ordinal from 1; `None` for a weak library that is absent.
    handles: Vec<Option<*mut c_void>>,
    /// The name being looked up, NUL-terminated, without its leading underscore.
    name: Vec<u8>,
}

impl Dl {
    fn open(macho: &MachO<'_>) -> Result<Dl, Error> {
        let mut handles = Vec::with_capacity(macho.dylibs.len());
        for dylib in &macho.dylibs {
            handles.push(open_dylib(macho, dylib)?);
        }
        Ok(Dl { handles, name: Vec::new() })
    }

    fn symbol(&mut self, handle: *mut c_void, name: &[u8]) -> Result<Option<u64>, Error> {
        let Some(name) = name.strip_prefix(b"_") else {
            return Err(Error::new(format!("symbol {} has no leading underscore, so no C name", String::from_utf8_lossy(name))));
        };
        self.name.clear();
        self.name.extend_from_slice(name);
        self.name.push(0);
        let name = CStr::from_bytes_with_nul(&self.name).map_err(|_| Error::new("a symbol name with a NUL in it"))?;
        // SAFETY: `handle` is a handle `dlopen` gave, never closed, or RTLD_DEFAULT; `name` is
        // NUL-terminated and lives for the call.
        let found = unsafe { libc::dlsym(handle, name.as_ptr()) };
        Ok((!found.is_null()).then_some(found as u64))
    }
}

impl Imports for Dl {
    fn library(&mut self, ordinal: usize, name: &[u8]) -> Result<Option<u64>, Error> {
        match self.handles.get(ordinal - 1).copied().flatten() {
            Some(handle) => self.symbol(handle, name),
            None => Ok(None),
        }
    }

    fn flat(&mut self, name: &[u8]) -> Result<Option<u64>, Error> {
        self.symbol(libc::RTLD_DEFAULT, name)
    }
}

/// Opens a library as dyld finds it for the image: `@rpath/` through each `LC_RPATH` in order,
/// the first that loads; a path relative to the executable is an error, there being no file.
fn open_dylib(macho: &MachO<'_>, dylib: &Dylib<'_>) -> Result<Option<*mut c_void>, Error> {
    let path = dylib.path.to_bytes();
    let shown = dylib.path.to_string_lossy();
    let candidates = if let Some(rest) = path.strip_prefix(b"@rpath/") {
        let mut out = Vec::with_capacity(macho.rpaths.len());
        for rpath in &macho.rpaths {
            let rpath = rpath.to_bytes();
            if rpath.starts_with(b"@") {
                return Err(Error::new(format!(
                    "LC_RPATH {} is relative to the executable, which has no file under blaze's loader",
                    String::from_utf8_lossy(rpath)
                )));
            }
            let mut joined = rpath.to_vec();
            if !joined.ends_with(b"/") {
                joined.push(b'/');
            }
            joined.extend_from_slice(rest);
            out.push(CString::new(joined).map_err(|_| Error::new("an rpath with a NUL in it"))?);
        }
        out
    } else if path.starts_with(b"@executable_path/") || path.starts_with(b"@loader_path/") {
        return Err(Error::new(format!("library {shown} is relative to the executable, which has no file under blaze's loader")));
    } else {
        vec![dylib.path.to_owned()]
    };
    let mut tried = Vec::with_capacity(candidates.len());
    for candidate in &candidates {
        // SAFETY: `candidate` is NUL-terminated and lives for the call; the handle stays open
        // for the life of the process.
        let handle = unsafe { libc::dlopen(candidate.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
        if !handle.is_null() {
            return Ok(Some(handle));
        }
        tried.push(dlerror());
    }
    if dylib.weak {
        return Ok(None);
    }
    if tried.is_empty() {
        tried.push("no LC_RPATH to look in".to_owned());
    }
    Err(Error::new(format!("library {shown}, which the image links, does not load: {}", tried.join("; "))))
}

/// The kernel's "apple" strings, which `main` and the initializers get after `envp`: on the
/// initial stack, after `argv`'s pointers and a null and the environment's and a null.
fn apple() -> Result<*const *const c_char, Error> {
    // SAFETY: crt_externs' variables, which dyld set before the loader's first instruction;
    // nothing of the loader changes `NXArgc` or `NXArgv` before the program runs, so they are
    // still the kernel's, on the initial stack.
    let (argc, argv) = unsafe { (*_NSGetArgc(), (*_NSGetArgv()).cast_const()) };
    let argc = usize::try_from(argc).map_err(|_| Error::new(format!("argc {argc}")))?;
    let invalid = || Error::new("no apple strings after the environment on the initial stack");
    // SAFETY: the kernel lays out `argc` argument pointers and a null, the environment's
    // pointers and a null, then the apple strings' pointers (at least `executable_path`) and a
    // null, contiguously on the initial stack, which lives as long as the process. `unsetenv`
    // moves the environment's pointers down within it, leaving nulls before the apple strings;
    // the walk stops at those bounds.
    unsafe {
        let mut at = argv.add(argc + 1);
        let mut steps = 0usize;
        while !(*at).is_null() {
            at = at.add(1);
            steps += 1;
            if steps > 1 << 20 {
                return Err(invalid());
            }
        }
        let mut nulls = 0usize;
        while (*at).is_null() {
            at = at.add(1);
            nulls += 1;
            if nulls > 4096 {
                return Err(invalid());
            }
        }
        if !CStr::from_ptr(*at).to_bytes().contains(&b'=') {
            return Err(invalid());
        }
        Ok(at.cast())
    }
}

fn check_stack(stack: u64) -> Result<(), Error> {
    if stack == 0 {
        return Ok(());
    }
    // SAFETY: the calling thread, the loader's main thread, is live.
    let have = unsafe { libc::pthread_get_stacksize_np(libc::pthread_self()) } as u64;
    if stack > have {
        return Err(Error::new(format!(
            "LC_MAIN asks for a stack of {stack:#x} bytes; the main thread it runs on has {have:#x}"
        )));
    }
    Ok(())
}

fn map_segment(
    object: &sys::Granted,
    channel: &mut Channel,
    base: u64,
    macho: &MachO<'_>,
    segment: &parse::Segment,
    page: u64,
) -> Result<(), Error> {
    let Some(access) = segment.access() else { return Ok(()) };
    let offset = segment.vmaddr - macho.first;
    let file = segment.filesize.next_multiple_of(page);
    let memory = segment.vmsize.next_multiple_of(page);
    if file != 0 {
        let at = base + offset;
        let mapped = object.map(channel, offset, file, at, access)?;
        if mapped != at {
            return Err(Error::new(format!("segment {} was mapped at {mapped:#x}, not {at:#x}", segment.name())));
        }
    }
    if memory > file {
        sys::protect(base + offset + file, memory - file, Access::Copy)?;
    }
    Ok(())
}

/// Fills the image's thread-local descriptors with the thunk and the key of its template.
fn thread_locals(macho: &MachO<'_>, memory: &mut Memory<'_>) -> Result<(), Error> {
    let descriptors: Vec<&Section> = macho.sections_of(S_THREAD_LOCAL_VARIABLES).collect();
    if descriptors.is_empty() {
        return Ok(());
    }
    let blocks = || macho.sections.iter().filter(|it| matches!(it.kind(), S_THREAD_LOCAL_REGULAR | S_THREAD_LOCAL_ZEROFILL) && it.size != 0);
    let start = blocks().map(|it| it.addr).min().unwrap_or(0);
    let end = blocks().map(|it| it.addr + it.size).max().unwrap_or(0);
    let init_end = blocks().filter(|it| it.kind() == S_THREAD_LOCAL_REGULAR).map(|it| it.addr + it.size).max().unwrap_or(start);
    let align = blocks().map(|it| 1u64 << it.align).max().unwrap_or(1);
    let size = end - start;
    let init = if init_end > start {
        memory
            .bytes(start, init_end - start)
            .or_else(|| macho.file(start, init_end - start))
            .ok_or_else(|| Error::new(format!("the thread-local template at {start:#x} is outside the image's data")))?
            .to_vec()
    } else {
        Vec::new()
    };
    let key = tlv::install(&init, size, align)?;
    let thunk = tlv::thunk();
    for section in descriptors {
        if !section.size.is_multiple_of(24) {
            return Err(Error::new(format!("thread-local descriptors {} of {:#x} bytes", section.name(), section.size)));
        }
        for index in 0..section.size / 24 {
            let at = section.addr + index * 24;
            let offset = memory
                .read(at + 16)
                .ok_or_else(|| Error::new(format!("thread-local descriptors {} outside the image's data", section.name())))?;
            if offset >= size {
                return Err(Error::new(format!(
                    "thread-local descriptor {index} of {} at offset {offset:#x} of a block of {size:#x}",
                    section.name()
                )));
            }
            if !memory.write(at, thunk) || !memory.write(at + 8, key) {
                return Err(Error::new(format!("thread-local descriptors {} outside the image's data", section.name())));
            }
        }
    }
    Ok(())
}

/// The image's initializers in the order dyld runs them, or its terminators, as addresses
/// where they run.
fn functions(macho: &MachO<'_>, memory: &Memory<'_>, base: u64, init: bool) -> Result<Vec<u64>, Error> {
    let slide = base.wrapping_sub(macho.first);
    let mut out = Vec::new();
    for section in &macho.sections {
        let entries: Vec<u64> = match section.kind() {
            S_MOD_INIT_FUNC_POINTERS if init => pointers(memory, section)?,
            S_MOD_TERM_FUNC_POINTERS if !init => pointers(memory, section)?,
            S_INIT_FUNC_OFFSETS if init => {
                let bytes = macho
                    .file(section.addr, section.size)
                    .filter(|it| it.len().is_multiple_of(4))
                    .ok_or_else(|| Error::new(format!("initializer offsets {} outside the image's file", section.name())))?;
                bytes
                    .chunks_exact(4)
                    .map(|it| base.wrapping_add(u64::from(u32::from_le_bytes([it[0], it[1], it[2], it[3]]))))
                    .collect()
            }
            _ => continue,
        };
        for (index, &entry) in entries.iter().enumerate() {
            if !macho.in_code(entry.wrapping_sub(slide)) {
                return Err(Error::new(format!("entry {index} of {} at {entry:#x} is not in the image's code", section.name())));
            }
        }
        out.extend(entries);
    }
    Ok(out)
}

fn pointers(memory: &Memory<'_>, section: &Section) -> Result<Vec<u64>, Error> {
    if !section.size.is_multiple_of(8) {
        return Err(Error::new(format!("function pointers {} of {:#x} bytes", section.name(), section.size)));
    }
    (0..section.size / 8)
        .map(|index| {
            memory.read(section.addr + index * 8).ok_or_else(|| {
                Error::new(format!("function pointers {} outside the image's writable data, where fixups reach", section.name()))
            })
        })
        .collect()
}

/// libunwind's `unw_dynamic_unwind_sections`.
#[repr(C)]
#[derive(Clone, Copy)]
struct UnwindSections {
    dso_base: usize,
    dwarf_section: usize,
    dwarf_section_length: usize,
    compact_unwind_section: usize,
    compact_unwind_section_length: usize,
}

/// An image's unwind tables, for the pcs of its code.
struct Unwind {
    code: std::ops::Range<u64>,
    sections: UnwindSections,
}

const MOST_UNWINDS: usize = 8;
static UNWINDS: [AtomicPtr<Unwind>; MOST_UNWINDS] = [const { AtomicPtr::new(std::ptr::null_mut()) }; MOST_UNWINDS];

/// libunwind's callback for a pc in no image dyld knows: the tables of the image whose code it
/// is in.
extern "C" fn find_unwind_sections(addr: usize, info: *mut UnwindSections) -> c_int {
    for slot in &UNWINDS {
        // SAFETY: a non-null slot is a leaked `Unwind`, never freed or written after it was
        // published (Release in `register_unwind`).
        let Some(unwind) = (unsafe { slot.load(Ordering::Acquire).as_ref() }) else { continue };
        if unwind.code.contains(&(addr as u64)) {
            // SAFETY: libunwind passes a live, writable `unw_dynamic_unwind_sections`.
            unsafe { info.write(unwind.sections) };
            return 1;
        }
    }
    0
}

type FindSections = extern "C" fn(usize, *mut UnwindSections) -> c_int;

/// Registers `find_unwind_sections` with the system's libunwind, once.
fn register_finder() -> Result<(), Error> {
    static REGISTERED: OnceLock<Result<(), Error>> = OnceLock::new();
    REGISTERED
        .get_or_init(|| {
            // SAFETY: a NUL-terminated name looked up in the global scope.
            let add = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"__unw_add_find_dynamic_unwind_sections".as_ptr()) };
            if add.is_null() {
                return Err(Error::new(
                    "the system's libunwind has no __unw_add_find_dynamic_unwind_sections (macOS 14 and later do), so \
                     the image's unwind tables can't be registered",
                ));
            }
            // SAFETY: libunwind's `int __unw_add_find_dynamic_unwind_sections(
            // unw_find_dynamic_unwind_sections)`, by its exported name.
            let add: unsafe extern "C" fn(FindSections) -> c_int = unsafe { std::mem::transmute(add) };
            // SAFETY: `find_unwind_sections` is callable from any thread for the life of the
            // process and only reads published `Unwind`s.
            let code = unsafe { add(find_unwind_sections) };
            if code != 0 {
                return Err(Error::new(format!("__unw_add_find_dynamic_unwind_sections returned {code}")));
            }
            Ok(())
        })
        .clone()
}

/// Makes the image's `__eh_frame` and `__unwind_info` what libunwind finds for its code. The
/// arena's code registers its own frames as it is placed.
fn register_unwind(macho: &MachO<'_>, base: u64) -> Result<(), Error> {
    let eh_frame = macho.section("__TEXT", "__eh_frame");
    let unwind_info = macho.section("__TEXT", "__unwind_info");
    if eh_frame.is_none() && unwind_info.is_none() {
        return Ok(());
    }
    let slide = base.wrapping_sub(macho.first);
    let start = macho.code().map(|it| it.start).min().unwrap_or(0).wrapping_add(slide);
    let end = macho.code().map(|it| it.end).max().unwrap_or(0).wrapping_add(slide);
    let place = |section: Option<&Section>| section.map_or((0, 0), |it| (it.addr.wrapping_add(slide) as usize, it.size as usize));
    let (dwarf_section, dwarf_section_length) = place(eh_frame);
    let (compact_unwind_section, compact_unwind_section_length) = place(unwind_info);
    register_finder()?;
    let unwind = Box::into_raw(Box::new(Unwind {
        code: start..end,
        sections: UnwindSections {
            dso_base: base as usize,
            dwarf_section,
            dwarf_section_length,
            compact_unwind_section,
            compact_unwind_section_length,
        },
    }));
    for slot in &UNWINDS {
        if slot.compare_exchange(std::ptr::null_mut(), unwind, Ordering::Release, Ordering::Relaxed).is_ok() {
            return Ok(());
        }
    }
    // SAFETY: the box just leaked above, which no slot took.
    drop(unsafe { Box::from_raw(unwind) });
    Err(Error::new(format!("unwind tables of more than {MOST_UNWINDS} images in one process")))
}

/// Maps, links and sets up the image in `bytes[..image_len]` from `object`, at `at` when
/// another process has the code memory there.
fn link(object: &sys::Granted, channel: &mut Channel, bytes: &[u8], image_len: u64, at: Option<u64>) -> Result<Loaded, Error> {
    let page = sys::page();
    let bytes = usize::try_from(image_len)
        .ok()
        .and_then(|len| bytes.get(..len))
        .ok_or_else(|| Error::new(format!("an image of {image_len:#x} bytes in a view of {:#x}", bytes.len())))?;
    let macho = MachO::parse(bytes, page)?;
    let object_len = object.len();
    if !image_len.is_multiple_of(page) || image_len > object_len {
        return Err(Error::new(format!(
            "an image of {image_len:#x} bytes in a code memory of {object_len:#x}: not whole pages of {page:#x} in it"
        )));
    }
    let arena = object_len - image_len;
    if arena != 0 && macho.span > image_len {
        return Err(Error::new(format!(
            "the image's memory ends at {:#x}, past the {image_len:#x} bytes of the code memory's image part, \
             where its arena starts: the image part has to hold the image's memory, zero pages included",
            macho.span
        )));
    }
    let span = if arena != 0 { object_len } else { macho.span };
    let apple = apple()?;
    check_stack(macho.stack)?;

    let mut dl = Dl::open(&macho)?;
    let base = sys::reserve(channel, span, at)?;
    if at.is_some_and(|at| at != base) {
        return Err(Error::new(format!("the image was reserved at {base:#x}, not {:#x}", at.unwrap_or_default())));
    }
    for segment in macho.segments.iter().filter(|it| it.mapped) {
        map_segment(object, channel, base, &macho, segment, page)?;
    }
    if arena != 0 {
        let to = base + image_len;
        let mapped = object.map(channel, image_len, arena, to, Access::Execute)?;
        if mapped != to {
            return Err(Error::new(format!("the arena was mapped at {mapped:#x}, not {to:#x}")));
        }
    }

    let mut segments = Vec::new();
    for segment in macho.segments.iter().filter(|it| it.writable()) {
        let offset = segment.vmaddr - macho.first;
        let len = segment.vmsize.next_multiple_of(page);
        // SAFETY: `base + offset .. + len` is the segment's private copy mapped just above
        // (its file's pages `Access::Copy`, the rest the reservation's zero pages made
        // writable), readable and writable, inside the reservation this function made;
        // nothing else refers to it until `memory` is dropped below, and the loader has one
        // thread.
        let bytes = unsafe { std::slice::from_raw_parts_mut((base + offset) as *mut u8, len as usize) };
        let file = segment.filesize.next_multiple_of(page).min(len);
        bytes[segment.filesize as usize..file as usize].fill(0);
        segments.push(Segment { vmaddr: segment.vmaddr, bytes });
    }
    let mut memory = Memory::new(segments);
    fixup::apply(&macho, &mut memory, base, &mut dl)?;
    thread_locals(&macho, &mut memory)?;
    let initializers = functions(&macho, &memory, base, true)?;
    let terminators = functions(&macho, &memory, base, false)?;
    drop(memory);

    for segment in macho.segments.iter().filter(|it| it.writable() && it.read_only_after_fixups()) {
        sys::protect(base + (segment.vmaddr - macho.first), segment.vmsize.next_multiple_of(page), Access::Read)?;
    }
    register_unwind(&macho, base)?;
    Ok(Loaded { base, entry: base + macho.entry, initializers, terminators, apple })
}

/// An image mapped, fixed up and set up; addresses are where it runs.
struct Loaded {
    base: u64,
    entry: u64,
    initializers: Vec<u64>,
    terminators: Vec<u64>,
    apple: *const *const c_char,
}

/// dyld's `ProgramVars`, the initializers' fifth argument.
#[repr(C)]
struct ProgramVars {
    mh: *const c_void,
    argc: *mut c_int,
    argv: *mut *mut *mut c_char,
    environ: *mut *mut *mut c_char,
    progname: *mut *const c_char,
}

type Initializer =
    unsafe extern "C" fn(c_int, *const *const c_char, *const *const c_char, *const *const c_char, *const ProgramVars);
type Main = unsafe extern "C" fn(c_int, *const *const c_char, *const *const c_char, *const *const c_char) -> c_int;
type Terminator = extern "C" fn();

/// `argv` as C has it, NUL-terminated, for the life of the process.
fn arguments(args: Vec<OsString>) -> Result<&'static mut [*mut c_char], Error> {
    let mut argv = Vec::with_capacity(args.len() + 1);
    for (index, arg) in args.into_iter().enumerate() {
        let arg = CString::new(arg.into_vec()).map_err(|_| Error::new(format!("argument {index} has a NUL byte")))?;
        argv.push(arg.into_raw());
    }
    argv.push(std::ptr::null_mut());
    Ok(argv.leak())
}

impl Program for Loaded {
    fn base(&self) -> u64 {
        self.base
    }

    fn enter(self: Box<Self>, args: Vec<OsString>) -> ! {
        let argv = match arguments(args) {
            Ok(argv) => argv,
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(127)
            }
        };
        let Ok(argc) = c_int::try_from(argv.len() - 1) else {
            eprintln!("{}", Error::new(format!("{} arguments", argv.len() - 1)));
            std::process::exit(127)
        };
        // dyld's `__progname`: `argv[0]` after its last slash.
        let progname = match argv.first().copied().filter(|it| !it.is_null()) {
            Some(first) => {
                // SAFETY: `first` is `argv[0]`, a NUL-terminated string leaked above.
                let bytes = unsafe { CStr::from_ptr(first) }.to_bytes();
                let skip = bytes.iter().rposition(|&it| it == b'/').map_or(0, |it| it + 1);
                // SAFETY: `skip` is at most the string's length, so within it or at its NUL.
                unsafe { first.add(skip).cast_const() }
            }
            None => c"".as_ptr(),
        };
        let argv = argv.as_mut_ptr();
        // SAFETY: crt_externs' variables, written on the loader's only thread before the
        // program's first instruction: what the program's `NXArgc`, `NXArgv` and `__progname`
        // read, as dyld sets them for a program it starts.
        let envp = unsafe {
            *_NSGetArgc() = argc;
            *_NSGetArgv() = argv;
            *_NSGetProgname() = progname;
            *_NSGetEnviron()
        };
        let vars = Box::leak(Box::new(ProgramVars {
            mh: self.base as *const c_void,
            // SAFETY: crt_externs' accessors have no preconditions.
            argc: unsafe { _NSGetArgc() },
            // SAFETY: as above.
            argv: unsafe { _NSGetArgv() },
            // SAFETY: as above.
            environ: unsafe { _NSGetEnviron() },
            // SAFETY: as above.
            progname: unsafe { _NSGetProgname() },
        }));
        // Registered before the initializers, so what those and `main` register with `atexit`
        // runs first, and the image's terminators from the last.
        for &terminator in &self.terminators {
            // SAFETY: `terminator` is in the image's code (`functions` checked), a function of
            // no arguments in memory mapped and fixed up for the life of the process.
            let terminator: Terminator = unsafe { std::mem::transmute(terminator as usize) };
            // SAFETY: as above.
            if unsafe { libc::atexit(terminator) } != 0 {
                eprintln!("{}", Error::new("atexit refused the image's terminator"));
                std::process::exit(127)
            }
        }
        let (argv, envp) = (argv.cast_const().cast::<*const c_char>(), envp.cast_const().cast::<*const c_char>());
        for &initializer in &self.initializers {
            // SAFETY: `initializer` is in the image's code (`functions` checked), in memory
            // mapped and fixed up for the life of the process, called as dyld calls one:
            // `argc`, `argv`, `envp`, `apple` and the program's variables.
            let initializer: Initializer = unsafe { std::mem::transmute(initializer as usize) };
            // SAFETY: as above; every pointer passed lives for the process.
            unsafe { initializer(argc, argv, envp, self.apple, vars) };
        }
        // SAFETY: the entry is in the image's code (`parse` checked), its `main` as LC_MAIN
        // names it, called as dyld calls it.
        let main: Main = unsafe { std::mem::transmute(self.entry as usize) };
        // SAFETY: as above.
        let code = unsafe { main(argc, argv, envp, self.apple) };
        // SAFETY: exit runs the atexit handlers, the image's terminators among them, then the
        // libraries' and ends the process, as `start` does after `main`.
        unsafe { libc::exit(code) }
    }
}

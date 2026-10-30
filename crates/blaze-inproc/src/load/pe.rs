//! blaze's loader for PE images (Windows x86-64), as the NT loader loads an executable but from
//! the daemon's memory object, with no file on disk.
//!
//! The image object holds the file as blaze-link lays it out: each section's bytes at its RVA,
//! all code first, data from a 64 KiB boundary on, the lazy arena after `image_len`. Views are
//! whole 64 KiB granules, so the image maps as three: headers and code executable and shared
//! with the daemon (so patches show), data as private copies, the arena executable. No view is
//! ever writable and executable. After base relocations (data only) and imports, read-only data
//! is made read-only; writable data stays copy-on-write.
//!
//! The program shares the loader's thread-locals: its `_tls_index` is the loader's, and its
//! template lives in a reserve of the loader's own block ([`RESERVE`]), which blaze-link biases
//! its thread-local offsets by. The NT loader starts each new thread's block from the loader's
//! raw template, into which the program's template is copied, then calls the loader's TLS
//! callback, which calls the program's.
//!
//! `main` sees the program's command line: the loader's own path, the first argument the
//! daemon spawned it with, is taken out of the process's command line in place, where the
//! program's C runtime and std read it.

mod parse;
#[cfg(test)]
mod tests;

use std::arch::asm;
use std::cell::UnsafeCell;
use std::ffi::{OsString, c_void};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use windows_sys::Win32::System::Diagnostics::Debug::{IMAGE_RUNTIME_FUNCTION_ENTRY, RtlAddFunctionTable};
use windows_sys::Win32::System::Environment::{GetCommandLineA, GetCommandLineW};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryExW};
use windows_sys::Win32::System::Memory::{PAGE_READWRITE, VirtualProtect};
use windows_sys::Win32::System::Threading::{ExitProcess, GetCurrentThreadStackLimits};

use self::parse::{Headers, Symbol, Tls, bad};
use super::{Image, Program, RESERVE_SIZE};
use crate::{Access, Error, TlsReserve, sys};

/// What the reserve's address is a multiple of: the NT loader allocates each thread's block
/// from the process heap, which aligns to 16, so no larger alignment holds at run time.
const ALIGN: u64 = 16;

const DLL_PROCESS_ATTACH: u32 = 1;

/// Not zero, so the reserve is raw data of the loader's template, never zero fill.
const PATTERN: u8 = 0xb1;

#[repr(C, align(16))]
struct Reserve(UnsafeCell<[u8; RESERVE_SIZE as usize]>);

const _: () = assert!(align_of::<Reserve>() as u64 == ALIGN);
const _: () = assert!(size_of::<Reserve>() as u64 == RESERVE_SIZE);
const _: () = assert!(!std::mem::needs_drop::<Reserve>());

thread_local! {
    /// A `const` initializer and no `Drop`: a plain thread-local of the loader's own static
    /// block (`.tls`), with no lazy registration.
    static RESERVE: Reserve = const { Reserve(UnsafeCell::new([PATTERN; RESERVE_SIZE as usize])) };
}

/// `IMAGE_TLS_DIRECTORY64`.
#[repr(C)]
#[derive(Clone, Copy)]
struct TlsDirectory {
    start: u64,
    end: u64,
    index: u64,
    callbacks: u64,
    zero: u32,
    characteristics: u32,
}

unsafe extern "C" {
    /// The C runtime's (`tlssup`): the loader's slot in each thread's array of blocks, which
    /// the NT loader writes before any code runs.
    static _tls_index: u32;
    /// The C runtime's: the loader's TLS directory, which its headers point at.
    static _tls_used: TlsDirectory;
}

type TlsCallback = unsafe extern "system" fn(*mut c_void, u32, *mut c_void);
type PcToFileHeader = unsafe extern "system" fn(*const c_void, *mut *mut c_void) -> *mut c_void;
type Entry = unsafe extern "system" fn(*mut c_void) -> u32;

/// Where the program's image object is mapped and how long it is, for [`pc_to_file_header`].
static IMAGE: OnceLock<(u64, u64)> = OnceLock::new();
/// The system's `RtlPcToFileHeader`.
static REAL: OnceLock<PcToFileHeader> = OnceLock::new();
/// The program's base and TLS callbacks, set when it is entered.
static CALLBACKS: OnceLock<(u64, Vec<u64>)> = OnceLock::new();

#[used]
#[unsafe(link_section = ".CRT$XLM")]
static ON_THREAD: TlsCallback = on_thread;

/// The loader's TLS callback: forwards each reason after the program is entered to the
/// program's callbacks, as the NT loader would call them were it the program's loader. It
/// reaches nothing of [`RESERVE`], so the reserve is in the blocks of loaders only.
unsafe extern "system" fn on_thread(_module: *mut c_void, reason: u32, _reserved: *mut c_void) {
    let Some((base, callbacks)) = CALLBACKS.get() else { return };
    for &callback in callbacks {
        // SAFETY: a TLS callback of the program, in its code, called as the NT loader calls one.
        unsafe { call(callback, *base, reason) };
    }
}

/// # Safety
///
/// `callback` is the address of a TLS callback of the program mapped at `base`.
unsafe fn call(callback: u64, base: u64, reason: u32) {
    // SAFETY: the caller's: `callback` is a function of this type.
    let callback: TlsCallback = unsafe { std::mem::transmute(callback as usize) };
    // SAFETY: as the NT loader calls a TLS callback: the module's base, the reason, no context.
    unsafe { callback(base as *mut c_void, reason, std::ptr::null_mut()) }
}

/// `RtlPcToFileHeader` that knows the program: the C++ runtime's `_CxxThrowException` (Rust's
/// panics) finds the image a throw's type information is in through it, and the system's
/// knows only modules it loaded.
unsafe extern "system" fn pc_to_file_header(pc: *const c_void, base: *mut *mut c_void) -> *mut c_void {
    if let Some(&(start, len)) = IMAGE.get() {
        if (pc as u64).wrapping_sub(start) < len {
            // SAFETY: the caller's out pointer, as `RtlPcToFileHeader` takes it.
            unsafe { *base = start as *mut c_void };
            return start as *mut c_void;
        }
    }
    match REAL.get() {
        // SAFETY: the system's `RtlPcToFileHeader`, with the caller's arguments.
        Some(real) => unsafe { real(pc, base) },
        None => {
            // SAFETY: as above.
            unsafe { *base = std::ptr::null_mut() };
            std::ptr::null_mut()
        }
    }
}

fn os(what: impl std::fmt::Display) -> Error {
    Error::new(format!("Windows: {what}: {}", std::io::Error::last_os_error()))
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

/// The TEB's word at `offset` from `gs`.
fn teb(offset: u64) -> u64 {
    let word: u64;
    // SAFETY: `gs` is the calling thread's TEB on Windows x86-64; the offsets read are the
    // TEB's fixed fields (`ThreadLocalStoragePointer` at 0x58, `ProcessEnvironmentBlock` at 0x60).
    unsafe { asm!("mov {word}, gs:[{offset}]", word = out(reg) word, offset = in(reg) offset, options(nostack, readonly, preserves_flags)) };
    word
}

fn peb() -> u64 {
    teb(0x60)
}

fn tls_index() -> u32 {
    // SAFETY: `_tls_index` is a plain `u32` the NT loader wrote before the loader started and
    // never changes.
    unsafe { std::ptr::read(&raw const _tls_index) }
}

/// The calling thread's block of the loader's thread-locals.
fn block() -> u64 {
    let slots = teb(0x58) as *const u64;
    // SAFETY: `ThreadLocalStoragePointer` is the thread's array of blocks, one slot per module
    // with thread-locals, and `_tls_index` is the loader's slot in it.
    unsafe { slots.add(tls_index() as usize).read() }
}

/// The loader's thread-locals as the calling thread has them.
struct Here {
    reserve: u64,
    block: u64,
    /// The loader's raw template, which each new thread's block starts as.
    template: u64,
}

impl Here {
    fn find() -> Result<Here, Error> {
        let reserve = RESERVE.with(|it| it.0.get() as u64);
        let block = block();
        // SAFETY: `_tls_used` is the C runtime's TLS directory of the loader, plain data the NT
        // loader only reads.
        let directory = unsafe { std::ptr::read(&raw const _tls_used) };
        let bias = reserve.wrapping_sub(block);
        let template = directory.end.wrapping_sub(directory.start);
        if directory.end < directory.start || bias > template || template - bias < RESERVE_SIZE {
            return Err(Error::new(format!(
                "the loader's thread-local reserve at {bias:#x} of its block is not in the {template:#x} bytes of \
                 its raw template, which new threads are started from"
            )));
        }
        if !reserve.is_multiple_of(ALIGN) {
            return Err(Error::new(format!("the loader's thread-local reserve at {reserve:#x}, not on {ALIGN}")));
        }
        Ok(Here { reserve, block, template: directory.start })
    }

    fn reserve(&self) -> TlsReserve {
        TlsReserve { bias: self.reserve.wrapping_sub(self.block) as i64, size: RESERVE_SIZE, align: ALIGN }
    }
}

pub(crate) fn reserve() -> Result<TlsReserve, Error> {
    Ok(Here::find()?.reserve())
}

struct Loaded {
    base: u64,
    entry: u64,
    callbacks: Vec<u64>,
}

impl Program for Loaded {
    fn base(&self) -> u64 {
        self.base
    }

    /// The program reads its arguments from the process's command line, which `load` made the
    /// program's; `args` is what the daemon spawned with after the loader's path, the same.
    fn enter(self: Box<Self>, _args: Vec<OsString>) -> ! {
        let Loaded { base, entry, callbacks } = *self;
        let (base, callbacks) = CALLBACKS.get_or_init(|| (base, callbacks));
        for &callback in callbacks {
            // SAFETY: a TLS callback of the program, called once for the process's start as the
            // NT loader calls it, before the entry.
            unsafe { call(callback, *base, DLL_PROCESS_ATTACH) };
        }
        // SAFETY: the image's `AddressOfEntryPoint`, in its code: the C runtime's
        // `mainCRTStartup`, which takes the PEB as the system's thread start passes it, runs
        // the initializers and `main`, and exits the process.
        let entry: Entry = unsafe { std::mem::transmute(entry as usize) };
        // SAFETY: as above; the image is mapped, linked and its thread-locals set up.
        let code = unsafe { entry(peb() as *mut c_void) };
        // SAFETY: ends the process, as the system's thread start does when the entry returns.
        unsafe { ExitProcess(code) }
    }
}

pub(crate) fn load(image: Image<'_>) -> Result<Box<dyn Program>, Error> {
    let Image { object, bytes, image_len, base, reserve, channel } = image;
    let bytes = usize::try_from(image_len)
        .ok()
        .and_then(|len| bytes.get(..len))
        .ok_or_else(|| Error::new(format!("an image of {image_len:#x} bytes in a view of {:#x}", bytes.len())))?;
    let headers = parse::parse(bytes)?;
    let code_end = headers.code_end(sys::page())?;
    let here = Here::find()?;
    if here.reserve() != reserve {
        return Err(Error::new(format!("the loader's reserve is {:?}, where it said {reserve:?}", here.reserve())));
    }
    let tls = parse::tls(&headers)?;
    if let Some(tls) = &tls {
        fits(tls, reserve)?;
    }
    let libraries = parse::imports(&headers)?;
    let functions = parse::functions(&headers, code_end)?;
    check_stack(headers.stack_reserve)?;

    let len = object.len();
    let at = place(channel, &headers, len, base)?;
    object.map(channel, 0, code_end, at, Access::Execute)?;
    if image_len > code_end {
        object.map(channel, code_end, image_len - code_end, at + code_end, Access::Copy)?;
    }
    if len > image_len {
        object.map(channel, image_len, len - image_len, at + image_len, Access::Execute)?;
    }
    IMAGE.set((at, len)).map_err(|_| Error::new("a second program loaded into one loader"))?;

    relocate(&headers, at)?;
    zero_tails(&headers, at);
    let real = system_pc_to_file_header()?;
    let modules = link(&headers, &libraries, at)?;
    if let Some(tls) = &tls {
        thread_locals(tls, at, &here)?;
    }
    for section in headers.sections.iter().filter(|it| !it.code() && !it.writable()) {
        sys::protect(at + section.rva, section.extent, Access::Read)?;
    }
    if let Some((rva, count)) = functions {
        // SAFETY: the image's function table, checked in order and in its code, mapped at
        // `at + rva` for the life of the process (the mapping is never undone).
        if !unsafe { RtlAddFunctionTable((at + rva) as *const IMAGE_RUNTIME_FUNCTION_ENTRY, count, at) } {
            return Err(os(format_args!("RtlAddFunctionTable of {count} functions at {:#x}", at + rva)));
        }
    }
    for module in modules {
        interpose(module, &real)?;
    }
    drop_first_argument();
    let callbacks = tls.map(|it| it.callbacks.iter().map(|rva| at + rva).collect()).unwrap_or_default();
    Ok(Box::new(Loaded { base: at, entry: at + headers.entry, callbacks }))
}

fn fits(tls: &Tls, reserve: TlsReserve) -> Result<(), Error> {
    if tls.len() > reserve.size || tls.align > reserve.align {
        return Err(bad(format!(
            "thread-locals of {:#x} bytes aligned to {}, for a reserve of {:#x} bytes aligned to {}: blaze-link \
             must keep a program's thread-locals within the loader's reserve",
            tls.len(),
            tls.align,
            reserve.size,
            reserve.align
        )));
    }
    Ok(())
}

/// The program runs on the loader's main thread, so that has to have the stack it asks for.
fn check_stack(want: u64) -> Result<(), Error> {
    let (mut low, mut high) = (0usize, 0usize);
    // SAFETY: writes the calling thread's stack limits to the two locals.
    unsafe { GetCurrentThreadStackLimits(&mut low, &mut high) };
    let have = high.saturating_sub(low) as u64;
    if want > have {
        return Err(bad(format!(
            "a stack of {want:#x} bytes (SizeOfStackReserve), where the loader's main thread has {have:#x}: the \
             loader has to be linked with a stack reserve at least as large"
        )));
    }
    Ok(())
}

/// Reserves where the image goes: exactly the base the daemon chose when it made the code
/// memory, which every view of it in the daemon's patches assumes. Taken is an error.
fn place(channel: &mut crate::channel::Channel, headers: &Headers<'_>, len: u64, base: Option<u64>) -> Result<u64, Error> {
    let Some(base) = base else {
        return Err(Error::new("the daemon granted a PE image with no base: its code memory chooses one"));
    };
    if !headers.relocatable && base != headers.image_base {
        return Err(bad(format!(
            "an image without base relocations (based at {:#x}) has to be mapped at {base:#x}",
            headers.image_base
        )));
    }
    sys::reserve(channel, len, Some(base)).map_err(|error| {
        Error::new(format!("the image's base {base:#x} ({len:#x} bytes) is taken in the run process: {error}"))
    })
}

/// Rebases the data from `image_base` to `at`. Code is never written: a relocation into it is
/// an error, whether or not the image moved.
fn relocate(headers: &Headers<'_>, at: u64) -> Result<(), Error> {
    let delta = at.wrapping_sub(headers.image_base);
    for rva in parse::relocations(headers)? {
        let rva = rva?;
        match headers.section_of(rva, 8) {
            Some(section) if !section.code() => {}
            Some(section) => {
                return Err(bad(format!(
                    "a base relocation at RVA {rva:#x}, in section {} of code: blaze's loader shares code with the \
                     daemon and never writes it, so blaze-link must address from code RIP-relative only, with \
                     absolute addresses in data",
                    section.name()
                )));
            }
            None => return Err(bad(format!("a base relocation at RVA {rva:#x}, in no section"))),
        }
        if delta != 0 {
            let place = (at + rva) as *mut u64;
            // SAFETY: `rva..rva + 8` is in a section of data, mapped as a private copy (writable)
            // at `at`; the loader reaches it through raw pointers only.
            unsafe { place.write_unaligned(place.read_unaligned().wrapping_add(delta)) };
        }
    }
    Ok(())
}

/// Zeroes each section of data past its raw bytes, as the NT loader maps it, where the object
/// holds anything else there.
fn zero_tails(headers: &Headers<'_>, at: u64) {
    for section in headers.sections.iter().filter(|it| !it.code() && it.raw_len < it.extent) {
        let tail = (section.rva + section.raw_len) as usize..section.end() as usize;
        let tail_bytes = &headers.bytes[tail.clone()];
        let (Some(first), Some(last)) =
            (tail_bytes.iter().position(|it| *it != 0), tail_bytes.iter().rposition(|it| *it != 0))
        else {
            continue;
        };
        // SAFETY: inside the section, which `parse` checked is inside the image, mapped as a
        // private copy at `at`.
        unsafe { ((at as usize + tail.start + first) as *mut u8).write_bytes(0, last + 1 - first) };
    }
}

/// The system's `RtlPcToFileHeader`, as ntdll and kernel32 export it.
fn system_pc_to_file_header() -> Result<[u64; 2], Error> {
    let mut out = [0; 2];
    for (index, name) in ["ntdll.dll", "kernel32.dll"].into_iter().enumerate() {
        let name_wide = wide(name);
        // SAFETY: a NUL-terminated name; both modules are in every process.
        let module = unsafe { GetModuleHandleW(name_wide.as_ptr()) };
        if module.is_null() {
            return Err(os(format_args!("GetModuleHandleW of {name}")));
        }
        out[index] = resolve(module, c"RtlPcToFileHeader".as_ptr().cast(), || format!("RtlPcToFileHeader in {name}"))?;
    }
    // SAFETY: the address GetProcAddress gave for `RtlPcToFileHeader`, of that type.
    let real: PcToFileHeader = unsafe { std::mem::transmute(out[0] as usize) };
    let _ = REAL.set(real);
    Ok(out)
}

/// `symbol` (a NUL-terminated name, or an ordinal below 0x10000) in `module`.
fn resolve(module: *mut c_void, symbol: *const u8, what: impl Fn() -> String) -> Result<u64, Error> {
    // SAFETY: `module` is loaded (and never freed); `symbol` is the caller's name or ordinal.
    match unsafe { GetProcAddress(module, symbol) } {
        Some(function) => Ok(function as usize as u64),
        None => Err(os(format_args!("GetProcAddress of {}", what()))),
    }
}

/// Loads each library the image imports and fills its address slots: the modules, in order.
fn link(headers: &Headers<'_>, libraries: &[parse::Library<'_>], at: u64) -> Result<Vec<u64>, Error> {
    let ours = pc_to_file_header as PcToFileHeader as usize as u64;
    let mut modules = Vec::with_capacity(libraries.len());
    for library in libraries {
        let label = String::from_utf8_lossy(library.name);
        if !library.name.is_ascii() {
            return Err(bad(format!("an import from a library named {label}, not ASCII")));
        }
        let name: Vec<u16> = library.name.iter().map(|it| u16::from(*it)).chain([0]).collect();
        // SAFETY: a NUL-terminated name; the standard search order; the module is never freed.
        let module = unsafe { LoadLibraryExW(name.as_ptr(), std::ptr::null_mut(), 0) };
        if module.is_null() {
            return Err(os(format_args!("LoadLibraryExW of {label}, which the program imports")));
        }
        modules.push(module as u64);
        for symbol in library.symbols(headers) {
            let (slot, symbol) = symbol?;
            let address = match symbol {
                Symbol::Name(b"RtlPcToFileHeader") => ours,
                // `parse` checked a NUL follows the name in the image's bytes, which stay mapped.
                Symbol::Name(name) => {
                    resolve(module, name.as_ptr(), || format!("{} in {label}", String::from_utf8_lossy(name)))?
                }
                Symbol::Ordinal(ordinal) => {
                    resolve(module, usize::from(ordinal) as *const u8, || format!("ordinal {ordinal} in {label}"))?
                }
            };
            let place = (at + slot) as *mut u64;
            // SAFETY: `parse` checked the slot is in a section of data, mapped as a private copy
            // (writable until the loader protects it) at `at`.
            unsafe { place.write_unaligned(address) };
        }
    }
    Ok(modules)
}

/// Points the slots of `module`'s import address table that hold the system's
/// `RtlPcToFileHeader` at [`pc_to_file_header`]: the program's libraries (the C++ runtime's
/// `_CxxThrowException`) look up the program's image through it.
fn interpose(module: u64, real: &[u64; 2]) -> Result<(), Error> {
    let read = |offset: u64| -> u32 {
        // SAFETY: `module` is a module the system loaded (and the loader never frees); its DOS
        // and PE headers are mapped readable at the offsets its headers give, which the system
        // checked when it loaded it.
        unsafe { ((module + offset) as *const u32).read_unaligned() }
    };
    let optional = u64::from(read(0x3c)) + 24;
    if read(optional + 108) <= 12 {
        return Ok(());
    }
    let (rva, size) = (u64::from(read(optional + 112 + 8 * 12)), u64::from(read(optional + 116 + 8 * 12)));
    if rva == 0 || size < 8 {
        return Ok(());
    }
    if !rva.is_multiple_of(8) {
        return Err(Error::new(format!("Windows: the import address table of the module at {module:#x} is not on 8")));
    }
    let slots = (module + rva) as *mut u64;
    let count = (size / 8) as usize;
    // SAFETY: the module's import address table, which the system filled and maps readable.
    let hits: Vec<usize> = (0..count).filter(|&index| real.contains(&unsafe { slots.add(index).read() })).collect();
    if hits.is_empty() {
        return Ok(());
    }
    let mut old = 0;
    // SAFETY: changes the protection of the module's import address table only, restored below.
    if unsafe { VirtualProtect(slots.cast(), size as usize, PAGE_READWRITE, &mut old) } == 0 {
        return Err(os(format_args!("VirtualProtect of the import address table of the module at {module:#x}")));
    }
    let ours = pc_to_file_header as PcToFileHeader as usize as u64;
    for index in hits {
        // SAFETY: an 8-aligned slot of the table, writable now; other threads may call through
        // it meanwhile, so it is written whole, atomically.
        unsafe { AtomicU64::from_ptr(slots.add(index)) }.store(ours, Ordering::Release);
    }
    let mut was = 0;
    // SAFETY: restores what the table was.
    if unsafe { VirtualProtect(slots.cast(), size as usize, old, &mut was) } == 0 {
        return Err(os(format_args!("VirtualProtect restoring the import address table at {module:#x}")));
    }
    Ok(())
}

/// Gives the program the loader's TLS index and its template in the reserve: the calling
/// thread's now, and every thread's after through the loader's raw template.
fn thread_locals(tls: &Tls, at: u64, here: &Here) -> Result<(), Error> {
    let index = (at + tls.index) as *mut u32;
    // SAFETY: `parse` checked the 4 bytes are in a section of data, mapped as a private copy
    // (writable) at `at`.
    unsafe { index.write_unaligned(tls_index()) };
    let template = (at + tls.start) as *const u8;
    let (init, zero) = (tls.init as usize, tls.zero as usize);
    if init + zero == 0 {
        return Ok(());
    }
    // SAFETY: the calling thread's reserve, `RESERVE_SIZE` bytes of its block in an
    // `UnsafeCell`, of which the first `init + zero` (checked no more) are written from the
    // relocated template; nothing refers to it, as the loader never reads it and the program
    // has not started.
    unsafe { fill(here.reserve as *mut u8, template, init, zero) };

    let raw = here.template + here.reserve.wrapping_sub(here.block);
    let len = init + zero;
    let mut old = 0;
    // SAFETY: changes the protection of the reserve's part of the loader's raw template only,
    // restored below; nothing reads it but the NT loader starting a thread, and the loader has
    // one thread.
    if unsafe { VirtualProtect(raw as *const c_void, len, PAGE_READWRITE, &mut old) } == 0 {
        return Err(os(format_args!("VirtualProtect of the loader's thread-local template at {raw:#x}")));
    }
    // SAFETY: `Here::find` checked the reserve's `RESERVE_SIZE` bytes are in the raw template,
    // writable now.
    unsafe { fill(raw as *mut u8, template, init, zero) };
    let mut was = 0;
    // SAFETY: restores what the template was.
    if unsafe { VirtualProtect(raw as *const c_void, len, old, &mut was) } == 0 {
        return Err(os(format_args!("VirtualProtect restoring the loader's thread-local template at {raw:#x}")));
    }
    Ok(())
}

/// # Safety
///
/// `out` is writable for `init + zero` bytes, `template` readable for `init`, not overlapping.
unsafe fn fill(out: *mut u8, template: *const u8, init: usize, zero: usize) {
    // SAFETY: the caller's.
    unsafe {
        std::ptr::copy_nonoverlapping(template, out, init);
        out.add(init).write_bytes(0, zero);
    }
}

/// Takes the loader's path, the first argument, out of the process's command line in place:
/// the PEB's, which `GetCommandLineW` and the C runtime's `_wcmdln` share, and the ANSI copy
/// `GetCommandLineA` and `_acmdln` share. The program's `mainCRTStartup` parses its `argv`
/// from them and std reads `GetCommandLineW`.
fn drop_first_argument() {
    // SAFETY: the PEB's `ProcessParameters` (at 0x20) points at the process's parameters, whose
    // `CommandLine` (at 0x70) is a `UNICODE_STRING`: `Length` in bytes, then (at 8) the buffer,
    // NUL-terminated past `Length`. The loader has one thread, so nothing reads it meanwhile.
    let (length, buffer) = unsafe {
        let line = ((peb() + 0x20) as *const u64).read() + 0x70;
        (line as *mut u16, ((line + 8) as *const *mut u16).read())
    };
    if !buffer.is_null() {
        // SAFETY: as above: `Length / 2` characters at `buffer`.
        let chars = usize::from(unsafe { length.read() }) / 2;
        // SAFETY: as above.
        let text = unsafe { std::slice::from_raw_parts_mut(buffer, chars) };
        let skip = parse::after_first(text);
        if skip > 0 {
            text.copy_within(skip.., 0);
            text[chars - skip] = 0;
            // SAFETY: as above.
            unsafe { length.write(((chars - skip) * 2) as u16) };
        }
    }
    // SAFETY: kernelbase's NUL-terminated copy of the command line, in its heap, writable;
    // one thread.
    let line = unsafe { GetCommandLineW() } as *mut u16;
    if line != buffer {
        // SAFETY: as above.
        unsafe { drop_first(line) };
    }
    // SAFETY: as above, the ANSI copy.
    unsafe { drop_first(GetCommandLineA() as *mut u8) };
}

/// # Safety
///
/// `line` is a writable NUL-terminated string nothing else reads meanwhile.
unsafe fn drop_first<T: Copy + Into<u32>>(line: *mut T) {
    if line.is_null() {
        return;
    }
    let mut len = 0;
    // SAFETY: the caller's: NUL-terminated.
    while unsafe { line.add(len).read() }.into() != 0 {
        len += 1;
    }
    // SAFETY: the caller's: `len` characters and the NUL.
    let text = unsafe { std::slice::from_raw_parts_mut(line, len + 1) };
    let skip = parse::after_first(&text[..len]);
    text.copy_within(skip.., 0);
}

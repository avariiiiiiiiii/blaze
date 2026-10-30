//! blaze's loader on Linux (x86-64 and AArch64, glibc): maps an ELF image as blaze-link lays it
//! out (PIE `ET_DYN`, each `PT_LOAD` on pages of its own at a file offset equal to its
//! address) from the image object, links it to the system's libraries with `dlopen` and
//! `dlsym`, puts its thread-locals in the loader's static thread-local reserve, registers its
//! unwind tables and runs its initializers and `main` in this process: what `ld.so` and the C
//! runtime's start would have done for it as a file, with no file.
//!
//! The image's bytes are untrusted: [`parse`] reads them and [`reloc`] relocates, both safe
//! and the same code the tests run over plain buffers. This file maps, calls the system and
//! jumps.
//!
//! Code is mapped shared with the daemon's writable view, so a patch is a store there; data
//! and RELRO are private copies; the code memory's arena after the image is mapped executable
//! at the same offsets. One `mmap` per segment, and relocation in one pass over each table
//! with resolved symbols cached by index, as glibc's `ld.so` and mold's output make it cheap.

mod parse;
mod reloc;
mod tls;
#[cfg(test)]
mod tests;

use std::ffi::{CStr, CString, OsString, c_char, c_int, c_void};
use std::os::unix::ffi::OsStringExt;

use parse::{Array, Elf, Load, Machine};
use reloc::{Imports, Memory, Place, Segment};
use tls::Loader;

use super::{Image, Program};
use crate::channel::Channel;
use crate::{Access, Error, TlsReserve, sys};

unsafe extern "C" {
    static mut environ: *mut *mut c_char;
    fn dlvsym(handle: *mut c_void, symbol: *const c_char, version: *const c_char) -> *mut c_void;
    fn __cxa_atexit(f: unsafe extern "C" fn(*mut c_void), arg: *mut c_void, dso: *mut c_void) -> c_int;
    /// libgcc's: registers the `.eh_frame` records from `begin` up to a zero terminator.
    fn __register_frame(begin: *const u8);
}

/// What the loader keeps aside for a program's thread-locals.
pub(crate) fn reserve() -> Result<TlsReserve, Error> {
    Ok(Loader::find()?.reserve())
}

pub(crate) fn load(image: Image<'_>) -> Result<Box<dyn Program>, Error> {
    let Image { object, bytes, image_len, base, reserve, channel } = image;
    let mut source = Granted { object, channel };
    Ok(Box::new(link(&mut source, bytes, image_len, base, reserve, &mut Dl)?))
}

/// Where segments are mapped from: the image object the daemon granted, or in the tests a
/// buffer.
trait Source {
    fn len(&self) -> u64;
    /// Address space of `len` bytes with no access, at `at` or anywhere.
    fn reserve(&mut self, len: u64, at: Option<u64>) -> Result<u64, Error>;
    /// `len` bytes from `offset` of the object at `at`, inside the reservation.
    fn map(&mut self, offset: u64, len: u64, at: u64, access: Access) -> Result<u64, Error>;
}

struct Granted<'a> {
    object: &'a sys::Granted,
    channel: &'a mut Channel,
}

impl Source for Granted<'_> {
    fn len(&self) -> u64 {
        self.object.len()
    }

    fn reserve(&mut self, len: u64, at: Option<u64>) -> Result<u64, Error> {
        sys::reserve(self.channel, len, at)
    }

    fn map(&mut self, offset: u64, len: u64, at: u64, access: Access) -> Result<u64, Error> {
        self.object.map(self.channel, offset, len, at, access)
    }
}

/// Symbols from the global scope (the loader, its libraries and those `dlopen`ed for the
/// image), and IFUNC resolvers called as glibc calls them.
struct Dl;

fn dlerror() -> Option<String> {
    // SAFETY: dlerror returns null or a NUL-terminated message valid until the next dl call
    // on this thread; it is copied at once.
    let message = unsafe { libc::dlerror() };
    (!message.is_null()).then(|| {
        // SAFETY: as above.
        unsafe { CStr::from_ptr(message) }.to_string_lossy().into_owned()
    })
}

#[cfg(target_arch = "aarch64")]
#[repr(C)]
struct IfuncArg {
    size: u64,
    hwcap: u64,
    hwcap2: u64,
    hwcap3: u64,
    hwcap4: u64,
}

impl Imports for Dl {
    fn find(&mut self, name: &CStr, version: Option<&CStr>) -> Result<u64, String> {
        dlerror();
        // SAFETY: both strings are NUL-terminated (`CStr`s of the image's string table) and
        // live for the call; RTLD_DEFAULT searches the global scope.
        let found = unsafe {
            match version {
                Some(version) => dlvsym(libc::RTLD_DEFAULT, name.as_ptr(), version.as_ptr()),
                None => libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()),
            }
        };
        match (found.is_null(), dlerror()) {
            (true, Some(why)) => Err(why),
            _ => Ok(found as u64),
        }
    }

    fn ifunc(&mut self, resolver: u64) -> u64 {
        #[cfg(target_arch = "x86_64")]
        {
            // SAFETY: `resolver` is in the image's code (`reloc` checked), its IFUNC resolver
            // for this relocation, called as glibc calls one on x86-64: no arguments,
            // returning the address. The code is mapped executable and its data relocated.
            let resolver: unsafe extern "C" fn() -> u64 = unsafe { std::mem::transmute(resolver as usize) };
            // SAFETY: as above.
            unsafe { resolver() }
        }
        #[cfg(target_arch = "aarch64")]
        {
            const IFUNC_ARG_HWCAP: u64 = 1 << 62;
            // SAFETY: getauxval has no preconditions; an absent entry is 0.
            let aux = |kind: u64| unsafe { libc::getauxval(kind as libc::c_ulong) } as u64;
            let arg = IfuncArg {
                size: size_of::<IfuncArg>() as u64,
                hwcap: aux(libc::AT_HWCAP as u64),
                hwcap2: aux(libc::AT_HWCAP2 as u64),
                hwcap3: aux(29),
                hwcap4: aux(30),
            };
            // SAFETY: as on x86-64, called as glibc calls an AArch64 resolver: the hardware
            // capabilities with `_IFUNC_ARG_HWCAP`, and an `__ifunc_arg_t` that names its size.
            let resolver: unsafe extern "C" fn(u64, *const IfuncArg) -> u64 =
                unsafe { std::mem::transmute(resolver as usize) };
            // SAFETY: as above; `arg` lives for the call.
            unsafe { resolver(arg.hwcap | IFUNC_ARG_HWCAP, &arg) }
        }
    }
}

/// The static TLSDESC resolver, as glibc's `_dl_tlsdesc_return`: the descriptor's address in
/// `rax`, its second word (the offset from the thread pointer) returned there, nothing else
/// touched.
#[cfg(target_arch = "x86_64")]
#[unsafe(naked)]
extern "C" fn tlsdesc_static() {
    std::arch::naked_asm!("mov rax, qword ptr [rax + 8]", "ret")
}

/// The same on AArch64: the descriptor in `x0`, the offset returned in `x0`.
#[cfg(target_arch = "aarch64")]
#[unsafe(naked)]
extern "C" fn tlsdesc_static() {
    std::arch::naked_asm!("hint #34", "ldr x0, [x0, #8]", "ret")
}

fn place(base: u64, loader: &Loader) -> Place {
    Place {
        base,
        tp: loader.reserve().bias,
        module: loader.module(),
        dtp: loader.offset(),
        tlsdesc: tlsdesc_static as usize as u64,
    }
}

/// Anonymous zero pages, readable and writable, at `at`: a writable segment's memory past its
/// file's pages.
fn anonymous(at: u64, len: u64) -> Result<(), Error> {
    // SAFETY: `at..at + len` is inside the reservation `link` made for the image and not yet
    // mapped (the reservation's own no-access pages): MAP_FIXED replaces only them.
    let mapped = unsafe {
        libc::mmap(
            at as *mut c_void,
            len as usize,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED,
            -1,
            0,
        )
    };
    if mapped == libc::MAP_FAILED || mapped as u64 != at {
        return Err(Error::new(format!(
            "mapping {len:#x} bytes of zero pages at {at:#x}: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

fn open_needed(elf: &Elf<'_>) -> Result<(), Error> {
    for &name in &elf.dynamic.needed {
        let name = elf.string(name)?;
        // SAFETY: `name` is NUL-terminated (a `CStr` of the image's string table) and lives
        // for the call; the handle stays open for the life of the process.
        let handle = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
        if handle.is_null() {
            return Err(Error::new(format!(
                "dlopen of `{}`, which the image needs: {}",
                name.to_string_lossy(),
                dlerror().unwrap_or_else(|| "no reason given".to_owned())
            )));
        }
    }
    Ok(())
}

fn map_load(source: &mut impl Source, base: u64, load: &Load, page: u64) -> Result<(), Error> {
    let file = load.filesz.next_multiple_of(page);
    let memory = load.memsz.next_multiple_of(page);
    if file != 0 {
        let at = base + load.vaddr;
        let mapped = source.map(load.vaddr, file, at, load.access())?;
        if mapped != at {
            return Err(Error::new(format!("{} was mapped at {mapped:#x}, not {at:#x}", load.describe())));
        }
    }
    if memory > file {
        anonymous(base + load.vaddr + file, memory - file)?;
    }
    Ok(())
}

/// The function addresses of an array of the image's, read once relocated.
fn addresses(elf: &Elf<'_>, memory: &Memory<'_>, array: Option<Array>, base: u64, name: &str) -> Result<Vec<u64>, Error> {
    let Some(array) = array else { return Ok(Vec::new()) };
    let mut out = Vec::with_capacity(array.count as usize);
    for index in 0..array.count {
        let at = array.vaddr + index * 8;
        let value = match memory.read(at) {
            Some(value) => value,
            None => parse::u64_at(elf.file(at, 8)?, 0).unwrap_or_default(),
        };
        if value == 0 {
            return Err(Error::new(format!("entry {index} of the image's {name} at {:#x} is null", base + at)));
        }
        out.push(value);
    }
    Ok(out)
}

/// Maps, links and sets up the image in `bytes[..image_len]` from `source`, at `at` when
/// another process has the code memory there.
fn link(
    source: &mut impl Source,
    bytes: &[u8],
    image_len: u64,
    at: Option<u64>,
    reserve: TlsReserve,
    imports: &mut impl Imports,
) -> Result<Loaded, Error> {
    let page = sys::page();
    let bytes = usize::try_from(image_len)
        .ok()
        .and_then(|len| bytes.get(..len))
        .ok_or_else(|| Error::new(format!("an image of {image_len:#x} bytes in a view of {:#x}", bytes.len())))?;
    let elf = Elf::parse(bytes, page, Machine::host())?;
    let loader = Loader::find()?;
    if loader.reserve() != reserve {
        return Err(Error::new(format!(
            "the loader's thread-local reserve is {:?}, not the {reserve:?} the image was linked for",
            loader.reserve()
        )));
    }
    if let Some(tls) = elf.tls {
        if tls.memsz > reserve.size || tls.align > reserve.align {
            return Err(Error::new(format!(
                "the image's thread-locals take {:#x} bytes aligned to {}, more than the loader's reserve of \
                 {:#x} bytes aligned to {}",
                tls.memsz, tls.align, reserve.size, reserve.align
            )));
        }
    }
    let object_len = source.len();
    if !image_len.is_multiple_of(page) || image_len > object_len {
        return Err(Error::new(format!(
            "an image of {image_len:#x} bytes in a code memory of {object_len:#x}: not whole pages of {page:#x} in it"
        )));
    }
    let arena = object_len - image_len;
    if arena != 0 && elf.span > image_len {
        return Err(Error::new(format!(
            "the image's memory ends at {:#x}, past the {image_len:#x} bytes of the code memory's image part, \
             where its arena starts: the image part has to hold the image's memory, .bss included",
            elf.span
        )));
    }
    let span = if arena != 0 { object_len } else { elf.span };

    open_needed(&elf)?;
    let base = source.reserve(span, at)?;
    if at.is_some_and(|at| at != base) {
        return Err(Error::new(format!("the image was reserved at {base:#x}, not {:#x}", at.unwrap_or_default())));
    }
    for load in &elf.loads {
        map_load(source, base, load, page)?;
    }
    if arena != 0 {
        let to = base + image_len;
        let mapped = source.map(image_len, arena, to, Access::Execute)?;
        if mapped != to {
            return Err(Error::new(format!("the arena was mapped at {mapped:#x}, not {to:#x}")));
        }
    }

    let mut segments = Vec::with_capacity(elf.loads.len());
    for load in elf.loads.iter().filter(|it| it.writable()) {
        let len = load.memsz.next_multiple_of(page);
        // SAFETY: `base + vaddr .. + len` is the segment's private copy mapped just above (its
        // file's pages `Access::Copy`, the rest anonymous zero pages), readable and writable,
        // inside the reservation this function made; nothing else refers to it until `memory`
        // is dropped below, and the loader has one thread.
        let bytes = unsafe { std::slice::from_raw_parts_mut((base + load.vaddr) as *mut u8, len as usize) };
        let file = load.filesz.next_multiple_of(page).min(len);
        bytes[load.filesz as usize..file as usize].fill(0);
        segments.push(Segment { vaddr: load.vaddr, memsz: load.memsz, bytes });
    }
    let mut memory = Memory::new(segments);
    reloc::relocate(&elf, &mut memory, &place(base, &loader), imports)?;

    let preinit = addresses(&elf, &memory, elf.dynamic.preinit_array, base, "DT_PREINIT_ARRAY")?;
    let init_array = addresses(&elf, &memory, elf.dynamic.init_array, base, "DT_INIT_ARRAY")?;
    let fini_array = addresses(&elf, &memory, elf.dynamic.fini_array, base, "DT_FINI_ARRAY")?;
    if let Some(tls) = elf.tls {
        let template = match memory.bytes(tls.vaddr, tls.filesz) {
            Some(template) => template,
            None => elf.file(tls.vaddr, tls.filesz)?,
        };
        loader.install(template, tls.memsz, page)?;
    }
    drop(memory);

    if let Some(eh_frame) = elf.eh_frame {
        // SAFETY: `base + eh_frame` is the image's `.eh_frame`, in a read-only segment mapped
        // for the life of the process (`load::start` leaks every mapping), whose records
        // `parse` walked to a zero terminator inside its pages: all libgcc's
        // `__register_frame` reads. It is never deregistered.
        unsafe { __register_frame((base + eh_frame) as *const u8) };
    }
    if let Some(relro) = &elf.relro {
        sys::protect(base + relro.start, relro.end - relro.start, Access::Read)?;
    }
    Ok(Loaded {
        base,
        entry: base + elf.entry,
        preinit,
        init: elf.dynamic.init.map(|it| base + it),
        init_array,
        fini_array,
        fini: elf.dynamic.fini.map(|it| base + it),
    })
}

/// An image mapped, relocated and set up; addresses are where it runs.
struct Loaded {
    base: u64,
    entry: u64,
    preinit: Vec<u64>,
    init: Option<u64>,
    init_array: Vec<u64>,
    fini_array: Vec<u64>,
    fini: Option<u64>,
}

type Init = unsafe extern "C" fn(c_int, *mut *mut c_char, *mut *mut c_char);
type Main = unsafe extern "C" fn(c_int, *mut *mut c_char, *mut *mut c_char) -> c_int;
type Fini = unsafe extern "C" fn(*mut c_void);

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
        let argv = argv.as_mut_ptr();
        // SAFETY: glibc's `environ`, read once on the loader's only thread.
        let envp = unsafe { environ };
        let dso = self.base as *mut c_void;
        // The C runtime registers `ld.so`'s finalizers before the program's initializers, so
        // what those and `main` register with `atexit` runs first: the same here, DT_FINI
        // first so it runs last, after DT_FINI_ARRAY from its end.
        for &fini in self.fini.iter().chain(&self.fini_array) {
            // SAFETY: `fini` is a finalizer of the image, mapped and relocated for the life of
            // the process, which takes no arguments: called with one, as glibc's `atexit`
            // registers functions with `__cxa_atexit`, it ignores it.
            let fini: Fini = unsafe { std::mem::transmute(fini as usize) };
            // SAFETY: `fini` as above; `dso` is the image's `__dso_handle` (its base), which
            // nothing unloads.
            if unsafe { __cxa_atexit(fini, std::ptr::null_mut(), dso) } != 0 {
                eprintln!("{}", Error::new("__cxa_atexit refused the image's finalizer"));
                std::process::exit(127)
            }
        }
        for &init in self.preinit.iter().chain(&self.init).chain(&self.init_array) {
            // SAFETY: `init` is an initializer of the image (DT_PREINIT_ARRAY, DT_INIT, then
            // DT_INIT_ARRAY, as glibc runs them), nonzero, in memory mapped and relocated for
            // the life of the process, called as glibc calls one: `argc`, `argv`, `envp`.
            let init: Init = unsafe { std::mem::transmute(init as usize) };
            // SAFETY: as above; `argv` and `envp` live for the process.
            unsafe { init(argc, argv, envp) };
        }
        // SAFETY: the entry is in the image's code (`parse` checked), its `main` as blaze-link
        // links an image for blaze's loader, called as the C runtime calls `main`.
        let main: Main = unsafe { std::mem::transmute(self.entry as usize) };
        // SAFETY: as above.
        let code = unsafe { main(argc, argv, envp) };
        // SAFETY: exit runs the atexit handlers, the image's finalizers among them, then the
        // libraries' and ends the process, as the C runtime does after `main`.
        unsafe { libc::exit(code) }
    }
}

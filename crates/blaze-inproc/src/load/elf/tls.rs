//! The loader's static thread-local block: the reserve a program's thread-locals live in, at a
//! fixed offset from every thread's pointer, and the loader's `PT_TLS` initialization image,
//! which glibc copies into each thread it starts, so a thread the program starts later begins
//! with the program's template.

use std::cell::UnsafeCell;
use std::ffi::{c_int, c_void};

use crate::load::{RESERVE_ALIGN, RESERVE_SIZE};
use crate::{Error, TlsReserve};

/// Not zero, so the reserve is in `.tdata`, which each new thread is started from, and never
/// in `.tbss`.
const PATTERN: u8 = 0xb1;

const PT_LOAD: u32 = 1;
const PT_TLS: u32 = 7;
const PT_GNU_RELRO: u32 = 0x6474_e552;

#[repr(C, align(64))]
struct Reserve(UnsafeCell<[u8; RESERVE_SIZE as usize]>);

const _: () = assert!(align_of::<Reserve>() as u64 == RESERVE_ALIGN);
const _: () = assert!(size_of::<Reserve>() as u64 == RESERVE_SIZE);
const _: () = assert!(!std::mem::needs_drop::<Reserve>());

thread_local! {
    /// A `const` initializer and no `Drop`: a plain static thread-local of the loader's own
    /// block, with no lazy registration.
    static RESERVE: Reserve = const { Reserve(UnsafeCell::new([PATTERN; RESERVE_SIZE as usize])) };
}

/// The calling thread's reserve.
pub(super) fn here() -> u64 {
    RESERVE.with(|it| it.0.get() as u64)
}

fn thread_pointer() -> u64 {
    let tp: u64;
    #[cfg(target_arch = "x86_64")]
    // SAFETY: on x86-64 Linux `fs` is the thread's TCB, whose first word glibc sets to its own
    // address (`tcbhead_t.tcb`), the thread pointer; the load reads only that word.
    unsafe {
        std::arch::asm!("mov {}, qword ptr fs:[0]", out(reg) tp, options(nostack, readonly, preserves_flags));
    }
    #[cfg(target_arch = "aarch64")]
    // SAFETY: EL0 may read TPIDR_EL0, the thread pointer; the read touches no memory.
    unsafe {
        std::arch::asm!("mrs {}, tpidr_el0", out(reg) tp, options(nomem, nostack, preserves_flags));
    }
    tp
}

/// One program header of the loader's own, slid.
#[derive(Clone, Copy)]
struct Header {
    p_type: u32,
    flags: u32,
    at: u64,
    filesz: u64,
    memsz: u64,
}

/// What `dl_iterate_phdr` says of the main program, the loader.
struct Found {
    seen: bool,
    module: usize,
    block: u64,
    headers: Vec<Header>,
}

unsafe extern "C" fn first(info: *mut libc::dl_phdr_info, _: usize, data: *mut c_void) -> c_int {
    // SAFETY: `data` is the `Found` `Loader::find` passed, live and borrowed by nothing else
    // for the call.
    let found = unsafe { &mut *data.cast::<Found>() };
    // SAFETY: glibc passes a `dl_phdr_info` live for the call.
    let info = unsafe { &*info };
    found.seen = true;
    found.module = info.dlpi_tls_modid;
    found.block = info.dlpi_tls_data as u64;
    for index in 0..usize::from(info.dlpi_phnum) {
        // SAFETY: `dlpi_phdr` points at `dlpi_phnum` program headers of the loaded object,
        // mapped as long as it is (the main program: the whole process).
        let phdr = unsafe { &*info.dlpi_phdr.add(index) };
        found.headers.push(Header {
            p_type: phdr.p_type,
            flags: phdr.p_flags,
            at: info.dlpi_addr.wrapping_add(phdr.p_vaddr),
            filesz: phdr.p_filesz,
            memsz: phdr.p_memsz,
        });
    }
    // The first object is the main program: stop.
    1
}

/// The loader's thread-local block as the calling thread has it.
pub(super) struct Loader {
    reserve: u64,
    tp: u64,
    module: u64,
    /// The reserve's offset in the loader's block.
    offset: u64,
    /// The loader's `PT_TLS` initialization image.
    template: u64,
    headers: Vec<Header>,
}

impl Loader {
    /// The calling thread's view of the loader's block, checked to hold the whole reserve in
    /// its initialized part.
    pub(super) fn find() -> Result<Loader, Error> {
        let mut found = Found { seen: false, module: 0, block: 0, headers: Vec::with_capacity(16) };
        // SAFETY: `first` reads only what glibc passes it and writes `found`, which outlives
        // the call.
        unsafe { libc::dl_iterate_phdr(Some(first), (&raw mut found).cast()) };
        if !found.seen {
            return Err(Error::new("dl_iterate_phdr names no main program"));
        }
        let tls = found
            .headers
            .iter()
            .find(|it| it.p_type == PT_TLS)
            .copied()
            .ok_or_else(|| Error::new("the loader has no PT_TLS, so no thread-local reserve"))?;
        if found.module == 0 || found.block == 0 {
            return Err(Error::new(format!(
                "the loader's thread-local block (module {}) is not allocated for this thread",
                found.module
            )));
        }
        let reserve = here();
        let offset = reserve
            .checked_sub(found.block)
            .filter(|&it| it.checked_add(RESERVE_SIZE).is_some_and(|end| end <= tls.filesz))
            .ok_or_else(|| {
                Error::new(format!(
                    "the loader's reserve at {reserve:#x} is not in the {:#x} initialized bytes of its \
                     thread-local block at {:#x} (`.tdata`), which new threads are started from",
                    tls.filesz, found.block
                ))
            })?;
        Ok(Loader {
            reserve,
            tp: thread_pointer(),
            module: found.module as u64,
            offset,
            template: tls.at,
            headers: found.headers,
        })
    }

    pub(super) fn reserve(&self) -> TlsReserve {
        TlsReserve { bias: self.reserve.wrapping_sub(self.tp) as i64, size: RESERVE_SIZE, align: RESERVE_ALIGN }
    }

    pub(super) fn module(&self) -> u64 {
        self.module
    }

    pub(super) fn offset(&self) -> u64 {
        self.offset
    }

    /// The protection the loader's page at `page` has, from its program headers as glibc
    /// applied them: its `PT_LOAD`'s, less writing inside `PT_GNU_RELRO`.
    fn protection(&self, page: u64, size: u64) -> Result<c_int, Error> {
        let load = self
            .headers
            .iter()
            .find(|it| it.p_type == PT_LOAD && page >= it.at - it.at % size && page < it.at + it.memsz)
            .ok_or_else(|| Error::new(format!("the loader's page at {page:#x} is in none of its segments")))?;
        let mut protection = 0;
        if load.flags & 4 != 0 {
            protection |= libc::PROT_READ;
        }
        if load.flags & 2 != 0 {
            protection |= libc::PROT_WRITE;
        }
        if load.flags & 1 != 0 {
            protection |= libc::PROT_EXEC;
        }
        let relro = self.headers.iter().find(|it| it.p_type == PT_GNU_RELRO);
        if let Some(relro) = relro {
            let start = relro.at - relro.at % size;
            let end = (relro.at + relro.memsz) / size * size;
            if page >= start && page < end {
                protection &= !libc::PROT_WRITE;
            }
        }
        Ok(protection)
    }

    /// Puts the program's thread-local template (`image`, then zeros to `memsz`) in the
    /// calling thread's reserve and in the loader's initialization image at the reserve's
    /// offset, so every thread started after has it too.
    pub(super) fn install(&self, image: &[u8], memsz: u64, page: u64) -> Result<(), Error> {
        if memsz > RESERVE_SIZE || image.len() as u64 > memsz {
            return Err(Error::new(format!(
                "a thread-local template of {:#x} bytes in a block of {memsz:#x}, for a reserve of {RESERVE_SIZE:#x}",
                image.len()
            )));
        }
        let fill = |out: &mut [u8]| {
            out[..image.len()].copy_from_slice(image);
            out[image.len()..].fill(0);
        };
        // SAFETY: `self.reserve` is the calling thread's `Reserve` (`find` read it on this
        // thread, the loader's only one), `RESERVE_SIZE` bytes in an `UnsafeCell` live as long
        // as the thread, of which the first `memsz` are written; nothing else refers to it: the
        // loader never reads it and the program it is for has not started.
        fill(unsafe { std::slice::from_raw_parts_mut(self.reserve as *mut u8, memsz as usize) });

        let at = self.template + self.offset;
        let start = at - at % page;
        let end = (at + memsz).next_multiple_of(page);
        let mut pages = Vec::with_capacity(((end - start) / page) as usize);
        let mut page_at = start;
        while page_at < end {
            pages.push((page_at, self.protection(page_at, page)?));
            page_at += page;
        }
        let protect = |page_at: u64, protection: c_int| {
            // SAFETY: a page of the loader's own image holding its PT_TLS initialization image;
            // adding or taking back write access changes no byte, and keeps it readable.
            if unsafe { libc::mprotect(page_at as *mut c_void, page as usize, protection) } != 0 {
                return Err(Error::new(format!(
                    "mprotect of the loader's thread-local template page at {page_at:#x}: {}",
                    std::io::Error::last_os_error()
                )));
            }
            Ok(())
        };
        for &(page_at, protection) in &pages {
            protect(page_at, protection | libc::PROT_READ | libc::PROT_WRITE)?;
        }
        // SAFETY: `at..at + memsz` is inside the loader's PT_TLS initialization image (`find`
        // checked the reserve's offset plus `RESERVE_SIZE` is within its file bytes, and
        // `memsz <= RESERVE_SIZE`), writable now; glibc reads it only when it starts a thread,
        // and the loader has one thread.
        fill(unsafe { std::slice::from_raw_parts_mut(at as *mut u8, memsz as usize) });
        for &(page_at, protection) in &pages {
            protect(page_at, protection)?;
        }
        Ok(())
    }
}

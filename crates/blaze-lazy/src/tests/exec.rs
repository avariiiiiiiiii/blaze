//! Executable memory for the tests that run the stubs on this machine: an anonymous mapping
//! written once, then made executable with the instruction cache made coherent.
//!
//! Unsafe for `mmap`, `mprotect` and the cache maintenance; the safe API takes the bytes and
//! hands out addresses, never a pointer into the mapping.

#![allow(unsafe_code)]

pub(crate) struct Code {
    at: *mut u8,
    len: usize,
}

// SAFETY: the mapping is never written after `Code::new` returns, so threads only read and
// run it.
unsafe impl Send for Code {}
// SAFETY: as above.
unsafe impl Sync for Code {}

impl Code {
    /// `bytes`, executable.
    pub(crate) fn new(bytes: &[u8]) -> Code {
        let len = bytes.len().next_multiple_of(1 << 14).max(1 << 14);
        let at = map(len);
        // SAFETY: `at` is a fresh mapping of `len >= bytes.len()` writable bytes (made so by
        // `map`), which nothing else refers to.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), at, bytes.len()) };
        seal(at, len);
        Code { at, len }
    }

    /// The address of the byte at `offset`.
    pub(crate) fn address(&self, offset: u32) -> u64 {
        assert!((offset as usize) < self.len);
        self.at as u64 + u64::from(offset)
    }
}

impl Drop for Code {
    fn drop(&mut self) {
        // SAFETY: the mapping `Code::new` made, of `len` bytes; no thread runs it once the
        // test that made it holds no address of it.
        unsafe { libc::munmap(self.at.cast(), self.len) };
    }
}

#[cfg(target_os = "macos")]
fn map(len: usize) -> *mut u8 {
    // SAFETY: an anonymous mapping at no fixed address; the result is checked.
    let at = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
            libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_JIT,
            -1,
            0,
        )
    };
    assert_ne!(at, libc::MAP_FAILED, "mmap: {}", std::io::Error::last_os_error());
    // SAFETY: makes this thread's view of MAP_JIT pages writable; `seal` turns it back.
    unsafe { libc::pthread_jit_write_protect_np(0) };
    at.cast()
}

#[cfg(target_os = "macos")]
fn seal(at: *mut u8, len: usize) {
    // SAFETY: as in `map`: this thread's view of MAP_JIT pages becomes executable again.
    unsafe { libc::pthread_jit_write_protect_np(1) };
    icache(at as usize, len);
}

#[cfg(not(target_os = "macos"))]
fn map(len: usize) -> *mut u8 {
    // SAFETY: an anonymous mapping at no fixed address; the result is checked.
    let at = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert_ne!(at, libc::MAP_FAILED, "mmap: {}", std::io::Error::last_os_error());
    at.cast()
}

#[cfg(not(target_os = "macos"))]
fn seal(at: *mut u8, len: usize) {
    // SAFETY: the whole mapping `map` made.
    let sealed = unsafe { libc::mprotect(at.cast(), len, libc::PROT_READ | libc::PROT_EXEC) };
    assert_eq!(sealed, 0, "mprotect: {}", std::io::Error::last_os_error());
    icache(at as usize, len);
}

#[cfg(target_arch = "x86_64")]
fn icache(_: usize, _: usize) {}

/// Cleans the data cache to the point of unification and invalidates the instruction cache
/// over the range, by the line sizes `CTR_EL0` gives (readable at EL0 on Linux and macOS).
#[cfg(target_arch = "aarch64")]
fn icache(start: usize, len: usize) {
    use std::arch::asm;

    let ctr: u64;
    // SAFETY: reads a register; Linux and macOS let EL0 read CTR_EL0.
    unsafe { asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack, preserves_flags)) };
    let data = 4usize << ((ctr >> 16) & 0xf);
    let instruction = 4usize << (ctr & 0xf);
    let end = start + len;
    let mut line = start & !(data - 1);
    while line < end {
        // SAFETY: a cache maintenance operation on a mapped address.
        unsafe { asm!("dc cvau, {}", in(reg) line, options(nostack, preserves_flags)) };
        line += data;
    }
    // SAFETY: a barrier.
    unsafe { asm!("dsb ish", options(nostack, preserves_flags)) };
    let mut line = start & !(instruction - 1);
    while line < end {
        // SAFETY: as above.
        unsafe { asm!("ic ivau, {}", in(reg) line, options(nostack, preserves_flags)) };
        line += instruction;
    }
    // SAFETY: barriers.
    unsafe { asm!("dsb ish", "isb", options(nostack, preserves_flags)) };
}

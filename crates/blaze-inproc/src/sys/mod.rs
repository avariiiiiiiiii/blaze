//! The platform: memory objects and their mappings, the channel's transport, handing objects
//! to a run process, its threads, the instruction cache and stack walks. Every `unsafe` of
//! the crate outside the loader is under this module, behind the safe API each platform
//! module exports with the same names and these signatures (`Error` is `crate::Error`,
//! `Channel` is `crate::channel::Channel`, `Access` is `crate::Access`):
//!
//! - `fn page() -> u64`: the mapping granule (Linux's page size, 16 KiB on macOS AArch64,
//!   the 64 KiB allocation granularity on Windows).
//! - `fn check_icache() -> Result<(), Error>`: whether `View::publish` from the daemon's
//!   view makes code right in another process's view of the same pages here (AArch64: an
//!   instruction cache that does not alias by virtual address); an error names what the CPU
//!   reports.
//! - `fn random(out: &mut [u8; 16]) -> Result<(), Error>`: from the OS's generator.
//! - `struct Object` (`Send`): a memory object this process holds, zero when made.
//!   `fn create(name: &str, len: u64) -> Result<Object, Error>`, `fn len(&self) -> u64`,
//!   `fn duplicate(&self) -> Result<Object, Error>` (another reference to the same object),
//!   `fn map_here(&self, access: Access) -> Result<View, Error>` (the whole object, in this
//!   process; the daemon maps `Access::Shared`).
//! - `struct View` (`Send + Sync`): a mapping in this process, unmapped when dropped.
//!   `fn len(&self) -> usize`, `fn address(&self) -> u64`,
//!   `fn bytes(&self, range: Range<usize>) -> Result<&[u8], Error>`,
//!   `fn bytes_mut(&mut self, range: Range<usize>) -> Result<&mut [u8], Error>` (an error on a
//!   view not writable), `fn word(&self, at: usize) -> &AtomicU64` and
//!   `fn word32(&self, at: usize) -> &AtomicU32` (`at` aligned and in bounds, else a panic:
//!   callers check), and `fn publish(&self, range: Range<usize>)`, the instruction-cache
//!   maintenance for code written there (`dc cvau`, `dsb ish`, `ic ivau`, `dsb ish`, `isb`
//!   on AArch64 as `CTR_EL0` asks; nothing on x86-64).
//! - `struct Stream` (`Send`): a connected byte stream of the channel.
//!   `fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error>` (0 when the peer hung up),
//!   `fn write_all(&mut self, bytes: &[u8]) -> Result<(), Error>`,
//!   `fn set_timeout(&mut self, timeout: Option<Duration>) -> Result<(), Error>`,
//!   `fn raw(&self) -> i64` (the fd or handle the program's runtime is given; it stays open
//!   when the `Stream` is forgotten) and `fn from_raw(raw: i64) -> Result<Stream, Error>`.
//! - `struct Listener` (daemon, `Send`): `fn bind(dir: &Path) -> Result<(Listener, String),
//!   Error>`, the `String` being what `connect` takes; `fn accept(&self) -> Result<(Stream,
//!   u32), Error>`, the peer's process ID as the OS says it, refusing peers of another user.
//! - `fn connect(address: &str) -> Result<Stream, Error>` (loader).
//! - `fn send_objects(channel: &mut Channel, pid: u32, grant: &[u8], objects: &[(&Object,
//!   Rights)]) -> Result<Remote, Error>` (daemon): sends the GRANT frame of body `grant`
//!   (kind `kinds::GRANT`, sequence 0) and, with it or after it, `objects` so the loader of
//!   process `pid` can map each with the rights `Rights` says and no more.
//!   `fn recv_objects(channel: &mut Channel, count: usize) -> Result<Vec<Granted>, Error>`
//!   (loader), called once the GRANT frame was read, gives them in the same order.
//! - `struct Remote` (daemon, `Send`): what the daemon keeps to act in the run process;
//!   `fn map(&self, object: u32, offset: u64, len: u64, at: u64, access: Access) ->
//!   Result<u64, Error>` serves a loader's MAP frame where the platform maps from the
//!   daemon's side (Windows, `at` 0 meaning anywhere), and is an error elsewhere.
//! - `fn reserve(channel: &mut Channel, len: u64, at: Option<u64>) -> Result<u64, Error>`
//!   (loader): address space of `len` bytes mapped with no access, at `at` or anywhere,
//!   aligned to `page()`, for `Granted::map` to map into.
//! - `struct Granted` (loader): an object the loader was given. `fn len(&self) -> u64`,
//!   `fn map(&self, channel: &mut Channel, offset: u64, len: u64, at: u64, access: Access) ->
//!   Result<u64, Error>` (at `at` exactly, inside a reservation of `reserve` or in free
//!   address space; never over anything else), `fn view(&self, channel: &mut Channel) ->
//!   Result<View, Error>` (the whole object, read-only, to parse the image from).
//! - `fn protect(at: u64, len: u64, access: Access) -> Result<(), Error>` (loader): the
//!   protection of pages of a private copy it mapped (`Access::Copy` to `Access::Read` for an
//!   ELF RELRO segment after relocation); never of code.
//! - `fn threads(pid: u32, out: &mut Vec<u64>) -> Result<(), Error>`: replaces `out` with the
//!   IDs of the live threads of process `pid` as `thread_id` names them; none for a process
//!   that is gone.
//! - `fn thread_id() -> u64`: the calling thread's, as the OS names it system-wide.
//! - `fn serialize()`: a context synchronization of the calling thread (`isb`; `cpuid` on
//!   x86-64).
//! - `fn backtrace(out: &mut [u64]) -> Option<usize>`: the return addresses of the calling
//!   thread's frames, all of them up to its outermost; `None` when they don't fit or the walk
//!   could not finish.

#[cfg(any(
    all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
    all(target_os = "macos", target_arch = "aarch64"),
))]
mod unix;

#[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
mod linux;
#[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
pub(crate) use linux::*;

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod macos;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub(crate) use macos::*;

#[cfg(all(windows, target_arch = "x86_64"))]
mod windows;
#[cfg(all(windows, target_arch = "x86_64"))]
pub(crate) use windows::*;

#[cfg(not(any(
    all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
    all(target_os = "macos", target_arch = "aarch64"),
    all(windows, target_arch = "x86_64"),
)))]
mod none;
#[cfg(not(any(
    all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
    all(target_os = "macos", target_arch = "aarch64"),
    all(windows, target_arch = "x86_64"),
)))]
pub(crate) use none::*;

/// What a run process may do with an object it is given.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Rights {
    /// Map it to read, execute or copy, never to write it shared: the image, a premapped one.
    Code,
    /// Map it shared, writable: the fence's control words.
    Control,
}

/// The boot record the loader of this process left at `at`
/// ([`crate::runtime::BOOT_VAR`]).
pub(crate) fn boot(at: u64) -> Result<crate::runtime::Boot, crate::Error> {
    if at == 0 || !at.is_multiple_of(align_of::<crate::runtime::Boot>() as u64) {
        return Err(crate::Error::new(format!("a boot record at {at:#x}")));
    }
    // SAFETY: the loader of this process (blaze-inproc-run) put a `Boot` at `at` in its own
    // static data, which lives as long as the process, and named it in the environment
    // before it jumped into the program; it is never written after. The address is non-null
    // and aligned (checked above). The fields are plain integers: any bits are a value.
    Ok(unsafe { std::ptr::read(at as *const crate::runtime::Boot) })
}

/// The control words the loader of this process mapped at `at`, `len` bytes, for the rest of
/// the process.
pub(crate) fn adopt_words(at: u64, len: u64) -> Result<&'static [std::sync::atomic::AtomicU64], crate::Error> {
    if at == 0 || !at.is_multiple_of(8) || len < crate::fence::CONTROL_BYTES || len > isize::MAX as u64 {
        return Err(crate::Error::new(format!("control words of {len} bytes at {at:#x}")));
    }
    // SAFETY: the loader mapped the control object shared and writable at `at`, `len` bytes,
    // and never unmaps it (`load::main` leaks its mapping); the boot record says so. It is
    // non-null, 8-aligned and of `len / 8` whole words (checked above). Every access to it,
    // here and in the daemon's process, is atomic, so a `&[AtomicU64]` to it is sound for
    // the rest of the process.
    Ok(unsafe { std::slice::from_raw_parts(at as *const std::sync::atomic::AtomicU64, (len / 8) as usize) })
}

impl crate::fence::Words for View {
    fn words(&self) -> usize {
        self.len() / 8
    }

    fn word(&self, index: usize) -> &std::sync::atomic::AtomicU64 {
        View::word(self, index * 8)
    }
}

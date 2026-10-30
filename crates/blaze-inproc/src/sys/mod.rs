//! The platform: memory objects and their mappings, the channel's transport, handing objects
//! to a run process, its threads, the instruction cache and stack walks. Every `unsafe` of
//! the crate is under this module, behind the safe API each platform module exports with the
//! same names:
//!
//! - `page() -> u64`: the mapping granule (Linux's page size, 16 KiB on macOS AArch64, the
//!   64 KiB allocation granularity on Windows).
//! - `check_icache() -> Result<(), Error>`: whether [`View::publish`] from the daemon's view
//!   makes code right in another process's view of the same pages here (AArch64: a PIPT
//!   instruction cache); an error names what the CPU reports.
//! - `Object`: a memory object this process holds. `Object::create(name, len)`, `len()`,
//!   `map_here(Access) -> View`.
//! - `View`: a mapping in this process, unmapped when dropped. `len()`, `address()`,
//!   `bytes(range)`, `bytes_mut(range)` (writable views only), `word(at) -> &AtomicU64` and
//!   `word32(at) -> &AtomicU32` (`at` aligned and in bounds, or a panic: callers check), and
//!   `publish(range)`, the instruction-cache maintenance for code written there.
//! - `Stream`: a connected byte stream of the channel. `read(&mut [u8]) -> usize` (0 at the
//!   peer's hangup), `write_all(&[u8])`, `set_timeout(Option<Duration>)`, `raw() -> i64` (the
//!   fd or handle the program's runtime is given) and `Stream::from_raw(i64)`.
//! - `Listener` (daemon): `bind(dir) -> (Listener, address)` where `address` is what
//!   `connect` takes; `accept() -> (Stream, Peer)`, the peer's process ID as the OS says it,
//!   refusing peers of another user.
//! - `connect(address) -> Stream` (loader).
//! - `send_objects(stream, peer_pid, frame, objects) -> Remote` (daemon): sends the encoded
//!   GRANT `frame` with `objects` so the loader at the other end can map them, each with the
//!   rights `Rights` says and no more; `recv_objects(stream, frame) -> Vec<Granted>` (loader),
//!   called once the GRANT frame was read, in the same order.
//! - `Remote` (daemon): what the daemon keeps to act in the run process; `Remote::map(&self,
//!   object index, offset, len, at, access) -> address` serves a loader's MAP frame where the
//!   platform maps from the daemon's side (Windows), and refuses elsewhere.
//! - `reserve(channel, len, at) -> address` (loader): address space for an image, mapped
//!   with no access, at `at` or anywhere.
//! - `Granted`: an object the loader was given. `len()`, `map(channel, offset, len, at,
//!   access) -> address` (`at` fixed, inside a reservation or free), `view() -> View` (the
//!   whole object, read-only, to parse the image from).
//! - `threads(pid, out)`: the IDs of the live threads of process `pid` as `thread_id`
//!   names them.
//! - `thread_id() -> u64`: the calling thread's.
//! - `serialize()`: a context synchronization of the calling thread (`isb`, `cpuid`).
//! - `backtrace(out) -> Option<usize>`: the return addresses of the calling thread's frames,
//!   all of them up to its outermost, `None` when they don't fit or the walk could not finish.
//! - `protect(address, len, access)` (loader): the protection of pages of a private copy
//!   it mapped (an ELF RELRO segment made read-only after relocation); never of code.

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

impl crate::fence::Words for View {
    fn words(&self) -> usize {
        self.len() / 8
    }

    fn word(&self, index: usize) -> &std::sync::atomic::AtomicU64 {
        View::word(self, index * 8)
    }
}

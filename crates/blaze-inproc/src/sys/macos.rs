//! macOS on AArch64: a memory object is a Mach memory entry (`mach_make_memory_entry_64` with
//! `MAP_MEM_NAMED_CREATE`, readable, writable and executable), mapped shared and writable in
//! the daemon. A run process is given entries over the same pages that carry fewer rights:
//! readable and executable for an image (`Rights::Code`), readable and writable for the
//! control words. The kernel caps every mapping of an entry, its current and its maximum
//! protection, by the entry's protection (`vm_map_enter_mem_object`, `KERN_INVALID_RIGHT`), so
//! a run process can map its image to read, execute or copy and never to write it shared.
//!
//! W^X is per mapping: `vm_map_enter` refuses only a mapping both writable and executable, so a
//! page writable in the daemon's map and executable in a run process's is allowed. Those pages
//! carry no code signature: a page runs unsigned only in a process whose map does not enforce
//! code signing (`CS_ENFORCEMENT`, which the hardened runtime sets unless the binary has
//! `com.apple.security.cs.allow-unsigned-executable-memory`), else `vm_fault` kills it at the
//! first such page it runs. `allow-jit` does not help: `MAP_JIT` is for anonymous memory of the
//! process's own. The executable is signed ad hoc by the linker, without the hardened runtime;
//! [`overlay`] checks before it maps code and says so.
//!
//! The start hook lays an overlay over its executable with one `mach_vm_map` of
//! `VM_FLAGS_FIXED | VM_FLAGS_OVERWRITE`, which replaces dyld's mapping of those pages in one
//! step of the task's map lock.
//!
//! The ports go by the daemon's rendezvous service ([`rendezvous`]), since a Unix socket can't
//! carry them.

mod rendezvous;
#[cfg(test)]
mod tests;

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::ops::Range;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, AtomicU64};

pub(crate) use super::unix::{Listener, Stream, backtrace, connect, random, serialize};
pub(crate) use rendezvous::{Remote, recv_objects, send_objects};
use crate::channel::Channel;
use crate::handshake::Overlay;
use crate::{Access, Error};

type Port = u32;
type Kern = c_int;

const KERN_SUCCESS: Kern = 0;
const MACH_PORT_NULL: Port = 0;
const MACH_PORT_RIGHT_SEND: u32 = 0;

const VM_PROT_READ: c_int = 1;
const VM_PROT_WRITE: c_int = 2;
const VM_PROT_EXECUTE: c_int = 4;
const VM_FLAGS_FIXED: c_int = 0;
const VM_FLAGS_ANYWHERE: c_int = 1;
const VM_FLAGS_OVERWRITE: c_int = 0x4000;
const VM_INHERIT_SHARE: u32 = 0;
const VM_INHERIT_COPY: u32 = 1;
const VM_INHERIT_NONE: u32 = 2;
const MAP_MEM_NAMED_CREATE: c_int = 0x02_0000;

/// `csops` status flags (`<kern/cs_blobs.h>`).
const CS_OPS_STATUS: u32 = 0;
const CS_ENFORCEMENT: u32 = 0x1000;
const CS_RUNTIME: u32 = 0x1_0000;

/// `<sys/proc_info.h>`'s flavor for the 64-bit IDs of a process's threads, as
/// `pthread_threadid_np` gives them.
const PROC_PIDLISTTHREADIDS: c_int = 28;

unsafe extern "C" {
    fn mach_task_self() -> Port;
    fn mach_make_memory_entry_64(
        target: Port,
        size: *mut u64,
        offset: u64,
        permission: c_int,
        handle: *mut Port,
        parent: Port,
    ) -> Kern;
    fn mach_vm_map(
        target: Port,
        address: *mut u64,
        size: u64,
        mask: u64,
        flags: c_int,
        object: Port,
        offset: u64,
        copy: c_int,
        current: c_int,
        maximum: c_int,
        inheritance: u32,
    ) -> Kern;
    fn mach_vm_deallocate(target: Port, address: u64, size: u64) -> Kern;
    fn mach_port_deallocate(task: Port, name: Port) -> Kern;
    fn mach_port_mod_refs(task: Port, name: Port, right: u32, delta: c_int) -> Kern;
    fn mach_error_string(code: Kern) -> *const c_char;
    /// libplatform's: makes code written at `start` right for instruction fetch on every core
    /// (`dc cvau`, `dsb ish`, `ic ivau`, `dsb ish`, `isb`).
    fn sys_icache_invalidate(start: *mut c_void, len: usize);
    fn csops(pid: c_int, ops: u32, out: *mut c_void, size: usize) -> c_int;
    /// dyld's: the Mach header of the main executable, where dyld mapped it.
    fn _NSGetMachExecuteHeader() -> *const c_void;
}

fn task() -> Port {
    // SAFETY: mach_task_self has no preconditions; it names this task's own port.
    unsafe { mach_task_self() }
}

/// An error naming a Mach call and its `kern_return_t`.
fn kern(what: &str, code: Kern) -> Error {
    // SAFETY: mach_error_string returns a static NUL-terminated string for any code.
    let reason = unsafe { CStr::from_ptr(mach_error_string(code)) };
    Error::new(format!("{what}: kern_return {code:#x} ({})", reason.to_string_lossy()))
}

pub(crate) fn page() -> u64 {
    // SAFETY: sysconf has no preconditions.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page > 0 { page as u64 } else { 16384 }
}

fn sysctl_int(name: &CStr) -> Result<i64, Error> {
    let mut value = [0u8; 8];
    let mut len = value.len();
    // SAFETY: `name` is NUL-terminated; `value` has `len` writable bytes and `len` is live.
    let got = unsafe { libc::sysctlbyname(name.as_ptr(), value.as_mut_ptr().cast(), &raw mut len, std::ptr::null_mut(), 0) };
    if got != 0 {
        return Err(Error::new(format!("sysctl {}: {}", name.to_string_lossy(), std::io::Error::last_os_error())));
    }
    match len {
        4 => Ok(i64::from(i32::from_ne_bytes([value[0], value[1], value[2], value[3]]))),
        8 => Ok(i64::from_ne_bytes(value)),
        _ => Err(Error::new(format!("sysctl {} is {len} bytes", name.to_string_lossy()))),
    }
}

/// Apple silicon's level-1 instruction caches are physically indexed and tagged, so the
/// maintenance `sys_icache_invalidate` does through the daemon's view holds for every view of
/// the same pages. EL0 can't read `CTR_EL0` on macOS (XNU leaves `SCTLR_EL1.UCT` clear, so the
/// read traps), so the CPU is told by `hw.optional.arm64`, which XNU sets on Apple silicon.
pub(crate) fn check_icache() -> Result<(), Error> {
    let arm64 = sysctl_int(c"hw.optional.arm64")?;
    if arm64 != 1 {
        return Err(Error::new(format!(
            "hw.optional.arm64 is {arm64}: not Apple silicon, so whether its instruction cache aliases by \
             virtual address is unknown"
        )));
    }
    Ok(())
}

/// A send right to a memory entry this process holds.
pub(crate) struct Object {
    port: Port,
    len: u64,
}

impl Drop for Object {
    fn drop(&mut self) {
        // SAFETY: `port` is a send right this `Object` holds one reference of.
        unsafe { mach_port_deallocate(task(), self.port) };
    }
}

impl Object {
    pub(crate) fn create(name: &str, len: u64) -> Result<Object, Error> {
        let mut size = len;
        let mut port = MACH_PORT_NULL;
        // SAFETY: `size` and `port` are live out-pointers; with no parent, NAMED_CREATE makes a
        // new zero-filled object of `size` bytes.
        let code = unsafe {
            mach_make_memory_entry_64(
                task(),
                &raw mut size,
                0,
                MAP_MEM_NAMED_CREATE | VM_PROT_READ | VM_PROT_WRITE | VM_PROT_EXECUTE,
                &raw mut port,
                MACH_PORT_NULL,
            )
        };
        if code != KERN_SUCCESS {
            return Err(kern(&format!("mach_make_memory_entry_64 of memory object {name:?}, {len:#x} bytes"), code));
        }
        let object = Object { port, len };
        if size < len {
            return Err(Error::new(format!("memory object {name:?} was made {size:#x} bytes, not {len:#x}")));
        }
        Ok(object)
    }

    pub(crate) fn len(&self) -> u64 {
        self.len
    }

    pub(crate) fn duplicate(&self) -> Result<Object, Error> {
        // SAFETY: `port` is a send right this process holds; one more reference to it.
        let code = unsafe { mach_port_mod_refs(task(), self.port, MACH_PORT_RIGHT_SEND, 1) };
        if code != KERN_SUCCESS {
            return Err(kern("duplicating a memory object's send right", code));
        }
        Ok(Object { port: self.port, len: self.len })
    }

    /// The whole object in this process, anywhere; never inherited by a child of it.
    pub(crate) fn map_here(&self, access: Access) -> Result<View, Error> {
        let (protection, copy) = match access {
            Access::Execute => (VM_PROT_READ | VM_PROT_EXECUTE, false),
            Access::Read => (VM_PROT_READ, false),
            Access::Copy => (VM_PROT_READ | VM_PROT_WRITE, true),
            Access::Shared => (VM_PROT_READ | VM_PROT_WRITE, false),
        };
        let at = map_entry(self.port, 0, self.len, None, protection, copy, VM_INHERIT_NONE)?;
        View::adopt(at, self.len, protection & VM_PROT_WRITE != 0)
    }

    /// A new entry over the whole object that carries `protection` and no more.
    fn entry(&self, protection: c_int) -> Result<Port, Error> {
        let mut size = self.len;
        let mut port = MACH_PORT_NULL;
        // SAFETY: `size` and `port` are live out-pointers; `self.port` is this process's send
        // right to the parent entry, which the new entry ranges over from offset 0.
        let code = unsafe { mach_make_memory_entry_64(task(), &raw mut size, 0, protection, &raw mut port, self.port) };
        if code != KERN_SUCCESS {
            return Err(kern(&format!("mach_make_memory_entry_64 of a sub-entry of protection {protection:#x}"), code));
        }
        if size < self.len {
            // SAFETY: `port` is the send right just made, which nothing else refers to.
            unsafe { mach_port_deallocate(task(), port) };
            return Err(Error::new(format!("a sub-entry was made {size:#x} bytes, not {:#x}", self.len)));
        }
        Ok(port)
    }
}

/// Maps `len` bytes from `offset` of the entry `port` anywhere, or over what is at `at`
/// (an overlay, which [`overlay`]'s caller proved may go there).
fn map_entry(
    port: Port,
    offset: u64,
    len: u64,
    at: Option<u64>,
    protection: c_int,
    copy: bool,
    inheritance: u32,
) -> Result<u64, Error> {
    let page = page();
    if !offset.is_multiple_of(page) || len == 0 || at.is_some_and(|it| !it.is_multiple_of(page)) {
        return Err(Error::new(format!("a map of {len:#x} bytes at offset {offset:#x}: not whole pages of {page:#x}")));
    }
    let len = len.next_multiple_of(page);
    let flags = if at.is_some() { VM_FLAGS_FIXED | VM_FLAGS_OVERWRITE } else { VM_FLAGS_ANYWHERE };
    let mut address = at.unwrap_or(0);
    // SAFETY: `port` is this process's send right to an entry; `address` is a live in-out
    // pointer. With no address the kernel picks free space; a fixed one is an overlay, which
    // replaces only pages its caller proved it may (`overlay`).
    let code = unsafe {
        mach_vm_map(task(), &raw mut address, len, 0, flags, port, offset, c_int::from(copy), protection, protection, inheritance)
    };
    if code != KERN_SUCCESS {
        let place = at.map(|it| format!(" to {it:#x}")).unwrap_or_default();
        return Err(kern(&format!("mach_vm_map of {len:#x} bytes at offset {offset:#x}{place}, protection {protection:#x}"), code));
    }
    if let Some(at) = at
        && address != at
    {
        // SAFETY: the mapping just made, of `len` bytes, which nothing uses.
        unsafe { mach_vm_deallocate(task(), address, len) };
        return Err(Error::new(format!("{at:#x} is taken: the kernel mapped {len:#x} bytes at {address:#x} instead")));
    }
    Ok(address)
}

/// A mapping in this process, unmapped when dropped.
pub(crate) struct View {
    at: *mut u8,
    len: usize,
    writable: bool,
}

// SAFETY: a `View` is an address range this process mapped; nothing of it is tied to a thread,
// and every write through it goes through `&mut self` or atomics.
unsafe impl Send for View {}
// SAFETY: as above: shared access is reads of bytes no `&self` method writes, and atomics.
unsafe impl Sync for View {}

impl Drop for View {
    fn drop(&mut self) {
        // SAFETY: `at..at + len` is this view's own mapping, which nothing borrows once it
        // drops (every borrow is of `self`).
        unsafe { mach_vm_deallocate(task(), self.at as u64, self.len as u64) };
    }
}

impl View {
    /// Takes the mapping of `len` bytes at `at` just made, unmapping it on an error.
    fn adopt(at: u64, len: u64, writable: bool) -> Result<View, Error> {
        match usize::try_from(len) {
            Ok(len) => Ok(View { at: at as *mut u8, len, writable }),
            Err(_) => {
                // SAFETY: the mapping just made, which nothing uses.
                unsafe { mach_vm_deallocate(task(), at, len) };
                Err(Error::new(format!("a view of {len:#x} bytes")))
            }
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn address(&self) -> u64 {
        self.at as u64
    }

    pub(crate) fn bytes(&self, range: Range<usize>) -> Result<&[u8], Error> {
        if range.start > range.end || range.end > self.len {
            return Err(Error::new(format!("bytes {range:?} of a view of {:#x}", self.len)));
        }
        // SAFETY: `range` is within the mapping (checked above), which lives as long as `self`;
        // nothing writes it while `&self` is borrowed but atomics on other words and the
        // daemon's `&mut` methods, which can't run meanwhile.
        Ok(unsafe { std::slice::from_raw_parts(self.at.add(range.start), range.end - range.start) })
    }

    pub(crate) fn bytes_mut(&mut self, range: Range<usize>) -> Result<&mut [u8], Error> {
        if !self.writable {
            return Err(Error::new("a write through a read-only view"));
        }
        if range.start > range.end || range.end > self.len {
            return Err(Error::new(format!("bytes {range:?} of a view of {:#x}", self.len)));
        }
        // SAFETY: as for `bytes`, and `&mut self` makes this the only borrow; the mapping is
        // writable (checked above).
        Ok(unsafe { std::slice::from_raw_parts_mut(self.at.add(range.start), range.end - range.start) })
    }

    pub(crate) fn word(&self, at: usize) -> &AtomicU64 {
        assert!(at.is_multiple_of(8) && at + 8 <= self.len, "a word at {at:#x} of a view of {:#x}", self.len);
        // SAFETY: `at` is 8-aligned and in the mapping (asserted), page-aligned itself, so the
        // word is aligned; every access to the word, in this process and others, is atomic.
        unsafe { AtomicU64::from_ptr(self.at.add(at).cast()) }
    }

    pub(crate) fn word32(&self, at: usize) -> &AtomicU32 {
        assert!(at.is_multiple_of(4) && at + 4 <= self.len, "a word at {at:#x} of a view of {:#x}", self.len);
        // SAFETY: as for `word`, 4-aligned.
        unsafe { AtomicU32::from_ptr(self.at.add(at).cast()) }
    }

    pub(crate) fn publish(&self, range: Range<usize>) {
        if range.start >= range.end || range.end > self.len {
            return;
        }
        // SAFETY: `range` is within this view's mapping (checked above); the maintenance
        // changes no value, and, the caches being physically indexed (`check_icache`), holds
        // for every view of the same pages.
        unsafe { sys_icache_invalidate(self.at.add(range.start).cast(), range.end - range.start) };
    }
}

/// Whether this process may run pages with no code signature: it must not enforce code
/// signing on its whole map. Asked once.
fn check_unsigned_code() -> Result<(), Error> {
    static CHECKED: OnceLock<Result<(), Error>> = OnceLock::new();
    CHECKED
        .get_or_init(|| {
            let mut flags: u32 = 0;
            // SAFETY: CS_OPS_STATUS writes this process's 4-byte code-signing status to `flags`,
            // which is live and writable.
            let got = unsafe { csops(libc::getpid(), CS_OPS_STATUS, (&raw mut flags).cast(), size_of::<u32>()) };
            if got != 0 {
                return Err(Error::new(format!("csops CS_OPS_STATUS: {}", std::io::Error::last_os_error())));
            }
            if flags & CS_ENFORCEMENT == 0 {
                return Ok(());
            }
            let runtime = if flags & CS_RUNTIME != 0 { ", the hardened runtime" } else { "" };
            Err(Error::new(format!(
                "this process enforces code signing (code-signing flags {flags:#x}{runtime}), and the code memory's \
                 pages carry no signature: the kernel would kill it at the first one it ran. The dev executable \
                 must be signed ad hoc without the hardened runtime, or with \
                 com.apple.security.cs.allow-unsigned-executable-memory"
            )))
        })
        .clone()
}

/// A send right to an entry the daemon gave this start hook.
pub(crate) struct Granted {
    port: Port,
    len: u64,
}

impl Drop for Granted {
    fn drop(&mut self) {
        // SAFETY: `port` is a send right this `Granted` holds one reference of; mappings made
        // from it stay.
        unsafe { mach_port_deallocate(task(), self.port) };
    }
}

impl Granted {
    pub(crate) fn len(&self) -> u64 {
        self.len
    }

    /// The whole object anywhere, for the rest of the process: the control words. Never code,
    /// which runs only where an overlay puts it.
    pub(crate) fn map_anywhere(&self, _: &mut Channel, access: Access) -> Result<u64, Error> {
        let protection = match access {
            Access::Read => VM_PROT_READ,
            Access::Shared => VM_PROT_READ | VM_PROT_WRITE,
            other => return Err(Error::new(format!("a whole granted object mapped anywhere as {other:?}"))),
        };
        map_entry(self.port, 0, self.len, None, protection, false, VM_INHERIT_SHARE)
    }

    /// `Access::Copy` is a copy of the pages as they are now, in anonymous memory: an entry
    /// that carries no write right can't be mapped writable even privately, and the kernel
    /// copies an object of this kind at once anyway (`MEMORY_OBJECT_COPY_NONE`).
    fn overlay(&self, offset: u64, len: u64, at: u64, access: Access) -> Result<(), Error> {
        let rounded = len.next_multiple_of(page());
        if offset.checked_add(rounded).is_none_or(|end| end > self.len) {
            return Err(Error::new(format!("an overlay of {len:#x} bytes at offset {offset:#x} of an object of {:#x}", self.len)));
        }
        let protection = match access {
            Access::Execute => {
                check_unsigned_code()?;
                VM_PROT_READ | VM_PROT_EXECUTE
            }
            Access::Read => VM_PROT_READ,
            Access::Shared => VM_PROT_READ | VM_PROT_WRITE,
            Access::Copy => return self.copy(offset, rounded, at),
        };
        map_entry(self.port, offset, rounded, Some(at), protection, false, VM_INHERIT_SHARE).map(drop)
    }

    fn copy(&self, offset: u64, len: u64, at: u64) -> Result<(), Error> {
        if !at.is_multiple_of(page()) || len == 0 {
            return Err(Error::new(format!("a copy of {len:#x} bytes to {at:#x}: not whole pages")));
        }
        let source = map_entry(self.port, offset, len, None, VM_PROT_READ, false, VM_INHERIT_NONE)?;
        let source = View::adopt(source, len, false)?;
        let mut address = at;
        // SAFETY: anonymous readable and writable memory over `at..at + len`, which `overlay`'s
        // caller proved is inside a reservation of this executable, holding nothing;
        // `address` is a live in-out pointer.
        let code = unsafe {
            mach_vm_map(
                task(),
                &raw mut address,
                len,
                0,
                VM_FLAGS_FIXED | VM_FLAGS_OVERWRITE,
                MACH_PORT_NULL,
                0,
                0,
                VM_PROT_READ | VM_PROT_WRITE,
                VM_PROT_READ | VM_PROT_WRITE,
                VM_INHERIT_COPY,
            )
        };
        if code != KERN_SUCCESS {
            return Err(kern(&format!("mach_vm_map of {len:#x} bytes of private memory over {at:#x}"), code));
        }
        let bytes = source.bytes(0..source.len())?;
        // SAFETY: `at..at + len` is the writable memory just mapped, which nothing else refers
        // to yet; `bytes` is the read-only view of the same length, a different mapping.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), at as *mut u8, bytes.len()) };
        Ok(())
    }
}

/// A start hook's grant of `object` with `rights`, made in this process as the daemon makes it.
#[cfg(test)]
pub(crate) fn granted(object: &Object, rights: super::Rights) -> Result<Granted, Error> {
    Ok(Granted { port: object.entry(rendezvous::protection(rights))?, len: object.len })
}

pub(crate) fn overlay(_: &mut Channel, objects: &[Granted], overlays: &[Overlay], slide: u64) -> Result<(), Error> {
    for overlay in overlays {
        let object = objects
            .get(overlay.object as usize)
            .ok_or_else(|| Error::new(format!("an overlay of object {} of {}", overlay.object, objects.len())))?;
        object.overlay(overlay.offset, overlay.len, overlay.at.wrapping_add(slide), overlay.access)?;
    }
    Ok(())
}

pub(crate) fn own_image() -> Result<(u64, &'static [u8]), Error> {
    // SAFETY: no preconditions; dyld set it before any initializer runs.
    let header = unsafe { _NSGetMachExecuteHeader() } as u64;
    if header == 0 || !header.is_multiple_of(page()) {
        return Err(Error::new(format!("dyld names the executable's header at {header:#x}")));
    }
    // SAFETY: the Mach header and load commands of the main executable are in its first page
    // and the rest of `__TEXT` from file offset 0, which dyld mapped readable for the life of
    // the process; 32 bytes of header are inside the first page.
    let prefix = unsafe { std::slice::from_raw_parts(header as *const u8, 32) };
    let len = crate::image::headers_len(crate::Format::MachO, prefix)
        .ok_or_else(|| Error::new("the executable's Mach header is not one"))?;
    // `__TEXT` is the segment of file offset 0 the header is in; its length is in the header,
    // which `image::layout` reads next: here only the load commands must stay in a sane bound.
    if len > 1 << 20 {
        return Err(Error::new(format!("the executable's load commands are {len:#x} bytes")));
    }
    // SAFETY: the load commands follow the header inside `__TEXT` (ld64 and blaze's linker put
    // them there, and dyld refuses an executable whose commands are not in its first segment),
    // mapped as above.
    Ok((header, unsafe { std::slice::from_raw_parts(header as *const u8, len as usize) }))
}

pub(crate) fn clone_file(from: &Path, to: &Path) -> Result<bool, Error> {
    let path = |it: &Path| CString::new(it.as_os_str().as_bytes()).map_err(|_| Error::new(format!("{} has a NUL", it.display())));
    let (source, target) = (path(from)?, path(to)?);
    // SAFETY: both are NUL-terminated paths live for the call; clonefile makes `target` new.
    if unsafe { libc::clonefile(source.as_ptr(), target.as_ptr(), 0) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ENOTSUP | libc::EXDEV) => Ok(false),
        _ => Err(Error::new(format!("cloning {} to {}: {error}", from.display(), to.display()))),
    }
}

pub(crate) fn threads(pid: u32, out: &mut Vec<u64>) -> Result<(), Error> {
    out.clear();
    let pid = c_int::try_from(pid).map_err(|_| Error::new(format!("process {pid}")))?;
    let mut capacity = 64usize;
    loop {
        out.resize(capacity, 0);
        let bytes = c_int::try_from(capacity * 8).map_err(|_| Error::new(format!("{capacity} threads in process {pid}")))?;
        // SAFETY: `out` has `capacity` writable u64s, `bytes` bytes, for the IDs.
        let got = unsafe { libc::proc_pidinfo(pid, PROC_PIDLISTTHREADIDS, 0, out.as_mut_ptr().cast(), bytes) };
        if got <= 0 {
            let error = std::io::Error::last_os_error();
            out.clear();
            if got == 0 || error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(());
            }
            return Err(Error::new(format!("proc_pidinfo PROC_PIDLISTTHREADIDS of process {pid}: {error}")));
        }
        let count = got as usize / 8;
        if count < capacity {
            out.truncate(count);
            return Ok(());
        }
        capacity *= 2;
    }
}

pub(crate) fn thread_id() -> u64 {
    let mut id = 0u64;
    // SAFETY: a null thread is the calling one; `id` is a live out-pointer.
    unsafe { libc::pthread_threadid_np(0, &raw mut id) };
    id
}

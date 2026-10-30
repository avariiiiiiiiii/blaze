//! Linux on x86-64 and AArch64: a memory object is a `memfd`, sealed against shrinking and
//! growing, mapped shared and writable in the daemon. The loader is given a read-only reopen
//! of it over `SCM_RIGHTS` (`/proc/self/fd/N` opened `O_RDONLY`), so it can map it to read,
//! execute or copy but never to write it shared; the control object goes as it is.

use std::ffi::{CString, c_void};
use std::ops::Range;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64};

pub(crate) use super::unix::{Listener, Stream, backtrace, connect, protect, random, serialize};
use super::Rights;
use super::unix::protection;
use crate::channel::Channel;
use crate::frame::{self, kinds};
use crate::{Access, Error};

fn os(what: &str) -> Error {
    Error::new(format!("{what}: {}", std::io::Error::last_os_error()))
}

pub(crate) fn page() -> u64 {
    // SAFETY: sysconf has no preconditions.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page > 0 { page as u64 } else { 4096 }
}

#[cfg(target_arch = "aarch64")]
fn ctr() -> u64 {
    let ctr: u64;
    // SAFETY: Linux lets EL0 read CTR_EL0 (SCTLR_EL1.UCT, or it traps and emulates it); the
    // read touches no memory.
    unsafe { std::arch::asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack, preserves_flags)) };
    ctr
}

/// AArch64: the instruction cache must not alias by virtual address, since code is written
/// through the daemon's view and run through another process's. PIPT, or VIPT whose way is
/// no larger than a page, or no maintenance needed at all (`CTR_EL0.DIC`).
pub(crate) fn check_icache() -> Result<(), Error> {
    #[cfg(target_arch = "aarch64")]
    {
        let ctr = ctr();
        let dic = ctr & (1 << 29) != 0;
        let l1ip = (ctr >> 14) & 3;
        if dic || l1ip == 0b11 {
            return Ok(());
        }
        if l1ip == 0b10 {
            let way = icache_way().map_err(|it| {
                Error::new(format!("CTR_EL0 {ctr:#x} reports a VIPT instruction cache and its geometry is unknown: {it}"))
            })?;
            if way <= page() {
                return Ok(());
            }
            return Err(Error::new(format!(
                "CTR_EL0 {ctr:#x} reports a VIPT instruction cache with ways of {way} bytes, larger than a page of {}: \
                 code written through one mapping may stay stale through another",
                page()
            )));
        }
        Err(Error::new(format!("CTR_EL0 {ctr:#x} reports an instruction cache policy {l1ip:#b} with aliases")))
    }
    #[cfg(target_arch = "x86_64")]
    Ok(())
}

/// The bytes of one way of the level-1 instruction cache, from sysfs.
#[cfg(target_arch = "aarch64")]
fn icache_way() -> Result<u64, String> {
    for index in 0..8 {
        let dir = format!("/sys/devices/system/cpu/cpu0/cache/index{index}");
        let read = |name: &str| std::fs::read_to_string(format!("{dir}/{name}")).map(|it| it.trim().to_owned());
        let Ok(kind) = read("type") else { break };
        if kind != "Instruction" || read("level").as_deref() != Ok("1") {
            continue;
        }
        let size = read("size").map_err(|it| it.to_string())?;
        let size: u64 = size
            .strip_suffix('K')
            .map(|it| it.parse::<u64>().map(|it| it << 10))
            .unwrap_or_else(|| size.parse())
            .map_err(|_| format!("{dir}/size is {size}"))?;
        let ways: u64 = read("ways_of_associativity").map_err(|it| it.to_string())?.parse().map_err(|_| format!("{dir}/ways_of_associativity"))?;
        if ways == 0 {
            return Err(format!("{dir} has no ways"));
        }
        return Ok(size / ways);
    }
    Err("no level-1 instruction cache under /sys/devices/system/cpu/cpu0/cache".to_owned())
}

pub(crate) struct Object {
    fd: OwnedFd,
    len: u64,
}

impl Object {
    pub(crate) fn create(name: &str, len: u64) -> Result<Object, Error> {
        let name = CString::new(format!("blaze-inproc:{name}")).map_err(|_| Error::new(format!("a memory object named {name:?}")))?;
        // SAFETY: `name` is a NUL-terminated string live for the call.
        let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            let hint = if error.raw_os_error() == Some(libc::EACCES) { " (vm.memfd_noexec is 2?)" } else { "" };
            return Err(Error::new(format!("memfd_create: {error}{hint}")));
        }
        // SAFETY: `fd` was just made and is owned by nothing else.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let size = libc::off_t::try_from(len).map_err(|_| Error::new(format!("a memory object of {len} bytes")))?;
        // SAFETY: `fd` is an open memfd.
        if unsafe { libc::ftruncate(fd.as_raw_fd(), size) } != 0 {
            return Err(os(&format!("sizing a memory object to {len:#x} bytes")));
        }
        // SAFETY: `fd` is an open memfd made with MFD_ALLOW_SEALING.
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, libc::F_SEAL_SHRINK | libc::F_SEAL_GROW) } != 0 {
            return Err(os("sealing a memory object's size"));
        }
        Ok(Object { fd, len })
    }

    pub(crate) fn len(&self) -> u64 {
        self.len
    }

    pub(crate) fn duplicate(&self) -> Result<Object, Error> {
        let fd = self.fd.try_clone().map_err(|it| Error::new(format!("duplicating a memory object: {it}")))?;
        Ok(Object { fd, len: self.len })
    }

    pub(crate) fn map_here(&self, access: Access) -> Result<View, Error> {
        View::map(self.fd.as_raw_fd(), 0, self.len, None, access)
    }

    /// A descriptor of the object opened read-only: what maps it can't write it shared.
    fn read_only(&self) -> Result<OwnedFd, Error> {
        let path = CString::new(format!("/proc/self/fd/{}", self.fd.as_raw_fd())).map_err(|_| Error::new("a /proc path"))?;
        // SAFETY: `path` is a NUL-terminated string live for the call.
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(os("reopening a memory object read-only through /proc/self/fd"));
        }
        // SAFETY: `fd` was just opened and is owned by nothing else.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

/// A mapping in this process, unmapped when dropped.
pub(crate) struct View {
    at: *mut u8,
    len: usize,
    writable: bool,
    /// Whether dropping it unmaps it: a view of a range inside a reservation goes with it.
    owned: bool,
}

// SAFETY: a `View` is an address range this process mapped; nothing of it is tied to a thread,
// and every write through it goes through `&mut self` or atomics.
unsafe impl Send for View {}
// SAFETY: as above: shared access is reads of bytes no `&self` method writes, and atomics.
unsafe impl Sync for View {}

impl Drop for View {
    fn drop(&mut self) {
        if self.owned && self.len != 0 {
            // SAFETY: `at..at + len` is this view's own mapping, which nothing borrows once it
            // drops (every borrow is of `self`).
            unsafe { libc::munmap(self.at.cast(), self.len) };
        }
    }
}

impl View {
    fn map(fd: RawFd, offset: u64, len: u64, at: Option<u64>, access: Access) -> Result<View, Error> {
        let page = page();
        if !offset.is_multiple_of(page) || len == 0 || at.is_some_and(|it| !it.is_multiple_of(page)) {
            return Err(Error::new(format!("a map of {len:#x} bytes at offset {offset:#x}: not whole pages of {page:#x}")));
        }
        let len_usize = usize::try_from(len).map_err(|_| Error::new(format!("a map of {len:#x} bytes")))?;
        let offset_t = libc::off_t::try_from(offset).map_err(|_| Error::new(format!("a map at offset {offset:#x}")))?;
        let shared = if access == Access::Copy { libc::MAP_PRIVATE } else { libc::MAP_SHARED };
        let (hint, fixed) = match at {
            Some(at) if within_reservation(at, len) => (at, libc::MAP_FIXED),
            Some(at) => (at, libc::MAP_FIXED_NOREPLACE),
            None => (0, 0),
        };
        // SAFETY: `fd` is an open memory object; a fixed address is either inside a
        // reservation of this process's own (`reserve`), which holds nothing yet, or mapped
        // with MAP_FIXED_NOREPLACE, which fails rather than replace anything.
        let mapped = unsafe { libc::mmap(hint as *mut c_void, len_usize, protection(access), shared | fixed, fd, offset_t) };
        if mapped == libc::MAP_FAILED {
            return Err(os(&format!("mmap of {len:#x} bytes at offset {offset:#x}{}", at.map(|it| format!(" to {it:#x}")).unwrap_or_default())));
        }
        if let Some(at) = at
            && mapped as u64 != at
        {
            // SAFETY: `mapped` is the mapping just made, of `len` bytes, which nothing uses.
            unsafe { libc::munmap(mapped, len_usize) };
            return Err(Error::new(format!("{at:#x} is taken: the kernel mapped {len:#x} bytes at {:#x} instead", mapped as u64)));
        }
        let writable = matches!(access, Access::Copy | Access::Shared);
        Ok(View { at: mapped.cast(), len: len_usize, writable, owned: at.is_none() })
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
        #[cfg(target_arch = "aarch64")]
        {
            let ctr = ctr();
            let (idc, dic) = (ctr & (1 << 28) != 0, ctr & (1 << 29) != 0);
            let dline = 4usize << ((ctr >> 16) & 15);
            let iline = 4usize << (ctr & 15);
            let start = self.at as usize + range.start;
            let end = self.at as usize + range.end;
            if !idc {
                let mut at = start & !(dline - 1);
                while at < end {
                    // SAFETY: `at` is in this view's mapping (within `range`, rounded down to a
                    // line that starts in it); `dc cvau` cleans the line and changes no value.
                    unsafe { std::arch::asm!("dc cvau, {}", in(reg) at, options(nostack, preserves_flags)) };
                    at += dline;
                }
            }
            // SAFETY: a barrier: it orders the cleans before what follows; touches no memory.
            unsafe { std::arch::asm!("dsb ish", options(nostack, preserves_flags)) };
            if !dic {
                let mut at = start & !(iline - 1);
                while at < end {
                    // SAFETY: as above; `ic ivau` invalidates instruction cache lines for the
                    // physical memory of `at`, which every view of the object shares.
                    unsafe { std::arch::asm!("ic ivau, {}", in(reg) at, options(nostack, preserves_flags)) };
                    at += iline;
                }
                // SAFETY: a barrier, as above.
                unsafe { std::arch::asm!("dsb ish", options(nostack, preserves_flags)) };
            }
            // SAFETY: a context synchronization of this core only.
            unsafe { std::arch::asm!("isb", options(nostack, preserves_flags)) };
        }
        #[cfg(target_arch = "x86_64")]
        std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
    }
}

/// Address space reserved by `reserve`: a fixed map inside it replaces what is there (nothing
/// yet), one outside must find the space free. Written by the loader alone, before the
/// program runs; the lock is never held across a call.
static RESERVED: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());

fn within_reservation(at: u64, len: u64) -> bool {
    let end = at.saturating_add(len);
    RESERVED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .any(|&(start, until)| at >= start && end <= until)
}

pub(crate) fn reserve(_: &mut Channel, len: u64, at: Option<u64>) -> Result<u64, Error> {
    let len = len.next_multiple_of(page());
    let len_usize = usize::try_from(len).map_err(|_| Error::new(format!("a reservation of {len:#x} bytes")))?;
    let flags = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE;
    let (hint, fixed) = match at {
        Some(at) => (at, libc::MAP_FIXED_NOREPLACE),
        None => (0, 0),
    };
    // SAFETY: an anonymous mapping with no access, at a free place (MAP_FIXED_NOREPLACE fails
    // rather than replace anything).
    let mapped = unsafe { libc::mmap(hint as *mut c_void, len_usize, libc::PROT_NONE, flags | fixed, -1, 0) };
    if mapped == libc::MAP_FAILED {
        return Err(os(&format!("reserving {len:#x} bytes of address space")));
    }
    if let Some(at) = at
        && mapped as u64 != at
    {
        // SAFETY: the mapping just made, which nothing uses.
        unsafe { libc::munmap(mapped, len_usize) };
        return Err(Error::new(format!("{at:#x} is taken: {len:#x} bytes can't be reserved there")));
    }
    RESERVED.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push((mapped as u64, mapped as u64 + len));
    Ok(mapped as u64)
}

/// The daemon's side of a run process: nothing, since the loader maps what it is given.
pub(crate) struct Remote;

impl Remote {
    pub(crate) fn map(&self, object: u32, _: u64, _: u64, _: u64, _: Access) -> Result<u64, Error> {
        Err(Error::new(format!("a loader asked the daemon to map object {object}: on Linux the loader maps it")))
    }
}

pub(crate) fn send_objects(channel: &mut Channel, _pid: u32, grant: &[u8], objects: &[(&Object, Rights)]) -> Result<Remote, Error> {
    let mut fds: Vec<OwnedFd> = Vec::with_capacity(objects.len());
    for (object, rights) in objects {
        fds.push(match rights {
            Rights::Code => object.read_only()?,
            Rights::Control => object.fd.try_clone().map_err(|it| Error::new(format!("duplicating the control object: {it}")))?,
        });
    }
    let mut bytes = Vec::with_capacity(frame::HEADER + grant.len());
    frame::encode(kinds::GRANT, 0, grant, &mut bytes).map_err(|it| Error::new(format!("the grant: {it}")))?;
    let raw: Vec<RawFd> = fds.iter().map(AsRawFd::as_raw_fd).collect();
    if raw.len() > super::unix::MOST_FDS {
        return Err(Error::new(format!(
            "{} objects to grant, past the {} one message carries",
            raw.len(),
            super::unix::MOST_FDS
        )));
    }
    channel.stream().send(&bytes, &raw)?;
    Ok(Remote)
}

pub(crate) struct Granted {
    fd: OwnedFd,
    len: u64,
}

pub(crate) fn recv_objects(channel: &mut Channel, count: usize) -> Result<Vec<Granted>, Error> {
    let mut out = Vec::with_capacity(count);
    for index in 0..count {
        let fd = channel
            .stream()
            .take_fd()
            .ok_or_else(|| Error::new(format!("the grant came with {index} descriptors of {count}")))?;
        // SAFETY: a zeroed `stat` is a valid one: plain integers.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: `fd` is open; `stat` is live and writable.
        if unsafe { libc::fstat(fd.as_raw_fd(), &raw mut stat) } != 0 {
            return Err(os(&format!("granted object {index}")));
        }
        out.push(Granted { fd, len: stat.st_size as u64 });
    }
    if channel.stream().take_fd().is_some() {
        return Err(Error::new(format!("the grant came with more than {count} descriptors")));
    }
    Ok(out)
}

impl Granted {
    pub(crate) fn len(&self) -> u64 {
        self.len
    }

    pub(crate) fn map(&self, _: &mut Channel, offset: u64, len: u64, at: u64, access: Access) -> Result<u64, Error> {
        if offset.checked_add(len).is_none_or(|end| end > self.len) {
            return Err(Error::new(format!("a map of {len:#x} bytes at offset {offset:#x} of an object of {:#x}", self.len)));
        }
        let view = View::map(self.fd.as_raw_fd(), offset, len, Some(at), access)?;
        let address = view.address();
        std::mem::forget(view);
        Ok(address)
    }

    pub(crate) fn view(&self, _: &mut Channel) -> Result<View, Error> {
        View::map(self.fd.as_raw_fd(), 0, self.len, None, Access::Read)
    }
}

pub(crate) fn threads(pid: u32, out: &mut Vec<u64>) -> Result<(), Error> {
    out.clear();
    let dir = format!("/proc/{pid}/task");
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(Error::new(format!("{dir}: {error}"))),
    };
    for entry in entries {
        let entry = entry.map_err(|it| Error::new(format!("{dir}: {it}")))?;
        if let Some(tid) = entry.file_name().to_str().and_then(|it| it.parse::<u64>().ok()) {
            out.push(tid);
        }
    }
    Ok(())
}

pub(crate) fn thread_id() -> u64 {
    // SAFETY: gettid has no preconditions.
    unsafe { libc::syscall(libc::SYS_gettid) as u64 }
}

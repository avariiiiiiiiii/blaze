//! Windows on x86-64. A memory object is a section backed by the paging file, mapped read-write
//! in the daemon. The run process holds no handle to it: the daemon maps each view into the run
//! process itself (`MapViewOfFile3` on its process handle) when the loader asks with a MAP frame
//! ([`Remote::map`]), so what a run process can map, and how, is what the daemon allows.
//! Address space the loader wants at a known place is reserved as a placeholder
//! (`MEM_RESERVE_PLACEHOLDER`), cut to the view's exact size and replaced by the view
//! (`MEM_REPLACE_PLACEHOLDER`), so nothing else can take it between the two. Views are placed on
//! the 64 KiB allocation granularity, which is [`page`]. The channel is a named pipe
//! ([`pipe`]).

mod pipe;
#[cfg(test)]
mod tests;

use std::ffi::c_void;
use std::ops::Range;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::atomic::{AtomicU32, AtomicU64};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use windows_sys::Win32::Foundation::{ERROR_NO_MORE_FILES, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::Cryptography::ProcessPrng;
use windows_sys::Win32::System::Diagnostics::Debug::{
    CONTEXT, FlushInstructionCache, RtlCaptureContext, RtlLookupFunctionEntry, RtlVirtualUnwind,
    UNW_FLAG_NHANDLER,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MEM_FREE, MEM_PRESERVE_PLACEHOLDER, MEM_RELEASE, MEM_REPLACE_PLACEHOLDER, MEM_RESERVE,
    MEM_RESERVE_PLACEHOLDER, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile3, PAGE_EXECUTE_READ,
    PAGE_EXECUTE_READWRITE, PAGE_NOACCESS, PAGE_READONLY, PAGE_READWRITE, PAGE_WRITECOPY, SEC_COMMIT,
    UnmapViewOfFile, VirtualAlloc2, VirtualFree, VirtualProtect, VirtualQueryEx,
};
use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThreadId, OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_OPERATION,
};

pub(crate) use pipe::{Listener, Stream, connect};

use super::Rights;
use crate::channel::Channel;
use crate::frame::{FrameError, Reader, Writer, kinds};
use crate::{Access, Error};

/// The sequence number of the frame after the GRANT that lists the objects granted: their
/// lengths and rights. Nothing else crosses: the daemon keeps the handles.
const OBJECTS_SEQ: u32 = 1;

/// What failed, with the error Windows gave. Call it right after the failing call.
fn os(what: impl std::fmt::Display) -> Error {
    Error::new(format!("Windows: {what}: {}", std::io::Error::last_os_error()))
}

pub(crate) fn page() -> u64 {
    static PAGE: OnceLock<u64> = OnceLock::new();
    *PAGE.get_or_init(|| {
        let mut info = SYSTEM_INFO::default();
        // SAFETY: GetSystemInfo only writes the struct it is given, which outlives the call.
        unsafe { GetSystemInfo(&mut info) };
        u64::from(info.dwAllocationGranularity)
    })
}

/// x86-64 keeps instruction fetch coherent with stores from any processor: a thread that
/// serializes after a store runs what was stored.
pub(crate) fn check_icache() -> Result<(), Error> {
    Ok(())
}

pub(crate) fn random(out: &mut [u8; 16]) -> Result<(), Error> {
    // SAFETY: ProcessPrng writes exactly `out.len()` bytes into `out`.
    if unsafe { ProcessPrng(out.as_mut_ptr(), out.len()) } == 0 {
        return Err(os("ProcessPrng"));
    }
    Ok(())
}

fn protection(access: Access) -> u32 {
    match access {
        Access::Execute => PAGE_EXECUTE_READ,
        Access::Read => PAGE_READONLY,
        Access::Copy => PAGE_WRITECOPY,
        Access::Shared => PAGE_READWRITE,
    }
}

fn rights_tag(rights: Rights) -> u8 {
    match rights {
        Rights::Code => 1,
        Rights::Control => 2,
    }
}

/// Whether a run process may map an object of `rights` with `access`: code never shared
/// writable, the control words never executable.
fn allowed(rights: Rights, access: Access) -> bool {
    match rights {
        Rights::Code => !matches!(access, Access::Shared),
        Rights::Control => matches!(access, Access::Shared | Access::Read),
    }
}

/// Views start, end and are placed on whole granules.
fn granular(offset: u64, len: u64, at: u64) -> Result<(), Error> {
    let page = page();
    if len == 0 || !offset.is_multiple_of(page) || !len.is_multiple_of(page) || !at.is_multiple_of(page) {
        return Err(Error::new(format!(
            "a view of {len:#x} bytes from offset {offset:#x} at {at:#x}: Windows maps views of whole \
             {page:#x}-byte granules"
        )));
    }
    Ok(())
}

/// Maps `len` bytes of `section` from `offset` into `process` at `at` (0: anywhere), as `kind`
/// says (0 into free address space, or `MEM_REPLACE_PLACEHOLDER`).
fn map_view(
    section: HANDLE,
    process: HANDLE,
    offset: u64,
    len: u64,
    at: u64,
    kind: u32,
    access: Access,
) -> Result<u64, Error> {
    let size = usize::try_from(len).map_err(|_| Error::new(format!("a view of {len:#x} bytes")))?;
    // SAFETY: `section` and `process` are handles the caller holds. The view goes where the
    // system finds room (`at` 0), into free address space at `at` (failing where any of it is
    // taken), or exactly over a placeholder (failing over anything else): it never lands on
    // memory the process uses.
    let view = unsafe {
        MapViewOfFile3(
            section,
            process,
            at as *const c_void,
            offset,
            size,
            kind,
            protection(access),
            std::ptr::null_mut(),
            0,
        )
    };
    if view.Value.is_null() {
        return Err(os(format_args!("MapViewOfFile3 of {len:#x} bytes from offset {offset:#x} at {at:#x} as {access:?}")));
    }
    Ok(view.Value as u64)
}

/// A pagefile-backed section, zero when made. Its pages are committed with it (`SEC_COMMIT`:
/// charged against the commit limit, given memory only once touched), so every view of it is
/// readable everywhere with no commit step in between.
pub(crate) struct Object {
    section: OwnedHandle,
    len: u64,
}

impl Object {
    pub(crate) fn create(name: &str, len: u64) -> Result<Object, Error> {
        // SAFETY: a section backed by the paging file (`INVALID_HANDLE_VALUE`), unnamed, with the
        // default security; every argument is a value or null.
        let section = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_EXECUTE_READWRITE | SEC_COMMIT,
                (len >> 32) as u32,
                len as u32,
                std::ptr::null(),
            )
        };
        if section.is_null() {
            return Err(os(format_args!("CreateFileMappingW of {len:#x} bytes for {name}")));
        }
        // SAFETY: the handle was just made and nothing else owns it.
        Ok(Object { section: unsafe { OwnedHandle::from_raw_handle(section) }, len })
    }

    pub(crate) fn len(&self) -> u64 {
        self.len
    }

    pub(crate) fn duplicate(&self) -> Result<Object, Error> {
        let section =
            self.section.try_clone().map_err(|it| Error::new(format!("Windows: DuplicateHandle of a section: {it}")))?;
        Ok(Object { section, len: self.len })
    }

    pub(crate) fn map_here(&self, access: Access) -> Result<View, Error> {
        let len = usize::try_from(self.len).map_err(|_| Error::new(format!("a view of {:#x} bytes", self.len)))?;
        // SAFETY: GetCurrentProcess returns a pseudo-handle, valid always.
        let at = map_view(self.section.as_raw_handle(), unsafe { GetCurrentProcess() }, 0, self.len, 0, 0, access)?;
        Ok(View { at, len, writable: matches!(access, Access::Shared | Access::Copy) })
    }
}

/// A view in this process, unmapped when dropped.
pub(crate) struct View {
    at: u64,
    len: usize,
    writable: bool,
}

impl View {
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn address(&self) -> u64 {
        self.at
    }

    fn check(&self, range: &Range<usize>) -> Result<(), Error> {
        if range.start > range.end || range.end > self.len {
            return Err(Error::new(format!("bytes {:#x}..{:#x} of a view of {:#x}", range.start, range.end, self.len)));
        }
        Ok(())
    }

    pub(crate) fn bytes(&self, range: Range<usize>) -> Result<&[u8], Error> {
        self.check(&range)?;
        // SAFETY: the range is inside the view (checked), which stays mapped and readable while
        // `self` lives. Nothing writes the bytes a caller reads while it reads them: the daemon
        // writes a range of code before any process can reach it (`CodeMemory::write`), and a
        // run process maps code objects only to read, execute or copy (`allowed`).
        Ok(unsafe { std::slice::from_raw_parts((self.at as usize + range.start) as *const u8, range.len()) })
    }

    pub(crate) fn bytes_mut(&mut self, range: Range<usize>) -> Result<&mut [u8], Error> {
        if !self.writable {
            return Err(Error::new("writing a view mapped read-only"));
        }
        self.check(&range)?;
        // SAFETY: as in `bytes`, and the view is writable (checked); `&mut self` keeps every
        // other slice of this view from living meanwhile.
        Ok(unsafe { std::slice::from_raw_parts_mut((self.at as usize + range.start) as *mut u8, range.len()) })
    }

    pub(crate) fn word(&self, at: usize) -> &AtomicU64 {
        assert!(
            at.is_multiple_of(8) && at.checked_add(8).is_some_and(|end| end <= self.len),
            "a word at {at:#x} of a view of {:#x}",
            self.len
        );
        // SAFETY: in bounds and 8-aligned (asserted; the view starts on a granule), in a mapping
        // that lives as long as `self`. Every access to these words, here and in other
        // processes, is atomic.
        unsafe { &*((self.at as usize + at) as *const AtomicU64) }
    }

    pub(crate) fn word32(&self, at: usize) -> &AtomicU32 {
        assert!(
            at.is_multiple_of(4) && at.checked_add(4).is_some_and(|end| end <= self.len),
            "a word at {at:#x} of a view of {:#x}",
            self.len
        );
        // SAFETY: as in `word`, for 4 bytes.
        unsafe { &*((self.at as usize + at) as *const AtomicU32) }
    }

    pub(crate) fn publish(&self, range: Range<usize>) {
        if self.check(&range).is_err() || range.is_empty() {
            return;
        }
        // SAFETY: a range of this view; FlushInstructionCache reads nothing of it.
        unsafe { FlushInstructionCache(GetCurrentProcess(), (self.at as usize + range.start) as *const c_void, range.len()) };
    }
}

impl Drop for View {
    fn drop(&mut self) {
        // SAFETY: `at` is the start of a view this `View` alone stands for; nothing refers into
        // it once it drops (every slice borrows `self`).
        unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.at as *mut c_void }) };
    }
}

/// What the daemon keeps to map views into a run process: the process, and the objects it was
/// granted with their rights.
pub(crate) struct Remote {
    process: OwnedHandle,
    objects: Vec<(Object, Rights)>,
}

pub(crate) fn send_objects(
    channel: &mut Channel,
    pid: u32,
    grant: &[u8],
    objects: &[(&Object, Rights)],
) -> Result<Remote, Error> {
    // SAFETY: value-only arguments; the handle it returns is checked and owned below.
    let process = unsafe { OpenProcess(PROCESS_VM_OPERATION | PROCESS_QUERY_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(os(format_args!("OpenProcess of the run process {pid}")));
    }
    // SAFETY: just opened, owned by nothing else.
    let process = unsafe { OwnedHandle::from_raw_handle(process) };
    let mut kept = Vec::with_capacity(objects.len());
    for (object, rights) in objects {
        kept.push((object.duplicate()?, *rights));
    }
    channel.send(kinds::GRANT, 0, grant)?;
    let mut body = Vec::with_capacity(4 + 9 * objects.len());
    let mut out = Writer::new(&mut body);
    out.u32(objects.len() as u32);
    for (object, rights) in objects {
        out.u64(object.len()).u8(rights_tag(*rights));
    }
    channel.send(kinds::GRANT, OBJECTS_SEQ, &body)?;
    Ok(Remote { process, objects: kept })
}

impl Remote {
    /// Maps `len` bytes of object `object` from `offset` into the run process at `at` (0:
    /// anywhere), if its rights allow `access`: into free address space, or replacing the
    /// placeholder that is exactly there.
    pub(crate) fn map(&self, object: u32, offset: u64, len: u64, at: u64, access: Access) -> Result<u64, Error> {
        let Some((granted, rights)) = self.objects.get(object as usize) else {
            return Err(Error::new(format!("a map of object {object} of {}", self.objects.len())));
        };
        if !allowed(*rights, access) {
            return Err(Error::new(format!("object {object} ({rights:?}) may not be mapped {access:?}")));
        }
        granular(offset, len, at)?;
        if offset.checked_add(len).is_none_or(|end| end > granted.len) {
            return Err(Error::new(format!(
                "{len:#x} bytes from offset {offset:#x} of object {object}, which is {:#x}",
                granted.len
            )));
        }
        let kind = match at {
            0 => 0,
            _ => match self.state(at)? {
                MEM_FREE => 0,
                MEM_RESERVE => MEM_REPLACE_PLACEHOLDER,
                _ => return Err(Error::new(format!("{at:#x} in the run process is taken"))),
            },
        };
        let mapped = map_view(granted.section.as_raw_handle(), self.process.as_raw_handle(), offset, len, at, kind, access)?;
        if at != 0 && mapped != at {
            return Err(Error::new(format!("a view asked at {at:#x} came at {mapped:#x}")));
        }
        Ok(mapped)
    }

    fn state(&self, at: u64) -> Result<u32, Error> {
        let mut info = MEMORY_BASIC_INFORMATION::default();
        // SAFETY: writes at most `size_of::<MEMORY_BASIC_INFORMATION>()` bytes into `info`.
        let got = unsafe {
            VirtualQueryEx(
                self.process.as_raw_handle(),
                at as *const c_void,
                &mut info,
                size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        if got == 0 {
            return Err(os(format_args!("VirtualQueryEx at {at:#x} in the run process")));
        }
        Ok(info.State)
    }
}

/// An object the loader was granted, by its index in the grant: the daemon maps it.
pub(crate) struct Granted {
    index: u32,
    len: u64,
    rights: Rights,
}

pub(crate) fn recv_objects(channel: &mut Channel, count: usize) -> Result<Vec<Granted>, Error> {
    let bad = |it: FrameError| Error::new(format!("the daemon's list of objects: {it}"));
    let frame = channel.recv()?;
    if frame.seq != OBJECTS_SEQ {
        return Err(Error::new(format!("the daemon's list of objects came as frame {}", frame.seq)));
    }
    let mut read = frame.reader(kinds::GRANT).map_err(bad)?;
    let listed = read.count(9).map_err(bad)?;
    if listed != count {
        return Err(Error::new(format!("the daemon listed {listed} objects, where the grant names {count}")));
    }
    let mut out = Vec::with_capacity(listed);
    for index in 0..listed {
        let len = read.u64().map_err(bad)?;
        let rights = match read.u8().map_err(bad)? {
            1 => Rights::Code,
            2 => Rights::Control,
            other => return Err(Error::new(format!("object {index} granted with rights {other}"))),
        };
        out.push(Granted { index: index as u32, len, rights });
    }
    read.finish().map_err(bad)?;
    Ok(out)
}

impl Granted {
    pub(crate) fn len(&self) -> u64 {
        self.len
    }

    pub(crate) fn map(&self, channel: &mut Channel, offset: u64, len: u64, at: u64, access: Access) -> Result<u64, Error> {
        if at == 0 {
            return Err(Error::new(format!("a map of object {} at no address", self.index)));
        }
        if !allowed(self.rights, access) {
            return Err(Error::new(format!("object {} ({:?}) mapped {access:?}", self.index, self.rights)));
        }
        granular(offset, len, at)?;
        let carved = carve(at, len)?;
        let mapped = self.ask(channel, offset, len, at, access);
        if mapped.is_err() && carved {
            placeholders().push((at, len));
        }
        let mapped = mapped?;
        if mapped != at {
            return Err(Error::new(format!("object {} was mapped at {mapped:#x}, asked at {at:#x}", self.index)));
        }
        Ok(mapped)
    }

    pub(crate) fn view(&self, channel: &mut Channel) -> Result<View, Error> {
        let len = usize::try_from(self.len).map_err(|_| Error::new(format!("a view of {:#x} bytes", self.len)))?;
        let at = self.ask(channel, 0, self.len, 0, Access::Read)?;
        Ok(View { at, len, writable: false })
    }

    fn ask(&self, channel: &mut Channel, offset: u64, len: u64, at: u64, access: Access) -> Result<u64, Error> {
        let bad = |it: FrameError| Error::new(format!("the daemon's answer to a map: {it}"));
        let mut body = Vec::with_capacity(29);
        Writer::new(&mut body).u32(self.index).u64(offset).u64(len).u64(at).u8(access.tag());
        let frame = channel.request(kinds::MAP, &body)?;
        match frame.kind {
            kinds::MAPPED => {
                let mut read = Reader::new(frame.body);
                let address = read.u64().map_err(bad)?;
                read.finish().map_err(bad)?;
                Ok(address)
            }
            kinds::REFUSED => Err(Error::new(format!(
                "the daemon would not map {len:#x} bytes of object {} at {at:#x}: {}",
                self.index,
                Reader::new(frame.body).str().unwrap_or("(not UTF-8)")
            ))),
            other => Err(Error::new(format!("the daemon answered a map with a frame of kind {:#06x}", other.0))),
        }
    }
}

/// The placeholders [`reserve`] made in this process and no view replaced yet: start and
/// length. Taken only while the loader maps, before the program runs.
static PLACEHOLDERS: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());

fn placeholders() -> MutexGuard<'static, Vec<(u64, u64)>> {
    PLACEHOLDERS.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) fn reserve(channel: &mut Channel, len: u64, at: Option<u64>) -> Result<u64, Error> {
    let _ = channel;
    let page = page();
    let len = len
        .checked_next_multiple_of(page)
        .filter(|it| *it != 0)
        .ok_or_else(|| Error::new(format!("a reservation of {len:#x} bytes")))?;
    let at = at.unwrap_or(0);
    if !at.is_multiple_of(page) {
        return Err(Error::new(format!("a reservation at {at:#x}, not on a granule of {page:#x}")));
    }
    let size = usize::try_from(len).map_err(|_| Error::new(format!("a reservation of {len:#x} bytes")))?;
    // SAFETY: reserves address space only, as a placeholder with no access, at `at` or where the
    // system finds room; it fails where any of `at..at + len` is taken, so it replaces nothing.
    let got = unsafe {
        VirtualAlloc2(
            GetCurrentProcess(),
            at as *const c_void,
            size,
            MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
            PAGE_NOACCESS,
            std::ptr::null_mut(),
            0,
        )
    };
    if got.is_null() {
        return Err(os(format_args!("VirtualAlloc2 of a placeholder of {len:#x} bytes at {at:#x}")));
    }
    placeholders().push((got as u64, len));
    Ok(got as u64)
}

/// Which of `list` holds all of `at..at + len`: `None` when none touches it (free address
/// space), an error when one holds only part of it.
fn find(list: &[(u64, u64)], at: u64, len: u64) -> Result<Option<usize>, Error> {
    let end = at.checked_add(len).ok_or_else(|| Error::new(format!("{len:#x} bytes at {at:#x}")))?;
    if let Some(index) = list.iter().position(|&(start, size)| start <= at && end <= start + size) {
        return Ok(Some(index));
    }
    match list.iter().find(|&&(start, size)| start < end && at < start + size) {
        Some(&(start, size)) => Err(Error::new(format!(
            "{at:#x}..{end:#x} runs past the reservation {start:#x}..{:#x}",
            start + size
        ))),
        None => Ok(None),
    }
}

/// The bytes to split off the front and the back of `placeholder` so that `at..at + len` is
/// one placeholder of its own.
fn cut(placeholder: (u64, u64), at: u64, len: u64) -> (u64, u64) {
    let (start, size) = placeholder;
    (at - start, start + size - (at + len))
}

/// Makes `at..at + len` one placeholder when it is inside one: whether it is.
fn carve(at: u64, len: u64) -> Result<bool, Error> {
    let mut list = placeholders();
    let Some(index) = find(&list, at, len)? else { return Ok(false) };
    let (start, size) = list.swap_remove(index);
    let (front, back) = cut((start, size), at, len);
    if front != 0 {
        if let Err(error) = split(start, front) {
            list.push((start, size));
            return Err(error);
        }
        list.push((start, front));
    }
    if back != 0 {
        if let Err(error) = split(at, len) {
            list.push((at, len + back));
            return Err(error);
        }
        list.push((at + len, back));
    }
    Ok(true)
}

fn split(start: u64, first: u64) -> Result<(), Error> {
    // SAFETY: `start..start + first` is the front of a placeholder `reserve` made, which no view
    // replaced (it is in `PLACEHOLDERS`): splitting it changes no memory anything uses.
    if unsafe { VirtualFree(start as *mut c_void, first as usize, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER) } == 0 {
        return Err(os(format_args!("VirtualFree splitting {first:#x} bytes off the placeholder at {start:#x}")));
    }
    Ok(())
}

pub(crate) fn protect(at: u64, len: u64, access: Access) -> Result<(), Error> {
    let protection = match access {
        Access::Read => PAGE_READONLY,
        Access::Copy => PAGE_WRITECOPY,
        Access::Execute | Access::Shared => {
            return Err(Error::new(format!(
                "protecting {len:#x} bytes at {at:#x} as {access:?}: only a private copy's pages change, \
                 and never to code or shared"
            )));
        }
    };
    let mut old = 0;
    // SAFETY: changes the protection of pages of a private copy the loader mapped (the caller's
    // range); the loader writes them through raw pointers only, so no reference into them can be
    // invalidated.
    if unsafe { VirtualProtect(at as *const c_void, len as usize, protection, &mut old) } == 0 {
        return Err(os(format_args!("VirtualProtect of {len:#x} bytes at {at:#x} to {access:?}")));
    }
    Ok(())
}

/// A walk of every thread of the system: Windows lists threads by process no other way. A
/// thread missing from the list would have its fence slot reaped, so a walk that fails midway
/// is an error, never a shorter list.
pub(crate) fn threads(pid: u32, out: &mut Vec<u64>) -> Result<(), Error> {
    out.clear();
    // SAFETY: value-only arguments; the handle it returns is checked and owned below.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(os("CreateToolhelp32Snapshot of the system's threads"));
    }
    // SAFETY: just made, owned by nothing else.
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
    let size = size_of::<THREADENTRY32>() as u32;
    let mut entry = THREADENTRY32 { dwSize: size, ..Default::default() };
    // SAFETY: `entry` is a THREADENTRY32 whose `dwSize` says so; the call writes only it.
    if unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) } == 0 {
        return Err(os("Thread32First"));
    }
    loop {
        if entry.th32OwnerProcessID == pid {
            out.push(u64::from(entry.th32ThreadID));
        }
        entry.dwSize = size;
        // SAFETY: as above.
        if unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) } == 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                return Ok(());
            }
            return Err(Error::new(format!("Windows: Thread32Next: {error}")));
        }
    }
}

pub(crate) fn thread_id() -> u64 {
    // SAFETY: no arguments; reads the calling thread's ID.
    u64::from(unsafe { GetCurrentThreadId() })
}

pub(crate) fn serialize() {
    // SAFETY: `cpuid` touches no memory and writes only eax, ebx, ecx and edx; rbx, which LLVM
    // reserves, is kept in another register around it.
    unsafe {
        std::arch::asm!(
            "mov {saved}, rbx",
            "cpuid",
            "mov rbx, {saved}",
            saved = out(reg) _,
            inout("eax") 0u32 => _,
            inout("ecx") 0u32 => _,
            out("edx") _,
            options(nostack, preserves_flags),
        );
    }
}

/// `RtlCaptureContext` stores the vector registers with aligned stores; windows-sys's `CONTEXT`
/// is aligned to 8 only.
#[repr(C, align(16))]
struct Context(CONTEXT);

/// The return addresses of the calling thread's frames, unwound one by one with the tables the
/// system keeps (the loaded images' and those added with `RtlAddFunctionTable`). A frame
/// below the first with no unwind information can't be stepped over, since only a leaf may
/// lack it, so the walk did not finish: `None`. The outermost frame returns to 0.
#[inline(never)]
pub(crate) fn backtrace(out: &mut [u64]) -> Option<usize> {
    // SAFETY: CONTEXT is plain integers: all zero is a value.
    let mut context = Context(unsafe { std::mem::zeroed() });
    // SAFETY: fills the 16-byte aligned CONTEXT it is given with this thread's registers.
    unsafe { RtlCaptureContext(&mut context.0) };
    let mut len = 0;
    loop {
        let pc = context.0.Rip;
        let mut image = 0u64;
        // SAFETY: looks `pc` up in the function tables the system keeps; writes only `image`.
        let function = unsafe { RtlLookupFunctionEntry(pc, &mut image, std::ptr::null_mut()) };
        if function.is_null() {
            return None;
        }
        let mut data = std::ptr::null_mut();
        let mut frame = 0u64;
        // SAFETY: `function` is the entry the system has for `pc`, in a table that stays while
        // its code is mapped; unwinding reads this thread's own stack, which is mapped, and
        // writes only `context`, `data` and `frame`.
        let _ = unsafe {
            RtlVirtualUnwind(
                UNW_FLAG_NHANDLER,
                image,
                pc,
                function,
                &mut context.0,
                &mut data,
                &mut frame,
                std::ptr::null_mut(),
            )
        };
        if context.0.Rip == 0 {
            return Some(len);
        }
        *out.get_mut(len)? = context.0.Rip;
        len += 1;
    }
}

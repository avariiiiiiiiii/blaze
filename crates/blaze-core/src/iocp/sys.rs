//! What every part of the driver shares of Windows: wide strings, owned handles, the request
//! an `OVERLAPPED` is the head of, and the completion port.

use std::{
    ffi::OsStr,
    io,
    os::windows::{
        ffi::OsStrExt as _,
        io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle},
    },
    path::Path,
    time::Duration,
};

use windows_sys::Win32::{
    Foundation::{GetLastError, HANDLE, INVALID_HANDLE_VALUE, WAIT_TIMEOUT},
    System::IO::{
        CreateIoCompletionPort, GetQueuedCompletionStatusEx, OVERLAPPED, OVERLAPPED_ENTRY,
        PostQueuedCompletionStatus,
    },
};

/// A request's own completion: its `lpOverlapped` is a [`Slot`] of the driver's.
pub(super) const KEY_IO: usize = 0;
/// Work done elsewhere sent what it came to through the channel.
pub(super) const KEY_WAKE: usize = 1;
/// A spawn's process ended: its `lpOverlapped` carries the spawn's serial, not an address.
pub(super) const KEY_EXIT: usize = 2;
/// Keys from here are a job's, the spawn's serial added: its `lpOverlapped` is a process id
/// and its byte count the message (`JOB_OBJECT_MSG_*`), neither of them an address.
pub(super) const KEY_JOB: usize = 16;

/// `s` as a NUL-terminated wide string, or an error where it holds a NUL of its own.
pub(super) fn wide(s: &OsStr) -> io::Result<Vec<u16>> {
    let mut wide: Vec<u16> = s.encode_wide().collect();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{}: a NUL inside a name", s.to_string_lossy()),
        ));
    }
    wide.push(0);
    Ok(wide)
}

/// `path` made absolute, `\`-separated and verbatim (`\\?\`), so no length limit or
/// reinterpretation of its names applies, NUL-terminated.
pub(super) fn path(path: &Path) -> io::Result<Vec<u16>> {
    let absolute = std::path::absolute(path)?;
    let text = absolute.as_os_str().to_string_lossy();
    if absolute.as_os_str().to_str().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{text}: a path that is not Unicode"),
        ));
    }
    let text = text.replace('/', "\\");
    let verbatim = if text.starts_with(r"\\?\") {
        text
    } else if let Some(unc) = text.strip_prefix(r"\\") {
        format!(r"\\?\UNC\{unc}")
    } else {
        format!(r"\\?\{text}")
    };
    wide(OsStr::new(&verbatim))
}

/// The handle a call returned, or the error it set where it returned none.
pub(super) fn owned(raw: HANDLE) -> io::Result<OwnedHandle> {
    if raw.is_null() || raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a handle the call just made, owned by no one else.
    Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
}

/// Ok where a call that returns a `BOOL` returned true.
pub(super) fn check(ok: windows_sys::core::BOOL) -> io::Result<()> {
    match ok {
        0 => Err(io::Error::last_os_error()),
        _ => Ok(()),
    }
}

/// The error code a call just set.
pub(super) fn last() -> u32 {
    // SAFETY: no arguments.
    unsafe { GetLastError() }
}

/// What a request is to the kernel: an `OVERLAPPED`, which a completion hands back, and the
/// driver's id of the request after it, read back through it. Each is boxed, so it stays where
/// it is while the request it heads moves.
#[repr(C)]
pub(super) struct Slot {
    pub(super) overlapped: OVERLAPPED,
    pub(super) id: u64,
}

impl Slot {
    pub(super) fn new(id: u64) -> Box<Slot> {
        // SAFETY: all zeros is an `OVERLAPPED` of no offset and no event.
        Box::new(Slot { overlapped: unsafe { std::mem::zeroed() }, id })
    }

    /// Made ready for the next request of its own, at `at` of a file (0 where none applies).
    pub(super) fn reset(&mut self, at: u64) -> *mut OVERLAPPED {
        // SAFETY: as in `new`.
        self.overlapped = unsafe { std::mem::zeroed() };
        self.overlapped.Anonymous.Anonymous.Offset = at as u32;
        self.overlapped.Anonymous.Anonymous.OffsetHigh = (at >> 32) as u32;
        &raw mut self.overlapped
    }

    /// How its request ended, once its completion is dequeued: the bytes it moved, or the
    /// Win32 error its status (an `NTSTATUS`, in `Internal`) says.
    pub(super) fn result(&self) -> Result<u32, u32> {
        let status = self.overlapped.Internal as i32;
        match status < 0 {
            // SAFETY: no pointers.
            true => Err(unsafe { windows_sys::Win32::Foundation::RtlNtStatusToDosError(status) }),
            false => Ok(self.overlapped.InternalHigh as u32),
        }
    }
}

/// The completion port: every request's completion, the jobs' messages and the wakes of work
/// done elsewhere come to it.
pub(super) struct Port(OwnedHandle);

impl Port {
    pub(super) fn new() -> io::Result<Port> {
        // SAFETY: no handle to associate; a new port, for one thread at a time.
        let port = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 1) };
        Ok(Port(owned(port)?))
    }

    pub(super) fn raw(&self) -> HANDLE {
        self.0.as_raw_handle()
    }

    /// Completions of overlapped requests of `handle` come to this port, with [`KEY_IO`].
    pub(super) fn associate(&self, handle: HANDLE) -> io::Result<()> {
        // SAFETY: an open handle of the caller's and this port.
        let port = unsafe { CreateIoCompletionPort(handle, self.raw(), KEY_IO, 0) };
        match port.is_null() {
            true => Err(io::Error::last_os_error()),
            false => Ok(()),
        }
    }

    /// Posts a packet of `key`, whose `carried` is a number of the key's, never an address.
    pub(super) fn post(&self, key: usize, bytes: u32, carried: usize) -> io::Result<()> {
        // SAFETY: the port is open; `carried` is handed back as it is and never read through.
        check(unsafe {
            PostQueuedCompletionStatus(self.raw(), bytes, key, carried as *const OVERLAPPED)
        })
    }

    /// Up to `entries.len()` packets, waiting up to `wait` for the first; how many came.
    pub(super) fn take(&self, entries: &mut [OVERLAPPED_ENTRY], wait: Duration) -> io::Result<usize> {
        let mut got = 0;
        // Rounded up, so a wait shorter than a millisecond is not a spin.
        let ms = match wait.is_zero() {
            true => 0,
            false => u32::try_from(wait.as_nanos().div_ceil(1_000_000)).unwrap_or(u32::MAX - 1),
        };
        // SAFETY: room for `entries.len()` entries and a count to write.
        let ok = unsafe {
            GetQueuedCompletionStatusEx(
                self.raw(),
                entries.as_mut_ptr(),
                entries.len() as u32,
                &mut got,
                ms,
                0,
            )
        };
        if ok == 0 {
            return match last() {
                WAIT_TIMEOUT => Ok(0),
                _ => Err(io::Error::last_os_error()),
            };
        }
        Ok(got as usize)
    }
}

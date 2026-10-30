//! A watch on Windows: `ReadDirectoryChangesW` of the whole tree under a directory, overlapped
//! on the port, asked again after each completion. A completion of no bytes, or of
//! `STATUS_NOTIFY_ENUM_DIR`, is the system's buffer overflowing: changes were lost.

use std::{
    io,
    os::windows::io::{AsRawHandle as _, OwnedHandle},
    path::Path,
};

use windows_sys::Win32::{
    Storage::FileSystem::{
        CreateFileW, FILE_ACTION_ADDED, FILE_ACTION_RENAMED_NEW_NAME, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_ATTRIBUTES,
        FILE_NOTIFY_CHANGE_CREATION, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME,
        FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, GetLongPathNameW, OPEN_EXISTING, ReadDirectoryChangesW,
    },
    System::IO::OVERLAPPED,
};

use super::sys;
use crate::{Done, FileName};

/// What one completion can say: 64 KiB, the most a watch of a share may take.
const BUFFER: usize = 64 << 10;

const FILTER: u32 = FILE_NOTIFY_CHANGE_FILE_NAME
    | FILE_NOTIFY_CHANGE_DIR_NAME
    | FILE_NOTIFY_CHANGE_ATTRIBUTES
    | FILE_NOTIFY_CHANGE_SIZE
    | FILE_NOTIFY_CHANGE_LAST_WRITE
    | FILE_NOTIFY_CHANGE_CREATION;

pub(super) struct Watching {
    handle: OwnedHandle,
    /// The watched directory, as the driver names it.
    dir: FileName,
    /// The driver's root, which names are relative to.
    root: std::sync::Arc<Path>,
    /// Where the system writes its records: `u32`s, as the call needs them aligned.
    buffer: Box<[u32]>,
    pub(super) stopping: bool,
}

impl Watching {
    pub(super) fn new(root: &std::sync::Arc<Path>, dir: &FileName) -> io::Result<Watching> {
        let path = root.join(dir.as_str());
        let wide = sys::path(&path)?;
        // SAFETY: a NUL-terminated path; no security attributes or template.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_LIST_DIRECTORY,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                std::ptr::null_mut(),
            )
        };
        let handle = sys::owned(handle).map_err(|error| {
            io::Error::new(error.kind(), format!("watching {}: {error}", path.display()))
        })?;
        Ok(Watching {
            handle,
            dir: dir.clone(),
            root: root.clone(),
            buffer: vec![0u32; BUFFER / 4].into_boxed_slice(),
            stopping: false,
        })
    }

    pub(super) fn raw(&self) -> windows_sys::Win32::Foundation::HANDLE {
        self.handle.as_raw_handle()
    }

    /// Asks for the next changes, into the buffer, completing on the port through
    /// `overlapped`. Both stay where they are until that completion is dequeued (the driver
    /// keeps them in the request until then).
    pub(super) fn read(&mut self, overlapped: *mut OVERLAPPED) -> io::Result<()> {
        // SAFETY: an open directory handle, a buffer of the length given, and an
        // `OVERLAPPED` that outlive the request (above).
        sys::check(unsafe {
            ReadDirectoryChangesW(
                self.raw(),
                self.buffer.as_mut_ptr().cast(),
                (self.buffer.len() * 4) as u32,
                1,
                FILTER,
                std::ptr::null_mut(),
                overlapped,
                None,
            )
        })
    }

    /// What `got` bytes of records say changed. A directory made or moved in is walked, and
    /// what is in it named, as nothing told of it.
    pub(super) fn take(&self, got: usize) -> Done {
        if got == 0 {
            return Done::Changed { files: Vec::new(), lost: true };
        }
        // SAFETY: the system wrote `got` bytes of the buffer; read as bytes.
        let bytes = unsafe { std::slice::from_raw_parts(self.buffer.as_ptr().cast::<u8>(), got.min(BUFFER)) };
        let (records, mut lost) = parse(bytes);
        let mut files = Vec::new();
        for (action, name) in records {
            let Some(name) = self.named(&name) else {
                lost = true;
                continue;
            };
            let path = self.root.join(name.as_str());
            if matches!(action, FILE_ACTION_ADDED | FILE_ACTION_RENAMED_NEW_NAME) && path.is_dir() {
                lost |= walk(&path, &name, &mut files).is_err();
            }
            files.push(name);
        }
        files.sort();
        files.dedup();
        Done::Changed { files, lost }
    }

    /// A record's name (relative to the watched directory, `\`-separated) as the driver names
    /// it; a short (8.3) name is made long where the file is still there to ask.
    fn named(&self, name: &str) -> Option<FileName> {
        let joined = format!("{}/{}", self.dir.as_str(), name.replace('\\', "/"));
        let long = match joined.contains('~') {
            true => long_name(&self.root.join(&joined)),
            false => None,
        };
        let long = long.and_then(|long| {
            let long = long.strip_prefix(&*self.root).ok()?.to_str()?.replace('\\', "/");
            FileName::new(&long)
        });
        long.or_else(|| FileName::new(&joined))
    }
}

/// The records of a buffer: each one's action and name. Where one runs past the end or is not
/// Unicode, the rest is lost.
fn parse(bytes: &[u8]) -> (Vec<(u32, String)>, bool) {
    let mut records = Vec::new();
    let mut at = 0usize;
    let u32_at = |at: usize| bytes.get(at..at + 4).map(|it| u32::from_ne_bytes(it.try_into().unwrap()));
    loop {
        let (Some(next), Some(action), Some(len)) = (u32_at(at), u32_at(at + 4), u32_at(at + 8))
        else {
            return (records, true);
        };
        let Some(name) = bytes.get(at + 12..at + 12 + len as usize) else {
            return (records, true);
        };
        let wide: Vec<u16> = name.chunks_exact(2).map(|it| u16::from_ne_bytes([it[0], it[1]])).collect();
        match String::from_utf16(&wide) {
            Ok(name) => records.push((action, name)),
            Err(_) => return (records, true),
        }
        if next == 0 {
            return (records, false);
        }
        at += next as usize;
    }
}

/// The long form of `path`, which exists.
fn long_name(path: &Path) -> Option<std::path::PathBuf> {
    use std::os::windows::ffi::OsStringExt as _;
    let wide = sys::wide(path.as_os_str()).ok()?;
    let mut long = vec![0u16; 1024];
    // SAFETY: a NUL-terminated path and a buffer of the length given.
    let len = unsafe { GetLongPathNameW(wide.as_ptr(), long.as_mut_ptr(), long.len() as u32) };
    let len = len as usize;
    if len == 0 || len >= long.len() {
        return None;
    }
    Some(std::ffi::OsString::from_wide(&long[..len]).into())
}

/// Every file and directory under `path` (named `name`) into `files`.
fn walk(path: &Path, name: &FileName, files: &mut Vec<FileName>) -> io::Result<()> {
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let Some(file) = entry.file_name().to_str().and_then(|it| FileName::new(&format!("{name}/{it}")))
        else {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "a name that is not Unicode"));
        };
        if entry.file_type()?.is_dir() {
            walk(&entry.path(), &file, files)?;
        }
        files.push(file);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(next: u32, action: u32, name: &str) -> Vec<u8> {
        let wide: Vec<u8> = name.encode_utf16().flat_map(u16::to_ne_bytes).collect();
        let mut bytes = Vec::new();
        bytes.extend(next.to_ne_bytes());
        bytes.extend(action.to_ne_bytes());
        bytes.extend((wide.len() as u32).to_ne_bytes());
        bytes.extend(wide);
        while bytes.len() % 4 != 0 {
            bytes.push(0);
        }
        bytes
    }

    #[test]
    fn records_are_read_to_their_end() {
        let first = record(0, FILE_ACTION_ADDED, "deep\\lib.rs");
        let mut bytes = record(0, 3, "keep");
        let next = bytes.len() as u32;
        bytes.splice(0..4, next.to_ne_bytes());
        bytes.extend(first);
        let (records, lost) = parse(&bytes);
        assert!(!lost);
        assert_eq!(records, [(3, "keep".to_owned()), (FILE_ACTION_ADDED, "deep\\lib.rs".to_owned())]);
    }

    #[test]
    fn a_record_past_the_end_is_lost() {
        let bytes = record(0, FILE_ACTION_ADDED, "keep");
        let (records, lost) = parse(&bytes[..bytes.len() - 6]);
        assert!(lost && records.is_empty(), "{records:?}");
        let (_, lost) = parse(&bytes[..6]);
        assert!(lost);
    }
}

//! Watching a tree with fanotify: one mark of each file system the watched directory spans
//! (`FAN_MARK_FILESYSTEM`), so nothing is added as directories appear, and events that name
//! the directory a change was in by a file handle and the file by its name
//! (`FAN_REPORT_DFID_NAME`, Linux 5.9). A handle is opened again (`open_by_handle_at`) to
//! learn where its directory is now; a directory gone since is named where it was last seen.
//!
//! Needs `CAP_SYS_ADMIN` (the mark) and `CAP_DAC_READ_SEARCH` (opening handles):
//! [`Fanotify::new`] fails with a bare `EPERM` where the process lacks either, and
//! [`crate::watch::Tree`] watches with inotify then.

use std::{
    collections::BTreeSet,
    ffi::CString,
    io,
    os::{
        fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd},
        unix::ffi::OsStrExt as _,
    },
    path::{Path, PathBuf},
};

use rustc_hash::FxHashMap;

use crate::{Done, FileName};

const CLASS_NOTIF: u32 = 0x0;
const CLOEXEC: u32 = 0x1;
const NONBLOCK: u32 = 0x2;
const REPORT_DIR_FID: u32 = 0x400;
const REPORT_NAME: u32 = 0x800;

const MARK_ADD: u32 = 0x1;
const MARK_FILESYSTEM: u32 = 0x100;

const MODIFY: u64 = 0x2;
const ATTRIB: u64 = 0x4;
const CLOSE_WRITE: u64 = 0x8;
const MOVED_FROM: u64 = 0x40;
const MOVED_TO: u64 = 0x80;
const CREATE: u64 = 0x100;
const DELETE: u64 = 0x200;
const Q_OVERFLOW: u64 = 0x4000;
const ONDIR: u64 = 0x4000_0000;
const MASK: u64 = MODIFY | ATTRIB | CLOSE_WRITE | MOVED_FROM | MOVED_TO | CREATE | DELETE | ONDIR;

const INFO_DFID_NAME: u8 = 2;
const INFO_DFID: u8 = 3;

/// `struct fanotify_event_metadata`.
const METADATA: usize = 24;
/// `MAX_HANDLE_SZ`.
const MAX_HANDLE: usize = 128;

/// `struct file_handle` with room for the largest handle.
#[repr(C)]
struct Handle {
    bytes: u32,
    kind: i32,
    handle: [u8; MAX_HANDLE],
}

/// A file system marked, and a directory of it to open its handles against.
struct Mount {
    fsid: [u8; 8],
    fd: OwnedFd,
}

pub(crate) struct Fanotify {
    fd: OwnedFd,
    /// The driver's root with its links resolved, as the kernel names an opened handle.
    root: PathBuf,
    dir: FileName,
    mounts: Vec<Mount>,
    /// Where each directory under `dir` was last seen, relative to the root, by its file
    /// system and handle.
    known: FxHashMap<Vec<u8>, String>,
}

impl Fanotify {
    /// Watches `dir` under `root` and everything under it.
    pub(crate) fn new(root: &Path, dir: &FileName, nonblocking: bool) -> io::Result<Fanotify> {
        let flags = CLASS_NOTIF
            | CLOEXEC
            | REPORT_DIR_FID
            | REPORT_NAME
            | if nonblocking { NONBLOCK } else { 0 };
        let event_flags = (libc::O_RDONLY | libc::O_CLOEXEC) as libc::c_uint;
        // SAFETY: no pointers; a descriptor of its own or -1.
        let fd = unsafe { libc::syscall(libc::SYS_fanotify_init, flags, event_flags) };
        if fd < 0 {
            let error = io::Error::last_os_error();
            return Err(match error.raw_os_error() {
                Some(libc::EPERM) => error,
                Some(libc::EINVAL) => io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!(
                        "fanotify_init: {error}: Linux 5.9 or later reports a change's directory \
                         and name (FAN_REPORT_DFID_NAME), and this is Linux {}",
                        crate::cgroup::kernel()
                    ),
                ),
                _ => named(error, "fanotify_init".into()),
            });
        }
        // SAFETY: the call returned a descriptor of its own.
        let fd = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };
        let root = std::fs::canonicalize(root)
            .map_err(|error| named(error, format!("resolving {}", root.display())))?;
        let mut watch = Fanotify {
            fd,
            root,
            dir: dir.clone(),
            mounts: Vec::new(),
            known: FxHashMap::default(),
        };
        watch.learn(dir.as_str(), true, &mut BTreeSet::new())?;
        Ok(watch)
    }

    pub(crate) fn raw(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// What the events in `bytes` say, learning each directory they say was made or moved in.
    pub(crate) fn take(&mut self, bytes: &[u8]) -> Done {
        let (events, mut lost) = parse(bytes);
        let mut files = BTreeSet::new();
        for (mask, record) in events {
            let dir = match self.resolve(record.fsid, record.kind, record.handle) {
                Ok(Some(dir)) => dir,
                Ok(None) => continue,
                Err(_) => {
                    lost = true;
                    continue;
                }
            };
            let rel = match record.name {
                b"" | b"." => dir,
                name => join(&dir, &String::from_utf8_lossy(name)),
            };
            let Some(file) = FileName::new(&rel) else { continue };
            if !file.is_under(&self.dir) {
                continue;
            }
            if mask & ONDIR != 0
                && mask & (CREATE | MOVED_TO) != 0
                && self.learn(&rel, false, &mut files).is_err()
            {
                lost = true;
            }
            files.insert(file);
        }
        Done::Changed { files: files.into_iter().collect(), lost }
    }

    /// Learns the handle of `rel` and of every directory under it, marking each file system
    /// met, and adds each file found to `found`: a directory moved in has no events of what
    /// was in it already. Every error of the watched directory itself (`top`) is the watch's;
    /// below it, a directory gone or unreadable is passed over.
    fn learn(&mut self, rel: &str, top: bool, found: &mut BTreeSet<FileName>) -> io::Result<()> {
        let mut todo = vec![rel.to_owned()];
        let mut first = top;
        while let Some(rel) = todo.pop() {
            let path = self.root.join(&rel);
            let is_top = std::mem::take(&mut first);
            match self.learn_one(&path, &rel) {
                Ok(()) => {}
                Err(error) if is_top => return Err(error),
                Err(error)
                    if matches!(
                        error.raw_os_error(),
                        Some(libc::ENOENT | libc::ENOTDIR | libc::ESTALE | libc::EACCES | libc::ELOOP)
                    ) =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            }
            let Ok(entries) = std::fs::read_dir(&path) else { continue };
            for entry in entries.flatten() {
                let child = join(&rel, &entry.file_name().to_string_lossy());
                if entry.file_type().is_ok_and(|it| it.is_dir()) {
                    todo.push(child);
                } else if let Some(file) = FileName::new(&child) {
                    found.insert(file);
                }
            }
        }
        Ok(())
    }

    /// The directory at `path` known by its handle, its file system marked if it was not.
    fn learn_one(&mut self, path: &Path, rel: &str) -> io::Result<()> {
        let c = c_path(path)?;
        // SAFETY: a NUL-terminated path; a descriptor of its own or -1.
        let dir = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) };
        if dir < 0 {
            let error = io::Error::last_os_error();
            return Err(keep_eperm(error, format!("opening {}", path.display())));
        }
        // SAFETY: the call returned a descriptor of its own.
        let dir = unsafe { OwnedFd::from_raw_fd(dir) };
        let fsid = fsid(dir.as_raw_fd()).map_err(|error| named(error, format!("statfs of {}", path.display())))?;
        let (kind, handle) = handle_of(dir.as_raw_fd()).map_err(|error| match error.raw_os_error() {
            Some(libc::EOPNOTSUPP) => io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "the file system of {} gives no file handles, which watching it with \
                     fanotify needs: {error}",
                    path.display()
                ),
            ),
            _ => keep_eperm(error, format!("name_to_handle_at of {}", path.display())),
        })?;
        if !self.mounts.iter().any(|it| it.fsid == fsid) {
            // SAFETY: a fanotify descriptor and a NUL-terminated path.
            let marked = unsafe {
                libc::syscall(
                    libc::SYS_fanotify_mark,
                    self.fd.as_raw_fd(),
                    MARK_ADD | MARK_FILESYSTEM,
                    MASK,
                    libc::AT_FDCWD,
                    c.as_ptr(),
                )
            };
            if marked != 0 {
                let error = io::Error::last_os_error();
                return Err(match error.raw_os_error() {
                    Some(libc::EXDEV | libc::EOPNOTSUPP | libc::ENODEV) => io::Error::new(
                        io::ErrorKind::Unsupported,
                        format!(
                            "fanotify cannot watch the file system of {} by file handles: {error}",
                            path.display()
                        ),
                    ),
                    _ => keep_eperm(error, format!("fanotify_mark of {}", path.display())),
                });
            }
            // Opening a handle needs CAP_DAC_READ_SEARCH, which marking did not.
            open_by_handle(dir.as_raw_fd(), kind, &handle)
                .map_err(|error| keep_eperm(error, format!("open_by_handle_at in {}", path.display())))?;
            self.mounts.push(Mount { fsid, fd: dir });
        }
        if FileName::new(rel).is_some_and(|it| it.is_under(&self.dir)) {
            self.known.insert(key(&fsid, kind, &handle), rel.to_owned());
        }
        Ok(())
    }

    /// Where the directory of `handle` is, relative to the root: `None` outside it, or for a
    /// directory gone that was never under the watched one. An error where it cannot be said.
    fn resolve(&mut self, fsid: &[u8], kind: i32, handle: &[u8]) -> io::Result<Option<String>> {
        let Some(mount) = self.mounts.iter().find(|it| it.fsid[..] == *fsid) else { return Ok(None) };
        let key = key(fsid, kind, handle);
        let path = open_by_handle(mount.fd.as_raw_fd(), kind, handle).and_then(|fd| path_of(&fd));
        match path {
            Ok(path) => {
                let Ok(rel) = path.strip_prefix(&self.root) else {
                    self.known.remove(&key);
                    return Ok(None);
                };
                let rel = rel.to_string_lossy().into_owned();
                if FileName::new(&rel).is_some_and(|it| it.is_under(&self.dir)) {
                    self.known.insert(key, rel.clone());
                } else {
                    self.known.remove(&key);
                }
                Ok(Some(rel))
            }
            Err(error) if matches!(error.raw_os_error(), Some(libc::ESTALE | libc::ENOENT)) => {
                Ok(self.known.get(&key).cloned())
            }
            Err(error) => Err(error),
        }
    }
}

/// A directory's handle and name in an event.
#[derive(Debug, PartialEq, Eq)]
struct Record<'a> {
    fsid: &'a [u8],
    kind: i32,
    handle: &'a [u8],
    name: &'a [u8],
}

/// Each event in `bytes` as `read` gave them, with its mask and its directory record, and
/// whether changes were lost: the queue overflowed, or an event could not be read.
fn parse(bytes: &[u8]) -> (Vec<(u64, Record<'_>)>, bool) {
    let mut events = Vec::new();
    let mut lost = false;
    let mut at = 0;
    while at + METADATA <= bytes.len() {
        let len = u32_at(bytes, at) as usize;
        let metadata_len = u16_at(bytes, at + 6) as usize;
        let mask = u64_at(bytes, at + 8);
        if len < METADATA || metadata_len < METADATA || metadata_len > len || at + len > bytes.len() {
            return (events, true);
        }
        let event = &bytes[at..at + len];
        at += len;
        if mask & Q_OVERFLOW != 0 {
            lost = true;
            continue;
        }
        let mut record = None;
        let mut info = metadata_len;
        while info + 4 <= event.len() {
            let kind = event[info];
            let info_len = u16_at(event, info + 2) as usize;
            if info_len < 4 || info + info_len > event.len() {
                break;
            }
            let body = &event[info + 4..info + info_len];
            info += info_len;
            if kind == INFO_DFID_NAME || kind == INFO_DFID {
                record = fid(body, kind == INFO_DFID_NAME);
            }
        }
        match record {
            Some(record) => events.push((mask, record)),
            None => lost = true,
        }
    }
    (events, lost || at != bytes.len())
}

/// `struct fanotify_event_info_fid` after its header: the file system's id, a
/// `struct file_handle`, and of a named record the name, NUL-terminated.
fn fid(body: &[u8], named: bool) -> Option<Record<'_>> {
    let fsid = body.get(..8)?;
    let size = u32_at(body.get(..16)?, 8) as usize;
    let kind = i32::from_ne_bytes(body[12..16].try_into().ok()?);
    let handle = body.get(16..16 + size)?;
    let name = match named {
        true => {
            let rest = &body[16 + size..];
            &rest[..rest.iter().position(|&it| it == 0)?]
        }
        false => &[],
    };
    Some(Record { fsid, kind, handle, name })
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_ne_bytes(bytes[at..at + 2].try_into().expect("two bytes"))
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_ne_bytes(bytes[at..at + 8].try_into().expect("eight bytes"))
}

fn key(fsid: &[u8], kind: i32, handle: &[u8]) -> Vec<u8> {
    [fsid, &kind.to_ne_bytes()[..], handle].concat()
}

/// The id of the file system of `fd`, as fanotify's events carry it.
fn fsid(fd: RawFd) -> io::Result<[u8; 8]> {
    // SAFETY: all zeros is a valid `statfs`; the call fills it.
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: an open descriptor and a `statfs` to write.
    if unsafe { libc::fstatfs(fd, &mut stat) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fsid_t` is two `int`s (`__kernel_fsid_t`), whose field libc keeps private.
    let words: [i32; 2] = unsafe { std::mem::transmute(stat.f_fsid) };
    let mut fsid = [0; 8];
    fsid[..4].copy_from_slice(&words[0].to_ne_bytes());
    fsid[4..].copy_from_slice(&words[1].to_ne_bytes());
    Ok(fsid)
}

/// The handle of the directory open as `fd`: its type and bytes.
fn handle_of(fd: RawFd) -> io::Result<(i32, Vec<u8>)> {
    let mut handle = Handle { bytes: MAX_HANDLE as u32, kind: 0, handle: [0; MAX_HANDLE] };
    let mut mount_id: libc::c_int = 0;
    // SAFETY: an open descriptor with an empty path, a handle with room for `bytes` bytes,
    // and an int for the mount's id.
    let got = unsafe {
        libc::syscall(
            libc::SYS_name_to_handle_at,
            fd,
            c"".as_ptr(),
            &raw mut handle,
            &raw mut mount_id,
            libc::AT_EMPTY_PATH,
        )
    };
    if got != 0 {
        return Err(io::Error::last_os_error());
    }
    let size = (handle.bytes as usize).min(MAX_HANDLE);
    Ok((handle.kind, handle.handle[..size].to_vec()))
}

/// The file of a handle, opened as a path only, against the file system of `mount`.
fn open_by_handle(mount: RawFd, kind: i32, bytes: &[u8]) -> io::Result<OwnedFd> {
    if bytes.len() > MAX_HANDLE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "a file handle longer than any"));
    }
    let mut handle = Handle { bytes: bytes.len() as u32, kind, handle: [0; MAX_HANDLE] };
    handle.handle[..bytes.len()].copy_from_slice(bytes);
    // SAFETY: an open descriptor and a handle of `bytes` bytes; a descriptor of its own or -1.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_open_by_handle_at,
            mount,
            &raw mut handle,
            libc::O_PATH | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call returned a descriptor of its own.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

/// Where the file open as `fd` is now; `ESTALE` for one removed since.
fn path_of(fd: &OwnedFd) -> io::Result<PathBuf> {
    let path = std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd()))?;
    match path.as_os_str().as_bytes().ends_with(b" (deleted)") {
        true => Err(io::Error::from_raw_os_error(libc::ESTALE)),
        false => Ok(path),
    }
}

fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a path with a NUL"))
}

/// `error` with what was being done, except a bare `EPERM`, which says a privilege is missing.
fn keep_eperm(error: io::Error, what: String) -> io::Error {
    match error.raw_os_error() {
        Some(libc::EPERM) => error,
        _ => named(error, what),
    }
}

fn named(error: io::Error, what: String) -> io::Error {
    io::Error::new(error.kind(), format!("{what}: {error}"))
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() { name.to_owned() } else { format!("{dir}/{name}") }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    fn event(mask: u64, info: &[u8]) -> Vec<u8> {
        let len = METADATA + info.len();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(len as u32).to_ne_bytes());
        bytes.extend_from_slice(&[3, 0]);
        bytes.extend_from_slice(&(METADATA as u16).to_ne_bytes());
        bytes.extend_from_slice(&mask.to_ne_bytes());
        bytes.extend_from_slice(&(-1i32).to_ne_bytes());
        bytes.extend_from_slice(&42i32.to_ne_bytes());
        bytes.extend_from_slice(info);
        bytes
    }

    fn dfid_name(fsid: [u8; 8], kind: i32, handle: &[u8], name: &[u8]) -> Vec<u8> {
        let mut body = fsid.to_vec();
        body.extend_from_slice(&(handle.len() as u32).to_ne_bytes());
        body.extend_from_slice(&kind.to_ne_bytes());
        body.extend_from_slice(handle);
        body.extend_from_slice(name);
        body.push(0);
        while (body.len() + 4) % 4 != 0 {
            body.push(0);
        }
        let mut info = vec![INFO_DFID_NAME, 0];
        info.extend_from_slice(&((body.len() + 4) as u16).to_ne_bytes());
        info.extend_from_slice(&body);
        info
    }

    #[test]
    fn events_are_read_with_their_directory_and_name() {
        let fsid = [1, 2, 3, 4, 5, 6, 7, 8];
        let mut bytes = event(CREATE, &dfid_name(fsid, 1, &[9; 8], b"lib.rs"));
        bytes.extend(event(DELETE | ONDIR, &dfid_name(fsid, 1, &[7; 12], b"sub")));
        let (events, lost) = parse(&bytes);
        assert!(!lost);
        assert_eq!(
            events,
            vec![
                (CREATE, Record { fsid: &fsid, kind: 1, handle: &[9; 8], name: b"lib.rs" }),
                (DELETE | ONDIR, Record { fsid: &fsid, kind: 1, handle: &[7; 12], name: b"sub" }),
            ]
        );
    }

    #[test]
    fn an_overflow_or_a_short_event_is_lost() {
        let fsid = [0; 8];
        let mut bytes = event(Q_OVERFLOW, &[]);
        bytes.extend(event(MODIFY, &dfid_name(fsid, 1, &[1; 8], b"a")));
        let (events, lost) = parse(&bytes);
        assert!(lost);
        assert_eq!(events.len(), 1);
        let (events, lost) = parse(&bytes[..bytes.len() - 3]);
        assert!(lost);
        assert!(events.is_empty());
        // An event with no record of its directory cannot be named.
        assert_eq!(parse(&event(MODIFY, &[])), (Vec::new(), true));
    }

    /// Where the process may: a file made under the watched directory, in a directory made
    /// after the watch began, is heard of, and one outside it is not.
    #[test]
    fn hears_what_is_under_the_watched_directory_only() {
        let root = std::env::temp_dir().join(format!("blaze-fanotify-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        let mut watch = match Fanotify::new(&root, &FileName::new("src").unwrap(), true) {
            Ok(it) => it,
            Err(error) if error.raw_os_error() == Some(libc::EPERM) => {
                eprintln!("skipped: fanotify needs CAP_SYS_ADMIN and CAP_DAC_READ_SEARCH: {error}");
                return;
            }
            Err(error) => panic!("{error}"),
        };
        std::fs::create_dir_all(root.join("src/deep")).unwrap();
        std::fs::write(root.join("src/deep/lib.rs"), b"fn f() {}").unwrap();
        std::fs::write(root.join("outside"), b"not watched").unwrap();
        let mut heard = BTreeSet::new();
        let mut buffer = vec![0u8; crate::watch::BUFFER];
        let deadline = Instant::now() + Duration::from_secs(10);
        let want = FileName::new("src/deep/lib.rs").unwrap();
        while !heard.contains(&want) && Instant::now() < deadline {
            let mut fds = [libc::pollfd { fd: watch.raw(), events: libc::POLLIN, revents: 0 }];
            // SAFETY: one pollfd of an open descriptor.
            unsafe { libc::poll(fds.as_mut_ptr(), 1, 100) };
            // SAFETY: the buffer has room for what is read.
            let n = unsafe { libc::read(watch.raw(), buffer.as_mut_ptr().cast(), buffer.len()) };
            if n > 0 {
                let Done::Changed { files, .. } = watch.take(&buffer[..n as usize]) else {
                    unreachable!()
                };
                heard.extend(files);
            }
        }
        assert!(heard.contains(&want), "{heard:?}");
        assert!(heard.contains(&FileName::new("src/deep").unwrap()), "{heard:?}");
        assert!(heard.iter().all(|it| it.is_under(&FileName::new("src").unwrap())), "{heard:?}");
        drop(watch);
        std::fs::remove_dir_all(root).unwrap();
    }
}

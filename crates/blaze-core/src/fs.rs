//! The blocking calls behind the effects, for [`crate::threads::Threads`]'s threads and for what
//! [`crate::uring::Uring`] does where it is submitted.

use std::{
    fs::{File, OpenOptions},
    io::{self, Write as _},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::UNIX_EPOCH,
};

use crate::Meta;

static TEMPORARY: AtomicU64 = AtomicU64::new(0);

/// A name next to `path` no other write of this process or another uses.
pub(crate) fn temporary_next_to(path: &Path) -> PathBuf {
    let n = TEMPORARY.fetch_add(1, Ordering::Relaxed);
    let name = path.file_name().map(|it| it.to_string_lossy().into_owned()).unwrap_or_default();
    path.with_file_name(format!(".{name}.{}-{n}.tmp", std::process::id()))
}

pub(crate) fn make_parent(path: &Path) -> io::Result<()> {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => std::fs::create_dir_all(parent),
        _ => Ok(()),
    }
}

/// `bytes` as the whole of `path`: written to a temporary file next to it, synced, renamed over
/// it, and the directory synced, so the new name survives a crash.
pub(crate) fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    make_parent(path)?;
    let temporary = temporary_next_to(path);
    let written = (|| {
        let mut file = OpenOptions::new().write(true).create_new(true).open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()
    })();
    if let Err(error) = written {
        _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    commit_rename(&temporary, path)
}

/// `from` renamed to `to`, and `to`'s directory synced.
pub(crate) fn commit_rename(from: &Path, to: &Path) -> io::Result<()> {
    if let Err(error) = std::fs::rename(from, to) {
        _ = std::fs::remove_file(from);
        return Err(error);
    }
    sync_dir_of(to)
}

#[cfg(unix)]
fn sync_dir_of(path: &Path) -> io::Result<()> {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => File::open(parent)?.sync_all(),
        _ => Ok(()),
    }
}

#[cfg(not(unix))]
fn sync_dir_of(_: &Path) -> io::Result<()> {
    Ok(())
}

pub(crate) fn stat(path: &Path) -> io::Result<Option<Meta>> {
    match std::fs::metadata(path) {
        Ok(meta) => Ok(Some(Meta {
            len: meta.len(),
            dir: meta.is_dir(),
            modified_ns: meta
                .modified()
                .ok()
                .and_then(|it| it.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |it| u64::try_from(it.as_nanos()).unwrap_or(u64::MAX)),
        })),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub(crate) fn remove(path: &Path) -> io::Result<()> {
    let removed = match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir(path),
        Ok(_) => std::fs::remove_file(path),
        Err(error) => Err(error),
    };
    match removed {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

pub(crate) fn link(from: &Path, to: &Path) -> io::Result<()> {
    make_parent(to)?;
    std::fs::hard_link(from, to)
}

/// `to` made sharing `from`'s blocks, or an error that says the file system cannot.
pub(crate) fn clone(from: &Path, to: &Path) -> io::Result<()> {
    make_parent(to)?;
    clone_file(from, to).map_err(|error| match error.raw_os_error() {
        Some(code) if refuses_sharing(code) => io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "the file system of {} cannot share blocks with {} ({error}); blaze does not \
                 copy instead",
                to.display(),
                from.display()
            ),
        ),
        _ => error,
    })
}

#[cfg(target_os = "linux")]
fn refuses_sharing(code: i32) -> bool {
    matches!(code, libc::EOPNOTSUPP | libc::EXDEV | libc::EINVAL)
}

#[cfg(target_os = "macos")]
fn refuses_sharing(code: i32) -> bool {
    matches!(code, libc::ENOTSUP | libc::EXDEV)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn refuses_sharing(_: i32) -> bool {
    false
}

#[cfg(target_os = "linux")]
fn clone_file(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::fd::AsRawFd as _;
    /// `_IOW(0x94, 9, int)` of `linux/fs.h`.
    const FICLONE: libc::c_ulong = 0x4004_9409;
    let source = File::open(from)?;
    let target = OpenOptions::new().write(true).create_new(true).open(to)?;
    // SAFETY: two open descriptors; the call reads the first's blocks into the second.
    let cloned = unsafe { libc::ioctl(target.as_raw_fd(), FICLONE as _, source.as_raw_fd()) };
    if cloned == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    drop(target);
    _ = std::fs::remove_file(to);
    Err(error)
}

#[cfg(target_os = "macos")]
fn clone_file(from: &Path, to: &Path) -> io::Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt as _};
    let c = |path: &Path| {
        CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a path with a NUL"))
    };
    let (source, target) = (c(from)?, c(to)?);
    // SAFETY: two NUL-terminated paths.
    match unsafe { libc::clonefile(source.as_ptr(), target.as_ptr(), 0) } {
        0 => Ok(()),
        _ => Err(io::Error::last_os_error()),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn clone_file(_: &Path, _: &Path) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "no call that shares blocks on this system"))
}

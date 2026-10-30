//! The blocking calls behind the effects, for [`crate::threads::Threads`]'s threads and for what
//! [`crate::uring::Uring`] does where it is submitted; and [`Mapped`], every crate's read-only
//! file mapping.

mod mapped;

pub use mapped::{Advice, MapOptions, Mapped};

#[cfg(unix)]
use std::{fs::File, io::Write as _};
use std::{
    fs::OpenOptions,
    io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::UNIX_EPOCH,
};

#[cfg(unix)]
use crate::{Names, Stamp};
use crate::{FileName, Meta};

static TEMPORARY: AtomicU64 = AtomicU64::new(0);

/// A name next to `path` no other write of this process or another uses.
pub(crate) fn temporary_next_to(path: &Path) -> PathBuf {
    // alloc: once per caller that keeps the name (a listen's socket).
    let mut temporary = PathBuf::new();
    temporary_into(path, &mut temporary);
    temporary
}

/// [`temporary_next_to`] into `into`, which is emptied first: a name made in a reused buffer.
fn temporary_into(path: &Path, into: &mut PathBuf) {
    use std::fmt::Write as _;
    let n = TEMPORARY.fetch_add(1, Ordering::Relaxed);
    into.as_mut_os_string().clear();
    if let Some(parent) = path.parent() {
        into.push(parent);
    }
    let name = path.file_name().map(|it| it.to_string_lossy()).unwrap_or_default();
    blaze_mem::scratch::with_string(|file| {
        _ = write!(file, ".{name}.{}-{n}.tmp", std::process::id());
        into.push(&*file);
    });
}

/// `f` of a NUL-terminated copy of `path`, made in the thread's byte buffer.
#[cfg(unix)]
fn with_c_path<R>(path: &Path, f: impl FnOnce(&std::ffi::CStr) -> io::Result<R>) -> io::Result<R> {
    use std::os::unix::ffi::OsStrExt as _;
    blaze_mem::scratch::with_bytes(|bytes| {
        bytes.extend_from_slice(path.as_os_str().as_bytes());
        bytes.push(0);
        let path = std::ffi::CStr::from_bytes_with_nul(bytes)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a path with a NUL"))?;
        f(path)
    })
}

/// The [`FileName`] `{dir}/{name}`, or an error naming `path` where `name` is not UTF-8 or
/// makes no name. The name is joined in the thread's string buffer, so only the `FileName` is
/// made.
fn child_name(dir: &FileName, name: &std::ffi::OsStr, path: impl FnOnce() -> PathBuf) -> io::Result<FileName> {
    use std::fmt::Write as _;
    let file = blaze_mem::scratch::with_string(|joined| {
        let name = name.to_str()?;
        _ = write!(joined, "{dir}/{name}");
        FileName::new(joined)
    });
    match file {
        Some(file) => Ok(file),
        None => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            // alloc: an error path.
            format!("{}: a name that is not UTF-8", path().display()),
        )),
    }
}

pub(crate) fn make_parent(path: &Path) -> io::Result<()> {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => std::fs::create_dir_all(parent),
        _ => Ok(()),
    }
}

/// `bytes` as the whole of `path`: written to a temporary file next to it, synced, renamed over
/// it, and the directory synced, so the new name survives a crash.
#[cfg(unix)]
pub(crate) fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    make_parent(path)?;
    blaze_mem::scratch::with_path(|temporary| {
        temporary_into(path, temporary);
        let written = (|| {
            let mut file = OpenOptions::new().write(true).create_new(true).open(&*temporary)?;
            file.write_all(bytes)?;
            file.sync_all()
        })();
        if let Err(error) = written {
            _ = std::fs::remove_file(&*temporary);
            return Err(error);
        }
        commit_rename(temporary, path)
    })
}

/// `from` renamed to `to`, and `to`'s directory synced.
#[cfg(unix)]
pub(crate) fn commit_rename(from: &Path, to: &Path) -> io::Result<()> {
    if let Err(error) = std::fs::rename(from, to) {
        _ = std::fs::remove_file(from);
        return Err(error);
    }
    sync_dir_of(to)
}

/// `from` renamed over `to`, made the new name durably; `from` is left as it was when the
/// rename fails.
#[cfg(unix)]
pub(crate) fn rename(from: &Path, to: &Path) -> io::Result<()> {
    make_parent(to)?;
    std::fs::rename(from, to)?;
    sync_dir_of(to)?;
    match (from.parent(), to.parent()) {
        (Some(was), Some(is)) if was != is => sync_dir_of(from),
        _ => Ok(()),
    }
}

/// `bytes` written at `at` of the file at `path`, which exists.
pub(crate) fn write_at(path: &Path, at: u64, bytes: &[u8]) -> io::Result<()> {
    let file = OpenOptions::new().write(true).open(path)?;
    // As `uring.rs`'s `O_DIRECT`: a large aligned write stays out of the unified buffer cache;
    // macOS keeps it coherent with cached and mapped pages.
    #[cfg(target_os = "macos")]
    if bytes.len() >= 1 << 20 && at % 4096 == 0 && bytes.len() % 4096 == 0 {
        use std::os::fd::AsRawFd as _;
        // SAFETY: `file`, opened above, owns the descriptor and outlives the call; the flag
        // changes only how its writes use the cache.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_NOCACHE, 1) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    #[cfg(unix)]
    return std::os::unix::fs::FileExt::write_all_at(&file, bytes, at);
    #[cfg(windows)]
    {
        let mut done = 0;
        while done < bytes.len() {
            let n = std::os::windows::fs::FileExt::seek_write(
                &file,
                &bytes[done..],
                at + done as u64,
            )?;
            if n == 0 {
                return Err(io::Error::from(io::ErrorKind::WriteZero));
            }
            done += n;
        }
        Ok(())
    }
}

#[cfg(unix)]
pub(crate) fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(windows)]
pub(crate) use crate::iocp::files::{clone, rename, set_mode, write_atomically};

#[cfg(unix)]
fn sync_dir_of(path: &Path) -> io::Result<()> {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => File::open(parent)?.sync_all(),
        _ => Ok(()),
    }
}

pub(crate) fn stat(path: &Path) -> io::Result<Option<Meta>> {
    match std::fs::metadata(path) {
        Ok(meta) => Ok(Some(meta_of(&meta))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn meta_of(meta: &std::fs::Metadata) -> Meta {
    Meta {
        len: meta.len(),
        dir: meta.is_dir(),
        modified_ns: meta
            .modified()
            .ok()
            .and_then(|it| it.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |it| u64::try_from(it.as_nanos()).unwrap_or(u64::MAX)),
    }
}

/// Every file and directory under `dir` (named `name` under the root), sorted by name;
/// symbolic links are not followed.
pub(crate) fn list(dir: &Path, name: &FileName) -> io::Result<Vec<(FileName, Meta)>> {
    // alloc: the listing's payload.
    let mut found = Vec::new();
    // Each directory still to read, by its index in `found`; `None` for `dir` itself.
    let mut todo: Vec<Option<usize>> = Vec::with_capacity(16); // alloc: once per listing.
    todo.push(None);
    while let Some(at) = todo.pop() {
        let read = blaze_mem::scratch::with_path(|path| {
            path.push(dir);
            if let Some(at) = at {
                let file: &FileName = &found[at].0;
                path.push(&file.as_str()[name.as_str().len() + 1..]);
            }
            // alloc: once per directory, std's handle of it.
            std::fs::read_dir(&*path).map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", path.display())))
        })?;
        for entry in read {
            let entry = entry?;
            // Of the entry itself, not followed through a symlink.
            let meta = entry.metadata()?;
            let under = at.map_or(name, |at| &found[at].0);
            // alloc: std's copy of the entry's name, and the name, the listing's payload.
            let file = child_name(under, &entry.file_name(), || entry.path())?;
            if meta.is_dir() {
                todo.push(Some(found.len()));
            }
            found.push((file, meta_of(&meta)));
        }
    }
    found.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    Ok(found)
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
#[cfg(unix)]
pub(crate) fn clone(from: &Path, to: &Path) -> io::Result<()> {
    make_parent(to)?;
    clone_file(from, to).map_err(|error| match error.raw_os_error() {
        Some(code) if refuses_sharing(code) => io::Error::new(
            io::ErrorKind::Unsupported,
            // alloc: an error path.
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

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
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
    with_c_path(from, |source| {
        with_c_path(to, |target| {
            // SAFETY: two NUL-terminated paths.
            match unsafe { libc::clonefile(source.as_ptr(), target.as_ptr(), 0) } {
                0 => Ok(()),
                _ => Err(io::Error::last_os_error()),
            }
        })
    })
}

/// The file mapped read-only and shared, for [`crate::Effect::Map`], as one [`Mapped`] that the
/// last [`crate::Mapping`] of it unmaps.
pub(crate) fn map(path: &Path) -> io::Result<crate::Mapping> {
    // SAFETY: `Effect::Map`'s contract: while any `Mapping` of the file lives, no effect writes,
    // appends to or truncates it in place; the store and the image replace a file only by
    // renaming another over it.
    let mapped = unsafe { Mapped::open(path) }?;
    Ok(crate::Mapping(std::sync::Arc::new(mapped))) // alloc: one per file mapped, at open
}

/// The [`Stamp`] of a `stat` of a name, not followed through a symlink.
#[cfg(unix)]
pub(crate) fn stamp_of(stat: &libc::stat) -> Stamp {
    let ns = |s: i64, n: i64| (s as u64).wrapping_mul(1_000_000_000).wrapping_add(n as u64);
    Stamp {
        ino: stat.st_ino as u64,
        size: stat.st_size as u64,
        mtime_ns: ns(stat.st_mtime as i64, stat.st_mtime_nsec as i64),
        ctime_ns: ns(stat.st_ctime as i64, stat.st_ctime_nsec as i64),
        #[allow(clippy::useless_conversion, reason = "st_mode is u16 on macOS, u32 on Linux")]
        mode: u32::from(stat.st_mode),
    }
}

/// The root's directory, opened once for a batch of `fstatat`s relative to it.
#[cfg(unix)]
fn open_dir(root: &Path) -> io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd as _;
    let fd = with_c_path(root, |path| {
        // SAFETY: a NUL-terminated path; the result is a descriptor of its own or -1.
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) };
        match fd < 0 {
            true => Err(io::Error::last_os_error()),
            false => Ok(fd),
        }
    })
    // alloc: an error path.
    .map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", root.display())))?;
    // SAFETY: the call returned a descriptor of its own.
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
}

/// One name's stamp, relative to the directory `dir`.
#[cfg(unix)]
fn stamp_at(dir: std::os::fd::RawFd, name: &std::ffi::CStr) -> Stamp {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: an open directory, a NUL-terminated relative name and room for one `stat`.
    let failed =
        unsafe { libc::fstatat(dir, name.as_ptr(), stat.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) } != 0;
    if failed {
        return match io::Error::last_os_error().raw_os_error() {
            Some(libc::ENOENT | libc::ENOTDIR) => Stamp::GONE,
            _ => Stamp::UNKNOWN,
        };
    }
    // SAFETY: the call succeeded, so it filled the `stat`.
    stamp_of(unsafe { &stat.assume_init() })
}

/// How many names one thread of a sweep stamps.
#[cfg(unix)]
const SWEEP_CHUNK: usize = 2048;

/// Every name's stamp, in order: [`crate::Effect::Stamp`] where the system has no batched
/// stat, as `fstatat`s relative to the root on up to 8 threads at once. Fails only when the
/// root cannot be opened.
#[cfg(unix)]
pub(crate) fn stamps(root: &Path, names: &Names) -> io::Result<Vec<Stamp>> {
    use std::os::fd::AsRawFd as _;
    let dir = open_dir(root)?;
    let dir = dir.as_raw_fd();
    // alloc: the batch's payload.
    let mut out = vec![Stamp::GONE; names.len()];
    let threads = std::thread::available_parallelism().map_or(1, |it| it.get().min(8));
    if names.len() <= SWEEP_CHUNK || threads == 1 {
        for (at, stamp) in out.iter_mut().enumerate() {
            *stamp = stamp_at(dir, names.c_str(at));
        }
        return Ok(out);
    }
    let each = names.len().div_ceil(threads);
    std::thread::scope(|scope| {
        for (n, stamps) in out.chunks_mut(each).enumerate() {
            scope.spawn(move || {
                for (at, stamp) in stamps.iter_mut().enumerate() {
                    *stamp = stamp_at(dir, names.c_str(n * each + at));
                }
            });
        }
    });
    Ok(out)
}

/// What is directly in `dir` (named `name` under the root), each with its stamp, sorted by
/// name; symbolic links are stamped themselves.
#[cfg(unix)]
pub(crate) fn entries(dir: &Path, name: &FileName) -> io::Result<Vec<(FileName, Stamp)>> {
    use std::os::fd::AsRawFd as _;
    let fd = open_dir(dir)?;
    // alloc: the listing's payload.
    let mut found = Vec::new();
    // alloc: once per listing, std's handle of the directory.
    let read = std::fs::read_dir(dir).map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", dir.display())));
    for entry in read? {
        let entry = entry?;
        // alloc: std's copy of the entry's name.
        let leaf = entry.file_name();
        // alloc: the name, the listing's payload.
        let file = child_name(name, &leaf, || entry.path())?;
        let stamp = with_c_path(Path::new(&leaf), |leaf| Ok(stamp_at(fd.as_raw_fd(), leaf)))?;
        found.push((file, stamp));
    }
    found.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    Ok(found)
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn clone_file(_: &Path, _: &Path) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "no call that shares blocks on this system"))
}

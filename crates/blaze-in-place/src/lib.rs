//! What a client and the daemon's state do to an executable that is spliced in place: wait
//! for the daemon to be through writing it, and copy it with the holes the splice left.

use std::{fs::File, path::Path};

/// Waits until the daemon is through writing the executable at `path` in place: one it
/// linked, which it writes after it answered. On Unix the file returned keeps it from writing
/// again until it is dropped, so a program started meanwhile is not refused.
pub fn unwritten(path: &Path) -> std::io::Result<File> {
    loop {
        let file = File::open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            file.lock_shared()?;
            // A copy the daemon is to write may have been put in its place meanwhile, under
            // the lock of its own.
            let (locked, now) = (file.metadata()?, std::fs::metadata(path)?);
            if (locked.dev(), locked.ino()) != (now.dev(), now.ino()) {
                continue;
            }
        }
        return Ok(file);
    }
}

/// Copies the file at `from` to `to` as [`std::fs::copy`] does, but for the runs of it the
/// filesystem keeps no blocks of, which stay so in the copy: a spliced executable has many,
/// and a filesystem that shares no blocks writes the rest in full.
pub fn copy_file(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    if copy_holes(from, to).is_ok() {
        return Ok(());
    }
    std::fs::copy(from, to).map(drop)
}

#[cfg(target_os = "linux")]
fn copy_holes(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::fd::AsRawFd as _;
    let source = File::open(from)?;
    let metadata = source.metadata()?;
    let target = std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(to)?;
    target.set_permissions(metadata.permissions())?;
    target.set_len(metadata.len())?;
    let (fd, out) = (source.as_raw_fd(), target.as_raw_fd());
    let end = i64::try_from(metadata.len()).map_err(std::io::Error::other)?;
    let mut at = 0;
    while at < end {
        // SAFETY: `lseek` and `copy_file_range` on descriptors this function holds open.
        let data = unsafe { libc::lseek(fd, at, libc::SEEK_DATA) };
        if data < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENXIO) {
                break;
            }
            return Err(error);
        }
        let hole = unsafe { libc::lseek(fd, data, libc::SEEK_HOLE) };
        if hole < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let (mut source, mut target) = (data, data);
        while source < hole {
            let len = usize::try_from(hole - source).unwrap_or(usize::MAX).min(1 << 30);
            let copied =
                unsafe { libc::copy_file_range(fd, &mut source, out, &mut target, len, 0) };
            match copied {
                ..0 => return Err(std::io::Error::last_os_error()),
                0 => return Err(std::io::ErrorKind::UnexpectedEof.into()),
                _ => {}
            }
        }
        at = hole;
    }
    Ok(())
}

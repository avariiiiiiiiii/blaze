//! Watching a tree on Linux (docs/v2/plan/lanes/core/plane.md's table): with fanotify, one mark of the whole
//! file system, where the process has the privileges it needs, and with inotify where it does
//! not (a watch on each directory under the watched one, added as directories appear). Either
//! way the events are read off one descriptor, which the threads driver reads on a thread of
//! its own and the ring driver through the ring.

use std::{
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

const MASK: u32 = libc::IN_CREATE
    | libc::IN_DELETE
    | libc::IN_MODIFY
    | libc::IN_CLOSE_WRITE
    | libc::IN_MOVED_FROM
    | libc::IN_MOVED_TO
    | libc::IN_ATTRIB
    | libc::IN_EXCL_UNLINK;

/// The head of an event, before its name.
const HEAD: usize = 16;

/// A tree watched, with fanotify or inotify.
pub(crate) struct Tree(Kind);

enum Kind {
    Fanotify(crate::fanotify::Fanotify),
    Inotify(Inotify),
}

impl Tree {
    /// Watches `dir` under `root` and everything under it: with fanotify unless the process
    /// lacks `CAP_SYS_ADMIN` or `CAP_DAC_READ_SEARCH` (the call fails with `EPERM`), with
    /// inotify then. Any other failure of fanotify's is the watch's.
    pub(crate) fn new(root: &Path, dir: &FileName, nonblocking: bool) -> io::Result<Tree> {
        match crate::fanotify::Fanotify::new(root, dir, nonblocking) {
            Ok(it) => Ok(Tree(Kind::Fanotify(it))),
            Err(error) if error.raw_os_error() == Some(libc::EPERM) => {
                Ok(Tree(Kind::Inotify(Inotify::new(root, dir, nonblocking)?)))
            }
            Err(error) => Err(error),
        }
    }

    /// The descriptor to read events from.
    pub(crate) fn raw(&self) -> RawFd {
        match &self.0 {
            Kind::Fanotify(it) => it.raw(),
            Kind::Inotify(it) => it.raw(),
        }
    }

    /// What the events read into `bytes` say: a [`Done::Changed`].
    pub(crate) fn take(&mut self, bytes: &[u8]) -> Done {
        match &mut self.0 {
            Kind::Fanotify(it) => it.take(bytes),
            Kind::Inotify(it) => it.take(bytes),
        }
    }
}

struct Inotify {
    fd: OwnedFd,
    root: PathBuf,
    /// Each directory watched, by its watch, relative to the root ("" for the root).
    dirs: FxHashMap<i32, String>,
}

impl Inotify {
    /// Watches `dir` under `root` and every directory under it.
    fn new(root: &Path, dir: &FileName, nonblocking: bool) -> io::Result<Inotify> {
        let flags = libc::IN_CLOEXEC | if nonblocking { libc::IN_NONBLOCK } else { 0 };
        // SAFETY: no pointers; a descriptor of its own or -1.
        let fd = unsafe { libc::inotify_init1(flags) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the call returned a descriptor of its own.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut watch = Inotify { fd, root: root.to_owned(), dirs: FxHashMap::default() };
        watch.add_tree(dir.as_str(), &mut Vec::new())?;
        Ok(watch)
    }

    fn raw(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Watches `rel` and the directories under it, adding each file in them to `found`: a file
    /// made in a directory before its watch was added has no event of its own.
    fn add_tree(&mut self, rel: &str, found: &mut Vec<FileName>) -> io::Result<()> {
        let mut todo = vec![rel.to_owned()];
        while let Some(rel) = todo.pop() {
            let path = self.root.join(&rel);
            let c = CString::new(path.as_os_str().as_bytes())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a path with a NUL"))?;
            // SAFETY: an inotify descriptor and a NUL-terminated path.
            let wd = unsafe { libc::inotify_add_watch(self.fd.as_raw_fd(), c.as_ptr(), MASK) };
            if wd < 0 {
                let error = io::Error::last_os_error();
                // Gone before it was watched: its removal is an event of its parent's.
                if error.raw_os_error() == Some(libc::ENOENT) && !self.dirs.is_empty() {
                    continue;
                }
                return Err(error);
            }
            self.dirs.insert(wd, rel.clone());
            let Ok(entries) = std::fs::read_dir(&path) else { continue };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let child = join(&rel, &name.to_string_lossy());
                if entry.file_type().is_ok_and(|it| it.is_dir()) {
                    todo.push(child);
                } else if let Some(file) = FileName::new(&child) {
                    found.push(file);
                }
            }
        }
        Ok(())
    }

    /// What the events in `bytes` say, watching each directory they say was made.
    fn take(&mut self, bytes: &[u8]) -> Done {
        let mut files = Vec::with_capacity(bytes.len() / HEAD);
        let mut rel = String::new();
        let mut lost = false;
        let mut at = 0;
        while at + HEAD <= bytes.len() {
            let word = |n: usize| {
                u32::from_ne_bytes(bytes[at + n..at + n + 4].try_into().expect("four bytes"))
            };
            let (wd, mask, len) = (word(0) as i32, word(4), word(12) as usize);
            let name_bytes = &bytes[at + HEAD..(at + HEAD + len).min(bytes.len())];
            at += HEAD + len;
            if mask & libc::IN_Q_OVERFLOW != 0 {
                lost = true;
                continue;
            }
            if mask & libc::IN_IGNORED != 0 {
                self.dirs.remove(&wd);
                continue;
            }
            let Some(dir) = self.dirs.get(&wd) else { continue };
            let end = name_bytes.iter().position(|&it| it == 0).unwrap_or(name_bytes.len());
            let name = String::from_utf8_lossy(&name_bytes[..end]);
            rel.clear();
            rel.push_str(dir);
            if !name.is_empty() {
                if !rel.is_empty() {
                    rel.push('/');
                }
                rel.push_str(&name);
            }
            if mask & libc::IN_ISDIR != 0
                && mask & (libc::IN_CREATE | libc::IN_MOVED_TO) != 0
                && self.add_tree(&rel, &mut files).is_err()
            {
                lost = true;
            }
            if let Some(file) = FileName::new(&rel) {
                files.push(file);
            }
        }
        files.sort_unstable();
        files.dedup();
        Done::Changed { files, lost }
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() { name.to_owned() } else { format!("{dir}/{name}") }
}

/// Room for many events at once.
pub(crate) const BUFFER: usize = 64 << 10;

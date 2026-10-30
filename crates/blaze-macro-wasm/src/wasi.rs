//! The part of WASI's first snapshot (`wasi_snapshot_preview1`) a proc macro's module is
//! given: its package's directory to read, at [`PACKAGE`] and nothing else, its arguments and
//! environment, the clock, random bytes, and stdout and stderr kept for its errors. This host
//! is the module's sandbox, and it grants no more than a native one: nothing is written,
//! nothing outside the package is read, no network. It sees every call, so it records what
//! the module read exactly, as a sandbox's trace has it ([`Files::traced`]): each file
//! opened or looked at with the hash of the bytes it gave, each listing with the hash of its
//! names, by their paths under [`PACKAGE`], which are the same on every machine; a read of
//! the clock or of randomness is [`Untraceable`]. A function this does not have answers
//! `ENOSYS`.

use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    sync::OnceLock,
    time::{Instant, SystemTime},
};

use blaze_core::{Traced, Untraceable};
use blaze_usercode::{
    inputs::{SPECIAL, hash_reader},
    trace::{self, DIRECTORY},
};
use wasmtime::{Caller, FuncType, Linker, Module, Val, ValType};

use crate::{State, keyed, memory};

/// Where the module sees its package: what `CARGO_MANIFEST_DIR` says to it. Not where the
/// package is, so that what it makes of a path is the same on every machine.
pub(crate) const PACKAGE: &str = "/blaze/package";

/// Where the module is told `OUT_DIR` is: nothing is there for it.
pub(crate) const OUT_DIR: &str = "/blaze/out";

const MODULE: &str = "wasi_snapshot_preview1";

const SUCCESS: i32 = 0;
const EBADF: i32 = 8;
const EINVAL: i32 = 28;
const EIO: i32 = 29;
const EISDIR: i32 = 31;
const ENOENT: i32 = 44;
const ENOSYS: i32 = 52;
const ENOTDIR: i32 = 54;
const EROFS: i32 = 69;
const ENOTCAPABLE: i32 = 76;

const FILETYPE_UNKNOWN: u8 = 0;
const FILETYPE_CHARACTER_DEVICE: u8 = 2;
const FILETYPE_DIRECTORY: u8 = 3;
const FILETYPE_REGULAR_FILE: u8 = 4;

const OFLAGS_CREAT: i32 = 1;
const OFLAGS_DIRECTORY: i32 = 2;
const OFLAGS_TRUNC: i32 = 8;
const RIGHTS_FD_WRITE: i64 = 1 << 6;

/// What one expansion may keep of what the module writes to stdout and stderr.
const KEPT_OUTPUT: usize = 64 << 10;

enum Open {
    Stdio,
    /// A directory of the package, and what its last listing from the start found: each name
    /// with its type, sorted, so a module that reads it in turns lists it once.
    Dir { rel: String, listing: Vec<(OsString, u8)> },
    File { bytes: Vec<u8>, at: u64 },
}

/// What a `stat` of a path says.
type Stat = std::io::Result<std::fs::Metadata>;

/// The module's files: the package, read-only, and what of it was read.
pub(crate) struct Files {
    root: PathBuf,
    /// `root` with its links resolved, found the first time a path through a link is checked.
    canonical: OnceLock<Option<PathBuf>>,
    fds: Vec<Option<Open>>,
    /// What the module read, by the paths it has under [`PACKAGE`], as it was when it read it.
    pub(crate) traced: Traced,
    /// What of the package is there and could not be read: an expansion that met one is not
    /// kept, as it can't say what it read.
    pub(crate) unreadable: Option<String>,
    pub(crate) output: Vec<u8>,
}

impl Files {
    /// The files of a module whose package is at `root`, or of one with no package to read
    /// when `None`.
    pub(crate) fn new(root: Option<&Path>) -> Files {
        // alloc: setup, once per instance: its descriptors.
        let mut fds = Vec::with_capacity(8);
        fds.extend([Some(Open::Stdio), Some(Open::Stdio), Some(Open::Stdio)]);
        if root.is_some() {
            // alloc: none, an empty listing until the module reads the directory.
            fds.push(Some(Open::Dir { rel: String::default(), listing: Vec::new() }));
        }
        Files {
            root: root.map(Path::to_path_buf).unwrap_or_default(),
            canonical: OnceLock::new(),
            fds,
            traced: Traced::default(),
            unreadable: None,
            output: Vec::default(),
        }
    }

    /// What the module read, each entry once, sorted.
    pub(crate) fn take_traced(&mut self) -> Traced {
        let mut traced = std::mem::take(&mut self.traced);
        trace::normalize(&mut traced);
        traced
    }

    /// Forgets what the module read, keeping the room for what it reads next.
    pub(crate) fn forget_traced(&mut self) {
        let Traced { files, dirs, env, untraceable } = &mut self.traced;
        files.clear();
        dirs.clear();
        env.clear();
        untraceable.clear();
    }

    fn insert(&mut self, open: Open) -> u32 {
        match self.fds.iter().position(Option::is_none) {
            Some(at) => {
                self.fds[at] = Some(open);
                at as u32
            }
            None => {
                self.fds.push(Some(open));
                (self.fds.len() - 1) as u32
            }
        }
    }

    fn get(&mut self, fd: i32) -> Option<&mut Open> {
        self.fds.get_mut(usize::try_from(fd).ok()?)?.as_mut()
    }

    /// The package's directory open at `fd`.
    fn dir(&self, fd: i32) -> Result<&str, i32> {
        let open = usize::try_from(fd).ok().and_then(|at| self.fds.get(at)?.as_ref());
        match open {
            Some(Open::Dir { rel, .. }) => Ok(rel),
            Some(_) => Err(ENOTDIR),
            None => Err(EBADF),
        }
    }

    /// The package's directory open at `fd` and its last listing, taken out of its descriptor
    /// until [`Files::give_dir`] puts them back.
    fn take_dir(&mut self, fd: i32) -> Result<(String, Vec<(OsString, u8)>), i32> {
        match self.get(fd) {
            Some(Open::Dir { rel, listing }) => Ok((std::mem::take(rel), std::mem::take(listing))),
            Some(_) => Err(ENOTDIR),
            None => Err(EBADF),
        }
    }

    fn give_dir(&mut self, fd: i32, (rel, listing): (String, Vec<(OsString, u8)>)) {
        if let Some(Open::Dir { rel: open, listing: had }) = self.get(fd) {
            *open = rel;
            *had = listing;
        }
    }

    /// The package's path of `path` from the directory `rel`, where that is on the machine, and
    /// what a `stat` of it says when that is known already ([`Files::links`]); or why the module
    /// may not have it: it leaves the package, by `..` or by a link.
    fn resolve(&self, rel: &str, path: &str) -> Result<(String, PathBuf, Option<Stat>), i32> {
        if path.starts_with('/') {
            return Err(ENOTCAPABLE);
        }
        // `rel` is one this made, of no `.` or `..`, so its parts go through as they are.
        let mut out = String::with_capacity(rel.len() + 1 + path.len());
        for part in rel.split('/').chain(path.split('/')) {
            match part {
                "" | "." => {}
                ".." if out.is_empty() => return Err(ENOTCAPABLE),
                ".." => out.truncate(out.rfind('/').unwrap_or(0)),
                part => {
                    if !out.is_empty() {
                        out.push('/');
                    }
                    out.push_str(part);
                }
            }
        }
        let rel = out;
        let real = self.root.join(&rel);
        let (linked, looked) = self.links(&rel, &real);
        // A link may point anywhere: what it leads to must be in the package. With no link
        // among its parts, the path is the package's own and is in it.
        if linked && let Ok(found) = real.canonicalize() {
            let root = self.canonical.get_or_init(|| self.root.canonicalize().ok());
            let Some(root) = root else { return Err(ENOENT) };
            if !found.starts_with(root) {
                return Err(ENOTCAPABLE);
            }
        }
        Ok((rel, real, looked))
    }

    /// Whether a part of `real`, the package's `rel`, is a link, each looked at without
    /// following one: a `lstat` per part of `rel`, where resolving the whole path would look at
    /// every part of the package's own path too. And, when none is a link, what a `stat` of it
    /// would say, which `lstat` of the last part said.
    fn links(&self, rel: &str, real: &Path) -> (bool, Option<Stat>) {
        let parts = if rel.is_empty() { 0 } else { 1 + rel.matches('/').count() };
        let mut looked = None;
        for (at, part) in real.ancestors().take(parts).enumerate() {
            let meta = std::fs::symlink_metadata(part);
            if meta.as_ref().is_ok_and(|it| it.file_type().is_symlink()) {
                return (true, None);
            }
            if at == 0 {
                looked = Some(meta);
            }
        }
        (false, looked)
    }

    #[cfg(test)]
    pub(crate) fn resolve_for_test(&self, rel: &str, path: &str) -> Result<String, i32> {
        self.resolve(rel, path).map(|(rel, ..)| rel)
    }

    /// Notes that the module found `found` at the package's `rel`: a file's hash, [`DIRECTORY`]
    /// for a directory, [`SPECIAL`] for neither, `None` for nothing there. The first of each
    /// path is what it found.
    fn found(&mut self, rel: &str, found: Option<[u8; 32]>) {
        self.traced.files.push((placed(rel), found));
    }

    /// Notes that the module listed the package's directory `rel`, whose names are `names`.
    pub(crate) fn listed<'a>(&mut self, rel: &str, names: impl IntoIterator<Item = &'a OsStr>) {
        self.traced.dirs.push((placed(rel), trace::listing(names)));
    }

    /// Notes what a look at `real`, the package's `rel`, finds, as [`Self::found`] has it.
    pub(crate) fn looked(&mut self, rel: &str, real: &Path) -> Option<std::fs::Metadata> {
        self.looked_as(rel, real, std::fs::metadata(real))
    }

    /// As [`Files::looked`], of what a `stat` of `real` said.
    fn looked_as(&mut self, rel: &str, real: &Path, stat: Stat) -> Option<std::fs::Metadata> {
        match stat {
            Ok(meta) if meta.is_dir() => {
                self.found(rel, Some(DIRECTORY));
                Some(meta)
            }
            Ok(meta) if meta.is_file() => {
                match std::fs::File::open(real).and_then(hash_reader) {
                    Ok(hash) => self.found(rel, Some(hash)),
                    Err(error) => self.cant(real, error),
                }
                Some(meta)
            }
            Ok(meta) => {
                self.found(rel, Some(SPECIAL));
                Some(meta)
            }
            Err(error) if missing(&error) => {
                self.found(rel, None);
                None
            }
            Err(error) => {
                self.cant(real, error);
                None
            }
        }
    }

    fn cant(&mut self, real: &Path, error: std::io::Error) {
        // alloc: the message of a file that can't be read.
        self.unreadable.get_or_insert_with(|| format!("{}: {error}", real.display()));
    }
}

fn missing(error: &std::io::Error) -> bool {
    matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory)
}

/// The path the module has for the package's `rel`.
pub(crate) fn placed(rel: &str) -> PathBuf {
    let mut path = String::with_capacity(PACKAGE.len() + 1 + rel.len());
    path.push_str(PACKAGE);
    if !rel.is_empty() {
        path.push('/');
        path.push_str(rel);
    }
    path.into()
}

/// The package's `rel` of a path the module has, `None` for one not under [`PACKAGE`].
pub(crate) fn rel_of(path: &Path) -> Option<&Path> {
    path.strip_prefix(PACKAGE).ok()
}

/// The nanoseconds the clock `id` says now: the real clock for `CLOCK_REALTIME` (0), else the
/// time since this process first asked.
fn now(id: i32) -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    match id {
        0 => SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |it| it.as_nanos() as u64),
        _ => START.get_or_init(Instant::now).elapsed().as_nanos() as u64,
    }
}

fn bytes(mem: &[u8], ptr: i32, len: i32) -> Result<&[u8], i32> {
    let (ptr, len) = (ptr as u32 as usize, len as u32 as usize);
    mem.get(ptr..ptr.checked_add(len).ok_or(EINVAL)?).ok_or(EINVAL)
}

fn put(mem: &mut [u8], ptr: i32, value: &[u8]) -> Result<(), i32> {
    let ptr = ptr as u32 as usize;
    let room = mem.get_mut(ptr..ptr.checked_add(value.len()).ok_or(EINVAL)?).ok_or(EINVAL)?;
    room.copy_from_slice(value);
    Ok(())
}

fn text(mem: &[u8], ptr: i32, len: i32) -> Result<&str, i32> {
    std::str::from_utf8(bytes(mem, ptr, len)?).map_err(|_| EINVAL)
}

/// Where an iovec array of `count` pairs at `iovs` starts and how many it has, checked to lie
/// in memory.
fn iovecs(mem: &[u8], iovs: i32, count: i32) -> Result<(usize, usize), i32> {
    bytes(mem, iovs, count.checked_mul(8).ok_or(EINVAL)?)?;
    Ok((iovs as u32 as usize, count as u32 as usize))
}

/// The (pointer, length) pair `index` of the iovec array [`iovecs`] checked at `start`.
fn iovec(mem: &[u8], start: usize, index: usize) -> (i32, i32) {
    let at = start + 8 * index;
    let word = |at: usize| i32::from_le_bytes(mem[at..at + 4].try_into().expect("four bytes"));
    (word(at), word(at + 4))
}

/// WASI's `environ_get`: the variables' text copied to `buf` whole, then each one's pointer at
/// `table`, nothing written unless both fit in memory. Guest pointers are `u32`s: past the top
/// of memory is an error, never a wrap.
pub(crate) fn environ_get(
    mem: &mut [u8],
    environ: &crate::Environ,
    table: i32,
    buf: i32,
) -> Result<(), i32> {
    if environ.starts.is_empty() {
        return Ok(());
    }
    let base = buf as u32;
    let len = u32::try_from(environ.text.len()).map_err(|_| EINVAL)?;
    let text_end = base.checked_add(len).ok_or(EINVAL)? as usize;
    let start = table as u32 as usize;
    let end = start.checked_add(4 * environ.starts.len()).ok_or(EINVAL)?;
    if text_end > mem.len() || end > mem.len() {
        return Err(EINVAL);
    }
    put(mem, buf, &environ.text)?;
    for (slot, &at) in mem[start..end].chunks_exact_mut(4).zip(&environ.starts) {
        slot.copy_from_slice(&(base + at).to_le_bytes());
    }
    Ok(())
}

fn errno(result: Result<(), i32>) -> i32 {
    result.err().unwrap_or(SUCCESS)
}

/// A WASI filestat: device, inode, type, links, size and three times, all but the type and
/// size the same for every file.
fn filestat(ino: u64, filetype: u8, size: u64) -> [u8; 64] {
    let mut out = [0u8; 64];
    out[8..16].copy_from_slice(&ino.to_le_bytes());
    out[16] = filetype;
    out[24..32].copy_from_slice(&1u64.to_le_bytes());
    out[32..40].copy_from_slice(&size.to_le_bytes());
    out
}

fn ino(rel: &str) -> u64 {
    ino_of(&[rel.as_bytes()])
}

/// The inode of the package's path that is `parts` one after the other.
fn ino_of(parts: &[&[u8]]) -> u64 {
    let digest = keyed::sha256_of(parts);
    u64::from_le_bytes(digest[..8].try_into().unwrap()) | 1
}

/// What the package has at `real`, as the module is told and as its read is recorded: a
/// file's type and size are the file's, so a look at one is a read of all of it. `looked` is
/// what a `stat` of it said, when [`Files::resolve`] knows already.
fn stat(files: &mut Files, rel: &str, real: &Path, looked: Option<Stat>) -> Result<[u8; 64], i32> {
    let looked = looked.unwrap_or_else(|| std::fs::metadata(real));
    match files.looked_as(rel, real, looked) {
        Some(meta) if meta.is_dir() => Ok(filestat(ino(rel), FILETYPE_DIRECTORY, 0)),
        Some(meta) => {
            let filetype = if meta.is_file() { FILETYPE_REGULAR_FILE } else { FILETYPE_UNKNOWN };
            Ok(filestat(ino(rel), filetype, meta.len()))
        }
        None => Err(ENOENT),
    }
}

/// Adds to `linker` the functions of WASI's first snapshot that a proc macro has.
pub(crate) fn add(linker: &mut Linker<State>) -> wasmtime::Result<()> {
    linker.func_wrap(
        MODULE,
        "args_sizes_get",
        |mut caller: Caller<'_, State>, argc: i32, size: i32| {
            let memory = memory(&mut caller)?;
            let mem = memory.data_mut(&mut caller);
            let arg = b"proc-macro\0";
            Ok(errno(put(mem, argc, &1u32.to_le_bytes()).and(put(
                mem,
                size,
                &(arg.len() as u32).to_le_bytes(),
            ))))
        },
    )?;
    linker.func_wrap(
        MODULE,
        "args_get",
        |mut caller: Caller<'_, State>, argv: i32, buf: i32| {
            let memory = memory(&mut caller)?;
            let mem = memory.data_mut(&mut caller);
            Ok(errno(put(mem, argv, &buf.to_le_bytes()).and(put(mem, buf, b"proc-macro\0"))))
        },
    )?;
    linker.func_wrap(
        MODULE,
        "environ_sizes_get",
        |mut caller: Caller<'_, State>, count: i32, size: i32| {
            let memory = memory(&mut caller)?;
            let (mem, state) = memory.data_and_store_mut(&mut caller);
            let environ = &state.environ;
            let counted = put(mem, count, &(environ.starts.len() as u32).to_le_bytes());
            Ok(errno(counted.and(put(mem, size, &(environ.text.len() as u32).to_le_bytes()))))
        },
    )?;
    linker.func_wrap(
        MODULE,
        "environ_get",
        |mut caller: Caller<'_, State>, table: i32, buf: i32| {
            let memory = memory(&mut caller)?;
            let (mem, state) = memory.data_and_store_mut(&mut caller);
            Ok(errno(environ_get(mem, &state.environ, table, buf)))
        },
    )?;
    linker.func_wrap(
        MODULE,
        "clock_res_get",
        |mut caller: Caller<'_, State>, _id: i32, out: i32| {
            let memory = memory(&mut caller)?;
            Ok(errno(put(memory.data_mut(&mut caller), out, &1u64.to_le_bytes())))
        },
    )?;
    linker.func_wrap(
        MODULE,
        "clock_time_get",
        |mut caller: Caller<'_, State>, id: i32, _precision: i64, out: i32| {
            let memory = memory(&mut caller)?;
            let (mem, state) = memory.data_and_store_mut(&mut caller);
            state.files.traced.untraceable.push(Untraceable::Clock);
            Ok(errno(put(mem, out, &now(id).to_le_bytes())))
        },
    )?;
    linker.func_wrap(
        MODULE,
        "random_get",
        |mut caller: Caller<'_, State>, buf: i32, len: i32| {
            let memory = memory(&mut caller)?;
            let (mem, state) = memory.data_and_store_mut(&mut caller);
            state.files.traced.untraceable.push(Untraceable::Random);
            let start = buf as u32 as usize;
            let end = start.checked_add(len as u32 as usize);
            let room = end.and_then(|end| mem.get_mut(start..end));
            let Some(room) = room else { return Ok(EINVAL) };
            stdx::rand::fill(room);
            Ok(SUCCESS)
        },
    )?;
    linker.func_wrap(MODULE, "sched_yield", || SUCCESS)?;
    linker.func_wrap(MODULE, "proc_exit", |code: i32| -> wasmtime::Result<()> {
        // alloc: the message of a module that ended its expansion.
        Err(wasmtime::Error::msg(format!("the proc macro exited the process, with status {code}")))
    })?;
    linker.func_wrap(
        MODULE,
        "fd_write",
        |mut caller: Caller<'_, State>, fd: i32, iovs: i32, count: i32, written: i32| {
            let memory = memory(&mut caller)?;
            let (mem, state) = memory.data_and_store_mut(&mut caller);
            if !matches!(state.files.get(fd), Some(Open::Stdio)) {
                return Ok(EBADF);
            }
            let (start, count) = match iovecs(mem, iovs, count) {
                Ok(it) => it,
                Err(e) => return Ok(e),
            };
            let mut total = 0u32;
            for index in 0..count {
                let (ptr, len) = iovec(mem, start, index);
                let Ok(chunk) = bytes(mem, ptr, len) else { return Ok(EINVAL) };
                let room = KEPT_OUTPUT.saturating_sub(state.files.output.len());
                state.files.output.extend_from_slice(&chunk[..chunk.len().min(room)]);
                let Some(more) = total.checked_add(len as u32) else { return Ok(EINVAL) };
                total = more;
            }
            Ok(errno(put(mem, written, &total.to_le_bytes())))
        },
    )?;
    fn read(
        state: &mut State,
        mem: &mut [u8],
        fd: i32,
        iovs: i32,
        count: i32,
        at: Option<u64>,
    ) -> Result<u32, i32> {
        let (start, count) = iovecs(mem, iovs, count)?;
        let (bytes, pos) = match state.files.get(fd) {
            Some(Open::File { bytes, at: pos }) => (bytes, pos),
            Some(Open::Stdio) => return Ok(0),
            Some(Open::Dir { .. }) => return Err(EISDIR),
            None => return Err(EBADF),
        };
        // The pairs as they are before any is filled: a chunk may land on the array itself.
        blaze_mem::scratch::with_u64s(|pairs| {
            pairs.extend((0..count).map(|index| {
                let (ptr, len) = iovec(mem, start, index);
                u64::from(ptr as u32) | u64::from(len as u32) << 32
            }));
            let from = usize::try_from(at.unwrap_or(*pos)).unwrap_or(usize::MAX);
            let mut from = from.min(bytes.len());
            let mut total = 0u32;
            for &pair in pairs.iter() {
                let (ptr, len) = (pair as u32 as i32, (pair >> 32) as u32);
                let end = from.saturating_add(len as usize).min(bytes.len());
                let chunk = &bytes[from..end];
                put(mem, ptr, chunk)?;
                from = end;
                total = total.checked_add(chunk.len() as u32).ok_or(EINVAL)?;
                if chunk.len() < len as usize {
                    break;
                }
            }
            if at.is_none() {
                *pos = from as u64;
            }
            Ok(total)
        })
    }
    linker.func_wrap(
        MODULE,
        "fd_read",
        |mut caller: Caller<'_, State>, fd: i32, iovs: i32, count: i32, out: i32| {
            let memory = memory(&mut caller)?;
            let (mem, state) = memory.data_and_store_mut(&mut caller);
            Ok(match read(state, mem, fd, iovs, count, None) {
                Ok(n) => errno(put(mem, out, &n.to_le_bytes())),
                Err(e) => e,
            })
        },
    )?;
    linker.func_wrap(
        MODULE,
        "fd_pread",
        |mut caller: Caller<'_, State>, fd: i32, iovs: i32, count: i32, at: i64, out: i32| {
            let memory = memory(&mut caller)?;
            let (mem, state) = memory.data_and_store_mut(&mut caller);
            Ok(match read(state, mem, fd, iovs, count, Some(at as u64)) {
                Ok(n) => errno(put(mem, out, &n.to_le_bytes())),
                Err(e) => e,
            })
        },
    )?;
    linker.func_wrap(MODULE, "fd_close", |mut caller: Caller<'_, State>, fd: i32| {
        let files = &mut caller.data_mut().files;
        // The package's directory stays: closing it would leave the module nothing to open.
        if fd <= 3 || files.get(fd).is_none() {
            return if files.get(fd).is_some() { SUCCESS } else { EBADF };
        }
        files.fds[fd as usize] = None;
        SUCCESS
    })?;
    linker.func_wrap(
        MODULE,
        "fd_seek",
        |mut caller: Caller<'_, State>, fd: i32, offset: i64, whence: i32, out: i32| {
            let memory = memory(&mut caller)?;
            let (mem, state) = memory.data_and_store_mut(&mut caller);
            let Some(Open::File { bytes, at }) = state.files.get(fd) else { return Ok(EBADF) };
            let base = match whence {
                0 => 0,
                1 => *at as i64,
                2 => bytes.len() as i64,
                _ => return Ok(EINVAL),
            };
            let Some(new) = base.checked_add(offset).filter(|it| *it >= 0) else {
                return Ok(EINVAL);
            };
            *at = new as u64;
            Ok(errno(put(mem, out, &(new as u64).to_le_bytes())))
        },
    )?;
    linker.func_wrap(MODULE, "fd_tell", |mut caller: Caller<'_, State>, fd: i32, out: i32| {
        let memory = memory(&mut caller)?;
        let (mem, state) = memory.data_and_store_mut(&mut caller);
        let Some(Open::File { at, .. }) = state.files.get(fd) else { return Ok(EBADF) };
        let at = *at;
        Ok(errno(put(mem, out, &at.to_le_bytes())))
    })?;
    linker.func_wrap(
        MODULE,
        "fd_fdstat_get",
        |mut caller: Caller<'_, State>, fd: i32, out: i32| {
            let memory = memory(&mut caller)?;
            let (mem, state) = memory.data_and_store_mut(&mut caller);
            let filetype = match state.files.get(fd) {
                Some(Open::Stdio) => FILETYPE_CHARACTER_DEVICE,
                Some(Open::Dir { .. }) => FILETYPE_DIRECTORY,
                Some(Open::File { .. }) => FILETYPE_REGULAR_FILE,
                None => return Ok(EBADF),
            };
            let mut stat = [0u8; 24];
            stat[0] = filetype;
            stat[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
            stat[16..24].copy_from_slice(&u64::MAX.to_le_bytes());
            Ok(errno(put(mem, out, &stat)))
        },
    )?;
    linker.func_wrap(MODULE, "fd_fdstat_set_flags", |_: i32, _: i32| ENOSYS)?;
    linker.func_wrap(
        MODULE,
        "fd_filestat_get",
        |mut caller: Caller<'_, State>, fd: i32, out: i32| {
            let memory = memory(&mut caller)?;
            let (mem, state) = memory.data_and_store_mut(&mut caller);
            let stat = match state.files.get(fd) {
                Some(Open::Stdio) => filestat(0, FILETYPE_CHARACTER_DEVICE, 0),
                Some(Open::File { bytes, .. }) => {
                    filestat(1, FILETYPE_REGULAR_FILE, bytes.len() as u64)
                }
                Some(Open::Dir { rel, .. }) => filestat(ino(rel), FILETYPE_DIRECTORY, 0),
                None => return Ok(EBADF),
            };
            Ok(errno(put(mem, out, &stat)))
        },
    )?;
    linker.func_wrap(
        MODULE,
        "fd_prestat_get",
        |mut caller: Caller<'_, State>, fd: i32, out: i32| {
            let memory = memory(&mut caller)?;
            let (mem, state) = memory.data_and_store_mut(&mut caller);
            if fd != 3 || state.files.fds.len() < 4 {
                return Ok(EBADF);
            }
            let mut prestat = [0u8; 8];
            prestat[4..].copy_from_slice(&(PACKAGE.len() as u32).to_le_bytes());
            Ok(errno(put(mem, out, &prestat)))
        },
    )?;
    linker.func_wrap(
        MODULE,
        "fd_prestat_dir_name",
        |mut caller: Caller<'_, State>, fd: i32, ptr: i32, len: i32| {
            let memory = memory(&mut caller)?;
            let (mem, state) = memory.data_and_store_mut(&mut caller);
            if fd != 3 || state.files.fds.len() < 4 {
                return Ok(EBADF);
            }
            let name = &PACKAGE.as_bytes()[..PACKAGE.len().min(len as u32 as usize)];
            Ok(errno(put(mem, ptr, name)))
        },
    )?;
    linker.func_wrap(
        MODULE,
        "path_open",
        |mut caller: Caller<'_, State>,
         dirfd: i32,
         _dirflags: i32,
         ptr: i32,
         len: i32,
         oflags: i32,
         rights: i64,
         _inheriting: i64,
         _fdflags: i32,
         out: i32| {
            let memory = memory(&mut caller)?;
            let (mem, state) = memory.data_and_store_mut(&mut caller);
            let files = &mut state.files;
            let opened = (|| {
                let path = text(mem, ptr, len)?;
                let dir = files.dir(dirfd)?;
                let (rel, real, looked) = files.resolve(&dir, &path)?;
                if oflags & (OFLAGS_CREAT | OFLAGS_TRUNC) != 0 || rights & RIGHTS_FD_WRITE != 0 {
                    return Err(EROFS);
                }
                let meta = match looked.unwrap_or_else(|| std::fs::metadata(&real)) {
                    Ok(meta) => meta,
                    Err(error) if missing(&error) => {
                        files.found(&rel, None);
                        return Err(ENOENT);
                    }
                    Err(error) => {
                        files.cant(&real, error);
                        return Err(EIO);
                    }
                };
                if meta.is_dir() {
                    files.found(&rel, Some(DIRECTORY));
                    // alloc: none, an empty listing until the module reads the directory.
                    return Ok(files.insert(Open::Dir { rel, listing: Vec::new() }));
                }
                if oflags & OFLAGS_DIRECTORY != 0 {
                    files.looked_as(&rel, &real, Ok(meta));
                    return Err(ENOTDIR);
                }
                if !meta.is_file() {
                    // A pipe or a device would give what is in it to whoever reads it first,
                    // or never end: a native sandbox lets no proc macro read one either.
                    files.found(&rel, Some(SPECIAL));
                    return Err(ENOTCAPABLE);
                }
                let bytes = match read_whole(&real, meta.len()) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        files.cant(&real, error);
                        return Err(EIO);
                    }
                };
                files.found(&rel, Some(blaze_ids::b3::hash(&bytes)));
                Ok(files.insert(Open::File { bytes, at: 0 }))
            })();
            Ok(match opened {
                Ok(fd) => errno(put(mem, out, &fd.to_le_bytes())),
                Err(e) => e,
            })
        },
    )?;
    linker.func_wrap(
        MODULE,
        "path_filestat_get",
        |mut caller: Caller<'_, State>, dirfd: i32, _flags: i32, ptr: i32, len: i32, out: i32| {
            let memory = memory(&mut caller)?;
            let (mem, state) = memory.data_and_store_mut(&mut caller);
            let files = &mut state.files;
            let found = (|| {
                let path = text(mem, ptr, len)?;
                let dir = files.dir(dirfd)?;
                let (rel, real, looked) = files.resolve(&dir, &path)?;
                stat(files, &rel, &real, looked)
            })();
            Ok(match found {
                Ok(stat) => errno(put(mem, out, &stat)),
                Err(e) => e,
            })
        },
    )?;
    linker.func_wrap(
        MODULE,
        "path_readlink",
        |_: i32, _: i32, _: i32, _: i32, _: i32, _: i32| EINVAL,
    )?;
    linker.func_wrap(
        MODULE,
        "fd_readdir",
        |mut caller: Caller<'_, State>, fd: i32, buf: i32, len: i32, cookie: i64, used: i32| {
            let memory = memory(&mut caller)?;
            let (mem, state) = memory.data_and_store_mut(&mut caller);
            let files = &mut state.files;
            let (rel, mut listing) = match files.take_dir(fd) {
                Ok(it) => it,
                Err(e) => return Ok(e),
            };
            let said = readdir(files, &rel, &mut listing, mem, [buf, len, used], cookie);
            files.give_dir(fd, (rel, listing));
            Ok(said)
        },
    )?;
    Ok(())
}

/// `fd_readdir` of the package's directory `rel`: its entries from `cookie` on, sorted by
/// name, into the module's `buf` of `len` bytes, and how many bytes they took at `used`.
/// `listing` is what the descriptor's last pass found: a pass starts at cookie 0 and lists
/// the directory once, and the calls that go on from a later cookie read it from there.
fn readdir(
    files: &mut Files,
    rel: &str,
    listing: &mut Vec<(OsString, u8)>,
    mem: &mut [u8],
    [buf, len, used]: [i32; 3],
    cookie: i64,
) -> i32 {
    if cookie == 0 || listing.is_empty() {
        let real = files.root.join(rel);
        listing.clear();
        if let Err(error) = list(&real, listing) {
            files.cant(&real, error);
            return EIO;
        }
        // Recorded with every name, as a native sandbox's listing is, though a name that is
        // not UTF-8 can't be given to the module.
        files.listed(rel, listing.iter().map(|(name, _)| name.as_os_str()));
        // An `OsString` sorts by its bytes, as a `String` of the same text does.
        listing.sort_unstable_by(|(one, _), (two, _)| one.cmp(two));
    }
    let len = len as u32 as usize;
    blaze_mem::scratch::with_bytes(|out| {
        let names = listing.iter().filter_map(|(name, filetype)| Some((name.to_str()?, *filetype)));
        for (index, (name, filetype)) in names.enumerate().skip(cookie as u64 as usize) {
            let ino = match rel.is_empty() {
                true => ino(name),
                false => ino_of(&[rel.as_bytes(), b"/", name.as_bytes()]),
            };
            out.extend_from_slice(&(index as u64 + 1).to_le_bytes());
            out.extend_from_slice(&ino.to_le_bytes());
            out.extend_from_slice(&(name.len() as u32).to_le_bytes());
            out.extend_from_slice(&[filetype, 0, 0, 0]);
            out.extend_from_slice(name.as_bytes());
            if out.len() >= len {
                break;
            }
        }
        out.truncate(len);
        errno(put(mem, buf, out).and(put(mem, used, &(out.len() as u32).to_le_bytes())))
    })
}

/// The entries of the directory at `real`, each name with the type of what it names, into
/// `listing`, in the order the system gives them. The type is the listing's own (`d_type`),
/// with no `stat` of the entry, but for a link, whose type is what it leads to.
fn list(real: &Path, listing: &mut Vec<(OsString, u8)>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(real)? {
        let entry = entry?;
        let ty = match entry.file_type() {
            Ok(ty) if ty.is_symlink() => std::fs::metadata(entry.path()).map(|it| it.file_type()),
            ty => ty,
        };
        let filetype = match ty {
            Ok(ty) if ty.is_dir() => FILETYPE_DIRECTORY,
            Ok(ty) if ty.is_file() => FILETYPE_REGULAR_FILE,
            _ => FILETYPE_UNKNOWN,
        };
        // alloc: the edge: the names the system lists, which the descriptor keeps for the
        // rest of its pass.
        listing.push((entry.file_name(), filetype));
    }
    Ok(())
}

/// The bytes of the file at `real`, which its `stat` said has `len`: one buffer of that size,
/// read to its end, so an open, one read and the read that finds the end, with no second
/// `stat` or seek as [`std::fs::read`] and `File::read_to_end` make. A file that grew since is
/// still read whole.
fn read_whole(real: &Path, len: u64) -> std::io::Result<Vec<u8>> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(real)?;
    // One byte past the size, so the read that finds the end needs no more room.
    let room = usize::try_from(len).unwrap_or(0).saturating_add(1);
    // alloc: the edge: the file's bytes, which the descriptor keeps for the module's reads.
    let mut bytes = vec![0u8; room];
    let mut filled = 0;
    loop {
        if filled == bytes.len() {
            bytes.resize(2 * filled, 0);
        }
        match file.read(&mut bytes[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    bytes.truncate(filled);
    Ok(bytes)
}

/// Adds to `linker`, for each function of WASI's that `module` imports and [`add`] does not
/// have, one that answers `ENOSYS`.
pub(crate) fn rest(linker: &mut Linker<State>, module: &Module) -> wasmtime::Result<()> {
    let mut store = wasmtime::Store::new(module.engine(), State::empty());
    for import in module.imports() {
        if import.module() != MODULE {
            continue;
        }
        let Some(ty) = import.ty().func().cloned() else { continue };
        if linker.get_by_import(&mut store, &import).is_some() {
            continue;
        }
        // alloc: setup, once per module linked: the results the stand-in answers.
        let results: Vec<ValType> = ty.results().collect();
        // clone: setup, once per module linked: the type keeps its own results.
        let ty = FuncType::new(module.engine(), ty.params(), results.clone());
        linker.func_new(MODULE, import.name(), ty, move |_, _, out| {
            for (slot, ty) in out.iter_mut().zip(&results) {
                *slot = match ty {
                    ValType::I64 => Val::I64(ENOSYS as i64),
                    _ => Val::I32(ENOSYS),
                };
            }
            Ok(())
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("blaze-wasi-{name}-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// What `resolve` said of a path of no `.` or `..` before it looked for links part by
    /// part: the whole path resolved, and the package's own root resolved, each time.
    fn resolve_by_realpath(root: &Path, path: &str) -> Result<String, i32> {
        if let Ok(found) = root.join(path).canonicalize() {
            let root = root.canonicalize().map_err(|_| ENOENT)?;
            if !found.starts_with(&root) {
                return Err(ENOTCAPABLE);
            }
        }
        Ok(path.trim_end_matches('/').to_owned())
    }

    #[cfg(unix)]
    #[test]
    fn a_path_is_in_the_package_as_resolving_all_of_it_says_with_its_stat_as_a_stats() {
        use std::os::unix::fs::symlink;
        let dir = temp("links");
        let package = dir.join("package");
        std::fs::create_dir_all(package.join("src")).unwrap();
        std::fs::create_dir_all(package.join("sub")).unwrap();
        std::fs::write(package.join("a.txt"), "a").unwrap();
        std::fs::write(package.join("src/lib.rs"), "fn f() {}").unwrap();
        std::fs::write(package.join("sub/x.txt"), "x").unwrap();
        std::fs::write(dir.join("outside.txt"), "o").unwrap();
        symlink(package.join("a.txt"), package.join("in_file")).unwrap();
        symlink("sub", package.join("in_dir")).unwrap();
        symlink(dir.join("outside.txt"), package.join("out_file")).unwrap();
        symlink(&dir, package.join("out_dir")).unwrap();
        symlink(package.join("missing"), package.join("dangling")).unwrap();
        symlink(&package, dir.join("package_link")).unwrap();
        let paths = [
            "", "a.txt", "src", "src/lib.rs", "missing", "src/missing", "a.txt/x", "in_file",
            "in_dir", "in_dir/x.txt", "out_file", "out_dir", "out_dir/outside.txt", "dangling",
        ];
        // The package as it is, and through a link to it: its own path's links are its own.
        for root in [package.clone(), dir.join("package_link")] {
            let files = Files::new(Some(&root));
            for path in paths {
                let now = files.resolve("", path).map(|(rel, real, looked)| {
                    if let Some(looked) = looked {
                        match (looked, std::fs::metadata(&real)) {
                            (Ok(one), Ok(two)) => assert_eq!(
                                (one.is_dir(), one.is_file(), one.len()),
                                (two.is_dir(), two.is_file(), two.len()),
                                "{path}"
                            ),
                            (Err(one), Err(two)) => assert_eq!(one.kind(), two.kind(), "{path}"),
                            (one, two) => panic!("{path}: {one:?} where a stat says {two:?}"),
                        }
                    }
                    rel
                });
                assert_eq!(now, resolve_by_realpath(&root, path), "{path} from {}", root.display());
            }
        }
        _ = std::fs::remove_dir_all(&dir);
    }

    /// What `fd_readdir` gave before a pass kept its listing: the directory listed, sorted and
    /// each entry's type looked up, at every call.
    fn readdir_per_call(root: &Path, rel: &str, cookie: usize, len: usize) -> Vec<u8> {
        let real = root.join(rel);
        let mut all: Vec<OsString> =
            std::fs::read_dir(&real).unwrap().map(|it| it.unwrap().file_name()).collect();
        all.sort_unstable();
        let mut out = Vec::new();
        for (index, name) in all.iter().filter_map(|it| it.to_str()).enumerate().skip(cookie) {
            let filetype = match std::fs::metadata(real.join(name)) {
                Ok(meta) if meta.is_dir() => FILETYPE_DIRECTORY,
                Ok(meta) if meta.is_file() => FILETYPE_REGULAR_FILE,
                _ => FILETYPE_UNKNOWN,
            };
            let ino = match rel.is_empty() {
                true => ino(name),
                false => ino_of(&[rel.as_bytes(), b"/", name.as_bytes()]),
            };
            out.extend_from_slice(&(index as u64 + 1).to_le_bytes());
            out.extend_from_slice(&ino.to_le_bytes());
            out.extend_from_slice(&(name.len() as u32).to_le_bytes());
            out.extend_from_slice(&[filetype, 0, 0, 0]);
            out.extend_from_slice(name.as_bytes());
            if out.len() >= len {
                break;
            }
        }
        out.truncate(len);
        out
    }

    #[test]
    fn a_pass_over_a_directory_lists_it_once_and_gives_what_a_listing_per_call_gave() {
        let dir = temp("readdir");
        let package = dir.join("package");
        std::fs::create_dir_all(package.join("src/sub")).unwrap();
        for name in ["b.rs", "a.rs", "c d.txt", "lib.rs"] {
            std::fs::write(package.join("src").join(name), name).unwrap();
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("sub", package.join("src/link")).unwrap();
            std::os::unix::fs::symlink("missing", package.join("src/dangling")).unwrap();
        }
        let mut files = Files::new(Some(&package));
        let mut mem = vec![0u8; 4096];
        let used = 4000;
        let mut passes = 0;
        for rel in ["src", ""] {
            for len in [30, 70, 1000] {
                let mut listing = Vec::new();
                passes += 1;
                for cookie in 0..8 {
                    let said =
                        readdir(&mut files, rel, &mut listing, &mut mem, [0, len, used], cookie);
                    assert_eq!(said, SUCCESS);
                    let at = used as usize;
                    let n = u32::from_le_bytes(mem[at..at + 4].try_into().unwrap()) as usize;
                    let want = readdir_per_call(&package, rel, cookie as usize, len as usize);
                    assert_eq!(mem[..n], want[..], "{rel:?} from {cookie} into {len} bytes");
                }
                assert_eq!(files.traced.dirs.len(), passes, "a pass lists its directory once");
            }
        }
        // A call that goes on from a later cookie reads the pass's listing and allocates nothing.
        let mut listing = Vec::new();
        readdir(&mut files, "src", &mut listing, &mut mem, [0, 30, used], 0);
        let said = blaze_alloc_count::assert_at_most(0, "a readdir that goes on with its pass", || {
            readdir(&mut files, "src", &mut listing, &mut mem, [0, 30, used], 2)
        });
        assert_eq!(said, SUCCESS);
        assert!(files.unreadable.is_none());
        _ = std::fs::remove_dir_all(&dir);
    }

    /// What `environ_get` wrote before the text was made once an instance: each variable's
    /// pointer, name, `=`, value and NUL in turn.
    fn environ_per_variable(mem: &mut [u8], env: &[(&str, &str)], table: i32, buf: i32) -> i32 {
        let (table, mut at) = (table as u32, buf as u32);
        for (index, (key, value)) in env.iter().enumerate() {
            let slot = table + 4 * index as u32;
            let (equals, value_at) = (at + key.len() as u32, at + key.len() as u32 + 1);
            let nul = value_at + value.len() as u32;
            let written = put(mem, slot as i32, &at.to_le_bytes())
                .and(put(mem, at as i32, key.as_bytes()))
                .and(put(mem, equals as i32, b"="))
                .and(put(mem, value_at as i32, value.as_bytes()))
                .and(put(mem, nul as i32, b"\0"));
            if let Err(e) = written {
                return e;
            }
            at = nul + 1;
        }
        SUCCESS
    }

    #[test]
    fn the_environment_is_written_as_one_variable_at_a_time_wrote_it() {
        let envs: [&[(&str, &str)]; 3] = [
            &[],
            &[("CARGO_MANIFEST_DIR", PACKAGE), ("CARGO_PKG_NAME", "pkg"), ("EMPTY", "")],
            &[("OUT_DIR", OUT_DIR)],
        ];
        for env in envs {
            let environ = crate::Environ::of(env);
            let total: usize = env.iter().map(|(k, v)| k.len() + v.len() + 2).sum();
            assert_eq!((environ.starts.len(), environ.text.len()), (env.len(), total));
            for (table, buf) in [(0, 64), (400, 16), (100, 200), (510, 0), (0, 500), (-4, 0)] {
                let (mut one, mut two) = (vec![0u8; 512], vec![0u8; 512]);
                let want = environ_per_variable(&mut one, env, table, buf);
                let got = errno(environ_get(&mut two, &environ, table, buf));
                assert_eq!(got, want, "{env:?} at {table}, {buf}");
                if want == SUCCESS {
                    assert_eq!(one, two, "{env:?} at {table}, {buf}");
                }
            }
        }
    }

    #[test]
    fn a_file_is_read_whole_in_one_buffer_whatever_its_stat_said() {
        let dir = temp("read");
        let path = dir.join("f");
        for size in [0usize, 1, 31, 4096, 70_000] {
            let bytes: Vec<u8> = (0..size).map(|it| (it * 7) as u8).collect();
            std::fs::write(&path, &bytes).unwrap();
            let read = blaze_alloc_count::assert_at_most(1, "a file read whole", || {
                read_whole(&path, size as u64)
            });
            assert_eq!(read.unwrap(), bytes);
            for said in [0, size as u64 / 2, 2 * size as u64 + 1] {
                assert_eq!(read_whole(&path, said).unwrap(), bytes, "{size} bytes, said {said}");
            }
        }
        _ = std::fs::remove_dir_all(&dir);
    }
}

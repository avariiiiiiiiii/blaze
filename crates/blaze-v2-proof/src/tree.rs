//! The digest of a source tree, which `harness/v2/engine.py`'s `tree_digest` asks for: every
//! regular file's path under the root, whether it is executable and the BLAKE3 hash of its
//! bytes, then every symbolic link's path and target, each in path order, with the directories
//! that `skip` names at the top left out. The bytes hashed are those the harness's own walk
//! hashed (`os.walk` and `lstat`), so a digest kept by an older run still compares equal.
//!
//! Directories are listed on every core, each one a task, and the type of each entry comes from
//! the listing (`getdents64`'s `d_type`), so only files are opened and nothing else is `stat`ed.
//! Files are hashed in place, in tasks of up to [`FILES_A_TASK`]: one read into a buffer the
//! thread reuses below [`MAPPED_FROM`], mapped from there.

use std::{
    cell::RefCell,
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    sync::Mutex,
};

use blaze_ids::b3;

use crate::bytes::Mapped;

pub const DOMAIN: &[u8] = b"blaze v2 proof tree 2\0";

/// Files this size or larger are mapped, not read.
pub const MAPPED_FROM: u64 = 1 << 20;

/// The most files one task hashes, so a directory of many is hashed on many cores.
const FILES_A_TASK: usize = 64;

enum Entry {
    File { rel: String, executable: bool, hash: [u8; 32] },
    Link { rel: String, to: String },
}

impl Entry {
    /// Files before links, each by its path, as the harness's walk wrote them.
    fn order(&self) -> (bool, &[u8]) {
        match self {
            Entry::File { rel, .. } => (false, rel.as_bytes()),
            Entry::Link { rel, .. } => (true, rel.as_bytes()),
        }
    }
}

struct Found {
    entries: Mutex<Vec<Entry>>,
    error: Mutex<Option<io::Error>>,
}

impl Found {
    fn add(&self, mut entries: Vec<Entry>) {
        self.entries.lock().expect("a walk's task panicked").append(&mut entries);
    }

    fn fail(&self, error: io::Error) {
        self.error.lock().expect("a walk's task panicked").get_or_insert(error);
    }
}

/// The digest of the tree at `root`, the directories named in `skip` at its top left out. An
/// entry that can't be read, or a name or link target that is not UTF-8, is an error naming it.
pub fn digest(root: &Path, skip: &[String]) -> io::Result<[u8; 32]> {
    let found = Found {
        // alloc: once a digest
        entries: Mutex::new(Vec::new()),
        error: Mutex::new(None),
    };
    // alloc: the walk's paths, once a directory
    rayon::scope(|scope| walk(scope, root.to_path_buf(), String::new(), skip, &found));
    if let Some(error) = found.error.into_inner().expect("a walk's task panicked") {
        return Err(error);
    }
    let mut entries = found.entries.into_inner().expect("a walk's task panicked");
    entries.sort_unstable_by(|a, b| a.order().cmp(&b.order()));
    let mut hasher = b3::Hasher::new();
    hasher.update(DOMAIN);
    // alloc: once a digest, reused for every line
    let mut line = Vec::with_capacity(256);
    for entry in &entries {
        line.clear();
        match entry {
            Entry::File { rel, executable, hash } => {
                line.extend_from_slice(b"f\0");
                line.extend_from_slice(rel.as_bytes());
                line.extend_from_slice(if *executable { b"\x001\0" } else { b"\x000\0" });
                hex(hash, &mut line);
            }
            Entry::Link { rel, to } => {
                line.extend_from_slice(b"l\0");
                line.extend_from_slice(rel.as_bytes());
                line.push(0);
                line.extend_from_slice(to.as_bytes());
            }
        }
        line.push(b'\n');
        hasher.update(&line);
    }
    Ok(hasher.finalize())
}

/// `hash` as 64 lowercase hex digits, pushed to `out`.
pub fn hex(hash: &[u8; 32], out: &mut Vec<u8>) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    for &byte in hash {
        out.push(DIGITS[usize::from(byte >> 4)]);
        out.push(DIGITS[usize::from(byte & 15)]);
    }
}

fn walk<'s>(scope: &rayon::Scope<'s>, dir: PathBuf, rel: String, skip: &'s [String], found: &'s Found) {
    if let Err(error) = list(scope, &dir, &rel, skip, found) {
        found.fail(error);
    }
}

fn list<'s>(scope: &rayon::Scope<'s>, dir: &Path, rel: &str, skip: &'s [String], found: &'s Found) -> io::Result<()> {
    // alloc: per directory, its links
    let mut links = Vec::new();
    // alloc: per directory, its files until a task takes them
    let mut files: Vec<(PathBuf, String)> = Vec::new();
    for entry in fs::read_dir(dir).map_err(|e| named(dir, e))? {
        let entry = entry.map_err(|e| named(dir, e))?;
        let path = entry.path();
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            // alloc: an error
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{}: a name that is not UTF-8", path.display())));
        };
        let kind = entry.file_type().map_err(|e| named(&path, e))?;
        // `os.walk` lists a link to a directory with the directories, so `skip` takes it too.
        if rel.is_empty()
            && skip.iter().any(|it| it == name)
            && (kind.is_dir() || (kind.is_symlink() && fs::metadata(&path).is_ok_and(|it| it.is_dir())))
        {
            continue;
        }
        // alloc: per entry, its path under the root
        let child = if rel.is_empty() { name.to_owned() } else { format!("{rel}/{name}") };
        if kind.is_symlink() {
            let to = fs::read_link(&path).map_err(|e| named(&path, e))?;
            let Some(to) = to.to_str() else {
                // alloc: an error
                return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{}: a link target that is not UTF-8", path.display())));
            };
            // alloc: per link, its target
            links.push(Entry::Link { rel: child, to: to.to_owned() });
        } else if kind.is_dir() {
            scope.spawn(move |scope| walk(scope, path, child, skip, found));
        } else if kind.is_file() {
            files.push((path, child));
            if files.len() == FILES_A_TASK {
                let taken = std::mem::take(&mut files);
                scope.spawn(move |_| hash_files(taken, found));
            }
        }
    }
    if !files.is_empty() {
        scope.spawn(move |_| hash_files(files, found));
    }
    if !links.is_empty() {
        found.add(links);
    }
    Ok(())
}

fn hash_files(files: Vec<(PathBuf, String)>, found: &Found) {
    // alloc: per task, its entries
    let mut entries = Vec::with_capacity(files.len());
    for (path, rel) in files {
        match hash_file(&path) {
            Ok((executable, hash)) => entries.push(Entry::File { rel, executable, hash }),
            Err(error) => return found.fail(named(&path, error)),
        }
    }
    found.add(entries);
}

thread_local! {
    // alloc: once a thread, grown to the largest file it reads whole
    static BUFFER: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Whether the file is executable, and the hash of its bytes.
pub fn hash_file(path: &Path) -> io::Result<(bool, [u8; 32])> {
    let mut file = fs::File::open(path)?;
    let meta = file.metadata()?;
    let executable = executable(path, &meta);
    if meta.len() >= MAPPED_FROM {
        return Ok((executable, b3::hash(Mapped::open(path)?.bytes())));
    }
    BUFFER.with_borrow_mut(|buffer| {
        buffer.clear();
        file.read_to_end(buffer)?;
        Ok((executable, b3::hash(buffer)))
    })
}

#[cfg(unix)]
fn executable(_: &Path, meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

/// What Python's `lstat` says of a file on Windows: executable by its extension.
#[cfg(not(unix))]
fn executable(path: &Path, _: &fs::Metadata) -> bool {
    path.extension()
        .and_then(|it| it.to_str())
        .is_some_and(|it| ["exe", "bat", "cmd", "com"].iter().any(|ext| it.eq_ignore_ascii_case(ext)))
}

fn named(path: &Path, error: io::Error) -> io::Error {
    // alloc: an error
    io::Error::new(error.kind(), format!("{}: {error}", path.display()))
}

#[cfg(test)]
mod tests;

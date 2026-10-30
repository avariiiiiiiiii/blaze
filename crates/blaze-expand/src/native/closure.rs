//! Each library's closure as its description and its packages' files on disk say it now, kept
//! between calls as git keeps its index: a description by its file's path and stamp, every
//! file's hash by its path and stamp, and a description's last snapshot of its packages with the
//! stamps of every file and directory in them. Asking again of an unchanged library reads no
//! file, takes no lock, writes no shared memory and allocates nothing: it is one `stat` of the
//! description and one of each directory and file, each compared with the snapshot's column.
//!
//! A stamp is trusted only once nothing changed the file for longer than the coarsest clock a
//! file system stamps with ([`SETTLE_NS`]): git's rule for racily clean entries. Until then the
//! file is read again on every call and what was read of it is not kept.
//!
//! The tables are content-addressed and append-only ([`blaze_ids::Sharded`]): a reader probes
//! them with plain loads, and a new entry is published once. A description's last snapshot is
//! the one value that changes: a `u32` written when a snapshot is made, never on a hit.

use std::{
    cell::RefCell,
    io,
    path::{Path, PathBuf},
    sync::{
        OnceLock,
        atomic::{AtomicU32, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use blaze_ids::{ContentId, Hasher, Idx, Interner, Sharded};
use paths::AbsPath;

use super::{Closure, Describe, PACKAGES, Package, ROOTS};

/// A closure [`of_library`] found: kept for the next call, or made of files that changed too
/// lately to be kept.
pub(super) enum Held {
    Kept(&'static Closure),
    Fresh(Box<Closure>),
}

impl std::ops::Deref for Held {
    type Target = Closure;

    fn deref(&self) -> &Closure {
        match self {
            Held::Kept(it) => it,
            Held::Fresh(it) => it,
        }
    }
}

/// The closure of the library at `dylib`, from the description `describe` names, or `None`
/// when it names none or its first crate is none blaze verified.
pub(super) fn of_library(dylib: &AbsPath, describe: &Describe) -> Result<Option<Held>, String> {
    thread_local! {
        static PATH: RefCell<PathBuf> = RefCell::new(PathBuf::with_capacity(512));
    }
    PATH.with_borrow_mut(|path| {
        path.as_mut_os_string().clear();
        if !describe(dylib, path)? {
            return Ok(None);
        }
        closure_at(path)
    })
}

/// The closure the `rust-project.json` at `path` describes, as [`of_library`] has it. `path`
/// is left as a path of the last file looked at.
pub(super) fn closure_at(path: &mut PathBuf) -> Result<Option<Held>, String> {
    match description_at(path)? {
        Description::Kept(described) => match &described.packages {
            Some(packages) => kept_closure(described, packages, path).map(Some),
            None => Ok(None),
        },
        Description::Fresh(described) => match described.packages {
            Some(packages) => {
                let (snapshot, _) = snapshot_of(&packages, path)?;
                Ok(Some(Held::Fresh(Box::new(snapshot.closure))))
            }
            None => Ok(None),
        },
    }
}

/// What `stat` says of a file or directory, which changes when its bytes or its entries do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Stamp {
    len: u64,
    inode: u64,
    modified_ns: i64,
    changed_ns: i64,
}

const _: () = assert!(std::mem::size_of::<Stamp>() == 32);

/// How long after a file's last change its stamp is trusted to change with its bytes: longer
/// than the coarsest clock a file system stamps with (FAT's two seconds; ext4 stamps with the
/// kernel's coarse clock, a few milliseconds).
#[cfg(not(test))]
const SETTLE_NS: i64 = 2_000_000_001;
/// In tests, which write files and ask of them at once, and change a file's length when they
/// change it, so that its stamp changes on any clock.
#[cfg(test)]
const SETTLE_NS: i64 = 0;

impl Stamp {
    fn of(meta: &std::fs::Metadata) -> Stamp {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            let ns = |secs: i64, nanos: i64| secs.saturating_mul(1_000_000_000).saturating_add(nanos);
            Stamp {
                len: meta.size(),
                inode: meta.ino(),
                modified_ns: ns(meta.mtime(), meta.mtime_nsec()),
                changed_ns: ns(meta.ctime(), meta.ctime_nsec()),
            }
        }
        #[cfg(not(unix))]
        {
            let modified = meta.modified().map_or(0, ns_since_epoch);
            Stamp { len: meta.len(), inode: 0, modified_ns: modified, changed_ns: modified }
        }
    }

    fn bytes(&self) -> [u8; 32] {
        let mut out = [0; 32];
        out[..8].copy_from_slice(&self.len.to_le_bytes());
        out[8..16].copy_from_slice(&self.inode.to_le_bytes());
        out[16..24].copy_from_slice(&self.modified_ns.to_le_bytes());
        out[24..].copy_from_slice(&self.changed_ns.to_le_bytes());
        out
    }

    /// Whether it was settled when what it stamps was read at `read_at`.
    fn settled(&self, read_at: i64) -> bool {
        self.modified_ns.max(self.changed_ns) < read_at.saturating_sub(SETTLE_NS)
    }
}

fn ns_since_epoch(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH).map_or(0, |it| it.as_nanos().min(i64::MAX as u128) as i64)
}

/// Now, as a stamp's clock has it: before the epoch, 0, so that nothing read is settled.
fn now_ns() -> i64 {
    ns_since_epoch(SystemTime::now())
}

const DESCRIPTION: &str = "blaze v2 native description 1";
const FILE: &str = "blaze v2 native file 1";
const SNAPSHOT: &str = "blaze v2 native snapshot 1";

/// A `rust-project.json` as read, kept by its path and stamp.
struct Described {
    id: ContentId,
    /// Its crates, `None` when its first is none blaze verified.
    packages: Option<Vec<DescribedPackage>>,
    /// Its last snapshot, as its index in [`snapshots`] plus one; 0 before the first.
    last: AtomicU32,
}

enum Description {
    Kept(&'static Described),
    /// Of a file that changed too lately to be kept.
    Fresh(Described),
}

/// A crate of a description, before its package's files are read.
struct DescribedPackage {
    name: String,
    version: String,
    dir: PathBuf,
    cfg: Vec<String>,
    deps: Vec<(String, usize)>,
}

/// A description's packages' files at one moment. Columns: the stamps of each package's files
/// (in `closure`'s order) then of its directories, package `p`'s at `starts[p]..starts[p + 1]`;
/// its directories' paths in `dirs`, at `dir_starts[p]..dir_starts[p + 1]`, its own first
/// (`""`). 32 bytes of stamp per file: a hit is a walk down the column.
struct Snapshot {
    closure: Closure,
    stamps: Vec<Stamp>,
    starts: Vec<u32>,
    dirs: Vec<Box<str>>,
    dir_starts: Vec<u32>,
}

fn descriptions() -> &'static Sharded<Described> {
    static TABLE: OnceLock<Sharded<Described>> = OnceLock::new();
    TABLE.get_or_init(Sharded::new)
}

fn snapshots() -> &'static Sharded<Snapshot> {
    static TABLE: OnceLock<Sharded<Snapshot>> = OnceLock::new();
    TABLE.get_or_init(Sharded::new)
}

/// The BLAKE3 of each file, by its path and settled stamp.
fn hashes() -> &'static Sharded<[u8; 32]> {
    static TABLE: OnceLock<Sharded<[u8; 32]>> = OnceLock::new();
    TABLE.get_or_init(Sharded::new)
}

fn keyed(domain: &'static str, path: &Path, stamp: &Stamp) -> ContentId {
    Hasher::new(domain).part(path.as_os_str().as_encoded_bytes()).part(&stamp.bytes()).finish()
}

fn reading(path: &Path, error: io::Error) -> String {
    format!("reading {}: {error}", path.display())
}

fn description_at(path: &Path) -> Result<Description, String> {
    let read_at = now_ns();
    let stamp = Stamp::of(&std::fs::metadata(path).map_err(|e| reading(path, e))?);
    let id = keyed(DESCRIPTION, path, &stamp);
    let table = descriptions();
    if let Some(idx) = table.find(&id) {
        return Ok(Description::Kept(table.get(idx)));
    }
    let text = std::fs::read_to_string(path).map_err(|e| reading(path, e))?;
    let packages = described(&text)
        .map_err(|why| format!("the description at {}: {why}", path.display()))?;
    let described = Described { id, packages, last: AtomicU32::new(0) };
    if !stamp.settled(read_at) {
        return Ok(Description::Fresh(described));
    }
    Ok(Description::Kept(table.get(table.intern(id, || described))))
}

/// The crates of the proc-macro crate a `rust-project.json` describes first, as blaze writes one
/// for the crate's module (`load-cargo/src/described.rs`): each crate's package is the
/// directory its `CARGO_MANIFEST_DIR` names. `None` when the crate is none blaze verified.
fn described(text: &str) -> Result<Option<Vec<DescribedPackage>>, String> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("the description is not JSON: {e}"))?;
    let crates = value["crates"].as_array().ok_or("the description has no crates")?;
    let first = crates.first().ok_or("the description is of no crate")?;
    let first = DescribedPackage::of(first).map_err(|why| format!("its crate 0: {why}"))?;
    let verified = ROOTS.iter().any(|it| {
        let package = &PACKAGES[it.package];
        package.name == first.name && package.version == first.version
    });
    if !verified {
        return Ok(None);
    }
    let mut packages = Vec::with_capacity(crates.len());
    packages.push(first);
    for (at, it) in crates.iter().enumerate().skip(1) {
        packages.push(DescribedPackage::of(it).map_err(|why| format!("its crate {at}: {why}"))?);
    }
    Ok(Some(packages))
}

impl DescribedPackage {
    fn of(value: &serde_json::Value) -> Result<DescribedPackage, String> {
        let env = |key: &str| {
            value["env"][key].as_str().map(str::to_owned).ok_or_else(|| format!("no {key}"))
        };
        let dir = PathBuf::from(env("CARGO_MANIFEST_DIR")?);
        if !dir.is_absolute() {
            return Err(format!("its CARGO_MANIFEST_DIR {} is not absolute", dir.display()));
        }
        let atoms = value["cfg"].as_array().ok_or("no cfg")?;
        let mut cfg = Vec::with_capacity(atoms.len());
        for atom in atoms {
            cfg.push(atom.as_str().ok_or("a cfg that is not a string")?.to_owned());
        }
        cfg.sort_unstable();
        cfg.dedup();
        let listed = value["deps"].as_array().ok_or("no deps")?;
        let mut deps = Vec::with_capacity(listed.len());
        for dep in listed {
            let at = dep["crate"].as_u64().ok_or("a dependency that names no crate")?;
            let name = dep["name"].as_str().ok_or("a dependency without a name")?;
            deps.push((name.to_owned(), at as usize));
        }
        deps.sort_unstable();
        Ok(DescribedPackage {
            name: env("CARGO_PKG_NAME")?,
            version: env("CARGO_PKG_VERSION")?,
            dir,
            cfg,
            deps,
        })
    }
}

fn kept_closure(
    described: &'static Described,
    packages: &[DescribedPackage],
    path: &mut PathBuf,
) -> Result<Held, String> {
    let table = snapshots();
    if let Some(last) = described.last.load(Ordering::Acquire).checked_sub(1) {
        let old = table.get(Idx::from_raw(last));
        if unchanged(old, packages, path) {
            return Ok(Held::Kept(&old.closure));
        }
    }
    let (snapshot, settled) = snapshot_of(packages, path)?;
    if !settled {
        return Ok(Held::Fresh(Box::new(snapshot.closure)));
    }
    let id = snapshot.id(&described.id);
    let idx = table.intern(id, || snapshot);
    described.last.store(idx.raw() + 1, Ordering::Release);
    Ok(Held::Kept(&table.get(idx).closure))
}

fn join(path: &mut PathBuf, dir: &Path, rel: &str) {
    path.as_mut_os_string().clear();
    path.push(dir);
    if !rel.is_empty() {
        path.push(rel);
    }
}

/// Whether every file and directory `old` was made of has the stamp it had.
fn unchanged(old: &Snapshot, packages: &[DescribedPackage], path: &mut PathBuf) -> bool {
    for (p, package) in packages.iter().enumerate() {
        let files = old.closure.packages[p].files.iter().map(|it| it.0.as_str());
        let dirs = old.dirs[old.dir_starts[p] as usize..old.dir_starts[p + 1] as usize].iter();
        let stamps = &old.stamps[old.starts[p] as usize..old.starts[p + 1] as usize];
        for (rel, stamp) in files.chain(dirs.map(|it| &**it)).zip(stamps) {
            join(path, &package.dir, rel);
            match std::fs::symlink_metadata(&*path) {
                Ok(meta) if Stamp::of(&meta) == *stamp => {}
                _ => return false,
            }
        }
    }
    true
}

enum Kind {
    File,
    /// A link or another kind of file, which no published package has.
    Other,
}

/// A snapshot of `packages`' files as they are now, and whether every one was settled.
fn snapshot_of(
    packages: &[DescribedPackage],
    path: &mut PathBuf,
) -> Result<(Snapshot, bool), String> {
    let read_at = now_ns();
    let mut settled = true;
    let mut closure = Vec::with_capacity(packages.len());
    let mut stamps = Vec::new();
    let mut starts = Vec::with_capacity(packages.len() + 1);
    let mut dirs = Vec::new();
    let mut dir_starts = Vec::with_capacity(packages.len() + 1);
    let (mut listed, mut listed_dirs) = (Vec::new(), Vec::new());
    for package in packages {
        let dir = &package.dir;
        let meta = std::fs::symlink_metadata(dir).map_err(|e| reading(dir, e))?;
        if !meta.is_dir() {
            return Err(format!("the package directory {} is not a directory", dir.display()));
        }
        listed_dirs.push((Box::<str>::from(""), Stamp::of(&meta)));
        walk(dir, "", &mut listed, &mut listed_dirs)?;
        listed.sort_unstable_by(|a: &(String, Stamp, Kind), b| a.0.cmp(&b.0));
        starts.push(stamps.len() as u32);
        dir_starts.push(dirs.len() as u32);
        let mut files = Vec::with_capacity(listed.len());
        for (rel, stamp, kind) in listed.drain(..) {
            settled &= stamp.settled(read_at);
            join(path, dir, &rel);
            let hash = match kind {
                Kind::File => file_hash(path, &stamp, read_at)?,
                Kind::Other => {
                    let target = std::fs::read_link(&*path).unwrap_or_default();
                    let said = format!("\0not a file: {}", target.display());
                    *blake3::hash(said.as_bytes()).as_bytes()
                }
            };
            stamps.push(stamp);
            files.push((rel, hash));
        }
        for (rel, stamp) in listed_dirs.drain(..) {
            settled &= stamp.settled(read_at);
            stamps.push(stamp);
            dirs.push(rel);
        }
        closure.push(Package {
            name: package.name.clone(),
            version: package.version.clone(),
            files,
            cfg: package.cfg.clone(),
            deps: package.deps.clone(),
        });
    }
    starts.push(stamps.len() as u32);
    dir_starts.push(dirs.len() as u32);
    let snapshot = Snapshot { closure: Closure::new(closure), stamps, starts, dirs, dir_starts };
    Ok((snapshot, settled))
}

/// Every file of the package directory `dir` under `prefix` (but what cargo writes beside a
/// package it unpacks, `.cargo-ok` and `.cargo-checksum.json`), by its path from the package's
/// directory with `/` between components, and every directory under it.
fn walk(
    dir: &Path,
    prefix: &str,
    files: &mut Vec<(String, Stamp, Kind)>,
    dirs: &mut Vec<(Box<str>, Stamp)>,
) -> Result<(), String> {
    for entry in std::fs::read_dir(dir).map_err(|e| reading(dir, e))? {
        let entry = entry.map_err(|e| reading(dir, e))?;
        let name = entry.file_name();
        // A name that is not UTF-8 is in no published package, so it is kept as one that
        // matches none.
        let name = match name.to_str() {
            Some(name) => name.to_owned(),
            None => format!("\0{}", name.to_string_lossy()),
        };
        if prefix.is_empty() && matches!(name.as_str(), ".cargo-ok" | ".cargo-checksum.json") {
            continue;
        }
        let rel = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
        let meta = entry.metadata().map_err(|e| reading(&entry.path(), e))?;
        let stamp = Stamp::of(&meta);
        if meta.is_dir() {
            walk(&entry.path(), &rel, files, dirs)?;
            dirs.push((rel.into_boxed_str(), stamp));
        } else {
            files.push((rel, stamp, if meta.is_file() { Kind::File } else { Kind::Other }));
        }
    }
    Ok(())
}

/// The BLAKE3 of the file at `path`, read once for each settled stamp: through a fixed
/// buffer, with BLAKE3's SIMD, never all of it in memory.
fn file_hash(path: &Path, stamp: &Stamp, read_at: i64) -> Result<[u8; 32], String> {
    let id = keyed(FILE, path, stamp);
    let table = hashes();
    if let Some(idx) = table.find(&id) {
        return Ok(*table.get(idx));
    }
    let file = std::fs::File::open(path).map_err(|e| reading(path, e))?;
    let mut hasher = blake3::Hasher::new();
    hasher.update_reader(file).map_err(|e| reading(path, e))?;
    let hash = *hasher.finalize().as_bytes();
    if stamp.settled(read_at) {
        table.intern(id, || hash);
    }
    Ok(hash)
}

#[cfg(test)]
#[test]
fn a_file_changed_in_the_tick_it_was_read_in_is_not_settled() {
    let stamp = Stamp { len: 1, inode: 2, modified_ns: 90, changed_ns: 100 };
    assert!(!stamp.settled(100 + SETTLE_NS));
    assert!(stamp.settled(101 + SETTLE_NS));
    let ahead = Stamp { modified_ns: 200, ..stamp };
    assert!(!ahead.settled(150 + SETTLE_NS), "a stamp ahead of the clock is never settled");
    assert_eq!(Stamp::of(&std::fs::metadata(file!()).unwrap()).bytes().len(), 32);
}

impl Snapshot {
    fn id(&self, described: &ContentId) -> ContentId {
        let mut hasher = Hasher::new(SNAPSHOT);
        hasher.id(described);
        for package in &self.closure.packages {
            hasher.update(&(package.files.len() as u64).to_le_bytes());
            for (path, hash) in &package.files {
                hasher.part(path.as_bytes()).update(hash);
            }
        }
        for stamp in &self.stamps {
            hasher.update(&stamp.bytes());
        }
        for dir in &self.dirs {
            hasher.part(dir.as_bytes());
        }
        hasher.finish()
    }
}

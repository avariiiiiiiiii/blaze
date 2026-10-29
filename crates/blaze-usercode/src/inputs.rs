//! What a run's reads are hashed as ([`crate::trace::Inputs`]): the machine's files as they
//! are on disk ([`Disk`]), for the launcher as a run ends and for paths outside every view
//! (a toolchain, the C compiler, `/usr/include`: #1480); and a view's files through the query
//! database ([`ViewInputs`]), which records each as an edge of the query that checks them.
//!
//! The machine's files are the one thing of user code read outside a view: they are hashed
//! once per change of what `stat` says of them, and a file that is there and can't be read
//! is an error naming it, never left out.

use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    fmt, io,
    path::{Path, PathBuf},
};

use blaze_ids::ContentId;
use blaze_query::{Cancelled, Cx};

use crate::trace::{DIRECTORY, Inputs, listing};

/// A path that is there and could not be read.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Unreadable {
    pub path: PathBuf,
    pub error: String,
}

impl fmt::Display for Unreadable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} is an input of a build script or proc macro and can't be read: {}", self.path.display(), self.error)
    }
}

/// What `stat` says of a file: a hash made when it said this is the file's hash now.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Stamp {
    len: u64,
    modified: i128,
    changed: i128,
    inode: u64,
    device: u64,
}

impl Stamp {
    fn of(meta: &std::fs::Metadata) -> Stamp {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            Stamp {
                len: meta.len(),
                modified: i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec()),
                changed: i128::from(meta.ctime()) * 1_000_000_000 + i128::from(meta.ctime_nsec()),
                inode: meta.ino(),
                device: meta.dev(),
            }
        }
        #[cfg(not(unix))]
        {
            let nanos = |time: io::Result<std::time::SystemTime>| {
                time.ok()
                    .and_then(|it| it.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(-1, |it| it.as_nanos() as i128)
            };
            Stamp {
                len: meta.len(),
                modified: nanos(meta.modified()),
                changed: nanos(meta.created()),
                inode: 0,
                device: 0,
            }
        }
    }
}

/// The machine's files as they are, and an environment.
pub struct Disk {
    env: Vec<(OsString, OsString)>,
    files: HashMap<PathBuf, (Stamp, [u8; 32])>,
}

impl Disk {
    pub fn new(env: Vec<(OsString, OsString)>) -> Disk {
        Disk { env, files: HashMap::new() }
    }

    /// The same files, cached, with another environment: one `Disk` serves every check of a
    /// daemon.
    pub fn set_env(&mut self, env: Vec<(OsString, OsString)>) {
        self.env = env;
    }
}

fn missing(error: &io::Error) -> bool {
    matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory)
        || error.raw_os_error() == Some(NOT_A_DIRECTORY)
}

#[cfg(unix)]
const NOT_A_DIRECTORY: i32 = libc::ENOTDIR;
#[cfg(not(unix))]
const NOT_A_DIRECTORY: i32 = 267;

/// The BLAKE3 hash of what `reader` reads.
pub fn hash_reader(mut reader: impl io::Read) -> io::Result<[u8; 32]> {
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0; 1 << 16];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(*hasher.finalize().as_bytes()),
            Ok(n) => _ = hasher.update(&buffer[..n]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

impl Inputs for Disk {
    type Error = Unreadable;

    fn file(&mut self, path: &Path) -> Result<Option<[u8; 32]>, Unreadable> {
        let unreadable = |error: io::Error| Unreadable { path: path.to_owned(), error: error.to_string() };
        let meta = match std::fs::metadata(path) {
            Ok(meta) => meta,
            Err(error) if missing(&error) => return Ok(None),
            Err(error) => return Err(unreadable(error)),
        };
        if meta.is_dir() {
            return Ok(Some(DIRECTORY));
        }
        let stamp = Stamp::of(&meta);
        if let Some((seen, hash)) = self.files.get(path)
            && *seen == stamp
        {
            return Ok(Some(*hash));
        }
        let file = std::fs::File::open(path).map_err(unreadable)?;
        let hash = hash_reader(file).map_err(unreadable)?;
        // A file changed while it was read is hashed again at its next check.
        let after = std::fs::metadata(path).map(|it| Stamp::of(&it)).ok();
        if after == Some(stamp) {
            self.files.insert(path.to_owned(), (stamp, hash));
        }
        Ok(Some(hash))
    }

    fn dir(&mut self, path: &Path) -> Result<Option<[u8; 32]>, Unreadable> {
        let entries = match std::fs::read_dir(path) {
            Ok(entries) => entries,
            Err(error) if missing(&error) => return Ok(None),
            Err(error) => return Err(Unreadable { path: path.to_owned(), error: error.to_string() }),
        };
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| Unreadable { path: path.to_owned(), error: error.to_string() })?;
            names.push(entry.file_name());
        }
        Ok(Some(listing(names.iter().map(OsString::as_os_str))))
    }

    fn env(&mut self, name: &OsStr) -> Option<OsString> {
        self.env.iter().find(|it| it.0 == name).map(|it| it.1.clone())
    }

    fn env_all(&mut self) -> Vec<(OsString, OsString)> {
        let mut all = self.env.clone();
        all.sort();
        all
    }
}

/// Why a check against a view did not end.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ViewError {
    Cancelled,
    Unreadable(Unreadable),
}

impl From<Cancelled> for ViewError {
    fn from(_: Cancelled) -> ViewError {
        ViewError::Cancelled
    }
}

impl From<Unreadable> for ViewError {
    fn from(it: Unreadable) -> ViewError {
        ViewError::Unreadable(it)
    }
}

/// The plain BLAKE3 hashes of a view's files, by their content ID: shared by every check.
#[derive(Default)]
pub struct Hashes(HashMap<ContentId, [u8; 32]>);

impl Hashes {
    fn of(&mut self, id: ContentId, bytes: &[u8]) -> [u8; 32] {
        *self.0.entry(id).or_insert_with(|| *blake3::hash(bytes).as_bytes())
    }
}

/// A view's files where the sandbox sees the view (`root`, the directory it is placed at),
/// read through `cx` so the checking query depends on each; the machine's everywhere else;
/// the environment of the request.
pub struct ViewInputs<'a, 'db> {
    pub cx: &'a mut Cx<'db>,
    pub root: &'a Path,
    pub disk: &'a mut Disk,
    pub hashes: &'a mut Hashes,
}

impl ViewInputs<'_, '_> {
    /// `path` as the view names it, when it is in the view.
    fn in_view(&self, path: &Path) -> Option<String> {
        let rest = path.strip_prefix(self.root).ok()?;
        let mut name = String::new();
        for part in rest.components() {
            let std::path::Component::Normal(part) = part else { return None };
            if !name.is_empty() {
                name.push('/');
            }
            name.push_str(part.to_str()?);
        }
        Some(name)
    }
}

impl Inputs for ViewInputs<'_, '_> {
    type Error = ViewError;

    fn file(&mut self, path: &Path) -> Result<Option<[u8; 32]>, ViewError> {
        let Some(name) = self.in_view(path) else { return Ok(self.disk.file(path)?) };
        if !name.is_empty()
            && let Some(file) = self.cx.file(&name)?
        {
            return Ok(Some(self.hashes.of(file.id, file.bytes)));
        }
        Ok(self.cx.dir(&name)?.map(|_| DIRECTORY))
    }

    fn dir(&mut self, path: &Path) -> Result<Option<[u8; 32]>, ViewError> {
        let Some(name) = self.in_view(path) else { return Ok(self.disk.dir(path)?) };
        Ok(self.cx.dir(&name)?.map(|entries| listing(entries.iter().map(|it| OsStr::new(&it.0)))))
    }

    fn env(&mut self, name: &OsStr) -> Option<OsString> {
        self.disk.env(name)
    }

    fn env_all(&mut self) -> Vec<(OsString, OsString)> {
        self.disk.env_all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_disk_hashes_files_lists_directories_and_refuses_nothing_quietly() {
        let dir = std::env::temp_dir().join(format!("blaze-disk-test-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a"), b"one").unwrap();
        let mut disk = Disk::new(vec![("B".into(), "2".into()), ("A".into(), "1".into())]);
        assert_eq!(disk.file(&dir.join("a")).unwrap(), Some(*blake3::hash(b"one").as_bytes()));
        assert_eq!(disk.file(&dir.join("sub")).unwrap(), Some(DIRECTORY));
        assert_eq!(disk.file(&dir.join("none")).unwrap(), None);
        assert_eq!(disk.file(&dir.join("a/below")).unwrap(), None);
        assert_eq!(
            disk.dir(&dir).unwrap(),
            Some(listing(["a", "sub"].map(OsStr::new)))
        );
        assert_eq!(disk.dir(&dir.join("none")).unwrap(), None);
        std::fs::write(dir.join("a"), b"two, longer").unwrap();
        assert_eq!(disk.file(&dir.join("a")).unwrap(), Some(*blake3::hash(b"two, longer").as_bytes()));
        assert_eq!(disk.env(OsStr::new("A")), Some("1".into()));
        assert_eq!(disk.env_all()[0].0, "A");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::write(dir.join("secret"), b"x").unwrap();
            std::fs::set_permissions(dir.join("secret"), std::fs::Permissions::from_mode(0)).unwrap();
            // Root reads it anyway; anyone else is told it can't be read.
            if std::fs::read(dir.join("secret")).is_err() {
                assert!(disk.file(&dir.join("secret")).is_err());
            }
            std::fs::set_permissions(dir.join("secret"), std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        _ = std::fs::remove_dir_all(&dir);
    }
}

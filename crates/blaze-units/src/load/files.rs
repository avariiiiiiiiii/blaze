//! Every file the load reads, through the database (docs/v2/plan/lanes/cargo/notes.md section 2).
//!
//! - A path, absolute as cargo names it, is read through its longest [`Mount`]: the view's
//!   files (`Cx::file`) or another root's (`Cx::file_at`). [`LoadFile`] and [`LoadDir`] are one
//!   query per path, so a mount added for a fetched source reruns them alone and cuts off.
//! - A path under no mount is never read: it is [`Found::Unmounted`], which the load turns into
//!   a fetch or an error naming it. Outside every mount only the files the client sent are
//!   (`Invocation::files`), and a file of a directory above the view or the cwd, or of
//!   `$CARGO_HOME`, that it did not send is absent.
//! - blaze-manifest reads through a [`Snapshot`]: an owned `Files` that answers from what was
//!   read so far and records what it was asked and did not have. [`run_over`] runs a step over
//!   it, reads what it missed, and runs it again until it misses nothing; what the last run used
//!   is the snapshot a query keeps, so a later query runs blaze-manifest again over exactly those
//!   files, and a `Cargo.toml` kept in its canonical form makes a comment edit cut off.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Component, Path};
use std::rc::Rc;

use blaze_ids::ContentId;
use blaze_manifest::files::{FileKind, Files};
use blaze_query::{Cancelled, Cx, Query, QueryKind};
use blaze_format::SchemaHash;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};

use super::invocation::Invocation;
use crate::query::{Cbor, Mount, Origin, Versioned, key_input, producer};

/// What a path is.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub(crate) enum Found<T> {
    /// Under no mount of the view: not fetched yet.
    Unmounted,
    Absent,
    Here(T),
}

impl Versioned for Found<Vec<u8>> {
    const SCHEMA: SchemaHash = blaze_format::schema("blaze v2 cargo found file 1");
}

impl Versioned for Found<Vec<(String, bool)>> {
    const SCHEMA: SchemaHash = blaze_format::schema("blaze v2 cargo found dir 1");
}

/// Where every directory cargo names is, and what the client sent of the rest.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub(crate) struct MountTable {
    pub mounts: Vec<Mount>,
    /// The files outside every mount the client sent, by absolute path.
    pub outside: Vec<(String, Vec<u8>)>,
    /// The directories outside every mount whose files cargo looks for (the view's and the
    /// cwd's ancestors, `$CARGO_HOME`): a file of one the client did not send is absent.
    pub known: Vec<String>,
}

impl Versioned for MountTable {
    const SCHEMA: SchemaHash = blaze_format::schema("blaze v2 cargo mount table 1");
}

/// The view's [`MountTable`]: of the invocation, the mounts alone, so a change to the command
/// line reruns no read.
pub(crate) struct Mounts;

impl Query for Mounts {
    const KIND: QueryKind = QueryKind(0x6012);
    const NAME: &'static str = "cargo mounts";
    const PRODUCER: ContentId = producer("blaze v2 cargo mounts 1");
    type Key = ();
    type Value = Cbor<MountTable>;

    fn input(key: &()) -> ContentId {
        key_input("blaze v2 cargo mounts key 1", key)
    }

    fn execute(cx: &mut Cx<'_>, _: &()) -> Result<Cbor<MountTable>, Cancelled> {
        let configured = cx.get::<Invocation>(&())?.get();
        Ok(Cbor(configured.mounts))
    }
}

/// A file by absolute path.
pub(crate) struct LoadFile;

impl Query for LoadFile {
    const KIND: QueryKind = QueryKind(0x6013);
    const NAME: &'static str = "cargo file";
    const PRODUCER: ContentId = producer("blaze v2 cargo file 1");
    type Key = String;
    type Value = Cbor<Found<Vec<u8>>>;

    fn input(key: &String) -> ContentId {
        key_input("blaze v2 cargo file key 1", key)
    }

    fn execute(cx: &mut Cx<'_>, path: &String) -> Result<Cbor<Found<Vec<u8>>>, Cancelled> {
        let table = cx.get::<Mounts>(&())?.get();
        Ok(Cbor(file_through(cx, &table, path, true)?))
    }
}

fn file_through(cx: &mut Cx<'_>, table: &MountTable, path: &str, view: bool) -> Result<Found<Vec<u8>>, Cancelled> {
    Ok(match locate(&table.mounts, path) {
        Some((Origin::View, under)) if view => match cx.file(&under)? {
            Some(file) => Found::Here(file.bytes.to_vec()),
            None => Found::Absent,
        },
        Some((Origin::View, _)) => Found::Unmounted,
        Some((Origin::Root(root), under)) => match cx.file_at(root, &under)? {
            Some(file) => Found::Here(file.bytes.to_vec()),
            None => Found::Absent,
        },
        None => match table.outside.iter().find(|(at, _)| at == path) {
            Some((_, bytes)) => Found::Here(bytes.clone()),
            None if known_file(&table.known, path) => Found::Absent,
            None => Found::Unmounted,
        },
    })
}

/// A directory's names by absolute path.
pub(crate) struct LoadDir;

impl Query for LoadDir {
    const KIND: QueryKind = QueryKind(0x6014);
    const NAME: &'static str = "cargo dir";
    const PRODUCER: ContentId = producer("blaze v2 cargo dir 1");
    type Key = String;
    type Value = Cbor<Found<Vec<(String, bool)>>>;

    fn input(key: &String) -> ContentId {
        key_input("blaze v2 cargo dir key 1", key)
    }

    fn execute(cx: &mut Cx<'_>, path: &String) -> Result<Cbor<Found<Vec<(String, bool)>>>, Cancelled> {
        let table = cx.get::<Mounts>(&())?.get();
        Ok(Cbor(dir_through(cx, &table, path, true)?))
    }
}

type Names = Vec<(String, bool)>;

fn dir_through(cx: &mut Cx<'_>, table: &MountTable, path: &str, view: bool) -> Result<Found<Names>, Cancelled> {
    Ok(match locate(&table.mounts, path) {
        Some((Origin::View, under)) if view => match cx.dir(&under)? {
            Some(names) => Found::Here(names.to_vec()),
            None => Found::Absent,
        },
        Some((Origin::View, _)) => Found::Unmounted,
        Some((Origin::Root(root), under)) => match cx.dir_at(root, &under)? {
            Some(names) => Found::Here(names.to_vec()),
            None => Found::Absent,
        },
        None if table.known.iter().any(|it| it == path) => {
            let prefix = if path == "/" { "/".to_owned() } else { format!("{path}/") };
            let mut names: FxHashMap<String, bool> = FxHashMap::default();
            for mount in &table.mounts {
                let Some(rest) = mount.at.strip_prefix(&prefix) else { continue };
                match rest.split_once('/') {
                    Some((first, _)) => names.insert(first.to_owned(), true),
                    // A mount of one file (a sparse index's) is a file of the directory.
                    None => {
                        let dir = match mount.origin {
                            Origin::View if view => cx.file(&mount.under)?.is_none(),
                            Origin::View => continue,
                            Origin::Root(root) => cx.file_at(root, &mount.under)?.is_none(),
                        };
                        names.insert(rest.to_owned(), dir)
                    }
                };
            }
            for (at, _) in &table.outside {
                if let Some(rest) = at.strip_prefix(&prefix) {
                    match rest.split_once('/') {
                        Some((dir, _)) => names.insert(dir.to_owned(), true),
                        None => names.insert(rest.to_owned(), false),
                    };
                }
            }
            let mut names: Names = names.into_iter().collect();
            names.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
            Found::Here(names)
        }
        None => Found::Unmounted,
    })
}

/// A path read through a [`MountTable`] of its own, never the view's: what a
/// [`super::config::ConfigOf::Fixed`] configuration reads.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub(crate) struct FixedPath {
    pub table: MountTable,
    pub path: String,
}

/// A file of a [`FixedPath`]: a mount of the view's own files is under no mount for it.
pub(crate) struct FixedFile;

impl Query for FixedFile {
    const KIND: QueryKind = QueryKind(0x6018);
    const NAME: &'static str = "cargo fixed file";
    const PRODUCER: ContentId = producer("blaze v2 cargo fixed file 1");
    type Key = FixedPath;
    type Value = Cbor<Found<Vec<u8>>>;

    fn input(key: &FixedPath) -> ContentId {
        key_input("blaze v2 cargo fixed file key 1", key)
    }

    fn execute(cx: &mut Cx<'_>, key: &FixedPath) -> Result<Cbor<Found<Vec<u8>>>, Cancelled> {
        Ok(Cbor(file_through(cx, &key.table, &key.path, false)?))
    }
}

pub(crate) struct FixedDir;

impl Query for FixedDir {
    const KIND: QueryKind = QueryKind(0x601d);
    const NAME: &'static str = "cargo fixed dir";
    const PRODUCER: ContentId = producer("blaze v2 cargo fixed dir 1");
    type Key = FixedPath;
    type Value = Cbor<Found<Names>>;

    fn input(key: &FixedPath) -> ContentId {
        key_input("blaze v2 cargo fixed dir key 1", key)
    }

    fn execute(cx: &mut Cx<'_>, key: &FixedPath) -> Result<Cbor<Found<Names>>, Cancelled> {
        Ok(Cbor(dir_through(cx, &key.table, &key.path, false)?))
    }
}

/// The file at `path` (absolute, as cargo names it) through the view's mounts.
pub(crate) fn read_file(cx: &mut Cx<'_>, path: &str) -> Result<Found<Vec<u8>>, Cancelled> {
    Ok(cx.get::<LoadFile>(&path.to_owned())?.get())
}

/// The names in the directory at `path`, sorted, each with whether it is a directory.
pub(crate) fn read_dir(cx: &mut Cx<'_>, path: &str) -> Result<Found<Vec<(String, bool)>>, Cancelled> {
    Ok(cx.get::<LoadDir>(&path.to_owned())?.get())
}

/// `path`'s longest mount: its origin and the path under the origin's root.
pub(crate) fn locate(mounts: &[Mount], path: &str) -> Option<(Origin, String)> {
    let mount = mounts
        .iter()
        .filter(|it| under(&it.at, path).is_some())
        .max_by_key(|it| it.at.len())?;
    let rest = under(&mount.at, path)?;
    let joined = match (mount.under.as_str(), rest) {
        ("", rest) => rest.to_owned(),
        (under, "") => under.to_owned(),
        (under, rest) => format!("{under}/{rest}"),
    };
    Some((mount.origin, joined))
}

/// What is left of `path` under `at`: "" for `at` itself.
fn under<'p>(at: &str, path: &'p str) -> Option<&'p str> {
    if at == "/" || at.is_empty() {
        return path.strip_prefix('/');
    }
    let rest = path.strip_prefix(at)?;
    if rest.is_empty() {
        return Some("");
    }
    rest.strip_prefix('/')
}

fn known_file(known: &[String], path: &str) -> bool {
    let Some((dir, _)) = path.rsplit_once('/') else { return false };
    let dir = if dir.is_empty() { "/" } else { dir };
    if known.iter().any(|it| it == dir) {
        return true;
    }
    match dir.rsplit_once('/') {
        Some((above, ".cargo")) => {
            let above = if above.is_empty() { "/" } else { above };
            known.iter().any(|it| it == above)
        }
        _ => false,
    }
}

/// `path` as the load names it: absolute, `/`-separated, `.` and `..` taken out. `None` for a
/// path that is not absolute or not UTF-8.
pub(crate) fn normal(path: &Path) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    let mut absolute = false;
    for component in path.components() {
        match component {
            Component::RootDir => absolute = true,
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop();
            }
            Component::Normal(part) => parts.push(part.to_str()?),
            Component::Prefix(_) => return None,
        }
    }
    if !absolute {
        return None;
    }
    Some(format!("/{}", parts.join("/")))
}

/// The directories above `path`, nearest first, `/` last.
pub(crate) fn ancestors(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut at = path;
    while let Some((above, _)) = at.rsplit_once('/') {
        let above = if above.is_empty() { "/" } else { above };
        out.push(above.to_owned());
        if above == "/" {
            break;
        }
        at = above;
    }
    out
}

/// What a step read, by absolute path, and what it found.
// BTreeMap: written into CBOR query values (`ManifestRead`).
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
pub(crate) struct Snapshot {
    pub files: BTreeMap<String, Found<Vec<u8>>>,
    pub dirs: BTreeMap<String, Found<Vec<(String, bool)>>>,
}

impl Snapshot {
    /// Adds what `other` has that this does not.
    pub fn merge(&mut self, other: &Snapshot) {
        for (path, found) in &other.files {
            self.files.entry(path.clone()).or_insert_with(|| found.clone());
        }
        for (path, found) in &other.dirs {
            self.dirs.entry(path.clone()).or_insert_with(|| found.clone());
        }
    }

    /// The first path it used that is under no mount.
    pub fn unmounted(&self) -> Option<&str> {
        let files = self.files.iter().filter(|(_, it)| **it == Found::Unmounted).map(|(p, _)| p);
        let dirs = self.dirs.iter().filter(|(_, it)| **it == Found::Unmounted).map(|(p, _)| p);
        files.chain(dirs).min().map(String::as_str)
    }

    /// Every TOML file (manifests, config files) in its canonical form: its parsed table
    /// written again, so an edit to a comment or the layout leaves the snapshot equal. A file
    /// that does not parse stays as read, for its error.
    pub fn canonical(mut self) -> Snapshot {
        for (path, found) in self.files.iter_mut() {
            let toml = path.ends_with(".toml") || path.ends_with("/.cargo/config");
            if let (true, Found::Here(bytes)) = (toml, &*found)
                && let Some(text) = canonical_toml(bytes)
            {
                *found = Found::Here(text.into_bytes());
            }
        }
        self
    }
}

fn canonical_toml(bytes: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    let table: toml::Table = toml::from_str(text).ok()?;
    let again = toml::to_string(&table).ok()?;
    // Kept only when it reads back as the same table.
    (toml::from_str::<toml::Table>(&again).ok()? == table).then_some(again)
}

/// What a step was asked for that its snapshot did not have.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub(crate) enum Need {
    File(String),
    Dir(String),
}

#[derive(Default)]
struct State {
    snapshot: Snapshot,
    used: Snapshot,
    missed: FxHashSet<Need>,
}

impl State {
    fn file(&mut self, path: &str) -> Option<Found<Vec<u8>>> {
        match self.snapshot.files.get(path) {
            Some(found) => {
                self.used.files.insert(path.to_owned(), found.clone());
                Some(found.clone())
            }
            None => {
                self.missed.insert(Need::File(path.to_owned()));
                None
            }
        }
    }

    fn dir(&mut self, path: &str) -> Option<Found<Vec<(String, bool)>>> {
        match self.snapshot.dirs.get(path) {
            Some(found) => {
                self.used.dirs.insert(path.to_owned(), found.clone());
                Some(found.clone())
            }
            None => {
                self.missed.insert(Need::Dir(path.to_owned()));
                None
            }
        }
    }

    /// What the run missed, each once, in order.
    fn take_missed(&mut self) -> Vec<Need> {
        let mut missed: Vec<Need> = self.missed.drain().collect();
        missed.sort_unstable();
        missed
    }
}

/// The `Files` blaze-manifest reads through: a [`Snapshot`] that records what it misses.
struct Snap(Rc<RefCell<State>>);

fn not_found() -> std::io::Error {
    std::io::Error::from_raw_os_error(2)
}

fn unmounted(path: &str) -> std::io::Error {
    std::io::Error::other(format!("`{path}` is under no mount of the view"))
}

impl Files for Snap {
    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        let Some(path) = normal(path) else { return Err(not_found()) };
        match self.0.borrow_mut().file(&path) {
            Some(Found::Here(bytes)) => Ok(bytes),
            Some(Found::Unmounted) => Err(unmounted(&path)),
            Some(Found::Absent) | None => Err(not_found()),
        }
    }

    /// From the parent directory's names, so an edit to a file's contents is never read by
    /// asking whether it is there; a path its parent's names don't answer for (a mount's own
    /// directory) is asked of itself.
    fn kind(&self, path: &Path) -> Option<FileKind> {
        let path = normal(path)?;
        let mut state = self.0.borrow_mut();
        if let Some((parent, name)) = path.rsplit_once('/') {
            let parent = if parent.is_empty() { "/" } else { parent };
            if let Found::Here(names) = state.dir(parent)? {
                return names.iter().find(|(it, _)| it == name).map(|(_, dir)| match dir {
                    true => FileKind::Dir,
                    false => FileKind::File,
                });
            }
        }
        if let Found::Here(_) = state.dir(&path)? {
            return Some(FileKind::Dir);
        }
        match state.file(&path)? {
            Found::Here(_) => Some(FileKind::File),
            Found::Absent | Found::Unmounted => None,
        }
    }

    fn list(&self, path: &Path) -> std::io::Result<Vec<(String, bool)>> {
        let Some(path) = normal(path) else { return Err(not_found()) };
        match self.0.borrow_mut().dir(&path) {
            Some(Found::Here(names)) => Ok(names),
            Some(Found::Unmounted) => Err(unmounted(&path)),
            Some(Found::Absent) | None => Err(not_found()),
        }
    }
}

/// How a step's misses are read: through which configuration's mounts, and with which a
/// `Cargo.toml` that is a manifest of its own is read.
pub(crate) struct Fill<'k> {
    pub config: &'k super::config::ConfigOf,
    /// The manifest the step reads itself, as a file; every other `Cargo.toml` it reads is
    /// taken from its own [`super::manifest::ManifestQuery`].
    pub own: Option<&'k str>,
}

impl Fill<'_> {
    fn read(&self, cx: &mut Cx<'_>, need: &Need, into: &mut Snapshot) -> Result<(), Cancelled> {
        match need {
            Need::File(path) => {
                if path.ends_with("/Cargo.toml") && self.own != Some(path.as_str()) {
                    let key = super::manifest::ManifestKey {
                        path: path.clone(),
                        config: self.config.clone(),
                        source: None,
                    };
                    let read = cx.get::<super::manifest::ManifestQuery>(&key)?.get();
                    into.merge(&read.snapshot);
                }
                if !into.files.contains_key(path) {
                    let found = self.config.read_file(cx, path)?;
                    into.files.insert(path.clone(), found);
                }
            }
            Need::Dir(path) => {
                let found = self.config.read_dir(cx, path)?;
                into.dirs.insert(path.clone(), found);
            }
        }
        Ok(())
    }
}

/// Runs `step` over a snapshot that starts as `base`, reading what it misses as `fill` says and
/// running it again, until a run misses nothing. Returns that run's answer and what it used.
/// `step` may ask queries itself; its `Files` is a fresh handle on the snapshot each run.
pub(crate) fn run_over<'db, R>(
    cx: &mut Cx<'db>,
    base: Snapshot,
    fill: &Fill<'_>,
    mut step: impl FnMut(&mut Cx<'db>, Rc<dyn Files>) -> Result<R, Cancelled>,
) -> Result<(R, Snapshot), Cancelled> {
    let state = Rc::new(RefCell::new(State { snapshot: base, ..State::default() }));
    loop {
        {
            let mut it = state.borrow_mut();
            it.used = Snapshot::default();
            it.missed.clear();
        }
        let files: Rc<dyn Files> = Rc::new(Snap(state.clone()));
        let answer = step(cx, files)?;
        let missed = state.borrow_mut().take_missed();
        if missed.is_empty() {
            let used = std::mem::take(&mut state.borrow_mut().used);
            return Ok((answer, used));
        }
        let mut snapshot = std::mem::take(&mut state.borrow_mut().snapshot);
        for need in &missed {
            fill.read(cx, need, &mut snapshot)?;
        }
        state.borrow_mut().snapshot = snapshot;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mount(at: &str, origin: Origin, under: &str) -> Mount {
        Mount { at: at.to_owned(), origin, under: under.to_owned() }
    }

    #[test]
    fn the_longest_mount_is_a_paths() {
        let lib = Origin::Root(ContentId([1; 32]));
        let mounts = vec![
            mount("/w", Origin::View, ""),
            mount("/w/vendor/x", lib, "x-1.0"),
            mount("/w2", Origin::View, "other"),
        ];
        assert_eq!(locate(&mounts, "/w/Cargo.toml"), Some((Origin::View, "Cargo.toml".to_owned())));
        assert_eq!(locate(&mounts, "/w"), Some((Origin::View, String::new())));
        assert_eq!(locate(&mounts, "/w/vendor/x/src/lib.rs"), Some((lib, "x-1.0/src/lib.rs".to_owned())));
        assert_eq!(locate(&mounts, "/w/vendor/x"), Some((lib, "x-1.0".to_owned())));
        assert_eq!(locate(&mounts, "/w2/a"), Some((Origin::View, "other/a".to_owned())));
        assert_eq!(locate(&mounts, "/wx/a"), None, "a mount is a directory, not a prefix");
        assert_eq!(locate(&mounts, "/"), None);
    }

    #[test]
    fn paths_are_normal() {
        assert_eq!(normal(Path::new("/w/a/../b/./Cargo.toml")).as_deref(), Some("/w/b/Cargo.toml"));
        assert_eq!(normal(Path::new("/")).as_deref(), Some("/"));
        assert_eq!(normal(Path::new("w/a")), None);
        assert_eq!(ancestors("/w/a/b"), vec!["/w/a", "/w", "/"]);
    }

    #[test]
    fn outside_the_mounts_only_known_directories_have_files() {
        let known = vec!["/home/u/.cargo".to_owned(), "/home/u".to_owned(), "/".to_owned()];
        assert!(known_file(&known, "/home/u/.cargo/config.toml"));
        assert!(known_file(&known, "/home/u/.cargo/config"));
        assert!(known_file(&known, "/.cargo/config.toml"));
        assert!(known_file(&known, "/Cargo.toml"));
        assert!(!known_file(&known, "/etc/passwd/x"));
        assert!(!known_file(&known, "/home/u/.cargo/registry/src/x/Cargo.toml"));
    }

    #[test]
    fn what_a_run_missed_is_read_once_each_in_order() {
        let mut state = State::default();
        state.snapshot.files.insert("/w/a".to_owned(), Found::Absent);
        assert_eq!(state.file("/w/c"), None);
        assert_eq!(state.dir("/w"), None);
        assert_eq!(state.file("/w/a"), Some(Found::Absent));
        assert_eq!(state.file("/w/b"), None);
        assert_eq!(state.file("/w/c"), None);
        assert_eq!(state.dir("/v"), None);
        let want = vec![
            Need::File("/w/b".to_owned()),
            Need::File("/w/c".to_owned()),
            Need::Dir("/v".to_owned()),
            Need::Dir("/w".to_owned()),
        ];
        assert_eq!(state.take_missed(), want);
        assert!(state.take_missed().is_empty());
        assert_eq!(state.used.files.keys().collect::<Vec<_>>(), ["/w/a"]);
    }

    #[test]
    fn a_comment_leaves_the_canonical_form() {
        let one = "[package]\nname = \"a\" # the name\nversion = \"0.1.0\"\n";
        let two = "# a comment\n[package]\nversion = \"0.1.0\"\nname    = \"a\"\n";
        assert_eq!(canonical_toml(one.as_bytes()), canonical_toml(two.as_bytes()));
        assert_eq!(canonical_toml(b"[package"), None);
    }
}

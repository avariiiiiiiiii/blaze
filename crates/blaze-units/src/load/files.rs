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

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;
use std::marker::PhantomData;
use std::path::{Component, Path};
use std::rc::Rc;

use blaze_ids::ContentId;
use blaze_manifest::files::{FileKind, Files};
use blaze_query::{Cancelled, Cx, Query, QueryKind};
use blaze_format::SchemaHash;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize, Serializer};

use super::invocation::Invocation;
use crate::query::{Cbor, Mount, Origin, Versioned, key_input, producer};

/// What a path is.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Found<T> {
    /// Under no mount of the view: not fetched yet.
    Unmounted,
    Absent,
    Here(T),
}

/// [`Found`] as written: a file's bytes are one CBOR byte string, not an array of integers.
#[derive(Serialize, Deserialize)]
enum Written<T> {
    Unmounted,
    Absent,
    Here(T),
}

impl<T> Found<T> {
    fn written<'a, W>(&'a self, how: impl FnOnce(&'a T) -> W) -> Written<W> {
        match self {
            Found::Unmounted => Written::Unmounted,
            Found::Absent => Written::Absent,
            Found::Here(it) => Written::Here(how(it)),
        }
    }

    fn read<W>(written: Written<W>, how: impl FnOnce(W) -> T) -> Found<T> {
        match written {
            Written::Unmounted => Found::Unmounted,
            Written::Absent => Found::Absent,
            Written::Here(it) => Found::Here(how(it)),
        }
    }
}

impl Serialize for Found<Vec<u8>> {
    fn serialize<S: Serializer>(&self, to: S) -> Result<S::Ok, S::Error> {
        self.written(|bytes| Raw(Cow::Borrowed(bytes))).serialize(to)
    }
}

impl<'de> Deserialize<'de> for Found<Vec<u8>> {
    fn deserialize<D: Deserializer<'de>>(from: D) -> Result<Self, D::Error> {
        Ok(Found::read(Written::<Raw<'de>>::deserialize(from)?, |raw| raw.0.into_owned()))
    }
}

impl Serialize for Found<Names> {
    fn serialize<S: Serializer>(&self, to: S) -> Result<S::Ok, S::Error> {
        self.written(|names| names).serialize(to)
    }
}

impl<'de> Deserialize<'de> for Found<Names> {
    fn deserialize<D: Deserializer<'de>>(from: D) -> Result<Self, D::Error> {
        Ok(Found::read(Written::<Names>::deserialize(from)?, |names| names))
    }
}

/// Bytes written as one CBOR byte string and read in place where the decoder can.
pub(crate) struct Raw<'a>(pub Cow<'a, [u8]>);

impl Serialize for Raw<'_> {
    fn serialize<S: Serializer>(&self, to: S) -> Result<S::Ok, S::Error> {
        to.serialize_bytes(&self.0)
    }
}

impl<'de: 'a, 'a> Deserialize<'de> for Raw<'a> {
    fn deserialize<D: Deserializer<'de>>(from: D) -> Result<Self, D::Error> {
        struct Bytes<'a>(PhantomData<&'a ()>);
        impl<'de: 'a, 'a> Visitor<'de> for Bytes<'a> {
            type Value = Raw<'a>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a byte string")
            }
            fn visit_borrowed_bytes<E: de::Error>(self, bytes: &'de [u8]) -> Result<Raw<'a>, E> {
                Ok(Raw(Cow::Borrowed(bytes)))
            }
            fn visit_bytes<E: de::Error>(self, bytes: &[u8]) -> Result<Raw<'a>, E> {
                Ok(Raw(Cow::Owned(bytes.to_vec())))
            }
            fn visit_byte_buf<E: de::Error>(self, bytes: Vec<u8>) -> Result<Raw<'a>, E> {
                Ok(Raw(Cow::Owned(bytes)))
            }
        }
        from.deserialize_bytes(Bytes(PhantomData))
    }
}

impl Versioned for Found<Vec<u8>> {
    const SCHEMA: SchemaHash = blaze_format::schema("blaze v2 cargo found file 2");
}

impl Versioned for Found<Names> {
    const SCHEMA: SchemaHash = blaze_format::schema("blaze v2 cargo found dir 2");
}

/// Where every directory cargo names is, and what the client sent of the rest. `mounts` and
/// `outside` are sorted by path, so a lookup is a binary search per ancestor.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub(crate) struct MountTable {
    pub mounts: Vec<Mount>,
    /// The files outside every mount the client sent, by absolute path.
    #[serde(with = "outside_bytes")]
    pub outside: Vec<(String, Vec<u8>)>,
    /// The directories outside every mount whose files cargo looks for (the view's and the
    /// cwd's ancestors, `$CARGO_HOME`): a file of one the client did not send is absent.
    pub known: Vec<String>,
}

impl Versioned for MountTable {
    const SCHEMA: SchemaHash = blaze_format::schema("blaze v2 cargo mount table 2");
}

mod outside_bytes {
    use std::borrow::Cow;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::Raw;

    pub fn serialize<S: Serializer>(files: &[(String, Vec<u8>)], to: S) -> Result<S::Ok, S::Error> {
        to.collect_seq(files.iter().map(|(at, bytes)| (at, Raw(Cow::Borrowed(bytes)))))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(from: D) -> Result<Vec<(String, Vec<u8>)>, D::Error> {
        let files = Vec::<(String, Raw<'de>)>::deserialize(from)?;
        Ok(files.into_iter().map(|(at, raw)| (at, raw.0.into_owned())).collect())
    }
}

/// A [`MountTable`] read where its bytes lie: what a file read looks its path up in, with
/// nothing of the table copied.
#[derive(Deserialize)]
struct TableRef<'a> {
    #[serde(borrow)]
    mounts: Vec<MountRef<'a>>,
    #[serde(borrow)]
    outside: Vec<(&'a str, Raw<'a>)>,
    #[serde(borrow)]
    known: Vec<&'a str>,
}

#[derive(Deserialize)]
struct MountRef<'a> {
    at: &'a str,
    origin: Origin,
    under: &'a str,
}

impl<'a> TableRef<'a> {
    fn of(table: &'a MountTable) -> TableRef<'a> {
        TableRef {
            mounts: table.mounts.iter().map(|it| MountRef { at: &it.at, origin: it.origin, under: &it.under }).collect(),
            outside: table.outside.iter().map(|(at, bytes)| (at.as_str(), Raw(Cow::Borrowed(bytes.as_slice())))).collect(),
            known: table.known.iter().map(String::as_str).collect(),
        }
    }

    fn view(bytes: &'a [u8]) -> TableRef<'a> {
        blaze_protocol::cbor::from_slice(bytes)
            .unwrap_or_else(|error| panic!("a kept mount table does not decode: {error}"))
    }
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
        let table = cx.get::<Mounts>(&())?.bytes();
        Ok(Cbor(file_through(cx, &TableRef::view(table), path, true)?))
    }
}

fn file_through(cx: &mut Cx<'_>, table: &TableRef<'_>, path: &str, view: bool) -> Result<Found<Vec<u8>>, Cancelled> {
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
        None => match table.outside.binary_search_by(|(at, _)| (*at).cmp(path)) {
            Ok(at) => {
                let (_, Raw(bytes)) = &table.outside[at];
                Found::Here(bytes.to_vec())
            }
            Err(_) if known_file(&table.known, path) => Found::Absent,
            Err(_) => Found::Unmounted,
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
        let table = cx.get::<Mounts>(&())?.bytes();
        Ok(Cbor(dir_through(cx, &TableRef::view(table), path, true)?))
    }
}

type Names = Vec<(String, bool)>;

/// A listing sorted by name, so [`Snap::kind`] finds a name by binary search; the tree's are
/// already, which the sort sees in one pass.
fn sorted(names: &[(String, bool)]) -> Found<Names> {
    let mut names = names.to_vec();
    names.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
    Found::Here(names)
}

fn dir_through(cx: &mut Cx<'_>, table: &TableRef<'_>, path: &str, view: bool) -> Result<Found<Names>, Cancelled> {
    Ok(match locate(&table.mounts, path) {
        Some((Origin::View, under)) if view => match cx.dir(&under)? {
            Some(names) => sorted(names),
            None => Found::Absent,
        },
        Some((Origin::View, _)) => Found::Unmounted,
        Some((Origin::Root(root), under)) => match cx.dir_at(root, &under)? {
            Some(names) => sorted(names),
            None => Found::Absent,
        },
        None if table.known.binary_search(&path).is_ok() => {
            let prefix: Cow<'_, str> = if path == "/" { Cow::Borrowed("/") } else { Cow::Owned(format!("{path}/")) };
            let mut names: FxHashMap<&str, bool> = FxHashMap::default();
            let first = table.mounts.partition_point(|it| it.at < &*prefix);
            for mount in table.mounts[first..].iter().take_while(|it| it.at.starts_with(&*prefix)) {
                let rest = &mount.at[prefix.len()..];
                match rest.split_once('/') {
                    Some((dir, _)) => names.insert(dir, true),
                    // A mount of one file (a sparse index's) is a file of the directory.
                    None => {
                        let dir = match mount.origin {
                            Origin::View if view => cx.file(mount.under)?.is_none(),
                            Origin::View => continue,
                            Origin::Root(root) => cx.file_at(root, mount.under)?.is_none(),
                        };
                        names.insert(rest, dir)
                    }
                };
            }
            let first = table.outside.partition_point(|(at, _)| *at < &*prefix);
            for (at, _) in table.outside[first..].iter().take_while(|(at, _)| at.starts_with(&*prefix)) {
                let rest = &at[prefix.len()..];
                match rest.split_once('/') {
                    Some((dir, _)) => names.insert(dir, true),
                    None => names.insert(rest, false),
                };
            }
            let mut names: Names = names.into_iter().map(|(name, dir)| (name.to_owned(), dir)).collect();
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
        Ok(Cbor(file_through(cx, &TableRef::of(&key.table), &key.path, false)?))
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
        Ok(Cbor(dir_through(cx, &TableRef::of(&key.table), &key.path, false)?))
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

/// `path`'s longest mount: its origin and the path under the origin's root. The mounts are
/// sorted by `at`, so the longest is the first of `path` and the directories above it, nearest
/// first, that one is at: a binary search each, and nothing allocated unless the mount is
/// under a directory of its root.
fn locate<'a>(mounts: &[MountRef<'a>], path: &'a str) -> Option<(Origin, Cow<'a, str>)> {
    let mut at = path;
    loop {
        if let Ok(found) = mounts.binary_search_by(|it| it.at.cmp(at)) {
            let mount = &mounts[found];
            let rest = under(mount.at, path)?;
            let joined = match (mount.under, rest) {
                ("", rest) => Cow::Borrowed(rest),
                (under, "") => Cow::Borrowed(under),
                (under, rest) => Cow::Owned(format!("{under}/{rest}")),
            };
            return Some((mount.origin, joined));
        }
        at = match at.rsplit_once('/') {
            Some(("", _)) if at != "/" => "/",
            Some((above, _)) if !above.is_empty() => above,
            _ if !at.is_empty() => "",
            _ => return None,
        };
    }
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

/// Whether `path` is a file of a known directory, or of a known directory's `.cargo`: `known`
/// is sorted.
fn known_file(known: &[&str], path: &str) -> bool {
    let Some((dir, _)) = path.rsplit_once('/') else { return false };
    let dir = if dir.is_empty() { "/" } else { dir };
    if known.binary_search(&dir).is_ok() {
        return true;
    }
    match dir.rsplit_once('/') {
        Some((above, ".cargo")) => {
            let above = if above.is_empty() { "/" } else { above };
            known.binary_search(&above).is_ok()
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

/// [`normal`], borrowing a path that already is normal: the paths blaze-manifest asks for are.
fn normal_ref(path: &Path) -> Option<Cow<'_, str>> {
    if let Some(text) = path.to_str()
        && (text == "/" || (text.starts_with('/') && text[1..].split('/').all(|it| !matches!(it, "" | "." | ".."))))
    {
        return Some(Cow::Borrowed(text));
    }
    normal(path).map(Cow::Owned)
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
    pub fn merge(&mut self, other: Snapshot) {
        for (path, found) in other.files {
            self.files.entry(path).or_insert(found);
        }
        for (path, found) in other.dirs {
            self.dirs.entry(path).or_insert(found);
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

/// A snapshot's paths as sorted columns, each with whether this run used it: a read is a
/// binary search and a flag, and nothing is copied until the run ends.
struct Column<T> {
    paths: Vec<String>,
    found: Vec<Found<T>>,
    used: Vec<bool>,
}

impl<T> Default for Column<T> {
    fn default() -> Self {
        Column { paths: Vec::new(), found: Vec::new(), used: Vec::new() }
    }
}

impl<T> Column<T> {
    fn of(map: BTreeMap<String, Found<T>>) -> Column<T> {
        let (paths, found): (Vec<String>, Vec<Found<T>>) = map.into_iter().unzip();
        let used = vec![false; paths.len()];
        Column { paths, found, used }
    }

    fn find(&self, path: &str) -> Option<usize> {
        self.paths.binary_search_by(|it| it.as_str().cmp(path)).ok()
    }

    fn use_at(&mut self, at: usize) -> &Found<T> {
        self.used[at] = true;
        &self.found[at]
    }

    /// Every path, for the next run to start from.
    fn all(self) -> BTreeMap<String, Found<T>> {
        self.paths.into_iter().zip(self.found).collect()
    }

    /// The paths this run used.
    fn used(self) -> BTreeMap<String, Found<T>> {
        let used = self.used;
        self.paths.into_iter().zip(self.found).zip(used).filter_map(|(it, used)| used.then_some(it)).collect()
    }
}

#[derive(Default)]
struct State {
    files: Column<Vec<u8>>,
    dirs: Column<Names>,
    missed: FxHashSet<Need>,
}

impl State {
    fn of(snapshot: Snapshot) -> State {
        State { files: Column::of(snapshot.files), dirs: Column::of(snapshot.dirs), missed: FxHashSet::default() }
    }

    fn file(&mut self, path: &str) -> Option<&Found<Vec<u8>>> {
        let Some(at) = self.files.find(path) else {
            self.missed.insert(Need::File(path.to_owned()));
            return None;
        };
        Some(self.files.use_at(at))
    }

    fn dir(&mut self, path: &str) -> Option<&Found<Names>> {
        let Some(at) = self.dirs.find(path) else {
            self.missed.insert(Need::Dir(path.to_owned()));
            return None;
        };
        Some(self.dirs.use_at(at))
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
        let Some(path) = normal_ref(path) else { return Err(not_found()) };
        match self.0.borrow_mut().file(&path) {
            Some(Found::Here(bytes)) => Ok(bytes.clone()),
            Some(Found::Unmounted) => Err(unmounted(&path)),
            Some(Found::Absent) | None => Err(not_found()),
        }
    }

    /// From the parent directory's names, so an edit to a file's contents is never read by
    /// asking whether it is there; a path its parent's names don't answer for (a mount's own
    /// directory) is asked of itself. A listing is sorted by name.
    fn kind(&self, path: &Path) -> Option<FileKind> {
        let path = normal_ref(path)?;
        let mut state = self.0.borrow_mut();
        if let Some((parent, name)) = path.rsplit_once('/') {
            let parent = if parent.is_empty() { "/" } else { parent };
            if let Found::Here(names) = state.dir(parent)? {
                let at = names.binary_search_by(|(it, _)| it.as_str().cmp(name)).ok()?;
                return Some(if names[at].1 { FileKind::Dir } else { FileKind::File });
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
        let Some(path) = normal_ref(path) else { return Err(not_found()) };
        match self.0.borrow_mut().dir(&path) {
            Some(Found::Here(names)) => Ok(names.clone()),
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
    /// Whether the step finds a workspace's members, so a missed manifest's neighbours are
    /// read with it ([`Fill::beside`]).
    pub members: bool,
}

impl Fill<'_> {
    fn of_its_own(&self, path: &str) -> bool {
        path.ends_with("/Cargo.toml") && self.own != Some(path)
    }

    fn read(&self, cx: &mut Cx<'_>, need: &Need, into: &mut Snapshot) -> Result<(), Cancelled> {
        match need {
            Need::File(path) => {
                if self.of_its_own(path) {
                    let key = super::manifest::ManifestKey {
                        path: path.clone(),
                        config: self.config.clone(),
                        source: None,
                    };
                    let read = cx.get::<super::manifest::ManifestQuery>(&key)?.get();
                    into.merge(read.snapshot);
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

    /// The manifests beside each manifest `missed` names, read in the same round: the
    /// `Cargo.toml` of every other directory in its directory's parent. blaze-manifest stops at
    /// the first workspace member it cannot read, so without these a workspace of N members
    /// would take N runs; with them it takes as many as its members are deep. A manifest read
    /// this way that the next run does not use is left out of what it keeps.
    fn beside(&self, cx: &mut Cx<'_>, missed: &[Need], into: &mut Snapshot) -> Result<(), Cancelled> {
        if !self.members {
            return Ok(());
        }
        let mut more: Vec<Need> = Vec::new();
        for need in missed {
            let Need::File(path) = need else { continue };
            if !self.of_its_own(path) {
                continue;
            }
            let Some((dir, _)) = path.rsplit_once('/') else { continue };
            let Some((above, _)) = dir.rsplit_once('/') else { continue };
            let above = if above.is_empty() { "/" } else { above };
            if !into.dirs.contains_key(above) {
                let found = self.config.read_dir(cx, above)?;
                into.dirs.insert(above.to_owned(), found);
            }
            let Some(Found::Here(names)) = into.dirs.get(above) else { continue };
            let base = if above == "/" { "" } else { above };
            for (name, _) in names.iter().filter(|(name, dir)| *dir && !name.starts_with('.')) {
                let manifest = format!("{base}/{name}/Cargo.toml");
                if !into.files.contains_key(&manifest) {
                    more.push(Need::File(manifest));
                }
            }
        }
        more.sort_unstable();
        more.dedup();
        for need in &more {
            self.read(cx, need, into)?;
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
    let mut snapshot = base;
    loop {
        let state = Rc::new(RefCell::new(State::of(snapshot)));
        let files: Rc<dyn Files> = Rc::new(Snap(state.clone()));
        let answer = step(cx, files)?;
        let mut state = std::mem::take(&mut *state.borrow_mut());
        let missed = state.take_missed();
        if missed.is_empty() {
            return Ok((answer, Snapshot { files: state.files.used(), dirs: state.dirs.used() }));
        }
        snapshot = Snapshot { files: state.files.all(), dirs: state.dirs.all() };
        for need in &missed {
            fill.read(cx, need, &mut snapshot)?;
        }
        fill.beside(cx, &missed, &mut snapshot)?;
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
        let mut mounts = vec![
            mount("/w2", Origin::View, "other"),
            mount("/w", Origin::View, ""),
            mount("/w/vendor/x", lib, "x-1.0"),
        ];
        mounts.sort();
        let table = MountTable { mounts, outside: Vec::new(), known: Vec::new() };
        let table = TableRef::of(&table);
        let at = |path: &'static str| locate(&table.mounts, path).map(|(origin, under)| (origin, under.into_owned()));
        assert_eq!(at("/w/Cargo.toml"), Some((Origin::View, "Cargo.toml".to_owned())));
        assert_eq!(at("/w"), Some((Origin::View, String::new())));
        assert_eq!(at("/w/vendor/x/src/lib.rs"), Some((lib, "x-1.0/src/lib.rs".to_owned())));
        assert_eq!(at("/w/vendor/x"), Some((lib, "x-1.0".to_owned())));
        assert_eq!(at("/w/vendor/xy"), Some((Origin::View, "vendor/xy".to_owned())));
        assert_eq!(at("/w2/a"), Some((Origin::View, "other/a".to_owned())));
        assert_eq!(at("/wx/a"), None, "a mount is a directory, not a prefix");
        assert_eq!(at("/"), None);

        let root = MountTable { mounts: vec![mount("/", lib, "r")], outside: Vec::new(), known: Vec::new() };
        let root = TableRef::of(&root);
        assert_eq!(locate(&root.mounts, "/a/b").map(|(_, it)| it.into_owned()), Some("r/a/b".to_owned()));
        assert_eq!(locate(&root.mounts, "/").map(|(_, it)| it.into_owned()), Some("r".to_owned()));
    }

    /// A mount table written and read back borrows its strings and bytes; file bytes are one
    /// byte string.
    #[test]
    fn a_table_is_read_in_place() {
        let table = MountTable {
            mounts: vec![mount("/w", Origin::View, "")],
            outside: vec![("/home/u/.cargo/config.toml".to_owned(), b"[build]\n".to_vec())],
            known: vec!["/home/u".to_owned()],
        };
        let bytes = blaze_protocol::cbor::to_vec(&table).unwrap();
        let view = TableRef::view(&bytes);
        assert_eq!(view.mounts[0].at, "/w");
        let (_, Raw(bytes)) = &view.outside[0];
        assert!(matches!(bytes, Cow::Borrowed(b"[build]\n")));
        assert_eq!(view.known, ["/home/u"]);
        assert_eq!(blaze_protocol::cbor::from_slice::<MountTable>(&bytes).unwrap(), table);

        let file = Found::Here(vec![7u8; 64]);
        let written = blaze_protocol::cbor::to_vec(&file).unwrap();
        assert!(written.len() < 64 + 16, "{} bytes", written.len());
        assert_eq!(blaze_protocol::cbor::from_slice::<Found<Vec<u8>>>(&written).unwrap(), file);
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
        let mut known = vec!["/home/u/.cargo", "/home/u", "/"];
        known.sort();
        assert!(known_file(&known, "/home/u/.cargo/config.toml"));
        assert!(known_file(&known, "/home/u/.cargo/config"));
        assert!(known_file(&known, "/.cargo/config.toml"));
        assert!(known_file(&known, "/Cargo.toml"));
        assert!(!known_file(&known, "/etc/passwd/x"));
        assert!(!known_file(&known, "/home/u/.cargo/registry/src/x/Cargo.toml"));
    }

    #[test]
    fn what_a_run_missed_is_read_once_each_in_order() {
        let mut snapshot = Snapshot::default();
        snapshot.files.insert("/w/a".to_owned(), Found::Absent);
        snapshot.files.insert("/w/z".to_owned(), Found::Absent);
        let mut state = State::of(snapshot);
        assert_eq!(state.file("/w/c"), None);
        assert_eq!(state.dir("/w"), None);
        assert_eq!(state.file("/w/a"), Some(&Found::Absent));
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
        assert_eq!(state.files.used().keys().collect::<Vec<_>>(), ["/w/a"]);
    }

    #[test]
    fn already_normal_paths_are_borrowed() {
        assert!(matches!(normal_ref(Path::new("/w/a/Cargo.toml")), Some(Cow::Borrowed("/w/a/Cargo.toml"))));
        assert!(matches!(normal_ref(Path::new("/")), Some(Cow::Borrowed("/"))));
        assert_eq!(normal_ref(Path::new("/w/./a/../b")).as_deref(), Some("/w/b"));
        assert_eq!(normal_ref(Path::new("/w/a/")).as_deref(), Some("/w/a"));
        assert_eq!(normal_ref(Path::new("w")), None);
    }

    #[test]
    fn a_comment_leaves_the_canonical_form() {
        let one = "[package]\nname = \"a\" # the name\nversion = \"0.1.0\"\n";
        let two = "# a comment\n[package]\nversion = \"0.1.0\"\nname    = \"a\"\n";
        assert_eq!(canonical_toml(one.as_bytes()), canonical_toml(two.as_bytes()));
        assert_eq!(canonical_toml(b"[package"), None);
    }
}

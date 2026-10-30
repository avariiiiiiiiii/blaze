//! The Merkle tree: immutable nodes interned by content ID, so one ID is one [`NodeId`] and
//! equal subtrees are one node. [`Tree::apply`] makes a new root from a base and ops, all or
//! nothing, copying only the directories on the paths it changes; [`Tree::diff`] descends only
//! where IDs differ.
//!
//! A file node is its bytes alone; its [`Mode`] is in the entry that names it, as in git, so a
//! change of mode copies the directories above it and nothing else.
//!
//! The nodes live in the [`Table`] readers share ([`crate::store`]): a node is readable from
//! any thread the moment it is interned, and a collection publishes a new generation.

use std::{
    cmp::Ordering,
    collections::{BTreeMap, VecDeque},
    hash::BuildHasher,
    sync::Arc,
};

use blaze_ids::ContentId;
use hashbrown::{HashTable, hash_table};
use rustc_hash::FxBuildHasher;

use crate::{
    Change, ChangeKind, DIR_DOMAIN, FILE_DOMAIN, Mode, Op, OpError,
    reader::Reader,
    store::{DIR, Entries, FILE, Shape, Shared, Span, Table, mode_byte, mode_of, run},
};

/// A node of a [`Tree`]. Only meaningful for the tree that made it, and only until it collects.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub(crate) struct NodeId(pub(crate) u32);

/// An interned entry name.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub(crate) struct NameId(pub(crate) u32);

/// A directory's entry: its name, its node and, for a file, its mode ([`Mode::Regular`] for a
/// directory).
pub(crate) type Entry = (NameId, NodeId, Mode);

/// git's mode of a directory entry.
pub(crate) const DIR_MODE: u32 = 0o040000;

/// How many symlinks one path may pass through, as Linux allows.
const MAX_LINKS: usize = 40;

/// A file's bytes and mode.
pub(crate) type Blob = (Arc<[u8]>, Mode);

/// What a transaction leaves at each path it touched: a new file, or nothing.
pub(crate) type Pending = BTreeMap<String, Option<Blob>>;

/// The paths an op reads or changes.
pub(crate) fn op_paths(op: &Op) -> impl Iterator<Item = &str> {
    let (one, two) = match op {
        Op::Write { path, .. } | Op::Edit { path, .. } | Op::Delete { path } => (path, None),
        Op::Rename { from, to } => (from, Some(to)),
    };
    std::iter::once(one.as_str()).chain(two.map(String::as_str))
}

/// The parts of a path, checked.
pub(crate) fn parts(path: &str) -> Result<Vec<&str>, OpError> {
    let parts: Vec<&str> = path.split('/').collect();
    let bad = path.is_empty() || parts.iter().any(|it| it.is_empty() || *it == "." || *it == "..");
    if bad { Err(OpError::BadPath(path.to_owned())) } else { Ok(parts) }
}

/// A directory's entries as the tree reads them.
#[derive(Clone, Copy)]
pub(crate) struct Dir<'a>(Entries<'a>);

impl<'a> Dir<'a> {
    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }

    pub(crate) fn get(&self, at: usize) -> Entry {
        (NameId(self.0.names[at]), NodeId(self.0.nodes[at]), mode_of(self.0.modes[at]))
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = Entry> + 'a {
        let dir = *self;
        (0..dir.len()).map(move |at| dir.get(at))
    }

    pub(crate) fn is_dir(&self, at: usize) -> bool {
        self.0.is_dir(at)
    }
}

/// Every node and name made so far, interned. The one writer of its [`Shared`] storage.
pub(crate) struct Tree {
    shared: Arc<Shared>,
    /// How many nodes and blobs the table in use holds, and where its next run of entries may
    /// start.
    nodes: u32,
    blobs: u32,
    entries: u32,
    /// How many names there are, where the next one's bytes may start, and each by the hash of
    /// its bytes (computed once and kept, so growing never hashes again).
    names: u32,
    name_bytes: u32,
    name_index: HashTable<(u64, u32)>,
    empty: NodeId,
    /// Tables replaced by collections and not freed yet.
    retired: usize,
    /// A directory's hashed bytes, reused.
    scratch: Vec<u8>,
}

impl Tree {
    pub(crate) fn new() -> Tree {
        let mut tree = Tree {
            shared: Arc::new(Shared::new(Table::new(0))),
            nodes: 0,
            blobs: 0,
            entries: 0,
            names: 0,
            name_bytes: 0,
            name_index: HashTable::new(),
            empty: NodeId(0),
            retired: 0,
            scratch: Vec::new(),
        };
        tree.empty = tree.dir_sorted(&mut []);
        tree
    }

    /// A handle that reads every node from any thread.
    pub(crate) fn reader(&self) -> Reader {
        Reader::new(self.shared.clone())
    }

    fn table(&self) -> &Table {
        // SAFETY: this tree is the table's one writer, and replaces it only in `collect`,
        // through `&mut self`, so no reference from `&self` outlives it.
        unsafe { self.shared.table() }
    }

    /// Frees the tables collections replaced that no reader is in any more.
    pub(crate) fn reclaim(&mut self) {
        if self.retired > 0 {
            self.retired = self.shared.reclaim();
        }
    }

    /// The root with no files.
    pub(crate) fn empty(&self) -> NodeId {
        self.empty
    }

    /// How many nodes it holds.
    pub(crate) fn len(&self) -> usize {
        self.nodes as usize
    }

    fn shape(&self, node: NodeId) -> Shape {
        assert!(node.0 < self.nodes, "node {} of a tree of {}", node.0, self.nodes);
        // SAFETY: every node below `nodes` was written by this tree.
        unsafe { self.table().shape(node.0) }
    }

    pub(crate) fn id(&self, node: NodeId) -> ContentId {
        assert!(node.0 < self.nodes, "node {} of a tree of {}", node.0, self.nodes);
        // SAFETY: as in `shape`.
        unsafe { *self.table().id(node.0) }
    }

    pub(crate) fn node(&self, id: &ContentId) -> Option<NodeId> {
        self.table().find(id).ok().map(NodeId)
    }

    pub(crate) fn is_dir(&self, node: NodeId) -> bool {
        self.shape(node).is_dir()
    }

    /// A directory's entries, sorted by name; `None` for a file.
    pub(crate) fn entries(&self, node: NodeId) -> Option<Dir<'_>> {
        let shape = self.shape(node);
        // SAFETY: a directory's shape of this tree's.
        shape.is_dir().then(|| Dir(unsafe { self.table().entries(shape) }))
    }

    /// A file's bytes; `None` for a directory.
    pub(crate) fn bytes(&self, node: NodeId) -> Option<&Arc<[u8]>> {
        let shape = self.shape(node);
        // SAFETY: a file's shape of this tree's.
        (!shape.is_dir()).then(|| unsafe { self.table().blob(shape) })
    }

    pub(crate) fn name(&mut self, name: &str) -> NameId {
        let hash = FxBuildHasher.hash_one(name.as_bytes());
        let Tree { shared, name_index, names, name_bytes, .. } = self;
        let store = &shared.names;
        // SAFETY: every interned name was written by this tree.
        let same =
            |&(at, id): &(u64, u32)| at == hash && unsafe { store.get(id) } == name.as_bytes();
        match name_index.entry(hash, same, |&(at, _)| at) {
            hash_table::Entry::Occupied(it) => NameId(it.get().1),
            hash_table::Entry::Vacant(it) => {
                let len = u32::try_from(name.len()).expect("a name under 4 GiB");
                let start = run(*name_bytes, len);
                let id = *names;
                assert!(id != u32::MAX, "2^32 names");
                // SAFETY: the writer, at offsets past every name written, published by the
                // directory that names it.
                unsafe {
                    store.bytes.write_run(start, name.as_bytes());
                    store.spans.write(id, Span { start, len });
                }
                *name_bytes = start + len;
                *names += 1;
                it.insert((hash, id));
                NameId(id)
            }
        }
    }

    fn name_bytes(&self, name: NameId) -> &[u8] {
        assert!(name.0 < self.names, "name {} of {}", name.0, self.names);
        // SAFETY: every name below `names` was written by this tree.
        unsafe { self.shared.names.get(name.0) }
    }

    pub(crate) fn name_str(&self, name: NameId) -> &str {
        // SAFETY: a name is interned only from a `&str`.
        unsafe { std::str::from_utf8_unchecked(self.name_bytes(name)) }
    }

    /// Writes a node found missing at `place` of the index.
    fn push(&mut self, id: ContentId, shape: Shape, place: usize) -> NodeId {
        let node = self.nodes;
        assert!(node < u32::MAX - 1, "2^32 nodes");
        let table = self.table();
        // SAFETY: the writer, at the next node, indexed (so published) only after.
        unsafe {
            table.ids.write(node, id);
            table.shapes.write(node, shape);
        }
        table.index(&id, node, place, node as usize);
        self.nodes += 1;
        NodeId(node)
    }

    /// The node of a file with these bytes.
    pub(crate) fn file(&mut self, bytes: Arc<[u8]>) -> NodeId {
        let id = ContentId::of(FILE_DOMAIN, &bytes);
        let place = match self.table().find(&id) {
            Ok(node) => return NodeId(node),
            Err(place) => place,
        };
        let blob = self.blobs;
        // SAFETY: the writer, at the next blob, named only by the node pushed after.
        unsafe { self.table().blobs.write(blob, bytes) };
        self.blobs += 1;
        self.push(id, Shape { start: blob, len: FILE }, place)
    }

    /// The node of a directory with these entries, in any order; names must be distinct.
    pub(crate) fn dir(&mut self, mut entries: Vec<Entry>) -> NodeId {
        entries.sort_unstable_by(|a, b| self.name_bytes(a.0).cmp(self.name_bytes(b.0)));
        self.dir_sorted(&mut entries)
    }

    /// The node of a directory with these entries, sorted by name and distinct. A directory's
    /// entry is given [`Mode::Regular`] whatever it says, so one tree is one ID. Its hashed
    /// bytes are each entry's git mode code, length-prefixed name and child ID, as
    /// [`DIR_DOMAIN`] says, built in one buffer and hashed in one call.
    fn dir_sorted(&mut self, entries: &mut [Entry]) -> NodeId {
        let mut bytes = std::mem::take(&mut self.scratch);
        bytes.clear();
        for entry in entries.iter_mut() {
            let (name, child, mode) = *entry;
            let code = if self.is_dir(child) {
                entry.2 = Mode::Regular;
                DIR_MODE
            } else {
                mode.git()
            };
            let name = self.name_bytes(name);
            bytes.extend_from_slice(&code.to_le_bytes());
            bytes.extend_from_slice(&(name.len() as u64).to_le_bytes());
            bytes.extend_from_slice(name);
            bytes.extend_from_slice(&self.id(child).0);
        }
        let id = ContentId::of(DIR_DOMAIN, &bytes);
        self.scratch = bytes;
        let place = match self.table().find(&id) {
            Ok(node) => return NodeId(node),
            Err(place) => place,
        };
        let len = u32::try_from(entries.len()).expect("fewer than 2^32 entries");
        let start = run(self.entries, len);
        let table = self.table();
        for (at, &(name, child, mode)) in (start..).zip(entries.iter()) {
            let byte = if self.is_dir(child) { DIR } else { mode_byte(mode) };
            // SAFETY: the writer, in a run past every entry written, named only by the node
            // pushed after.
            unsafe {
                table.entry_names.write(at, name.0);
                table.entry_nodes.write(at, child.0);
                table.entry_modes.write(at, byte);
            }
        }
        self.entries = start + len;
        self.push(id, Shape { start, len }, place)
    }

    /// git's code for a directory entry: 040000 for a directory, else its file's mode.
    pub(crate) fn code(&self, child: NodeId, mode: Mode) -> u32 {
        if self.is_dir(child) { DIR_MODE } else { mode.git() }
    }

    fn child(&self, dir: NodeId, name: &str) -> Option<(NodeId, Mode)> {
        let Dir(entries) = self.entries(dir)?;
        let at = entries.find(&self.shared.names, name.as_bytes())?;
        Some((NodeId(entries.nodes[at]), mode_of(entries.modes[at])))
    }

    /// The node at `path` under `root` and its mode, as it is (a symlink is not followed);
    /// `""` is the root itself.
    pub(crate) fn entry(&self, root: NodeId, path: &str) -> Option<(NodeId, Mode)> {
        if path.is_empty() {
            return Some((root, Mode::Regular));
        }
        path.split('/').try_fold((root, Mode::Regular), |(node, _), name| self.child(node, name))
    }

    /// The node at `path` under `root`; `""` is the root itself.
    pub(crate) fn lookup(&self, root: NodeId, path: &str) -> Option<NodeId> {
        self.entry(root, path).map(|it| it.0)
    }

    /// The bytes of the file at `path` under `root`.
    pub(crate) fn read(&self, root: NodeId, path: &str) -> Option<&Arc<[u8]>> {
        self.bytes(self.lookup(root, path)?)
    }

    /// The file at `path` under `root`, with its mode.
    pub(crate) fn blob(&self, root: NodeId, path: &str) -> Option<Blob> {
        let (node, mode) = self.entry(root, path)?;
        Some((self.bytes(node)?.clone(), mode))
    }

    /// The node at `path` under `root` with every symlink on the way followed, as the system
    /// opens a path: a relative target is taken from the link's directory. `Ok(None)` when
    /// nothing is there or a file is passed through as a directory.
    pub(crate) fn resolve(
        &self,
        root: NodeId,
        path: &str,
    ) -> Result<Option<(NodeId, Mode)>, OpError> {
        if !path.is_empty() {
            parts(path)?;
        }
        let mut rest: VecDeque<String> =
            path.split('/').filter(|it| !it.is_empty()).map(str::to_owned).collect();
        // The directories from the root to where the walk is, and their names.
        let mut dirs = vec![root];
        let mut names: Vec<String> = Vec::new();
        let mut links = 0;
        let mut last_link: Option<(String, String)> = None;
        let bad = |last: &Option<(String, String)>| {
            let (path, target) = last.clone().expect("only a link's target leaves the tree");
            OpError::BadLink { path, target }
        };
        while let Some(part) = rest.pop_front() {
            match part.as_str() {
                "" | "." => continue,
                ".." => {
                    if dirs.len() == 1 {
                        return Err(bad(&last_link));
                    }
                    dirs.pop();
                    names.pop();
                    continue;
                }
                name => {
                    let dir = *dirs.last().expect("the root");
                    let Some((child, mode)) = self.child(dir, name) else { return Ok(None) };
                    if mode == Mode::Symlink {
                        let link: Vec<&str> =
                            names.iter().map(String::as_str).chain([name]).collect();
                        let link = link.join("/");
                        let bytes = self.bytes(child).expect("a symlink is a file");
                        let target = String::from_utf8_lossy(bytes).into_owned();
                        links += 1;
                        let leaves = target.is_empty() || target.starts_with('/');
                        last_link = Some((link, target.clone()));
                        if links > MAX_LINKS || leaves {
                            return Err(bad(&last_link));
                        }
                        for part in target.split('/').rev() {
                            rest.push_front(part.to_owned());
                        }
                        continue;
                    }
                    if rest.is_empty() {
                        return Ok(Some((child, mode)));
                    }
                    if !self.is_dir(child) {
                        return Ok(None);
                    }
                    dirs.push(child);
                    names.push(name.to_owned());
                }
            }
        }
        Ok(Some((*dirs.last().expect("the root"), Mode::Regular)))
    }

    /// Every file under `node`, with its path below `prefix` and its mode, in path order. `node`
    /// itself, when a file, is taken as `mode`.
    pub(crate) fn files(
        &self,
        node: NodeId,
        mode: Mode,
        prefix: &str,
    ) -> Vec<(String, NodeId, Mode)> {
        let mut out = Vec::new();
        self.each_file(node, mode, &mut prefix.to_owned(), &mut |path, node, mode| {
            out.push((path.to_owned(), node, mode));
        });
        out
    }

    /// Calls `f` with every file under `node`, its path below `path` and its mode, in path
    /// order, building each path in the one buffer.
    fn each_file(
        &self,
        node: NodeId,
        mode: Mode,
        path: &mut String,
        f: &mut impl FnMut(&str, NodeId, Mode),
    ) {
        let Some(entries) = self.entries(node) else {
            f(path, node, mode);
            return;
        };
        for (name, child, mode) in entries.iter() {
            let len = path.len();
            if !path.is_empty() {
                path.push('/');
            }
            path.push_str(self.name_str(name));
            self.each_file(child, mode, path, f);
            path.truncate(len);
        }
    }

    /// The root `ops` make of `base`, or why they can't. Each op sees what the ones before it
    /// did; `base` itself is never changed.
    pub(crate) fn apply(&mut self, base: NodeId, ops: &[Op]) -> Result<NodeId, OpError> {
        let mut pending = Pending::new();
        for op in ops {
            self.stage(base, &mut pending, op)?;
        }
        // By component, so each directory's changes are one run, in its entries' order.
        let mut changes: Vec<(Vec<&str>, Option<Blob>)> =
            pending.iter().map(|(path, blob)| (path.split('/').collect(), blob.clone())).collect();
        changes.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        Ok(self.rebuild(Some(base), &changes, 0).unwrap_or(self.empty))
    }

    /// The file at `path` as the transaction sees it so far: `Err` for a directory.
    fn current(&self, base: NodeId, pending: &Pending, path: &str) -> Result<Option<Blob>, ()> {
        if let Some(blob) = self.file_now(base, pending, path) {
            return Ok(Some(blob));
        }
        if self.has_files_under(base, pending, path) { Err(()) } else { Ok(None) }
    }

    /// The file at `path` as the transaction sees it so far, if a file is there.
    fn file_now(&self, base: NodeId, pending: &Pending, path: &str) -> Option<Blob> {
        match pending.get(path) {
            Some(blob) => blob.clone(),
            None if shadowed(pending, path) => None,
            None => self.blob(base, path),
        }
    }

    /// Whether the transaction sees any file under the directory `path`; stops at the first.
    fn has_files_under(&self, base: NodeId, pending: &Pending, path: &str) -> bool {
        let below = format!("{path}/");
        let staged = pending
            .range(below.clone()..)
            .take_while(|it| it.0.starts_with(&below))
            .any(|it| it.1.is_some());
        if staged {
            return true;
        }
        if pending.contains_key(path) || shadowed(pending, path) {
            return false;
        }
        let Some(dir) = self.lookup(base, path).filter(|it| self.is_dir(*it)) else { return false };
        let mut stack = vec![(dir, path.to_owned())];
        while let Some((node, at)) = stack.pop() {
            for (name, child, _) in self.entries(node).expect("a directory").iter() {
                let file = format!("{at}/{}", self.name_str(name));
                if pending.contains_key(&file) {
                    continue;
                }
                if self.is_dir(child) {
                    stack.push((child, file));
                } else {
                    return true;
                }
            }
        }
        false
    }

    /// The files under the directory `path`, as the transaction sees them so far.
    fn files_under(&self, base: NodeId, pending: &Pending, path: &str) -> Vec<(String, Blob)> {
        let mut found: BTreeMap<String, Blob> = BTreeMap::new();
        let gone = pending.contains_key(path) || shadowed(pending, path);
        if !gone && let Some(dir) = self.lookup(base, path).filter(|it| self.is_dir(*it)) {
            for (file, node, mode) in self.files(dir, Mode::Regular, path) {
                if !pending.contains_key(&file) && !shadowed(pending, &file) {
                    found.insert(file, (self.bytes(node).expect("a file").clone(), mode));
                }
            }
        }
        let below = format!("{path}/");
        for (file, blob) in pending.range(below.clone()..) {
            if !file.starts_with(&below) {
                break;
            }
            if let Some(blob) = blob {
                found.insert(file.clone(), blob.clone());
            }
        }
        found.into_iter().collect()
    }

    /// Checks `op` against `base` with the ops staged before it in `pending`, and stages it.
    pub(crate) fn stage(
        &self,
        base: NodeId,
        pending: &mut Pending,
        op: &Op,
    ) -> Result<(), OpError> {
        for path in op_paths(op) {
            parts(path)?;
        }
        let file_at = |pending: &Pending, path: &str| -> Result<Option<Blob>, OpError> {
            self.current(base, pending, path).map_err(|()| OpError::IsADirectory(path.to_owned()))
        };
        let no_file_above = |pending: &Pending, path: &str| -> Result<(), OpError> {
            for up in ancestors(path) {
                if self.file_now(base, pending, up).is_some() {
                    return Err(OpError::NotADirectory(path.to_owned()));
                }
            }
            Ok(())
        };
        match op {
            Op::Write { path, bytes, mode } => {
                no_file_above(pending, path)?;
                file_at(pending, path)?;
                pending.insert(path.clone(), Some((Arc::from(bytes.as_slice()), *mode)));
            }
            Op::Edit { path, at, len, bytes } => {
                let (old, mode) =
                    file_at(pending, path)?.ok_or_else(|| OpError::NotFound(path.clone()))?;
                let size = old.len() as u64;
                if at.checked_add(*len).is_none_or(|end| end > size) {
                    return Err(OpError::OutOfRange {
                        path: path.clone(),
                        at: *at,
                        len: *len,
                        size,
                    });
                }
                let (at, end) = (*at as usize, (*at + *len) as usize);
                let new: Arc<[u8]> = [&old[..at], bytes, &old[end..]].concat().into();
                pending.insert(path.clone(), Some((new, mode)));
            }
            Op::Delete { path } => match file_at(pending, path) {
                Ok(Some(_)) => {
                    pending.insert(path.clone(), None);
                }
                Ok(None) => return Err(OpError::NotFound(path.clone())),
                Err(_) => {
                    for (file, _) in self.files_under(base, pending, path) {
                        pending.insert(file, None);
                    }
                    pending.insert(path.clone(), None);
                }
            },
            Op::Rename { from, to } => {
                if to.starts_with(&format!("{from}/")) {
                    return Err(OpError::BadPath(to.clone()));
                }
                if from == to {
                    return Err(OpError::Exists(to.clone()));
                }
                let moved: Vec<(String, Blob)> = match file_at(pending, from) {
                    Ok(Some(blob)) => vec![(String::new(), blob)],
                    Ok(None) => return Err(OpError::NotFound(from.clone())),
                    Err(_) => self
                        .files_under(base, pending, from)
                        .into_iter()
                        .map(|(file, blob)| (file[from.len()..].to_owned(), blob))
                        .collect(),
                };
                match file_at(pending, to) {
                    Ok(None) => {}
                    _ => return Err(OpError::Exists(to.clone())),
                }
                no_file_above(pending, to)?;
                self.stage(base, pending, &Op::Delete { path: from.clone() })?;
                for (rest, blob) in moved {
                    pending.insert(format!("{to}{rest}"), Some(blob));
                }
            }
        }
        Ok(())
    }

    /// The directory `dir` with `changes` made to it (paths whose first `depth` components lead
    /// to it, sorted by component): `None` when it is left with nothing in it. Its entries and
    /// the changes are merged in one pass, as both are in name order.
    fn rebuild(
        &mut self,
        dir: Option<NodeId>,
        changes: &[(Vec<&str>, Option<Blob>)],
        depth: usize,
    ) -> Option<NodeId> {
        let old: Vec<Entry> =
            dir.and_then(|it| self.entries(it)).map(|it| it.iter().collect()).unwrap_or_default();
        let mut out: Vec<Entry> = Vec::with_capacity(old.len() + changes.len());
        let mut kept = 0;
        let mut start = 0;
        while start < changes.len() {
            let name = changes[start].0[depth];
            let end = start + changes[start..].iter().take_while(|it| it.0[depth] == name).count();
            let group = &changes[start..end];
            start = end;
            while kept < old.len() && self.name_bytes(old[kept].0) < name.as_bytes() {
                out.push(old[kept]);
                kept += 1;
            }
            let existing = (kept < old.len() && self.name_bytes(old[kept].0) == name.as_bytes())
                .then(|| {
                    kept += 1;
                    old[kept - 1]
                });
            // The change at this name itself sorts first in its group: its path is the shortest.
            let (here, deeper) = match group[0].0.len() == depth + 1 {
                true => (Some(&group[0].1), &group[1..]),
                false => (None, group),
            };
            let child = match here {
                Some(here) if !deeper.iter().any(|it| it.1.is_some()) => {
                    here.clone().map(|(bytes, mode)| (self.file(bytes), mode))
                }
                // A directory, made of what was there if that was one and nothing replaced it.
                _ => {
                    let was =
                        existing.map(|it| it.1).filter(|it| here.is_none() && self.is_dir(*it));
                    self.rebuild(was, deeper, depth + 1).map(|dir| (dir, Mode::Regular))
                }
            };
            if let Some((child, mode)) = child {
                let name = match existing {
                    Some(it) => it.0,
                    None => self.name(name),
                };
                out.push((name, child, mode));
            }
        }
        out.extend_from_slice(&old[kept..]);
        if out.is_empty() && dir != Some(self.empty) {
            return None;
        }
        Some(self.dir_sorted(&mut out))
    }

    /// The files that differ between `from` and `to`, in path order. A file whose bytes or mode
    /// changed is [`ChangeKind::Modified`].
    pub(crate) fn diff(&self, from: NodeId, to: NodeId) -> Vec<Change> {
        let mut out = Vec::new();
        let root = |node| Some((node, Mode::Regular));
        self.diff_into(root(from), root(to), &mut String::new(), &mut out);
        out
    }

    fn diff_into(
        &self,
        from: Option<(NodeId, Mode)>,
        to: Option<(NodeId, Mode)>,
        path: &mut String,
        out: &mut Vec<Change>,
    ) {
        if from == to {
            return;
        }
        let all = |(node, mode): (NodeId, Mode),
                   kind: ChangeKind,
                   path: &mut String,
                   out: &mut Vec<Change>| {
            self.each_file(node, mode, path, &mut |path, _, _| {
                out.push(Change { path: path.to_owned(), kind });
            });
        };
        match (from, to) {
            (Some(a), Some(b)) => match (self.entries(a.0), self.entries(b.0)) {
                (None, None) => out.push(Change { path: path.clone(), kind: ChangeKind::Modified }),
                (Some(x), Some(y)) => {
                    let (mut i, mut j) = (0, 0);
                    while i < x.len() || j < y.len() {
                        let order = match (i < x.len(), j < y.len()) {
                            (true, true) if x.0.names[i] == y.0.names[j] => Ordering::Equal,
                            (true, true) => self
                                .name_bytes(NameId(x.0.names[i]))
                                .cmp(self.name_bytes(NameId(y.0.names[j]))),
                            (true, false) => Ordering::Less,
                            _ => Ordering::Greater,
                        };
                        let side = |dir: &Dir<'_>, at: usize| {
                            let (_, node, mode) = dir.get(at);
                            Some((node, mode))
                        };
                        let (name, left, right) = match order {
                            Ordering::Less => (x.0.names[i], side(&x, i), None),
                            Ordering::Greater => (y.0.names[j], None, side(&y, j)),
                            Ordering::Equal => (x.0.names[i], side(&x, i), side(&y, j)),
                        };
                        i += usize::from(order.is_le());
                        j += usize::from(order.is_ge());
                        // A shared subtree: skipped without building its path.
                        if left == right {
                            continue;
                        }
                        let len = path.len();
                        if !path.is_empty() {
                            path.push('/');
                        }
                        path.push_str(self.name_str(NameId(name)));
                        self.diff_into(left, right, path, out);
                        path.truncate(len);
                    }
                }
                // A file replaced by a directory or the other way: the file's path comes before
                // every path under it.
                (None, Some(_)) => {
                    out.push(Change { path: path.clone(), kind: ChangeKind::Removed });
                    all(b, ChangeKind::Added, path, out);
                }
                (Some(_), None) => {
                    out.push(Change { path: path.clone(), kind: ChangeKind::Added });
                    all(a, ChangeKind::Removed, path, out);
                }
            },
            (Some(a), None) => all(a, ChangeKind::Removed, path, out),
            (None, Some(b)) => all(b, ChangeKind::Added, path, out),
            (None, None) => {}
        }
    }

    /// Drops every node no root in `live` reaches, and renumbers the rest into a new table that
    /// readers move to at once; `live` is updated in place. Names are kept. Returns each old
    /// node's new one, `None` for a node dropped.
    pub(crate) fn collect(&mut self, live: &mut [NodeId]) -> Vec<Option<NodeId>> {
        let len = self.nodes as usize;
        let mut reached = vec![0u64; len.div_ceil(64)];
        let mut stack: Vec<NodeId> = live.iter().copied().chain([self.empty]).collect();
        while let Some(node) = stack.pop() {
            let (word, bit) = (node.0 as usize / 64, 1u64 << (node.0 % 64));
            if reached[word] & bit != 0 {
                continue;
            }
            reached[word] |= bit;
            if let Some(entries) = self.entries(node) {
                stack.extend(entries.0.nodes.iter().map(|&it| NodeId(it)));
            }
        }
        let count = reached.iter().map(|it| it.count_ones() as usize).sum();
        let next = Table::new(count);
        let old = self.table();
        let mut renumbered = vec![None; len];
        let (mut nodes, mut blobs, mut entries) = (0u32, 0u32, 0u32);
        // Children have smaller numbers than their parents, so they are renumbered first.
        for at in 0..len as u32 {
            if reached[at as usize / 64] & (1 << (at % 64)) == 0 {
                continue;
            }
            // SAFETY: every node below `nodes` was written by this tree.
            let (id, shape) = unsafe { (*old.id(at), old.shape(at)) };
            let shape = if shape.is_dir() {
                // SAFETY: a directory's shape, of this tree's.
                let from = unsafe { old.entries(shape) };
                let start = run(entries, shape.len);
                for (place, k) in (start..).zip(0..from.len()) {
                    let child = renumbered[from.nodes[k] as usize].expect("a child is reached");
                    // SAFETY: the writer of the new table, which no reader has yet.
                    unsafe {
                        next.entry_names.write(place, from.names[k]);
                        next.entry_nodes.write(place, child);
                        next.entry_modes.write(place, from.modes[k]);
                    }
                }
                entries = start + shape.len;
                Shape { start, len: shape.len }
            } else {
                // SAFETY: as above.
                unsafe { next.blobs.write(blobs, old.blob(shape).clone()) };
                blobs += 1;
                Shape { start: blobs - 1, len: FILE }
            };
            // SAFETY: as above.
            unsafe {
                next.ids.write(nodes, id);
                next.shapes.write(nodes, shape);
            }
            let Err(place) = next.find(&id) else { unreachable!("a node kept twice") };
            next.index(&id, nodes, place, nodes as usize);
            renumbered[at as usize] = Some(NodeId(nodes));
            nodes += 1;
        }
        self.retired = self.shared.replace(next);
        (self.nodes, self.blobs, self.entries) = (nodes, blobs, entries);
        self.empty = renumbered[self.empty.0 as usize].expect("the empty root is kept");
        for root in live {
            *root = renumbered[root.0 as usize].expect("a live root is kept");
        }
        renumbered
    }
}

/// Whether the transaction deleted or replaced with a file a directory above `path`, so the
/// base's contents there are gone.
fn shadowed(pending: &Pending, path: &str) -> bool {
    ancestors(path).any(|up| pending.contains_key(up))
}

/// `a/b/c` -> `a/b`, `a`.
pub(crate) fn ancestors(path: &str) -> impl Iterator<Item = &str> {
    let mut rest = path;
    std::iter::from_fn(move || {
        let at = rest.rfind('/')?;
        rest = &rest[..at];
        Some(rest)
    })
}

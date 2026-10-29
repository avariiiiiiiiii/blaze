//! The Merkle tree: immutable nodes interned by content ID, so one ID is one [`NodeId`] and
//! equal subtrees are one node. [`Tree::apply`] makes a new root from a base and ops, all or
//! nothing, copying only the directories on the paths it changes; [`Tree::diff`] descends only
//! where IDs differ.

use std::{cmp::Ordering, collections::BTreeMap, sync::Arc};

use blaze_ids::{ContentId, Hasher};
use rustc_hash::FxHashMap;

use crate::{Change, ChangeKind, DIR_DOMAIN, FILE_DOMAIN, Op, OpError};

/// A node of a [`Tree`]. Only meaningful for the tree that made it, and only until it collects.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub(crate) struct NodeId(pub(crate) u32);

/// An interned entry name.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub(crate) struct NameId(u32);

#[derive(Clone)]
enum Node {
    File {
        id: ContentId,
        bytes: Arc<[u8]>,
    },
    /// Sorted by the names' bytes, so the ID does not depend on interning order.
    Dir {
        id: ContentId,
        entries: Box<[(NameId, NodeId)]>,
    },
}

impl Node {
    fn id(&self) -> ContentId {
        match self {
            Node::File { id, .. } | Node::Dir { id, .. } => *id,
        }
    }
}

/// What a transaction leaves at each path it touched: new bytes, or nothing.
pub(crate) type Pending = BTreeMap<String, Option<Arc<[u8]>>>;

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

/// Every node and name made so far, interned.
#[derive(Clone)]
pub(crate) struct Tree {
    nodes: Vec<Node>,
    by_id: FxHashMap<ContentId, NodeId>,
    names: Vec<Box<str>>,
    name_ids: FxHashMap<Box<str>, NameId>,
    empty: NodeId,
}

impl Tree {
    pub(crate) fn new() -> Tree {
        let mut tree = Tree {
            nodes: Vec::new(),
            by_id: FxHashMap::default(),
            names: Vec::new(),
            name_ids: FxHashMap::default(),
            empty: NodeId(0),
        };
        tree.empty = tree.dir(Vec::new());
        tree
    }

    /// The root with no files.
    pub(crate) fn empty(&self) -> NodeId {
        self.empty
    }

    /// How many nodes it holds.
    pub(crate) fn len(&self) -> usize {
        self.nodes.len()
    }

    pub(crate) fn id(&self, node: NodeId) -> ContentId {
        self.nodes[node.0 as usize].id()
    }

    pub(crate) fn node(&self, id: &ContentId) -> Option<NodeId> {
        self.by_id.get(id).copied()
    }

    pub(crate) fn is_dir(&self, node: NodeId) -> bool {
        matches!(self.nodes[node.0 as usize], Node::Dir { .. })
    }

    /// A directory's entries, sorted by name; `None` for a file.
    pub(crate) fn entries(&self, node: NodeId) -> Option<&[(NameId, NodeId)]> {
        match &self.nodes[node.0 as usize] {
            Node::Dir { entries, .. } => Some(entries),
            Node::File { .. } => None,
        }
    }

    /// A file's bytes; `None` for a directory.
    pub(crate) fn bytes(&self, node: NodeId) -> Option<&Arc<[u8]>> {
        match &self.nodes[node.0 as usize] {
            Node::File { bytes, .. } => Some(bytes),
            Node::Dir { .. } => None,
        }
    }

    pub(crate) fn name(&mut self, name: &str) -> NameId {
        if let Some(&id) = self.name_ids.get(name) {
            return id;
        }
        let id = NameId(u32::try_from(self.names.len()).expect("fewer than 2^32 names"));
        self.names.push(name.into());
        self.name_ids.insert(name.into(), id);
        id
    }

    pub(crate) fn name_str(&self, name: NameId) -> &str {
        &self.names[name.0 as usize]
    }

    fn intern(&mut self, id: ContentId, node: impl FnOnce() -> Node) -> NodeId {
        if let Some(&node) = self.by_id.get(&id) {
            return node;
        }
        let at = NodeId(u32::try_from(self.nodes.len()).expect("fewer than 2^32 nodes"));
        self.nodes.push(node());
        self.by_id.insert(id, at);
        at
    }

    /// The node of a file with these bytes.
    pub(crate) fn file(&mut self, bytes: Arc<[u8]>) -> NodeId {
        let id = ContentId::of(FILE_DOMAIN, &bytes);
        self.intern(id, || Node::File { id, bytes })
    }

    /// The node of a directory with these entries, in any order; names must be distinct.
    pub(crate) fn dir(&mut self, mut entries: Vec<(NameId, NodeId)>) -> NodeId {
        entries
            .sort_unstable_by(|a, b| self.names[a.0.0 as usize].cmp(&self.names[b.0.0 as usize]));
        let mut hasher = Hasher::new(DIR_DOMAIN);
        for &(name, child) in &entries {
            hasher.update(&[u8::from(self.is_dir(child))]);
            hasher.part(self.names[name.0 as usize].as_bytes());
            hasher.id(&self.id(child));
        }
        let id = hasher.finish();
        self.intern(id, || Node::Dir { id, entries: entries.into_boxed_slice() })
    }

    fn child(&self, dir: NodeId, name: &str) -> Option<NodeId> {
        let entries = self.entries(dir)?;
        let at = entries.binary_search_by(|(it, _)| (*self.names[it.0 as usize]).cmp(name)).ok()?;
        Some(entries[at].1)
    }

    /// The node at `path` under `root`; `""` is the root itself.
    pub(crate) fn lookup(&self, root: NodeId, path: &str) -> Option<NodeId> {
        if path.is_empty() {
            return Some(root);
        }
        path.split('/').try_fold(root, |node, name| self.child(node, name))
    }

    /// The bytes of the file at `path` under `root`.
    pub(crate) fn read(&self, root: NodeId, path: &str) -> Option<&Arc<[u8]>> {
        self.bytes(self.lookup(root, path)?)
    }

    /// Every file under `node`, with its path below `prefix`, in path order.
    pub(crate) fn files(&self, node: NodeId, prefix: &str) -> Vec<(String, NodeId)> {
        let mut out = Vec::new();
        self.walk(node, &mut prefix.to_owned(), &mut out);
        out
    }

    fn walk(&self, node: NodeId, path: &mut String, out: &mut Vec<(String, NodeId)>) {
        let Some(entries) = self.entries(node) else {
            out.push((path.clone(), node));
            return;
        };
        for &(name, child) in entries {
            let len = path.len();
            if !path.is_empty() {
                path.push('/');
            }
            path.push_str(&self.names[name.0 as usize]);
            self.walk(child, path, out);
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
        let mut changes: Vec<(Vec<&str>, Option<Arc<[u8]>>)> = Vec::with_capacity(pending.len());
        for (path, bytes) in &pending {
            changes.push((path.split('/').collect(), bytes.clone()));
        }
        Ok(self.rebuild(Some(base), &changes).unwrap_or(self.empty))
    }

    /// The bytes at `path` as the transaction sees it so far: `Err` for a directory.
    fn current(
        &self,
        base: NodeId,
        pending: &Pending,
        path: &str,
    ) -> Result<Option<Arc<[u8]>>, ()> {
        if let Some(bytes) = self.file_now(base, pending, path) {
            return Ok(Some(bytes));
        }
        if self.has_files_under(base, pending, path) { Err(()) } else { Ok(None) }
    }

    /// The file at `path` as the transaction sees it so far, if a file is there.
    fn file_now(&self, base: NodeId, pending: &Pending, path: &str) -> Option<Arc<[u8]>> {
        match pending.get(path) {
            Some(bytes) => bytes.clone(),
            None if shadowed(pending, path) => None,
            None => self.read(base, path).cloned(),
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
            for &(name, child) in self.entries(node).expect("a directory") {
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
    fn files_under(&self, base: NodeId, pending: &Pending, path: &str) -> Vec<(String, Arc<[u8]>)> {
        let mut found: BTreeMap<String, Arc<[u8]>> = BTreeMap::new();
        let gone = pending.contains_key(path) || shadowed(pending, path);
        if !gone && let Some(dir) = self.lookup(base, path).filter(|it| self.is_dir(*it)) {
            for (file, node) in self.files(dir, path) {
                if !pending.contains_key(&file) && !shadowed(pending, &file) {
                    found.insert(file, self.bytes(node).expect("a file").clone());
                }
            }
        }
        let below = format!("{path}/");
        for (file, bytes) in pending.range(below.clone()..) {
            if !file.starts_with(&below) {
                break;
            }
            if let Some(bytes) = bytes {
                found.insert(file.clone(), bytes.clone());
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
        let file_at = |pending: &Pending, path: &str| -> Result<Option<Arc<[u8]>>, OpError> {
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
            Op::Write { path, bytes } => {
                no_file_above(pending, path)?;
                file_at(pending, path)?;
                pending.insert(path.clone(), Some(Arc::from(bytes.as_slice())));
            }
            Op::Edit { path, at, len, bytes } => {
                let old = file_at(pending, path)?.ok_or_else(|| OpError::NotFound(path.clone()))?;
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
                pending.insert(path.clone(), Some(new));
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
                let moved: Vec<(String, Arc<[u8]>)> = match file_at(pending, from) {
                    Ok(Some(bytes)) => vec![(String::new(), bytes)],
                    Ok(None) => return Err(OpError::NotFound(from.clone())),
                    Err(_) => self
                        .files_under(base, pending, from)
                        .into_iter()
                        .map(|(file, bytes)| (file[from.len()..].to_owned(), bytes))
                        .collect(),
                };
                match file_at(pending, to) {
                    Ok(None) => {}
                    _ => return Err(OpError::Exists(to.clone())),
                }
                no_file_above(pending, to)?;
                self.stage(base, pending, &Op::Delete { path: from.clone() })?;
                for (rest, bytes) in moved {
                    pending.insert(format!("{to}{rest}"), Some(bytes));
                }
            }
        }
        Ok(())
    }

    /// The directory `dir` with `changes` (paths below it, sorted) made to it: `None` when it is
    /// left with nothing in it.
    fn rebuild(
        &mut self,
        dir: Option<NodeId>,
        changes: &[(Vec<&str>, Option<Arc<[u8]>>)],
    ) -> Option<NodeId> {
        let mut entries: Vec<(NameId, NodeId)> =
            dir.and_then(|it| self.entries(it)).map(<[_]>::to_vec).unwrap_or_default();
        let mut start = 0;
        while start < changes.len() {
            let name = changes[start].0[0];
            let end = start + changes[start..].iter().take_while(|it| it.0[0] == name).count();
            let group = &changes[start..end];
            start = end;
            let name_id = self.name(name);
            let existing = entries.iter().position(|it| it.0 == name_id);
            let here = group.iter().find(|it| it.0.len() == 1).map(|it| &it.1);
            let deeper: Vec<(Vec<&str>, Option<Arc<[u8]>>)> = group
                .iter()
                .filter(|it| it.0.len() > 1)
                .map(|(path, bytes)| (path[1..].to_vec(), bytes.clone()))
                .collect();
            let child = match here {
                Some(here) if !deeper.iter().any(|it| it.1.is_some()) => {
                    here.clone().map(|bytes| self.file(bytes))
                }
                // A directory, made of what was there if that was one and nothing replaced it.
                _ => {
                    let was = existing.map(|at| entries[at].1).filter(|it| self.is_dir(*it));
                    self.rebuild(was.filter(|_| here.is_none()), &deeper)
                }
            };
            match (existing, child) {
                (Some(at), Some(child)) => entries[at].1 = child,
                (Some(at), None) => {
                    entries.remove(at);
                }
                (None, Some(child)) => entries.push((name_id, child)),
                (None, None) => {}
            }
        }
        if entries.is_empty() && dir != Some(self.empty) {
            return None;
        }
        Some(self.dir(entries))
    }

    /// The files that differ between `from` and `to`, in path order.
    pub(crate) fn diff(&self, from: NodeId, to: NodeId) -> Vec<Change> {
        let mut out = Vec::new();
        self.diff_into(Some(from), Some(to), &mut String::new(), &mut out);
        out
    }

    fn diff_into(
        &self,
        from: Option<NodeId>,
        to: Option<NodeId>,
        path: &mut String,
        out: &mut Vec<Change>,
    ) {
        if from == to {
            return;
        }
        let all = |node: NodeId, kind: ChangeKind, path: &str, out: &mut Vec<Change>| {
            out.extend(self.files(node, path).into_iter().map(|(path, _)| Change { path, kind }));
        };
        match (from, to) {
            (Some(a), Some(b)) => match (self.entries(a), self.entries(b)) {
                (None, None) => out.push(Change { path: path.clone(), kind: ChangeKind::Modified }),
                (Some(x), Some(y)) => {
                    let (mut i, mut j) = (0, 0);
                    while i < x.len() || j < y.len() {
                        let order = match (x.get(i), y.get(j)) {
                            (Some(a), Some(b)) => self.name_str(a.0).cmp(self.name_str(b.0)),
                            (Some(_), None) => Ordering::Less,
                            _ => Ordering::Greater,
                        };
                        let (name, left, right) = match order {
                            Ordering::Less => (x[i].0, Some(x[i].1), None),
                            Ordering::Greater => (y[j].0, None, Some(y[j].1)),
                            Ordering::Equal => (x[i].0, Some(x[i].1), Some(y[j].1)),
                        };
                        i += usize::from(order.is_le());
                        j += usize::from(order.is_ge());
                        let len = path.len();
                        if !path.is_empty() {
                            path.push('/');
                        }
                        path.push_str(self.name_str(name));
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

    /// Drops every node no root in `live` reaches, and renumbers the rest; `live` is updated in
    /// place. Names are kept. Returns each old node's new one, `None` for a node dropped.
    pub(crate) fn collect(&mut self, live: &mut [NodeId]) -> Vec<Option<NodeId>> {
        let mut reached = vec![false; self.nodes.len()];
        let mut stack: Vec<NodeId> = live.iter().copied().chain([self.empty]).collect();
        while let Some(node) = stack.pop() {
            if std::mem::replace(&mut reached[node.0 as usize], true) {
                continue;
            }
            if let Some(entries) = self.entries(node) {
                stack.extend(entries.iter().map(|it| it.1));
            }
        }
        let mut renumbered = vec![None; self.nodes.len()];
        let mut kept = Vec::with_capacity(reached.iter().filter(|it| **it).count());
        // Children have smaller ids than their parents, so they are renumbered first.
        for (old, node) in std::mem::take(&mut self.nodes).into_iter().enumerate() {
            if !reached[old] {
                continue;
            }
            renumbered[old] = Some(NodeId(kept.len() as u32));
            kept.push(match node {
                Node::Dir { id, entries } => Node::Dir {
                    id,
                    entries: entries
                        .iter()
                        .map(|&(name, child)| {
                            (name, renumbered[child.0 as usize].expect("a child is reached"))
                        })
                        .collect(),
                },
                file => file,
            });
        }
        self.nodes = kept;
        self.by_id =
            self.nodes.iter().enumerate().map(|(at, node)| (node.id(), NodeId(at as u32))).collect();
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

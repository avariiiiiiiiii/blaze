//! blaze v2's files (docs/v2/vfs.md section 2, docs/v2/core.md section 3): a persistent, content-addressed Merkle
//! tree, ported from blaze-next's `blaze-vfs` (`git show origin/blaze-next:crates/blaze-vfs`).
//!
//! - A file's [`ContentId`] is the hash of its bytes (domain [`FILE_DOMAIN`]); a directory's
//!   is the hash of its entries sorted by name ([`DIR_DOMAIN`]). Equal subtrees are one node.
//! - A **root** is a directory's content ID: a map of every file to its hash.
//! - A **view** ([`View`]) is a root a request builds: a branch, a PR, a CI job, a what-if
//!   of an agent's. Opening one is constant time, and every result keyed by input hashes is
//!   shared by every view whose inputs are equal.
//! - **Heads** are named roots (a workspace, a branch) that transactions move: a transaction
//!   made against an older root is rebased onto the head file by file, or refused naming
//!   the path ([`CommitError::Conflict`]).
//! - The journal makes heads and the nodes they reach durable through
//!   [`blaze_core::Effect`]s, with group commit and recovery from a torn tail.
//! - Watching: changes a driver reports ([`blaze_core::Done::Changed`]) become ops on the
//!   workspace's head ([`Watcher`]).

mod journal;
mod rebase;
mod tree;
pub mod watch;

use std::{borrow::Cow, fmt};

use blaze_core::{Completion, Effect, FileName, Tag};
use blaze_ids::{ContentId, Idx};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

use crate::{
    journal::{Journal, Loaded},
    tree::{NodeId, Tree},
};
pub use crate::watch::{Watched, Watcher};

pub const FILE_DOMAIN: &str = "blaze v2 vfs file 1";
pub const DIR_DOMAIN: &str = "blaze v2 vfs directory 1";

/// A root a request builds. Its identity is its content ID; [`ViewId`] is the daemon's dense
/// name for an open one.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct View {
    pub root: ContentId,
}

/// An open view, in the daemon's arena of them. Never in a key or a hash: results are keyed by
/// input hashes, never by view.
pub type ViewId = Idx<View>;

/// A file of a view, read in place.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct File<'a> {
    pub id: ContentId,
    pub bytes: &'a [u8],
}

/// A change to a list of files, applied all together or not at all.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum Op {
    /// The whole file, created with its directories if missing.
    Write { path: String, bytes: Vec<u8> },
    /// `len` bytes at `at` replaced by `bytes`, as an editor's change is.
    Edit { path: String, at: u64, len: u64, bytes: Vec<u8> },
    /// A file, or a directory with everything under it.
    Delete { path: String },
    /// A file or a directory moved; `to` must not exist.
    Rename { from: String, to: String },
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum OpError {
    /// Empty, absolute, or with an empty, `.` or `..` part.
    BadPath(String),
    NotFound(String),
    IsADirectory(String),
    NotADirectory(String),
    Exists(String),
    OutOfRange { path: String, at: u64, len: u64, size: u64 },
}

impl fmt::Display for OpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OpError::BadPath(path) => write!(f, "{path:?} is not a path in the tree"),
            OpError::NotFound(path) => write!(f, "{path} is not there"),
            OpError::IsADirectory(path) => write!(f, "{path} is a directory"),
            OpError::NotADirectory(path) => write!(f, "a part of {path} is a file"),
            OpError::Exists(path) => write!(f, "{path} is already there"),
            OpError::OutOfRange { path, at, len, size } => {
                write!(f, "{path} has {size} bytes, so {len} at {at} are not in it")
            }
        }
    }
}

/// A file that differs between two roots.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Change {
    pub path: String,
    pub kind: ChangeKind,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum ChangeKind {
    Added,
    Removed,
    Modified,
}

/// What a transaction made.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct Commit {
    pub root: ContentId,
    /// The head had moved past the transaction's base and the ops were moved onto it.
    pub rebased: bool,
    /// Its place in line for durability: [`Vfs::complete`] says when it is durable.
    pub ticket: Ticket,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum CommitError {
    /// The base root is not one this tree has.
    UnknownBase(ContentId),
    /// The head moved and changed `path` in a way the ops can't be moved past.
    Conflict { path: String },
    Op(OpError),
}

impl fmt::Display for CommitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CommitError::UnknownBase(root) => write!(f, "no root {root} is known"),
            CommitError::Conflict { path } => write!(f, "{path} changed since the base"),
            CommitError::Op(error) => fmt::Display::fmt(error, f),
        }
    }
}

impl From<OpError> for CommitError {
    fn from(error: OpError) -> CommitError {
        CommitError::Op(error)
    }
}

/// A commit's place in line for durability.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Ticket(pub u64);

/// The tree, its heads, its open views and its journal: a state machine that does its IO
/// through effects.
pub struct Vfs {
    tree: Tree,
    heads: FxHashMap<Box<str>, NodeId>,
    /// By view: its root while it is open. A closed view's number is not given out again.
    views: Vec<Option<NodeId>>,
    journal: Journal,
}

impl Vfs {
    /// A VFS recovered from its journal's bytes (empty for a new one), and what it found.
    /// Tags it asks with are `tags..tags + 2^32`.
    pub fn recover(journal: FileName, tags: u64, bytes: &[u8]) -> (Vfs, Recovered) {
        let Loaded { tree, heads, journal, found } = Journal::recover(journal, tags, bytes);
        (Vfs { tree, heads, views: Vec::new(), journal }, found)
    }

    /// The root `head` is at, if it was ever written.
    pub fn head(&self, head: &str) -> Option<ContentId> {
        self.heads.get(head).map(|it| self.tree.id(*it))
    }

    /// Every head and its root, by name.
    pub fn heads(&self) -> Vec<(&str, ContentId)> {
        let mut heads: Vec<(&str, ContentId)> =
            self.heads.iter().map(|(name, root)| (&**name, self.tree.id(*root))).collect();
        heads.sort_unstable();
        heads
    }

    /// The root with no files: the base of a head's first transaction.
    pub fn empty(&self) -> ContentId {
        self.tree.id(self.tree.empty())
    }

    /// Whether `root` is a root this tree has.
    pub fn has(&self, root: ContentId) -> bool {
        self.root(root).is_some()
    }

    /// The journal's sequence number of `head`'s last move: later moves have larger ones.
    pub fn sequence(&self, head: &str) -> Option<u64> {
        self.journal.sequence(head)
    }

    fn root(&self, root: ContentId) -> Option<NodeId> {
        self.tree.node(&root).filter(|it| self.tree.is_dir(*it))
    }

    /// Applies `ops`, made against `base`, to `head`, rebasing them if it moved.
    pub fn commit(
        &mut self,
        head: &str,
        base: ContentId,
        ops: &[Op],
    ) -> Result<Commit, CommitError> {
        let base = self.root(base).ok_or(CommitError::UnknownBase(base))?;
        let at = self.heads.get(head).copied().unwrap_or(self.tree.empty());
        let (ops, rebased) = if base == at {
            (Cow::Borrowed(ops), false)
        } else {
            (Cow::Owned(rebase::rebase(&self.tree, base, at, ops)?), true)
        };
        let root = self.tree.apply(at, &ops)?;
        self.heads.insert(head.into(), root);
        let ticket = self.journal.record_head(&self.tree, head, root);
        Ok(Commit { root: self.tree.id(root), rebased, ticket })
    }

    /// Moves `head` to `root` whatever it was at, as a checkout, a fork or a push does.
    pub fn set_head(&mut self, head: &str, root: ContentId) -> Result<Ticket, CommitError> {
        let node = self.root(root).ok_or(CommitError::UnknownBase(root))?;
        self.heads.insert(head.into(), node);
        Ok(self.journal.record_head(&self.tree, head, node))
    }

    /// Removes `head`, as a deleted branch or a closed PR does; `None` when there is none.
    pub fn remove_head(&mut self, head: &str) -> Option<Ticket> {
        self.heads.remove(head)?;
        Some(self.journal.record_removal(head))
    }

    /// The root made of `base` and `ops`, kept as a view's root and moving no head: a what-if.
    pub fn apply(&mut self, base: ContentId, ops: &[Op]) -> Result<ContentId, OpError> {
        let base = self.root(base).ok_or_else(|| OpError::NotFound(format!("root {base:x}")))?;
        let root = self.tree.apply(base, ops)?;
        Ok(self.tree.id(root))
    }

    /// Opens `view`, in constant time. Its root and what it reaches are kept until it closes.
    pub fn open(&mut self, view: View) -> Result<ViewId, CommitError> {
        let root = self.root(view.root).ok_or(CommitError::UnknownBase(view.root))?;
        let id = ViewId::from_raw(u32::try_from(self.views.len()).expect("fewer than 2^32 views"));
        self.views.push(Some(root));
        Ok(id)
    }

    pub fn close(&mut self, view: ViewId) {
        let slot = self.views.get_mut(view.raw() as usize).and_then(Option::take);
        assert!(slot.is_some(), "view {view:?} is not open");
    }

    fn open_root(&self, view: ViewId) -> NodeId {
        self.views
            .get(view.raw() as usize)
            .copied()
            .flatten()
            .unwrap_or_else(|| panic!("view {view:?} is not open"))
    }

    pub fn view(&self, view: ViewId) -> View {
        View { root: self.tree.id(self.open_root(view)) }
    }

    /// The file at `path` in `view`; `None` for none, or a directory.
    pub fn read(&self, view: ViewId, path: &str) -> Option<File<'_>> {
        let node = self.tree.lookup(self.open_root(view), path)?;
        let bytes = self.tree.bytes(node)?;
        Some(File { id: self.tree.id(node), bytes })
    }

    /// The content ID of what is at `path` in `view`, a file or a directory.
    pub fn lookup(&self, view: ViewId, path: &str) -> Option<ContentId> {
        self.tree.lookup(self.open_root(view), path).map(|it| self.tree.id(it))
    }

    /// The names in the directory at `path`, sorted, each with whether it is a directory.
    pub fn list(&self, view: ViewId, path: &str) -> Option<Vec<(String, bool)>> {
        let node = self.tree.lookup(self.open_root(view), path)?;
        let entries = self.tree.entries(node)?;
        Some(
            entries
                .iter()
                .map(|&(name, child)| (self.tree.name_str(name).to_owned(), self.tree.is_dir(child)))
                .collect(),
        )
    }

    /// The files that differ between two roots, in path order, descending only where hashes
    /// differ. Both must be roots this tree has ([`Vfs::has`]): an open view's, a head's.
    pub fn diff(&self, from: ContentId, to: ContentId) -> Vec<Change> {
        let node = |root: ContentId| {
            self.root(root).unwrap_or_else(|| panic!("a diff of {root:x}, which is no root here"))
        };
        self.tree.diff(node(from), node(to))
    }

    /// Asks for what commits made since the last flush need to be durable.
    pub fn flush(&mut self, effects: &mut Vec<Effect>) {
        self.journal.flush(&self.tree, &self.heads, effects);
    }

    /// Whether a completion is of an effect this asked for.
    pub fn owns(&self, tag: Tag) -> bool {
        self.journal.owns(tag)
    }

    /// Takes a completion of its own, and says which commits are now durable.
    pub fn complete(&mut self, completion: Completion, effects: &mut Vec<Effect>) -> Vec<Ticket> {
        self.journal.complete(&self.tree, &self.heads, completion, effects)
    }

    /// Drops every node no head, open view or root in `keep` reaches. The next flush also
    /// rewrites the journal as just what the heads reach.
    pub fn collect(&mut self, keep: &[ContentId]) {
        let names: Vec<Box<str>> = self.heads.keys().cloned().collect();
        let views: Vec<usize> = (0..self.views.len()).filter(|&at| self.views[at].is_some()).collect();
        let mut live: Vec<NodeId> = names.iter().map(|name| self.heads[name]).collect();
        live.extend(views.iter().map(|&at| self.views[at].expect("an open view")));
        live.extend(keep.iter().filter_map(|root| self.tree.node(root)));
        let renumbered = self.tree.collect(&mut live);
        let mut live = live.into_iter();
        for name in names {
            self.heads.insert(name, live.next().expect("a head's root"));
        }
        for at in views {
            self.views[at] = live.next();
        }
        self.journal.collected(&renumbered, self.tree.len());
    }
}

/// What recovering a journal found.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Recovered {
    pub records: usize,
    /// Bytes that were not a whole record with a good checksum.
    pub skipped: usize,
    /// Head records whose root did not survive, dropped.
    pub heads_dropped: usize,
}

#[cfg(test)]
mod tests;

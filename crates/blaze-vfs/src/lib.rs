//! blaze v2's files (docs/v2/vfs.md section 2, docs/v2/core.md section 3): a persistent,
//! content-addressed Merkle tree, ported from blaze-next's `blaze-vfs`
//! (`git show origin/blaze-next:crates/blaze-vfs`).
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
//!   workspace's head.

use std::fmt;

use blaze_core::{Completion, Effect, FileName, Tag};
use blaze_ids::{ContentId, Idx};
use serde::{Deserialize, Serialize};

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
            CommitError::Op(error) => write!(f, "{error:?}"),
        }
    }
}

/// A commit's place in line for durability.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Ticket(pub u64);

/// The tree, its heads, its open views and its journal: a state machine that does its IO
/// through effects.
pub struct Vfs {
    _private: (),
}

impl Vfs {
    /// A VFS recovered from its journal's bytes (empty for a new one), and what it found.
    /// Tags it asks with are `tags..tags + 2^32`.
    pub fn recover(journal: FileName, tags: u64, bytes: &[u8]) -> (Vfs, Recovered) {
        let _ = (journal, tags, bytes);
        unimplemented!("v2: recovering the tree and heads from the journal (v2-vfs)")
    }

    /// The root `head` is at, if it was ever written.
    pub fn head(&self, head: &str) -> Option<ContentId> {
        let _ = head;
        unimplemented!("v2: a head's root (v2-vfs)")
    }

    /// Applies `ops`, made against `base`, to `head`, rebasing them if it moved.
    pub fn commit(
        &mut self,
        head: &str,
        base: ContentId,
        ops: &[Op],
    ) -> Result<Commit, CommitError> {
        let _ = (head, base, ops);
        unimplemented!("v2: a transaction against a head (v2-vfs)")
    }

    /// The root made of `base` and `ops`, kept as a view's root and moving no head: a what-if.
    pub fn apply(&mut self, base: ContentId, ops: &[Op]) -> Result<ContentId, OpError> {
        let _ = (base, ops);
        unimplemented!("v2: a root from a base and ops (v2-vfs)")
    }

    /// Opens `view`, in constant time. Its root and what it reaches are kept until it closes.
    pub fn open(&mut self, view: View) -> Result<ViewId, CommitError> {
        let _ = view;
        unimplemented!("v2: opening a view (v2-vfs)")
    }

    pub fn close(&mut self, view: ViewId) {
        let _ = view;
        unimplemented!("v2: closing a view (v2-vfs)")
    }

    pub fn view(&self, view: ViewId) -> View {
        let _ = view;
        unimplemented!("v2: an open view's root (v2-vfs)")
    }

    /// The file at `path` in `view`; `None` for none, or a directory.
    pub fn read(&self, view: ViewId, path: &str) -> Option<File<'_>> {
        let _ = (view, path);
        unimplemented!("v2: reading a file of a view (v2-vfs)")
    }

    /// The content ID of what is at `path` in `view`, a file or a directory.
    pub fn lookup(&self, view: ViewId, path: &str) -> Option<ContentId> {
        let _ = (view, path);
        unimplemented!("v2: looking a path up in a view (v2-vfs)")
    }

    /// The names in the directory at `path`, sorted, each with whether it is a directory.
    pub fn list(&self, view: ViewId, path: &str) -> Option<Vec<(String, bool)>> {
        let _ = (view, path);
        unimplemented!("v2: listing a directory of a view (v2-vfs)")
    }

    /// The files that differ between two roots, in path order, descending only where hashes
    /// differ.
    pub fn diff(&self, from: ContentId, to: ContentId) -> Vec<Change> {
        let _ = (from, to);
        unimplemented!("v2: the diff of two roots (v2-vfs)")
    }

    /// Asks for what commits made since the last flush need to be durable.
    pub fn flush(&mut self, effects: &mut Vec<Effect>) {
        let _ = effects;
        unimplemented!("v2: group commit of the journal (v2-vfs)")
    }

    /// Whether a completion is of an effect this asked for.
    pub fn owns(&self, tag: Tag) -> bool {
        let _ = tag;
        unimplemented!("v2: the journal's tags (v2-vfs)")
    }

    /// Takes a completion of its own, and says which commits are now durable.
    pub fn complete(&mut self, completion: Completion, effects: &mut Vec<Effect>) -> Vec<Ticket> {
        let _ = (completion, effects);
        unimplemented!("v2: the journal's completions (v2-vfs)")
    }

    /// Drops every node no head, open view or root in `keep` reaches.
    pub fn collect(&mut self, keep: &[ContentId]) {
        let _ = keep;
        unimplemented!("v2: garbage collection of the tree (v2-vfs)")
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

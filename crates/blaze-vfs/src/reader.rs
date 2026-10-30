//! [`Reader`]: the tree's nodes by content ID, read from any thread while the [`Vfs`] goes on,
//! without a lock and without writing memory another core writes.
//!
//! A node is immutable and named by its content ID, so what a root holds never changes; the
//! only change is which nodes exist. A reader reads the tree's own storage ([`crate::store`]):
//! a node is readable as soon as the [`Vfs`] interned it, before any call that gives its ID
//! returns, and after [`Vfs::collect`] a dropped root reads as none. A read is in a task
//! ([`blaze_reclaim::enter`]: thread-local counts inside the task a plane worker already runs,
//! else a store to the thread's own slot and a fence), then walks the path with acquire loads:
//! one index probe for the root, one binary search of a name column per component. A file's
//! bytes are the tree's own [`Arc`], never copied; [`Pinned`] reads them in place, with no
//! reference count touched, and many reads under one pin.
//!
//! [`Vfs`]: crate::Vfs
//! [`Vfs::collect`]: crate::Vfs::collect

use std::sync::Arc;

use blaze_ids::ContentId;

use crate::{
    File, Mode,
    store::{Entries, Shared, Table, mode_of},
};

/// A handle on every node the [`Vfs`](crate::Vfs) has, by content ID: cheap to clone, `Send`
/// and `Sync`. A root is readable while the tree has it (a head reaches it, a view of it is
/// open, or a collection kept it); after that it reads as none.
#[derive(Clone)]
pub struct Reader {
    shared: Arc<Shared>,
}

impl Reader {
    pub(crate) fn new(shared: Arc<Shared>) -> Reader {
        Reader { shared }
    }

    /// The file at `path` under `root`: its content ID, mode and bytes (a symlink's are its
    /// target, not followed); `None` for none, a directory, or a root the tree does not have.
    /// The bytes are the tree's own [`Arc`], with one count taken so they outlive the pin;
    /// [`Pinned::file`] lends them with none.
    pub fn file(&self, root: ContentId, path: &str) -> Option<(ContentId, Mode, Arc<[u8]>)> {
        let pinned = self.pin();
        let (node, mode) = pinned.walk(root, path)?;
        let (id, bytes) = pinned.blob(node)?;
        Some((*id, mode, bytes.clone()))
    }

    /// The names in the directory at `path` (`""` for the root), sorted, each with whether it
    /// is a directory; `None` for none, a file, or a root the tree does not have. Owned, as
    /// they outlive the pin; [`Pinned::list`] lends them instead.
    pub fn list(&self, root: ContentId, path: &str) -> Option<std::vec::IntoIter<(String, bool)>> {
        let pinned = self.pin();
        let names: Vec<(String, bool)> =
            pinned.list(root, path)?.map(|(name, dir)| (name.to_owned(), dir)).collect();
        Some(names.into_iter())
    }

    /// Pins this thread to read the tree as it is now, in place, as many times as wanted.
    /// Hold it only while reading: a collection's old nodes are freed once the task it is in
    /// ends.
    pub fn pin(&self) -> Pinned<'_> {
        let task = blaze_reclaim::enter();
        // SAFETY: in a task, for as long as `Pinned` lives.
        let table = unsafe { self.shared.table() };
        Pinned { shared: &self.shared, table, _task: task }
    }
}

/// The tree as it was when [`Reader::pin`] was called, read in place. Not `Send`: the task is
/// the thread's.
pub struct Pinned<'r> {
    shared: &'r Shared,
    table: &'r Table,
    _task: blaze_reclaim::Guard,
}

impl Pinned<'_> {
    /// The file at `path` under `root`, its bytes read in place; `None` for none, a
    /// directory, or a root the tree does not have.
    pub fn file(&self, root: ContentId, path: &str) -> Option<File<'_>> {
        let (node, mode) = self.walk(root, path)?;
        let (id, bytes) = self.blob(node)?;
        Some(File { id: *id, mode, bytes })
    }

    /// The names in the directory at `path` (`""` for the root), sorted, each with whether it
    /// is a directory; `None` for none, a file, or a root the tree does not have.
    pub fn list(&self, root: ContentId, path: &str) -> Option<impl Iterator<Item = (&str, bool)> + '_> {
        let (node, _) = self.walk(root, path)?;
        // SAFETY: reached through this table while pinned.
        let shape = unsafe { self.table.shape(node) };
        if !shape.is_dir() {
            return None;
        }
        // SAFETY: a directory's shape, as above.
        let entries: Entries<'_> = unsafe { self.table.entries(shape) };
        let names = &self.shared.names;
        Some((0..entries.len()).map(move |at| {
            // SAFETY: an entry's name was interned before its directory was published.
            let name = unsafe { names.get(entries.names[at]) };
            // SAFETY: names are interned only from `&str`s.
            (unsafe { std::str::from_utf8_unchecked(name) }, entries.is_dir(at))
        }))
    }

    /// The node `path` names under the directory `root`, and its mode, as the tree's `entry`.
    fn walk(&self, root: ContentId, path: &str) -> Option<(u32, Mode)> {
        let table = self.table;
        let root = table.find(&root).ok()?;
        // SAFETY: reached through the index while pinned; so is every node below.
        let mut shape = unsafe { table.shape(root) };
        if !shape.is_dir() {
            return None;
        }
        if path.is_empty() {
            return Some((root, Mode::Regular));
        }
        let (mut node, mut mode) = (root, Mode::Regular);
        for name in path.split('/') {
            if !shape.is_dir() {
                return None;
            }
            // SAFETY: as above.
            let entries = unsafe { table.entries(shape) };
            let at = entries.find(&self.shared.names, name.as_bytes())?;
            (node, mode) = (entries.nodes[at], mode_of(entries.modes[at]));
            // SAFETY: as above.
            shape = unsafe { table.shape(node) };
        }
        Some((node, mode))
    }

    /// A file node's ID and bytes; `None` for a directory.
    fn blob(&self, node: u32) -> Option<(&ContentId, &Arc<[u8]>)> {
        // SAFETY: reached through this table while pinned.
        unsafe {
            let shape = self.table.shape(node);
            (!shape.is_dir()).then(|| (self.table.id(node), self.table.blob(shape)))
        }
    }
}

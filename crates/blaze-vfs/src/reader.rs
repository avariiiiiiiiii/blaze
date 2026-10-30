//! [`Reader`]: the tree's nodes by content ID, read from any thread while the [`Vfs`] goes on.
//!
//! A node is immutable and named by its content ID, so what a root holds never changes; the
//! only change is which nodes exist. The [`Vfs`] publishes the nodes each change made (one
//! write lock per change) and, at [`Vfs::collect`], drops the ones it dropped. A read takes the
//! read lock once and walks the path by content ID. A file's bytes are the tree's own
//! [`Arc`], not a copy; a directory is copied once, as its entries.
//!
//! [`Vfs`]: crate::Vfs
//! [`Vfs::collect`]: crate::Vfs::collect

use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard};

use blaze_ids::ContentId;
use rustc_hash::FxHashMap;

use crate::Mode;

/// A directory's entry as the reader holds it.
pub(crate) struct Entry {
    pub(crate) name: Arc<str>,
    pub(crate) id: ContentId,
    /// [`Mode::Regular`] for a directory.
    pub(crate) mode: Mode,
    pub(crate) dir: bool,
}

pub(crate) enum Shared {
    File(Arc<[u8]>),
    /// Sorted by name, as the tree's directory is.
    Dir(Box<[Entry]>),
}

/// A handle on every node the [`Vfs`](crate::Vfs) has, by content ID: cheap to clone, `Send`
/// and `Sync`. A root is readable while the tree has it (a head reaches it, a view of it is
/// open, or a collection kept it); after that it reads as none.
#[derive(Clone, Default)]
pub struct Reader {
    nodes: Arc<RwLock<FxHashMap<ContentId, Shared>>>,
}

impl Reader {
    /// The file at `path` under `root`: its content ID, mode and bytes (a symlink's are its
    /// target, not followed); `None` for none, a directory, or a root the tree does not have.
    pub fn file(&self, root: ContentId, path: &str) -> Option<(ContentId, Mode, Arc<[u8]>)> {
        let nodes = self.nodes();
        let (id, mode) = walk(&nodes, root, path)?;
        match nodes.get(&id)? {
            Shared::File(bytes) => Some((id, mode, bytes.clone())),
            Shared::Dir(_) => None,
        }
    }

    /// The names in the directory at `path` (`""` for the root), sorted, each with whether it
    /// is a directory; `None` for none, a file, or a root the tree does not have.
    pub fn list(&self, root: ContentId, path: &str) -> Option<Vec<(String, bool)>> {
        let nodes = self.nodes();
        let (id, _) = walk(&nodes, root, path)?;
        match nodes.get(&id)? {
            Shared::Dir(entries) => {
                Some(entries.iter().map(|it| (it.name.to_string(), it.dir)).collect())
            }
            Shared::File(_) => None,
        }
    }

    fn nodes(&self) -> RwLockReadGuard<'_, FxHashMap<ContentId, Shared>> {
        self.nodes.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// Adds nodes a change made. A node already there is the same node (its ID is its content).
    pub(crate) fn publish(&self, made: Vec<(ContentId, Shared)>) {
        if made.is_empty() {
            return;
        }
        let mut nodes = self.nodes.write().unwrap_or_else(PoisonError::into_inner);
        nodes.extend(made);
    }

    /// Keeps only the nodes `kept` says the tree still has.
    pub(crate) fn retain(&self, kept: impl Fn(&ContentId) -> bool) {
        let mut nodes = self.nodes.write().unwrap_or_else(PoisonError::into_inner);
        nodes.retain(|id, _| kept(id));
    }
}

/// The ID and mode of what `path` names under the directory `root`, as the tree's `entry`.
fn walk(
    nodes: &FxHashMap<ContentId, Shared>,
    root: ContentId,
    path: &str,
) -> Option<(ContentId, Mode)> {
    let Shared::Dir(_) = nodes.get(&root)? else { return None };
    if path.is_empty() {
        return Some((root, Mode::Regular));
    }
    path.split('/').try_fold((root, Mode::Regular), |(at, _), name| {
        let Shared::Dir(entries) = nodes.get(&at)? else { return None };
        let found = entries.binary_search_by(|it| (*it.name).cmp(name)).ok()?;
        Some((entries[found].id, entries[found].mode))
    })
}

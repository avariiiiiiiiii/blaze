//! How the database reads a view's files: a [`Tree`] answers for any root, from any thread.

use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex, PoisonError},
};

use blaze_ids::{ContentId, Hasher};
use blaze_vfs::Mode;

/// The files of every root, read from many worker threads at once. A root's files never
/// change (a root is a content ID), so every answer for a root is the same forever.
pub trait Tree: Send + Sync {
    /// The file at `path` under `root`: its content ID, mode and bytes (a symlink's are its
    /// target); `None` for none, a directory, or a root this tree does not have.
    fn file(&self, root: ContentId, path: &str) -> Option<(ContentId, Mode, Arc<[u8]>)>;
    /// The names in the directory at `path` (`""` for the root), sorted, each with whether it
    /// is a directory; `None` for none or a file.
    fn list(&self, root: ContentId, path: &str) -> Option<Vec<(String, bool)>>;
}

/// A tree in memory, for tests and for what-ifs built without the VFS. Its roots are content
/// IDs of the sorted list of paths, modes and file IDs (not the VFS's Merkle IDs).
#[derive(Default)]
pub struct MemTree {
    roots: Mutex<HashMap<ContentId, Arc<Files>>>,
}

type Files = BTreeMap<String, (ContentId, Mode, Arc<[u8]>)>;

pub const MEM_ROOT_DOMAIN: &str = "blaze v2 db memory root 2";

impl MemTree {
    /// The root holding exactly `files`, as `(path, bytes)`, each a regular file.
    pub fn root(&self, files: &[(&str, &[u8])]) -> ContentId {
        let files: Vec<(&str, Mode, &[u8])> =
            files.iter().map(|&(path, bytes)| (path, Mode::Regular, bytes)).collect();
        self.root_with_modes(&files)
    }

    /// The root holding exactly `files`, as `(path, mode, bytes)`.
    pub fn root_with_modes(&self, files: &[(&str, Mode, &[u8])]) -> ContentId {
        let files: Files = files
            .iter()
            .map(|&(path, mode, bytes)| {
                let id = ContentId::of(blaze_vfs::FILE_DOMAIN, bytes);
                (path.to_owned(), (id, mode, Arc::from(bytes)))
            })
            .collect();
        let mut hasher = Hasher::new(MEM_ROOT_DOMAIN);
        for (path, (id, mode, _)) in &files {
            hasher.part(path.as_bytes()).update(&mode.git().to_le_bytes()).id(id);
        }
        let root = hasher.finish();
        self.roots.lock().unwrap_or_else(PoisonError::into_inner).insert(root, Arc::new(files));
        root
    }

    fn files(&self, root: ContentId) -> Option<Arc<Files>> {
        self.roots.lock().unwrap_or_else(PoisonError::into_inner).get(&root).cloned()
    }
}

impl Tree for MemTree {
    fn file(&self, root: ContentId, path: &str) -> Option<(ContentId, Mode, Arc<[u8]>)> {
        self.files(root)?.get(path).cloned()
    }

    fn list(&self, root: ContentId, path: &str) -> Option<Vec<(String, bool)>> {
        let files = self.files(root)?;
        if files.contains_key(path) {
            return None;
        }
        let prefix = if path.is_empty() { String::new() } else { format!("{path}/") };
        let mut names: BTreeMap<String, bool> = BTreeMap::new();
        for name in files.keys() {
            let Some(rest) = name.strip_prefix(&prefix) else { continue };
            match rest.split_once('/') {
                Some((dir, _)) => {
                    names.insert(dir.to_owned(), true);
                }
                None => {
                    names.entry(rest.to_owned()).or_insert(false);
                }
            }
        }
        if names.is_empty() && !path.is_empty() {
            return None;
        }
        Some(names.into_iter().collect())
    }
}

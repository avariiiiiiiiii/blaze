//! Maps paths to compact integer ids. We don't care about clearings paths which
//! no longer exist -- the assumption is total size of paths we ever look at is
//! not too big.
use std::hash::BuildHasherDefault;

use indexmap::IndexSet;
use rustc_hash::FxHasher;

use crate::{FileId, VfsPath};

/// `paths`' entry for an ID no path has.
const NO_PATH: u32 = u32::MAX;

/// Structure to map between [`VfsPath`] and [`FileId`].
///
/// A path's ID is either the next dense one ([`PathInterner::intern`]) or one the caller hands
/// out ([`PathInterner::intern_with`]), so the two are mapped both ways rather than taking the
/// path's index as its ID.
#[derive(Default)]
pub(crate) struct PathInterner {
    map: IndexSet<VfsPath, BuildHasherDefault<FxHasher>>,
    /// The ID of each path, by its index in `map`.
    ids: Vec<FileId>,
    /// The index in `map` of each ID's path, [`NO_PATH`] for an ID no path has.
    paths: Vec<u32>,
}

impl PathInterner {
    /// Get the id corresponding to `path`.
    ///
    /// If `path` does not exist in `self`, returns [`None`].
    pub(crate) fn get(&self, path: &VfsPath) -> Option<FileId> {
        self.map.get_index_of(path).map(|i| self.ids[i])
    }

    /// Insert `path` in `self`.
    ///
    /// - If `path` already exists in `self`, returns its associated id;
    /// - Else, returns a newly allocated id: one past the highest id any path has, so it never
    ///   meets an id given to [`Self::intern_with`].
    pub(crate) fn intern(&mut self, path: VfsPath) -> FileId {
        let (index, added) = self.map.insert_full(path);
        if !added {
            return self.ids[index];
        }
        let id = FileId::from_raw(u32::try_from(self.paths.len()).expect("file IDs fit in u32"));
        self.place(index, id)
    }

    /// Insert `path` in `self`, taking a new path's id from `new_id`.
    ///
    /// - If `path` already exists in `self`, returns its associated id and doesn't call `new_id`;
    /// - Else, returns `new_id()`.
    ///
    /// # Panics
    ///
    /// Panics if `new_id()` is already another path's id.
    pub(crate) fn intern_with(&mut self, path: VfsPath, new_id: impl FnOnce() -> FileId) -> FileId {
        let (index, added) = self.map.insert_full(path);
        if !added {
            return self.ids[index];
        }
        self.place(index, new_id())
    }

    fn place(&mut self, index: usize, id: FileId) -> FileId {
        let slot = id.index() as usize;
        if slot >= self.paths.len() {
            self.paths.resize(slot + 1, NO_PATH);
        }
        assert_eq!(self.paths[slot], NO_PATH, "file ID {} is already a path's", id.index());
        self.paths[slot] = u32::try_from(index).expect("path indices fit in u32");
        debug_assert_eq!(self.ids.len(), index);
        self.ids.push(id);
        id
    }

    /// Returns the path corresponding to `id`.
    ///
    /// # Panics
    ///
    /// Panics if `id` does not exist in `self`.
    pub(crate) fn lookup(&self, id: FileId) -> &VfsPath {
        let index = self.paths[id.index() as usize];
        assert_ne!(index, NO_PATH, "no path has file ID {}", id.index());
        self.map.get_index(index as usize).unwrap()
    }
}

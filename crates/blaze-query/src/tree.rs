//! How the database reads a view's files: a [`Tree`] answers for any root, from any thread.

use std::{collections::BTreeMap, ops::Bound, sync::Arc};

use blaze_ids::{ContentId, Hasher};
use blaze_vfs::{Bytes, Corrupt, Mode};
use rustc_hash::FxHashMap;

use crate::table::{FxMap, ShortLock};

/// The files of every root, read from many worker threads at once. A root's files never
/// change (a root is a content ID), so every answer for a root is the same forever.
pub trait Tree: Send + Sync {
    /// The file at `path` under `root`: its content ID, mode and bytes (a symlink's are its
    /// target); `None` for none, a directory, or a root this tree does not have. A file whose
    /// bytes are a range of the mapped image that fails its check is `Err`, naming the
    /// image's section and group: never `None`, which would be a file that is not there.
    fn file(&self, root: ContentId, path: &str) -> Result<Option<(ContentId, Mode, Bytes)>, Corrupt>;
    /// Lends `read` the names in the directory at `path` (`""` for the root), sorted by name,
    /// each with whether it is a directory; `false`, with `read` not called, for none or a
    /// file. The database hashes a listing as it is lent, copying nothing, and stops naming
    /// the root and directory at a name out of order.
    fn read_dir(&self, root: ContentId, path: &str, read: &mut dyn FnMut(Names<'_, '_>)) -> bool; // dyn: one call a directory per view, and `Tree` is a trait object

    /// [`Tree::read_dir`]'s names, owned: for a caller that keeps them.
    fn list(&self, root: ContentId, path: &str) -> Option<Vec<(String, bool)>> {
        let mut owned = None;
        self.read_dir(root, path, &mut |names| {
            // alloc: the caller asked for the names owned.
            owned = Some(names.map(|(name, dir)| (name.to_owned(), dir)).collect());
        });
        owned
    }
}

/// A directory's names as a [`Tree`] lends them (for `'n`), each with whether it is a
/// directory.
pub type Names<'a, 'n> = &'a mut (dyn Iterator<Item = (&'n str, bool)> + 'a); // dyn: see `Tree::read_dir`

/// The VFS's files: [`blaze_vfs::Vfs::reader`], a lock-free handle on its immutable nodes by
/// content ID. Here rather than in blaze-vfs because blaze-query depends on blaze-vfs.
///
/// A file's [`Bytes`] are the tree's own (its `Arc`, or its range of the mapped image) with
/// one count taken, once per file per view, as the database keeps the input.
impl Tree for blaze_vfs::Reader {
    fn file(&self, root: ContentId, path: &str) -> Result<Option<(ContentId, Mode, Bytes)>, Corrupt> {
        blaze_vfs::Reader::file(self, root, path)
    }

    fn read_dir(&self, root: ContentId, path: &str, read: &mut dyn FnMut(Names<'_, '_>)) -> bool { // dyn: see `Tree::read_dir`
        let pinned = self.pin();
        let Some(mut names) = pinned.list(root, path) else { return false };
        read(&mut names);
        true
    }
}

/// A tree in memory, for tests and for what-ifs built without the VFS. Its roots are content
/// IDs of the sorted list of paths, modes and file IDs (not the VFS's Merkle IDs).
#[derive(Default)]
pub struct MemTree {
    roots: ShortLock<FxMap<ContentId, Arc<Files>>>,
}

// BTreeMap: hashed in path order into the root's ID, and listed by prefix range.
type Files = BTreeMap<String, (ContentId, Mode, Bytes)>;

pub const MEM_ROOT_DOMAIN: &str = "blaze v2 db memory root 2";

impl MemTree {
    /// The root holding exactly `files`, as `(path, bytes)`, each a regular file.
    pub fn root(&self, files: &[(&str, &[u8])]) -> ContentId {
        let files: Vec<(&str, Mode, &[u8])> =
            files.iter().map(|&(path, bytes)| (path, Mode::Regular, bytes)).collect(); // alloc: `MemTree` is the tree of tests and what-ifs, off every hot path
        self.root_with_modes(&files)
    }

    /// The root holding exactly `files`, as `(path, mode, bytes)`.
    pub fn root_with_modes(&self, files: &[(&str, Mode, &[u8])]) -> ContentId {
        let files: Files = files
            .iter()
            .map(|&(path, mode, bytes)| {
                let id = ContentId::of(blaze_vfs::FILE_DOMAIN, bytes);
                (path.to_owned(), (id, mode, Bytes::from(Arc::<[u8]>::from(bytes)))) // alloc: `MemTree` is the tree of tests and what-ifs, off every hot path
            })
            .collect(); // alloc: `MemTree` is the tree of tests and what-ifs, off every hot path
        let mut hasher = Hasher::new(MEM_ROOT_DOMAIN);
        for (path, (id, mode, _)) in &files {
            hasher.part(path.as_bytes()).update(&mode.git().to_le_bytes()).id(id);
        }
        let root = hasher.finish();
        self.roots.lock().insert(root, Arc::new(files)); // alloc: `MemTree` is the tree of tests and what-ifs, off every hot path
        root
    }

    fn files(&self, root: ContentId) -> Option<Arc<Files>> {
        self.roots.lock().get(&root).cloned()
    }

    /// The names in the directory at `path` under `root`, sorted, each with whether it is a
    /// directory; `None` for none or a file.
    fn names(&self, root: ContentId, path: &str) -> Option<Vec<(String, bool)>> {
        let files = self.files(root)?;
        if files.contains_key(path) {
            return None;
        }
        let prefix = if path.is_empty() { String::new() } else { format!("{path}/") }; // alloc: `MemTree` is the tree of tests and what-ifs, off every hot path
        let mut names: FxHashMap<String, bool> = FxHashMap::default();
        let under = files.range::<str, _>((Bound::Included(prefix.as_str()), Bound::Unbounded));
        for (name, _) in under {
            let Some(rest) = name.strip_prefix(&prefix) else { break };
            match rest.split_once('/') {
                Some((dir, _)) => {
                    names.insert(dir.to_owned(), true); // alloc: `MemTree` is the tree of tests and what-ifs, off every hot path
                }
                None => {
                    names.entry(rest.to_owned()).or_insert(false); // alloc: `MemTree` is the tree of tests and what-ifs, off every hot path
                }
            }
        }
        if names.is_empty() && !path.is_empty() {
            return None;
        }
        let mut names: Vec<(String, bool)> = names.into_iter().collect(); // alloc: `MemTree` is the tree of tests and what-ifs, off every hot path
        names.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        Some(names)
    }
}

impl Tree for MemTree {
    fn file(&self, root: ContentId, path: &str) -> Result<Option<(ContentId, Mode, Bytes)>, Corrupt> {
        Ok(self.files(root).and_then(|files| files.get(path).cloned()))
    }

    fn read_dir(&self, root: ContentId, path: &str, read: &mut dyn FnMut(Names<'_, '_>)) -> bool { // dyn: see `Tree::read_dir`
        let Some(names) = self.names(root, path) else { return false };
        read(&mut names.iter().map(|(name, dir)| (name.as_str(), *dir)));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::MemTree;

    #[test]
    fn a_listing_is_sorted_by_name_and_sees_only_its_directory() {
        let tree = MemTree::default();
        let root = tree.root(&[
            ("a/b", b"1"),
            ("a-x", b"2"),
            ("a/c/d", b"3"),
            ("a.rs", b"4"),
            ("ab", b"5"),
            ("a/c.rs", b"6"),
            ("b", b"7"),
        ]);
        let names = |path| tree.names(root, path);
        let owned = |it: &[(&str, bool)]| Some(it.iter().map(|&(n, d)| (n.to_owned(), d)).collect::<Vec<_>>());
        assert_eq!(
            names(""),
            owned(&[("a", true), ("a-x", false), ("a.rs", false), ("ab", false), ("b", false)])
        );
        assert_eq!(names("a"), owned(&[("b", false), ("c", true), ("c.rs", false)]));
        assert_eq!(names("a/c"), owned(&[("d", false)]));
        assert_eq!(names("a/b"), None, "a file");
        assert_eq!(names("ab/x"), None, "nothing there");
        assert_eq!(names("a-"), None, "a prefix of a name is no directory");
    }
}

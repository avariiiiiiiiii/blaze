//! One view's files as the front end's inputs (docs/v2/vfs.md section 2): a view is a root of
//! blaze-vfs, a map of every file to its content ID, and setting one writes only the inputs
//! whose content changed, so salsa sees no change where there is none.
//!
//! A view's path maps to a file and a source root through what the database has already: a
//! path a source root has named keeps its [`FileId`] for good (as `vfs::Vfs` gives one), and a
//! path none has is given an unused one and put in the source root that the partition the roots
//! were made with ([`FileSetConfig`]) puts it in.
//!
//! The content ID a view gives is blaze-vfs's ([`blaze_vfs::FILE_DOMAIN`], of the bytes
//! alone), checked against the bytes; a file's [`FileText::content`] is another thing (the text
//! and the module the compiler appends, framed), so both are kept: blaze-vfs's to tell next time
//! that a file is unchanged without reading it, the text's in the input.

use std::fmt;

use blaze_ids::ContentId;
use rustc_hash::{FxHashMap, FxHashSet};
use salsa::Durability;
use triomphe::Arc;
use vfs::{
    AbsPath, AbsPathBuf, FileId, VfsPath,
    file_set::{FileSet, FileSetConfig},
};

use crate::{FileText, Files, SourceDatabase, SourceRoot, SourceRootId};

/// One file of a view, as blaze-vfs reads it.
#[derive(Clone, Copy, Debug)]
pub struct ViewFile<'a> {
    /// Relative to the view's root and `/`-separated, as blaze-vfs names it.
    pub path: &'a str,
    /// Its blaze-vfs content ID and its bytes.
    pub file: blaze_vfs::File<'a>,
}

/// Every file of one view, and where the database's source roots put them.
#[derive(Clone, Copy, Debug)]
pub struct ViewRoot<'a> {
    /// The directory the view is checked out as: its paths are relative to this.
    pub base: &'a AbsPath,
    /// Directories under `base` whose files are not the view's (what a build writes): their
    /// files are left as they are.
    pub built: &'a [AbsPathBuf],
    /// The partition the source roots were made with: set `i` is `SourceRootId(i)`.
    pub partition: &'a FileSetConfig,
    /// The source root of the files no set of `partition` has.
    pub other: SourceRootId,
    pub files: &'a [ViewFile<'a>],
}

/// What setting a view wrote, each list sorted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ViewChange {
    /// Files that were in a source root already whose text was set.
    pub set: Vec<FileId>,
    /// Files put in a source root.
    pub added: Vec<FileId>,
    /// Files under the view's base taken out of their source root: the view has them no more.
    pub removed: Vec<FileId>,
    /// Of the files set or added, those whose bytes are not UTF-8: they have no text
    /// ([`SourceDatabase::has_file_text`] is false).
    pub no_text: Vec<FileId>,
    /// Source roots whose files were added or removed: a crate root may have come or gone.
    pub roots: Vec<SourceRootId>,
}

/// Why a view was refused. Nothing is written when one is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ViewError {
    /// Empty, absolute, or with an empty, `.` or `..` part.
    BadPath(String),
    /// Two files of the view have the path.
    Duplicate(String),
    /// The content ID given for the file is not that of its bytes.
    Mismatch { path: String, given: ContentId, bytes: ContentId },
    /// The partition puts the new file in a source root the database does not have.
    NoSourceRoot { path: String, root: SourceRootId },
    /// Every file ID is taken.
    NoFileId(String),
}

impl fmt::Display for ViewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ViewError::BadPath(path) => write!(f, "{path:?} is not a path of a view"),
            ViewError::Duplicate(path) => write!(f, "the view has {path} twice"),
            ViewError::Mismatch { path, given, bytes } => write!(
                f,
                "{path} was given the content ID {given:x}, but its bytes are {bytes:x}"
            ),
            ViewError::NoSourceRoot { path, root } => {
                write!(f, "{path} belongs in the source root {root:?}, which the database lacks")
            }
            ViewError::NoFileId(path) => write!(f, "no file ID is left for {path}"),
        }
    }
}

impl std::error::Error for ViewError {}

impl Files {
    /// Sets the inputs of the files of `view`, and only those whose content changed: a file
    /// whose blaze-vfs ID is the one it was last set from is not read again (nor its bytes
    /// checked), one whose text is what it has is not written, so setting a view twice writes
    /// nothing. Files under `view.base` that the view lacks are taken out of their source
    /// root. Bytes that are not UTF-8 are a file with no text. Every file's ID is checked
    /// against its bytes before anything is written, and a mismatch refuses the whole view.
    pub fn set_view_root(
        &self,
        db: &mut dyn SourceDatabase,
        view: ViewRoot<'_>,
    ) -> Result<ViewChange, ViewError> {
        let base = VfsPath::from(view.base.to_path_buf());
        let mut in_view = FxHashSet::default();
        // Each file to write: its ID, its text, its source root.
        let mut writes: Vec<(FileId, Option<&str>, SourceRootId)> = Vec::new();
        let mut added: FxHashMap<SourceRootId, Vec<(FileId, VfsPath)>> = FxHashMap::default();
        let mut verified = Vec::new();
        let mut next = self.next_file.lock().unwrap_or_else(|it| it.into_inner());
        for file in view.files {
            check_path(file.path)?;
            let path = VfsPath::from(view.base.join(file.path));
            if !in_view.insert(path.clone()) {
                return Err(ViewError::Duplicate(file.path.to_owned()));
            }
            let known = self.by_path.get(&path).map(|it| *it);
            let in_root = known.and_then(|id| self.in_roots.get(&id).and_then(|it| it.first().copied()));
            if let (Some(id), Some(_)) = (known, in_root)
                && self.view_ids.get(&id).is_some_and(|it| *it == file.file.id)
            {
                continue;
            }
            let bytes = ContentId::of(blaze_vfs::FILE_DOMAIN, file.file.bytes);
            if bytes != file.file.id {
                return Err(ViewError::Mismatch {
                    path: file.path.to_owned(),
                    given: file.file.id,
                    bytes,
                });
            }
            let text = std::str::from_utf8(file.file.bytes).ok();
            let (id, root) = match (known, in_root) {
                (Some(id), Some(root)) => (id, root),
                _ => {
                    let root = view
                        .partition
                        .classify_path(&path)
                        .map_or(view.other, |set| SourceRootId(set as u32));
                    if !self.source_roots.contains_key(&root) {
                        return Err(ViewError::NoSourceRoot { path: file.path.to_owned(), root });
                    }
                    let id = match known {
                        Some(id) => id,
                        None => self
                            .unused_file_id(&mut next)
                            .ok_or_else(|| ViewError::NoFileId(file.path.to_owned()))?,
                    };
                    added.entry(root).or_default().push((id, path));
                    (id, root)
                }
            };
            verified.push((id, file.file.id));
            writes.push((id, text, root));
        }

        let mut removed: FxHashMap<SourceRootId, FxHashSet<FileId>> = FxHashMap::default();
        for entry in self.by_path.iter() {
            let (path, id) = (entry.key(), *entry.value());
            if !path.starts_with(&base) || in_view.contains(path) {
                continue;
            }
            let built = path.as_path().is_some_and(|path| {
                view.built.iter().any(|built| path.starts_with(built))
            });
            if built {
                continue;
            }
            let Some(roots) = self.in_roots.get(&id).map(|it| it.clone()) else { continue };
            for root in roots {
                removed.entry(root).or_default().insert(id);
            }
        }

        // Nothing is refused from here on.
        let mut change = ViewChange::default();
        let added_ids: FxHashSet<FileId> = added.values().flatten().map(|it| it.0).collect();
        let mut libraries: FxHashMap<SourceRootId, bool> = FxHashMap::default();
        let mut changed_roots = Vec::new();
        for (id, text, root) in writes {
            if text.is_none() {
                change.no_text.push(id);
            }
            let is_added = added_ids.contains(&id);
            if is_added {
                change.added.push(id);
            }
            let unchanged = match (text, self.files.get(&id).map(|it| *it)) {
                (Some(text), Some(input)) => {
                    FileText::content_of(text, input.appended_root_module(&*db))
                        == input.content(&*db)
                }
                (None, None) => true,
                (Some(_), None) | (None, Some(_)) => false,
            };
            if unchanged {
                continue;
            }
            let is_library = *libraries
                .entry(root)
                .or_insert_with(|| self.source_root(root).source_root(&*db).is_library);
            let durability = if is_library { Durability::HIGH } else { Durability::LOW };
            let content = self.write_text(db, id, text, Some(durability));
            self.index_file(id, content, &mut changed_roots);
            if !is_added {
                change.set.push(id);
            }
        }

        for (&root, files) in &added {
            for &(id, _) in files {
                self.set_file_source_root_with_durability(db, id, root, Durability::MEDIUM);
            }
        }
        let mut roots: Vec<SourceRootId> = added.keys().chain(removed.keys()).copied().collect();
        roots.sort_unstable();
        roots.dedup();
        for &root in &roots {
            let old = self.source_root(root).source_root(&*db);
            let gone = removed.get(&root);
            let mut files = FileSet::default();
            for id in old.iter() {
                if gone.is_some_and(|gone| gone.contains(&id)) {
                    continue;
                }
                if let Some(path) = old.path_for_file(&id) {
                    files.insert(id, path.clone());
                }
            }
            for (id, path) in added.get(&root).into_iter().flatten() {
                files.insert(*id, path.clone());
            }
            let new = match old.is_library {
                true => SourceRoot::new_library(files),
                false => SourceRoot::new_local(files),
            };
            self.set_source_root_with_durability(db, root, Arc::new(new), Durability::MEDIUM);
        }
        self.write_root_contents(db, &changed_roots);

        for id in removed.values().flatten() {
            self.view_ids.remove(id);
            change.removed.push(*id);
        }
        for (id, view_id) in verified {
            self.view_ids.insert(id, view_id);
        }
        change.roots = roots;
        for list in [&mut change.set, &mut change.added, &mut change.removed, &mut change.no_text] {
            list.sort_unstable();
            list.dedup();
        }
        Ok(change)
    }

    /// The first file ID from `next` on that nothing has.
    fn unused_file_id(&self, next: &mut u32) -> Option<FileId> {
        while *next <= span::EditionedFileId::MAX_FILE_ID {
            let id = FileId::from_raw(*next);
            *next += 1;
            let used = self.files.contains_key(&id)
                || self.without_text.contains_key(&id)
                || self.file_source_roots.contains_key(&id)
                || self.in_roots.contains_key(&id)
                || self.view_ids.contains_key(&id);
            if !used {
                return Some(id);
            }
        }
        None
    }
}

/// A path as blaze-vfs takes one.
fn check_path(path: &str) -> Result<(), ViewError> {
    let bad = path.is_empty()
        || path.starts_with('/')
        || path.split('/').any(|part| part.is_empty() || part == "." || part == "..");
    if bad { Err(ViewError::BadPath(path.to_owned())) } else { Ok(()) }
}

//! One view's files as the front end's inputs (docs/v2/plan/lanes/vfs.md section 2): a view is a root of
//! blaze-vfs, a map of every file to its content ID, and setting one writes only the inputs
//! whose content changed, so salsa sees no change where there is none.
//!
//! A view's path maps to a file and a source root through what the database has already: a
//! path a source root has named keeps its [`FileId`] for good (as `vfs::Vfs` gives one), and a
//! path none has is given an unused one and put in the source root that the partition the roots
//! were made with ([`FileSetConfig`]) puts it in.
//!
//! The content ID a view gives is blaze-vfs's ([`blaze_vfs::FILE_DOMAIN`], of the bytes
//! alone), checked against the bytes: the one time a file's bytes are hashed, every file of a
//! call in one batch. A file's [`FileText::content`] is built on it
//! ([`FileText::content_of_file`], adding the module the compiler appends), and blaze-vfs's is
//! kept to tell next time that a file is unchanged without reading it.
//!
//! A file the view has as it was costs a check of its path, one lookup of the path as the view
//! names it in its base's [`ViewIndex`], and two lookups by file ID: nothing is allocated, joined
//! or hashed with BLAKE3 for it. Only a file the source roots gain or lose rebuilds its root's
//! file set; its content ID is updated in place ([`RootContent::insert`]).

use std::{borrow::Cow, fmt};

use blaze_ids::ContentId;
use rustc_hash::{FxHashMap, FxHashSet};
use salsa::{Durability, Setter};
use triomphe::Arc;
use vfs::{
    AbsPath, AbsPathBuf, FileId, VfsPath,
    file_set::{FileSet, FileSetConfig},
};

use crate::{
    FileText, Files, FxIndexMap, SourceDatabase, SourceRoot, SourceRootId, content,
    files::{RootContent, Writer, index_file},
};

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

/// Every path under one view base that a source root has named, as a view names it (relative,
/// `/`-separated), with its file: what [`Files::set_view_root`] looks a view's path up in
/// without joining it to the base. Made from [`Writer::by_path`] the first time the base is
/// set, then kept by [`Writer::note_path`].
///
/// Per path: its text once, its file, and a 4-byte mark of the last call that saw it, so
/// finding the paths a view lacks is a pass over the marks.
pub(crate) struct ViewIndex {
    paths: FxIndexMap<Box<str>, FileId>,
    seen: Vec<u32>,
    call: u32,
}

impl ViewIndex {
    pub(crate) fn new(base: &VfsPath, by_path: &FxHashMap<VfsPath, FileId>) -> ViewIndex {
        let mut index = ViewIndex { paths: FxIndexMap::default(), seen: Vec::new(), call: 0 };
        for (path, &file) in by_path {
            index.note(base, path, file);
        }
        index
    }

    /// Notes that a source root names `file` at `path`, if `path` is under `base`.
    pub(crate) fn note(&mut self, base: &VfsPath, path: &VfsPath, file: FileId) {
        let Some(rel) = path.strip_prefix(base) else { return };
        let rel = rel_key(rel.as_str());
        if rel.is_empty() {
            return;
        }
        let (at, _) = self.paths.insert_full(Box::from(&*rel), file);
        if at == self.seen.len() {
            self.seen.push(0);
        }
    }

    /// Starts a call: no path is marked seen by it yet.
    fn begin(&mut self) -> u32 {
        self.call = self.call.wrapping_add(1);
        if self.call == 0 {
            self.seen.fill(0);
            self.call = 1;
        }
        self.call
    }

    fn find(&self, rel: &str) -> Option<usize> {
        self.paths.get_index_of(rel)
    }

    fn file(&self, at: usize) -> FileId {
        self.paths[at]
    }

    /// Marks the path at `at` seen by `call`; false if it was already.
    fn mark(&mut self, at: usize, call: u32) -> bool {
        let seen = &mut self.seen[at];
        let first = *seen != call;
        *seen = call;
        first
    }

    /// The paths `call` has not seen, with their files.
    fn unseen(&self, call: u32) -> impl Iterator<Item = (&str, FileId)> + '_ {
        self.paths
            .iter()
            .zip(&self.seen)
            .filter(move |(_, seen)| **seen != call)
            .map(|((rel, file), _)| (&**rel, *file))
    }
}

/// A path relative to a view's base as the view names it: `/`-separated.
fn rel_key(rel: &str) -> Cow<'_, str> {
    if cfg!(windows) && rel.contains('\\') { Cow::Owned(rel.replace('\\', "/")) } else { Cow::Borrowed(rel) }
}

/// Whether `rel` is `dir` or under it, both relative to one base; `""` is the base.
fn under(rel: &str, dir: &str) -> bool {
    dir.is_empty()
        || rel.strip_prefix(dir).is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// What a view will write, worked out before anything is.
struct Plan {
    /// Each file whose input may change, in the view's order.
    writes: Vec<Write>,
    /// The files put in a source root, with their paths, in the view's order.
    added: Vec<(FileId, SourceRootId, VfsPath)>,
    /// The files taken out of a source root, with it.
    removed: Vec<(SourceRootId, FileId)>,
    /// [`Writer::next_file`] once the plan is written.
    next_file: u32,
}

struct Write {
    /// Where it is in [`ViewRoot::files`].
    at: usize,
    id: FileId,
    root: SourceRootId,
    added: bool,
}

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
        let mut writer = self.lock();
        let base = VfsPath::from(view.base.to_path_buf());
        let mut index = match writer.views.remove(&base) {
            Some(index) => index,
            None => ViewIndex::new(&base, &writer.by_path),
        };
        let plan = self.plan_view(&writer, &mut index, &view);
        writer.views.insert(base, index);
        Ok(self.write_view(&mut writer, db, &view, plan?))
    }

    /// What `view` changes, or why it is refused: reads only.
    fn plan_view(
        &self,
        writer: &Writer,
        index: &mut ViewIndex,
        view: &ViewRoot<'_>,
    ) -> Result<Plan, ViewError> {
        let call = index.begin();
        let mut fresh: FxHashSet<&str> = FxHashSet::default();
        let mut plan =
            Plan { writes: Vec::new(), added: Vec::new(), removed: Vec::new(), next_file: writer.next_file };
        for (at, file) in view.files.iter().enumerate() {
            check_path(file.path)?;
            let known = match index.find(file.path) {
                Some(slot) => {
                    if !index.mark(slot, call) {
                        return Err(ViewError::Duplicate(file.path.to_owned()));
                    }
                    Some(index.file(slot))
                }
                None => {
                    if !fresh.insert(file.path) {
                        return Err(ViewError::Duplicate(file.path.to_owned()));
                    }
                    None
                }
            };
            let in_root = known.and_then(|id| writer.roots_of(id).first().copied());
            if let (Some(id), Some(_)) = (known, in_root)
                && writer.view_ids.get(&id) == Some(&file.file.id)
            {
                continue;
            }
            let (id, root, added) = match (known, in_root) {
                (Some(id), Some(root)) => (id, root, false),
                _ => {
                    let path = VfsPath::from(view.base.join(file.path));
                    let root = view
                        .partition
                        .classify_path(&path)
                        .map_or(view.other, |set| SourceRootId(set as u32));
                    if !self.has_source_root(root) {
                        return Err(ViewError::NoSourceRoot { path: file.path.to_owned(), root });
                    }
                    let id = match known {
                        Some(id) => id,
                        None => self
                            .unused_file_id(writer, &mut plan.next_file)
                            .ok_or_else(|| ViewError::NoFileId(file.path.to_owned()))?,
                    };
                    plan.added.push((id, root, path));
                    (id, root, true)
                }
            };
            plan.writes.push(Write { at, id, root, added });
        }

        let bytes: Vec<&[u8]> = plan.writes.iter().map(|it| view.files[it.at].file.bytes).collect();
        let ids = ContentId::of_many(blaze_vfs::FILE_DOMAIN, &bytes);
        for (write, &bytes) in plan.writes.iter().zip(&ids) {
            let file = &view.files[write.at];
            if bytes != file.file.id {
                return Err(ViewError::Mismatch {
                    path: file.path.to_owned(),
                    given: file.file.id,
                    bytes,
                });
            }
        }

        let built: Vec<Cow<'_, str>> = view
            .built
            .iter()
            .filter_map(|built| match view.base.starts_with(built) {
                true => Some(Cow::Borrowed("")),
                false => Some(rel_key(built.strip_prefix(view.base)?.as_str())),
            })
            .collect();
        for (rel, id) in index.unseen(call) {
            if built.iter().any(|built| under(rel, built)) {
                continue;
            }
            plan.removed.extend(writer.roots_of(id).iter().map(|&root| (root, id)));
        }
        Ok(plan)
    }

    /// Writes what [`Files::plan_view`] found, and says what it wrote.
    fn write_view(
        &self,
        writer: &mut Writer,
        db: &mut dyn SourceDatabase,
        view: &ViewRoot<'_>,
        plan: Plan,
    ) -> ViewChange {
        let mut change = ViewChange::default();
        let texts: Vec<Option<&str>> = plan
            .writes
            .iter()
            .map(|it| std::str::from_utf8(view.files[it.at].file.bytes).ok())
            .collect();
        let mut bytes = Vec::new();
        let mut ranges = Vec::new();
        for (write, text) in plan.writes.iter().zip(&texts) {
            if text.is_none() {
                continue;
            }
            let appended = self.input_of(writer, write.id).and_then(|it| it.appended_root_module(&*db));
            let start = bytes.len();
            FileText::content_bytes(&view.files[write.at].file.id, appended, &mut bytes);
            ranges.push(start..bytes.len());
        }
        let mut contents = content::hash_ranges(FileText::DOMAIN, &bytes, &ranges).into_iter();

        let mut changed_roots = Vec::new();
        for (write, text) in plan.writes.iter().zip(texts) {
            let content = match text {
                Some(_) => contents.next().expect("a text ID for each file with text"),
                None => {
                    change.no_text.push(write.id);
                    FileText::no_text()
                }
            };
            if write.added {
                change.added.push(write.id);
            }
            let unchanged = match (text, self.text_input(write.id)) {
                (Some(_), Some(input)) => input.content(&*db) == content,
                (None, None) => true,
                (Some(_), None) | (None, Some(_)) => false,
            };
            if unchanged {
                continue;
            }
            let is_library = writer
                .root_contents
                .get(&write.root)
                .expect("a source root's content is kept from when it is set")
                .is_library;
            let durability = if is_library { Durability::HIGH } else { Durability::LOW };
            let content =
                self.write_text(writer, db, write.id, text.map(|text| (text, content)), Some(durability));
            index_file(writer, write.id, content, &mut changed_roots);
            if !write.added {
                change.set.push(write.id);
            }
        }

        for &(id, root, _) in &plan.added {
            self.set_file_source_root_in(writer, db, id, root, Durability::MEDIUM);
        }
        let mut roots: Vec<SourceRootId> = plan
            .added
            .iter()
            .map(|it| it.1)
            .chain(plan.removed.iter().map(|it| it.0))
            .collect();
        roots.sort_unstable();
        roots.dedup();
        self.write_root_files(writer, db, &plan, &roots, &mut changed_roots);
        self.write_root_contents(writer, db, &changed_roots);

        for &(_, id) in &plan.removed {
            writer.view_ids.remove(&id);
            change.removed.push(id);
        }
        for write in &plan.writes {
            writer.view_ids.insert(write.id, view.files[write.at].file.id);
        }
        writer.next_file = plan.next_file;
        change.roots = roots;
        for list in [&mut change.set, &mut change.added, &mut change.removed, &mut change.no_text] {
            list.sort_unstable();
            list.dedup();
        }
        change
    }

    /// Puts the plan's added files in their source roots and takes its removed ones out: each
    /// of `roots` gets its new file set, and its content is updated for the files it gained
    /// and lost (the paths of those gained hashed in one batch), not made again from all of
    /// them. Adds to `changed` the roots whose content IDs are to be written.
    fn write_root_files(
        &self,
        writer: &mut Writer,
        db: &mut dyn SourceDatabase,
        plan: &Plan,
        roots: &[SourceRootId],
        changed: &mut Vec<SourceRootId>,
    ) {
        let keys: Vec<Vec<u8>> = plan.added.iter().map(|(_, _, path)| content::path_key(path)).collect();
        let key_bytes: Vec<&[u8]> = keys.iter().map(Vec::as_slice).collect();
        let path_hashes = ContentId::of_many(SourceRoot::PATH_DOMAIN, &key_bytes);
        let mut keys: Vec<Option<(Box<[u8]>, u8)>> = keys
            .into_iter()
            .zip(&path_hashes)
            .map(|(key, hash)| Some((key.into_boxed_slice(), RootContent::bucket(hash))))
            .collect();

        for &root in roots {
            let input = self.source_root(root);
            let old = input.source_root(&*db);
            let gone: FxHashSet<FileId> =
                plan.removed.iter().filter(|it| it.0 == root).map(|it| it.1).collect();
            let mut files = FileSet::default();
            for id in old.iter() {
                if gone.contains(&id) {
                    continue;
                }
                if let Some(path) = old.path_for_file(&id) {
                    files.insert(id, path.clone());
                }
            }
            let index = writer
                .root_contents
                .get_mut(&root)
                .expect("a source root's content is kept from when it is set");
            for &id in &gone {
                index.remove(id);
            }
            for (at, (id, added_root, path)) in plan.added.iter().enumerate() {
                if *added_root != root {
                    continue;
                }
                files.insert(*id, path.clone());
                let (key, bucket) = keys[at].take().expect("an added file is put in one root");
                index.insert(*id, key, bucket, self.entry_content(&*db, *id));
            }
            for &id in &gone {
                if let Some(roots) = writer.in_roots.get_mut(&id) {
                    roots.retain(|it| *it != root);
                }
            }
            for (id, added_root, path) in &plan.added {
                if *added_root != root {
                    continue;
                }
                let roots = writer.in_roots.entry(*id).or_default();
                if !roots.contains(&root) {
                    roots.push(root);
                }
                writer.note_path(path, *id);
            }
            let new = match old.is_library {
                true => SourceRoot::new_library(files),
                false => SourceRoot::new_local(files),
            };
            input.set_source_root_field(db).with_durability(Durability::MEDIUM).to(Arc::new(new));
            if !changed.contains(&root) {
                changed.push(root);
            }
        }
    }

    /// The first file ID from `next` on that nothing has.
    fn unused_file_id(&self, writer: &Writer, next: &mut u32) -> Option<FileId> {
        while *next <= span::EditionedFileId::MAX_FILE_ID {
            let id = FileId::from_raw(*next);
            *next += 1;
            if !self.is_used(writer, id) {
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

#[cfg(test)]
mod tests {
    use super::under;

    #[test]
    fn under_is_by_whole_parts() {
        assert!(under("target/debug/a.rs", "target"));
        assert!(under("target", "target"));
        assert!(!under("targets/a.rs", "target"));
        assert!(under("anything", ""));
    }
}

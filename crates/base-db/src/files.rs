//! [`Files`]: the inputs of files and source roots, and what queries note about the files
//! outside the VFS they read.
//!
//! What a query reads (`file_text`, `has_file_text`, `source_root`, `file_source_root`) is a
//! [`Column`] indexed by the file's or root's dense ID: two acquire loads, no lock, nothing
//! written. What a query notes (`included_file`, `include_source`, `env_is_read`,
//! `env_is_unset`) is a [`Table`] keyed by a hash computed once: a lookup of what was noted
//! already takes no lock and builds no `String` or path. Everything else is the setters'
//! own state ([`Writer`]), behind one lock that no query takes.

use std::{
    collections::BTreeMap,
    fmt,
    ops::Range,
    path::{Path, PathBuf},
    sync::{
        Mutex, MutexGuard, OnceLock, PoisonError,
        atomic::{AtomicU32, Ordering},
    },
};

use blaze_format::cursor::Put;
use blaze_ids::ContentId;
use blaze_mem::{Frame, Tag, frame, scratch};
use rustc_hash::FxHashMap;
use salsa::{Durability, Setter};
use smallvec::SmallVec;
use triomphe::Arc;
use vfs::{AbsPath, AbsPathBuf, FileId, VfsPath};

use crate::{
    Crate, Db, FileSourceRootInput, FileText, IncludedFile, InputId, ReadError, SourceRoot,
    SourceRootId, SourceRootInput, content,
    named::Named,
    reparse::Reparse,
    table::{Column, Log, Table, hash_of},
    view::ViewIndex,
};

/// The source roots a setter changed: one, or a few for a file several roots have.
pub(crate) type Roots = SmallVec<[SourceRootId; 4]>;

/// The inputs of a database's files and source roots. Shared by every handle of the database
/// (`Arc<Files>`), so a reader on one handle may run while a setter runs on another.
pub struct Files {
    /// Each file's text, while it has one. A file whose bytes are not UTF-8 has none here; its
    /// input waits in [`Writer::parked`].
    text: Column<FileText>,
    pub(crate) file_source_roots: Column<FileSourceRootInput>,
    source_roots: Column<SourceRootInput>,
    /// What only the setters read and write. Every setter runs on `&mut` of the database and
    /// no query takes this lock, so no query waits on it. A setter holds it across the salsa
    /// writes it makes; those call no query.
    writer: Mutex<Writer>,
    /// The files outside the VFS that `include_bytes!` or `include_str!` named.
    included: Table<Included>,
    /// Which crate read which of `included` (by its index there).
    included_by: Table<(Crate, u32)>,
    /// Files an `include!` or a module declaration named that are not in the VFS, with the
    /// source root of the file that names them.
    included_sources: Table<(AbsPathBuf, SourceRootId)>,
    /// Which crate named which of `included_sources` (by its index there): a file is shared by
    /// crates through `#[path]`, so the source root of the file naming one does not tell the
    /// crates that did.
    sources_included_by: Table<(Crate, u32)>,
    /// The variables `env!` and `option_env!` have asked for, by the crate asking.
    env_read: Table<(Crate, Box<str>)>,
    /// The variables `env!` has found unset, by the crate asking.
    env_unset: Table<(Crate, Box<str>)>,
    /// Every list of directories whose files are read from another directory that
    /// [`Files::set_view`] made, kept for readers that may hold one; `view` names the one in use.
    views: Log<Box<[View]>>,
    /// The index in `views` of the list in use, plus one; 0 for none.
    view: AtomicU32,
}

struct Included {
    path: AbsPathBuf,
    file: IncludedFile,
}

/// A checkout that is looked at as the one it was loaded as with other contents.
#[derive(Debug, Clone)]
struct View {
    base: PathBuf,
    over: PathBuf,
    built: Vec<PathBuf>,
}

/// What the setters keep beside the inputs, read and written only with [`Files::writer`] held.
#[derive(Default)]
pub(crate) struct Writer {
    /// The inputs of files that had text and lost it (bytes that are not UTF-8), kept so that
    /// what read them reruns if they get text again.
    pub(crate) parked: FxHashMap<FileId, FileText>,
    /// The content IDs of each source root's files, from which its own is kept.
    pub(crate) root_contents: FxHashMap<SourceRootId, RootContent>,
    /// The source roots whose [`RootContent`] has each file.
    pub(crate) in_roots: ByFile<SmallVec<[SourceRootId; 1]>>,
    /// Every path a source root has named, with its file: a path keeps its file for good.
    pub(crate) by_path: FxHashMap<VfsPath, FileId>,
    /// The blaze-vfs content ID each file was last set from by [`Files::set_view_root`].
    pub(crate) view_ids: ByFile<ContentId>,
    /// Where [`Files::unused_file_id`] starts looking for an unused file ID.
    pub(crate) next_file: u32,
    /// The crate whose crate attribute each file holds ([`crate::crate_attrs`]).
    pub(crate) crate_attrs: ByFile<Crate>,
    /// Files of crate attributes taken out, with no text: the next attribute's file is one of
    /// these.
    pub(crate) free_crate_attrs: Vec<FileId>,
    /// The index of each view base's paths that [`Files::set_view_root`] keeps.
    pub(crate) views: FxHashMap<AbsPathBuf, ViewIndex>,
    /// `by_path` as queries read it, across source roots ([`crate::named`]).
    pub(crate) named: Named,
}

impl Writer {
    /// The source roots that have `file`.
    pub(crate) fn roots_of(&self, file: FileId) -> &[SourceRootId] {
        self.in_roots.get(file).map_or(&[], |it| it.as_slice())
    }

    /// Notes that a source root names `file` at `path`.
    pub(crate) fn note_path(&mut self, path: &VfsPath, file: FileId) {
        match self.by_path.get(path) {
            Some(&known) if known == file => {}
            Some(_) => {
                // A path given another file: every view's index of paths is made again.
                // clone: the one copy of a path, made when a source root first names it or moves it.
                self.by_path.insert(path.clone(), file);
                self.views.clear();
                self.named.note(path, file);
            }
            None => {
                // clone: the one copy of a path, made when a source root first names it or moves it.
                self.by_path.insert(path.clone(), file);
                for (base, index) in &mut self.views {
                    index.note(base, path, file);
                }
                self.named.note(path, file);
            }
        }
    }
}

/// A setter's value per file, by [`FileId::index`]: a direct index where a hash map took a hash
/// and a probe. File IDs are dense, so it costs `size_of::<Option<T>>()` bytes per ID up to the
/// largest one set (33 for a [`ContentId`]).
pub(crate) struct ByFile<T>(Vec<Option<T>>);

impl<T> Default for ByFile<T> {
    fn default() -> ByFile<T> {
        ByFile(Vec::new()) // alloc: none, an empty `Vec`.
    }
}

impl<T> ByFile<T> {
    #[inline]
    pub(crate) fn get(&self, file: FileId) -> Option<&T> {
        self.0.get(file.index() as usize)?.as_ref()
    }

    #[inline]
    pub(crate) fn get_mut(&mut self, file: FileId) -> Option<&mut T> {
        self.0.get_mut(file.index() as usize)?.as_mut()
    }

    pub(crate) fn contains_key(&self, file: FileId) -> bool {
        self.get(file).is_some()
    }

    /// `file`'s place, the column grown to it.
    pub(crate) fn slot(&mut self, file: FileId) -> &mut Option<T> {
        let at = file.index() as usize;
        if at >= self.0.len() {
            // alloc: once per new largest file ID, amortized by `Vec`'s doubling.
            self.0.resize_with(at + 1, || None);
        }
        &mut self.0[at]
    }

    pub(crate) fn insert(&mut self, file: FileId, value: T) {
        *self.slot(file) = Some(value);
    }

    pub(crate) fn remove(&mut self, file: FileId) -> Option<T> {
        self.0.get_mut(file.index() as usize)?.take()
    }
}

impl Default for Files {
    fn default() -> Files {
        Files {
            text: Column::new(),
            file_source_roots: Column::new(),
            source_roots: Column::new(),
            writer: Mutex::default(),
            included: Table::new(1 << 22),
            included_by: Table::new(1 << 22),
            included_sources: Table::new(1 << 22),
            sources_included_by: Table::new(1 << 22),
            env_read: Table::new(1 << 22),
            env_unset: Table::new(1 << 22),
            views: Log::new(1 << 16),
            view: AtomicU32::new(0),
        }
    }
}

impl fmt::Debug for Files {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Files").finish_non_exhaustive()
    }
}

impl Files {
    pub(crate) fn lock(&self) -> MutexGuard<'_, Writer> {
        self.writer.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn file_text(&self, file_id: FileId) -> FileText {
        match self.text.get(file_id.index()) {
            Some(text) => text,
            None => {
                panic!("Unable to fetch file text for `vfs::FileId`: {file_id:?}; this is a bug")
            }
        }
    }

    /// Whether the file was loaded as text. A file of the VFS that is not UTF-8 has an id and
    /// no text.
    pub fn has_file_text(&self, file_id: FileId) -> bool {
        self.text.get(file_id.index()).is_some()
    }

    pub fn set_file_text(&self, db: &mut Db, file_id: FileId, text: &str) {
        self.set_text(db, file_id, text, None);
    }

    pub fn set_file_text_with_durability(
        &self,
        db: &mut Db,
        file_id: FileId,
        text: &str,
        durability: Durability,
    ) {
        self.set_text(db, file_id, text, Some(durability));
    }

    /// Sets the module the compiler appends to the crate root `file_id`'s text, see
    /// [`FileText::appended_root_module`]. Writes nothing if it is that already.
    pub fn set_appended_root_module(
        &self,
        db: &mut Db,
        file_id: FileId,
        module: Option<&'static str>,
        durability: Durability,
    ) {
        frame::task(Tag::Parse, |frame| {
            let mut writer = self.lock();
            let input = self.file_text(file_id);
            if input.appended_root_module(&*db) == module {
                return;
            }
            let content = FileText::content_of(input.text(&*db), module);
            input.set_appended_root_module_field(db).with_durability(durability).to(module);
            input.set_content_field(db).with_durability(durability).to(InputId(content));
            let mut changed = Roots::new();
            index_file(&mut writer, file_id, content, &mut changed);
            self.write_root_contents(&mut writer, db, &changed, frame);
        })
    }

    /// Sets a file's text from outside a view: what it was set from by a view is forgotten.
    fn set_text(
        &self,
        db: &mut Db,
        file_id: FileId,
        text: &str,
        durability: Option<Durability>,
    ) {
        frame::task(Tag::Parse, |frame| {
            let mut writer = self.lock();
            self.refuse_crate_attr(&writer, file_id);
            writer.view_ids.remove(file_id);
            let appended =
                self.input_of(&writer, file_id).and_then(|it| it.appended_root_module(&*db));
            let file = ContentId::of(blaze_vfs::FILE_DOMAIN, text.as_bytes());
            let content = FileText::content_of_file(file, appended);
            let text = syntax::Text::from(text);
            self.write_text(&mut writer, db, file_id, Some((text, file, content)), durability);
            let mut changed = Roots::new();
            index_file(&mut writer, file_id, content, &mut changed);
            self.write_root_contents(&mut writer, db, &changed, frame);
        })
    }

    /// The text input of `file` while it has text.
    pub(crate) fn text_input(&self, file: FileId) -> Option<FileText> {
        self.text.get(file.index())
    }

    /// The text input of `file`, with text or without.
    pub(crate) fn input_of(&self, writer: &Writer, file: FileId) -> Option<FileText> {
        self.text.get(file.index()).or_else(|| writer.parked.get(&file).copied())
    }

    /// Sets the text of `file_id` (`file` the blaze-vfs ID of its bytes) with its
    /// [`FileText::content`] (made with the module the input appends, if it has one), or that
    /// it has none (`None`), and returns its content ID in a source root. The edit from the
    /// text it had is found here, for its next parse ([`Reparse::after`]). The source roots
    /// that have the file are left to the caller.
    pub(crate) fn write_text(
        &self,
        writer: &mut Writer,
        db: &mut Db,
        file_id: FileId,
        text: Option<(syntax::Text, ContentId, ContentId)>,
        durability: Option<Durability>,
    ) -> ContentId {
        let raw = file_id.index();
        let Some((text, file, content)) = text else {
            if let Some(input) = self.text.get(raw) {
                // Nothing reads the text left behind: `file_text` no longer has the file.
                self.text.set(raw, None);
                let empty = syntax::Text::from_static("");
                let reparse = Reparse::new(&empty, ContentId::of(blaze_vfs::FILE_DOMAIN, b""));
                set(input.set_reparse_field(db), durability, reparse);
                set(input.set_text_field(db), durability, empty);
                set(input.set_content_field(db), durability, InputId(FileText::no_text()));
                writer.parked.insert(file_id, input);
            }
            return FileText::no_text();
        };
        let existing = match self.text.get(raw) {
            Some(input) => Some(input),
            None => writer.parked.remove(&file_id),
        };
        match existing {
            Some(input) => {
                let reparse = input.reparse_field(&*db).after(&text, file);
                set(input.set_reparse_field(db), durability, reparse);
                set(input.set_text_field(db), durability, text);
                if input.content(&*db) != content {
                    set(input.set_content_field(db), durability, InputId(content));
                }
                self.text.set(raw, Some(input));
            }
            None => {
                let reparse = Reparse::new(&text, file);
                let builder = FileText::builder(text, file_id, InputId(content), reparse);
                let builder = match durability {
                    Some(durability) => builder.durability(durability),
                    None => builder,
                };
                self.text.set(raw, Some(builder.new(&*db)));
            }
        }
        content
    }

    /// The content ID `file` has in a source root.
    pub(crate) fn entry_content(&self, db: &Db, file: FileId) -> ContentId {
        match self.text.get(file.index()) {
            Some(input) => input.content(db),
            None => FileText::no_text(),
        }
    }

    /// Brings the content IDs of `roots` up to date and writes those that changed.
    pub(crate) fn write_root_contents(
        &self,
        writer: &mut Writer,
        db: &mut Db,
        roots: &[SourceRootId],
        frame: &Frame,
    ) {
        RootContent::refresh(&mut writer.root_contents, roots, frame);
        for &root in roots {
            let Some(index) = writer.root_contents.get(&root) else { continue };
            let Some(input) = self.source_roots.get(root.0) else { continue };
            let content = InputId(index.content);
            if input.content_field(&*db) != content {
                input
                    .set_content_field(db)
                    .with_durability(root_content_durability(index.is_library))
                    .to(content);
            }
        }
    }

    /// The content of the source root `id` as `root` has it, noting which of its files and
    /// paths it has. Buckets whose files are what they were keep their hashes.
    fn index_root(
        &self,
        writer: &mut Writer,
        db: &Db,
        id: SourceRootId,
        root: &SourceRoot,
        frame: &Frame,
    ) -> RootContent {
        let old = writer.root_contents.remove(&id);
        if let Some(old) = &old {
            for file in old.keys.keys() {
                if let Some(roots) = writer.in_roots.get_mut(*file) {
                    roots.retain(|it| *it != id);
                }
            }
        }
        let mut files = Vec::with_capacity_in(root.len(), frame);
        for file in root.iter() {
            let Some(path) = root.path_for_file(&file) else { continue };
            // A path two files were given is the last one's.
            if root.file_for_path(path) != Some(&file) {
                continue;
            }
            writer.note_path(path, file);
            let roots = writer.in_roots.slot(file).get_or_insert_default();
            if !roots.contains(&id) {
                roots.push(id);
            }
            files.push((file, content::path_key(path), self.entry_content(db, file)));
        }
        RootContent::new(root.is_library, files, old.as_ref(), frame)
    }

    /// Has the files under `base` read from where they are under `over` from now on, but for
    /// those under one of `built`: what a build writes is not part of a checkout. Nothing is
    /// read again for it: whoever watches the disk does that.
    pub fn set_view(&self, base: &Path, over: &Path, built: Vec<PathBuf>) {
        // Held so that two of these don't each drop the other's change.
        let _writer = self.lock();
        let mut views: Vec<View> =
            self.views_in_use().iter().filter(|it| it.base != base).cloned().collect(); // alloc: setup: a new list of a few views, published once.
        if base != over {
            // alloc: as above.
            views.push(View { base: base.to_owned(), over: over.to_owned(), built });
        }
        let raw = self.views.push(views.into_boxed_slice());
        self.view.store(raw + 1, Ordering::Release);
    }

    fn views_in_use(&self) -> &[View] {
        match self.view.load(Ordering::Acquire) {
            0 => &[],
            plus_one => self.views.get(plus_one - 1).map_or(&[], |it| &**it),
        }
    }

    /// Where the file known as `path` is read from.
    pub fn on_disk(&self, path: &Path) -> PathBuf {
        let over = self
            .views_in_use()
            .iter()
            .filter(|it| !it.built.iter().any(|built| path.starts_with(built)))
            .find_map(|it| Some(it.over.join(path.strip_prefix(&it.base).ok()?)));
        // alloc: the edge: the path is handed to `std::fs::read` on a miss.
        over.unwrap_or_else(|| path.to_owned())
    }

    /// A file outside the VFS that `include_bytes!` or `include_str!` names, read from disk the
    /// first time it is asked for. Whoever watches the disk keeps it current with
    /// [`Files::reload_included_file`]. Two readers that ask for it first at once each read it,
    /// with no lock held; the first to finish is kept.
    pub fn included_file(
        &self,
        db: &Db,
        krate: Crate,
        path: &AbsPath,
        durability: Durability,
    ) -> IncludedFile {
        let hash = hash_of(path);
        let at = match self.included.find(hash, |it| it.path.as_path() == path) {
            Some(at) => at,
            None => {
                let read = self.read_included(path);
                let content = InputId(IncludedFile::content_of(read.as_deref()));
                let file = IncludedFile::builder(read, content).durability(durability).new(db);
                let make = || Included { path: path.to_path_buf(), file };
                self.included.insert(hash, |it| it.path.as_path() == path, make).0
            }
        };
        self.included_by.insert(
            hash_of((krate, at)),
            |&(by, of)| by == krate && of == at,
            || (krate, at),
        );
        self.included.get(at).file
    }

    /// The files outside the VFS that `krate` has read, with the bytes it read of each:
    /// `None` for one that could not be read. Sorted by path.
    pub fn included_by(
        &self,
        db: &Db,
        krate: Crate,
    ) -> Vec<(AbsPathBuf, Option<std::sync::Arc<[u8]>>)> {
        let mut read: Vec<_> = self
            .included_by
            .iter()
            .filter(|(by, _)| *by == krate)
            .map(|&(_, at)| {
                let included = self.included.get(at);
                // clone: the edge: the list leaves base-db, read once per build (a path and a refcount each).
                (included.path.clone(), included.file.bytes(db).cloned())
            })
            .collect(); // alloc: the edge, as above.
        read.sort_by(|a, b| a.0.cmp(&b.0));
        read
    }

    fn read_included(&self, path: &AbsPath) -> Result<std::sync::Arc<[u8]>, ReadError> {
        std::fs::read(self.on_disk(path.as_ref())).map(Into::into).map_err(|it| ReadError::of(&it))
    }

    fn find_included(&self, path: &AbsPath) -> Option<IncludedFile> {
        let at = self.included.find(hash_of(path), |it| it.path.as_path() == path)?;
        Some(self.included.get(at).file)
    }

    /// Notes a file an `include!` or a module declaration names that the VFS has not: whoever
    /// owns the VFS puts it in the source root, see [`Files::included_sources`].
    pub fn include_source(&self, path: &AbsPath, root: SourceRootId, krate: Crate) {
        let (at, _) = self.included_sources.insert(
            hash_of(path),
            |(known, _)| known.as_path() == path,
            || (path.to_path_buf(), root),
        );
        self.sources_included_by.insert(
            hash_of((krate, at)),
            |&(by, of)| by == krate && of == at,
            || (krate, at),
        );
    }

    /// The crates that named each file of [`Files::included_sources`], sorted.
    pub fn sources_included_by(&self) -> Vec<(Crate, AbsPathBuf)> {
        let mut named: Vec<_> = self
            .sources_included_by
            .iter()
            // clone: the edge: a sorted list for whoever loads included sources.
            .map(|&(krate, at)| (krate, self.included_sources.get(at).0.clone()))
            .collect(); // alloc: as above.
        named.sort();
        named
    }

    /// Sorted by path.
    pub fn included_sources(&self) -> Vec<(AbsPathBuf, SourceRootId)> {
        // alloc: the edge, as above.
        let mut sources: Vec<_> = self.included_sources.iter().cloned().collect();
        sources.sort();
        sources
    }

    pub fn env_is_read(&self, krate: Crate, key: &str) {
        note_env(&self.env_read, krate, key);
    }

    /// Sorted.
    pub fn env_read(&self) -> Vec<(Crate, String)> {
        env_list(&self.env_read)
    }

    pub fn env_is_unset(&self, krate: Crate, key: &str) {
        note_env(&self.env_unset, krate, key);
    }

    /// Sorted.
    pub fn env_unset(&self) -> Vec<(Crate, String)> {
        env_list(&self.env_unset)
    }

    /// Sorted.
    pub fn included_files(&self) -> Vec<AbsPathBuf> {
        // alloc: the edge, as above.
        let mut paths: Vec<_> = self.included.iter().map(|it| it.path.clone()).collect();
        paths.sort();
        paths
    }

    /// Whether the file on disk is what was read of it.
    pub fn included_is_read(&self, db: &Db, path: &AbsPath) -> bool {
        let Some(file) = self.find_included(path) else { return false };
        *file.read(db) == self.read_included(path)
    }

    /// Reads the file again, and says whether it changed.
    pub fn reload_included_file(&self, db: &mut Db, path: &AbsPath) -> bool {
        let Some(file) = self.find_included(path) else { return false };
        let read = self.read_included(path);
        let changed = *file.read(&*db) != read;
        if changed {
            let content = InputId(IncludedFile::content_of(read.as_deref()));
            file.set_read_field(db).to(read);
            file.set_content_field(db).to(content);
        }
        changed
    }

    /// Source root of the file.
    pub fn source_root(&self, source_root_id: SourceRootId) -> SourceRootInput {
        match self.source_roots.get(source_root_id.0) {
            Some(source_root) => source_root,
            None => panic!(
                "Unable to fetch `SourceRootInput` with `SourceRootId` ({source_root_id:?}); this is a bug"
            ),
        }
    }

    pub(crate) fn has_source_root(&self, id: SourceRootId) -> bool {
        self.source_roots.get(id.0).is_some()
    }

    pub fn set_source_root_with_durability(
        &self,
        db: &mut Db,
        source_root_id: SourceRootId,
        source_root: Arc<SourceRoot>,
        durability: Durability,
    ) {
        frame::task(Tag::Parse, |frame| {
            let mut writer = self.lock();
            for file in source_root.iter() {
                self.refuse_crate_attr(&writer, file);
            }
            let index = self.index_root(&mut writer, &*db, source_root_id, &source_root, frame);
            let is_library = index.is_library;
            writer.root_contents.insert(source_root_id, index);
            RootContent::refresh(&mut writer.root_contents, &[source_root_id], frame);
            let content = InputId(writer.root_contents[&source_root_id].content);
            let content_durability = root_content_durability(is_library);
            match self.source_roots.get(source_root_id.0) {
                Some(input) => {
                    input.set_source_root_field(db).with_durability(durability).to(source_root);
                    if input.content_field(&*db) != content {
                        input.set_content_field(db).with_durability(content_durability).to(content);
                    }
                }
                None => {
                    let input = SourceRootInput::builder(source_root, content)
                        .durability(durability)
                        .content_durability(content_durability)
                        .new(&*db);
                    self.source_roots.set(source_root_id.0, Some(input));
                }
            }
            writer.named.publish(db, frame);
        })
    }

    pub fn file_source_root(&self, db: &Db, id: FileId) -> FileSourceRootInput {
        match self.file_source_roots.get(id.index()) {
            Some(file_source_root) => file_source_root,
            None => panic!(
                "Unable to get `FileSourceRootInput` with `vfs::FileId` ({id:?}, path: {}); this is a bug",
                self.path_for_file(db, id)
                    // alloc: the panic's message.
                    .map_or_else(|| "<unknown>".to_owned(), |path| path.to_string()),
            ),
        }
    }

    fn path_for_file(&self, db: &Db, id: FileId) -> Option<VfsPath> {
        self.source_roots
            .iter()
            .find_map(|(_, source_root)| source_root.source_root_ref(db).path_for_file(&id).cloned())
    }

    pub fn set_file_source_root_with_durability(
        &self,
        db: &mut Db,
        id: FileId,
        source_root_id: SourceRootId,
        durability: Durability,
    ) {
        let mut writer = self.lock();
        self.refuse_crate_attr(&writer, id);
        self.set_file_source_root_in(&mut writer, db, id, source_root_id, durability);
    }

    /// [`Files::set_file_source_root_with_durability`] with the writer's lock held.
    pub(crate) fn set_file_source_root_in(
        &self,
        _: &mut Writer,
        db: &mut Db,
        id: FileId,
        source_root_id: SourceRootId,
        durability: Durability,
    ) {
        match self.file_source_roots.get(id.index()) {
            Some(input) => {
                input.set_source_root_id(db).with_durability(durability).to(source_root_id);
            }
            None => {
                let input =
                    FileSourceRootInput::builder(source_root_id).durability(durability).new(&*db);
                self.file_source_roots.set(id.index(), Some(input));
            }
        }
    }

    /// Whether anything has `id`: a file ID [`Files::set_view_root`] may not give a new path.
    pub(crate) fn is_used(&self, writer: &Writer, id: FileId) -> bool {
        self.text.get(id.index()).is_some()
            || writer.parked.contains_key(&id)
            || self.file_source_roots.get(id.index()).is_some()
            || writer.in_roots.contains_key(id)
            || writer.view_ids.contains_key(id)
            || writer.crate_attrs.contains_key(id)
    }

    /// The first file ID from `next` on that nothing has: the one place base-db hands out file
    /// IDs, for a view's new files ([`Files::set_view_root`]) and crate attributes' files
    /// ([`crate::crate_attrs`]).
    pub(crate) fn unused_file_id(&self, writer: &Writer, next: &mut u32) -> Option<FileId> {
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

fn note_env(table: &Table<(Crate, Box<str>)>, krate: Crate, key: &str) {
    table.insert(hash_of((krate, key)), |(by, known)| *by == krate && **known == *key, || {
        (krate, key.into())
    });
}

fn env_list(table: &Table<(Crate, Box<str>)>) -> Vec<(Crate, String)> {
    // alloc: the edge, as above.
    let mut list: Vec<_> = table.iter().map(|(krate, key)| (*krate, key.to_string())).collect();
    list.sort();
    list
}

/// Puts `content` as `file`'s in each source root that has it, adding to `changed` those whose
/// content it changes. Their content IDs are written by [`Files::write_root_contents`].
pub(crate) fn index_file(
    writer: &mut Writer,
    file: FileId,
    content: ContentId,
    changed: &mut Roots,
) {
    let Some(roots) = writer.in_roots.get(file) else { return };
    for &root in roots {
        let Some(index) = writer.root_contents.get_mut(&root) else { continue };
        if index.set(file, content) && !changed.contains(&root) {
            changed.push(root);
        }
    }
}

/// The files of one source root as its content ID is made of them (see [`SourceRoot::DOMAIN`]),
/// with the hash of each bucket kept and only those a change touches hashed again.
#[derive(Debug)]
pub(crate) struct RootContent {
    pub(crate) is_library: bool,
    /// Each file's bucket and key (its path, encoded).
    keys: FxHashMap<FileId, (u8, Box<[u8]>)>,
    // BTreeMap: hashed in key order into the root's content ID, edited in place between hashes.
    buckets: Vec<BTreeMap<Box<[u8]>, ContentId>>,
    hashes: Vec<ContentId>,
    /// The buckets whose hash is out of date, a bit each.
    dirty: u64,
    /// The root's content ID, when `dirty` is 0.
    content: ContentId,
}

const _: () = assert!(SourceRoot::BUCKETS == u64::BITS as usize);

impl RootContent {
    fn empty(is_library: bool) -> RootContent {
        RootContent {
            is_library,
            keys: FxHashMap::default(),
            // alloc: once per source root set (setup).
            buckets: vec![BTreeMap::new(); SourceRoot::BUCKETS],
            // alloc: as above.
            hashes: vec![ContentId::default(); SourceRoot::BUCKETS],
            dirty: u64::MAX,
            content: ContentId::default(),
        }
    }

    /// The content of a root of `files` (file, path key, content ID), each path once. The
    /// buckets of `old` whose files are the same keep their hash, and each key whose file had
    /// it in `old` keeps its bucket; the others' buckets are hashed in one batch.
    fn new(
        is_library: bool,
        files: Vec<(FileId, Box<[u8]>, ContentId), &Frame>,
        old: Option<&RootContent>,
        frame: &Frame,
    ) -> RootContent {
        let mut buckets = Vec::with_capacity_in(files.len(), frame);
        let mut hash = Vec::new_in(frame);
        for (at, (file, key, _)) in files.iter().enumerate() {
            match old.and_then(|old| old.keys.get(file)) {
                Some((bucket, known)) if **known == **key => buckets.push(*bucket),
                _ => {
                    buckets.push(0);
                    hash.push(at);
                }
            }
        }
        let keys = frame.alloc_slice_from_iter(hash.iter().map(|&at| &*files[at].1));
        let hashes = content::hash_many(SourceRoot::PATH_DOMAIN, keys, frame);
        for (&at, path) in hash.iter().zip(hashes) {
            buckets[at] = RootContent::bucket(path);
        }
        let mut content = RootContent::empty(is_library);
        content.keys.reserve(files.len());
        for ((file, key, id), bucket) in files.into_iter().zip(buckets) {
            // clone: the bucket's copy of a key, per file of a root set (setup).
            content.buckets[bucket as usize].insert(key.clone(), id);
            content.keys.insert(file, (bucket, key));
        }
        if let Some(old) = old
            && old.is_library == is_library
        {
            for bucket in 0..SourceRoot::BUCKETS {
                let bit = 1 << bucket;
                if old.dirty & bit == 0 && old.buckets[bucket] == content.buckets[bucket] {
                    content.hashes[bucket] = old.hashes[bucket];
                    content.dirty &= !bit;
                }
            }
            if content.dirty == 0 && old.dirty == 0 {
                content.content = old.content;
            }
        }
        content
    }

    /// The bucket of a path whose [`SourceRoot::PATH_DOMAIN`] hash is `path`.
    pub(crate) fn bucket(path: &ContentId) -> u8 {
        (path.0[0] as usize % SourceRoot::BUCKETS) as u8
    }

    /// Puts `file` in the root at `key` (its bucket `bucket`) with `content`: a file another
    /// file's key had is taken from it.
    pub(crate) fn insert(&mut self, file: FileId, key: Box<[u8]>, bucket: u8, content: ContentId) {
        self.remove(file);
        let map = &mut self.buckets[bucket as usize];
        if map.contains_key(&key) {
            self.keys.retain(|_, (at, known)| !(*at == bucket && **known == *key));
        }
        // clone: the bucket's copy of a key, per file a root gains.
        map.insert(key.clone(), content);
        self.keys.insert(file, (bucket, key));
        self.dirty |= 1 << bucket;
    }

    /// Takes `file` out of the root; whether it had it.
    pub(crate) fn remove(&mut self, file: FileId) -> bool {
        let Some((bucket, key)) = self.keys.remove(&file) else { return false };
        self.buckets[bucket as usize].remove(&key);
        self.dirty |= 1 << bucket;
        true
    }

    /// Sets the content ID of `file`, if the root has it; whether the root's changed.
    fn set(&mut self, file: FileId, content: ContentId) -> bool {
        let Some((bucket, key)) = self.keys.get(&file) else { return false };
        let bucket = *bucket;
        let Some(slot) = self.buckets[bucket as usize].get_mut(key) else { return false };
        if *slot == content {
            return false;
        }
        *slot = content;
        self.dirty |= 1 << bucket;
        true
    }

    /// What [`SourceRoot::BUCKET_DOMAIN`] hashes of a bucket.
    fn bucket_bytes(&self, bucket: usize, out: &mut Vec<u8>) {
        let files = &self.buckets[bucket];
        out.put_u64(files.len() as u64);
        for (key, content) in files {
            out.put_bytes_u64(key);
            out.put_bytes(&content.0);
        }
    }

    /// Brings each of `roots`' content IDs up to date: every bucket out of date in any of them
    /// hashed in one batch, then the roots in another.
    pub(crate) fn refresh(
        contents: &mut FxHashMap<SourceRootId, RootContent>,
        roots: &[SourceRootId],
        frame: &Frame,
    ) {
        let mut bytes = scratch::lend_bytes();
        let mut ranges: Vec<Range<usize>, &Frame> = Vec::new_in(frame);
        let mut places = Vec::new_in(frame);
        for root in roots {
            let Some(index) = contents.get(root) else { continue };
            if places.iter().any(|(known, _)| known == root) {
                continue;
            }
            let mut dirty = index.dirty;
            while dirty != 0 {
                let bucket = dirty.trailing_zeros() as usize;
                dirty &= dirty - 1;
                let start = bytes.len();
                index.bucket_bytes(bucket, &mut bytes);
                ranges.push(start..bytes.len());
                places.push((*root, bucket));
            }
        }
        if places.is_empty() {
            return;
        }
        let hashes = content::hash_ranges(SourceRoot::BUCKET_DOMAIN, &bytes, &ranges, frame);
        for (&(root, bucket), &hash) in places.iter().zip(hashes) {
            if let Some(index) = contents.get_mut(&root) {
                index.hashes[bucket] = hash;
            }
        }

        bytes.clear();
        ranges.clear();
        let mut refreshed = Vec::new_in(frame);
        for root in roots {
            let Some(index) = contents.get_mut(root) else { continue };
            if index.dirty == 0 {
                continue;
            }
            index.dirty = 0;
            let start = bytes.len();
            bytes.push(index.is_library as u8);
            for hash in &index.hashes {
                bytes.extend_from_slice(&hash.0);
            }
            ranges.push(start..bytes.len());
            refreshed.push(*root);
        }
        let hashes = content::hash_ranges(SourceRoot::DOMAIN, &bytes, &ranges, frame);
        for (root, &hash) in refreshed.into_iter().zip(hashes) {
            if let Some(index) = contents.get_mut(&root) {
                index.content = hash;
            }
        }
    }
}

/// A source root's content ID changes with its files' texts, so it is as durable as they are.
pub(crate) fn root_content_durability(is_library: bool) -> Durability {
    if is_library { Durability::HIGH } else { Durability::LOW }
}

pub(crate) fn set<S: Setter>(setter: S, durability: Option<Durability>, value: S::FieldTy) {
    let setter = match durability {
        Some(durability) => setter.with_durability(durability),
        None => setter,
    };
    setter.to(value);
}

/// [`FileText::no_text`], hashed once.
pub(crate) fn no_text() -> ContentId {
    static NO_TEXT: OnceLock<ContentId> = OnceLock::new();
    *NO_TEXT.get_or_init(|| ContentId::of(FileText::NO_TEXT_DOMAIN, b""))
}

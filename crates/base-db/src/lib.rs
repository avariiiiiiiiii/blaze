//! This crate defines the basic database trait for interacting with source code using [`salsa`].
//!
//! The concrete implementation DB is defined by ide.

#![cfg_attr(feature = "in-rust-tree", feature(rustc_private))]

#[cfg(feature = "in-rust-tree")]
extern crate rustc_driver as _;

pub use salsa;
use span::TextSize;

mod change;
mod content;
mod editioned_file_id;
mod input;
pub mod target;
#[cfg(test)]
mod tests;
mod view;

use std::{
    cell::RefCell,
    collections::BTreeMap,
    hash::BuildHasherDefault,
    panic,
    str::FromStr as _,
    sync::{Mutex, Once, atomic::AtomicUsize},
};

pub use crate::{
    change::FileChange,
    content::{CFG_OPTIONS_DOMAIN, InputId, cfg_options_content},
    editioned_file_id::EditionedFileId,
    input::{
        BuiltCrateData, BuiltDependency, Crate, CrateBuilder, CrateBuilderId, CrateDataBuilder,
        CrateDisplayName, CrateGraphBuilder, CrateName, CrateOrigin, CratesIdMap, CratesMap,
        DependencyBuilder, Env, ExtraCrateData, LangCrateOrigin, ProcMacroLoadingError,
        ProcMacroPaths, ReleaseChannel, SourceRoot, SourceRootId, TargetKind, UniqueCrateData,
    },
    view::{ViewChange, ViewError, ViewFile, ViewRoot},
};
pub use blaze_ids::ContentId;
use blaze_ids::Hasher;
use dashmap::{DashMap, mapref::entry::Entry};
use rustc_hash::{FxHashMap, FxHashSet, FxHasher};
use salsa::{Durability, Setter};
pub use semver::{BuildMetadata, Prerelease, Version, VersionReq};
use triomphe::Arc;
use vfs::AbsPath;
pub use vfs::{AbsPathBuf, AnchoredPath, AnchoredPathBuf, FileId, VfsPath, file_set::FileSet};

pub type FxIndexSet<T> = indexmap::IndexSet<T, rustc_hash::FxBuildHasher>;
pub type FxIndexMap<K, V> =
    indexmap::IndexMap<K, V, std::hash::BuildHasherDefault<rustc_hash::FxHasher>>;

#[macro_export]
macro_rules! impl_intern_key {
    ($id:ident, $loc:ident) => {
        #[salsa::interned(unsafe(no_lifetime), revisions = usize::MAX)]
        #[derive(PartialOrd, Ord)]
        pub struct $id {
            #[returns(ref)]
            pub loc: $loc,
        }

        // If we derive this salsa prints the values recursively, and this causes us to blow.
        impl ::std::fmt::Debug for $id {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.debug_tuple(stringify!($id))
                    .field(&format_args!("{:04x}", self.0.index()))
                    .finish()
            }
        }
    };
}

pub const DEFAULT_FILE_TEXT_LRU_CAP: u16 = 16;
pub const DEFAULT_PARSE_LRU_CAP: u16 = 128;
pub const DEFAULT_BORROWCK_LRU_CAP: u16 = 2024;

#[derive(Debug, Default)]
pub struct Files {
    files: Arc<DashMap<vfs::FileId, FileText, BuildHasherDefault<FxHasher>>>,
    /// The inputs of files that had text and lost it (bytes that are not UTF-8), kept so that
    /// what read them reruns if they get text again.
    without_text: Arc<DashMap<vfs::FileId, FileText, BuildHasherDefault<FxHasher>>>,
    source_roots: Arc<DashMap<SourceRootId, SourceRootInput, BuildHasherDefault<FxHasher>>>,
    /// The content IDs of each source root's files, from which its own is kept.
    root_contents: Arc<DashMap<SourceRootId, RootContent, BuildHasherDefault<FxHasher>>>,
    /// The source roots whose [`RootContent`] has each file.
    in_roots: Arc<DashMap<vfs::FileId, Vec<SourceRootId>, BuildHasherDefault<FxHasher>>>,
    /// Every path a source root has named, with its file: a path keeps its file for good.
    by_path: Arc<DashMap<VfsPath, vfs::FileId, BuildHasherDefault<FxHasher>>>,
    /// The blaze-vfs content ID each file was last set from by [`Files::set_view_root`].
    view_ids: Arc<DashMap<vfs::FileId, ContentId, BuildHasherDefault<FxHasher>>>,
    /// Where [`Files::set_view_root`] starts looking for an unused file ID.
    next_file: Arc<Mutex<u32>>,
    file_source_roots: Arc<DashMap<vfs::FileId, FileSourceRootInput, BuildHasherDefault<FxHasher>>>,
    included: Arc<DashMap<AbsPathBuf, IncludedFile, BuildHasherDefault<FxHasher>>>,
    /// Which crate's `include_bytes!` or `include_str!` read which of those.
    included_by: Arc<DashMap<(Crate, AbsPathBuf), (), BuildHasherDefault<FxHasher>>>,
    /// Files an `include!` or a module declaration named that are not in the VFS, with the
    /// source root of the file that names them.
    included_sources: Arc<DashMap<AbsPathBuf, SourceRootId, BuildHasherDefault<FxHasher>>>,
    /// Which crate named which of those: a file is shared by crates through `#[path]`, so the
    /// source root of the file naming one does not tell the crates that did.
    sources_included_by: Arc<DashMap<(Crate, AbsPathBuf), (), BuildHasherDefault<FxHasher>>>,
    /// Directories whose files are read from another directory: a checkout that is looked at
    /// as the one it was loaded as with other contents.
    views: Arc<std::sync::RwLock<Vec<View>>>,
    /// The variables `env!` and `option_env!` have asked for, by the crate asking.
    env_read: Arc<DashMap<(Crate, String), (), BuildHasherDefault<FxHasher>>>,
    /// The variables `env!` has found unset, by the crate asking.
    env_unset: Arc<DashMap<(Crate, String), (), BuildHasherDefault<FxHasher>>>,
}

#[derive(Debug)]
struct View {
    base: std::path::PathBuf,
    over: std::path::PathBuf,
    built: Vec<std::path::PathBuf>,
}

impl Files {
    pub fn file_text(&self, file_id: vfs::FileId) -> FileText {
        match self.files.get(&file_id) {
            Some(text) => *text,
            None => {
                panic!("Unable to fetch file text for `vfs::FileId`: {file_id:?}; this is a bug")
            }
        }
    }

    /// Whether the file was loaded as text. A file of the VFS that is not UTF-8 has an id and
    /// no text.
    pub fn has_file_text(&self, file_id: vfs::FileId) -> bool {
        self.files.contains_key(&file_id)
    }

    pub fn set_file_text(&self, db: &mut dyn SourceDatabase, file_id: vfs::FileId, text: &str) {
        self.set_text(db, file_id, Some(text), None);
    }

    pub fn set_file_text_with_durability(
        &self,
        db: &mut dyn SourceDatabase,
        file_id: vfs::FileId,
        text: &str,
        durability: Durability,
    ) {
        self.set_text(db, file_id, Some(text), Some(durability));
    }

    /// Sets the module the compiler appends to the crate root `file_id`'s text, see
    /// [`FileText::appended_root_module`]. Writes nothing if it is that already.
    pub fn set_appended_root_module(
        &self,
        db: &mut dyn SourceDatabase,
        file_id: vfs::FileId,
        module: Option<&'static str>,
        durability: Durability,
    ) {
        let input = self.file_text(file_id);
        if input.appended_root_module(&*db) == module {
            return;
        }
        let content = FileText::content_of(input.text(&*db), module);
        input.set_appended_root_module_field(db).with_durability(durability).to(module);
        input.set_content_field(db).with_durability(durability).to(InputId(content));
        let mut changed = Vec::new();
        self.index_file(file_id, content, &mut changed);
        self.write_root_contents(db, &changed);
    }

    /// Sets a file's text from outside a view: what it was set from by a view is forgotten.
    fn set_text(
        &self,
        db: &mut dyn SourceDatabase,
        file_id: vfs::FileId,
        text: Option<&str>,
        durability: Option<Durability>,
    ) {
        self.view_ids.remove(&file_id);
        let content = self.write_text(db, file_id, text, durability);
        let mut changed = Vec::new();
        self.index_file(file_id, content, &mut changed);
        self.write_root_contents(db, &changed);
    }

    /// Sets the text of `file_id`, or that it has none (`None`), with its content ID, and
    /// returns that ID. The source roots that have the file are left to the caller.
    fn write_text(
        &self,
        db: &mut dyn SourceDatabase,
        file_id: vfs::FileId,
        text: Option<&str>,
        durability: Option<Durability>,
    ) -> ContentId {
        let Some(text) = text else {
            // Nothing reads the text left behind: `file_text` no longer has the file.
            if let Some((_, input)) = self.files.remove(&file_id) {
                set(input.set_text_field(db), durability, Arc::from(""));
                set(input.set_content_field(db), durability, InputId(FileText::no_text()));
                self.without_text.insert(file_id, input);
            }
            return FileText::no_text();
        };
        let existing = match self.files.get(&file_id).map(|it| *it) {
            Some(input) => Some(input),
            None => self.without_text.remove(&file_id).map(|(_, input)| input),
        };
        match existing {
            Some(input) => {
                let content = FileText::content_of(text, input.appended_root_module(&*db));
                set(input.set_text_field(db), durability, Arc::from(text));
                if input.content(&*db) != content {
                    set(input.set_content_field(db), durability, InputId(content));
                }
                self.files.insert(file_id, input);
                content
            }
            None => {
                let content = FileText::content_of(text, None);
                let builder = FileText::builder(Arc::from(text), file_id, InputId(content));
                let builder = match durability {
                    Some(durability) => builder.durability(durability),
                    None => builder,
                };
                self.files.insert(file_id, builder.new(&*db));
                content
            }
        }
    }

    /// The content ID `file` has in a source root.
    fn entry_content(&self, db: &dyn SourceDatabase, file: vfs::FileId) -> ContentId {
        match self.files.get(&file).map(|it| *it) {
            Some(input) => input.content(db),
            None => FileText::no_text(),
        }
    }

    /// Puts `content` as `file`'s in each source root that has it, adding to `changed` those
    /// whose content ID it changes. Their inputs are written by [`Files::write_root_contents`].
    fn index_file(&self, file: vfs::FileId, content: ContentId, changed: &mut Vec<SourceRootId>) {
        let Some(roots) = self.in_roots.get(&file).map(|it| it.clone()) else { return };
        for root in roots {
            let Some(mut index) = self.root_contents.get_mut(&root) else { continue };
            if index.set(file, content) && !changed.contains(&root) {
                changed.push(root);
            }
        }
    }

    fn write_root_contents(&self, db: &mut dyn SourceDatabase, roots: &[SourceRootId]) {
        for &root in roots {
            let Some(index) = self.root_contents.get(&root) else { continue };
            let (content, is_library) = (InputId(index.content()), index.is_library);
            drop(index);
            let Some(input) = self.source_roots.get(&root).map(|it| *it) else { continue };
            if input.content_field(&*db) != content {
                input
                    .set_content_field(db)
                    .with_durability(root_content_durability(is_library))
                    .to(content);
            }
        }
    }

    /// The content of the source root `id` as `root` has it, noting which of its files and
    /// paths it has.
    fn index_root(&self, db: &dyn SourceDatabase, id: SourceRootId, root: &SourceRoot) -> RootContent {
        if let Some(old) = self.root_contents.get(&id) {
            for file in old.keys.keys() {
                if let Some(mut roots) = self.in_roots.get_mut(file) {
                    roots.retain(|it| *it != id);
                }
            }
        }
        let mut files = Vec::new();
        for file in root.iter() {
            let Some(path) = root.path_for_file(&file) else { continue };
            // A path two files were given is the last one's.
            if root.file_for_path(path) != Some(&file) {
                continue;
            }
            self.by_path.insert(path.clone(), file);
            let mut roots = self.in_roots.entry(file).or_default();
            if !roots.contains(&id) {
                roots.push(id);
            }
            drop(roots);
            files.push((file, path, self.entry_content(db, file)));
        }
        RootContent::new(root.is_library, files)
    }

    /// Has the files under `base` read from where they are under `over` from now on, but for
    /// those under one of `built`: what a build writes is not part of a checkout. Nothing is
    /// read again for it: whoever watches the disk does that.
    pub fn set_view(
        &self,
        base: &std::path::Path,
        over: &std::path::Path,
        built: Vec<std::path::PathBuf>,
    ) {
        let mut views = self.views.write().unwrap_or_else(|it| it.into_inner());
        views.retain(|it| it.base != base);
        if base != over {
            let (base, over) = (base.to_owned(), over.to_owned());
            views.push(View { base, over, built });
        }
    }

    /// Where the file known as `path` is read from.
    pub fn on_disk(&self, path: &std::path::Path) -> std::path::PathBuf {
        let views = self.views.read().unwrap_or_else(|it| it.into_inner());
        let over = views
            .iter()
            .filter(|it| !it.built.iter().any(|built| path.starts_with(built)))
            .find_map(|it| Some(it.over.join(path.strip_prefix(&it.base).ok()?)));
        over.unwrap_or_else(|| path.to_owned())
    }

    /// A file outside the VFS that `include_bytes!` or `include_str!` names, read from disk the
    /// first time it is asked for. Whoever watches the disk keeps it current with
    /// [`Files::reload_included_file`].
    pub fn included_file(
        &self,
        db: &dyn SourceDatabase,
        krate: Crate,
        path: &AbsPath,
        durability: Durability,
    ) -> IncludedFile {
        if !self.included_by.contains_key(&(krate, path.to_path_buf())) {
            self.included_by.insert((krate, path.to_path_buf()), ());
        }
        *self.included.entry(path.to_path_buf()).or_insert_with(|| {
            let bytes = self.read_included(path);
            let content = InputId(IncludedFile::content_of(bytes.as_deref()));
            IncludedFile::builder(bytes, content).durability(durability).new(db)
        })
    }

    /// The files outside the VFS that `krate` has read, with the bytes it read of each:
    /// `None` for one that could not be read.
    pub fn included_by(
        &self,
        db: &dyn SourceDatabase,
        krate: Crate,
    ) -> Vec<(AbsPathBuf, Option<std::sync::Arc<[u8]>>)> {
        let paths = self.included_by.iter().filter(|it| it.key().0 == krate);
        let paths: Vec<AbsPathBuf> = paths.map(|it| it.key().1.clone()).collect();
        let read = |path: AbsPathBuf| {
            let file = self.included.get(&path).map(|it| *it)?;
            Some((path, file.bytes(db).clone()))
        };
        paths.into_iter().filter_map(read).collect()
    }

    fn read_included(&self, path: &AbsPath) -> Option<std::sync::Arc<[u8]>> {
        std::fs::read(self.on_disk(path.as_ref())).ok().map(Into::into)
    }

    /// Notes a file an `include!` or a module declaration names that the VFS has not: whoever
    /// owns the VFS puts it in the source root, see [`Files::included_sources`].
    pub fn include_source(&self, path: &AbsPath, root: SourceRootId, krate: Crate) {
        self.included_sources.entry(path.to_path_buf()).or_insert(root);
        if !self.sources_included_by.contains_key(&(krate, path.to_path_buf())) {
            self.sources_included_by.insert((krate, path.to_path_buf()), ());
        }
    }

    /// The crates that named each file of [`Files::included_sources`].
    pub fn sources_included_by(&self) -> Vec<(Crate, AbsPathBuf)> {
        self.sources_included_by.iter().map(|it| it.key().clone()).collect()
    }

    pub fn included_sources(&self) -> Vec<(AbsPathBuf, SourceRootId)> {
        self.included_sources.iter().map(|it| (it.key().clone(), *it.value())).collect()
    }

    pub fn env_is_read(&self, krate: Crate, key: &str) {
        if !self.env_read.contains_key(&(krate, key.to_owned())) {
            self.env_read.insert((krate, key.to_owned()), ());
        }
    }

    pub fn env_read(&self) -> Vec<(Crate, String)> {
        self.env_read.iter().map(|it| it.key().clone()).collect()
    }

    pub fn env_is_unset(&self, krate: Crate, key: &str) {
        self.env_unset.insert((krate, key.to_owned()), ());
    }

    pub fn env_unset(&self) -> Vec<(Crate, String)> {
        self.env_unset.iter().map(|it| it.key().clone()).collect()
    }

    pub fn included_files(&self) -> Vec<AbsPathBuf> {
        self.included.iter().map(|it| it.key().clone()).collect()
    }

    /// Whether the file on disk is what was read of it.
    pub fn included_is_read(&self, db: &dyn SourceDatabase, path: &AbsPath) -> bool {
        let Some(file) = self.included.get(path).map(|it| *it) else { return false };
        *file.bytes(db) == self.read_included(path)
    }

    /// Reads the file again, and says whether it changed.
    pub fn reload_included_file(&self, db: &mut dyn SourceDatabase, path: &AbsPath) -> bool {
        let Some(file) = self.included.get(path).map(|it| *it) else { return false };
        let bytes = self.read_included(path);
        let changed = *file.bytes(db) != bytes;
        if changed {
            let content = InputId(IncludedFile::content_of(bytes.as_deref()));
            file.set_bytes_field(db).to(bytes);
            file.set_content_field(db).to(content);
        }
        changed
    }

    /// Source root of the file.
    pub fn source_root(&self, source_root_id: SourceRootId) -> SourceRootInput {
        let source_root = match self.source_roots.get(&source_root_id) {
            Some(source_root) => source_root,
            None => panic!(
                "Unable to fetch `SourceRootInput` with `SourceRootId` ({source_root_id:?}); this is a bug"
            ),
        };

        *source_root
    }

    pub fn set_source_root_with_durability(
        &self,
        db: &mut dyn SourceDatabase,
        source_root_id: SourceRootId,
        source_root: Arc<SourceRoot>,
        durability: Durability,
    ) {
        let index = self.index_root(&*db, source_root_id, &source_root);
        let content = InputId(index.content());
        let content_durability = root_content_durability(source_root.is_library);
        self.root_contents.insert(source_root_id, index);
        match self.source_roots.entry(source_root_id) {
            Entry::Occupied(occupied) => {
                let input = *occupied.get();
                input.set_source_root_field(db).with_durability(durability).to(source_root);
                if input.content_field(&*db) != content {
                    input.set_content_field(db).with_durability(content_durability).to(content);
                }
            }
            Entry::Vacant(vacant) => {
                let input = SourceRootInput::builder(source_root, content)
                    .durability(durability)
                    .content_durability(content_durability)
                    .new(&*db);
                vacant.insert(input);
            }
        };
    }

    pub fn file_source_root(
        &self,
        db: &dyn SourceDatabase,
        id: vfs::FileId,
    ) -> FileSourceRootInput {
        let file_source_root = match self.file_source_roots.get(&id) {
            Some(file_source_root) => file_source_root,
            None => panic!(
                "Unable to get `FileSourceRootInput` with `vfs::FileId` ({id:?}, path: {}); this is a bug",
                self.path_for_file(db, id)
                    .map_or_else(|| "<unknown>".to_owned(), |path| path.to_string()),
            ),
        };
        *file_source_root
    }

    fn path_for_file(&self, db: &dyn SourceDatabase, id: vfs::FileId) -> Option<vfs::VfsPath> {
        for source_root in &*self.source_roots {
            let source_root = *source_root.value();
            if let Some(path) = source_root.source_root(db).path_for_file(&id) {
                return Some(path.clone());
            }
        }
        None
    }

    pub fn set_file_source_root_with_durability(
        &self,
        db: &mut dyn SourceDatabase,
        id: vfs::FileId,
        source_root_id: SourceRootId,
        durability: Durability,
    ) {
        match self.file_source_roots.entry(id) {
            Entry::Occupied(mut occupied) => {
                occupied
                    .get_mut()
                    .set_source_root_id(db)
                    .with_durability(durability)
                    .to(source_root_id);
            }
            Entry::Vacant(vacant) => {
                let file_source_root =
                    FileSourceRootInput::builder(source_root_id).durability(durability).new(db);
                vacant.insert(file_source_root);
            }
        };
    }
}

/// The files of one source root as its content ID is made of them (see [`SourceRoot::DOMAIN`]).
#[derive(Debug)]
struct RootContent {
    is_library: bool,
    /// Each file's bucket and key (its path, encoded).
    keys: FxHashMap<vfs::FileId, (usize, Box<[u8]>)>,
    buckets: Vec<BTreeMap<Box<[u8]>, ContentId>>,
    hashes: Vec<ContentId>,
}

impl RootContent {
    fn new<'a>(
        is_library: bool,
        files: impl IntoIterator<Item = (vfs::FileId, &'a VfsPath, ContentId)>,
    ) -> RootContent {
        let mut buckets = vec![BTreeMap::new(); SourceRoot::BUCKETS];
        let mut keys = FxHashMap::default();
        for (file, path, content) in files {
            let key: Box<[u8]> = content::path_key(path).into();
            let bucket = RootContent::bucket(&key);
            buckets[bucket].insert(key.clone(), content);
            keys.insert(file, (bucket, key));
        }
        let hashes = buckets.iter().map(RootContent::bucket_hash).collect();
        RootContent { is_library, keys, buckets, hashes }
    }

    fn bucket(key: &[u8]) -> usize {
        ContentId::of(SourceRoot::PATH_DOMAIN, key).0[0] as usize % SourceRoot::BUCKETS
    }

    fn bucket_hash(bucket: &BTreeMap<Box<[u8]>, ContentId>) -> ContentId {
        let mut hasher = Hasher::new(SourceRoot::BUCKET_DOMAIN);
        content::len(&mut hasher, bucket.len());
        for (key, content) in bucket {
            hasher.part(key).id(content);
        }
        hasher.finish()
    }

    /// Sets the content ID of `file`, if the root has it; whether the root's changed.
    fn set(&mut self, file: vfs::FileId, content: ContentId) -> bool {
        let Some((bucket, key)) = self.keys.get(&file) else { return false };
        let bucket = *bucket;
        let Some(slot) = self.buckets[bucket].get_mut(key) else { return false };
        if *slot == content {
            return false;
        }
        *slot = content;
        self.hashes[bucket] = RootContent::bucket_hash(&self.buckets[bucket]);
        true
    }

    fn content(&self) -> ContentId {
        let mut hasher = Hasher::new(SourceRoot::DOMAIN);
        hasher.update(&[self.is_library as u8]);
        for hash in &self.hashes {
            hasher.id(hash);
        }
        hasher.finish()
    }
}

/// A source root's content ID changes with its files' texts, so it is as durable as they are.
fn root_content_durability(is_library: bool) -> Durability {
    if is_library { Durability::HIGH } else { Durability::LOW }
}

fn set<S: Setter>(setter: S, durability: Option<Durability>, value: S::FieldTy) {
    let setter = match durability {
        Some(durability) => setter.with_durability(durability),
        None => setter,
    };
    setter.to(value);
}

/// The set of roots for crates.io libraries.
/// Files in libraries are assumed to never change.
#[salsa::input(singleton, debug)]
pub struct LibraryRoots {
    #[returns(ref)]
    pub roots: FxHashSet<SourceRootId>,
}

/// The set of "local" (that is, from the current workspace) roots.
/// Files in local roots are assumed to change frequently.
#[salsa::input(singleton, debug)]
pub struct LocalRoots {
    #[returns(ref)]
    pub roots: FxHashSet<SourceRootId>,
}

/// The variables of the process a build is for, which `env!` and `option_env!` read where
/// the crate's own have nothing: those of whoever asked for the build.
/// Its content ID is [`Env::content`] of its variables.
#[salsa::input(singleton, debug)]
pub struct ProcessEnv {
    #[returns(ref)]
    #[get(vars_field)]
    #[set(set_vars_field)]
    vars: Env,
    #[returns(copy)]
    #[get(content_field)]
    #[set(set_content_field)]
    content: InputId,
}

impl ProcessEnv {
    pub fn vars<'db, Db: ?Sized + salsa::Database>(self, db: &'db Db) -> &'db Env {
        self.vars_field(db)
    }

    pub fn content<Db: ?Sized + salsa::Database>(self, db: &Db) -> ContentId {
        self.content_field(db).id()
    }
}

/// The variable `key` of the process the build is for.
pub fn process_env_var(db: &dyn salsa::Database, key: &str) -> Option<String> {
    ProcessEnv::try_get(db)?.vars(db).get(key)
}

/// Has builds from here on be for a process with the variables `vars`.
pub fn set_process_env(db: &mut dyn salsa::Database, vars: Env) {
    let content = InputId(vars.content());
    match ProcessEnv::try_get(&*db) {
        Some(env) => {
            env.set_vars_field(db).to(vars);
            if env.content_field(&*db) != content {
                env.set_content_field(db).to(content);
            }
        }
        None => {
            ProcessEnv::new(&*db, vars, content);
        }
    }
}

/// A file's text. Set only through [`Files`] (and the [`SourceDatabase`] methods that call it),
/// which keeps [`FileText::content`] the content ID of the text and its appended module.
#[salsa::input(debug)]
pub struct FileText {
    #[returns(ref)]
    #[get(text_field)]
    #[set(set_text_field)]
    text: Arc<str>,
    pub file_id: vfs::FileId,
    /// The module a crate root's text ends with that the compiler wrote, not the file on disk:
    /// blaze's test harness or list of proc macros. rustc injects its own after it applies
    /// the crate's `#![cfg]`, so a root that `#![cfg]` takes away still has it.
    #[default]
    #[returns(copy)]
    #[get(appended_root_module_field)]
    #[set(set_appended_root_module_field)]
    appended_root_module: Option<&'static str>,
    #[returns(copy)]
    #[get(content_field)]
    #[set(set_content_field)]
    content: InputId,
}

impl FileText {
    /// A file text's content ID is of its text, then of its appended root module (0, or 1 and
    /// the module), which changes what the crate root is. The file's ID and path are not in it.
    pub const DOMAIN: &'static str = "blaze v2 file text 1";
    /// The content ID a file of a source root has when it has no text (bytes that are not
    /// UTF-8, or none loaded): what the front end sees of it is that it has none.
    pub const NO_TEXT_DOMAIN: &'static str = "blaze v2 file without text 1";

    pub fn content_of(text: &str, appended_root_module: Option<&str>) -> ContentId {
        let mut hasher = Hasher::new(FileText::DOMAIN);
        hasher.part(text.as_bytes());
        content::opt(&mut hasher, appended_root_module);
        hasher.finish()
    }

    pub fn no_text() -> ContentId {
        ContentId::of(FileText::NO_TEXT_DOMAIN, b"")
    }

    pub fn text<'db, Db: ?Sized + salsa::Database>(self, db: &'db Db) -> &'db Arc<str> {
        self.text_field(db)
    }

    /// See the field: the module the compiler appends to a crate root.
    pub fn appended_root_module<Db: ?Sized + salsa::Database>(
        self,
        db: &Db,
    ) -> Option<&'static str> {
        self.appended_root_module_field(db)
    }

    pub fn content<Db: ?Sized + salsa::Database>(self, db: &Db) -> ContentId {
        self.content_field(db).id()
    }
}

/// The contents of a file named by `include_bytes!` or `include_str!`, `None` if it can't be read.
#[salsa::input(debug)]
pub struct IncludedFile {
    #[returns(ref)]
    #[get(bytes_field)]
    #[set(set_bytes_field)]
    bytes: Option<std::sync::Arc<[u8]>>,
    #[returns(copy)]
    #[get(content_field)]
    #[set(set_content_field)]
    content: InputId,
}

impl IncludedFile {
    /// Of its bytes (1 and the bytes), or that it could not be read (0).
    pub const DOMAIN: &'static str = "blaze v2 included file 1";

    pub fn content_of(bytes: Option<&[u8]>) -> ContentId {
        let mut hasher = Hasher::new(IncludedFile::DOMAIN);
        match bytes {
            None => hasher.update(&[0]),
            Some(bytes) => hasher.update(&[1]).part(bytes),
        };
        hasher.finish()
    }

    pub fn bytes<'db, Db: ?Sized + salsa::Database>(
        self,
        db: &'db Db,
    ) -> &'db Option<std::sync::Arc<[u8]>> {
        self.bytes_field(db)
    }

    pub fn content<Db: ?Sized + salsa::Database>(self, db: &Db) -> ContentId {
        self.content_field(db).id()
    }
}

#[salsa::input(debug)]
pub struct FileSourceRootInput {
    #[returns(copy)]
    pub source_root_id: SourceRootId,
}

/// A source root. Its content ID is of its files' paths and content IDs and `is_library`
/// ([`SourceRoot::DOMAIN`]), kept by [`Files`] as either changes.
#[salsa::input(debug)]
pub struct SourceRootInput {
    #[returns(clone)]
    #[get(source_root_field)]
    #[set(set_source_root_field)]
    source_root: Arc<SourceRoot>,
    #[returns(copy)]
    #[get(content_field)]
    #[set(set_content_field)]
    content: InputId,
}

impl SourceRootInput {
    pub fn source_root<Db: ?Sized + salsa::Database>(self, db: &Db) -> Arc<SourceRoot> {
        self.source_root_field(db)
    }

    pub fn content<Db: ?Sized + salsa::Database>(self, db: &Db) -> ContentId {
        self.content_field(db).id()
    }
}

#[salsa::db]
pub trait SourceDatabase: salsa::Database + std::fmt::Debug {
    /// Text of the file.
    fn file_text(&self, file_id: vfs::FileId) -> FileText;

    fn set_file_text(&mut self, file_id: vfs::FileId, text: &str);

    /// Whether [`SourceDatabase::file_text`] has the file: one that is not UTF-8 has no text.
    fn has_file_text(&self, _file_id: vfs::FileId) -> bool {
        true
    }

    fn set_file_text_with_durability(
        &mut self,
        file_id: vfs::FileId,
        text: &str,
        durability: Durability,
    );

    /// Contents of the source root.
    fn source_root(&self, id: SourceRootId) -> SourceRootInput;

    fn file_source_root(&self, id: vfs::FileId) -> FileSourceRootInput;

    fn set_file_source_root_with_durability(
        &mut self,
        id: vfs::FileId,
        source_root_id: SourceRootId,
        durability: Durability,
    );

    /// Source root of the file.
    fn set_source_root_with_durability(
        &mut self,
        source_root_id: SourceRootId,
        source_root: Arc<SourceRoot>,
        durability: Durability,
    );

    /// See [`Files::set_appended_root_module`].
    fn set_appended_root_module(
        &mut self,
        file_id: vfs::FileId,
        module: Option<&'static str>,
        durability: Durability,
    );

    /// Sets the inputs of one view's files, writing only those that changed: see
    /// [`Files::set_view_root`].
    fn set_view_root(&mut self, view: ViewRoot<'_>) -> Result<ViewChange, ViewError>;

    fn resolve_path(&self, path: AnchoredPath<'_>) -> Option<FileId> {
        // FIXME: this *somehow* should be platform agnostic...
        let source_root = self.file_source_root(path.anchor);
        let source_root = self.source_root(source_root.source_root_id(self));
        source_root.source_root(self).resolve_path(path)
    }

    /// The file `path` names relative to `anchor`, when it is not one of the files of the VFS,
    /// as `krate` reads it.
    fn included_file(&self, _krate: Crate, _path: AnchoredPath<'_>) -> Option<IncludedFile> {
        None
    }

    /// Notes that `include!` or a module declaration names a file `resolve_path` has nothing
    /// for: a file that is not source by its extension, or one outside its package, which the
    /// VFS does not load until it is asked to. `krate` is the crate whose expansion or module
    /// tree names it.
    fn include_source(&self, _path: AnchoredPath<'_>, _krate: Crate) {}

    /// Notes that `env!` or `option_env!` asked for the variable `key` in `krate`.
    fn env_is_read(&self, _krate: Crate, _key: &str) {}

    /// Notes that `env!` in `krate` found the variable `key` unset, which rustc turns the
    /// crate down for.
    fn env_is_unset(&self, _krate: Crate, _key: &str) {}

    #[doc(hidden)]
    fn crates_map(&self) -> Arc<CratesMap>;

    /// A handle on the same database for another thread: the same storage and inputs, its own
    /// query stack. Results either computes are seen by both. A write to an input waits until
    /// every fork is dropped, so a fork lives only as long as one parallel read.
    fn fork(&self) -> Box<dyn SourceDatabase + Send>;

    fn nonce_and_revision(&self) -> (Nonce, salsa::Revision);

    fn line_column(&self, file: FileId, offset: TextSize) -> Result<(u32, u32), ()>;
}

static NEXT_NONCE: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Nonce(usize);

impl Default for Nonce {
    #[inline]
    fn default() -> Self {
        Nonce::new()
    }
}

impl Nonce {
    #[inline]
    pub const fn invalid() -> Nonce {
        Nonce(usize::MAX)
    }

    #[inline]
    pub fn new() -> Nonce {
        Nonce(NEXT_NONCE.fetch_add(1, std::sync::atomic::Ordering::SeqCst))
    }
}

/// Crate related data shared by the whole workspace.
#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct CrateWorkspaceData {
    pub target: Result<target::TargetData, target::TargetLoadError>,
    /// Toolchain version used to compile the crate.
    pub toolchain: Option<Version>,
    /// How far the evaluation of one constant of the workspace may go.
    pub const_eval: ConstEvalBudget,
}

/// What one evaluation of a constant, static or other compile-time code may spend before it
/// stops with an error: `const-eval` under `[workspace.metadata.blaze]` in the root manifest.
///
/// ```toml
/// [workspace.metadata.blaze]
/// const-eval = { steps = 4_000_000_000, memory = 2_147_483_648 }
/// ```
///
/// rustc's evaluation has no limit of its own past its `long_running_const_eval` lint (at 2
/// million steps, allowed away by `#[allow]`), so the default is far beyond what code rustc
/// takes without that `#[allow]` ever spends: it is there to end a loop that never would.
#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
pub struct ConstEvalBudget {
    /// Basic blocks run, as rustc counts the terminators it evaluates.
    pub steps: u64,
    /// Bytes of the evaluation's stack and heap together.
    pub memory: u64,
}

impl ConstEvalBudget {
    /// The key of the table under `[workspace.metadata.blaze]`.
    pub const KEY: &str = "const-eval";
}

impl Default for ConstEvalBudget {
    fn default() -> Self {
        ConstEvalBudget { steps: 1_000_000_000, memory: 1 << 30 }
    }
}

impl CrateWorkspaceData {
    /// Of the target ([`target::TargetData::content`], or 1 and why it could not be loaded),
    /// the toolchain's version, and the const-eval budget.
    pub const DOMAIN: &'static str = "blaze v2 crate workspace data 1";

    pub fn content(&self) -> ContentId {
        let mut hasher = Hasher::new(CrateWorkspaceData::DOMAIN);
        match &self.target {
            Ok(target) => hasher.update(&[0]).id(&target.content()),
            Err(error) => hasher.update(&[1]).part(error.to_string().as_bytes()),
        };
        content::opt(&mut hasher, self.toolchain.as_ref().map(Version::to_string).as_deref());
        hasher
            .update(&self.const_eval.steps.to_le_bytes())
            .update(&self.const_eval.memory.to_le_bytes())
            .finish()
    }

    pub fn is_atleast_187(&self) -> bool {
        const VERSION_187: Version = Version {
            major: 1,
            minor: 87,
            patch: 0,
            pre: Prerelease::EMPTY,
            build: BuildMetadata::EMPTY,
        };
        self.toolchain.as_ref().map_or(false, |v| *v >= VERSION_187)
    }
}

pub fn toolchain_channel(db: &dyn salsa::Database, krate: Crate) -> Option<ReleaseChannel> {
    krate.workspace_data(db).toolchain.as_ref().and_then(|v| ReleaseChannel::from_str(&v.pre).ok())
}

#[salsa::input(singleton, debug)]
struct AllCrates {
    #[returns(clone)]
    crates: std::sync::Arc<[Crate]>,
    #[returns(copy)]
    content: InputId,
}

/// The crate graph's content ID is of its crates' content IDs, in topological order.
pub const CRATE_GRAPH_DOMAIN: &str = "blaze v2 crate graph 1";

fn crate_graph_content(db: &dyn salsa::Database, crates: &[Crate]) -> ContentId {
    let mut hasher = Hasher::new(CRATE_GRAPH_DOMAIN);
    content::len(&mut hasher, crates.len());
    for krate in crates {
        hasher.id(&krate.content(db));
    }
    hasher.finish()
}

/// The content ID of [`all_crates`].
pub fn all_crates_content(db: &dyn salsa::Database) -> ContentId {
    match AllCrates::try_get(db) {
        Some(all) => all.content(db).id(),
        None => crate_graph_content(db, &[]),
    }
}

/// Sets the crate graph's content ID from its crates' as they are.
pub(crate) fn refresh_crate_graph_content(db: &mut dyn salsa::Database) {
    let Some(all) = AllCrates::try_get(&*db) else { return };
    let content = InputId(crate_graph_content(&*db, &all.crates(&*db)));
    if all.content(&*db) != content {
        all.set_content(db).to(content);
    }
}

pub fn set_all_crates_with_durability(
    db: &mut dyn salsa::Database,
    crates: impl IntoIterator<Item = Crate>,
    durability: Durability,
) {
    let crates: std::sync::Arc<[Crate]> = crates.into_iter().collect();
    let content = InputId(crate_graph_content(&*db, &crates));
    match AllCrates::try_get(&*db) {
        Some(all) => {
            all.set_crates(db).with_durability(durability).to(crates);
            all.set_content(db).with_durability(durability).to(content);
        }
        None => {
            _ = AllCrates::builder(crates, content).durability(durability).new(&*db);
        }
    }
}

/// Returns the crates in topological order.
///
/// **Warning**: do not use this query in `hir-*` crates! It kills incrementality across crate metadata modifications.
pub fn all_crates(db: &dyn salsa::Database) -> std::sync::Arc<[Crate]> {
    AllCrates::try_get(db).map_or(std::sync::Arc::default(), |all_crates| all_crates.crates(db))
}

// FIXME: VFS rewrite should allow us to get rid of this wrapper
#[doc(hidden)]
#[salsa::interned]
pub struct InternedSourceRootId {
    #[returns(copy)]
    pub id: SourceRootId,
}

/// Crates whose root file is in `id`.
pub fn source_root_crates(db: &dyn SourceDatabase, id: SourceRootId) -> &[Crate] {
    #[salsa::tracked(returns(deref))]
    pub fn source_root_crates<'db>(
        db: &'db dyn SourceDatabase,
        id: InternedSourceRootId<'db>,
    ) -> Box<[Crate]> {
        let crates = AllCrates::get(db).crates(db);
        let id = id.id(db);
        crates
            .iter()
            .copied()
            .filter(|&krate| {
                let root_file = krate.data(db).root_file_id;
                db.file_source_root(root_file).source_root_id(db) == id
            })
            .collect()
    }
    source_root_crates(db, InternedSourceRootId::new(db, id))
}

pub fn relevant_crates(db: &dyn SourceDatabase, file_id: FileId) -> &[Crate] {
    let _p = tracing::info_span!("relevant_crates").entered();

    let source_root = db.file_source_root(file_id);
    source_root_crates(db, source_root.source_root_id(db))
}

#[must_use]
#[non_exhaustive]
pub struct DbPanicContext;

impl Drop for DbPanicContext {
    fn drop(&mut self) {
        Self::with_ctx(|ctx| assert!(ctx.pop().is_some()));
    }
}

impl DbPanicContext {
    pub fn enter(frame: String) -> DbPanicContext {
        #[expect(clippy::print_stderr, reason = "already panicking anyway")]
        fn set_hook() {
            let default_hook = panic::take_hook();
            panic::set_hook(Box::new(move |panic_info| {
                default_hook(panic_info);
                if let Some(backtrace) = salsa::Backtrace::capture() {
                    eprintln!("{backtrace:#}");
                }
                DbPanicContext::with_ctx(|ctx| {
                    if !ctx.is_empty() {
                        eprintln!("additional context:");
                        for (idx, frame) in ctx.iter().enumerate() {
                            eprintln!("{idx:>4}: {frame}\n");
                        }
                    }
                });
            }));
        }

        static SET_HOOK: Once = Once::new();
        SET_HOOK.call_once(set_hook);

        Self::with_ctx(|ctx| ctx.push(frame));
        DbPanicContext
    }

    fn with_ctx(f: impl FnOnce(&mut Vec<String>)) {
        thread_local! {
            static CTX: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
        }
        CTX.with(|ctx| f(&mut ctx.borrow_mut()));
    }
}

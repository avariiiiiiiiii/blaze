//! The front end's database of one lowered build: base-db's inputs over files held in memory,
//! with every query of `hir` (`hir::impl_hir_database!`). Nothing it does reads the disk: a
//! file is in a source root or it is not there.

use std::{fmt, panic};

use base_db::{
    AnchoredPath, CratesMap, FileSourceRootInput, FileText, Files, Nonce, SourceDatabase,
    SourceRoot, SourceRootId, SourceRootInput, ViewChange, ViewError, ViewRoot,
};
use salsa::Durability;
use span::{FileId, TextSize};
use triomphe::Arc;

#[salsa::db]
pub struct Db {
    storage: salsa::Storage<Db>,
    files: Arc<Files>,
    crates_map: Arc<CratesMap>,
    nonce: Nonce,
}

impl Db {
    /// A database with no crate, no source root and proc-macro attributes expanded as rustc
    /// expands them.
    pub(crate) fn new() -> Db {
        let mut db = Db {
            storage: salsa::Storage::new(None),
            files: Default::default(),
            crates_map: Default::default(),
            nonce: Nonce::new(),
        };
        base_db::set_all_crates_with_durability(&mut db, std::iter::empty(), Durability::HIGH);
        hir::ProcMacros::init_default(&db, Durability::HIGH);
        hir::db::set_expand_proc_attr_macros(&mut db, true);
        db
    }
}

impl Clone for Db {
    fn clone(&self) -> Db {
        Db {
            storage: self.storage.clone(),
            files: self.files.clone(),
            crates_map: self.crates_map.clone(),
            nonce: self.nonce,
        }
    }
}

impl fmt::Debug for Db {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("load_cargo::v2::Db").finish()
    }
}

#[salsa::db]
impl salsa::Database for Db {}

impl panic::RefUnwindSafe for Db {}

#[salsa::db]
impl SourceDatabase for Db {
    fn file_text(&self, file_id: FileId) -> FileText {
        self.files.file_text(file_id)
    }

    fn has_file_text(&self, file_id: FileId) -> bool {
        self.files.has_file_text(file_id)
    }

    fn set_file_text(&mut self, file_id: FileId, text: &str) {
        let files = Arc::clone(&self.files);
        files.set_file_text(self, file_id, text);
    }

    fn set_file_text_with_durability(&mut self, file_id: FileId, text: &str, durability: Durability) {
        let files = Arc::clone(&self.files);
        files.set_file_text_with_durability(self, file_id, text, durability);
    }

    fn source_root(&self, id: SourceRootId) -> SourceRootInput {
        self.files.source_root(id)
    }

    fn file_source_root(&self, id: FileId) -> FileSourceRootInput {
        self.files.file_source_root(self, id)
    }

    fn set_file_source_root_with_durability(
        &mut self,
        id: FileId,
        source_root_id: SourceRootId,
        durability: Durability,
    ) {
        let files = Arc::clone(&self.files);
        files.set_file_source_root_with_durability(self, id, source_root_id, durability);
    }

    fn set_source_root_with_durability(
        &mut self,
        source_root_id: SourceRootId,
        source_root: Arc<SourceRoot>,
        durability: Durability,
    ) {
        let files = Arc::clone(&self.files);
        files.set_source_root_with_durability(self, source_root_id, source_root, durability);
    }

    fn set_appended_root_module(
        &mut self,
        file_id: FileId,
        module: Option<&'static str>,
        durability: Durability,
    ) {
        let files = Arc::clone(&self.files);
        files.set_appended_root_module(self, file_id, module, durability);
    }

    fn set_view_root(&mut self, view: ViewRoot<'_>) -> Result<ViewChange, ViewError> {
        let files = Arc::clone(&self.files);
        files.set_view_root(self, view)
    }

    fn include_source(&self, path: AnchoredPath<'_>, krate: base_db::Crate) {
        let root_id = self.file_source_root(path.anchor).source_root_id(self);
        let root = self.source_root(root_id).source_root(self);
        if let Some(anchor) = root.path_for_file(&path.anchor).and_then(|it| it.as_path())
            && let Some(parent) = anchor.parent()
        {
            self.files.include_source(&parent.absolutize(path.path), root_id, krate);
        }
    }

    fn env_is_read(&self, krate: base_db::Crate, key: &str) {
        self.files.env_is_read(krate, key);
    }

    fn env_is_unset(&self, krate: base_db::Crate, key: &str) {
        self.files.env_is_unset(krate, key);
    }

    fn crates_map(&self) -> Arc<CratesMap> {
        self.crates_map.clone()
    }

    fn fork(&self) -> Box<dyn SourceDatabase + Send> {
        Box::new(self.clone())
    }

    fn nonce_and_revision(&self) -> (Nonce, salsa::Revision) {
        (self.nonce, salsa::plumbing::ZalsaDatabase::zalsa(self).current_revision())
    }

    fn line_column(&self, file: FileId, offset: TextSize) -> Result<(u32, u32), ()> {
        let at = line_index(self, file).try_line_col(offset).ok_or(())?;
        Ok((at.line, at.col))
    }
}

hir::impl_hir_database!(Db);

fn line_index(db: &dyn SourceDatabase, file: FileId) -> &Arc<line_index::LineIndex> {
    #[salsa::interned]
    pub struct LinedFile {
        #[returns(copy)]
        id: FileId,
    }

    #[salsa::tracked(returns(ref))]
    fn line_index<'db>(db: &'db dyn SourceDatabase, file: LinedFile<'db>) -> Arc<line_index::LineIndex> {
        Arc::new(line_index::LineIndex::new(db.file_text(file.id(db)).text(db)))
    }

    line_index(db, LinedFile::new(db, file))
}

/// A handle on a [`crate::v2::Lowered`]'s database for one worker: the same storage and
/// inputs, its own query stack, read while other workers read theirs. It is `Send`, not
/// `Sync`: one thread reads it at a time.
///
/// While it is alive, a write to the database ([`crate::v2::Lowered::ran`],
/// [`crate::v2::Lowered::macros`]) waits for it: salsa cancels it, so the query it is in (or
/// its next one) unwinds with [`salsa::Cancelled`], which the worker catches
/// (`salsa::Cancelled::catch`), and the write goes on once the snapshot is dropped. A
/// worker holds one snapshot at a time and never asks for another while it holds one.
pub struct Snapshot {
    db: Db,
}

impl Snapshot {
    pub(crate) fn new(db: Db) -> Snapshot {
        Snapshot { db }
    }

    /// As `hir_ty_codegen::v2::Databases::new` takes one.
    pub fn into_boxed(self) -> Box<dyn hir::db::HirDatabase + Send> {
        Box::new(self.db)
    }
}

impl std::ops::Deref for Snapshot {
    type Target = dyn hir::db::HirDatabase;

    fn deref(&self) -> &Self::Target {
        &self.db
    }
}

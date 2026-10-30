//! The front end's database of one lowered build: base-db's inputs over files held in memory,
//! with every query of `hir` (`hir::impl_hir_database!`). Nothing it does reads the disk: a
//! file is in a source root or it is not there.

use std::{fmt, panic};

use base_db::{
    CratesMap, FileSourceRootInput, FileText, Files, Nonce, SourceDatabase, SourceRoot,
    SourceRootId, SourceRootInput, ViewChange, ViewError, ViewRoot,
};
use salsa::Durability;
use span::{FileId, TextSize};
use triomphe::Arc;

/// The database of a [`crate::v2::Lowered`], `Sync` and shared by reference.
///
/// There is one per lowered build and no handle on it: every task reads it through `&Db`
/// ([`crate::v2::Lowered::db`]), each with its own query stack (salsa's per-task state), and
/// nothing clones it. A write ([`crate::v2::Lowered::ran`], [`crate::v2::Lowered::macros`])
/// takes `&mut`, so it happens only once no reader holds `&Db`; salsa cancels no reader.
#[salsa::db]
pub struct Db {
    storage: salsa::Storage<Db>,
    files: Arc<Files>,
    crates_map: Arc<CratesMap>,
    nonce: Nonce,
}

impl Db {
    /// A database with no crate, no source root and proc-macro attributes expanded as rustc
    /// expands them. Installs salsa's task-depth hook ([`base_db::install_plane_hooks`]): its
    /// queries run as tasks of the process's pool.
    pub(crate) fn new() -> Db {
        base_db::install_plane_hooks();
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

    fn crates_map(&self) -> Arc<CratesMap> {
        self.crates_map.clone()
    }

    /// Each task is a job of the process's worker pool (`blaze_graph::exec::shared`, the
    /// daemon's one pool) reading this database by reference ([`base_db::run_on_pool`]): one
    /// that unwinds with `salsa::Cancelled::WouldWaitOnItself` runs again, any other panic goes
    /// on from here.
    fn run_all(&self, count: usize, task: &(dyn Fn(&dyn SourceDatabase, usize) + Sync)) {
        if count == 0 {
            return;
        }
        base_db::run_on_pool(self, blaze_graph::exec::shared(), count, task);
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

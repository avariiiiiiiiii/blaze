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

/// The database of a [`crate::v2::Lowered`], and each worker's handle on it.
///
/// A handle ([`crate::v2::Lowered::snapshot`], or a clone of one) shares the storage and the
/// inputs and has its own query stack. While one is alive, a write to the database
/// ([`crate::v2::Lowered::ran`], [`crate::v2::Lowered::macros`]) sets salsa's cancellation
/// flag, so the query its reader is in (or its next one) unwinds with [`salsa::Cancelled`],
/// which the worker turns into `Failed::Again`; the write goes on once every handle is
/// dropped. The loop makes a task's handle as it posts the task and is the only writer; a
/// task drops its handle when it ends or before it waits on anything.
#[salsa::db]
pub struct Db {
    storage: salsa::Storage<Db>,
    files: Arc<Files>,
    crates_map: Arc<CratesMap>,
    nonce: Nonce,
}

/// What [`crate::v2::Lowered::snapshot`] gives a worker: a handle on the database, a
/// `hir::db::HirDatabase` (`Box::new(snapshot) as Box<dyn HirDatabase + Send>` is what
/// `hir_ty_codegen::v2::Databases::new` takes).
pub type Snapshot = Db;

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

    fn crates_map(&self) -> Arc<CratesMap> {
        self.crates_map.clone()
    }

    /// Each task is a job of the process's worker pool (`blaze_graph::exec::shared`, the
    /// daemon's one pool), near this thread's group, reading this database by reference; the
    /// calling thread pump-waits for the batch (`Pool::scope`). The first panic of a task,
    /// salsa's cancellation included, goes on from here once the batch is over.
    fn run_all(&self, count: usize, task: &(dyn Fn(&dyn SourceDatabase, usize) + Sync)) {
        if count == 0 {
            return;
        }
        let pool = blaze_graph::exec::shared();
        let near = blaze_graph::pool::group();
        let panicked = std::sync::Mutex::new(None);
        pool.scope(|scope| {
            for at in 0..count {
                let panicked = &panicked;
                scope.submit(
                    near,
                    move || task(self, at),
                    move |ran| {
                        if let Err(panic) = ran {
                            let mut first = panicked.lock().unwrap_or_else(|it| it.into_inner());
                            first.get_or_insert(panic);
                        }
                    },
                );
            }
        });
        if let Some(panic) = panicked.into_inner().unwrap_or_else(|it| it.into_inner()) {
            panic::resume_unwind(panic);
        }
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

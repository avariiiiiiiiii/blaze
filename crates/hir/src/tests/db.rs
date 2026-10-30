//! A database for the tests: `hir-ide`'s test database, with the names of the queries a step
//! executed.
//!
//! It has one handle, itself: no clone shares its storage. It has no plane either, so
//! [`SourceDatabase::run_all`] runs a batch on the calling thread, in order or reversed
//! ([`TestDB::reverse_batches`]): a result that changed with the order would depend on the
//! plane's scheduling, which none may.

use std::{
    fmt, panic,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use base_db::{
    CrateGraphBuilder, CratesMap, FileSourceRootInput, FileText, Nonce, SourceDatabase, SourceRoot,
    SourceRootId, SourceRootInput, set_all_crates_with_durability,
};
use salsa::Durability;
use span::FileId;
use triomphe::Arc;

#[salsa::db]
pub(crate) struct TestDB {
    storage: salsa::Storage<Self>,
    files: Arc<base_db::Files>,
    crates_map: Arc<CratesMap>,
    events: Arc<Mutex<Option<Vec<salsa::Event>>>>,
    nonce: Nonce,
    reversed: AtomicBool,
}

impl Default for TestDB {
    fn default() -> Self {
        let events = <Arc<Mutex<Option<Vec<salsa::Event>>>>>::default();
        let mut this = Self {
            storage: salsa::Storage::new(Some(Box::new({
                let events = events.clone();
                move |event| {
                    let mut events = events.lock().unwrap();
                    if let Some(events) = &mut *events {
                        events.push(event);
                    }
                }
            }))),
            events,
            files: Default::default(),
            crates_map: Default::default(),
            nonce: Nonce::new(),
            reversed: AtomicBool::new(false),
        };
        hir_def::set_expand_proc_attr_macros(&mut this, true);
        set_all_crates_with_durability(&mut this, std::iter::empty(), Durability::HIGH);
        _ = base_db::LibraryRoots::builder(Default::default())
            .durability(Durability::MEDIUM)
            .new(&this);
        _ = base_db::LocalRoots::builder(Default::default())
            .durability(Durability::MEDIUM)
            .new(&this);
        CrateGraphBuilder::default().set_in_db(&mut this);
        this
    }
}

impl fmt::Debug for TestDB {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TestDB").finish()
    }
}

#[salsa::db]
impl SourceDatabase for TestDB {
    fn file_text(&self, file_id: base_db::FileId) -> FileText {
        self.files.file_text(file_id)
    }

    fn set_file_text(&mut self, file_id: base_db::FileId, text: &str) {
        let files = Arc::clone(&self.files);
        files.set_file_text(self, file_id, text);
    }

    fn set_file_text_with_durability(
        &mut self,
        file_id: base_db::FileId,
        text: &str,
        durability: Durability,
    ) {
        let files = Arc::clone(&self.files);
        files.set_file_text_with_durability(self, file_id, text, durability);
    }

    fn source_root(&self, source_root_id: SourceRootId) -> SourceRootInput {
        self.files.source_root(source_root_id)
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

    fn file_source_root(&self, id: base_db::FileId) -> FileSourceRootInput {
        self.files.file_source_root(self, id)
    }

    fn set_file_source_root_with_durability(
        &mut self,
        id: base_db::FileId,
        source_root_id: SourceRootId,
        durability: Durability,
    ) {
        let files = Arc::clone(&self.files);
        files.set_file_source_root_with_durability(self, id, source_root_id, durability);
    }

    fn set_appended_root_module(
        &mut self,
        file_id: base_db::FileId,
        module: Option<&'static str>,
        durability: Durability,
    ) {
        let files = Arc::clone(&self.files);
        files.set_appended_root_module(self, file_id, module, durability);
    }

    fn set_view_root(
        &mut self,
        view: base_db::ViewRoot<'_>,
    ) -> Result<base_db::ViewChange, base_db::ViewError> {
        let files = Arc::clone(&self.files);
        files.set_view_root(self, view)
    }

    fn crates_map(&self) -> Arc<CratesMap> {
        self.crates_map.clone()
    }

    fn run_all(&self, count: usize, task: &(dyn Fn(&dyn SourceDatabase, usize) + Sync)) {
        if self.reversed.load(Ordering::Relaxed) {
            (0..count).rev().for_each(|at| task(self, at));
        } else {
            (0..count).for_each(|at| task(self, at));
        }
    }

    fn nonce_and_revision(&self) -> (Nonce, salsa::Revision) {
        (self.nonce, salsa::plumbing::ZalsaDatabase::zalsa(self).current_revision())
    }

    fn line_column(&self, _file: FileId, _offset: syntax::TextSize) -> Result<(u32, u32), ()> {
        Err(())
    }
}

#[salsa::db]
impl salsa::Database for TestDB {}

impl panic::RefUnwindSafe for TestDB {}

hir_ide::impl_hir_database!(TestDB);

impl TestDB {
    /// The names of the queries `f` executed, as salsa names them.
    pub(crate) fn executed(&self, f: impl FnOnce()) -> Vec<String> {
        *self.events.lock().unwrap() = Some(Vec::new());
        f();
        let events = self.events.lock().unwrap().take().unwrap();
        events
            .iter()
            .filter_map(|event| match event.kind {
                salsa::EventKind::WillExecute { database_key } => Some(
                    (self as &dyn salsa::Database)
                        .ingredient_debug_name(database_key.ingredient_index())
                        .to_string(),
                ),
                _ => None,
            })
            .collect()
    }

    /// Runs the tasks of every later batch last to first.
    pub(crate) fn reverse_batches(&self) {
        self.reversed.store(true, Ordering::Relaxed);
    }
}

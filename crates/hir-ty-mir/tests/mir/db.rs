//! A database for the tests: `hir-ide`'s test database.

use std::{fmt, panic};

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
    nonce: Nonce,
}

impl Default for TestDB {
    fn default() -> Self {
        let mut this = Self {
            storage: salsa::Storage::new(None),
            files: Default::default(),
            crates_map: Default::default(),
            nonce: Nonce::new(),
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

impl Clone for TestDB {
    fn clone(&self) -> Self {
        Self {
            storage: self.storage.clone(),
            files: self.files.clone(),
            crates_map: self.crates_map.clone(),
            nonce: self.nonce,
        }
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

    fn crates_map(&self) -> Arc<CratesMap> {
        self.crates_map.clone()
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

hir::impl_hir_database!(TestDB);

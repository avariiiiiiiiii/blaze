//! Defines [`EditionedFileId`], an interned wrapper around [`span::EditionedFileId`] that
//! is interned (so queries can take it) and stores only the underlying `span::EditionedFileId`.

use std::hash::Hash;

use salsa::Database;
use span::Edition;
use syntax::{SyntaxError, ast};
use vfs::FileId;

use crate::SourceDatabase;

#[salsa::interned(debug, constructor = from_span_file_id, unsafe(no_lifetime), revisions = usize::MAX)]
#[derive(PartialOrd, Ord)]
pub struct EditionedFileId {
    #[returns(copy)]
    field: span::EditionedFileId,
}

#[salsa::tracked]
impl EditionedFileId {
    /// The file's tree, sharing its text. A file of a library source root (a dependency's) is an
    /// outline, whose `fn` bodies are parsed when first walked into (#1294); every other file is
    /// parsed in full. Decided by the source root, so a file two crates read gets one answer.
    #[salsa::tracked(lru = 128, returns(ref))]
    pub fn parse(self, db: &dyn SourceDatabase) -> syntax::Parse<ast::SourceFile> {
        let _p = tracing::info_span!("parse", ?self).entered();
        let (file_id, edition) = self.unpack(db);
        let text = db.file_text(file_id).text(db).clone();
        match self.is_library(db) {
            true => ast::SourceFile::parse_outline(text, edition),
            false => ast::SourceFile::parse_shared(text, edition),
        }
    }

    /// Whether the file is in a library source root: a firewall, so an edit to the root's set
    /// of files that keeps the answer re-parses none of them.
    #[salsa::tracked]
    pub fn is_library(self, db: &dyn SourceDatabase) -> bool {
        let root = db.file_source_root(self.file_id(db)).source_root_id(db);
        db.source_root(root).source_root_ref(db).is_library
    }

    // firewall query
    #[salsa::tracked(returns(as_deref))]
    pub fn parse_errors(self, db: &dyn SourceDatabase) -> Option<Box<[SyntaxError]>> {
        let errors = self.parse(db).errors();
        match &*errors {
            [] => None,
            [..] => Some(errors.into()),
        }
    }
}

impl EditionedFileId {
    #[inline]
    pub fn new(db: &dyn Database, file_id: FileId, edition: Edition) -> Self {
        Self::from_span_file_id(db, span::EditionedFileId::new(file_id, edition))
    }

    #[inline]
    pub fn current_edition(db: &dyn Database, file_id: FileId) -> Self {
        Self::from_span_file_id(db, span::EditionedFileId::current_edition(file_id))
    }

    #[inline]
    pub fn file_id(self, db: &dyn Database) -> vfs::FileId {
        self.field(db).file_id()
    }

    #[inline]
    pub fn span_file_id(self, db: &dyn Database) -> span::EditionedFileId {
        self.field(db)
    }

    #[inline]
    pub fn unpack(self, db: &dyn Database) -> (vfs::FileId, span::Edition) {
        self.field(db).unpack()
    }

    #[inline]
    pub fn edition(self, db: &dyn Database) -> Edition {
        self.field(db).edition()
    }
}

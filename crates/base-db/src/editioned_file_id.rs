//! Defines [`EditionedFileId`], an interned wrapper around [`span::EditionedFileId`] that
//! is interned (so queries can take it) and stores only the underlying `span::EditionedFileId`.

use std::hash::Hash;

use blaze_ids::ContentId;
use span::Edition;
use syntax::{SyntaxError, ast};
use vfs::FileId;

use crate::Db;

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
    /// After an edit it is made from the file's last parse, when one is kept (see
    /// [`Reparse`](crate::Reparse)): the same tree, in the time of the edited item's parse.
    #[salsa::tracked(lru = 128, returns(ref))]
    pub fn parse(self, db: &Db) -> syntax::Parse<ast::SourceFile> {
        let _p = tracing::info_span!("parse", ?self).entered();
        let (file_id, edition) = self.unpack(db);
        let input = db.file_text(file_id);
        input.reparse_field(db).parse(input.text(db), edition, self.is_library(db))
    }

    /// Whether the file is in a library source root: a firewall, so an edit to the root's set
    /// of files that keeps the answer re-parses none of them.
    #[salsa::tracked]
    pub fn is_library(self, db: &Db) -> bool {
        let root = db.file_source_root(self.file_id(db)).source_root_id(db);
        db.source_root(root).source_root_ref(db).is_library
    }

    // firewall query
    #[salsa::tracked(returns(as_deref))]
    pub fn parse_errors(self, db: &Db) -> Option<Box<[SyntaxError]>> {
        let errors = self.parse(db).errors();
        match &*errors {
            [] => None,
            [..] => Some(errors.into()),
        }
    }

    /// The file's syntax errors with every lazy body's: a firewall, so a check of a
    /// dependency parses its bodies for their errors once.
    #[salsa::tracked(returns(as_deref))]
    pub fn parse_errors_with_bodies(self, db: &Db) -> Option<Box<[SyntaxError]>> {
        let errors = self.parse(db).errors_with_bodies();
        match &*errors {
            [] => None,
            [..] => Some(errors.into()),
        }
    }
}

impl EditionedFileId {
    #[inline]
    pub fn new(db: &Db, file_id: FileId, edition: Edition) -> Self {
        Self::from_span_file_id(db, span::EditionedFileId::new(file_id, edition))
    }

    #[inline]
    pub fn current_edition(db: &Db, file_id: FileId) -> Self {
        Self::from_span_file_id(db, span::EditionedFileId::current_edition(file_id))
    }

    #[inline]
    pub fn file_id(self, db: &Db) -> vfs::FileId {
        self.field(db).file_id()
    }

    #[inline]
    pub fn span_file_id(self, db: &Db) -> span::EditionedFileId {
        self.field(db)
    }

    #[inline]
    pub fn unpack(self, db: &Db) -> (vfs::FileId, span::Edition) {
        self.field(db).unpack()
    }

    #[inline]
    pub fn edition(self, db: &Db) -> Edition {
        self.field(db).edition()
    }

    /// Hands the file's next [`EditionedFileId::parse`] a tree already made of its text, in
    /// full and in this edition (an edit's tree reparsed in place, `blaze_intent::Seed`):
    /// `parse` returns that tree while the caller holds it, instead of parsing again. `content`
    /// is the [`FileText::content`](crate::FileText::content) the tree was made for. Nothing is
    /// taken, and it is `false`, when the file's text has moved on from it, the tree's text is
    /// not the file's, or the file already has a live parse in this edition. What `parse`
    /// returns is the same either way: debug builds check a seed against a whole parse.
    pub fn seed_parse(
        self,
        db: &Db,
        content: ContentId,
        parse: &syntax::Parse<ast::SourceFile>,
    ) -> bool {
        let (file_id, edition) = self.unpack(db);
        let input = db.file_text(file_id);
        input.content(db) == content && input.reparse_field(db).seed(input.text(db), edition, parse)
    }
}

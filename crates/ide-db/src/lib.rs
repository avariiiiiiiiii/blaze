//! The IDE's semantic helpers over the front end's one database, `base_db::Db`, which this crate
//! names `RootDatabase`, with symbol indexes for fuzzy search.

#![cfg_attr(feature = "in-rust-tree", feature(rustc_private))]

#[cfg(feature = "in-rust-tree")]
extern crate rustc_driver as _;

extern crate self as ide_db;

mod apply_change;

pub mod active_parameter;
pub mod assists;
pub mod defs;
pub mod documentation;
pub mod famous_defs;
pub mod helpers;
pub mod items_locator;
pub mod label;
pub mod path_transform;
pub mod prime_caches;
pub mod ra_fixture;
pub mod range_mapper;
pub mod rename;
pub mod rust_doc;
pub mod search;
pub mod source_change;
pub mod symbol_index;
pub mod text_edit;
pub mod traits;
pub mod ty_filter;
pub mod use_trivial_constructor;

pub mod imports {
    pub mod import_assets;
    pub mod insert_use;
    pub mod merge_imports;
}

pub mod generated {
    pub mod lints;
}

pub mod syntax_helpers {
    pub mod format_string;
    pub mod format_string_exprs;
    pub mod tree_diff;
    pub use hir::prettify_macro_expansion;
    pub mod node_ext;
    pub mod suggest_name;

    pub use parser::LexedStr;
}

pub use hir::{ChangeWithProcMacros, EditionedFileId, const_macro};

use std::{
    fmt,
    path::{Path, PathBuf},
    time::Duration,
};

use base_db::{Db, SourceRootId};
use hir::{FilePositionWrapper, FileRangeWrapper};
use profile::Bytes;
use triomphe::Arc;

use crate::line_index::LineIndex;
pub use rustc_hash::{FxHashMap, FxHashSet, FxHasher};

pub use ::line_index;

/// `base_db` is normally also needed in places where `ide_db` is used, so this re-export is for convenience.
pub use base_db::{self, FxIndexMap, FxIndexSet, LibraryRoots, LocalRoots};
pub use span::{self, FileId};

pub type FilePosition = FilePositionWrapper<FileId>;
pub type FileRange = FileRangeWrapper<FileId>;

/// The database ide-db's code and its users hold: the front end's one [`Db`], with what was
/// `RootDatabase`'s own in [`RootDatabaseExt`]. A build's is [`Db::new`] and a test's
/// [`Db::for_tests`] (or `test_fixture::WithFixture`'s `with_files` and the rest); it has no
/// `Default` and no `Clone`, and every thread reads the one database through `&Db`.
pub type RootDatabase = Db;

/// What `RootDatabase` had of its own, on [`Db`]: import it where those methods are called.
pub trait RootDatabaseExt {
    /// The files outside the VFS that an `include_bytes!` or `include_str!` has read.
    fn included_files(&self) -> Vec<vfs::AbsPathBuf>;

    /// The files outside the VFS that `include_bytes!` and `include_str!` in `krate` have read,
    /// with the bytes read of each.
    fn included_by(
        &self,
        krate: base_db::Crate,
    ) -> Vec<(vfs::AbsPathBuf, Option<std::sync::Arc<[u8]>>)>;

    /// The files an `include!` named that the VFS has not, each with the source root of the
    /// file that includes it.
    fn included_sources(&self) -> Vec<(vfs::AbsPathBuf, SourceRootId)>;

    fn sources_included_by(&self) -> Vec<(base_db::Crate, vfs::AbsPathBuf)>;

    /// See [`base_db::Files::set_view`].
    fn set_view(&self, base: &Path, over: &Path, built: Vec<PathBuf>);

    /// The variables `env!` and `option_env!` have asked for, by the crate asking.
    fn env_read(&self) -> Vec<(base_db::Crate, String)>;

    /// The variables `env!` has found unset, by the crate asking.
    fn env_unset(&self) -> Vec<(base_db::Crate, String)>;

    /// Has `krate` be compiled with `env`.
    fn set_env(&mut self, krate: base_db::Crate, env: base_db::Env);

    /// See [`base_db::Files::on_disk`].
    fn on_disk(&self, path: &Path) -> PathBuf;

    /// Whether such a file is on disk what was read of it.
    fn included_is_read(&self, path: &vfs::AbsPath) -> bool;

    fn enable_proc_attr_macros(&mut self);

    /// Cancels every query in flight and applies `change`; returns how long the cancellation
    /// took.
    fn apply_change(&mut self, change: ChangeWithProcMacros) -> Duration;

    fn per_query_memory_usage(&mut self) -> Vec<(String, Bytes, usize)>;
}

impl RootDatabaseExt for Db {
    #[inline]
    fn included_files(&self) -> Vec<vfs::AbsPathBuf> {
        self.files().included_files()
    }

    #[inline]
    fn included_by(
        &self,
        krate: base_db::Crate,
    ) -> Vec<(vfs::AbsPathBuf, Option<std::sync::Arc<[u8]>>)> {
        self.files().included_by(self, krate)
    }

    #[inline]
    fn included_sources(&self) -> Vec<(vfs::AbsPathBuf, SourceRootId)> {
        self.files().included_sources()
    }

    #[inline]
    fn sources_included_by(&self) -> Vec<(base_db::Crate, vfs::AbsPathBuf)> {
        self.files().sources_included_by()
    }

    #[inline]
    fn set_view(&self, base: &Path, over: &Path, built: Vec<PathBuf>) {
        self.files().set_view(base, over, built);
    }

    #[inline]
    fn env_read(&self) -> Vec<(base_db::Crate, String)> {
        self.files().env_read()
    }

    #[inline]
    fn env_unset(&self) -> Vec<(base_db::Crate, String)> {
        self.files().env_unset()
    }

    #[inline]
    fn set_env(&mut self, krate: base_db::Crate, env: base_db::Env) {
        krate.set_env(self, env);
    }

    #[inline]
    fn on_disk(&self, path: &Path) -> PathBuf {
        self.files().on_disk(path)
    }

    #[inline]
    fn included_is_read(&self, path: &vfs::AbsPath) -> bool {
        self.files().included_is_read(self, path)
    }

    fn enable_proc_attr_macros(&mut self) {
        hir::db::set_expand_proc_attr_macros(self, true);
    }

    fn apply_change(&mut self, change: ChangeWithProcMacros) -> Duration {
        apply_change::apply_change(self, change)
    }

    fn per_query_memory_usage(&mut self) -> Vec<(String, Bytes, usize)> {
        apply_change::per_query_memory_usage(self)
    }
}

pub fn line_index(db: &Db, file_id: FileId) -> &Arc<LineIndex> {
    #[salsa::interned]
    pub struct InternedFileId {
        #[returns(copy)]
        id: FileId,
    }
    #[salsa::tracked(returns(ref))]
    fn line_index<'db>(db: &'db Db, file_id: InternedFileId<'db>) -> Arc<LineIndex> {
        let text = db.file_text(file_id.id(db)).text(db);
        Arc::new(LineIndex::new(text))
    }
    line_index(db, InternedFileId::new(db, file_id))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SymbolKind {
    Attribute,
    BuiltinAttr,
    Const,
    ConstParam,
    CrateRoot,
    Derive,
    DeriveHelper,
    Enum,
    Field,
    Function,
    Method,
    Impl,
    InlineAsmRegOrRegClass,
    Label,
    LifetimeParam,
    Local,
    Macro,
    ProcMacro,
    Module,
    SelfParam,
    SelfType,
    Static,
    Struct,
    ToolModule,
    Trait,
    TypeAlias,
    TypeParam,
    Union,
    ValueParam,
    Variant,
}

impl From<hir::MacroKind> for SymbolKind {
    fn from(it: hir::MacroKind) -> Self {
        match it {
            hir::MacroKind::Declarative | hir::MacroKind::DeclarativeBuiltIn => SymbolKind::Macro,
            hir::MacroKind::ProcMacro => SymbolKind::ProcMacro,
            hir::MacroKind::Derive | hir::MacroKind::DeriveBuiltIn => SymbolKind::Derive,
            hir::MacroKind::Attr | hir::MacroKind::AttrBuiltIn => SymbolKind::Attribute,
        }
    }
}

impl SymbolKind {
    pub fn from_module_def(db: &Db, it: hir::ModuleDef) -> Self {
        match it {
            hir::ModuleDef::Const(..) => SymbolKind::Const,
            hir::ModuleDef::EnumVariant(..) => SymbolKind::Variant,
            hir::ModuleDef::Function(..) => SymbolKind::Function,
            hir::ModuleDef::Macro(mac) if mac.is_proc_macro() => SymbolKind::ProcMacro,
            hir::ModuleDef::Macro(..) => SymbolKind::Macro,
            hir::ModuleDef::Module(m) if m.is_crate_root(db) => SymbolKind::CrateRoot,
            hir::ModuleDef::Module(..) => SymbolKind::Module,
            hir::ModuleDef::Static(..) => SymbolKind::Static,
            hir::ModuleDef::Adt(hir::Adt::Struct(..)) => SymbolKind::Struct,
            hir::ModuleDef::Adt(hir::Adt::Enum(..)) => SymbolKind::Enum,
            hir::ModuleDef::Adt(hir::Adt::Union(..)) => SymbolKind::Union,
            hir::ModuleDef::Trait(..) => SymbolKind::Trait,
            hir::ModuleDef::TypeAlias(..) => SymbolKind::TypeAlias,
            hir::ModuleDef::BuiltinType(..) => SymbolKind::TypeAlias,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnippetCap {
    _private: (),
}

impl SnippetCap {
    pub const fn new(allow_snippets: bool) -> Option<SnippetCap> {
        if allow_snippets { Some(SnippetCap { _private: () }) } else { None }
    }
}

pub struct Ranker<'a> {
    pub kind: parser::SyntaxKind,
    pub text: &'a str,
    pub ident_kind: bool,
}

impl<'a> Ranker<'a> {
    pub const MAX_RANK: usize = 0b1110;

    pub fn from_token(token: &'a syntax::SyntaxToken) -> Self {
        let kind = token.kind();
        Ranker { kind, text: token.text(), ident_kind: kind.is_any_identifier() }
    }

    /// A utility function that ranks a token again a given kind and text, returning a number that
    /// represents how close the token is to the given kind and text.
    pub fn rank_token(&self, tok: &syntax::SyntaxToken) -> usize {
        let tok_kind = tok.kind();

        let exact_same_kind = tok_kind == self.kind;
        let both_idents = exact_same_kind || (tok_kind.is_any_identifier() && self.ident_kind);
        let same_text = tok.text() == self.text;
        // anything that mapped into a token tree has likely no semantic information
        let no_tt_parent =
            tok.parent().is_some_and(|it| it.kind() != parser::SyntaxKind::TOKEN_TREE);
        (both_idents as usize)
            | ((exact_same_kind as usize) << 1)
            | ((same_text as usize) << 2)
            | ((no_tt_parent as usize) << 3)
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum Severity {
    Error,
    Warning,
    WeakWarning,
    Allow,
}

#[derive(Clone, Copy)]
pub struct MiniCore<'a>(&'a str);

impl<'a> MiniCore<'a> {
    #[inline]
    pub fn new(minicore: &'a str) -> Self {
        Self(minicore)
    }

    #[inline]
    pub const fn default() -> Self {
        Self(test_utils::MiniCore::RAW_SOURCE)
    }
}

impl std::fmt::Debug for MiniCore<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut d = f.debug_tuple("MiniCore");
        if self.0 == test_utils::MiniCore::RAW_SOURCE {
            // Don't print the whole contents if they correspond to the default.
            // The `format_args!` makes it so that the output is
            // `MiniCore(<default>)` and not `MiniCore("<default>").
            d.field(&format_args!("<default>"));
        } else {
            d.field(&self.0);
        };
        d.finish()
    }
}

impl<'a> Default for MiniCore<'a> {
    #[inline]
    fn default() -> Self {
        Self::default()
    }
}

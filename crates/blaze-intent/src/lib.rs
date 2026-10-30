//! Edits by meaning and declared intent (docs/v2/plan/design/compile-time.md, ideas 9 and 10;
//! docs/v2/plan/lanes/fastcompile/intent.md).
//!
//! [`apply`] makes a request's [`Edit`]s in the files of a view the front end has lowered:
//! each finds its target by path ([`path`]), replaces only that text and reparses only what it
//! touched ([`file::Working::splice`]), and says which items it changed and how. A rename finds
//! every reference through the front end's name resolution, never by text search ([`rename`]).
//! What comes out is the VFS's text edits ([`blaze_vfs::Op::Edit`], and the moves of a renamed
//! module's files): committing them gives exactly the bytes, and so the root, a text edit
//! would. The trees reparsed in place come out too ([`Seed`]), for the front end to take in
//! place of parsing the new text again.
//!
//! [`ahead`] is declared intent's half that needs the tests' last runs: which test binaries and
//! which functions a [`blaze_api::Request::WillTest`] needs built.

#![forbid(unsafe_code)]

pub mod ahead;
pub mod edit;
pub mod file;
pub mod path;
pub mod rename;

#[cfg(test)]
mod tests;

use base_db::{FileId, FileText, SourceDatabase};
use blaze_api::{ApiError, Edit, Edited, EditedItem};
use blaze_ids::ContentId;
use blaze_vfs::Op;
use hir::{Semantics, db::HirDatabase};
use rustc_hash::FxHashMap;
use syntax::{AstNode, Edition, Parse, SourceFile, SyntaxKind, SyntaxNode, ast};

use crate::{
    edit::{At, Touch},
    file::Working,
};

/// The view's files as the job that edits sees them.
pub trait Files {
    /// The front end's file at `path` in the view (relative to its root, as the VFS names
    /// it), and its bytes there.
    fn file(&self, path: &str) -> Option<(FileId, &[u8])>;
    /// The view's path of a front-end file; `None` for one outside the view (a registry
    /// dependency's, the standard library's).
    fn path(&self, file: FileId) -> Option<&str>;
}

/// A file's tree after the edits, parsed in place: what the front end's parse of the file whose
/// text has the content ID `content` (a [`FileText`]'s) and `edition` gives. Only a tree with
/// no syntax errors is one: the errors of a tree reparsed in place may come in another order.
pub struct Seed {
    pub content: ContentId,
    pub edition: Edition,
    pub parse: Parse<SourceFile>,
}

/// What [`apply`] made.
pub struct Applied {
    pub edited: Edited,
    pub seeds: Vec<Seed>,
}

/// The files a request's edits work in, as they leave them.
pub(crate) struct Edits<'a> {
    pub db: &'a dyn HirDatabase,
    pub sema: Semantics<'a, dyn HirDatabase>,
    pub files: &'a dyn Files,
    pub working: Vec<Working>,
    pub ids: Vec<FileId>,
    pub by_path: FxHashMap<Box<str>, u32>,
    pub touches: Vec<Touch>,
    /// The moves of renamed modules' files, after every text edit.
    pub moves: Vec<Op>,
}

/// Makes `edits`, in order, in the view whose front end is `db` and whose files are `files`.
/// A rename resolves in the view as it is, so every rename comes before the request's other
/// edits.
pub fn apply(db: &dyn HirDatabase, files: &dyn Files, edits: &[Edit]) -> Result<Applied, ApiError> {
    let mut cx = Edits {
        db,
        sema: Semantics::new_dyn(db),
        files,
        working: Vec::new(),
        ids: Vec::new(),
        by_path: FxHashMap::default(),
        touches: Vec::new(),
        moves: Vec::new(),
    };
    let mut others_made = false;
    for edit in edits {
        match edit {
            Edit::Rename { to, item } => {
                if others_made {
                    return Err(ApiError::new(
                        format!(
                            "the rename of `{}` comes after another kind of edit: a rename \
                             resolves in the view as it is, so renames come first",
                            item.item
                        ),
                        Some(item.file.clone()),
                    ));
                }
                rename::rename(&mut cx, item, to)?;
            }
            _ => {
                others_made = true;
                let path = edit::file_of(edit);
                let at = cx.open(path).map_err(|what| ApiError::new(what, Some(path.to_owned())))?;
                edit::apply(&mut cx.working[at as usize], at, edit, &mut cx.touches)
                    .map_err(|what| ApiError::new(what, Some(path.to_owned())))?;
            }
        }
    }
    Ok(cx.finish())
}

impl Edits<'_> {
    /// The working copy of the view's file at `path`, opened on first use.
    pub fn open(&mut self, path: &str) -> Result<u32, String> {
        if let Some(&at) = self.by_path.get(path) {
            return Ok(at);
        }
        let (file, _) = self
            .files
            .file(path)
            .ok_or_else(|| format!("{path} is no file of the view the front end has"))?;
        self.load(file, path)
    }

    /// The working copy of the front end's `file`, opened on first use.
    pub fn open_id(&mut self, file: FileId) -> Result<u32, String> {
        if let Some(at) = self.ids.iter().position(|it| *it == file) {
            return Ok(at as u32);
        }
        let path = self
            .files
            .path(file)
            .ok_or_else(|| format!("file {file:?} the edit reaches is outside the view"))?
            .to_owned();
        self.load(file, &path)
    }

    fn load(&mut self, file: FileId, path: &str) -> Result<u32, String> {
        let (_, bytes) = self.files.file(path).ok_or_else(|| format!("{path} is not in the view"))?;
        let bytes = std::str::from_utf8(bytes).map_err(|_| format!("{path} is not UTF-8"))?;
        let editioned = self.sema.attach_first_edition(file);
        let base = self.sema.parse(editioned).syntax().clone();
        let text = self.db.file_text(file).text(self.db);
        if !text.starts_with(bytes) {
            return Err(format!(
                "the front end's text of {path} is not the view's bytes: the job lowered another view"
            ));
        }
        let parse = editioned.parse(self.db).clone();
        let mut working = Working::new(path, editioned.edition(self.db), text.to_string(), bytes.len(), parse);
        working.base = base;
        let at = u32::try_from(self.working.len()).expect("fewer than 2^32 files");
        self.working.push(working);
        self.ids.push(file);
        self.by_path.insert(path.into(), at);
        Ok(at)
    }

    /// The edits' ops, the items they touched, named and keyed in the view as it was, and the
    /// trees to seed.
    fn finish(self) -> Applied {
        let mut ops: Vec<Op> = self.working.iter().flat_map(Working::ops).collect();
        ops.extend(self.moves.iter().cloned());
        let mut touches = self.touches;
        touches.sort_unstable();
        touches.dedup();
        let mut items: Vec<EditedItem> = touches
            .iter()
            .map(|touch| {
                let file = &self.working[touch.file as usize];
                let path = edit::path_at(file, &touch.at);
                let key = match &touch.at {
                    At::Item(offset) => {
                        let node = edit::item_at(&file.base, *offset).expect("a touched item starts there");
                        key_of(&self.sema, self.db, &node)
                    }
                    At::File => file_key(&self.sema, self.db, self.ids[touch.file as usize]),
                    At::Added(_) => None,
                };
                EditedItem { how: touch.how, key, file: file.path.to_string(), path }
            })
            .collect();
        items.sort_unstable_by(|a, b| (&a.file, &a.path, a.how).cmp(&(&b.file, &b.path, b.how)));
        items.dedup();
        let seeds = self
            .working
            .iter()
            .zip(&self.ids)
            .filter(|(file, _)| !file.splices.is_empty() && file.parse.errors().is_empty())
            .map(|(file, id)| {
                let appended = self.db.file_text(*id).appended_root_module(self.db);
                Seed {
                    content: FileText::content_of(file.bytes(), appended),
                    edition: file.edition,
                    parse: file.parse.clone(),
                }
            })
            .collect();
        Applied { edited: Edited { ops, items }, seeds }
    }
}

/// The key ([`blaze_items::ItemKey`]) of the item `node` is, in the view the front end has;
/// `None` for one it has none of (cfg'd out, or no item with a key).
pub(crate) fn key_of(
    sema: &Semantics<'_, dyn HirDatabase>,
    db: &dyn HirDatabase,
    node: &SyntaxNode,
) -> Option<ContentId> {
    use blaze_items::ItemId;
    use hir_def::{AdtId, ConstId, FunctionId, ImplId, MacroId, ModuleId, StaticId, TraitId, TypeAliasId};

    let id = match node.kind() {
        SyntaxKind::FN => {
            ItemId::Function(FunctionId::try_from(sema.to_fn_def(&ast::Fn::cast(node.clone())?)?).ok()?)
        }
        SyntaxKind::STRUCT => {
            ItemId::Adt(AdtId::StructId(sema.to_struct_def(&ast::Struct::cast(node.clone())?)?.into()))
        }
        SyntaxKind::ENUM => ItemId::Adt(AdtId::EnumId(sema.to_enum_def(&ast::Enum::cast(node.clone())?)?.into())),
        SyntaxKind::UNION => {
            ItemId::Adt(AdtId::UnionId(sema.to_def(&ast::Union::cast(node.clone())?)?.into()))
        }
        SyntaxKind::TRAIT => ItemId::Trait(TraitId::from(sema.to_trait_def(&ast::Trait::cast(node.clone())?)?)),
        SyntaxKind::IMPL => ItemId::Impl(ImplId::try_from(sema.to_impl_def(&ast::Impl::cast(node.clone())?)?).ok()?),
        SyntaxKind::CONST => ItemId::Const(ConstId::from(sema.to_const_def(&ast::Const::cast(node.clone())?)?)),
        SyntaxKind::STATIC => {
            ItemId::Static(StaticId::from(sema.to_static_def(&ast::Static::cast(node.clone())?)?))
        }
        SyntaxKind::TYPE_ALIAS => {
            ItemId::Alias(TypeAliasId::from(sema.to_def(&ast::TypeAlias::cast(node.clone())?)?))
        }
        SyntaxKind::MACRO_RULES => ItemId::Macro(MacroId::from(
            sema.to_macro_def(&ast::Macro::MacroRules(ast::MacroRules::cast(node.clone())?))?,
        )),
        SyntaxKind::MACRO_DEF => ItemId::Macro(MacroId::from(
            sema.to_macro_def(&ast::Macro::MacroDef(ast::MacroDef::cast(node.clone())?))?,
        )),
        SyntaxKind::MODULE => {
            let module = ModuleId::from(sema.to_module_def(&ast::Module::cast(node.clone())?)?);
            return Some(blaze_items::key::module_key(db, module).0);
        }
        _ => return None,
    };
    Some(blaze_items::item_key(db, id).0)
}

/// The key of the module a file is.
fn file_key(sema: &Semantics<'_, dyn HirDatabase>, db: &dyn HirDatabase, file: FileId) -> Option<ContentId> {
    let module = sema.file_to_module_def(file)?;
    Some(blaze_items::key::module_key(db, module.into()).0)
}
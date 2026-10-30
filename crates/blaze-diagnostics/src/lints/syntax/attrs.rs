//! The lints of attributes: what rustc's attribute parser (`rustc_attr_parsing`), its
//! attribute checks (`rustc_passes/src/check_attr.rs`), expansion's checks of the attributes of
//! macro calls and the early lint pass say of the attributes of a crate.
//!
//! - [`target`]: what an attribute is applied to (rustc's `Target`).
//! - [`table`]: rustc's builtin attribute parsers: each one's path, template, targets and what
//!   a second one does.
//! - `parse`: what attribute parsing gives: an attribute where it does nothing or is not
//!   allowed, a duplicate, an empty list, an ill-formed input, `unsafe` left out, a crate-level
//!   attribute below the crate root, and expansion's checks of the attributes of macro calls.
//! - `check_attr`: what `check_mod_attrs` lints: `conflicting_repr_hints`, `#[inline]` on an
//!   exported function, `#[link]` on what it does nothing for, empty lint attributes, ..
//! - `doc`: `invalid_doc_attributes`, the lints of `#[doc(..)]`, `unused_doc_comments`.
//! - `diagnostic`: the lints of the `#[diagnostic::..]` attributes.
//! - `features`: `incomplete_features`, `internal_features`, `unstable_features`.
//!
//! In this nightly rustc parses attributes as it lowers the AST to HIR and emits the lints it
//! finds then (`rustc_hir::lints::DelayedLint`) in `emit_delayed_lints`, right after
//! `check_mod_attrs`; the attributes of a macro call are parsed at expansion and their lints
//! buffered for the early lint pass.
//!
//! Follows rustc's `compiler/rustc_attr_parsing/src/interface.rs` (`parse_attribute_list`),
//! `compiler/rustc_ast_lowering/src/lib.rs` (`lower_attrs`), `compiler/rustc_expand/src/
//! config.rs` (`expand_cfg_attr`) and `compiler/rustc_interface/src/passes.rs`
//! (`emit_delayed_lints`) (MIT or Apache-2.0).

mod check_attr;
mod diagnostic;
mod doc;
pub(crate) mod features;
pub(crate) mod parse;
pub(crate) mod table;
pub(crate) mod target;

pub(crate) use target::Target;

use either::Either;
use hir::{HirFileId, InFile};
use rustc_hash::FxHashMap;
use syntax::{AstNode, SyntaxKind, SyntaxNode, SyntaxNodePtr, TextRange, ast};

use super::{Emitted, Pass, Walk};
use crate::{cx::Cx, lower::Phase, model::Diag};

/// An attribute of a node as rustc's attribute parser takes it: one of a `cfg_attr`'s
/// attributes is one of its own, a doc comment is `#[doc = ".."]`.
#[derive(Clone, Debug)]
pub(crate) struct Attr {
    /// The file it is written in (a module file's inner attributes are its `mod` item's).
    pub(crate) file: HirFileId,
    pub(crate) kind: AttrKind,
    /// `#![..]` or `//!`.
    pub(crate) inner: bool,
}

#[derive(Clone, Debug)]
pub(crate) enum AttrKind {
    /// `#[path args]`.
    Meta {
        /// The attribute, without the `unsafe(..)` around it.
        meta: ast::Meta,
        /// The `#[..]` it is written in: its own, or the `cfg_attr`'s that gives it.
        written: ast::Attr,
        /// The `unsafe(..)` around it.
        unsafety: Option<ast::UnsafeMeta>,
        /// Whether a `cfg_attr` gives it.
        from_cfg_attr: bool,
    },
    /// `///`, `//!`, `/** */`, `/*! */`.
    Doc(ast::DocComment),
}

/// An attribute's arguments: rustc's `ArgParser`.
#[derive(Clone, Debug)]
pub(crate) enum Args {
    /// `#[path]`.
    Word,
    /// `#[path(..)]`, `#[path[..]]`, `#[path{..}]`.
    List(ast::TokenTree),
    /// `#[path = value]`; a doc comment's is its text.
    NameValue(Option<ast::Expr>),
}

impl Attr {
    pub(crate) fn meta(&self) -> Option<&ast::Meta> {
        match &self.kind {
            AttrKind::Meta { meta, .. } => Some(meta),
            AttrKind::Doc(_) => None,
        }
    }

    pub(crate) fn is_doc_comment(&self) -> bool {
        matches!(self.kind, AttrKind::Doc(_))
    }

    /// The segments of its path: `["inline"]`, `["diagnostic", "on_unimplemented"]`; a doc
    /// comment's is `["doc"]`.
    pub(crate) fn path(&self) -> Vec<String> {
        let AttrKind::Meta { meta, .. } = &self.kind else { return vec!["doc".to_owned()] };
        let Some(path) = meta.path() else { return Vec::new() };
        path.segments()
            .map(|it| match it.name_ref() {
                Some(name) => name.text().to_string(),
                None => it.syntax().text().to_string(),
            })
            .collect()
    }

    /// Its path as rustc prints it: `inline`, `diagnostic::on_unimplemented`.
    pub(crate) fn name(&self) -> String {
        self.path().join("::")
    }

    /// Whether its path is the one segment `name`.
    pub(crate) fn is(&self, name: &str) -> bool {
        let path = self.path();
        path.len() == 1 && path[0] == name
    }

    /// Whether it is written `#[unsafe(..)]`.
    pub(crate) fn is_unsafe(&self) -> bool {
        matches!(&self.kind, AttrKind::Meta { unsafety: Some(_), .. })
    }

    /// rustc's `attr_span`: the whole `#[..]`, or for one a `cfg_attr` gives, the attribute
    /// in it (its outer and inner spans are the same).
    pub(crate) fn range(&self) -> TextRange {
        match &self.kind {
            AttrKind::Doc(doc) => doc.syntax().text_range(),
            AttrKind::Meta { from_cfg_attr: true, .. } => self.inner_range(),
            AttrKind::Meta { written, .. } => written.syntax().text_range(),
        }
    }

    /// rustc's `inner_span`: what is between `#[` and `]`, the `unsafe(..)` included.
    pub(crate) fn inner_range(&self) -> TextRange {
        match &self.kind {
            AttrKind::Doc(doc) => doc.syntax().text_range(),
            AttrKind::Meta { unsafety: Some(unsafety), .. } => unsafety.syntax().text_range(),
            AttrKind::Meta { meta, .. } => meta.syntax().text_range(),
        }
    }

    /// Where its path is written; a doc comment's is the comment.
    pub(crate) fn path_range(&self) -> TextRange {
        match self.meta().and_then(|it| it.path()) {
            Some(path) => path.syntax().text_range(),
            None => self.inner_range(),
        }
    }

    /// Its arguments.
    pub(crate) fn args(&self) -> Args {
        match &self.kind {
            AttrKind::Doc(_) => Args::NameValue(None),
            AttrKind::Meta { meta, .. } => match meta {
                ast::Meta::TokenTreeMeta(it) => match it.token_tree() {
                    Some(tt) => Args::List(tt),
                    None => Args::Word,
                },
                ast::Meta::KeyValueMeta(it) => Args::NameValue(it.expr()),
                _ => Args::Word,
            },
        }
    }

    /// `range` of the file it is written in.
    pub(crate) fn in_file(&self, range: TextRange) -> InFile<TextRange> {
        InFile::new(self.file, range)
    }
}

/// A node with attributes, as rustc lowers it: what they are applied to and each of them, in
/// order (outer, then inner).
#[derive(Clone, Debug)]
pub(crate) struct Owner {
    pub(crate) node: InFile<SyntaxNode>,
    pub(crate) target: Target,
    pub(crate) attrs: Vec<Attr>,
    /// For a macro call, the node expansion buffers its attributes' lints at
    /// (`current_expansion.lint_node_id`).
    pub(crate) buffered_at: Option<InFile<SyntaxNodePtr>>,
}

impl Owner {
    /// The node a lint of its attributes is emitted at.
    pub(crate) fn at(&self) -> InFile<SyntaxNodePtr> {
        self.node.as_ref().map(SyntaxNodePtr::new)
    }

    /// Whether it is in a macro call's expansion.
    pub(crate) fn in_expansion(&self) -> bool {
        self.node.file_id.is_macro()
    }
}

/// Every node of the crate rustc lowers attributes of, as the walk comes to them, each with
/// its attributes as `cfg` and `cfg_attr` leave them; the nodes with none left out.
pub(crate) fn owners(cx: &Cx<'_, '_>, walk: &Walk<'_>) -> Vec<Owner> {
    let Some(root) = cx.krate.root_module(cx.db).as_source_file_id(cx.db) else { return Vec::new() };
    let crate_root = HirFileId::from(root);
    let mut declared: FxHashMap<HirFileId, InFile<SyntaxNodePtr>> = FxHashMap::default();
    for module in crate::checks::modules(cx) {
        if let Some(file) = module.as_source_file_id(cx.db)
            && let Some(declaration) = module.declaration_source(cx.db)
        {
            let ptr = declaration.as_ref().map(|it| SyntaxNodePtr::new(it.syntax()));
            declared.insert(HirFileId::from(file), ptr);
        }
    }
    let mut owners: Vec<Owner> = Vec::new();
    let mut index: FxHashMap<InFile<SyntaxNodePtr>, usize> = FxHashMap::default();
    for tree in &walk.trees {
        for node in tree.of_kind(ast::AnyHasAttrs::can_cast) {
            let Some(has_attrs) = ast::AnyHasAttrs::cast(node.clone()) else { continue };
            if node.kind() == SyntaxKind::SOURCE_FILE
                && tree.in_file()
                && let Some(declaration) = declared.get(&tree.file)
                && let Some(&at) = index.get(declaration)
            {
                owners[at].attrs.extend(attributes(walk, tree.file, &has_attrs));
                continue;
            }
            let Some(target) = Target::of(cx, InFile::new(tree.file, node), crate_root) else { continue };
            let ptr = tree.ptr(node);
            index.insert(ptr, owners.len());
            owners.push(Owner {
                node: InFile::new(tree.file, node.clone()),
                target,
                attrs: attributes(walk, tree.file, &has_attrs),
                buffered_at: None,
            });
        }
    }
    owners.retain(|it| !it.attrs.is_empty());
    let mut modules = doc::comments::Modules::default();
    for owner in owners.iter_mut().filter(|it| it.target == Target::MacroCall) {
        owner.buffered_at = Some(doc::comments::lint_node(cx, &owner.node, &mut modules));
    }
    owners
}

/// The attributes of `owner`, written in `file`, with `cfg_attr` expanded as the crate's cfg
/// says and the `cfg`s left out (rustc keeps only a trace of them).
fn attributes(walk: &Walk<'_>, file: HirFileId, owner: &ast::AnyHasAttrs) -> Vec<Attr> {
    let mut attrs = Vec::new();
    let written = ast::attrs_with_doc_including_inner(owner).map(|it| match it {
        ast::AnyAttr::Attr(it) => Either::Left(it),
        ast::AnyAttr::DocComment(it) => Either::Right(it),
    });
    hir_expand::attrs::expand_cfg_attr_with_doc_comments::<_, ()>(written, || walk.cfg, |it| {
        match it {
            Either::Left((ast::Meta::CfgMeta(_), _)) => {}
            Either::Left((meta, written)) => {
                let unsafety = meta.syntax().parent().and_then(ast::UnsafeMeta::cast);
                let top = written.meta();
                let from_cfg_attr = matches!(top, Some(ast::Meta::CfgAttrMeta(_)))
                    || matches!(&top, Some(ast::Meta::UnsafeMeta(it)) if matches!(it.meta(), Some(ast::Meta::CfgAttrMeta(_))));
                let inner = written.kind().is_inner();
                attrs.push(Attr { file, kind: AttrKind::Meta { meta, written, unsafety, from_cfg_attr }, inner });
            }
            Either::Right(doc) => {
                let inner = doc.kind().is_inner();
                attrs.push(Attr { file, kind: AttrKind::Doc(doc), inner });
            }
        }
        std::ops::ControlFlow::Continue(())
    });
    attrs
}

/// A lint attribute parsing gives as AST lowering parses `owner`'s attributes: emitted with
/// the rest of the delayed lints, right after `check_mod_attrs`; for a macro call's attributes,
/// which expansion parses, buffered for the early lint pass.
pub(crate) fn parsed(diag: Diag, owner: &Owner) -> Emitted {
    match &owner.buffered_at {
        Some(at) => Emitted::new(diag, Phase::Resolve, Pass::Buffered, at.clone()),
        None => Emitted::new(diag, Phase::Items, Pass::Delayed, owner.at()),
    }
}

/// The node a lint of `owner`'s attributes is emitted at: for a macro call, the one
/// expansion buffers at.
pub(crate) fn lint_at(owner: &Owner) -> InFile<SyntaxNodePtr> {
    owner.buffered_at.clone().unwrap_or_else(|| owner.at())
}

/// A lint `check_mod_attrs` gives of `owner`'s attributes.
pub(crate) fn checked(diag: Diag, owner: &Owner) -> Emitted {
    Emitted::new(diag, Phase::Items, Pass::Early, owner.at())
}

/// The lints of the attributes of the crate.
pub(crate) fn lints(cx: &Cx<'_, '_>, walk: &Walk<'_>) -> Vec<Emitted> {
    let owners = owners(cx, walk);
    let mut emitted = parse::lints(cx, walk, &owners);
    emitted.extend(check_attr::lints(cx, walk, &owners));
    emitted.extend(doc::lints(cx, walk, &owners));
    emitted.extend(diagnostic::lints(cx, walk, &owners));
    emitted.extend(features::lints(cx, walk, &owners));
    emitted
}

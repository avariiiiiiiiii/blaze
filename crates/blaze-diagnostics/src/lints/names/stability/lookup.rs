//! The deprecation and stability of a definition: its own attribute, else what it inherits
//! from the definitions it is in; and whether a macro's expansion may use an unstable
//! feature.
//!
//! Follows `compiler/rustc_passes/src/stability.rs` (`lookup_deprecation_entry`,
//! `inherit_deprecation`, `lookup_stability`, `inherit_stability`),
//! `compiler/rustc_middle/src/middle/stability.rs` (`DeprecationEntry`, `same_origin`) and
//! `compiler/rustc_span/src/lib.rs` (`Span::allows_unstable`) (MIT or Apache-2.0).

use hir::{HirFileId, InFile, ModuleSource};
use intern::sym;
use syntax::{AstNode, SyntaxNode, SyntaxNodePtr, TextRange, ast};

use super::{
    attr::{self, Deprecation, Stability},
    def::{Def, attr_parent},
};
use crate::cx::Cx;

/// `DeprecationEntry`: a deprecation and, for one of this crate, the node its attribute is
/// on, which those inheriting it share.
#[derive(Clone, Debug)]
pub(super) struct Entry {
    pub(super) depr: Deprecation,
    pub(super) origin: Option<InFile<SyntaxNodePtr>>,
}

impl Entry {
    pub(super) fn same_origin(&self, other: &Entry) -> bool {
        self.origin.is_some() && self.origin == other.origin
    }
}

/// Whether `krate` is compiled with `#![feature(staged_api)]`.
pub(super) fn staged(cx: &Cx<'_, '_>, krate: hir::Crate) -> bool {
    krate.is_unstable_feature_enabled(cx.db, &sym::staged_api)
}

/// The attributes of the definition at `node`: a `mod foo;`'s with those its file starts
/// with.
pub(super) fn own_attrs(cx: &Cx<'_, '_>, node: &InFile<SyntaxNode>, krate: hir::Crate) -> Vec<(ast::Meta, ast::Attr)> {
    let cfg = krate.cfg(cx.db);
    let mut attrs = attr::attrs(&node.value, cfg);
    if let Some(module) = ast::Module::cast(node.value.clone())
        && module.item_list().is_none()
        && let Some(def) = cx.sema.to_def2(node.with_value(&module))
        && let ModuleSource::SourceFile(file) = def.definition_source(cx.db).value
    {
        attrs.extend(attr::attrs(file.syntax(), cfg));
    }
    attrs
}

/// `lookup_deprecation_entry` of `def`.
pub(super) fn deprecation(cx: &Cx<'_, '_>, def: Def) -> Option<Entry> {
    deprecation_at(cx, def.source(cx)?, def.krate(cx.db)?)
}

/// `lookup_deprecation_entry` of the definition of `krate` at `node`.
pub(super) fn deprecation_at(cx: &Cx<'_, '_>, node: InFile<SyntaxNode>, krate: hir::Crate) -> Option<Entry> {
    let staged = staged(cx, krate);
    let local = krate == cx.krate;
    let mut at = Some(node);
    while let Some(node) = at {
        if let Some(depr) = attr::deprecation(&own_attrs(cx, &node, krate), staged) {
            let origin = local.then(|| node.map(|it| SyntaxNodePtr::new(&it)));
            return Some(Entry { depr, origin });
        }
        at = attr_parent(cx, node.clone()).or_else(|| crate_root(cx, &node, krate));
    }
    None
}

/// The crate root of `krate`, as the parent of `node` that has no other.
fn crate_root(cx: &Cx<'_, '_>, node: &InFile<SyntaxNode>, krate: hir::Crate) -> Option<InFile<SyntaxNode>> {
    let root = krate.root_module(cx.db).definition_source(cx.db).map(|it| it.node());
    (root != *node).then_some(root)
}

/// `lookup_stability` of `def`, of a crate with `#![feature(staged_api)]`.
pub(super) fn stability(cx: &Cx<'_, '_>, def: Def) -> Option<Stability> {
    let krate = def.krate(cx.db)?;
    if !staged(cx, krate) {
        return None;
    }
    stability_at(cx, def.source(cx)?, krate)
}

fn stability_at(cx: &Cx<'_, '_>, node: InFile<SyntaxNode>, krate: hir::Crate) -> Option<Stability> {
    if let Some(stability) = attr::stability(&own_attrs(cx, &node, krate)) {
        return Some(stability);
    }
    let parent = attr_parent(cx, node.clone()).or_else(|| crate_root(cx, &node, krate))?;
    let parent = stability_at(cx, parent, krate)?;
    let inherits = ast::RecordField::can_cast(node.value.kind())
        || ast::TupleField::can_cast(node.value.kind())
        || ast::Variant::can_cast(node.value.kind());
    (parent.is_unstable() || inherits).then_some(parent)
}

/// `Span::allows_unstable(feature)` of `range` in `file`: the macro whose expansion its
/// first token comes from is defined with `#[allow_internal_unstable(feature)]`.
pub(super) fn allows_unstable(cx: &Cx<'_, '_>, file: HirFileId, range: TextRange, feature: &str) -> bool {
    let db = cx.db;
    let ctxt = file.span_map(db).span_for_range(range).ctx;
    let Some(call) = ctxt.outer_expn(db) else { return false };
    let def = hir_expand::MacroCallId::from(call).loc(db).def;
    let node: SyntaxNode = match def.ast_id() {
        either::Either::Left(id) => id.to_node(db).syntax().clone(),
        either::Either::Right(id) => id.to_node(db).syntax().clone(),
    };
    let attrs = attr::attrs(&node, hir::Crate::from(def.krate).cfg(db));
    attr::allow_internal_unstable(&attrs).iter().any(|it| it == feature)
}

//! What rustc's resolver says of the names in the signatures of items, where hir resolves them
//! and reports nothing: the paths of types and traits in fields, signatures, impl headers,
//! bounds, where clauses, generic defaults and type aliases (E0425, E0405, E0404, E0573, E0433,
//! E0411, E0401, E0603), their lifetimes (E0261, E0401), and re-exports more visible than
//! their items (E0364, E0365). The names in bodies and in the constants of types (array
//! lengths) are the front end's: `UnresolvedIdent` of their bodies and anon consts.
//!
//! Follows rustc's `compiler/rustc_resolve/src/late.rs` (`smart_resolve_path_fragment`,
//! `resolve_lifetime`, `PathSource`), `late/diagnostics.rs` and `imports.rs` (MIT or
//! Apache-2.0).

mod failure;
mod lifetimes;
mod outer;
mod paths;
mod privacy;
mod reexports;

use blaze_items::{Fact, ItemId};
use hir::{HasSource as _, InFile};
use hir_def::ImplId;
use syntax::{AstNode, SyntaxNode, WalkEvent, ast};

use super::defs::{item_id, scopes};
use crate::{cx::Cx, lower::Found};

/// The imports first, as rustc finalizes them before it resolves the items.
pub(crate) fn check(cx: &Cx<'_, '_>) -> Vec<Found> {
    let mut found = reexports::check(cx);
    for signature in signatures(cx) {
        walk(cx, &signature, &mut found);
    }
    found
}

/// An item whose signature is walked: its syntax, rooted where Semantics resolves it, and the
/// fact of it its errors read.
pub(super) struct Signature {
    pub(super) node: InFile<SyntaxNode>,
    pub(super) fact: Option<(Fact, String)>,
}

/// The items of the crate's scopes, in rustc's order, those of traits and impls after them.
fn signatures(cx: &Cx<'_, '_>) -> Vec<Signature> {
    let db = cx.db;
    let mut out = Vec::new();
    let mut push = |node: Option<InFile<SyntaxNode>>, item: Option<ItemId>| {
        let Some(node) = node else { return };
        let fact = item.map(|it| (Fact::Signature(cx.key(it)), "the signature of the item".to_owned()));
        out.push(Signature { node, fact });
    };
    for module in scopes(cx) {
        for def in module.declarations(db) {
            let item = item_id(db, def);
            match def {
                hir::ModuleDef::Function(it) => push(syntax_of(cx.sema.source(it)), item),
                hir::ModuleDef::Adt(it) => push(syntax_of(cx.sema.source(it)), item),
                hir::ModuleDef::TypeAlias(it) => push(syntax_of(cx.sema.source(it)), item),
                hir::ModuleDef::Const(it) => push(syntax_of(cx.sema.source(it)), item),
                hir::ModuleDef::Static(it) => push(syntax_of(cx.sema.source(it)), item),
                hir::ModuleDef::Trait(it) => {
                    push(syntax_of(cx.sema.source(it)), item);
                    for assoc in it.items(db) {
                        push(assoc_syntax(cx, assoc), item_id(db, assoc.into()));
                    }
                }
                _ => {}
            }
        }
        for imp in module.impl_defs(db) {
            push(syntax_of(cx.sema.source(imp)), ImplId::try_from(imp).ok().map(ItemId::Impl));
            for assoc in imp.items(db) {
                push(assoc_syntax(cx, assoc), item_id(db, assoc.into()));
            }
        }
    }
    out
}

fn syntax_of<N: AstNode>(source: Option<InFile<N>>) -> Option<InFile<SyntaxNode>> {
    source.map(|it| it.map(|it| it.syntax().clone()))
}

fn assoc_syntax(cx: &Cx<'_, '_>, assoc: hir::AssocItem) -> Option<InFile<SyntaxNode>> {
    match assoc {
        hir::AssocItem::Function(it) => syntax_of(cx.sema.source(it)),
        hir::AssocItem::Const(it) => syntax_of(cx.sema.source(it)),
        hir::AssocItem::TypeAlias(it) => syntax_of(cx.sema.source(it)),
    }
}

/// Each path of a type or trait and each lifetime of the signature, the items, bodies,
/// attributes and constants inside it left out.
fn walk(cx: &Cx<'_, '_>, signature: &Signature, found: &mut Vec<Found>) {
    let mut walk = signature.node.value.preorder();
    walk.next();
    while let Some(event) = walk.next() {
        let WalkEvent::Enter(node) = event else { continue };
        if is_skipped(&node) {
            walk.skip_subtree();
            continue;
        }
        if let Some(ty) = ast::PathType::cast(node.clone()) {
            if let Some(path) = ty.path() {
                found.extend(paths::check(cx, signature, &ty, &path));
            }
        } else if let Some(lifetime) = ast::Lifetime::cast(node) {
            found.extend(lifetimes::check(cx, signature, &lifetime));
        }
    }
}

fn is_skipped(node: &SyntaxNode) -> bool {
    let kind = node.kind();
    ast::Item::can_cast(kind)
        || ast::AssocItemList::can_cast(kind)
        || ast::Expr::can_cast(kind)
        || ast::Attr::can_cast(kind)
        || ast::ConstArg::can_cast(kind)
        || ast::Visibility::can_cast(kind)
        || ast::MacroCall::can_cast(kind)
        || ast::UseBoundGenericArgs::can_cast(kind)
}

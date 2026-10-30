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
use syntax::{AstNode, SyntaxKind, SyntaxNode, WalkEvent, ast};

use super::defs::item_id;
use crate::{cx::Cx, lower::Found, model::Diag};

/// The imports first, as rustc finalizes them before it resolves the items. `scopes` is
/// `defs::scopes`, made once per check.
pub(crate) fn check(cx: &Cx<'_, '_>, scopes: &[hir::Module]) -> Vec<Found> {
    let mut found = reexports::check(cx, scopes);
    signatures(cx, scopes, |signature| walk(cx, &signature, &mut found));
    found
}

/// An item whose signature is walked: its syntax, rooted where Semantics resolves it, and the
/// item whose signature its errors read.
pub(super) struct Signature {
    pub(super) node: InFile<SyntaxNode>,
    pub(super) item: Option<ItemId>,
}

impl Signature {
    /// The fact of the item's signature an error of it reads (its key is asked for here, at
    /// the edge, not for every item walked).
    pub(super) fn fact(&self, cx: &Cx<'_, '_>, diag: &mut Diag) {
        if let Some(item) = self.item {
            diag.fact(Fact::Signature(cx.key(item)), "the signature of the item");
        }
    }
}

/// Each item of the crate's scopes given to `visit`, in rustc's order, those of traits and
/// impls after them.
fn signatures(cx: &Cx<'_, '_>, scopes: &[hir::Module], mut visit: impl FnMut(Signature)) {
    let db = cx.db;
    let mut push = |node: Option<InFile<SyntaxNode>>, item: Option<ItemId>| {
        let Some(node) = node else { return };
        visit(Signature { node, item });
    };
    for &module in scopes {
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
        match node.kind() {
            SyntaxKind::PATH_TYPE => {
                let ty = ast::PathType::cast(node).expect("a PATH_TYPE node is a path type");
                if let Some(path) = ty.path() {
                    found.extend(paths::check(cx, signature, &ty, &path));
                }
            }
            SyntaxKind::LIFETIME => {
                let lifetime = ast::Lifetime::cast(node).expect("a LIFETIME node is a lifetime");
                found.extend(lifetimes::check(cx, signature, &lifetime));
            }
            _ if is_skipped(&node) => walk.skip_subtree(),
            _ => {}
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

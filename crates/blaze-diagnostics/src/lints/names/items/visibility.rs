//! The defs of walked items as their effective visibilities (`names::effective`) keep them,
//! the syntax of a def's interface, and the module a `pub` item's own visibility and its
//! modules' restrict it to.

use hir::{Adt, FieldSource, HasSource, HasVisibility, InFile, ModuleDef, Visibility};
use syntax::{AstNode, AstPtr, SyntaxNode, ast};

use super::walk::Item;
use crate::{cx::Cx, lints::names::effective::Def};

/// The def of a walked item, as visibilities are kept.
pub(super) fn def(cx: &Cx<'_, '_>, item: &Item) -> Option<Def> {
    let node = item.node.as_ref();
    let def: ModuleDef = match node.value {
        ast::Item::Fn(it) => cx.sema.to_def2(node.with_value(it))?.into(),
        ast::Item::Struct(it) => Adt::from(cx.sema.to_def2(node.with_value(it))?).into(),
        ast::Item::Enum(it) => Adt::from(cx.sema.to_def2(node.with_value(it))?).into(),
        ast::Item::Union(it) => Adt::from(cx.sema.to_def2(node.with_value(it))?).into(),
        ast::Item::Trait(it) => cx.sema.to_def2(node.with_value(it))?.into(),
        ast::Item::Const(it) => cx.sema.to_def2(node.with_value(it))?.into(),
        ast::Item::Static(it) => cx.sema.to_def2(node.with_value(it))?.into(),
        ast::Item::TypeAlias(it) => cx.sema.to_def2(node.with_value(it))?.into(),
        ast::Item::Module(it) => cx.sema.to_def2(node.with_value(it))?.into(),
        ast::Item::Impl(it) => return Some(Def::Impl(cx.sema.to_def2(node.with_value(it))?)),
        _ => return None,
    };
    Some(Def::Item(def))
}

/// The module a def is visible in as its own visibility and its modules' say, `None` for
/// everywhere: rustc's effective visibility of a `pub` item that is not reachable, without
/// what `use`s re-export it to.
pub(super) fn restricted_to(cx: &Cx<'_, '_>, item: &Item) -> Option<hir::Module> {
    if item.in_body.is_some() {
        return Some(cx.krate.root_module(cx.db));
    }
    let db = cx.db;
    let to_module = |vis: Visibility| -> Option<hir::Module> {
        match vis {
            Visibility::Public => None,
            Visibility::PubCrate(_) => Some(cx.krate.root_module(db)),
            Visibility::Module(id, _) => Some(hir::Module::from(id)),
        }
    };
    let mut at: Option<hir::Module> = None;
    let mut module = item.module;
    while let Some(parent) = module.parent(db) {
        if let Some(it) = to_module(module.visibility(db)) {
            at = Some(deeper(cx, at, it));
        }
        module = parent;
    }
    at
}

/// Of a restriction so far and another, the one that lets in less: the deeper module, both
/// being ancestors of the item.
fn deeper(cx: &Cx<'_, '_>, at: Option<hir::Module>, other: hir::Module) -> hir::Module {
    let Some(at) = at else { return other };
    let mut module = Some(at);
    while let Some(it) = module {
        if it == other {
            return at;
        }
        module = it.parent(cx.db);
    }
    other
}

/// The syntax of a def's interface, parsed as the semantics have it.
pub(super) fn interface_node(cx: &Cx<'_, '_>, def: Def) -> Option<InFile<SyntaxNode>> {
    fn of<N: AstNode>(cx: &Cx<'_, '_>, src: Option<InFile<N>>) -> Option<InFile<SyntaxNode>> {
        let src = src?;
        let node = cx.node(src.as_ref().map(AstPtr::new));
        Some(node.map(|it| it.syntax().clone()))
    }
    let db = cx.db;
    match def {
        Def::Item(ModuleDef::Function(it)) => of(cx, it.source(db)),
        Def::Item(ModuleDef::Adt(Adt::Struct(it))) => of(cx, it.source(db)),
        Def::Item(ModuleDef::Adt(Adt::Union(it))) => of(cx, it.source(db)),
        Def::Item(ModuleDef::Adt(Adt::Enum(it))) => of(cx, it.source(db)),
        Def::Item(ModuleDef::Trait(it)) => of(cx, it.source(db)),
        Def::Item(ModuleDef::TypeAlias(it)) => of(cx, it.source(db)),
        Def::Item(ModuleDef::Const(it)) => of(cx, it.source(db)),
        Def::Item(ModuleDef::Static(it)) => of(cx, it.source(db)),
        Def::Impl(it) => of(cx, it.source(db)),
        Def::Field(it) => match it.source(db)? {
            InFile { file_id, value: FieldSource::Named(it) } => of(cx, Some(InFile::new(file_id, it))),
            InFile { file_id, value: FieldSource::Pos(it) } => of(cx, Some(InFile::new(file_id, it))),
        },
        _ => None,
    }
}

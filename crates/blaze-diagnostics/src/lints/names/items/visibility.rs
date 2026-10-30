//! The effective visibilities of the crate's items, as `rustc_privacy` computes them for its
//! lints: an item is exported (`Level::Reexported`) when another crate can name it, through
//! public modules or `pub use`; reachable (`Level::Reachable`) when it is exported or named in
//! the interface of what is reachable (its generics, bounds and types, a public field's type,
//! the items of a trait or of an impl both of whose types and traits are). What is reachable
//! only through an `impl Trait` a function returns (`ReachableThroughImplTrait`) is not.
//!
//! The defs a type names are its paths' resolutions, a type alias's own type in its place: as
//! rustc's visitor walks types with aliases expanded, an alias is never reached through what
//! names it.
//!
//! Follows `compiler/rustc_privacy/src/lib.rs` (`EmbargoVisitor`, `ReachEverythingInTheInterface
//! Visitor`, `EffectiveVisibility::of_impl`) and `compiler/rustc_resolve/src/effective_visibilities.rs`
//! (MIT or Apache-2.0).

use hir::{
    Adt, AsAssocItem as _, AssocItem, Field, FieldSource, HasSource, HasVisibility, InFile, ModuleDef,
    PathResolution, Visibility,
};
use rustc_hash::FxHashSet;
use syntax::{
    AstNode, AstPtr, SyntaxKind, SyntaxNode,
    ast::{self, HasGenericParams, HasTypeBounds},
};

use super::walk::Item;
use crate::cx::Cx;

/// What has an effective visibility.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Def {
    Item(ModuleDef),
    Field(Field),
    Impl(hir::Impl),
}

/// The crate's exported and reachable defs.
pub(super) struct Effective {
    exported: FxHashSet<Def>,
    reachable: FxHashSet<Def>,
}

impl Effective {
    pub(super) fn of(cx: &Cx<'_, '_>, items: &[Item]) -> Effective {
        let impls: Vec<hir::Impl> = items
            .iter()
            .filter_map(|it| match &it.node.value {
                ast::Item::Impl(imp) => cx.sema.to_def2(it.node.with_value(imp)),
                _ => None,
            })
            .collect();
        let mut effective = Effective { exported: FxHashSet::default(), reachable: FxHashSet::default() };
        effective.export(cx);
        for imp in &impls {
            if effective.impl_at(cx, *imp, Level::Exported) {
                for def in impl_items(cx, *imp) {
                    effective.exported.insert(def);
                }
                effective.exported.insert(Def::Impl(*imp));
            }
        }
        effective.reachable = effective.exported.clone();
        let mut work: Vec<Def> = effective.reachable.iter().copied().collect();
        let mut done = FxHashSet::default();
        loop {
            while let Some(def) = work.pop() {
                for next in reaches(cx, def) {
                    if effective.is_local(cx, next) && effective.reachable.insert(next) {
                        work.push(next);
                    }
                }
            }
            for imp in &impls {
                if !done.contains(imp) && effective.impl_at(cx, *imp, Level::Reachable) {
                    done.insert(*imp);
                    for def in impl_items(cx, *imp).into_iter().chain([Def::Impl(*imp)]) {
                        if effective.reachable.insert(def) {
                            work.push(def);
                        }
                    }
                }
            }
            if work.is_empty() {
                return effective;
            }
        }
    }

    /// rustc's `is_exported`: another crate can name it.
    pub(super) fn is_exported(&self, def: Def) -> bool {
        self.exported.contains(&def)
    }

    /// rustc's `is_reachable`: another crate can name it or use it through what it can name.
    pub(super) fn is_reachable(&self, def: Def) -> bool {
        self.reachable.contains(&def)
    }

    /// The defs public modules and `pub use`s name, from the crate's root.
    fn export(&mut self, cx: &Cx<'_, '_>) {
        let db = cx.db;
        let root = cx.krate.root_module(db);
        self.exported.insert(Def::Item(ModuleDef::Module(root)));
        let mut modules = vec![root];
        while let Some(module) = modules.pop() {
            let id: hir_def::ModuleId = module.into();
            let scope = id.def_map(db).module(db, id).scope(db);
            let names = scope
                .types()
                .map(|(_, it)| (it.def, it.vis))
                .chain(scope.values().map(|(_, it)| (it.def, it.vis)))
                .chain(scope.macros().map(|(_, it)| (hir_def::ModuleDefId::MacroId(it.def), it.vis)));
            let mut found = Vec::new();
            for (def, vis) in names {
                if vis == Visibility::Public {
                    found.push(ModuleDef::from(def));
                }
            }
            for def in found {
                if !self.is_local(cx, Def::Item(def)) || !self.exported.insert(Def::Item(def)) {
                    continue;
                }
                match def {
                    ModuleDef::Module(it) => modules.push(it),
                    _ => self.exported.extend(members(cx, Def::Item(def))),
                }
            }
        }
    }

    fn is_local(&self, cx: &Cx<'_, '_>, def: Def) -> bool {
        let module = match def {
            Def::Item(it) => it.module(cx.db),
            Def::Field(it) => Some(it.parent_def(cx.db).module(cx.db)),
            Def::Impl(it) => Some(it.module(cx.db)),
        };
        module.is_some_and(|it| it.krate(cx.db) == cx.krate)
    }

    /// `EffectiveVisibility::of_impl` at `level`: whether every local type and trait its self
    /// type and its trait name are.
    fn impl_at(&self, cx: &Cx<'_, '_>, imp: hir::Impl, level: Level) -> bool {
        let set = match level {
            Level::Exported => &self.exported,
            Level::Reachable => &self.reachable,
        };
        let mut named = Vec::new();
        imp.self_ty(cx.db).walk(cx.db, |ty| {
            if let Some(adt) = ty.as_adt() {
                named.push(Def::Item(ModuleDef::Adt(adt)));
            }
            if let Some(it) = ty.as_dyn_trait() {
                named.push(Def::Item(ModuleDef::Trait(it)));
            }
        });
        if let Some(it) = imp.trait_(cx.db) {
            named.push(Def::Item(ModuleDef::Trait(it)));
        }
        named.into_iter().all(|def| !self.is_local(cx, def) || set.contains(&def))
    }
}

#[derive(Clone, Copy)]
enum Level {
    Exported,
    Reachable,
}

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

/// What a def makes exported or reachable with it: an enum's variants and their fields, a
/// struct's or a union's public fields, a trait's items.
fn members(cx: &Cx<'_, '_>, def: Def) -> Vec<Def> {
    let db = cx.db;
    match def {
        Def::Item(ModuleDef::Adt(Adt::Enum(it))) => it
            .variants(db)
            .into_iter()
            .flat_map(|variant| {
                std::iter::once(Def::Item(ModuleDef::EnumVariant(variant)))
                    .chain(variant.fields(db).into_iter().map(Def::Field))
            })
            .collect(),
        Def::Item(ModuleDef::Adt(Adt::Struct(it))) => public_fields(cx, it.fields(db)),
        Def::Item(ModuleDef::Adt(Adt::Union(it))) => public_fields(cx, it.fields(db)),
        Def::Item(ModuleDef::Trait(it)) => it.items(db).into_iter().map(|it| Def::Item(it.into())).collect(),
        _ => Vec::new(),
    }
}

fn public_fields(cx: &Cx<'_, '_>, fields: Vec<Field>) -> Vec<Def> {
    fields.into_iter().filter(|it| it.visibility(cx.db) == Visibility::Public).map(Def::Field).collect()
}

/// The items of an impl that are as visible as it: an inherent impl's public ones, all of a
/// trait impl's.
fn impl_items(cx: &Cx<'_, '_>, imp: hir::Impl) -> Vec<Def> {
    let db = cx.db;
    let inherent = imp.trait_(db).is_none();
    imp.items(db)
        .into_iter()
        .filter(|it| {
            !inherent
                || match it {
                    AssocItem::Function(f) => f.visibility(db) == Visibility::Public,
                    AssocItem::Const(c) => c.visibility(db) == Visibility::Public,
                    AssocItem::TypeAlias(t) => t.visibility(db) == Visibility::Public,
                }
        })
        .map(|it| Def::Item(it.into()))
        .collect()
}

/// What a reachable def makes reachable: its members, and the defs its interface names.
fn reaches(cx: &Cx<'_, '_>, def: Def) -> Vec<Def> {
    let mut found = members(cx, def);
    if let Some(node) = interface_node(cx, def) {
        found.extend(named(cx, &node));
    }
    found
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

/// The local types and traits an item's interface names (`generics`, `predicates`, `ty`,
/// `trait_ref` of rustc's visitor), aliases followed to their types; not its body, not its
/// fields or items (each reached on its own), not what an `impl Trait` it returns names.
pub(super) fn named(cx: &Cx<'_, '_>, node: &InFile<SyntaxNode>) -> Vec<Def> {
    let mut found = Vec::new();
    let mut aliases = FxHashSet::default();
    named_into(cx, node, &mut found, &mut aliases);
    found
}

fn named_into(
    cx: &Cx<'_, '_>,
    node: &InFile<SyntaxNode>,
    found: &mut Vec<Def>,
    aliases: &mut FxHashSet<hir::TypeAlias>,
) {
    for part in interface_parts(&node.value) {
        let mut walk = part.preorder();
        while let Some(event) = walk.next() {
            let syntax::WalkEvent::Enter(it) = event else { continue };
            if ast::ImplTraitType::can_cast(it.kind()) && is_return_position(&it, &node.value) {
                walk.skip_subtree();
                continue;
            }
            let Some(ty) = ast::PathType::cast(it) else { continue };
            let Some(path) = ty.path() else { continue };
            match cx.sema.resolve_path(&path) {
                Some(PathResolution::Def(ModuleDef::TypeAlias(alias))) if alias.as_assoc_item(cx.db).is_none() => {
                    if aliases.insert(alias)
                        && let Some(src) = alias.source(cx.db)
                    {
                        let alias_node = cx.node(src.as_ref().map(AstPtr::new));
                        if let Some(ty) = alias_node.value.ty() {
                            let ty = alias_node.with_value(ty.syntax().clone());
                            named_into(cx, &ty, found, aliases);
                        }
                    }
                }
                Some(PathResolution::Def(def @ (ModuleDef::Adt(_) | ModuleDef::Trait(_)))) => {
                    found.push(Def::Item(def));
                }
                _ => {}
            }
        }
    }
}

/// The parts of an item that are its interface.
fn interface_parts(node: &SyntaxNode) -> Vec<SyntaxNode> {
    let mut parts: Vec<SyntaxNode> = Vec::new();
    let mut generics = |params: Option<ast::GenericParamList>, clause: Option<ast::WhereClause>| {
        parts.extend(params.map(|it| it.syntax().clone()));
        parts.extend(clause.map(|it| it.syntax().clone()));
    };
    let node = node.clone();
    match node.kind() {
        SyntaxKind::FN => {
            let Some(it) = ast::Fn::cast(node) else { return parts };
            generics(it.generic_param_list(), it.where_clause());
            parts.extend(it.param_list().map(|it| it.syntax().clone()));
            parts.extend(it.ret_type().map(|it| it.syntax().clone()));
        }
        SyntaxKind::STRUCT | SyntaxKind::UNION | SyntaxKind::ENUM => {
            let Some(it) = ast::Adt::cast(node) else { return parts };
            generics(it.generic_param_list(), it.where_clause());
        }
        SyntaxKind::TRAIT => {
            let Some(it) = ast::Trait::cast(node) else { return parts };
            generics(it.generic_param_list(), it.where_clause());
            parts.extend(it.type_bound_list().map(|it| it.syntax().clone()));
        }
        SyntaxKind::TYPE_ALIAS => {
            let Some(it) = ast::TypeAlias::cast(node) else { return parts };
            generics(it.generic_param_list(), it.where_clause());
            parts.extend(it.type_bound_list().map(|it| it.syntax().clone()));
            parts.extend(it.ty().map(|it| it.syntax().clone()));
        }
        SyntaxKind::IMPL => {
            let Some(it) = ast::Impl::cast(node) else { return parts };
            generics(it.generic_param_list(), it.where_clause());
            parts.extend(it.self_ty().map(|it| it.syntax().clone()));
            parts.extend(it.trait_().map(|it| it.syntax().clone()));
        }
        SyntaxKind::CONST => parts.extend(ast::Const::cast(node).and_then(|it| it.ty()).map(|it| it.syntax().clone())),
        SyntaxKind::STATIC => parts.extend(ast::Static::cast(node).and_then(|it| it.ty()).map(|it| it.syntax().clone())),
        SyntaxKind::RECORD_FIELD => {
            parts.extend(ast::RecordField::cast(node).and_then(|it| it.ty()).map(|it| it.syntax().clone()))
        }
        SyntaxKind::TUPLE_FIELD => {
            parts.extend(ast::TupleField::cast(node).and_then(|it| it.ty()).map(|it| it.syntax().clone()))
        }
        _ => {}
    }
    parts
}

/// Whether `node` is in the return type of the function `item` is.
fn is_return_position(node: &SyntaxNode, item: &SyntaxNode) -> bool {
    node.ancestors().take_while(|it| it != item).any(|it| ast::RetType::can_cast(it.kind()))
}

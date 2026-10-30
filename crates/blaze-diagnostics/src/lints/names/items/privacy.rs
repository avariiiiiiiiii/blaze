//! `check_private_in_public`: a type or trait in the interface of an item that is less visible
//! than the item is reachable at. In the item's own interface (its type, its signature, its
//! generics' defaults and const types, an impl's types and trait) it is `private_interfaces`; in
//! its bounds and `where` clauses `private_bounds`. In an associated type a type less visible than
//! the type's nominal visibility is E0446, rustc's hard error of the same pass.
//!
//! The visibility an item is reachable at is rustc's effective visibility at its `Reachable`
//! level, a module restriction or `pub`: from resolve, the least of its visibility and its
//! module's (raised by `use`s); from the embargo visitor, raised for the items of impls and for
//! what a reachable item's interface names. An item private to its module has none, and is not
//! checked.
//!
//! Types are walked as rustc's `DefIdVisitorSkeleton` walks them: each ADT and trait they name,
//! aliases expanded in place, `<T as Trait>::Assoc` as its associated type, its trait and then
//! `T`, `impl Trait` as its bounds; an impl's associated types not looked into.
//!
//! Follows `compiler/rustc_privacy/src/lib.rs` (`EmbargoVisitor`, `SearchInterfaceForPrivateItems
//! Visitor`, `PrivateItemsInPublicInterfacesChecker`, `check_private_in_public`),
//! `compiler/rustc_resolve/src/effective_visibilities.rs`, `compiler/rustc_middle/src/ty/mod.rs`
//! (`Visibility::to_string`) and `compiler/rustc_privacy/src/diagnostics.rs`
//! (`PrivateInterfacesOrBoundsLint`, `InPublicInterface`) (MIT or Apache-2.0).

use std::cell::OnceCell;

use blaze_items::Fact;
use hir::{AsAssocItem as _, AssocItemContainer, HasSource, HasVisibility, InFile, ModuleDef, PathResolution};
use rustc_hash::{FxHashMap, FxHashSet};
use syntax::{
    AstNode, AstPtr, SyntaxKind, SyntaxNode, SyntaxNodePtr,
    ast::{self, HasGenericArgs, HasTypeBounds},
};

use super::{
    def_span,
    visibility::{self, Def},
    walk::{self, Container, Item},
};
use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::Diag,
};

/// A visibility: everywhere, or in a module and the modules in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Vis {
    Public,
    In(hir::Module),
}

fn vis_of(cx: &Cx<'_, '_>, vis: hir::Visibility) -> Vis {
    match vis {
        hir::Visibility::Public => Vis::Public,
        hir::Visibility::PubCrate(_) => Vis::In(cx.krate.root_module(cx.db)),
        hir::Visibility::Module(id, _) => Vis::In(hir::Module::from(id).nearest_non_block_module(cx.db)),
    }
}

/// rustc's `is_at_least`.
fn at_least(cx: &Cx<'_, '_>, a: Vis, b: Vis) -> bool {
    match (a, b) {
        (Vis::Public, _) => true,
        (_, Vis::Public) => false,
        (Vis::In(a), Vis::In(b)) => {
            let mut at = Some(b);
            while let Some(module) = at {
                if module == a {
                    return true;
                }
                at = module.parent(cx.db);
            }
            false
        }
    }
}

fn min(cx: &Cx<'_, '_>, a: Vis, b: Vis) -> Vis {
    if at_least(cx, a, b) { b } else { a }
}

fn max(cx: &Cx<'_, '_>, a: Vis, b: Vis) -> Vis {
    if at_least(cx, a, b) { a } else { b }
}

/// rustc's `greater_than`.
fn greater(cx: &Cx<'_, '_>, a: Vis, b: Vis) -> bool {
    a != b && at_least(cx, a, b)
}

/// The module a def is in, not counting blocks: rustc's `parent_module_from_def_id`.
fn module_of(cx: &Cx<'_, '_>, def: Def) -> Option<hir::Module> {
    let db = cx.db;
    let module = match def {
        Def::Item(it) => it.module(db)?,
        Def::Field(it) => it.parent_def(db).module(db),
        Def::Impl(it) => it.module(db),
    };
    Some(module.nearest_non_block_module(db))
}

fn is_local(cx: &Cx<'_, '_>, def: Def) -> bool {
    module_of(cx, def).is_some_and(|it| it.krate(cx.db) == cx.krate)
}

/// What a def is visible in as it is written: an enum's variants and their fields as the enum,
/// a trait's items as the trait.
fn nominal(cx: &Cx<'_, '_>, def: Def) -> Vis {
    let db = cx.db;
    match def {
        Def::Item(ModuleDef::EnumVariant(it)) => vis_of(cx, it.parent_enum(db).visibility(db)),
        Def::Item(it) => vis_of(cx, it.visibility(db)),
        Def::Field(it) => match it.parent_def(db) {
            hir::Variant::EnumVariant(variant) => vis_of(cx, variant.parent_enum(db).visibility(db)),
            _ => vis_of(cx, it.visibility(db)),
        },
        Def::Impl(_) => Vis::Public,
    }
}

/// The visibility of a def private to its module.
fn private(cx: &Cx<'_, '_>, def: Def) -> Vis {
    match module_of(cx, def) {
        Some(module) => Vis::In(module),
        None => Vis::Public,
    }
}

/// rustc's `Visibility::to_string` of `vis` for `def`.
fn vis_str(cx: &Cx<'_, '_>, vis: Vis, def: Def) -> String {
    match vis {
        Vis::Public => "pub".to_owned(),
        Vis::In(module) if module.is_crate_root(cx.db) => "pub(crate)".to_owned(),
        Vis::In(module) if Some(module) == module_of(cx, def) => "pub(self)".to_owned(),
        Vis::In(module) => {
            let mut path = String::new();
            for it in module.path_to_root(cx.db).into_iter().rev() {
                if let Some(name) = it.name(cx.db) {
                    path.push_str("::");
                    path.push_str(name.as_str());
                }
            }
            format!("pub(in crate{path})")
        }
    }
}

/// The effective visibilities at rustc's `Reachable` level.
struct Levels {
    ev: FxHashMap<Def, Vis>,
}

impl Levels {
    fn of(cx: &Cx<'_, '_>, items: &[Item]) -> Levels {
        let db = cx.db;
        let mut levels = Levels { ev: FxHashMap::default() };
        let root = cx.krate.root_module(db);
        levels.ev.insert(Def::Item(ModuleDef::Module(root)), Vis::Public);
        let modules = crate::checks::modules(cx);
        loop {
            let mut changed = false;
            for &module in &modules {
                let inherited = levels.or_private(cx, Def::Item(ModuleDef::Module(module)));
                let id: hir_def::ModuleId = module.into();
                let scope = id.def_map(db).module(db, id).scope(db);
                let names: Vec<(hir_def::ModuleDefId, hir::Visibility)> = scope
                    .types()
                    .map(|(_, it)| (it.def, it.vis))
                    .chain(scope.values().map(|(_, it)| (it.def, it.vis)))
                    .collect();
                for (def, vis) in names {
                    let def = Def::Item(ModuleDef::from(def));
                    let vis = vis_of(cx, vis);
                    if vis == Vis::In(module) || !is_local(cx, def) {
                        continue;
                    }
                    changed |= levels.update(cx, def, min(cx, vis, inherited));
                }
            }
            if !changed {
                break;
            }
        }
        let defs: Vec<Def> = items.iter().filter_map(|it| visibility::def(cx, it)).collect();
        for &def in &defs {
            levels.members(cx, def);
        }
        let impls: Vec<hir::Impl> = defs
            .iter()
            .filter_map(|it| match it {
                Def::Impl(imp) => Some(*imp),
                _ => None,
            })
            .collect();
        // Each def's interface is walked once per level it reaches, not once per round: what it
        // raises depends only on its level. What it names is resolved once.
        let mut walked: FxHashMap<Def, Vis> = FxHashMap::default();
        let mut named: FxHashMap<Def, Vec<Def>> = FxHashMap::default();
        let mut reached: Vec<(Def, Vis)> = Vec::new();
        loop {
            let mut changed = false;
            for &imp in &impls {
                let ev = levels.impl_ev(cx, imp, true);
                changed |= levels.update(cx, Def::Impl(imp), ev);
                let inherent = imp.trait_(db).is_none();
                for assoc in imp.items(db) {
                    let def = Def::Item(assoc.into());
                    let ev = match inherent {
                        true => {
                            let vis = nominal(cx, def);
                            if vis == private(cx, def) {
                                continue;
                            }
                            min(cx, ev, vis)
                        }
                        false => ev,
                    };
                    changed |= levels.update(cx, def, ev);
                }
            }
            reached.clear();
            reached.extend(levels.ev.iter().filter(|&(def, ev)| walked.get(def) != Some(ev)).map(|(def, ev)| (*def, *ev)));
            for &(def, ev) in &reached {
                walked.insert(def, ev);
                let nexts = named.entry(def).or_insert_with(|| match visibility::interface_node(cx, def) {
                    Some(node) => visibility::named(cx, &node),
                    None => Vec::new(),
                });
                for &next in nexts.iter() {
                    if !is_local(cx, next) {
                        continue;
                    }
                    let vis = nominal(cx, next);
                    if vis == private(cx, next) {
                        continue;
                    }
                    changed |= levels.update(cx, next, min(cx, ev, vis));
                }
            }
            if !changed {
                return levels;
            }
        }
    }

    fn get(&self, def: Def) -> Option<Vis> {
        self.ev.get(&def).copied()
    }

    fn or_private(&self, cx: &Cx<'_, '_>, def: Def) -> Vis {
        self.get(def).unwrap_or_else(|| private(cx, def))
    }

    /// Raises `def` to `vis`, and its members with it; whether it changed.
    fn update(&mut self, cx: &Cx<'_, '_>, def: Def, vis: Vis) -> bool {
        let old = self.get(def);
        let new = match old {
            Some(old) => max(cx, old, vis),
            None => vis,
        };
        if old == Some(new) {
            return false;
        }
        self.ev.insert(def, new);
        self.members(cx, def);
        true
    }

    /// An ADT's variants and fields, a trait's items: each at the least of its own visibility
    /// and its parent's effective one, unless it is private.
    fn members(&mut self, cx: &Cx<'_, '_>, def: Def) {
        let db = cx.db;
        let members: Vec<Def> = match def {
            Def::Item(ModuleDef::Adt(hir::Adt::Enum(it))) => it
                .variants(db)
                .into_iter()
                .flat_map(|variant| {
                    std::iter::once(Def::Item(ModuleDef::EnumVariant(variant)))
                        .chain(variant.fields(db).into_iter().map(Def::Field))
                })
                .collect(),
            Def::Item(ModuleDef::Adt(hir::Adt::Struct(it))) => it.fields(db).into_iter().map(Def::Field).collect(),
            Def::Item(ModuleDef::Adt(hir::Adt::Union(it))) => it.fields(db).into_iter().map(Def::Field).collect(),
            Def::Item(ModuleDef::Trait(it)) => it.items(db).into_iter().map(|it| Def::Item(it.into())).collect(),
            _ => return,
        };
        let inherited = self.or_private(cx, def);
        for member in members {
            let vis = nominal(cx, member);
            if vis == private(cx, member) {
                continue;
            }
            let ev = min(cx, vis, inherited);
            let new = match self.get(member) {
                Some(old) => max(cx, old, ev),
                None => ev,
            };
            self.ev.insert(member, new);
        }
    }

    /// `EffectiveVisibility::of_impl`: the least effective visibility of the local types and
    /// traits its self type and trait name; only the outermost ones when `shallow`.
    fn impl_ev(&self, cx: &Cx<'_, '_>, imp: hir::Impl, shallow: bool) -> Vis {
        impl_defs(cx, imp, shallow).into_iter().fold(Vis::Public, |at, def| min(cx, self.or_private(cx, def), at))
    }
}

/// The local types and traits an impl's self type and trait name.
fn impl_defs(cx: &Cx<'_, '_>, imp: hir::Impl, shallow: bool) -> Vec<Def> {
    let db = cx.db;
    let mut defs = Vec::new();
    let self_ty = imp.self_ty(db);
    match shallow {
        true => {
            if let Some(adt) = self_ty.as_adt() {
                defs.push(Def::Item(ModuleDef::Adt(adt)));
            }
            if let Some(it) = self_ty.as_dyn_trait() {
                defs.push(Def::Item(ModuleDef::Trait(it)));
            }
        }
        false => self_ty.walk(db, |ty| {
            if let Some(adt) = ty.as_adt() {
                defs.push(Def::Item(ModuleDef::Adt(adt)));
            }
            if let Some(it) = ty.as_dyn_trait() {
                defs.push(Def::Item(ModuleDef::Trait(it)));
            }
        }),
    }
    if let Some(it) = imp.trait_(db) {
        defs.push(Def::Item(ModuleDef::Trait(it)));
    }
    defs.retain(|it| is_local(cx, *it));
    defs
}

pub(super) fn lints(cx: &Cx<'_, '_>, items: &[Item]) -> Vec<Found> {
    let levels = Levels::of(cx, items);
    let mut checker = Checker { cx, levels: &levels, found: Vec::new(), seen: FxHashSet::default() };
    let mut at = 0;
    while at < items.len() {
        let module = items[at].module;
        let end = items[at..].iter().position(|it| it.module != module).map_or(items.len(), |it| at + it);
        for (index, item) in items.iter().enumerate().take(end).skip(at) {
            if matches!(item.container, Container::Module) {
                checker.item(item, item.held(items, index));
            }
        }
        for item in &items[at..end] {
            if matches!(item.container, Container::Extern(_)) {
                checker.foreign_item(item);
            }
        }
        at = end;
    }
    checker.found
}

/// One `SearchInterfaceForPrivateItemsVisitor`: the item it checks and what it requires.
struct Check<'i> {
    item: Def,
    node: InFile<SyntaxNode>,
    kind: &'static str,
    /// rustc's `def_path_str` of `item`, printed when a diagnostic first needs it.
    descr: OnceCell<String>,
    required: Vis,
    required_ev: Option<Vis>,
    hard_error: bool,
    primary: bool,
    skip_assoc: bool,
    /// The item whose interface it is, for the fact the diagnostic read.
    owner: &'i Item,
}

struct Checker<'c, 'a, 'db> {
    cx: &'c Cx<'a, 'db>,
    levels: &'c Levels,
    found: Vec<Found>,
    /// rustc emits a diagnostic it already emitted once.
    seen: FxHashSet<String>,
}

impl Checker<'_, '_, '_> {
    #[allow(clippy::too_many_arguments)]
    fn check<'i>(
        &self,
        owner: &'i Item,
        item: Def,
        node: InFile<SyntaxNode>,
        kind: &'static str,
        required: Vis,
        required_ev: Option<Vis>,
    ) -> Check<'i> {
        let descr = OnceCell::new();
        Check { item, node, kind, descr, required, required_ev, hard_error: false, primary: true, skip_assoc: false, owner }
    }

    /// `check_item`, with `held` the items it holds (its own items among them).
    fn item(&mut self, item: &Item, held: &[Item]) {
        let cx = self.cx;
        let Some(def) = visibility::def(cx, item) else { return };
        let node = item.syntax();
        let vis = nominal(cx, def);
        let ev = self.levels.get(def);
        match &item.node.value {
            ast::Item::Const(_) | ast::Item::Static(_) | ast::Item::Fn(_) | ast::Item::TypeAlias(_) => {
                let mut check = self.check(item, def, node.clone(), descr_kind(item, false), vis, ev);
                self.generics(&mut check, &item.node.value);
                self.clauses(&mut check, &item.node.value);
                self.ty(&mut check, &item.node.value, None);
            }
            ast::Item::Trait(t) => {
                let mut check = self.check(item, def, node.clone(), descr_kind(item, false), vis, ev);
                self.generics(&mut check, &item.node.value);
                self.clauses(&mut check, &item.node.value);
                let own = item.node.with_value(t.clone());
                for assoc in held.iter().filter(|it| matches!(&it.container, Container::Trait(c) if *c == own)) {
                    self.assoc_item(assoc, vis, ev, true, None);
                }
            }
            ast::Item::Enum(_) => {
                let mut check = self.check(item, def, node.clone(), descr_kind(item, false), vis, ev);
                self.generics(&mut check, &item.node.value);
                self.clauses(&mut check, &item.node.value);
                for field in walk::fields(item) {
                    self.field(item, field, |_| (vis, ev));
                }
            }
            ast::Item::Struct(_) | ast::Item::Union(_) => {
                let mut check = self.check(item, def, node.clone(), descr_kind(item, false), vis, ev);
                self.generics(&mut check, &item.node.value);
                self.clauses(&mut check, &item.node.value);
                let levels = self.levels;
                for field in walk::fields(item) {
                    self.field(item, field, |field| (min(cx, vis, nominal(cx, field)), levels.get(field)));
                }
            }
            ast::Item::Impl(imp) => {
                let Def::Impl(id) = def else { return };
                let of_trait = imp.trait_().is_some();
                let defs = impl_defs(cx, id, false);
                let impl_vis = defs.iter().fold(Vis::Public, |at, it| min(cx, nominal(cx, *it), at));
                let impl_ev = self.levels.impl_ev(cx, id, false);
                let mut check = self.check(item, def, node.clone(), "implementation", impl_vis, Some(impl_ev));
                if !of_trait {
                    self.generics(&mut check, &item.node.value);
                    self.clauses(&mut check, &item.node.value);
                }
                check.skip_assoc = true;
                check.primary = true;
                let mut aliases = FxHashSet::default();
                if let Some(ty) = imp.self_ty() {
                    self.visit(&check, item.node.with_value(ty.syntax().clone()), false, &mut aliases);
                }
                if let Some(tr) = imp.trait_() {
                    self.visit(&check, item.node.with_value(tr.syntax().clone()), false, &mut aliases);
                }
                let own = item.node.with_value(imp.clone());
                for assoc in held.iter().filter(|it| matches!(&it.container, Container::Impl(c) if *c == own)) {
                    let Some(assoc_def) = visibility::def(cx, assoc) else { continue };
                    let (vis, ev) = match of_trait {
                        false => (
                            min(cx, nominal(cx, assoc_def), impl_vis),
                            self.levels.get(assoc_def).map(|ev| min(cx, ev, impl_ev)),
                        ),
                        true => (impl_vis, Some(impl_ev)),
                    };
                    self.assoc_item(assoc, vis, ev, false, imp.self_ty());
                }
            }
            _ => {}
        }
    }

    /// `check_foreign_item`.
    fn foreign_item(&mut self, item: &Item) {
        let Some(def) = visibility::def(self.cx, item) else { return };
        let vis = nominal(self.cx, def);
        let ev = self.levels.get(def);
        let mut check = self.check(item, def, item.syntax(), descr_kind(item, false), vis, ev);
        self.generics(&mut check, &item.node.value);
        self.clauses(&mut check, &item.node.value);
        self.ty(&mut check, &item.node.value, None);
    }

    fn field(&mut self, owner: &Item, field: InFile<SyntaxNode>, required: impl FnOnce(Def) -> (Vis, Option<Vis>)) {
        let cx = self.cx;
        let def = match ast::RecordField::cast(field.value.clone()) {
            Some(it) => cx.sema.to_def2(field.with_value(&it)),
            None => ast::TupleField::cast(field.value.clone()).and_then(|it| cx.sema.to_def2(field.with_value(&it))),
        };
        let Some(def) = def else { return };
        let def = Def::Field(def);
        let (vis, ev) = required(def);
        let mut check = self.check(owner, def, field.clone(), "field", vis, ev);
        check.primary = true;
        let ty = field.value.children().find_map(ast::Type::cast);
        if let Some(ty) = ty {
            self.visit(&check, field.with_value(ty.syntax().clone()), false, &mut FxHashSet::default());
        }
    }

    /// `check_assoc_item`: of a trait (`in_trait`) or of an impl whose self type is `impl_self`.
    fn assoc_item(&mut self, item: &Item, vis: Vis, ev: Option<Vis>, in_trait: bool, impl_self: Option<ast::Type>) {
        let cx = self.cx;
        let Some(def) = visibility::def(cx, item) else { return };
        let is_assoc_ty = matches!(item.node.value, ast::Item::TypeAlias(_));
        let mut check = self.check(item, def, item.syntax(), descr_kind(item, true), vis, ev);
        check.hard_error = is_assoc_ty;
        self.generics(&mut check, &item.node.value);
        self.clauses(&mut check, &item.node.value);
        let has_type_of = match &item.node.value {
            ast::Item::TypeAlias(it) => !in_trait || it.ty().is_some(),
            _ => true,
        };
        if has_type_of {
            let impl_self = impl_self.map(|it| item.node.with_value(it.syntax().clone()));
            self.ty(&mut check, &item.node.value, impl_self);
        }
        if is_assoc_ty && in_trait {
            check.hard_error = false;
            check.primary = false;
            if let ast::Item::TypeAlias(it) = &item.node.value {
                let mut aliases = FxHashSet::default();
                for bound in it.type_bound_list().into_iter().flat_map(|it| it.bounds()) {
                    self.visit(&check, item.node.with_value(bound.syntax().clone()), false, &mut aliases);
                }
            }
        }
    }

    /// `generics()`: the types of const parameters and the defaults of parameters.
    fn generics(&mut self, check: &mut Check<'_>, node: &ast::Item) {
        check.primary = true;
        let Some(params) = node.syntax().children().find_map(ast::GenericParamList::cast) else { return };
        let file = check.node.file_id;
        let mut aliases = FxHashSet::default();
        for param in params.generic_params() {
            let ty = match &param {
                ast::GenericParam::ConstParam(it) => it.ty(),
                ast::GenericParam::TypeParam(it) => it.default_type(),
                ast::GenericParam::LifetimeParam(_) => None,
            };
            if let Some(ty) = ty {
                self.visit(check, InFile::new(file, ty.syntax().clone()), false, &mut aliases);
            }
        }
    }

    /// `clauses()`: a trait's supertraits, the bounds of parameters, the `where` clause, the
    /// bounds of `impl Trait` parameters; each bound's trait before the type it bounds.
    fn clauses(&mut self, check: &mut Check<'_>, node: &ast::Item) {
        check.primary = false;
        let file = check.node.file_id;
        let mut aliases = FxHashSet::default();
        let at = |it: &SyntaxNode| InFile::new(file, it.clone());
        if let ast::Item::Trait(it) = node {
            for bound in it.type_bound_list().into_iter().flat_map(|it| it.bounds()) {
                self.visit(check, at(bound.syntax()), false, &mut aliases);
            }
        }
        let params = node.syntax().children().find_map(ast::GenericParamList::cast);
        for param in params.iter().flat_map(|it| it.generic_params()) {
            if let ast::GenericParam::TypeParam(param) = param {
                for bound in param.type_bound_list().into_iter().flat_map(|it| it.bounds()) {
                    self.visit(check, at(bound.syntax()), false, &mut aliases);
                }
            }
        }
        let clause = node.syntax().children().find_map(ast::WhereClause::cast);
        for pred in clause.iter().flat_map(|it| it.predicates()) {
            let Some(ty) = pred.ty() else { continue };
            for bound in pred.type_bound_list().into_iter().flat_map(|it| it.bounds()) {
                if bound.lifetime().is_none() {
                    self.visit(check, at(bound.syntax()), false, &mut aliases);
                }
                self.visit(check, at(ty.syntax()), false, &mut aliases);
            }
        }
        if let ast::Item::Fn(f) = node {
            for ty in f.param_list().into_iter().flat_map(|it| it.params()).filter_map(|it| it.ty()) {
                for apit in ty.syntax().descendants().filter_map(ast::ImplTraitType::cast) {
                    for bound in apit.type_bound_list().into_iter().flat_map(|it| it.bounds()) {
                        self.visit(check, at(bound.syntax()), false, &mut aliases);
                    }
                }
            }
        }
    }

    /// `ty()`: a function's signature (and its impl's self type), else the item's type.
    fn ty(&mut self, check: &mut Check<'_>, node: &ast::Item, impl_self: Option<InFile<SyntaxNode>>) {
        check.primary = true;
        let file = check.node.file_id;
        let mut aliases = FxHashSet::default();
        let at = |it: &SyntaxNode| InFile::new(file, it.clone());
        match node {
            ast::Item::Fn(f) => {
                if let Some(list) = f.param_list() {
                    if let Some(ty) = list.self_param().and_then(|it| it.ty()) {
                        self.visit(check, at(ty.syntax()), true, &mut aliases);
                    }
                    for ty in list.params().filter_map(|it| it.ty()) {
                        self.visit(check, at(ty.syntax()), true, &mut aliases);
                    }
                }
                if let Some(ty) = f.ret_type().and_then(|it| it.ty()) {
                    self.visit(check, at(ty.syntax()), false, &mut aliases);
                }
                if let Some(ty) = impl_self {
                    self.visit(check, ty, false, &mut aliases);
                }
            }
            ast::Item::Const(it) => {
                if let Some(ty) = it.ty() {
                    self.visit(check, at(ty.syntax()), false, &mut aliases);
                }
            }
            ast::Item::Static(it) => {
                if let Some(ty) = it.ty() {
                    self.visit(check, at(ty.syntax()), false, &mut aliases);
                }
            }
            ast::Item::TypeAlias(it) => {
                if let Some(ty) = it.ty() {
                    self.visit(check, at(ty.syntax()), false, &mut aliases);
                }
            }
            _ => {}
        }
    }

    /// The defs a type (or a bound) names, in the order rustc's skeleton visits them.
    fn visit(&mut self, check: &Check<'_>, node: InFile<SyntaxNode>, skip_impl_trait: bool, aliases: &mut FxHashSet<hir::TypeAlias>) {
        let cx = self.cx;
        let mut walk = node.value.preorder();
        while let Some(event) = walk.next() {
            let syntax::WalkEvent::Enter(it) = event else { continue };
            let kind = it.kind();
            if kind == SyntaxKind::CONST_ARG || ast::Expr::can_cast(kind) {
                walk.skip_subtree();
                continue;
            }
            if skip_impl_trait && kind == SyntaxKind::IMPL_TRAIT_TYPE {
                walk.skip_subtree();
                continue;
            }
            let Some(ty) = ast::PathType::cast(it) else { continue };
            let Some(path) = ty.path() else { continue };
            match cx.sema.resolve_path(&path) {
                Some(PathResolution::Def(ModuleDef::TypeAlias(alias))) => {
                    walk.skip_subtree();
                    if alias.as_assoc_item(cx.db).is_some() {
                        if !check.skip_assoc {
                            self.projection(check, node.with_value(path), alias, aliases);
                        }
                        continue;
                    }
                    if aliases.insert(alias)
                        && let Some(src) = alias.source(cx.db)
                    {
                        let alias_node = cx.node(src.as_ref().map(AstPtr::new));
                        if let Some(aliased) = alias_node.value.ty() {
                            self.visit(check, alias_node.with_value(aliased.syntax().clone()), skip_impl_trait, aliases);
                        }
                    }
                    if let Some(args) = path.segment().and_then(|it| it.generic_arg_list()) {
                        self.visit(check, node.with_value(args.syntax().clone()), skip_impl_trait, aliases);
                    }
                }
                Some(PathResolution::Def(def @ ModuleDef::Adt(_))) => {
                    let descr = || match cx.sema.resolve_type(&ast::Type::PathType(ty.clone())) {
                        Some(it) => cx.ty(&it),
                        None => cx.def_path(def),
                    };
                    self.def_id(check, Def::Item(def), "type", descr);
                }
                Some(PathResolution::Def(ModuleDef::Trait(tr))) => {
                    self.def_id(check, Def::Item(ModuleDef::Trait(tr)), "trait", || trait_descr(cx, tr, &path));
                }
                _ => {}
            }
        }
    }

    /// `<T as Trait>::Assoc` (or `T::Assoc`): the associated type, its trait, then `T` and the
    /// trait's arguments.
    fn projection(&mut self, check: &Check<'_>, path: InFile<ast::Path>, alias: hir::TypeAlias, aliases: &mut FxHashSet<hir::TypeAlias>) {
        let cx = self.cx;
        let def = ModuleDef::TypeAlias(alias);
        self.def_id(check, Def::Item(def), "associated type", || cx.def_path(def));
        let Some(AssocItemContainer::Trait(tr)) = alias.as_assoc_item(cx.db).map(|it| it.container(cx.db)) else {
            return;
        };
        let anchor = path.value.qualifier().and_then(|it| it.segment()).and_then(|it| it.type_anchor());
        let types: Vec<ast::Type> = anchor.iter().flat_map(|it| it.syntax().children().filter_map(ast::Type::cast)).collect();
        let trait_path = match types.get(1) {
            Some(ast::Type::PathType(it)) => it.path(),
            _ => None,
        };
        let descr = || match &trait_path {
            Some(it) => trait_descr(cx, tr, it),
            None => cx.def_path(ModuleDef::Trait(tr)),
        };
        self.def_id(check, Def::Item(ModuleDef::Trait(tr)), "trait", descr);
        if let Some(self_ty) = types.first() {
            self.visit(check, path.with_value(self_ty.syntax().clone()), false, aliases);
        }
        if let Some(args) = trait_path.and_then(|it| it.segment()).and_then(|it| it.generic_arg_list()) {
            self.visit(check, path.with_value(args.syntax().clone()), false, aliases);
        }
    }

    /// `check_def_id`: the error or the lint of a def named in the interface of `check.item`.
    /// What a lint must print is printed only once it fires.
    fn def_id(&mut self, check: &Check<'_>, def: Def, kind: &str, descr: impl FnOnce() -> String) {
        let cx = self.cx;
        if !is_local(cx, def) {
            return;
        }
        let vis = nominal(cx, def);
        let hard_error = check.hard_error && greater(cx, check.required, vis);
        if !hard_error && !check.required_ev.is_some_and(|ev| greater(cx, ev, vis)) {
            return;
        }
        let Some(def_node) = source(cx, def) else { return };
        let item_span = cx.span(check.node.as_ref().map(def_span));
        let vis_span = cx.span(def_node.as_ref().map(def_span));
        let descr = descr();
        if hard_error {
            let vis_descr = match vis {
                Vis::Public => "public",
                Vis::In(module) if Some(module) == module_of(cx, def) => "private",
                Vis::In(module) if module.is_crate_root(cx.db) => "crate-private",
                Vis::In(_) => "restricted",
            };
            let message = format!("{vis_descr} {kind} `{descr}` in public interface");
            let mut diag = Diag::error(Some("E0446"), message, item_span.clone());
            diag.span_label(item_span, format!("can't leak {vis_descr} {kind}"));
            diag.span_label(vis_span, format!("`{descr}` declared as {vis_descr}"));
            self.fact(check, &mut diag);
            self.push(Found::new(diag, Phase::Late));
            return;
        }
        let Some(ev) = check.required_ev else { return };
        let item_descr = check.descr.get_or_init(|| self.descr(check.item));
        let lint = if check.primary { "private_interfaces" } else { "private_bounds" };
        let message = format!("{kind} `{descr}` is more private than the item `{item_descr}`");
        let mut diag = Diag::lint(lint, message, item_span.clone());
        let item_vis = vis_str(cx, ev, check.item);
        diag.span_label(item_span, format!("{} `{item_descr}` is reachable at visibility `{item_vis}`", check.kind));
        let def_vis = vis_str(cx, vis, def);
        diag.span_note(vis_span, format!("but {kind} `{descr}` is only usable at visibility `{def_vis}`"));
        self.fact(check, &mut diag);
        let at = check.node.as_ref().map(SyntaxNodePtr::new);
        self.push(Found::lint(diag, Phase::Late, at));
    }

    fn fact(&self, check: &Check<'_>, diag: &mut Diag) {
        if let Some((id, name)) = super::item_id(self.cx, check.owner) {
            diag.fact(Fact::Signature(self.cx.key(id)), format!("the interface of `{name}`"));
        }
    }

    fn push(&mut self, found: Found) {
        let diag = &found.diag;
        let key = format!("{}|{}|{:?}|{:?}", diag.message, diag.code.as_ref().map_or("", |it| it.as_str()), diag.span, diag.children);
        if self.seen.insert(key) {
            self.found.push(found);
        }
    }

    /// rustc's `def_path_str` of the item checked: a field by its parent's, an impl as its type
    /// (or `<Type as Trait>`).
    fn descr(&self, def: Def) -> String {
        let cx = self.cx;
        let db = cx.db;
        match def {
            Def::Item(it) => cx.def_path(it),
            Def::Field(field) => {
                let parent = match field.parent_def(db) {
                    hir::Variant::Struct(it) => cx.def_path(ModuleDef::Adt(it.into())),
                    hir::Variant::Union(it) => cx.def_path(ModuleDef::Adt(it.into())),
                    hir::Variant::EnumVariant(it) => cx.def_path(ModuleDef::EnumVariant(it)),
                };
                format!("{parent}::{}", field.name(db).as_str())
            }
            Def::Impl(imp) => {
                let self_ty = cx.ty(&imp.self_ty(db));
                match imp.trait_(db) {
                    Some(tr) => format!("<{self_ty} as {}>", cx.def_path(ModuleDef::Trait(tr))),
                    None => self_ty,
                }
            }
        }
    }
}

/// `print_only_trait_path`: the trait's path and the arguments written to it.
fn trait_descr(cx: &Cx<'_, '_>, tr: hir::Trait, path: &ast::Path) -> String {
    let mut descr = cx.def_path(ModuleDef::Trait(tr));
    let Some(args) = path.segment().and_then(|it| it.generic_arg_list()) else { return descr };
    let printed: Vec<String> = args
        .generic_args()
        .map(|arg| match &arg {
            ast::GenericArg::TypeArg(it) => match it.ty().and_then(|ty| cx.sema.resolve_type(&ty)) {
                Some(ty) => cx.ty(&ty),
                None => arg.syntax().text().to_string(),
            },
            _ => arg.syntax().text().to_string(),
        })
        .collect();
    if !printed.is_empty() {
        descr.push('<');
        descr.push_str(&printed.join(", "));
        descr.push('>');
    }
    descr
}

/// rustc's `def_descr` of a walked item, `assoc` when it is an item of a trait or an impl.
fn descr_kind(item: &Item, assoc: bool) -> &'static str {
    match &item.node.value {
        ast::Item::Fn(it) if assoc => match it.param_list().and_then(|it| it.self_param()) {
            Some(_) => "method",
            None => "associated function",
        },
        ast::Item::Fn(_) => "function",
        ast::Item::Const(_) if assoc => "associated constant",
        ast::Item::Const(_) => "constant",
        ast::Item::Static(_) => "static",
        ast::Item::TypeAlias(_) if assoc => "associated type",
        ast::Item::TypeAlias(_) if matches!(item.container, Container::Extern(_)) => "foreign type",
        ast::Item::TypeAlias(_) => "type alias",
        ast::Item::Struct(_) => "struct",
        ast::Item::Enum(_) => "enum",
        ast::Item::Union(_) => "union",
        ast::Item::Trait(it) if it.eq_token().is_some() => "trait alias",
        ast::Item::Trait(_) => "trait",
        ast::Item::Impl(_) => "implementation",
        _ => "item",
    }
}

/// The syntax of a def, for its `def_span`.
fn source(cx: &Cx<'_, '_>, def: Def) -> Option<InFile<SyntaxNode>> {
    fn of<N: AstNode>(cx: &Cx<'_, '_>, src: Option<InFile<N>>) -> Option<InFile<SyntaxNode>> {
        let src = src?;
        Some(cx.node(src.as_ref().map(AstPtr::new)).map(|it| it.syntax().clone()))
    }
    let db = cx.db;
    match def {
        Def::Item(ModuleDef::EnumVariant(it)) => of(cx, it.source(db)),
        Def::Item(ModuleDef::Module(it)) => it.declaration_source(db).map(|it| it.map(|it| it.syntax().clone())),
        _ => visibility::interface_node(cx, def),
    }
}

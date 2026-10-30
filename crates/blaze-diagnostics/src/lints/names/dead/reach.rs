//! The definitions public at rustc's `Level::Reachable` (`effective_visibilities`, as far as the
//! dead-code pass asks: `is_public_at_level(Reachable)` and `is_reachable`): what another crate
//! can name or reach. A definition is public there when its own visibility is `pub` (a variant's
//! and a trait item's are their parent's; a trait impl's items have none of their own) and it is
//! named by a public path from the crate root (an item or a `pub use` of a public module), or
//! named in the interface of a definition that is (its signature's types, its bounds, its
//! supertraits, its fields' types), or it is an impl whose self type and trait are, or an item of
//! one. Each definition is public at all levels or at none of them below `Reachable` here: a
//! level is the least of its visibility and its parent's, and only `pub` is public.
//!
//! Both sets are bitsets over the definitions; the impls' fixpoint reads what each impl is of
//! from `Defs`, resolved once, so a round costs a few bit tests per impl not yet public.
//!
//! Follows `compiler/rustc_resolve/src/effective_visibilities.rs` (the public paths),
//! `compiler/rustc_privacy/src/lib.rs` (`EmbargoVisitor::check_def_id`,
//! `ReachEverythingInTheInterfaceVisitor`, `EffectiveVisibility::of_impl`) and
//! `compiler/rustc_middle/src/middle/privacy.rs` (`EffectiveVisibilities::update`)
//! (MIT or Apache-2.0). Not followed: what an `impl Trait` reaches
//! (`ReachableThroughImplTrait`, below `Reachable`), and the where clauses of an item beyond the
//! bounds of its parameters (`v2/requests/diagnostics.md`).

use hir::GenericDef;
use hir_def::{AdtId, ModuleDefId, ModuleId, item_scope::ItemScope, visibility::Visibility};
use rustc_hash::{FxHashMap, FxHashSet};
use syntax::{
    AstNode,
    ast::{self, HasVisibility as _, VisibilityKind},
};

use super::defs::{Bits, DefIdx, Defs, Id, ImplOf, Kind, Parent};
use crate::cx::Cx;

pub(super) struct Reach {
    public: Bits,
}

impl Reach {
    pub(super) fn of(cx: &Cx<'_, '_>, defs: &Defs) -> Reach {
        let mut walk = Walk {
            cx,
            defs,
            nominal: nominal_pub(cx, defs),
            public: Bits::new(defs.len()),
            queue: Vec::new(),
            named: Vec::new(),
        };
        walk.paths();
        walk.interfaces();
        Reach { public: walk.public }
    }

    /// rustc's `is_public_at_level(def, Level::Reachable)`, and `is_reachable`.
    pub(super) fn is_reachable(&self, def: DefIdx) -> bool {
        self.public.get(def)
    }
}

struct Walk<'c, 'a, 'db> {
    cx: &'c Cx<'a, 'db>,
    defs: &'c Defs,
    /// Whether a definition's own visibility is `pub`: the most its level can be.
    nominal: Bits,
    public: Bits,
    /// Definitions made public whose interfaces are still to be walked.
    queue: Vec<DefIdx>,
    /// The definitions a type names, reused from type to type.
    named: Vec<Id>,
}

impl Walk<'_, '_, '_> {
    /// rustc's `update` of `def` from a public parent: public when its own visibility allows.
    fn update(&mut self, def: DefIdx) {
        if self.nominal.get(def) {
            self.set(def);
        }
    }

    /// `update_eff_vis` with no visibility of its own (`max_vis: None`).
    fn set(&mut self, def: DefIdx) {
        if self.public.insert(def) {
            self.queue.push(def);
        }
    }

    fn update_id(&mut self, id: Id) {
        if let Some(def) = self.defs.get(id) {
            self.update(def);
        }
    }

    /// The items and `use`s named by public paths: from the crate root, each public name of a
    /// public module, the modules among them walked in turn.
    fn paths(&mut self) {
        let db = self.cx.db;
        let defs = self.defs;
        let index: FxHashMap<hir::Module, u32> =
            defs.module_ids.iter().enumerate().map(|(at, &module)| (module, at as u32)).collect();
        let mut public_modules = vec![false; defs.module_ids.len()];
        let root: ModuleId = self.cx.krate.root_module(db).into();
        let mut seen: FxHashSet<ModuleId> = FxHashSet::default();
        let mut stack = vec![root];
        seen.insert(root);
        while let Some(module) = stack.pop() {
            if let Some(&at) = index.get(&hir::Module::from(module)) {
                public_modules[at as usize] = true;
            }
            let scope: &ItemScope = module.def_map(db).module(db, module).scope(db);
            for (_, per_ns) in scope.entries() {
                let types = per_ns.types.filter(|it| it.vis == Visibility::Public).map(|it| it.def);
                let values = per_ns.values.filter(|it| it.vis == Visibility::Public).map(|it| it.def);
                for def in types.into_iter().chain(values) {
                    match def {
                        ModuleDefId::ModuleId(it) => {
                            let local = hir::Module::from(it).krate(db) == self.cx.krate;
                            if local && seen.insert(it) {
                                stack.push(it);
                            }
                        }
                        _ => {
                            if let Some(id) = module_def_id(def) {
                                self.update_id(id);
                            }
                            if let ModuleDefId::EnumVariantId(it) = def
                                && let Some(variant) = defs.get(Id::Variant(it))
                                && let Parent::Def(adt) = defs[variant].parent
                            {
                                self.update(adt);
                            }
                        }
                    }
                }
            }
        }
        for def in defs.all() {
            if defs[def].kind == Kind::Use
                && defs[def].parent == Parent::Module
                && public_modules[defs.module_of[def as usize] as usize]
            {
                self.update(def);
            }
        }
    }

    /// `EmbargoVisitor`'s fixed point: the interface of each public definition, and the impls
    /// whose self type and trait are public, until nothing changes.
    fn interfaces(&mut self) {
        let mut pending: Vec<ImplOf> = self.defs.impls().to_vec();
        loop {
            while let Some(def) = self.queue.pop() {
                self.check(def);
            }
            pending.retain(|it| {
                if self.public.get(it.def) {
                    return false;
                }
                let public = |part: Option<DefIdx>| part.is_none_or(|it| self.public.get(it));
                if public(it.self_adt) && public(it.self_dyn) && public(it.trait_) {
                    if self.public.insert(it.def) {
                        self.queue.push(it.def);
                    }
                    return false;
                }
                true
            });
            if self.queue.is_empty() {
                break;
            }
        }
    }

    /// rustc's `check_def_id` of a public definition: what its public parts and interface reach.
    fn check(&mut self, def: DefIdx) {
        let db = self.cx.db;
        let defs = self.defs;
        let d = &defs[def];
        match (d.id, d.kind) {
            (Id::Fn(id), _) => {
                let function = hir::Function::from(id);
                self.generics(GenericDef::Function(function));
                for param in function.assoc_fn_params(db) {
                    self.ty(param.ty());
                }
                self.ty(&function.ret_type(db));
            }
            (Id::Const(id), _) => {
                let it = hir::Const::from(id);
                self.ty(&it.ty(db));
            }
            (Id::Static(id), _) => self.ty(&hir::Static::from(id).ty(db)),
            (Id::Alias(id), _) => {
                let it = hir::TypeAlias::from(id);
                self.generics(GenericDef::TypeAlias(it));
                self.ty(&it.ty(db));
            }
            (Id::Trait(id), _) => {
                let trait_ = hir::Trait::from(id);
                self.generics(GenericDef::Trait(trait_));
                for it in trait_.direct_supertraits(db) {
                    self.trait_(it);
                }
                for &item in defs.children(def) {
                    self.update(item);
                }
            }
            (Id::Impl(id), _) => {
                let impl_ = hir::Impl::from(id);
                self.generics(GenericDef::Impl(impl_));
                self.ty(&impl_.self_ty(db));
                if let Some(it) = impl_.trait_(db) {
                    self.trait_(it);
                }
                self.impl_items(def);
            }
            (Id::Derive(_), _) => self.impl_items(def),
            (Id::Adt(adt), Kind::Enum) => {
                self.generics(GenericDef::Adt(adt.into()));
                for &variant in defs.children(def) {
                    self.update(variant);
                }
            }
            (Id::Adt(adt), _) => {
                self.generics(GenericDef::Adt(adt.into()));
                for &field in defs.children(def) {
                    self.update(field);
                }
            }
            (Id::Variant(_), _) => {
                for &field in defs.children(def) {
                    self.update(field);
                }
            }
            (Id::Field(id), _) => self.ty(&hir::Field::from(id).ty(db)),
            (Id::DeriveMethod(_) | Id::Use(_), _) => {}
        }
    }

    /// The items of a public impl: of a trait impl, all of them; of an inherent one, its `pub`
    /// ones.
    fn impl_items(&mut self, impl_: DefIdx) {
        let defs = self.defs;
        let of_trait = matches!(defs[impl_].kind, Kind::Impl { of_trait: true });
        for &item in defs.children(impl_) {
            match of_trait {
                true => self.set(item),
                false => self.update(item),
            }
        }
    }

    /// The bounds and defaults of the type parameters of `def`, and the types of its const
    /// parameters.
    fn generics(&mut self, def: GenericDef) {
        let db = self.cx.db;
        for param in def.type_or_const_params(db) {
            match param.as_type_param(db) {
                Some(it) => {
                    for bound in it.trait_bounds(db) {
                        self.trait_(bound);
                    }
                    if let Some(default) = it.default(db) {
                        self.ty(&default);
                    }
                }
                None => self.ty(&param.ty(db)),
            }
        }
    }

    fn trait_(&mut self, trait_: hir::Trait) {
        self.update_id(Id::Trait(trait_.into()));
    }

    /// The definitions a type names: its ADTs, its `dyn` traits, the traits of its projections
    /// and its `impl Trait`s, in its arguments too.
    fn ty(&mut self, ty: &hir::Type<'_>) {
        let db = self.cx.db;
        let mut named = std::mem::take(&mut self.named);
        ty.walk(db, |it| {
            if let Some(adt) = it.as_adt() {
                named.push(Id::Adt(AdtId::from(adt)));
            }
            if let Some(trait_) = it.as_dyn_trait() {
                named.push(Id::Trait(trait_.into()));
            }
            if let Some(trait_) = it.as_associated_type_parent_trait(db) {
                named.push(Id::Trait(trait_.into()));
            }
            if let Some(traits) = it.as_impl_traits(db) {
                named.extend(traits.map(|it| Id::Trait(it.into())));
            }
        });
        for id in named.drain(..) {
            self.update_id(id);
        }
        self.named = named;
    }
}

/// The definition a name of a module's scope stands for, if the pass has one for it.
fn module_def_id(def: ModuleDefId) -> Option<Id> {
    Some(match def {
        ModuleDefId::FunctionId(it) => Id::Fn(it),
        ModuleDefId::AdtId(it) => Id::Adt(it),
        ModuleDefId::EnumVariantId(it) => Id::Variant(it),
        ModuleDefId::ConstId(it) => Id::Const(it),
        ModuleDefId::StaticId(it) => Id::Static(it),
        ModuleDefId::TraitId(it) => Id::Trait(it),
        ModuleDefId::TypeAliasId(it) => Id::Alias(it),
        ModuleDefId::ModuleId(_) | ModuleDefId::BuiltinType(_) | ModuleDefId::MacroId(_) => return None,
    })
}

/// Of each definition, whether the visibility it has of its own is `pub`: rustc's
/// `local_visibility` is `Visibility::Public`. A variant and its fields have their enum's, a
/// trait's item its trait's; an impl and a trait impl's items have none of their own
/// (`max_vis: None`). One pass in definition order: a parent is numbered before its children.
fn nominal_pub(cx: &Cx<'_, '_>, defs: &Defs) -> Bits {
    let mut nominal = Bits::new(defs.len());
    for def in defs.all() {
        let d = &defs[def];
        let parent = match d.parent {
            Parent::Def(it) => Some(it),
            Parent::Module | Parent::Anon(_) => None,
        };
        debug_assert!(parent.is_none_or(|it| it < def), "a parent is numbered before its children");
        let inherited = parent.is_some_and(|it| nominal.get(it));
        let public = match (d.kind, parent.map(|it| defs[it].kind)) {
            (Kind::Variant, _) | (Kind::Field, Some(Kind::Variant)) | (_, Some(Kind::Trait)) => inherited,
            (_, Some(Kind::Impl { of_trait: true })) | (Kind::Impl { .. }, _) => true,
            _ => d.node.is_some_and(|node| {
                let node = cx.syntax(node);
                let vis = ast::AnyHasVisibility::cast(node.value).and_then(|it| it.visibility());
                vis.is_some_and(|it| matches!(it.kind(), VisibilityKind::Pub))
            }),
        };
        if public {
            nominal.insert(def);
        }
    }
    nominal
}

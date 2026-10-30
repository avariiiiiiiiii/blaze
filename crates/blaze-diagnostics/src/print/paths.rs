//! What rustc names a definition by: its definitions and their parents (`def_key`), the
//! children a module, enum or trait shows (`module_children`), the names it trims paths to
//! (`trimmed_def_paths`) and the parents it reaches a dependency's items through
//! (`visible_parent_map`). Ported from `compiler/rustc_middle/src/ty/print/pretty.rs` and
//! `compiler/rustc_metadata/src/rmeta/decoder/cstore_impl.rs` (MIT or Apache-2.0).

use std::collections::VecDeque;

use base_db::{CrateOrigin, LangCrateOrigin};
use hir_def::{
    AdtId, AssocItemId, AttrDefId, BlockId, ConstId, DefWithBodyId, EnumVariantId,
    ExternCrateId, FunctionId, HasModule, ImplId, ItemContainerId, Lookup, MacroId, ModuleDefId,
    ModuleId, StaticId, TraitId, TypeAliasId,
    attrs::AttrFlags,
    expr_store::Body,
    item_scope::{ImportOrExternCrate, ImportOrGlob, ItemScope},
    nameres::{block_def_map, crate_def_map},
    signatures::FunctionSignature,
    visibility::Visibility,
};
use hir_expand::name::Name;
use rustc_hash::{FxHashMap, FxHashSet};
use syntax::{AstNode, SyntaxKind, SyntaxNode, ast};

use crate::cx::Cx;

/// A definition a path names: rustc's `DefId`, of the kinds diagnostics print.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Def {
    Module(ModuleId),
    Adt(AdtId),
    Variant(EnumVariantId),
    Function(FunctionId),
    Const(ConstId),
    Static(StaticId),
    Trait(TraitId),
    TypeAlias(TypeAliasId),
    Macro(MacroId),
    Impl(ImplId),
    ExternCrate(ExternCrateId),
}

impl Def {
    /// None for a primitive type, which has no definition.
    pub(crate) fn of(def: ModuleDefId) -> Option<Def> {
        Some(match def {
            ModuleDefId::ModuleId(it) => Def::Module(it),
            ModuleDefId::FunctionId(it) => Def::Function(it),
            ModuleDefId::AdtId(it) => Def::Adt(it),
            ModuleDefId::EnumVariantId(it) => Def::Variant(it),
            ModuleDefId::ConstId(it) => Def::Const(it),
            ModuleDefId::StaticId(it) => Def::Static(it),
            ModuleDefId::TraitId(it) => Def::Trait(it),
            ModuleDefId::TypeAliasId(it) => Def::TypeAlias(it),
            ModuleDefId::MacroId(it) => Def::Macro(it),
            ModuleDefId::BuiltinType(_) => return None,
        })
    }

    fn of_assoc(item: AssocItemId) -> Def {
        match item {
            AssocItemId::FunctionId(it) => Def::Function(it),
            AssocItemId::ConstId(it) => Def::Const(it),
            AssocItemId::TypeAliasId(it) => Def::TypeAlias(it),
        }
    }

    fn attr_owner(self) -> AttrDefId {
        match self {
            Def::Module(it) => AttrDefId::ModuleId(it),
            Def::Adt(it) => AttrDefId::AdtId(it),
            Def::Variant(it) => AttrDefId::EnumVariantId(it),
            Def::Function(it) => AttrDefId::FunctionId(it),
            Def::Const(it) => AttrDefId::ConstId(it),
            Def::Static(it) => AttrDefId::StaticId(it),
            Def::Trait(it) => AttrDefId::TraitId(it),
            Def::TypeAlias(it) => AttrDefId::TypeAliasId(it),
            Def::Macro(it) => AttrDefId::MacroId(it),
            Def::Impl(it) => AttrDefId::ImplId(it),
            Def::ExternCrate(it) => AttrDefId::ExternCrateId(it),
        }
    }

    /// The module the definition is written in (itself, for a module).
    pub(crate) fn module(self, cx: &Cx<'_, '_>) -> ModuleId {
        self.attr_owner().module(cx.db)
    }

    pub(crate) fn krate(self, cx: &Cx<'_, '_>) -> base_db::Crate {
        self.module(cx).krate(cx.db)
    }

    pub(crate) fn is_local(self, cx: &Cx<'_, '_>) -> bool {
        hir::Crate::from(self.krate(cx)) == cx.krate
    }

    pub(crate) fn is_doc_hidden(self, cx: &Cx<'_, '_>) -> bool {
        AttrFlags::query(cx.db, self.attr_owner()).contains(AttrFlags::IS_DOC_HIDDEN)
    }

    /// rustc's `DefKind::ns`: None for an impl.
    fn ns(self) -> Option<Ns> {
        match self {
            Def::Module(_)
            | Def::Adt(_)
            | Def::Variant(_)
            | Def::Trait(_)
            | Def::TypeAlias(_)
            | Def::ExternCrate(_) => Some(Ns::Type),
            Def::Function(_) | Def::Const(_) | Def::Static(_) => Some(Ns::Value),
            Def::Macro(_) => Some(Ns::Macro),
            Def::Impl(_) => None,
        }
    }

    /// rustc's `DefKind::is_module_like`, with the enums and traits whose children
    /// `module_children` lists.
    fn is_module_like(self) -> bool {
        matches!(self, Def::Module(_) | Def::Adt(AdtId::EnumId(_)) | Def::Trait(_))
    }

    /// Whether it is in value namespace: what `def_path_str` prints with `::<..>`.
    pub(crate) fn is_value(self) -> bool {
        self.ns() == Some(Ns::Value)
    }

    /// Whether its path segment is rustc's `DefPathData::TypeNs`, the segments a re-export
    /// can rename.
    pub(crate) fn is_type_ns(self) -> bool {
        self.ns() == Some(Ns::Type)
    }

    /// The name of the definition's path segment; None for a crate root and an impl.
    pub(crate) fn name(self, cx: &Cx<'_, '_>) -> Option<Name> {
        let db = cx.db;
        match self {
            Def::Module(it) => it.name(db),
            Def::Impl(_) => None,
            Def::ExternCrate(it) => hir::ExternCrateDecl::from(it).alias_or_name(db),
            Def::Variant(it) => Some(it.lookup(db).name.clone()),
            Def::Macro(it) => Some(hir::Macro::from(it).name(db)),
            Def::Adt(it) => Some(hir::Adt::from(it).name(db)),
            Def::Function(it) => Some(FunctionSignature::of(db, it).name.clone()),
            Def::Const(it) => hir::Const::from(it).name(db),
            Def::Static(it) => Some(hir::Static::from(it).name(db)),
            Def::Trait(it) => Some(hir::Trait::from(it).name(db)),
            Def::TypeAlias(it) => Some(hir::TypeAlias::from(it).name(db)),
        }
    }

    /// The container of an associated item, of a function or constant written in a trait or
    /// an impl.
    pub(crate) fn container(self, cx: &Cx<'_, '_>) -> Option<ItemContainerId> {
        let db = cx.db;
        Some(match self {
            Def::Function(it) => it.lookup(db).container,
            Def::Const(it) => it.lookup(db).container,
            Def::Static(it) => it.lookup(db).container,
            Def::TypeAlias(it) => it.lookup(db).container,
            _ => return None,
        })
    }

    /// Whether it is written in an `extern` block: a foreign item, not one of rustc's free
    /// items.
    fn is_foreign(self, cx: &Cx<'_, '_>) -> bool {
        matches!(self.container(cx), Some(ItemContainerId::ExternBlockId(_)))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Ns {
    Type,
    Value,
    Macro,
}

/// What a definition's path is printed under: rustc's `def_key(def).parent`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Parent {
    Def(Def),
    /// An item written in a body: the body's owner, then the closures it is written in,
    /// outermost first, each with its `{closure#N}` disambiguator.
    Body { owner: Def, closures: Vec<u32> },
}

/// The definition's parent; None for a crate root.
pub(crate) fn parent(cx: &Cx<'_, '_>, def: Def) -> Option<Parent> {
    let db = cx.db;
    let module_parent = |module: ModuleId| {
        if module.is_block_module(db) { block_owner(cx, module) } else { Some(Parent::Def(Def::Module(module))) }
    };
    match def {
        Def::Module(it) => module_parent(it.containing_module(db)?),
        Def::Variant(it) => Some(Parent::Def(Def::Adt(it.lookup(db).parent.into()))),
        Def::Function(_) | Def::Const(_) | Def::Static(_) | Def::TypeAlias(_) => {
            match def.container(cx)? {
                ItemContainerId::ModuleId(it) => module_parent(it),
                ItemContainerId::ExternBlockId(it) => module_parent(it.lookup(db).container),
                ItemContainerId::ImplId(it) => Some(Parent::Def(Def::Impl(it))),
                ItemContainerId::TraitId(it) => Some(Parent::Def(Def::Trait(it))),
            }
        }
        Def::Adt(_) | Def::Trait(_) | Def::Macro(_) | Def::Impl(_) | Def::ExternCrate(_) => {
            module_parent(def.module(cx))
        }
    }
}

/// The body a block module is written in, found from the syntax of its block: rustc's
/// definitions have no blocks, an item in a body is a child of the body's owner, or of the
/// closure it is written in.
fn block_owner(cx: &Cx<'_, '_>, module: ModuleId) -> Option<Parent> {
    let db = cx.db;
    let block = module.block(db)?;
    let ast_id = block.ast_id(db);
    let root = cx.sema.parse_or_expand(ast_id.file_id);
    let node = ast_id.file_id.ast_id_map(db).get(ast_id.value).to_node(&root);
    body_parent(cx, node.syntax().clone())
}

/// The path of a closure, an `async` block or a `gen` block `node`: its body's owner and the
/// closures down to it, itself last.
pub(crate) fn closure_path(cx: &Cx<'_, '_>, node: SyntaxNode) -> Option<Parent> {
    body_parent(cx, node)
}

/// The body's owner `node` is written in and the closures around it, `node` included when it
/// is one.
fn body_parent(cx: &Cx<'_, '_>, node: SyntaxNode) -> Option<Parent> {
    let db = cx.db;
    let mut closures = Vec::new();
    for ancestor in cx.sema.ancestors_with_macros(node) {
        if is_closure_like(&ancestor) {
            closures.push(ancestor);
            continue;
        }
        let owner = if let Some(it) = ast::Fn::cast(ancestor.clone()) {
            Def::Function(FunctionId::try_from(cx.sema.to_def(&it)?).ok()?)
        } else if let Some(it) = ast::Const::cast(ancestor.clone()) {
            Def::Const(cx.sema.to_def(&it)?.into())
        } else if let Some(it) = ast::Static::cast(ancestor.clone()) {
            Def::Static(cx.sema.to_def(&it)?.into())
        } else {
            continue;
        };
        let mut indices = Vec::new();
        if let Def::Function(f) = owner {
            let sig = FunctionSignature::of(db, f);
            if sig.is_async() || sig.is_gen() {
                indices.push(0);
            }
        }
        let mut parent = ancestor;
        for closure in closures.into_iter().rev() {
            indices.push(closure_index(&parent, &closure));
            parent = closure;
        }
        return Some(Parent::Body { owner, closures: indices });
    }
    None
}

/// Whether rustc makes a closure definition of `node`: a closure, an `async` or `gen` block.
pub(crate) fn is_closure_like(node: &SyntaxNode) -> bool {
    match node.kind() {
        SyntaxKind::CLOSURE_EXPR => true,
        SyntaxKind::BLOCK_EXPR => ast::BlockExpr::cast(node.clone()).is_some_and(|it| {
            it.async_token().is_some() || it.gen_token().is_some()
        }),
        _ => false,
    }
}

/// Whether `node` is a definition closures are numbered under: an item, or a closure.
fn is_closure_parent(node: &SyntaxNode) -> bool {
    is_closure_like(node) || ast::Item::can_cast(node.kind())
}

/// The disambiguator rustc gives `closure` among the closures of `parent`: closures are
/// numbered as they are reached, outer before inner, each under the nearest item or closure
/// around it.
pub(crate) fn closure_index(parent: &SyntaxNode, closure: &SyntaxNode) -> u32 {
    let mut index = 0;
    for node in parent.descendants().skip(1) {
        if &node == closure {
            break;
        }
        if is_closure_like(&node)
            && node.ancestors().skip(1).find(is_closure_parent).as_ref() == Some(parent)
        {
            index += 1;
        }
    }
    index
}

/// A child `module_children` lists.
struct Child {
    name: Name,
    def: Def,
    public: bool,
    /// Whether the `use` or `extern crate` it is re-exported by is `#[doc(hidden)]`.
    hidden_reexport: bool,
}

fn children(cx: &Cx<'_, '_>, def: Def) -> Vec<Child> {
    let db = cx.db;
    match def {
        Def::Module(module) => {
            let scope = module.def_map(db).module(db, module).scope(db);
            scope_children(cx, scope)
        }
        Def::Adt(AdtId::EnumId(it)) => it
            .enum_variants(db)
            .variants
            .iter()
            .map(|(name, &(id, _))| Child {
                name: name.clone(),
                def: Def::Variant(id),
                public: true,
                hidden_reexport: false,
            })
            .collect(),
        Def::Trait(it) => it
            .trait_items(db)
            .items
            .iter()
            .map(|(name, item)| Child {
                name: name.clone(),
                def: Def::of_assoc(*item),
                public: true,
                hidden_reexport: false,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn scope_children(cx: &Cx<'_, '_>, scope: &ItemScope) -> Vec<Child> {
    let hidden = |owner: AttrDefId| AttrFlags::query(cx.db, owner).contains(AttrFlags::IS_DOC_HIDDEN);
    let reexport = |import: Option<ImportOrExternCrate>| match import {
        Some(ImportOrExternCrate::Import(it)) => hidden(AttrDefId::UseId(it.use_)),
        Some(ImportOrExternCrate::Glob(it)) => hidden(AttrDefId::UseId(it.use_)),
        Some(ImportOrExternCrate::ExternCrate(it)) => hidden(AttrDefId::ExternCrateId(it)),
        None => false,
    };
    let mut out = Vec::new();
    for (name, item) in scope.types() {
        if let Some(def) = Def::of(item.def) {
            let public = item.vis == Visibility::Public;
            out.push(Child { name: name.clone(), def, public, hidden_reexport: reexport(item.import) });
        }
    }
    for (name, item) in scope.values() {
        if let Some(def) = Def::of(item.def) {
            let import = item.import.map(|it| match it {
                ImportOrGlob::Import(it) => ImportOrExternCrate::Import(it),
                ImportOrGlob::Glob(it) => ImportOrExternCrate::Glob(it),
            });
            let public = item.vis == Visibility::Public;
            out.push(Child { name: name.clone(), def, public, hidden_reexport: reexport(import) });
        }
    }
    for (name, item) in scope.macros() {
        let public = item.vis == Visibility::Public;
        let def = Def::Macro(item.def);
        out.push(Child { name: name.clone(), def, public, hidden_reexport: reexport(item.import) });
    }
    out
}

/// The names glob imports bring into a scope: rustc's `glob_map` has the names used through
/// a glob import, a name any local glob brings in counts here until the front end records
/// which are used.
fn glob_names(scope: &ItemScope, out: &mut FxHashSet<Name>) {
    for (name, item) in scope.types() {
        if matches!(item.import, Some(ImportOrExternCrate::Glob(_))) {
            out.insert(name.clone());
        }
    }
    for (name, item) in scope.values() {
        if matches!(item.import, Some(ImportOrGlob::Glob(_))) {
            out.insert(name.clone());
        }
    }
    for (name, item) in scope.macros() {
        if matches!(item.import, Some(ImportOrExternCrate::Glob(_))) {
            out.insert(name.clone());
        }
    }
}

/// The names and parents paths are printed with, computed once for a crate.
pub(crate) struct Paths {
    trimmed: FxHashMap<Def, Name>,
    visible_parent: FxHashMap<Def, Def>,
    /// Each dependency the crate names by an `extern crate` item written in it.
    extern_crates: FxHashMap<base_db::Crate, ExternCrateId>,
}

impl Paths {
    pub(crate) fn new(cx: &Cx<'_, '_>) -> Paths {
        let local = local_defs(cx);
        let direct = direct_crates(cx, &local.extern_crates);
        Paths {
            trimmed: trimmed_def_paths(cx, &local, &direct),
            visible_parent: visible_parent_map(cx, &direct),
            extern_crates: local.extern_crates,
        }
    }

    /// The one name `def` is known by across the crate and its direct dependencies.
    pub(crate) fn trimmed(&self, def: Def) -> Option<&Name> {
        self.trimmed.get(&def)
    }

    pub(crate) fn visible_parent(&self, def: Def) -> Option<Def> {
        self.visible_parent.get(&def).copied()
    }

    /// The `extern crate` item the crate names `krate` by, when it writes one.
    pub(crate) fn extern_crate(&self, krate: base_db::Crate) -> Option<ExternCrateId> {
        self.extern_crates.get(&krate).copied()
    }

    /// The name of the public child of `parent` that is `def`, other than `_`: what a
    /// re-export calls it.
    pub(crate) fn reexport_name(&self, cx: &Cx<'_, '_>, parent: Def, def: Def) -> Option<Name> {
        children(cx, parent)
            .into_iter()
            .find(|it| it.def == def && it.public && it.name.as_str() != "_")
            .map(|it| it.name)
    }
}

/// The crate's own named definitions, wherever they are written (rustc's `hir_free_items`),
/// the names its glob imports bring in, and its `extern crate` items.
struct LocalDefs {
    defs: Vec<(Name, Def)>,
    globs: FxHashSet<Name>,
    extern_crates: FxHashMap<base_db::Crate, ExternCrateId>,
}

fn local_defs(cx: &Cx<'_, '_>) -> LocalDefs {
    let db = cx.db;
    let mut out = LocalDefs { defs: Vec::new(), globs: FxHashSet::default(), extern_crates: FxHashMap::default() };
    let mut bodies: Vec<DefWithBodyId> = Vec::new();
    let mut seen_bodies = FxHashSet::default();
    let def_map = crate_def_map(db, cx.krate.into());
    let mut scopes: Vec<&ItemScope> = def_map.modules(db).map(|(_, data)| data.scope(db)).collect();
    loop {
        for scope in scopes.drain(..) {
            glob_names(scope, &mut out.globs);
            for id in scope.extern_crate_decls() {
                let decl = hir::ExternCrateDecl::from(id);
                if let Some(name) = decl.alias_or_name(db) {
                    out.defs.push((name, Def::ExternCrate(id)));
                }
                if let Some(krate) = decl.resolved_crate(db) {
                    out.extern_crates.entry(krate.into()).or_insert(id);
                }
            }
            for id in scope.declarations() {
                let Some(def) = Def::of(id) else { continue };
                if !def.is_foreign(cx)
                    && let Some(name) = def.name(cx)
                {
                    out.defs.push((name, def));
                }
                match id {
                    ModuleDefId::FunctionId(it) => bodies.push(it.into()),
                    ModuleDefId::ConstId(it) => bodies.push(it.into()),
                    ModuleDefId::StaticId(it) => bodies.push(it.into()),
                    ModuleDefId::TraitId(it) => {
                        bodies.extend(it.trait_items(db).items.iter().filter_map(|(_, it)| body_of(*it)));
                    }
                    _ => {}
                }
            }
            for id in scope.impls() {
                bodies.extend(id.impl_items(db).items.iter().filter_map(|(_, it)| body_of(*it)));
            }
        }
        let Some(body) = bodies.pop() else { break };
        if !seen_bodies.insert(body) {
            continue;
        }
        let blocks: Vec<BlockId> = Body::of(db, body).blocks(db).map(|(block, _)| block).collect();
        for block in blocks {
            scopes.extend(block_def_map(db, block).modules(db).map(|(_, data)| data.scope(db)));
        }
    }
    out
}

fn body_of(item: AssocItemId) -> Option<DefWithBodyId> {
    match item {
        AssocItemId::FunctionId(it) => Some(it.into()),
        AssocItemId::ConstId(it) => Some(it.into()),
        AssocItemId::TypeAliasId(_) => None,
    }
}

/// The crates rustc counts as the crate's direct dependencies, the ones it has loaded for
/// the crate itself, `std` first: `std` (or `core` when there is no `std`), the crates of
/// the crate's `extern crate` items, and the dependencies that are not the language's.
fn direct_crates(
    cx: &Cx<'_, '_>,
    extern_crates: &FxHashMap<base_db::Crate, ExternCrateId>,
) -> Vec<hir::Crate> {
    let db = cx.db;
    let deps: Vec<hir::Crate> = cx.krate.dependencies(db).into_iter().map(|it| it.krate).collect();
    let lang = |krate: hir::Crate| match krate.origin(db) {
        CrateOrigin::Lang(it) => Some(it),
        _ => None,
    };
    let injected = deps
        .iter()
        .copied()
        .find(|&it| lang(it) == Some(LangCrateOrigin::Std))
        .or_else(|| deps.iter().copied().find(|&it| lang(it) == Some(LangCrateOrigin::Core)));
    let mut out: Vec<hir::Crate> = injected.into_iter().collect();
    for krate in deps {
        let named = extern_crates.contains_key(&krate.into());
        if !out.contains(&krate) && (lang(krate).is_none() || named) {
            out.push(krate);
        }
    }
    out
}

/// rustc's `trimmed_def_paths`: each definition whose name names nothing else, in its
/// namespace, across the crate's own definitions and the public ones of its direct
/// dependencies; a name glob imports bring in names nothing.
fn trimmed_def_paths(cx: &Cx<'_, '_>, local: &LocalDefs, direct: &[hir::Crate]) -> FxHashMap<Def, Name> {
    let mut unique: FxHashMap<(Ns, Name), Option<Def>> = FxHashMap::default();
    for name in &local.globs {
        for ns in [Ns::Type, Ns::Value, Ns::Macro] {
            unique.insert((ns, name.clone()), None);
        }
    }
    let mut collect = |name: &Name, ns: Ns, def: Def| {
        unique
            .entry((ns, name.clone()))
            .and_modify(|it| {
                if it.is_some_and(|existing| existing != def) {
                    *it = None;
                }
            })
            .or_insert(Some(def));
    };
    for (name, def) in &local.defs {
        collect(name, def.ns().unwrap_or(Ns::Type), *def);
    }
    let mut queue: Vec<Def> =
        direct.iter().map(|it| Def::Module(it.root_module(cx.db).into())).collect();
    let mut seen = FxHashSet::default();
    while let Some(parent) = queue.pop() {
        for child in children(cx, parent) {
            if !child.public {
                continue;
            }
            if let Def::TypeAlias(it) = child.def
                && !matches!(it.lookup(cx.db).container, ItemContainerId::ExternBlockId(_))
            {
                continue;
            }
            if child.def.is_doc_hidden(cx) {
                continue;
            }
            if let Some(ns) = child.def.ns() {
                collect(&child.name, ns, child.def);
            }
            if child.def.is_module_like() && seen.insert(child.def) {
                queue.push(child.def);
            }
        }
    }
    let mut map: FxHashMap<Def, Name> = FxHashMap::default();
    for ((_, name), def) in unique {
        let Some(def) = def else { continue };
        map.entry(def)
            .and_modify(|it| {
                if it.symbol().cmp_order(name.symbol()).is_gt() {
                    *it = name.clone();
                }
            })
            .or_insert(name);
    }
    map
}

/// rustc's `visible_parent_map`: for each definition of a direct dependency reached through
/// public children, breadth first over all of them so that the shortest path wins, the
/// parent it is reached through; parents through `_`, a `#[doc(hidden)]` parent or a
/// `#[doc(hidden)]` re-export only where there is no other.
fn visible_parent_map(cx: &Cx<'_, '_>, direct: &[hir::Crate]) -> FxHashMap<Def, Def> {
    let mut map: FxHashMap<Def, Def> = FxHashMap::default();
    let mut fallback: Vec<(Def, Def)> = Vec::new();
    let mut queue: VecDeque<Def> =
        direct.iter().map(|it| Def::Module(it.root_module(cx.db).into())).collect();
    while let Some(parent) = queue.pop_front() {
        let parent_hidden = parent.is_doc_hidden(cx);
        for child in children(cx, parent) {
            if !child.public {
                continue;
            }
            if child.name.as_str() == "_" || parent_hidden || child.hidden_reexport {
                fallback.push((child.def, parent));
                continue;
            }
            if let std::collections::hash_map::Entry::Vacant(entry) = map.entry(child.def) {
                entry.insert(parent);
                if child.def.is_module_like() {
                    queue.push_back(child.def);
                }
            }
        }
    }
    for (child, parent) in fallback {
        map.entry(child).or_insert(parent);
    }
    map
}

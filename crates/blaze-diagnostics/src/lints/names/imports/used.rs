//! What the crate's name resolution goes through: every path of the crate, its `use` items'
//! own included, resolved prefix by prefix in hir-def's scopes, each import, `extern crate`
//! item and `#[macro_use]` import a prefix is found through marked used as rustc's
//! `record_use` marks it, with the imports each of those is found through in turn; the
//! traits in scope at each method call, field and type-relative path (rustc's
//! `traits_in_scope`), with the imports type checking uses of them; the macros invoked and
//! the rules of theirs that matched; and which imports the crate re-exports.
//!
//! What a prefix is found through is checked against what hir resolves the path to: where
//! they differ, or the path does not resolve, every import the name could be found through
//! counts as used, so nothing is linted that rustc may find used.
//!
//! Follows rustc's `compiler/rustc_resolve/src/lib.rs` (`record_use`, `traits_in_scope`,
//! `traits_in_module`, `find_transitive_imports`), `ident.rs` (`resolve_ident_in_scope_set`,
//! `resolve_ident_in_module`), `imports.rs` (`finalize_import`), `late.rs`
//! (`smart_resolve_path_fragment`, `record_candidate_traits_for_expr_if_necessary`,
//! `resolve_qpath`), `macros.rs` (`resolve_macro_path`), `build_reduced_graph.rs`
//! (`build_reduced_graph_for_extern_crate`, `process_macro_use_imports`),
//! `effective_visibilities.rs`, and `compiler/rustc_hir_typeck/src/method/probe.rs`
//! (`assemble_extension_candidates_for_traits_in_scope`: the first candidate of a trait is
//! the one used) (MIT or Apache-2.0).

use std::{
    cell::RefCell,
    hash::{Hash, Hasher},
};

use hashbrown::HashTable;
use hir::{AsAssocItem, HirFileId, InFile, ModuleDef, PathResolution};
use hir_def::{
    ExternCrateId, Lookup, MacroId, ModuleDefId, ModuleId, TraitId, UseId,
    item_scope::{GlobId, ImportId, ImportOrExternCrate, ImportOrGlob},
    per_ns::PerNs,
    resolver::HasResolver,
    src::HasChildSource,
    visibility::Visibility,
};
use hir_expand::{
    mod_path::{ModPath, PathKind},
    name::{AsName, Name},
};
use intern::sym;
use rustc_hash::{FxHashMap, FxHashSet, FxHasher};
use smallvec::SmallVec;
use span::Edition;
use syntax::{
    AstNode, SyntaxKind, SyntaxNode, SyntaxNodePtr, TextRange, TextSize,
    ast::{self, HasAttrs, HasVisibility},
};

use super::Places;
use crate::{cx::Cx, lints::syntax::Walk};

/// What a name is found through: rustc's `Import` of a `use` tree, glob or `extern crate` item.
pub(super) type Binding = ImportOrExternCrate;

/// How a binding is used: rustc's `Used`. A name found where a path starts, in the scopes
/// around it, is used from the scope; one found in a module, by a path's later segment or
/// through another import, is used otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Used {
    Scope,
    Other,
}

/// rustc's `Namespace`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Ns {
    Types,
    Values,
    Macros,
}

impl Ns {
    pub(super) const ALL: [Ns; 3] = [Ns::Types, Ns::Values, Ns::Macros];

    /// rustc's `Namespace::descr`.
    pub(super) fn descr(self) -> &'static str {
        match self {
            Ns::Types => "type",
            Ns::Values => "value",
            Ns::Macros => "macro",
        }
    }
}

/// What a name is in one namespace of a scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Entry {
    pub(super) def: ModuleDefId,
    pub(super) vis: Visibility,
    /// The import or `extern crate` item it is found through, when it is not defined there.
    pub(super) import: Option<Binding>,
}

/// What `per` has in `ns`.
pub(super) fn entry(per: &PerNs, ns: Ns) -> Option<Entry> {
    match ns {
        Ns::Types => per.types.map(|it| Entry { def: it.def, vis: it.vis, import: it.import }),
        Ns::Values => per.values.map(|it| Entry {
            def: it.def,
            vis: it.vis,
            import: it.import.map(|it| match it {
                ImportOrGlob::Import(it) => Binding::Import(it),
                ImportOrGlob::Glob(it) => Binding::Glob(it),
            }),
        }),
        Ns::Macros => per.macros.map(|it| Entry {
            def: ModuleDefId::MacroId(it.def),
            vis: it.vis,
            import: it.import,
        }),
    }
}

/// What a `use` tree is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TreeKind {
    /// `a::b`, `a::b as c`, `self` in a list.
    Simple,
    /// `a::*`.
    Glob,
    /// `a::{..}`.
    List,
}

/// A `use` tree.
pub(super) struct Tree {
    pub(super) node: ast::UseTree,
    /// The index of the tree whose list it is in.
    pub(super) parent: Option<u32>,
    pub(super) kind: TreeKind,
    /// The import it is; `None` for a list.
    pub(super) binding: Option<Binding>,
    /// The name it imports, the module's for `self` in a list.
    pub(super) name: Option<Name>,
    /// `as _`.
    pub(super) underscore: bool,
    /// `self` in a list.
    pub(super) self_import: bool,
    /// Whether its name is looked up where the `use` is, not in a module.
    pub(super) lexical: bool,
    /// What its name is looked up in: the module or enum of its prefix; for a glob, what it
    /// imports from.
    pub(super) source: Option<ModuleDefId>,
    /// What it imports, in each namespace.
    pub(super) imported: PerNs,
}

/// A `use` item of the crate.
pub(super) struct UseItem {
    pub(super) node: InFile<ast::Use>,
    pub(super) id: UseId,
    /// The module it is in, a block's when it is in one.
    pub(super) module: ModuleId,
    /// `pub use`.
    pub(super) public: bool,
    /// Its trees in pre-order, the root first.
    pub(super) trees: Vec<Tree>,
}

/// An `extern crate` item of the crate.
pub(super) struct ExternCrate {
    pub(super) node: InFile<ast::ExternCrate>,
    pub(super) id: ExternCrateId,
    pub(super) module: ModuleId,
    /// The name it binds.
    pub(super) name: Name,
    /// `extern crate a as b;`.
    pub(super) renames: bool,
    pub(super) public: bool,
    /// The crate it loads, when it resolves.
    pub(super) krate: Option<hir::Crate>,
    /// Its `#[macro_use]` imports: the attribute for `#[macro_use]`, else each name
    /// `#[macro_use(a, b)]` lists, with where it is written.
    pub(super) macro_use: Vec<(TextRange, Option<Name>)>,
}

/// What the crate's name resolution goes through.
pub(super) struct Uses {
    /// The `use` items, in the order rustc's walk comes to them.
    pub(super) items: Vec<UseItem>,
    /// The `extern crate` items, in that order.
    pub(super) crates: Vec<ExternCrate>,
    /// The local bindings used, each as it was used most: rustc's `import_use_map`.
    pub(super) used: FxHashMap<Binding, Used>,
    /// The trait imports a method call, field or type-relative path could use, in the order
    /// they were first found: rustc's `maybe_unused_trait_imports` before `check_unused`.
    pub(super) candidates: Vec<Binding>,
    /// The trait imports type checking uses: rustc's `used_trait_imports`.
    pub(super) typeck: FxHashSet<Binding>,
    /// The bindings the crate re-exports: rustc's `effective_visibilities.is_exported`.
    pub(super) exported: FxHashSet<Binding>,
    /// Each name in a namespace a binding is re-exported as.
    pub(super) exported_names: FxHashSet<(Binding, Name, Ns)>,
    /// The modules reachable from other crates, through public names from the root.
    public_modules: FxHashSet<ModuleId>,
    /// The `#[macro_use]` imports used: of every macro (`None`) or of one.
    pub(super) macro_uses: FxHashSet<(ExternCrateId, Option<Name>)>,
    /// The macros invoked: by a call, an attribute or a derive.
    pub(super) macros: FxHashSet<MacroId>,
    /// The rules of each `macro_rules!` a call matched.
    pub(super) arms: FxHashMap<MacroId, FxHashSet<u32>>,
    /// The macros invoked where which rule matched can't be told.
    pub(super) arms_unknown: FxHashSet<MacroId>,
    /// The names of macro calls that don't resolve: a macro of such a name may be used.
    pub(super) macros_unknown: FxHashSet<Name>,
    /// The names `--extern` puts in the crate's extern prelude.
    pub(super) extern_prelude: FxHashSet<Name>,
    /// Each module's `use` items, by index into `items`, in `items`' order.
    pub(super) by_module: FxHashMap<ModuleId, SmallVec<[u32; 4]>>,
    /// Each module's glob trees, as (item, tree) indices, in `items`' order.
    pub(super) globs: FxHashMap<ModuleId, SmallVec<[(u32, u32); 2]>>,
    trees: FxHashMap<Binding, (u32, u32)>,
    traits: FxHashMap<(ModuleId, Name), Vec<(TraitId, Vec<Binding>)>>,
    /// Each module's `use` items by file and start, filled once per module.
    use_ids: FxHashMap<(HirFileId, TextSize), UseId>,
    indexed: FxHashSet<ModuleId>,
    lookups: RefCell<HashTable<Lookup>>,
    krate: base_db::Crate,
}

impl Uses {
    pub(super) fn of(cx: &Cx<'_, '_>, walk: &Walk<'_>, places: &Places) -> Uses {
        let extern_prelude = cx
            .krate
            .base()
            .data(cx.db)
            .dependencies
            .iter()
            .filter(|it| it.is_prelude())
            .map(|it| Name::new_symbol_root(it.name.symbol().clone()))
            .collect();
        let mut uses = Uses {
            items: Vec::new(),
            crates: Vec::new(),
            used: FxHashMap::default(),
            candidates: Vec::new(),
            typeck: FxHashSet::default(),
            exported: FxHashSet::default(),
            exported_names: FxHashSet::default(),
            public_modules: FxHashSet::default(),
            macro_uses: FxHashSet::default(),
            macros: FxHashSet::default(),
            arms: FxHashMap::default(),
            arms_unknown: FxHashSet::default(),
            macros_unknown: FxHashSet::default(),
            extern_prelude,
            by_module: FxHashMap::default(),
            globs: FxHashMap::default(),
            trees: FxHashMap::default(),
            traits: FxHashMap::default(),
            use_ids: FxHashMap::default(),
            indexed: FxHashSet::default(),
            lookups: RefCell::new(HashTable::new()),
            krate: cx.krate.base(),
        };
        for tree in &walk.trees {
            for node in &tree.nodes {
                match node.kind() {
                    SyntaxKind::USE => uses.use_item(cx, InFile::new(tree.file, ast::Use::cast(node.clone()).unwrap())),
                    SyntaxKind::EXTERN_CRATE => {
                        uses.extern_crate(cx, walk, InFile::new(tree.file, ast::ExternCrate::cast(node.clone()).unwrap()))
                    }
                    _ => {}
                }
            }
        }
        let place = |node: InFile<&SyntaxNode>| places.place(cx, node.map(SyntaxNodePtr::new));
        uses.items.sort_by_cached_key(|it| place(it.node.as_ref().map(|it| it.syntax())));
        uses.crates.sort_by_cached_key(|it| place(it.node.as_ref().map(|it| it.syntax())));
        let bindings = uses.items.iter().map(|it| it.trees.len()).sum();
        uses.trees.reserve(bindings);
        for (i, item) in uses.items.iter().enumerate() {
            uses.by_module.entry(item.module).or_default().push(i as u32);
            for (j, tree) in item.trees.iter().enumerate() {
                if let Some(binding) = tree.binding {
                    uses.trees.insert(binding, (i as u32, j as u32));
                }
                if tree.kind == TreeKind::Glob {
                    uses.globs.entry(item.module).or_default().push((i as u32, j as u32));
                }
            }
        }
        for i in 0..uses.items.len() {
            uses.resolve_use(cx, i);
        }
        for krate in 0..uses.crates.len() {
            let it = &uses.crates[krate];
            if !it.macro_use.is_empty() || it.krate.is_none() {
                uses.used.insert(Binding::ExternCrate(it.id), Used::Other);
            }
        }
        for tree in &walk.trees {
            for node in &tree.nodes {
                uses.node(cx, walk, tree.file, node);
            }
        }
        for tree in &walk.trees {
            for item in tree.replaced.iter().filter_map(|it| ast::Item::cast(it.clone())) {
                uses.attr_macro(cx, InFile::new(tree.file, item));
            }
        }
        uses.export(cx);
        uses
    }

    /// The tree `binding` is.
    pub(super) fn tree(&self, binding: Binding) -> Option<(&UseItem, &Tree)> {
        let &(i, j) = self.trees.get(&binding)?;
        let item = &self.items[i as usize];
        Some((item, &item.trees[j as usize]))
    }

    /// What `kind` and `names` resolve to from `module`, through which bindings.
    pub(super) fn lookup(&self, cx: &Cx<'_, '_>, module: ModuleId, kind: PathKind, names: &[Name]) -> PerNs {
        let hash = lookup_hash(module, kind, names);
        let found = self
            .lookups
            .borrow()
            .find(hash, |it| it.module == module && it.kind == kind && it.names.as_slice() == names)
            .map(|it| it.per);
        if let Some(per) = found {
            return per;
        }
        let per = resolve_in_items(cx, module, kind, names);
        self.lookups.borrow_mut().insert_unique(
            hash,
            Lookup { module, kind, names: names.iter().cloned().collect(), per },
            |it| lookup_hash(it.module, it.kind, &it.names),
        );
        per
    }

    /// The `use` items of `module`.
    pub(super) fn items_of(&self, module: ModuleId) -> impl Iterator<Item = &UseItem> + '_ {
        self.by_module.get(&module).into_iter().flatten().map(|&i| &self.items[i as usize])
    }

    /// The glob trees of `module`, each with its item.
    pub(super) fn globs_of(&self, module: ModuleId) -> impl Iterator<Item = (&UseItem, &Tree)> + '_ {
        self.globs.get(&module).into_iter().flatten().map(|&(i, j)| {
            let item = &self.items[i as usize];
            (item, &item.trees[j as usize])
        })
    }

    /// Whether `binding` is one of the crate's own.
    pub(super) fn is_local(&self, cx: &Cx<'_, '_>, binding: Binding) -> bool {
        let module = match binding {
            Binding::Import(it) => it.use_.lookup(cx.db).container,
            Binding::Glob(it) => it.use_.lookup(cx.db).container,
            Binding::ExternCrate(it) => it.lookup(cx.db).container,
        };
        module.krate(cx.db) == self.krate
    }

    /// The `extern crate` item `id`.
    pub(super) fn extern_crate_of(&self, id: ExternCrateId) -> Option<&ExternCrate> {
        self.crates.iter().find(|it| it.id == id)
    }

    /// Whether the root's `extern crate` item `it` is what puts its name in the extern
    /// prelude: rustc's `introduced_by_item`, when it renames the crate or `--extern` does not
    /// name it.
    pub(super) fn introduces(&self, it: &ExternCrate) -> bool {
        it.renames || !self.extern_prelude.contains(&it.name)
    }

    fn use_item(&mut self, cx: &Cx<'_, '_>, node: InFile<ast::Use>) {
        let db = cx.db;
        let Some(scope) = cx.sema.scope(node.value.syntax()) else { return };
        let module = ModuleId::from(scope.module());
        if self.indexed.insert(module) {
            for id in module.def_map(db).module(db, module).scope(db).use_decls() {
                let loc = id.lookup(db);
                self.use_ids.insert((loc.id.file_id, loc.id.to_ptr(db).text_range().start()), id);
            }
        }
        let Some(&id) = self.use_ids.get(&(node.file_id, node.value.syntax().text_range().start())) else { return };
        let sources = id.child_source(db);
        let public = node
            .value
            .visibility()
            .is_some_and(|it| matches!(it.kind(), ast::VisibilityKind::Pub));
        let mut trees: Vec<Tree> = Vec::new();
        // The trees enclosing the one being added, innermost last: pre-order makes the parent
        // the innermost that still contains it.
        let mut open: SmallVec<[(TextRange, u32); 4]> = SmallVec::new();
        for tree in node.value.syntax().descendants().filter_map(ast::UseTree::cast) {
            let range = tree.syntax().text_range();
            while open.last().is_some_and(|(it, _)| !it.contains_range(range)) {
                open.pop();
            }
            let parent = open.last().map(|&(_, it)| it);
            open.push((range, trees.len() as u32));
            // Lowering numbers an item's trees in pre-order as well, so starting at this tree's
            // own index the scan stops at its first step.
            let idx = sources
                .value
                .iter()
                .skip(trees.len())
                .chain(sources.value.iter().take(trees.len()))
                .find(|(_, it)| it.syntax().text_range() == range)
                .map(|(idx, _)| idx);
            let kind = if tree.use_tree_list().is_some() {
                TreeKind::List
            } else if tree.star_token().is_some() {
                TreeKind::Glob
            } else {
                TreeKind::Simple
            };
            let binding = match (kind, idx) {
                (TreeKind::List, _) | (_, None) => None,
                (TreeKind::Glob, Some(idx)) => Some(Binding::Glob(GlobId { use_: id, idx })),
                (TreeKind::Simple, Some(idx)) => Some(Binding::Import(ImportId { use_: id, idx })),
            };
            let underscore = tree.rename().is_some_and(|it| it.underscore_token().is_some());
            trees.push(Tree {
                node: tree,
                parent,
                kind,
                binding,
                name: None,
                underscore,
                self_import: false,
                lexical: false,
                source: None,
                imported: PerNs::default(),
            });
        }
        self.items.push(UseItem { node, id, module, public, trees });
    }

    fn extern_crate(&mut self, cx: &Cx<'_, '_>, walk: &Walk<'_>, node: InFile<ast::ExternCrate>) {
        let db = cx.db;
        let Some(decl) = cx.sema.to_def(&node.value) else { return };
        let id = ExternCrateId::from(decl);
        let module = id.lookup(db).container;
        let name = match node.value.rename() {
            Some(rename) if rename.underscore_token().is_some() => Name::new_symbol_root(sym::underscore),
            Some(rename) => match ast::HasName::name(&rename) {
                Some(it) => it.as_name(),
                None => return,
            },
            None => match node.value.name_ref() {
                Some(it) => it.as_name(),
                None => return,
            },
        };
        let mut macro_use = Vec::new();
        let owner = ast::AnyHasAttrs::new(node.value.clone());
        for (meta, attr) in walk.attrs(&owner) {
            let path = match &meta {
                ast::Meta::PathMeta(it) => it.path(),
                ast::Meta::TokenTreeMeta(it) => it.path(),
                _ => None,
            };
            if path.and_then(|it| it.as_single_name_ref()).is_none_or(|it| it.text() != "macro_use") {
                continue;
            }
            match &meta {
                ast::Meta::TokenTreeMeta(it) => {
                    let Some(tt) = it.token_tree() else { continue };
                    for token in tt.syntax().children_with_tokens().filter_map(|it| it.into_token()) {
                        if token.kind() == SyntaxKind::IDENT {
                            macro_use.push((token.text_range(), Some(Name::new_root(token.text()))));
                        }
                    }
                }
                _ => macro_use.push((attr.syntax().text_range(), None)),
            }
        }
        let public = node
            .value
            .visibility()
            .is_some_and(|it| matches!(it.kind(), ast::VisibilityKind::Pub));
        let renames = node.value.rename().is_some();
        self.crates.push(ExternCrate {
            node,
            id,
            module,
            name,
            renames,
            public,
            krate: decl.resolved_crate(db),
            macro_use,
        });
    }

    /// Marks `binding` used, and what it is found through in turn (as `name` in `ns`), as
    /// rustc's `record_use` does.
    fn mark(&mut self, cx: &Cx<'_, '_>, binding: Binding, used: Used, name: &Name, ns: Ns) {
        let mut at = Some((binding, used));
        let mut seen: SmallVec<[Binding; 4]> = SmallVec::new();
        while let Some((binding, used)) = at.take() {
            if seen.contains(&binding) || !self.is_local(cx, binding) {
                return;
            }
            seen.push(binding);
            if let Binding::ExternCrate(id) = binding
                && ns == Ns::Macros
            {
                self.macro_uses.insert((id, None));
                self.macro_uses.insert((id, Some(name.clone())));
                return;
            }
            if let Binding::ExternCrate(id) = binding
                && used == Used::Scope
                && let Some(it) = self.extern_crate_of(id)
                && it.module == cx.krate.root_module(cx.db).into()
                && !self.introduces(it)
            {
                return;
            }
            let was = self.used.entry(binding).or_insert(used);
            *was = (*was).max(used);
            at = self.next(cx, binding, name, ns).and_then(|it| it.import).map(|it| (it, Used::Other));
        }
    }

    /// What `binding` finds `name` as in `ns`: what a single import imports, or what the
    /// module a glob imports from has.
    pub(super) fn next(&self, cx: &Cx<'_, '_>, binding: Binding, name: &Name, ns: Ns) -> Option<Entry> {
        let (_, tree) = self.tree(binding)?;
        let found = match binding {
            Binding::Import(_) => entry(&tree.imported, ns)?,
            Binding::Glob(_) => {
                let Some(ModuleDefId::ModuleId(from)) = tree.source else { return None };
                entry(&from.def_map(cx.db).module(cx.db, from).name_in_scope(cx.db, name), ns)?
            }
            Binding::ExternCrate(_) => return None,
        };
        (found.import != Some(binding)).then_some(found)
    }

    /// Counts as used every binding `name` could be found through, from the scopes around
    /// `module` when `lexical`, else in `module`: what it names there, every glob import there,
    /// and for a lexical name the root's `extern crate` items and `#[macro_use]` imports.
    fn taint(&mut self, cx: &Cx<'_, '_>, module: ModuleId, name: &Name, lexical: bool, except: Option<Binding>) {
        let db = cx.db;
        let mut at = Some(module);
        while let Some(id) = at {
            let def_map = id.def_map(db);
            let per = def_map.module(db, id).name_in_scope(db, name);
            for ns in Ns::ALL {
                if let Some(Entry { import: Some(binding), .. }) = entry(&per, ns)
                    && Some(binding) != except
                {
                    self.mark(cx, binding, Used::Other, name, ns);
                }
            }
            if let Some(globs) = self.globs.get(&id) {
                for &(i, j) in globs {
                    if let Some(glob) = self.items[i as usize].trees[j as usize].binding {
                        self.used.entry(glob).or_insert(Used::Other);
                    }
                }
            }
            at = if lexical && id.is_block_module(db) { def_map.containing_module(db, id) } else { None };
        }
        if lexical {
            let root: ModuleId = cx.krate.root_module(db).into();
            for it in &self.crates {
                if it.module == root && it.name == *name {
                    self.used.entry(Binding::ExternCrate(it.id)).or_insert(Used::Other);
                }
                if !it.macro_use.is_empty() {
                    self.macro_uses.insert((it.id, None));
                    self.macro_uses.insert((it.id, Some(name.clone())));
                }
            }
        }
    }

    /// Resolves the trees of the `use` item `item` as `finalize_import` does: each prefix in
    /// the type namespace, each leaf in every namespace, marking what each is found through.
    fn resolve_use(&mut self, cx: &Cx<'_, '_>, item: usize) {
        let file = self.items[item].node.file_id;
        let module = self.items[item].module;
        let span_map = file.span_map(cx.db);
        let count = self.items[item].trees.len();
        // Each tree's prefix as its kind and a range of `flat`; a child's starts with a copy of
        // its parent's.
        let mut prefixes: SmallVec<[Option<(PathKind, u32, u32)>; 4]> = SmallVec::with_capacity(count);
        let mut flat: SmallVec<[Name; 8]> = SmallVec::new();
        for j in 0..count {
            let tree = self.items[item].trees[j].node.clone();
            let base = match self.items[item].trees[j].parent {
                Some(parent) => prefixes[parent as usize],
                None => Some((PathKind::Plain, 0, 0)),
            };
            let inherited = base.map_or(0, |(_, start, end)| (end - start) as usize);
            let own = match tree.path() {
                Some(path) => ModPath::from_src(cx.db, path, &mut |range| span_map.span_for_range(range).ctx),
                None => Some(ModPath::from_kind(if tree.coloncolon_token().is_some() {
                    PathKind::Abs
                } else {
                    PathKind::Plain
                })),
            };
            let prefix = match (base, own) {
                (Some((kind, start, end)), Some(own)) if start == end && kind == PathKind::Plain => {
                    let mut kind = own.kind;
                    if kind == PathKind::Plain && cx.edition == Edition::Edition2015 && !own.segments().is_empty() {
                        kind = PathKind::Abs;
                    }
                    let start = flat.len() as u32;
                    flat.extend(own.segments().iter().cloned());
                    Some((kind, start, flat.len() as u32))
                }
                (Some((kind, start, end)), Some(own)) if own.kind == PathKind::Plain => {
                    let new = flat.len() as u32;
                    for k in start as usize..end as usize {
                        let name = flat[k].clone();
                        flat.push(name);
                    }
                    flat.extend(own.segments().iter().cloned());
                    Some((kind, new, flat.len() as u32))
                }
                (Some(base), Some(own)) if own.kind == PathKind::Super(0) && own.segments().is_empty() => {
                    self.items[item].trees[j].self_import = true;
                    Some(base)
                }
                _ => None,
            };
            prefixes.push(prefix);
            let Some((kind, start, end)) = prefix else { continue };
            self.resolve_tree(cx, item, j, module, kind, &flat[start as usize..end as usize], inherited);
        }
    }

    /// Resolves the tree `j` of `item`, whose path is `names` after `kind`, the first
    /// `inherited` of them its parent's and resolved with it.
    #[allow(clippy::too_many_arguments)]
    fn resolve_tree(
        &mut self,
        cx: &Cx<'_, '_>,
        item: usize,
        j: usize,
        module: ModuleId,
        kind: PathKind,
        names: &[Name],
        inherited: usize,
    ) {
        let tree = &self.items[item].trees[j];
        let (tree_kind, binding, self_import) = (tree.kind, tree.binding, tree.self_import);
        let leaf = tree_kind == TreeKind::Simple && !self_import;
        let module_segments = if leaf { names.len().saturating_sub(1) } else { names.len() };
        for k in inherited.min(module_segments)..module_segments {
            let per = self.lookup(cx, module, kind, &names[..=k]);
            let used = if k == 0 && kind == PathKind::Plain { Used::Scope } else { Used::Other };
            match entry(&per, Ns::Types) {
                Some(Entry { def: ModuleDefId::ModuleId(_) | ModuleDefId::AdtId(hir_def::AdtId::EnumId(_)), import, .. }) => {
                    if let Some(found) = import
                        && Some(found) != binding
                    {
                        self.mark(cx, found, used, &names[k], Ns::Types);
                    }
                }
                _ => {
                    self.taint(cx, module, &names[k], k == 0 && kind == PathKind::Plain, binding);
                    return;
                }
            }
        }
        let source = if names.len() > usize::from(leaf) {
            let prefix = if leaf { &names[..names.len() - 1] } else { names };
            entry(&self.lookup(cx, module, kind, prefix), Ns::Types).map(|it| it.def)
        } else {
            match kind {
                PathKind::Plain => None,
                _ => entry(&self.lookup(cx, module, kind, &[]), Ns::Types).map(|it| it.def),
            }
        };
        let lexical = leaf && names.len() == 1 && kind == PathKind::Plain;
        let name = if self_import {
            names.last().cloned()
        } else if leaf {
            names.last().cloned()
        } else {
            None
        };
        let mut imported = PerNs::default();
        if self_import {
            imported = self.lookup(cx, module, kind, names);
            imported.values = None;
            imported.macros = None;
        } else if leaf {
            let Some(name) = name.clone() else { return };
            imported = self.lookup(cx, module, kind, names);
            for ns in Ns::ALL {
                let Some(found) = entry(&imported, ns) else { continue };
                match found.import {
                    Some(found) if Some(found) == binding => {
                        self.taint(cx, module, &name, lexical, binding);
                    }
                    Some(found) => {
                        let used = if lexical { Used::Scope } else { Used::Other };
                        self.mark(cx, found, used, &name, ns);
                    }
                    None => {}
                }
            }
        }
        let tree = &mut self.items[item].trees[j];
        tree.name = name;
        tree.lexical = lexical;
        tree.source = if tree_kind == TreeKind::Glob { entry(&self.lookup(cx, module, kind, names), Ns::Types).map(|it| it.def) } else { source };
        tree.imported = imported;
    }

    /// Marks what the paths, method calls, macro calls and derives of `node` go through.
    fn node(&mut self, cx: &Cx<'_, '_>, walk: &Walk<'_>, file: HirFileId, node: &SyntaxNode) {
        let kind = node.kind();
        if kind == SyntaxKind::PATH {
            let path = ast::Path::cast(node.clone()).unwrap();
            self.path(cx, file, &path);
            if path.parent_path().is_none()
                && let Some(parent) = path.syntax().parent()
                && matches!(parent.kind(), SyntaxKind::PATH_EXPR | SyntaxKind::PATH_PAT | SyntaxKind::TUPLE_STRUCT_PAT)
            {
                self.type_relative(cx, &path);
            }
        } else if kind == SyntaxKind::METHOD_CALL_EXPR {
            let call = ast::MethodCallExpr::cast(node.clone()).unwrap();
            let Some(name) = call.name_ref() else { return };
            let resolved = cx.sema.resolve_method_call(&call).map(|it| it.as_assoc_item(cx.db));
            self.lookup_trait_item(cx, call.syntax(), &name.as_name(), Some(resolved.flatten()));
        } else if kind == SyntaxKind::FIELD_EXPR {
            let field = ast::FieldExpr::cast(node.clone()).unwrap();
            let Some(name) = field.name_ref() else { return };
            self.lookup_trait_item(cx, field.syntax(), &name.as_name(), None);
        } else if kind == SyntaxKind::MACRO_CALL {
            let call = ast::MacroCall::cast(node.clone()).unwrap();
            match cx.sema.resolve_macro_call(&call) {
                Some(it) => {
                    let id = MacroId::from(it);
                    self.macros.insert(id);
                    match cx.sema.resolve_macro_call_arm(&call) {
                        Some(arm) => {
                            self.arms.entry(id).or_default().insert(arm);
                        }
                        None => {
                            self.arms_unknown.insert(id);
                        }
                    }
                }
                None => {
                    if let Some(name) = call.path().and_then(|it| it.segment()).and_then(|it| it.name_ref()) {
                        self.macros_unknown.insert(name.as_name());
                    }
                }
            }
        } else if ast::Adt::can_cast(kind)
            && let Some(owner) = ast::AnyHasAttrs::cast(node.clone())
            && owner.attrs().next().is_some()
        {
            for (meta, attr) in walk.attrs(&owner) {
                let ast::Meta::TokenTreeMeta(it) = &meta else { continue };
                if it.path().and_then(|it| it.as_single_name_ref()).is_none_or(|it| it.text() != "derive") {
                    continue;
                }
                self.derive(cx, node, &meta, &attr);
            }
        }
    }

    /// Marks what each path of a `#[derive(..)]` goes through.
    fn derive(&mut self, cx: &Cx<'_, '_>, owner: &SyntaxNode, meta: &ast::Meta, attr: &ast::Attr) {
        let Some(scope) = cx.sema.scope(owner) else { return };
        let module = ModuleId::from(scope.module());
        let resolved = cx.sema.resolve_derive_macro(meta).unwrap_or_default();
        for (k, (_, text)) in crate::cx::derive_paths(attr).into_iter().enumerate() {
            let mut kind = PathKind::Plain;
            let mut names = Vec::new();
            for (i, segment) in text.split("::").enumerate() {
                match segment {
                    "" if i == 0 => kind = PathKind::Abs,
                    "crate" if i == 0 => kind = PathKind::Crate,
                    "self" if i == 0 => kind = PathKind::Super(0),
                    "super" => {
                        kind = match kind {
                            PathKind::Super(n) => PathKind::Super(n + 1),
                            _ => PathKind::Super(1),
                        }
                    }
                    name => names.push(Name::new_root(name.trim_start_matches("r#"))),
                }
            }
            if names.is_empty() {
                continue;
            }
            let expected = resolved.get(k).copied().flatten();
            if let Some(it) = expected {
                let id = MacroId::from(it);
                self.macros.insert(id);
                self.arms_unknown.insert(id);
            }
            for n in 0..names.len() {
                let used = if n == 0 && kind == PathKind::Plain { Used::Scope } else { Used::Other };
                let per = self.lookup(cx, module, kind, &names[..=n]);
                let (ns, expected) = if n + 1 == names.len() {
                    (Ns::Macros, expected.map(|it| ModuleDefId::MacroId(it.into())))
                } else {
                    (Ns::Types, entry(&per, Ns::Types).map(|it| it.def).filter(|it| matches!(it, ModuleDefId::ModuleId(_))))
                };
                match (entry(&per, ns), expected) {
                    (Some(found), Some(expected)) if found.def == expected => {
                        if let Some(binding) = found.import {
                            self.mark(cx, binding, used, &names[n], ns);
                        }
                    }
                    _ => {
                        self.taint(cx, module, &names[n], n == 0 && kind == PathKind::Plain, None);
                        break;
                    }
                }
            }
        }
    }

    /// Marks the attribute macro that replaces `item`, and what its attributes' paths go
    /// through.
    fn attr_macro(&mut self, cx: &Cx<'_, '_>, item: InFile<ast::Item>) {
        if let Some(it) = cx.sema.resolve_attr_macro_call(&item.value) {
            let id = MacroId::from(it);
            self.macros.insert(id);
            self.arms_unknown.insert(id);
        }
        for attr in item.value.attrs() {
            if let Some(path) = attr.path() {
                for path in path.syntax().descendants().filter_map(ast::Path::cast) {
                    self.path(cx, item.file_id, &path);
                }
            }
        }
    }

    /// Marks what `path`, not a `use` tree's, goes through: each prefix in the type namespace
    /// as a module, the whole path in the namespaces hir resolves it in.
    fn path(&mut self, cx: &Cx<'_, '_>, file: HirFileId, path: &ast::Path) {
        if path.parent_path().is_some() || path.syntax().parent().is_some_and(|it| it.kind() == SyntaxKind::USE_TREE) {
            return;
        }
        let Some(scope) = cx.sema.scope(path.syntax()) else { return };
        let module = ModuleId::from(scope.module());
        let span_map = file.span_map(cx.db);
        let mut chain = vec![path.clone()];
        while let Some(qualifier) = chain.last().and_then(|it| it.qualifier()) {
            chain.push(qualifier);
        }
        chain.reverse();
        let mut done = 0;
        for (i, prefix) in chain.iter().enumerate() {
            let Some(mod_path) = ModPath::from_src(cx.db, prefix.clone(), &mut |range| span_map.span_for_range(range).ctx) else {
                return;
            };
            let names = mod_path.segments();
            if names.len() == done {
                continue;
            }
            done = names.len();
            let kind = mod_path.kind;
            let name = names[names.len() - 1].clone();
            let lexical = kind == PathKind::Plain && names.len() == 1;
            let used = if lexical { Used::Scope } else { Used::Other };
            let per = self.lookup(cx, module, kind, names);
            let in_module = if lexical {
                Some(module)
            } else {
                match entry(&self.lookup(cx, module, kind, &names[..names.len() - 1]), Ns::Types) {
                    Some(Entry { def: ModuleDefId::ModuleId(it), .. }) => Some(it),
                    _ => None,
                }
            };
            let last = i + 1 == chain.len();
            let mut module_prefix = false;
            let expected = if last {
                expected(cx, path)
            } else {
                match cx.sema.resolve_path(prefix) {
                    Some(PathResolution::Def(def)) => {
                        module_prefix = matches!(def, ModuleDef::Module(_));
                        match ModuleDefId::try_from(def) {
                            Ok(def) => Expected::Defs(vec![(Ns::Types, def)]),
                            Err(()) => Expected::Other,
                        }
                    }
                    Some(_) => Expected::Other,
                    None => Expected::Unknown,
                }
            };
            match expected {
                Expected::Defs(defs) => {
                    for (ns, def) in defs {
                        match entry(&per, ns) {
                            Some(found) if found.def == def => {
                                if let Some(binding) = found.import {
                                    self.mark(cx, binding, used, &name, ns);
                                }
                            }
                            _ => {
                                if let Some(at) = in_module {
                                    self.taint(cx, at, &name, lexical, None);
                                }
                            }
                        }
                    }
                }
                Expected::Unknown => {
                    if let Some(at) = in_module {
                        self.taint(cx, at, &name, lexical, None);
                    }
                    return;
                }
                Expected::Other => return,
            }
            if !last && !module_prefix {
                return;
            }
        }
    }

    /// Records the traits in scope for a value path whose type rustc resolves only in type
    /// checking (`<T>::f`, `T::f`, `Self::f`, `Alias::f`).
    fn type_relative(&mut self, cx: &Cx<'_, '_>, path: &ast::Path) {
        let Some(qualifier) = path.qualifier() else { return };
        let Some(name) = path.segment().and_then(|it| it.name_ref()) else { return };
        let deferred = match qualifier.segment().and_then(|it| it.kind()) {
            Some(ast::PathSegmentKind::Type { trait_ref: Some(_), .. }) => false,
            Some(ast::PathSegmentKind::Type { .. } | ast::PathSegmentKind::SelfTypeKw) => true,
            _ => match cx.sema.resolve_path(&qualifier) {
                Some(PathResolution::Def(ModuleDef::Adt(hir::Adt::Enum(_)))) => {
                    !matches!(cx.sema.resolve_path(path), Some(PathResolution::Def(ModuleDef::EnumVariant(_))))
                }
                Some(PathResolution::Def(
                    ModuleDef::Adt(_) | ModuleDef::TypeAlias(_) | ModuleDef::BuiltinType(_),
                ))
                | Some(PathResolution::TypeParam(_) | PathResolution::SelfType(_)) => true,
                _ => false,
            },
        };
        if !deferred {
            return;
        }
        let resolved = match cx.sema.resolve_path(path) {
            Some(PathResolution::Def(ModuleDef::Function(it))) => it.as_assoc_item(cx.db),
            Some(PathResolution::Def(ModuleDef::Const(it))) => it.as_assoc_item(cx.db),
            _ => None,
        };
        self.lookup_trait_item(cx, path.syntax(), &name.as_name(), Some(resolved));
    }

    /// Records the trait imports a lookup of `name` at `at` could use, and for a method call
    /// or type-relative path (`picked` is `Some`) those type checking uses: the first
    /// candidate of the trait of what it resolves to; every candidate when it does not resolve.
    fn lookup_trait_item(&mut self, cx: &Cx<'_, '_>, at: &SyntaxNode, name: &Name, picked: Option<Option<hir::AssocItem>>) {
        let Some(scope) = cx.sema.scope(at) else { return };
        let module = ModuleId::from(scope.module());
        let traits = self.traits_in_scope(cx, module, name);
        for (_, chain) in &traits {
            for binding in chain {
                if !self.candidates.contains(binding) {
                    self.candidates.push(*binding);
                }
            }
        }
        match picked {
            None => {}
            Some(Some(item)) => {
                let Some(trait_) = item.container_or_implemented_trait(cx.db) else { return };
                let trait_ = TraitId::from(trait_);
                if let Some((_, chain)) = traits.iter().find(|(it, _)| *it == trait_) {
                    self.typeck.extend(chain.iter().copied());
                }
            }
            Some(None) => {
                for (_, chain) in &traits {
                    self.typeck.extend(chain.iter().copied());
                }
            }
        }
    }

    /// The traits in scope at `module` with an associated value named `name`, from the
    /// innermost block out to the nearest module, each with the imports it is found through
    /// (rustc's `TraitCandidate`'s `import_ids`).
    fn traits_in_scope(&mut self, cx: &Cx<'_, '_>, module: ModuleId, name: &Name) -> Vec<(TraitId, Vec<Binding>)> {
        if let Some(it) = self.traits.get(&(module, name.clone())) {
            return it.clone();
        }
        let db = cx.db;
        let mut found = Vec::new();
        let mut at = Some(module);
        while let Some(id) = at {
            let def_map = id.def_map(db);
            let mut seen = FxHashSet::default();
            let traits: Vec<TraitId> = def_map.module(db, id).traits_in_scope(db).collect();
            for trait_ in traits {
                if !seen.insert(trait_) || !has_value_item(cx, trait_, name) {
                    continue;
                }
                for binding in self.trait_bindings(cx, id, trait_) {
                    found.push((trait_, self.trait_chain(cx, binding, trait_)));
                }
            }
            at = if id.is_block_module(db) { def_map.containing_module(db, id) } else { None };
        }
        self.traits.insert((module, name.clone()), found.clone());
        found
    }

    /// What `trait_` is in scope through in `module`: each name it has there, each `_` import
    /// of it, else each glob import that brings it in unnamed; `None` when it is in scope
    /// through none of those.
    fn trait_bindings(&self, cx: &Cx<'_, '_>, module: ModuleId, trait_: TraitId) -> Vec<Option<Binding>> {
        let db = cx.db;
        let scope = module.def_map(db).module(db, module).scope(db);
        let mut out: Vec<Option<Binding>> = scope
            .types()
            .filter(|(_, it)| it.def == ModuleDefId::TraitId(trait_))
            .map(|(_, it)| it.import)
            .collect();
        let local = self.items.iter().filter(|it| it.module == module).flat_map(|it| it.trees.iter());
        for tree in local.clone() {
            if tree.underscore
                && entry(&tree.imported, Ns::Types).is_some_and(|it| it.def == ModuleDefId::TraitId(trait_))
            {
                out.push(tree.binding);
            }
        }
        if out.is_empty() {
            for tree in local.filter(|it| it.kind == TreeKind::Glob) {
                let Some(ModuleDefId::ModuleId(from)) = tree.source else { continue };
                if from.def_map(db).module(db, from).traits_in_scope(db).any(|it| it == trait_) {
                    out.push(tree.binding);
                }
            }
        }
        if out.is_empty() {
            out.push(None);
        }
        out
    }

    /// The local imports `binding` goes through to `trait_`: rustc's `find_transitive_imports`.
    fn trait_chain(&self, cx: &Cx<'_, '_>, binding: Option<Binding>, trait_: TraitId) -> Vec<Binding> {
        let mut chain = Vec::new();
        let mut at = binding;
        while let Some(binding) = at {
            if !self.is_local(cx, binding) || chain.contains(&binding) {
                break;
            }
            chain.push(binding);
            at = match binding {
                Binding::Import(_) => self
                    .tree(binding)
                    .and_then(|(_, tree)| entry(&tree.imported, Ns::Types))
                    .and_then(|it| it.import),
                Binding::Glob(_) => match self.tree(binding).and_then(|(_, tree)| tree.source) {
                    Some(ModuleDefId::ModuleId(from)) => {
                        self.trait_bindings(cx, from, trait_).into_iter().next().flatten()
                    }
                    _ => None,
                },
                Binding::ExternCrate(_) => None,
            };
        }
        chain
    }

    /// The bindings re-exported: rustc's effective visibilities, from the crate root through
    /// every public name, into each public module and along each public import's chain.
    fn export(&mut self, cx: &Cx<'_, '_>) {
        let db = cx.db;
        let mut stack: Vec<ModuleId> = vec![cx.krate.root_module(db).into()];
        while let Some(module) = stack.pop() {
            if !self.public_modules.insert(module) {
                continue;
            }
            let scope = module.def_map(db).module(db, module).scope(db);
            let entries: Vec<(Name, PerNs)> = scope.entries().map(|(name, per)| (name.clone(), per)).collect();
            for (name, per) in entries {
                for ns in Ns::ALL {
                    let Some(found) = entry(&per, ns) else { continue };
                    if found.vis != Visibility::Public {
                        continue;
                    }
                    if let ModuleDefId::ModuleId(child) = found.def
                        && child.krate(db) == self.krate
                    {
                        stack.push(child);
                    }
                    let mut at = found.import;
                    while let Some(binding) = at {
                        if !self.is_local(cx, binding) || !self.exported_names.insert((binding, name.clone(), ns)) {
                            break;
                        }
                        self.exported.insert(binding);
                        at = self
                            .next(cx, binding, &name, ns)
                            .filter(|it| it.vis == Visibility::Public)
                            .and_then(|it| it.import);
                    }
                }
            }
        }
    }
}

/// What a path resolves to, as hir has it.
enum Expected {
    /// These definitions, each in its namespace.
    Defs(Vec<(Ns, ModuleDefId)>),
    /// Something no import is found through: a local, a generic parameter, a builtin attribute.
    Other,
    /// It does not resolve.
    Unknown,
}

/// What hir resolves the whole of `path` to, in the namespaces it is used in.
fn expected(cx: &Cx<'_, '_>, path: &ast::Path) -> Expected {
    let defs = |defs: Vec<(Ns, Option<PathResolution<'_>>)>| {
        let any = defs.iter().any(|(_, it)| it.is_some());
        let defs: Vec<(Ns, ModuleDefId)> = defs
            .into_iter()
            .filter_map(|(ns, it)| match it {
                Some(PathResolution::Def(def)) => ModuleDefId::try_from(def).ok().map(|it| (ns, it)),
                _ => None,
            })
            .collect();
        match (defs.is_empty(), any) {
            (false, _) => Expected::Defs(defs),
            (true, true) => Expected::Other,
            (true, false) => Expected::Unknown,
        }
    };
    let parent = path.syntax().parent();
    if let Some(call) = parent.clone().and_then(ast::MacroCall::cast) {
        return match cx.sema.resolve_macro_call(&call) {
            Some(it) => Expected::Defs(vec![(Ns::Macros, ModuleDefId::MacroId(it.into()))]),
            None => Expected::Unknown,
        };
    }
    if parent.is_some_and(|it| ast::Meta::can_cast(it.kind())) {
        return defs(vec![(Ns::Macros, cx.sema.resolve_path(path))]);
    }
    match cx.sema.resolve_path_per_ns(path) {
        Some(it) => defs(vec![(Ns::Types, it.type_ns), (Ns::Values, it.value_ns), (Ns::Macros, it.macro_ns)]),
        None => Expected::Unknown,
    }
}

/// What `kind` and `names` resolve to from `module`, through which bindings: hir-def's
/// `resolve_module_path_in_items`.
fn resolve_in_items(cx: &Cx<'_, '_>, module: ModuleId, kind: PathKind, names: &[Name]) -> PerNs {
    let path = ModPath::from_segments(kind, names.iter().cloned());
    module.resolver(cx.db).resolve_module_path_in_items(cx.db, &path)
}

/// A memoized [`resolve_in_items`]. A lookup costs one FxHash of the module, the kind and each
/// name's symbol pointer; a hit allocates and clones nothing.
struct Lookup {
    module: ModuleId,
    kind: PathKind,
    names: SmallVec<[Name; 4]>,
    per: PerNs,
}

fn lookup_hash(module: ModuleId, kind: PathKind, names: &[Name]) -> u64 {
    let mut hasher = FxHasher::default();
    module.hash(&mut hasher);
    kind.hash(&mut hasher);
    names.hash(&mut hasher);
    hasher.finish()
}

/// Whether `trait_` has an associated function or constant named `name`: rustc's
/// `trait_may_have_item` in the value namespace.
fn has_value_item(cx: &Cx<'_, '_>, trait_: TraitId, name: &Name) -> bool {
    hir::Trait::from(trait_).items(cx.db).into_iter().any(|it| {
        matches!(it, hir::AssocItem::Function(_) | hir::AssocItem::Const(_)) && it.name(cx.db).as_ref() == Some(name)
    })
}

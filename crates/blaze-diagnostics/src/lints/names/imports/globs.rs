//! What rustc lints of the names imports bring in once every import is resolved: a glob
//! import more visible than anything it imports (`unused_imports`), a `pub use` of a private
//! `extern crate` item (`pub_use_of_private_extern_crate`), two public globs that re-export
//! different items under one name (`ambiguous_glob_reexports`), and a private item that
//! shadows a name a public glob re-exports (`hidden_glob_reexports`).
//!
//! A glob from a module with unnamed trait imports (`use Trait as _`) is not checked for its
//! visibility: hir-def does not keep how visible those are.
//!
//! Follows rustc's `compiler/rustc_resolve/src/imports.rs` (`finalize_import`'s glob
//! visibility check and `report_cannot_reexport`, `pub_use_of_private_extern_crate_hack`,
//! `lint_reexports`, `select_glob_decl`'s glob-glob ambiguity, `new_import_decl`'s
//! `max_vis`), `compiler/rustc_middle/src/ty/mod.rs` (`Visibility::to_string`) and
//! `compiler/rustc_resolve/src/effective_visibilities.rs` (MIT or Apache-2.0).

use hir::{HasSource, InFile, ModuleDef};
use hir_def::{MacroId, ModuleDefId, ModuleId, visibility::Visibility};
use hir_expand::mod_path::ModPath;
use syntax::{
    AstNode, SyntaxNode, SyntaxNodePtr,
    ast::{self, HasVisibility},
};

use super::used::{Binding, Entry, Ns, TreeKind, UseItem, Uses, entry, lookup};
use crate::{
    cx::{Cx, item_range},
    lower::{Found, Phase},
    model::{Applicability, Diag},
};

/// The lints of this module, in the order rustc buffers them: each import's as it is
/// finalized, then each module's names'.
pub(super) fn lints(cx: &Cx<'_, '_>, uses: &Uses) -> Vec<Found> {
    let mut found = Vec::new();
    for item in &uses.items {
        for tree in &item.trees {
            match tree.kind {
                TreeKind::Glob => found.extend(glob_visibility(cx, item, tree)),
                TreeKind::Simple => found.extend(private_extern_crate(cx, uses, item, tree)),
                TreeKind::List => {}
            }
        }
    }
    let mut modules: Vec<ModuleId> = uses.items.iter().map(|it| it.module).collect();
    modules.dedup();
    let mut seen = rustc_hash::FxHashSet::default();
    modules.retain(|it| seen.insert(*it));
    for module in modules {
        found.extend(reexports(cx, uses, module));
    }
    found
}

/// rustc's check that a glob import re-exports something as visibly as it is declared: its
/// visibility greater than `max_vis`, the greatest of those of what it imports.
fn glob_visibility(cx: &Cx<'_, '_>, item: &UseItem, tree: &Tree) -> Option<Found> {
    let db = cx.db;
    let Some(ModuleDefId::ModuleId(from)) = tree.source else { return None };
    let home = nearest_module(cx, item.module);
    if from == item.module {
        return None;
    }
    let import_vis = item_visibility(cx, item)?;
    let scope = from.def_map(db).module(db, from).scope(db);
    let named: Vec<hir_def::TraitId> = scope
        .types()
        .filter_map(|(_, it)| match it.def {
            ModuleDefId::TraitId(it) => Some(it),
            _ => None,
        })
        .collect();
    if from.def_map(db).module(db, from).traits_in_scope(db).any(|it| !named.contains(&it)) {
        return None;
    }
    let mut max: Option<Vis> = None;
    for (_, per) in scope.entries() {
        for ns in Ns::ALL {
            let Some(found) = entry(&per, ns) else { continue };
            if !found.vis.is_visible_from(db, item.module) {
                continue;
            }
            if let ModuleDefId::MacroId(MacroId::MacroRulesId(_)) = found.def
                && found.vis != Visibility::Public
            {
                continue;
            }
            let vis = Vis::min(cx, Vis::of(cx, found.vis), import_vis);
            if vis == import_vis || max.is_none_or(|max| Vis::greater_than(cx, vis, max)) {
                max = Some(vis);
            }
        }
    }
    let max = max?;
    if !Vis::greater_than(cx, import_vis, max) {
        return None;
    }
    let span = cx.span(InFile::new(item.node.file_id, tree.node.syntax().text_range()));
    let message = format!(
        "glob import doesn't reexport anything with visibility `{}` because no imported item is public enough",
        import_vis.to_string(cx, home)
    );
    let mut diag = Diag::lint("unused_imports", message, span.clone());
    diag.span_note(span, format!("the most public imported item is `{}`", max.to_string(cx, home)));
    diag.help("reduce the glob import's visibility or increase visibility of imported items");
    diag.fact(cx.resolves_fact(from.into(), "*"), "what the glob import brings in");
    let at = InFile::new(item.node.file_id, SyntaxNodePtr::new(tree.node.syntax()));
    Some(Found::lint(diag, Phase::Resolve, at))
}

/// `pub_use_of_private_extern_crate`: a `pub` single import of nothing but an `extern crate`
/// item less visible than it.
fn private_extern_crate(cx: &Cx<'_, '_>, uses: &Uses, item: &UseItem, tree: &super::used::Tree) -> Option<Found> {
    if !item.public || tree.self_import || tree.binding.is_none() {
        return None;
    }
    let Some(Entry { import: Some(Binding::ExternCrate(id)), .. }) = entry(&tree.imported, Ns::Types) else {
        return None;
    };
    if entry(&tree.imported, Ns::Values).is_some() || entry(&tree.imported, Ns::Macros).is_some() {
        return None;
    }
    let krate = uses.extern_crate_of(id)?;
    if krate.public {
        return None;
    }
    let path = tree.node.path()?;
    let ident = path.segment()?.name_ref()?.text().to_string();
    let span = cx.span(InFile::new(item.node.file_id, tree.node.syntax().text_range()));
    let mut diag = Diag::lint(
        "pub_use_of_private_extern_crate",
        format!("extern crate `{ident}` is private and cannot be re-exported"),
        span,
    );
    let start = item_range(krate.node.value.syntax());
    let sugg = cx.span(InFile::new(krate.node.file_id, start)).shrink_to_lo();
    diag.suggestion_verbose(
        sugg,
        "consider making the `extern crate` item publicly accessible",
        "pub ",
        Applicability::MaybeIncorrect,
    );
    diag.fact(cx.resolves_fact(item.module.into(), &ident), format!("what `{ident}` is imported from"));
    let at = InFile::new(item.node.file_id, SyntaxNodePtr::new(tree.node.syntax()));
    Some(Found::lint(diag, Phase::Resolve, at))
}

/// `ambiguous_glob_reexports` and `hidden_glob_reexports` of the names of `module`.
fn reexports(cx: &Cx<'_, '_>, uses: &Uses, module: ModuleId) -> Vec<Found> {
    let db = cx.db;
    let mut found = Vec::new();
    let globs: Vec<(&UseItem, &super::used::Tree, ModuleId)> = uses
        .items
        .iter()
        .filter(|it| it.module == module)
        .flat_map(|item| item.trees.iter().map(move |tree| (item, tree)))
        .filter_map(|(item, tree)| match (tree.kind, tree.source) {
            (TreeKind::Glob, Some(ModuleDefId::ModuleId(from))) if from != module => Some((item, tree, from)),
            _ => None,
        })
        .collect();
    if globs.is_empty() {
        return found;
    }
    let here = module.def_map(db).module(db, module).scope(db);
    let mut names: Vec<hir_expand::name::Name> = Vec::new();
    for (_, _, from) in &globs {
        for (name, _) in from.def_map(db).module(db, *from).scope(db).entries() {
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
    }
    for name in names {
        let current = here.get(&name);
        for ns in Ns::ALL {
            let imported: Vec<(&UseItem, &super::used::Tree, Entry)> = globs
                .iter()
                .filter_map(|(item, tree, from)| {
                    let per = from.def_map(db).module(db, *from).name_in_scope(db, &name);
                    let it = entry(&per, ns)?;
                    it.vis.is_visible_from(db, module).then_some((*item, *tree, it))
                })
                .collect();
            let Some(&(first_item, first_tree, first)) = imported.first() else { continue };
            let current = entry(&current, ns);
            let glob_binding = |it: &Entry| matches!(it.import, Some(Binding::Glob(_)));
            match current {
                Some(it) if glob_binding(&it) => {
                    let public = first_item.public && first.vis == Visibility::Public;
                    if !public || !uses.public_modules.contains(&module) {
                        continue;
                    }
                    let Some(&(_, other_tree, _)) = imported.iter().find(|(_, _, it)| it.def != first.def) else {
                        continue;
                    };
                    let root = first_item.trees.first().map(|it| it.node.syntax().text_range());
                    let Some(root) = root else { continue };
                    let span = cx.span(InFile::new(first_item.node.file_id, root));
                    let text = name.as_str().to_owned();
                    let mut diag = Diag::lint("ambiguous_glob_reexports", "ambiguous glob re-exports", span.clone());
                    diag.span_label(
                        span,
                        format!("the name `{text}` in the {} namespace is first re-exported here", ns.descr()),
                    );
                    let other = cx.span(InFile::new(first_item.node.file_id, other_tree.node.syntax().text_range()));
                    diag.span_label(
                        other,
                        format!("but the name `{text}` in the {} namespace is also re-exported here", ns.descr()),
                    );
                    diag.fact(cx.resolves_fact(module.into(), &text), format!("what `{text}` is in the module"));
                    let at = InFile::new(first_item.node.file_id, SyntaxNodePtr::new(first_item.node.value.syntax()));
                    found.push(Found::lint(diag, Phase::Resolve, at));
                }
                Some(it) => {
                    if it.vis == Visibility::Public || matches!(it.def, ModuleDefId::MacroId(MacroId::MacroRulesId(_))) {
                        continue;
                    }
                    let Some(&(glob_item, glob_tree, _)) = imported.iter().find(|(item, tree, it)| {
                        item.public
                            && it.vis == Visibility::Public
                            && tree.binding.is_some_and(|it| uses.exported.contains(&it))
                    }) else {
                        continue;
                    };
                    let Some(private) = binding_node(cx, uses, it) else { continue };
                    let text = name.as_str().to_owned();
                    let span = cx.span(private.map(|it| item_span(&it)));
                    let mut diag =
                        Diag::lint("hidden_glob_reexports", "private item shadows public glob re-export", span.clone());
                    let glob = cx.span(InFile::new(glob_item.node.file_id, glob_tree.node.syntax().text_range()));
                    diag.span_note(
                        glob,
                        format!(
                            "the name `{text}` in the {} namespace is supposed to be publicly re-exported here",
                            ns.descr()
                        ),
                    );
                    diag.span_note(span, "but the private item here shadows it");
                    diag.fact(cx.resolves_fact(module.into(), &text), format!("what `{text}` is in the module"));
                    found.push(Found::lint(diag, Phase::Resolve, private.map(|it| SyntaxNodePtr::new(&it))));
                }
                None => {}
            }
        }
    }
    found
}

/// The node of what a name is bound to in a module, when it is not a glob's: the item that
/// defines it, the `use` tree or the `extern crate` item that imports it.
fn binding_node(cx: &Cx<'_, '_>, uses: &Uses, it: Entry) -> Option<InFile<SyntaxNode>> {
    match it.import {
        Some(binding @ Binding::Import(_)) => {
            let (item, tree) = uses.tree(binding)?;
            Some(InFile::new(item.node.file_id, tree.node.syntax().clone()))
        }
        Some(Binding::ExternCrate(id)) => {
            let krate = uses.extern_crate_of(id)?;
            Some(krate.node.as_ref().map(|it| it.syntax().clone()))
        }
        Some(Binding::Glob(_)) => None,
        None => def_node(cx, it.def),
    }
}

/// The item that defines `def`.
fn def_node(cx: &Cx<'_, '_>, def: ModuleDefId) -> Option<InFile<SyntaxNode>> {
    let db = cx.db;
    let syntax = |it: Option<InFile<_>>| it.map(|it: InFile<_>| it.map(|it: ast::Item| it.syntax().clone()));
    match ModuleDef::from(def) {
        ModuleDef::Module(it) => it.declaration_source(db).map(|it| it.map(|it| it.syntax().clone())),
        ModuleDef::Function(it) => syntax(it.source(db).map(|it| it.map(ast::Item::from))),
        ModuleDef::Adt(hir::Adt::Struct(it)) => syntax(it.source(db).map(|it| it.map(ast::Item::from))),
        ModuleDef::Adt(hir::Adt::Union(it)) => syntax(it.source(db).map(|it| it.map(ast::Item::from))),
        ModuleDef::Adt(hir::Adt::Enum(it)) => syntax(it.source(db).map(|it| it.map(ast::Item::from))),
        ModuleDef::Const(it) => syntax(it.source(db).map(|it| it.map(ast::Item::from))),
        ModuleDef::Static(it) => syntax(it.source(db).map(|it| it.map(ast::Item::from))),
        ModuleDef::Trait(it) => syntax(it.source(db).map(|it| it.map(ast::Item::from))),
        ModuleDef::TypeAlias(it) => syntax(it.source(db).map(|it| it.map(ast::Item::from))),
        ModuleDef::Macro(it) => it.source(db).map(|it| it.map(|it| it.syntax().clone())),
        ModuleDef::EnumVariant(_) | ModuleDef::BuiltinType(_) => None,
    }
}

/// rustc's span of a binding: an item's without its attributes, a `use` tree's whole.
fn item_span(node: &SyntaxNode) -> syntax::TextRange {
    if ast::UseTree::can_cast(node.kind()) { node.text_range() } else { item_range(node) }
}

/// The module rustc's privacy counts an item of `module` in: the nearest that is not a block.
fn nearest_module(cx: &Cx<'_, '_>, module: ModuleId) -> ModuleId {
    let db = cx.db;
    let mut at = module;
    while at.is_block_module(db) {
        match at.def_map(db).containing_module(db, at) {
            Some(it) => at = it,
            None => break,
        }
    }
    at
}

/// The visibility a `use` item is declared with, resolved.
fn item_visibility(cx: &Cx<'_, '_>, item: &UseItem) -> Option<Vis> {
    let db = cx.db;
    let home = nearest_module(cx, item.module);
    let Some(vis) = item.node.value.visibility() else { return Some(Vis::Module(home)) };
    Some(match vis.kind() {
        ast::VisibilityKind::Pub => Vis::Public,
        ast::VisibilityKind::PubCrate => Vis::Crate,
        ast::VisibilityKind::PubSelf => Vis::Module(home),
        ast::VisibilityKind::PubSuper => Vis::Module(home.def_map(db).containing_module(db, home)?),
        ast::VisibilityKind::In(path) => {
            let span_map = item.node.file_id.span_map(db);
            let path = ModPath::from_src(db, path, &mut |range| span_map.span_for_range(range).ctx)?;
            match entry(&lookup(cx, home, path.kind, path.segments()), Ns::Types)?.def {
                ModuleDefId::ModuleId(it) => Vis::module(cx, it),
                _ => return None,
            }
        }
    })
}

/// A visibility as rustc has it: public, the crate's, or restricted to a module that is not
/// the root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Vis {
    Public,
    Crate,
    Module(ModuleId),
}

impl Vis {
    fn of(cx: &Cx<'_, '_>, vis: Visibility) -> Vis {
        match vis {
            Visibility::Public => Vis::Public,
            Visibility::PubCrate(_) => Vis::Crate,
            Visibility::Module(module, _) => Vis::module(cx, module),
        }
    }

    /// Visible in `module`, the nearest module that is not a block.
    fn module(cx: &Cx<'_, '_>, module: ModuleId) -> Vis {
        let module = nearest_module(cx, module);
        if module.def_map(cx.db).module(cx.db, module).parent.is_none() && !module.is_block_module(cx.db) {
            Vis::Crate
        } else {
            Vis::Module(module)
        }
    }

    /// Whether `a` is at least `b`: everywhere `b` is visible, `a` is.
    fn at_least(cx: &Cx<'_, '_>, a: Vis, b: Vis) -> bool {
        match (a, b) {
            (Vis::Public, _) => true,
            (_, Vis::Public) => false,
            (Vis::Crate, _) => true,
            (_, Vis::Crate) => false,
            (Vis::Module(a), Vis::Module(b)) => {
                let mut at = Some(b);
                while let Some(module) = at {
                    if module == a {
                        return true;
                    }
                    at = module.def_map(cx.db).module(cx.db, module).parent;
                }
                false
            }
        }
    }

    fn greater_than(cx: &Cx<'_, '_>, a: Vis, b: Vis) -> bool {
        a != b && Vis::at_least(cx, a, b)
    }

    fn min(cx: &Cx<'_, '_>, a: Vis, b: Vis) -> Vis {
        if Vis::at_least(cx, b, a) { a } else { b }
    }

    /// rustc's `Visibility::to_string` for an item of `home`.
    fn to_string(self, cx: &Cx<'_, '_>, home: ModuleId) -> String {
        match self {
            Vis::Public => "pub".to_owned(),
            Vis::Crate => "pub(crate)".to_owned(),
            Vis::Module(module) if module == home => "pub(self)".to_owned(),
            Vis::Module(module) => {
                let mut names = Vec::new();
                let mut at = Some(module);
                while let Some(it) = at {
                    if let Some(name) = it.name(cx.db) {
                        names.push(name.as_str().to_owned());
                    }
                    at = it.def_map(cx.db).module(cx.db, it).parent;
                }
                names.reverse();
                format!("pub(in crate::{})", names.join("::"))
            }
        }
    }
}

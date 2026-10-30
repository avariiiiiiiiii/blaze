//! What rustc's `check_unused` lints once the crate is resolved: `#[macro_use]` imports
//! (`macro_use_extern_crate`, `unused_imports`), the `use` trees nothing is found through
//! (`unused_imports`, one diagnostic per `use` item, with the spans rustfix removes),
//! `extern crate` items (`unused_extern_crates`), and single imports of what is in scope
//! without them (`redundant_imports`); then, after type checking, the trait imports only a
//! method lookup could have used and none did (`unused_imports`).
//!
//! An `extern crate` item of a crate that may provide a global allocator, a panic handler, a
//! panic runtime, the compiler builtins or an externally implementable item is never
//! unused (rustc keeps those); where that can't be told of the crate, it is not linted. A
//! redundant import is looked for in the `use` item's module, its globs and the blocks around
//! it, the extern prelude and the builtin types; one rustc finds redundant through the
//! standard library's prelude, or in the macro namespace, is not linted.
//!
//! Follows rustc's `compiler/rustc_resolve/src/check_unused.rs` (`check_unused`,
//! `UnusedImportCheckVisitor`, `calc_unused_spans`, `report_unused_extern_crate_items`),
//! `compiler/rustc_resolve/src/imports.rs` (`check_for_redundant_imports`),
//! `compiler/rustc_resolve/src/lib.rs` (`module_to_string`),
//! `compiler/rustc_span/src/source_map.rs` (`guess_head_span`) and
//! `compiler/rustc_hir_analysis/src/check_unused.rs` (`check_unused_traits`) (MIT or
//! Apache-2.0).

use base_db::{CrateOrigin, LangCrateOrigin};
use cfg::CfgAtom;
use hir::InFile;
use hir_def::{ModuleDefId, ModuleId};
use hir_expand::name::Name;
use intern::sym;
use rustc_hash::{FxHashMap, FxHashSet};
use syntax::{
    AstNode, NodeOrToken, SyntaxNode, SyntaxNodePtr, TextRange,
    ast::{self, HasName},
};

use super::used::{Binding, Entry, ExternCrate, Ns, Tree, TreeKind, UseItem, Used, Uses, entry};
use crate::{
    cx::{Cx, item_range},
    lower::{Found, Phase},
    model::{Applicability, Diag, Span, SuggestionStyle},
};

/// What `check_unused` finds.
pub(super) struct Unused {
    /// Its lints, in the order rustc buffers them.
    pub(super) resolve: Vec<Found>,
    /// What `check_unused_traits` lints after type checking.
    pub(super) bodies: Vec<Found>,
    /// The `use` trees found unused.
    pub(super) unused: FxHashSet<Binding>,
    /// The single imports found redundant.
    pub(super) redundant: FxHashSet<Binding>,
}

impl Unused {
    pub(super) fn of(cx: &Cx<'_, '_>, uses: &Uses) -> Unused {
        let mut out =
            Unused { resolve: Vec::new(), bodies: Vec::new(), unused: FxHashSet::default(), redundant: FxHashSet::default() };
        let mut maybe_unused_crates = FxHashSet::default();
        for krate in &uses.crates {
            out.macro_use(cx, uses, krate);
            let binding = Binding::ExternCrate(krate.id);
            if !krate.public && !uses.used.contains_key(&binding) && krate.krate.is_some_and(|it| may_be_unused(cx, it)) {
                maybe_unused_crates.insert(krate.id);
            }
        }

        let mut candidates = uses.candidates.clone();
        let mut unused_trees: Vec<(usize, FxHashSet<usize>)> = Vec::new();
        for (i, item) in uses.items.iter().enumerate() {
            let mut unused = FxHashSet::default();
            for (j, tree) in item.trees.iter().enumerate() {
                if let Some(binding) = tree.binding
                    && uses.exported.contains(&binding)
                {
                    if tree.kind == TreeKind::Simple
                        && tree.underscore
                        && !matches!(entry(&tree.imported, Ns::Types), Some(Entry { def: ModuleDefId::TraitId(_), .. }))
                    {
                        unused.insert(j);
                    }
                    swap_remove(&mut candidates, binding);
                    continue;
                }
                match tree.kind {
                    TreeKind::List => {
                        if tree.node.use_tree_list().is_none_or(|it| it.use_trees().next().is_none()) {
                            unused.insert(j);
                        }
                    }
                    TreeKind::Simple | TreeKind::Glob => {
                        let Some(binding) = tree.binding else { continue };
                        if is_used(uses, binding, tree) {
                            swap_remove(&mut candidates, binding);
                            unused.remove(&j);
                        } else if !candidates.contains(&binding) {
                            unused.insert(j);
                        }
                    }
                }
            }
            if !unused.is_empty() {
                for &j in &unused {
                    if let Some(binding) = item.trees[j].binding {
                        out.unused.insert(binding);
                    }
                }
                unused_trees.push((i, unused));
            }
        }

        for krate in &uses.crates {
            out.extern_crate(cx, uses, krate, &maybe_unused_crates);
        }
        for (i, unused) in &unused_trees {
            out.unused_imports(cx, &uses.items[*i], unused);
        }

        let mut modules: Vec<ModuleId> = Vec::new();
        for item in &uses.items {
            if !modules.contains(&item.module) {
                modules.push(item.module);
            }
        }
        for module in modules {
            for item in uses.items.iter().filter(|it| it.module == module) {
                for tree in &item.trees {
                    let Some(binding) = tree.binding else { continue };
                    if tree.kind != TreeKind::Simple || out.unused.contains(&binding) {
                        continue;
                    }
                    if let Some(found) = redundant(cx, uses, item, tree, binding) {
                        out.redundant.insert(binding);
                        out.resolve.push(found);
                    }
                }
            }
        }

        for binding in candidates {
            let Some((item, tree)) = uses.tree(binding) else { continue };
            if item.public || uses.typeck.contains(&binding) {
                continue;
            }
            let Some(path) = tree.node.path() else { continue };
            let span = cx.span(InFile::new(item.node.file_id, path.syntax().text_range()));
            let message = match cx.snippet(&span) {
                Some(snippet) => format!("unused import: `{snippet}`"),
                None => "unused import".to_owned(),
            };
            let mut diag = Diag::lint("unused_imports", message, span);
            if let Some(name) = &tree.name {
                diag.fact(cx.resolves_fact(item.module.into(), name.as_str()), format!("what uses `{}`", name.as_str()));
            }
            let root = item.trees.first().is_some_and(|it| it.node == tree.node);
            let at = if root { item.node.value.syntax().clone() } else { tree.node.syntax().clone() };
            out.bodies.push(Found::lint(diag, Phase::Bodies, InFile::new(item.node.file_id, SyntaxNodePtr::new(&at))));
        }
        out
    }

    /// The lints of the `#[macro_use]` imports of `krate`: rustc's `MACRO_USE_EXTERN_CRATE`
    /// for a used one, `UNUSED_IMPORTS` for one nothing is found through.
    fn macro_use(&mut self, cx: &Cx<'_, '_>, uses: &Uses, krate: &ExternCrate) {
        let Some(loaded) = krate.krate else { return };
        let at = krate.node.as_ref().map(|it| SyntaxNodePtr::new(it.syntax()));
        for (range, name) in &krate.macro_use {
            if let Some(name) = name
                && !exports_macro(cx, loaded, name)
            {
                continue;
            }
            let span = cx.span(InFile::new(krate.node.file_id, *range));
            let mut diag = if uses.macro_uses.contains(&(krate.id, name.clone())) {
                let mut diag = Diag::lint(
                    "macro_use_extern_crate",
                    "applying the `#[macro_use]` attribute to an `extern crate` item is deprecated",
                    span,
                );
                diag.help("remove it and import macros at use sites with a `use` item instead");
                diag
            } else {
                Diag::lint("unused_imports", "unused `#[macro_use]` import", span)
            };
            let module = hir::Module::from(krate.module);
            diag.fact(cx.resolves_fact(module, &krate.name), format!("which macros of `{}` are used", krate.name));
            self.resolve.push(Found::lint(diag, Phase::Resolve, at));
        }
    }

    /// `unused_extern_crates` of `krate`: unused, or one a `use` could replace.
    fn extern_crate(&mut self, cx: &Cx<'_, '_>, uses: &Uses, krate: &ExternCrate, maybe_unused: &FxHashSet<hir_def::ExternCrateId>) {
        let node = &krate.node;
        let at = node.as_ref().map(|it| SyntaxNodePtr::new(it.syntax()));
        let span = cx.span(node.as_ref().map(|it| item_range(it.syntax())));
        let module = hir::Module::from(krate.module);
        let fact = cx.resolves_fact(module, &krate.name);
        if !krate.name.starts_with('_') && maybe_unused.contains(&krate.id) {
            let mut diag = Diag::lint("unused_extern_crates", "unused extern crate", span.clone());
            diag.span_label(span, "unused");
            let removal = cx.span(node.as_ref().map(|it| with_attrs(it.syntax())));
            diag.suggestion_verbose(removal, "remove the unused `extern crate`", "", Applicability::MachineApplicable);
            diag.fact(fact, format!("what uses `{}`", krate.name));
            self.resolve.push(Found::lint(diag, Phase::Resolve, at));
            return;
        }
        if !cx.edition.at_least_2018()
            || ast::HasAttrs::attrs(&node.value).next().is_some()
            || krate.renames
            || !uses.extern_flags(cx).contains(&krate.name)
            || no_implicit_prelude(cx, krate.module)
        {
            return;
        }
        let Some(ident) = node.value.name_ref() else { return };
        let item = item_range(node.value.syntax());
        let vis = match ast::HasVisibility::visibility(&node.value) {
            Some(vis) => vis.syntax().text_range(),
            None => TextRange::empty(item.start()),
        };
        let between = TextRange::new(vis.end(), ident.syntax().text_range().start());
        let code = if vis.is_empty() { "use " } else { " use " };
        let mut diag = Diag::lint("unused_extern_crates", "`extern crate` is not idiomatic in the new edition", span);
        diag.suggestion_verbose(
            cx.span(InFile::new(node.file_id, between)),
            "convert it to a `use`",
            code,
            Applicability::MachineApplicable,
        );
        diag.fact(fact, format!("what `{}` is in the extern prelude", krate.name));
        self.resolve.push(Found::lint(diag, Phase::Resolve, at));
    }

    /// The `unused_imports` diagnostic of `item`, whose trees `unused` are unused.
    fn unused_imports(&mut self, cx: &Cx<'_, '_>, item: &UseItem, unused: &FxHashSet<usize>) {
        let file = item.node.file_id;
        let children = children(item);
        let item_span = with_attrs(item.node.value.syntax());
        let (spans, remove) = match calc_unused_spans(item, &children, unused, 0, item_span) {
            Calc::Used => return,
            Calc::Unused { spans, remove } => (spans, vec![remove]),
            Calc::Partial { spans, remove } => (spans, remove),
        };
        let spans: Vec<Span> = spans.into_iter().map(|it| cx.span(InFile::new(file, it))).collect();
        let mut snippets: Vec<String> = spans.iter().filter_map(|it| cx.snippet(it)).map(|it| format!("`{it}`")).collect();
        snippets.sort();
        let list = and_list(&snippets);
        let message =
            if snippets.len() == 1 { format!("unused import: {list}") } else { format!("unused imports: {list}") };
        let Some((first, rest)) = spans.split_first() else { return };
        let mut diag = Diag::lint("unused_imports", message, first.clone());
        for span in rest {
            diag.primary(span.clone());
        }
        let remove_whole_use = remove.len() == 1 && remove[0] == item_span;
        let remove: Vec<Span> = remove.into_iter().map(|it| cx.span(InFile::new(file, it))).collect();
        if remove.iter().all(|it| it.expansion.is_none()) {
            if remove_whole_use {
                diag.suggestion_with_style(
                    vec![(remove[0].clone(), String::new())],
                    "remove the whole `use` item",
                    Applicability::MachineApplicable,
                    SuggestionStyle::CompletelyHidden,
                );
            } else {
                let message = if spans.len() == 1 { "remove the unused import" } else { "remove the unused imports" };
                diag.suggestion_with_style(
                    remove.into_iter().map(|it| (it, String::new())).collect(),
                    message,
                    Applicability::MachineApplicable,
                    SuggestionStyle::CompletelyHidden,
                );
            }
        }
        if !cx.krate.cfg(cx.db).check_atom(&CfgAtom::Flag(sym::test))
            && let Some(span) = test_module(cx, item.module)
        {
            diag.span_help(span, "if this is a test module, consider adding a `#[cfg(test)]` to the containing module");
        }
        let module = hir::Module::from(item.module);
        for &j in unused {
            if let Some(name) = &item.trees[j].name {
                diag.fact(cx.resolves_fact(module, name.as_str()), format!("what uses `{}`", name.as_str()));
            }
        }
        let at = InFile::new(file, SyntaxNodePtr::new(item.node.value.syntax()));
        self.resolve.push(Found::lint(diag, Phase::Resolve, at));
    }
}

/// Removes `binding` from `candidates` as an `IndexSet`'s `swap_remove` does: the last takes
/// its place.
fn swap_remove(candidates: &mut Vec<Binding>, binding: Binding) {
    if let Some(at) = candidates.iter().position(|it| *it == binding) {
        candidates.swap_remove(at);
    }
}

/// Whether name resolution used `binding`, or it did not resolve (rustc's dummy binding of
/// an unresolved import counts as used).
fn is_used(uses: &Uses, binding: Binding, tree: &Tree) -> bool {
    if uses.used.contains_key(&binding) {
        return true;
    }
    match tree.kind {
        TreeKind::Glob => tree.source.is_none(),
        _ => Ns::ALL.iter().all(|ns| entry(&tree.imported, *ns).is_none()),
    }
}

/// The trees directly in each tree of `item`, by index.
fn children(item: &UseItem) -> FxHashMap<usize, Vec<usize>> {
    let mut children: FxHashMap<usize, Vec<usize>> = FxHashMap::default();
    for (j, tree) in item.trees.iter().enumerate() {
        let Some(list) = tree.node.syntax().parent().and_then(ast::UseTreeList::cast) else { continue };
        let parent = list.parent_use_tree().syntax().text_range();
        if let Some(p) = item.trees.iter().position(|it| it.node.syntax().text_range() == parent) {
            children.entry(p).or_default().push(j);
        }
    }
    children
}

/// What rustfix removes of a `use` tree: rustc's `UnusedSpanResult`.
enum Calc {
    Used,
    Unused { spans: Vec<TextRange>, remove: TextRange },
    Partial { spans: Vec<TextRange>, remove: Vec<TextRange> },
}

/// rustc's `calc_unused_spans` of the tree `j` of `item`.
fn calc_unused_spans(
    item: &UseItem,
    children: &FxHashMap<usize, Vec<usize>>,
    unused: &FxHashSet<usize>,
    j: usize,
    item_span: TextRange,
) -> Calc {
    let tree = &item.trees[j];
    let span = tree.node.syntax().text_range();
    let full_span = if j == 0 { item_span } else { span };
    match tree.kind {
        TreeKind::Simple | TreeKind::Glob => {
            if unused.contains(&j) { Calc::Unused { spans: vec![span], remove: full_span } } else { Calc::Used }
        }
        TreeKind::List => {
            let nested = children.get(&j).map(Vec::as_slice).unwrap_or(&[]);
            if nested.is_empty() {
                return Calc::Unused { spans: vec![span], remove: full_span };
            }
            let mut unused_spans = Vec::new();
            let mut to_remove: Vec<TextRange> = Vec::new();
            let mut used_children = 0;
            let mut contains_self = false;
            let mut previous_unused = false;
            for (pos, &child) in nested.iter().enumerate() {
                let remove = match calc_unused_spans(item, children, unused, child, item_span) {
                    Calc::Used => {
                        used_children += 1;
                        None
                    }
                    Calc::Unused { mut spans, remove } => {
                        unused_spans.append(&mut spans);
                        Some(remove)
                    }
                    Calc::Partial { mut spans, remove: mut extra } => {
                        used_children += 1;
                        unused_spans.append(&mut spans);
                        to_remove.append(&mut extra);
                        None
                    }
                };
                if let Some(remove) = remove {
                    let child_span = item.trees[child].node.syntax().text_range();
                    let remove_span = if nested.len() == 1 {
                        remove
                    } else if pos == nested.len() - 1 || used_children > 0 {
                        let previous = item.trees[nested[pos - 1]].node.syntax().text_range();
                        TextRange::new(previous.end(), child_span.end())
                    } else {
                        let next = item.trees[nested[pos + 1]].node.syntax().text_range();
                        TextRange::new(child_span.start(), next.start())
                    };
                    match to_remove.pop() {
                        Some(previous) if previous_unused => to_remove.push(previous.cover(remove_span)),
                        Some(previous) => {
                            to_remove.push(previous);
                            to_remove.push(remove_span);
                        }
                        None => to_remove.push(remove_span),
                    }
                }
                let child_tree = &item.trees[child];
                contains_self |= child_tree.kind == TreeKind::Simple
                    && child_tree.node.path().is_some_and(|it| it.syntax().text() == "self")
                    && !unused.contains(&child);
                previous_unused = remove.is_some();
            }
            if unused_spans.is_empty() {
                Calc::Used
            } else if used_children == 0 {
                Calc::Unused { spans: unused_spans, remove: full_span }
            } else {
                if used_children == 1
                    && !contains_self
                    && let Some(list) = tree.node.use_tree_list()
                {
                    let list = list.syntax().text_range();
                    let first = item.trees[nested[0]].node.syntax().text_range();
                    let last = item.trees[nested[nested.len() - 1]].node.syntax().text_range();
                    to_remove.push(TextRange::new(list.start(), first.start()));
                    to_remove.push(TextRange::new(last.end(), list.end()));
                }
                Calc::Partial { spans: unused_spans, remove: to_remove }
            }
        }
    }
}

/// rustc's `redundant_imports` of the single import `tree`: each namespace it imports in
/// resolves to the same without it.
fn redundant(cx: &Cx<'_, '_>, uses: &Uses, item: &UseItem, tree: &Tree, binding: Binding) -> Option<Found> {
    if tree.self_import || tree.underscore || item.node.file_id.is_macro() {
        return None;
    }
    let name = tree.name.clone()?;
    let source = tree.node.path()?.segment()?.name_ref()?;
    if let Some(rename) = tree.node.rename()
        && rename.name().is_none_or(|it| it.text() != source.text())
    {
        return None;
    }
    if uses.used.get(&binding) == Some(&Used::Other) || uses.exported.contains(&binding) {
        return None;
    }
    let mut found: Vec<(Option<Span>, bool)> = Vec::new();
    let mut any = false;
    for ns in Ns::ALL {
        let Some(mine) = entry(&tree.imported, ns) else { continue };
        any = true;
        if ns == Ns::Macros {
            return None;
        }
        let (def, span, import) = without(cx, uses, item, binding, &name, ns)?;
        if def != mine.def {
            return None;
        }
        found.push((span, import));
    }
    if !any {
        return None;
    }
    found.sort_by_key(|(span, import)| (span.as_ref().map(|it| (it.lo, it.hi)), *import));
    found.dedup();
    let text = source.text().to_string();
    let span = cx.span(InFile::new(item.node.file_id, tree.node.syntax().text_range()));
    let mut diag = Diag::lint("redundant_imports", format!("the item `{text}` is imported redundantly"), span.clone());
    for (other, import) in found {
        let label = match (other.is_some(), import) {
            (true, true) => format!("the item `{text}` is already imported here"),
            (true, false) => format!("the item `{text}` is already defined here"),
            (false, true) => format!("the item `{text}` is already imported by the extern prelude"),
            (false, false) => format!("the item `{text}` is already defined by the extern prelude"),
        };
        diag.span_label(other.unwrap_or_else(|| span.clone()), label);
    }
    diag.fact(cx.resolves_fact(item.module.into(), &text), format!("what `{text}` is in scope as"));
    let at = InFile::new(item.node.file_id, SyntaxNodePtr::new(tree.node.syntax()));
    Some(Found::lint(diag, Phase::Resolve, at))
}

/// What `name` resolves to in `ns` where `item` is, were `binding` not there: rustc's
/// `resolve_ident_in_scope_set` ignoring the import. The definition, where the binding found
/// is (`None` for the extern prelude's and the builtin types'), and whether it is an import;
/// `None` when it is not found, or not where this can tell.
fn without(
    cx: &Cx<'_, '_>,
    uses: &Uses,
    item: &UseItem,
    binding: Binding,
    name: &Name,
    ns: Ns,
) -> Option<(ModuleDefId, Option<Span>, bool)> {
    let db = cx.db;
    let mut at = item.module;
    loop {
        if at == item.module {
            let mut globs = uses
                .items
                .iter()
                .filter(|it| it.module == at)
                .flat_map(|it| it.trees.iter().map(move |tree| (it, tree)))
                .filter(|(_, tree)| tree.kind == TreeKind::Glob && tree.binding != Some(binding))
                .filter_map(|(it, tree)| {
                    let Some(ModuleDefId::ModuleId(from)) = tree.source else { return None };
                    let found = entry(&from.def_map(db).module(db, from).name_in_scope(db, name), ns)?;
                    found.vis.is_visible_from(db, at).then_some((it, tree, found))
                });
            if let Some((glob_item, glob, found)) = globs.next() {
                if globs.any(|(_, _, it)| it.def != found.def) {
                    return None;
                }
                let span = cx.span(InFile::new(glob_item.node.file_id, glob.node.syntax().text_range()));
                return Some((found.def, Some(span), true));
            }
        } else if let Some(found) = entry(&at.def_map(db).module(db, at).name_in_scope(db, name), ns) {
            if let Some(Binding::Glob(_)) = found.import {
                let differs = uses
                    .items
                    .iter()
                    .filter(|it| it.module == at)
                    .flat_map(|it| it.trees.iter())
                    .filter_map(|tree| match (tree.kind, tree.source) {
                        (TreeKind::Glob, Some(ModuleDefId::ModuleId(from))) => {
                            entry(&from.def_map(db).module(db, from).name_in_scope(db, name), ns)
                        }
                        _ => None,
                    })
                    .any(|it| it.vis.is_visible_from(db, at) && it.def != found.def);
                if differs {
                    return None;
                }
            }
            let (span, import) = binding_span(cx, uses, found)?;
            return Some((found.def, Some(span), import));
        }
        if !at.is_block_module(db) {
            break;
        }
        at = at.def_map(db).containing_module(db, at)?;
    }
    if ns != Ns::Types {
        return None;
    }
    let root: ModuleId = cx.krate.root_module(db).into();
    let text = name.as_str();
    if let Some(krate) = uses.crates.iter().find(|it| it.module == root && it.name == text) {
        let loaded = krate.krate?;
        let span = cx.span(krate.node.as_ref().map(|it| item_range(it.syntax())));
        return Some((ModuleDefId::ModuleId(loaded.root_module(db).into()), Some(span), true));
    }
    if uses.extern_flags(cx).iter().any(|it| it == text) {
        let dep = cx.krate.dependencies(db).into_iter().find(|it| it.name.as_str() == text)?;
        return Some((ModuleDefId::ModuleId(dep.krate.root_module(db).into()), None, false));
    }
    let builtin = hir_def::builtin_type::BuiltinType::by_name(name)?;
    Some((ModuleDefId::BuiltinType(builtin), None, false))
}

/// Where the binding `found` is, and whether it is an import's.
fn binding_span(cx: &Cx<'_, '_>, uses: &Uses, found: Entry) -> Option<(Span, bool)> {
    match found.import {
        Some(binding @ (Binding::Import(_) | Binding::Glob(_))) => {
            let (item, tree) = uses.tree(binding)?;
            Some((cx.span(InFile::new(item.node.file_id, tree.node.syntax().text_range())), true))
        }
        Some(Binding::ExternCrate(id)) => {
            let krate = uses.extern_crate_of(id)?;
            Some((cx.span(krate.node.as_ref().map(|it| item_range(it.syntax()))), true))
        }
        None => {
            let node = def_item(cx, found.def)?;
            Some((cx.span(node.map(|it| item_range(&it))), false))
        }
    }
}

/// The item defining `def`, when it is one a block can have.
fn def_item(cx: &Cx<'_, '_>, def: ModuleDefId) -> Option<InFile<SyntaxNode>> {
    use hir::HasSource;
    fn node<N: AstNode>(it: Option<InFile<N>>) -> Option<InFile<SyntaxNode>> {
        it.map(|it| it.map(|it| it.syntax().clone()))
    }
    let db = cx.db;
    match hir::ModuleDef::from(def) {
        hir::ModuleDef::Module(it) => node(it.declaration_source(db)),
        hir::ModuleDef::Function(it) => node(it.source(db)),
        hir::ModuleDef::Adt(hir::Adt::Struct(it)) => node(it.source(db)),
        hir::ModuleDef::Adt(hir::Adt::Union(it)) => node(it.source(db)),
        hir::ModuleDef::Adt(hir::Adt::Enum(it)) => node(it.source(db)),
        hir::ModuleDef::Const(it) => node(it.source(db)),
        hir::ModuleDef::Static(it) => node(it.source(db)),
        hir::ModuleDef::Trait(it) => node(it.source(db)),
        hir::ModuleDef::TypeAlias(it) => node(it.source(db)),
        hir::ModuleDef::Macro(it) => node(it.source(db)),
        hir::ModuleDef::EnumVariant(_) | hir::ModuleDef::BuiltinType(_) => None,
    }
}

/// rustc's `span_with_attributes` of an item: from its first attribute or doc comment.
fn with_attrs(node: &SyntaxNode) -> TextRange {
    let start = node
        .children_with_tokens()
        .find(|it| match it {
            NodeOrToken::Node(_) => true,
            NodeOrToken::Token(token) => !token.kind().is_trivia(),
        })
        .map_or(node.text_range().start(), |it| it.text_range().start());
    TextRange::new(start, node.text_range().end())
}

/// rustc's `StrListSepByAnd`: `a`, `a and b`, `a, b, and c`.
fn and_list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [a, b] => format!("{a} and {b}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

/// Where the help on a test module points, when the `use` item's nearest module that is not a
/// block is named as a test module's: rustc's `guess_head_span` of the module.
fn test_module(cx: &Cx<'_, '_>, module: ModuleId) -> Option<Span> {
    let db = cx.db;
    let mut module = module;
    while module.is_block_module(db) {
        module = module.def_map(db).containing_module(db, module)?;
    }
    let path = module_path(cx, module)?;
    let test = path == "test"
        || path == "tests"
        || path.starts_with("test_")
        || path.starts_with("tests_")
        || path.ends_with("_test")
        || path.ends_with("_tests");
    if !test {
        return None;
    }
    let declared = hir::Module::from(module).declaration_source(db)?;
    let span = cx.span(declared.as_ref().map(|it| item_range(it.syntax())));
    let snippet = cx.snippet(&span)?;
    let head = snippet.split('{').next().unwrap_or("").trim_end();
    if head.is_empty() || head.contains('\n') {
        return Some(span);
    }
    Some(span.with_hi(span.lo + head.len() as u32))
}

/// rustc's `module_to_string`: the names from the root to `module`, `None` for the root.
fn module_path(cx: &Cx<'_, '_>, module: ModuleId) -> Option<String> {
    let db = cx.db;
    let mut names = Vec::new();
    let mut at = Some(module);
    while let Some(it) = at {
        if let Some(name) = it.name(db) {
            names.push(name.as_str().to_owned());
        }
        at = it.def_map(db).module(db, it).parent;
    }
    if names.is_empty() {
        return None;
    }
    names.reverse();
    Some(names.join("::"))
}

/// Whether `module`, or a module it is in, is `#![no_implicit_prelude]`.
fn no_implicit_prelude(cx: &Cx<'_, '_>, module: ModuleId) -> bool {
    let db = cx.db;
    let mut at = Some(module);
    while let Some(id) = at {
        let module = hir::Module::from(id);
        let source = module.definition_source(db).value;
        let mut attrs: Vec<ast::Attr> = match &source {
            hir::ModuleSource::SourceFile(it) => ast::attrs_including_inner(it).collect(),
            hir::ModuleSource::Module(it) => ast::attrs_including_inner(it).collect(),
            hir::ModuleSource::BlockExpr(_) => Vec::new(),
        };
        if let Some(declared) = module.declaration_source(db) {
            attrs.extend(ast::HasAttrs::attrs(&declared.value));
        }
        if attrs.iter().any(|it| it.path().and_then(|it| it.as_single_name_ref()).is_some_and(|it| it.text() == "no_implicit_prelude")) {
            return true;
        }
        at = id.def_map(db).containing_module(db, id);
    }
    false
}

/// Whether the crate `krate` exports a macro named `name` at its root.
fn exports_macro(cx: &Cx<'_, '_>, krate: hir::Crate, name: &Name) -> bool {
    let db = cx.db;
    let root: ModuleId = krate.root_module(db).into();
    root.def_map(db).module(db, root).scope(db).macros().any(|(it, _)| it == name)
}

/// Whether rustc may find an `extern crate` of `krate` unused: it provides none of what
/// rustc keeps such an item for (the compiler builtins, a panic runtime, a global allocator, a
/// panic handler, an externally implementable item). `false` where that can't be told.
fn may_be_unused(cx: &Cx<'_, '_>, krate: hir::Crate) -> bool {
    let db = cx.db;
    match &krate.base().data(db).origin {
        CrateOrigin::Lang(LangCrateOrigin::Core | LangCrateOrigin::Alloc | LangCrateOrigin::ProcMacro | LangCrateOrigin::Test) => {
            return true;
        }
        CrateOrigin::Lang(_) => return false,
        _ => {}
    }
    const KEPT: &[&str] = &["compiler_builtins", "panic_runtime", "global_allocator", "panic_handler", "eii"];
    for module in krate.modules(db) {
        let Some(file) = module.as_source_file_id(db) else { continue };
        let text = cx.file_text(file.file_id(db));
        let words = text.split(|c: char| !(c.is_alphanumeric() || c == '_'));
        if words.clone().any(|word| KEPT.contains(&word) || word.starts_with("eii_")) {
            return false;
        }
    }
    true
}

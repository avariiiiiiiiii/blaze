//! A name defined twice in a namespace of a module, block, enum or trait: E0428 for two items,
//! E0252 for two imports, E0255 for an import and an item, E0254, E0259 and E0260 when one is
//! an `extern crate`, each with the suggestion to rename or remove the import.
//!
//! Follows rustc's `compiler/rustc_resolve/src/build_reduced_graph.rs`
//! (`plant_decl_into_local_module`, `build_reduced_graph_for_use_tree`), `imports.rs`
//! (`try_plant_decl_into_local_module`), `diagnostics/impls.rs` (`report_conflict`,
//! `add_suggestion_for_rename_of_use`, `add_suggestion_for_duplicate_nested_use`,
//! `find_span_of_binding_until_next_binding`, `extend_span_to_previous_binding`) and
//! `diagnostics/mod.rs` (`NameDefinedMultipleTime`) (MIT or Apache-2.0).

use std::ops::ControlFlow;

use blaze_ids::radix;
use blaze_items::ItemId;
use cfg::CfgExpr;
use either::Either;
use hir::{HasSource as _, InFile, PathResolution};
use rustc_hash::FxHashMap;
use syntax::{
    AstNode, AstPtr, SyntaxNode, TextRange, TextSize,
    ast::{self, HasAttrs, HasName as _},
};

use super::{head_range, is_block, item_id};
use crate::{
    cx::{Cx, item_range},
    lower::{Found, Phase},
    model::{Applicability, Diag, Span, SuggestionStyle},
};

/// rustc's `Namespace`, in the order `per_ns` visits them.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Ns {
    Type,
    Value,
    Macro,
}

impl Ns {
    fn descr(self) -> &'static str {
        match self {
            Ns::Type => "type",
            Ns::Value => "value",
            Ns::Macro => "macro",
        }
    }
}

/// What put a name in a scope: rustc's `DeclKind` with the import's kind.
#[derive(Clone)]
enum Origin {
    Def,
    ExternCrate(ExternCrate),
    Import(Import),
}

#[derive(Clone)]
struct ExternCrate {
    /// The crate's name as written, before any `as`.
    source: String,
    /// The whole item with its attributes: rustc's `use_span_with_attributes`.
    with_attrs: TextRange,
    has_attrs: bool,
}

#[derive(Clone)]
struct Import {
    /// Where the last segment of its path ends: rustc's `source.span.hi()`.
    source_end: TextSize,
    /// Where the `use` item ends: rustc's `use_span.hi()`.
    use_end: TextSize,
    with_attrs: TextRange,
    has_attrs: bool,
    nested: bool,
}

/// A name put in a namespace of a scope: rustc's `Decl` as `report_conflict` reads it.
#[derive(Clone)]
struct Binding {
    name: String,
    ns: Ns,
    origin: Origin,
    /// What it resolves to, to tell two imports of one item.
    res: Option<hir::ModuleDef>,
    /// rustc's `old_kind` of it, said when it is the first of two.
    kind: &'static str,
    /// Its span: the item, the `extern crate` item, or the use tree.
    range: InFile<TextRange>,
    /// rustc's `guess_head_span` of its span.
    head: InFile<TextRange>,
    item: Option<ItemId>,
}

impl Binding {
    fn is_extern_crate(&self) -> bool {
        matches!(self.origin, Origin::ExternCrate(_))
    }

    /// rustc's `is_import_user_facing`, which an `extern crate` is.
    fn is_import(&self) -> bool {
        !matches!(self.origin, Origin::Def)
    }

    fn has_attrs(&self) -> bool {
        match &self.origin {
            Origin::Def => false,
            Origin::ExternCrate(it) => it.has_attrs,
            Origin::Import(it) => it.has_attrs,
        }
    }

    /// Whether it is written after `other` (rustc compares where spans start); bindings of
    /// different files keep the order they were planted in.
    fn is_written_after(&self, other: &Binding) -> bool {
        self.range.file_id == other.range.file_id
            && self.range.value.start() > other.range.value.start()
    }
}

/// The scope bindings are planted in, and how rustc's note names it.
struct Scope {
    module: hir::Module,
    container: &'static str,
    bindings: Vec<Binding>,
}

pub(super) fn check(cx: &Cx<'_, '_>, scopes: &[hir::Module]) -> Vec<Found> {
    let mut found = Vec::new();
    let mut seen: FxHashMap<String, InFile<TextRange>> = FxHashMap::default();
    let root = cx.krate.root_module(cx.db);
    let extern_prelude = ExternPrelude::of(cx, root);
    for &module in scopes {
        let scope = module_scope(cx, module);
        found.extend(plant(cx, &scope, &extern_prelude, &mut seen));
        for def in module.declarations(cx.db) {
            let inner = match def {
                hir::ModuleDef::Adt(hir::Adt::Enum(it)) => enum_scope(cx, module, it),
                hir::ModuleDef::Trait(it) => trait_scope(cx, module, it),
                _ => continue,
            };
            found.extend(plant(cx, &inner, &extern_prelude, &mut seen));
        }
    }
    found
}

/// The names of the crate's extern prelude, with whether an `extern crate` item put each there.
struct ExternPrelude {
    names: FxHashMap<String, bool>,
}

impl ExternPrelude {
    fn of(cx: &Cx<'_, '_>, root: hir::Module) -> ExternPrelude {
        let mut names: FxHashMap<String, bool> = cx
            .krate
            .dependencies(cx.db)
            .into_iter()
            .map(|dep| (dep.name.as_str().to_owned(), false))
            .collect();
        for decl in root.extern_crate_decls(cx.db) {
            if let Some(name) = decl.alias_or_name(cx.db) {
                names.insert(name.as_str().to_owned(), true);
            }
        }
        ExternPrelude { names }
    }

    /// rustc's `extern_prelude.get(ident).is_none_or(|it| it.introduced_by_item())`.
    fn from_item(&self, name: &str) -> bool {
        self.names.get(name).is_none_or(|&by_item| by_item)
    }
}

/// Each binding of `scope` in the order rustc plants them, each conflict reported as rustc
/// reports it.
fn plant(
    cx: &Cx<'_, '_>,
    scope: &Scope,
    extern_prelude: &ExternPrelude,
    seen: &mut FxHashMap<String, InFile<TextRange>>,
) -> Vec<Found> {
    let mut found = Vec::new();
    let mut planted: FxHashMap<(&str, Ns), usize> = FxHashMap::default();
    for (index, binding) in scope.bindings.iter().enumerate() {
        let Some(&old) = planted.get(&(binding.name.as_str(), binding.ns)) else {
            planted.insert((binding.name.as_str(), binding.ns), index);
            continue;
        };
        let (old, new) = match scope.bindings[old].is_written_after(binding) {
            true => (binding, &scope.bindings[old]),
            false => (&scope.bindings[old], binding),
        };
        if seen.get(&new.name) == Some(&new.head) {
            continue;
        }
        found.push(Found::new(conflict(cx, scope, old, new, extern_prelude), Phase::Resolve));
        seen.insert(new.name.clone(), new.head);
    }
    found
}

/// rustc's `report_conflict` of `new`, written after `old` in the same namespace.
fn conflict(
    cx: &Cx<'_, '_>,
    scope: &Scope,
    old: &Binding,
    new: &Binding,
    extern_prelude: &ExternPrelude,
) -> Diag {
    let name = &new.name;
    let code = match (old.is_extern_crate(), new.is_extern_crate()) {
        (true, true) => "E0259",
        (true, _) | (_, true) => match old.is_import() && new.is_import() {
            true => "E0254",
            false => "E0260",
        },
        _ => match (old.is_import(), new.is_import()) {
            (false, false) => "E0428",
            (true, true) => "E0252",
            _ => "E0255",
        },
    };
    let span = cx.span(new.head);
    let mut diag =
        Diag::error(Some(code), format!("the name `{name}` is defined multiple times"), span.clone());
    let label = match new.is_import() {
        true => format!("`{name}` reimported here"),
        false => format!("`{name}` redefined here"),
    };
    diag.span_label(span, label);
    if old.head != new.head {
        let what = match old.is_import() {
            true => "import",
            false => "definition",
        };
        diag.span_label(cx.span(old.head), format!("previous {what} of the {} `{name}` here", old.kind));
    }
    diag.note(format!(
        "`{name}` must be defined only once in the {} namespace of this {}",
        new.ns.descr(),
        scope.container
    ));

    let import = match (&new.origin, &old.origin) {
        (Origin::Def, Origin::Def) => None,
        _ if new.is_import() && old.is_import() && (new.has_attrs() || old.has_attrs()) => {
            match old.has_attrs() {
                true => Some((new, true)),
                false => Some((old, true)),
            }
        }
        _ if new.is_import() => Some((new, old.is_import())),
        _ => Some((old, new.is_import())),
    };
    let duplicate = new.res.is_some() && new.res == old.res;
    let should_remove = duplicate
        && (new.is_extern_crate() || old.is_extern_crate() || extern_prelude.from_item(name));
    match import {
        Some((binding, true)) if should_remove => remove(cx, &mut diag, binding),
        Some((binding, _)) => rename(cx, &mut diag, binding),
        None => {}
    }

    diag.fact(cx.resolves_fact(scope.module, name), format!("what `{name}` names in its scope"));
    for binding in [old, new] {
        if let Some(item) = binding.item {
            diag.fact(blaze_items::Fact::Name(cx.key(item)), format!("the definition of `{name}`"));
        }
    }
    diag
}

/// rustc's `add_suggestion_for_duplicate_nested_use` and `ToolOnlyRemoveUnnecessaryImport`:
/// removing the import, which names what the other binding names.
fn remove(cx: &Cx<'_, '_>, diag: &mut Diag, binding: &Binding) {
    let file = binding.range.file_id;
    let (with_attrs, nested) = match &binding.origin {
        Origin::Import(it) => (it.with_attrs, it.nested.then_some(it.use_end)),
        Origin::ExternCrate(it) => (it.with_attrs, None),
        Origin::Def => return,
    };
    let whole = cx.span(InFile::new(file, with_attrs));
    let Some(use_end) = nested else {
        hidden_removal(diag, whole);
        return;
    };
    let text = cx.sema.parse_or_expand(file).text().to_string();
    let range = binding.range.value;
    let (closing_brace, until_next) = until_next_binding(&text, range, use_end);
    if !closing_brace {
        diag.suggestion(cx.span(InFile::new(file, until_next)), "remove unnecessary import", "", Applicability::MaybeIncorrect);
        return;
    }
    match to_previous_binding(&text, until_next) {
        Some(it) => hidden_removal(diag, cx.span(InFile::new(file, it))),
        None => {
            diag.suggestion(whole, "remove unnecessary import", "", Applicability::MaybeIncorrect);
        }
    }
}

fn hidden_removal(diag: &mut Diag, span: Span) {
    diag.suggestion_with_style(
        vec![(span, String::new())],
        "remove unnecessary import",
        Applicability::MaybeIncorrect,
        SuggestionStyle::CompletelyHidden,
    );
}

/// rustc's `find_span_of_binding_until_next_binding`: the binding with the commas and spaces
/// after it, and whether a `}` came next.
fn until_next_binding(text: &str, binding: TextRange, use_end: TextSize) -> (bool, TextRange) {
    let after = text.get(usize::from(binding.end())..usize::from(use_end)).unwrap_or("");
    let mut closing_brace = false;
    let taken: usize = after
        .chars()
        .take_while(|&ch| {
            if ch == '}' {
                closing_brace = true;
            }
            ch == ' ' || ch == ','
        })
        .map(char::len_utf8)
        .sum();
    let end = binding.end() + TextSize::from(taken as u32);
    (closing_brace, TextRange::new(binding.start(), end))
}

/// rustc's `extend_span_to_previous_binding`: back to the comma before it, unless the list's
/// `{` is nearer (it is the only binding).
fn to_previous_binding(text: &str, range: TextRange) -> Option<TextRange> {
    let before = text.get(..usize::from(range.start()))?;
    let comma = before.rfind(',')?;
    let brace = before.rfind('{')?;
    let after_comma = before.len() - comma - 1;
    let after_brace = before.len() - brace - 1;
    if after_comma > after_brace {
        return None;
    }
    let start = range.start() - TextSize::from(after_comma as u32 + 1);
    Some(TextRange::new(start, range.end()))
}

/// rustc's `add_suggestion_for_rename_of_use`: `as Other{name}` or `as other_{name}`.
fn rename(cx: &Cx<'_, '_>, diag: &mut Diag, binding: &Binding) {
    let name = &binding.name;
    let suggested = match name.chars().next().is_some_and(char::is_uppercase) {
        true => format!("Other{name}"),
        false => format!("other_{name}"),
    };
    let message = "you can use `as` to change the binding name of the import";
    let file = binding.range.file_id;
    let range = binding.range.value;
    let (at, code) = match &binding.origin {
        Origin::Import(import) => {
            let snippet = cx.sema.parse_or_expand(file).text().to_string();
            let snippet = snippet.get(usize::from(range.start())..usize::from(range.end())).unwrap_or("");
            let pos = import.source_end.checked_sub(range.start());
            match pos {
                Some(pos) if usize::from(pos) <= snippet.len() => {
                    let end = range.end() - TextSize::from(u32::from(snippet.ends_with(';')));
                    (TextRange::new(range.start() + pos, end), format!(" as {suggested}"))
                }
                _ => {
                    diag.span_label(cx.span(binding.range), message);
                    return;
                }
            }
        }
        Origin::ExternCrate(it) => (range, format!("extern crate {} as {suggested};", it.source)),
        Origin::Def => return,
    };
    diag.suggestion(cx.span(InFile::new(file, at)), message, code, Applicability::MaybeIncorrect);
}

/// The bindings of a module or block, in the order rustc plants them: items and `extern crate`s
/// as written (those a macro wrote after), then single imports.
fn module_scope(cx: &Cx<'_, '_>, module: hir::Module) -> Scope {
    let db = cx.db;
    let mut defs: Vec<Binding> = Vec::new();
    for def in module.declarations(db) {
        defs.extend(item_bindings(cx, def));
    }
    for decl in module.extern_crate_decls(db) {
        defs.extend(extern_crate_binding(cx, decl));
    }
    let home = module.definition_source_file_id(db);
    let order = |it: &Binding| match it.range.file_id == home {
        true => (0, it.range.value.start()),
        false => (1, TextSize::from(0)),
    };
    radix::sort_by_key(&mut defs, |it| {
        let (tier, at) = order(it);
        radix::pack_32(tier, u32::from(at))
    });
    for decl in module.use_decls(db) {
        let source = decl.source(db);
        defs.extend(import_bindings(cx, cx.node(source.as_ref().map(AstPtr::new))));
    }
    let container = if is_block(cx, module) { "block" } else { "module" };
    Scope { module, container, bindings: defs }
}

/// The variants of an enum, each in both namespaces, read from its syntax (the front end keeps
/// one variant per name).
fn enum_scope(cx: &Cx<'_, '_>, module: hir::Module, enum_: hir::Enum) -> Scope {
    let mut bindings = Vec::new();
    if let Some(source) = enum_.source(cx.db)
        && let Some(list) = source.value.variant_list()
    {
        let item = Some(ItemId::Adt(hir::Adt::from(enum_).into()));
        for variant in list.variants().filter(|it| enabled(cx, it)) {
            let Some(name) = variant.name() else { continue };
            let node = variant.syntax();
            for (ns, kind) in [(Ns::Type, "type"), (Ns::Value, "value")] {
                bindings.push(Binding {
                    name: name.text().to_string(),
                    ns,
                    origin: Origin::Def,
                    res: None,
                    kind,
                    range: source.with_value(item_range(node)),
                    head: source.with_value(head_range(node)),
                    item,
                });
            }
        }
    }
    Scope { module, container: "enum", bindings }
}

/// The associated items of a trait, functions and consts as values, types as types.
fn trait_scope(cx: &Cx<'_, '_>, module: hir::Module, trait_: hir::Trait) -> Scope {
    let db = cx.db;
    let mut bindings = Vec::new();
    for assoc in trait_.items(db) {
        let (name, ns, node) = match assoc {
            hir::AssocItem::Function(it) => {
                (it.name(db), Ns::Value, it.source(db).map(|it| it.map(|it| it.syntax().clone())))
            }
            hir::AssocItem::Const(it) => {
                let Some(name) = it.name(db) else { continue };
                (name, Ns::Value, it.source(db).map(|it| it.map(|it| it.syntax().clone())))
            }
            hir::AssocItem::TypeAlias(it) => {
                (it.name(db), Ns::Type, it.source(db).map(|it| it.map(|it| it.syntax().clone())))
            }
        };
        let Some(node) = node else { continue };
        if name.as_str() == "_" {
            continue;
        }
        bindings.push(def_binding(
            name.as_str().to_owned(),
            ns,
            ns.descr(),
            None,
            &node,
            Some(ItemId::Trait(trait_.into())),
        ));
    }
    Scope { module, container: "trait", bindings }
}

fn def_binding(
    name: String,
    ns: Ns,
    kind: &'static str,
    res: Option<hir::ModuleDef>,
    node: &InFile<SyntaxNode>,
    item: Option<ItemId>,
) -> Binding {
    Binding {
        name,
        ns,
        origin: Origin::Def,
        res,
        kind,
        range: node.with_value(item_range(&node.value)),
        head: node.with_value(head_range(&node.value)),
        item,
    }
}

/// The names an item puts in its module: types, modules and traits in the type namespace,
/// functions, consts, statics and the constructors of tuple and unit structs in the value
/// namespace, macros in the macro namespace.
fn item_bindings(cx: &Cx<'_, '_>, def: hir::ModuleDef) -> Vec<Binding> {
    let db = cx.db;
    let Some(name) = def.name(db) else { return Vec::new() };
    let name = name.as_str().to_owned();
    if name == "_" {
        return Vec::new();
    }
    let Some(node) = def_node(cx, def) else { return Vec::new() };
    let namespaces: &[Ns] = match def {
        hir::ModuleDef::Module(_) | hir::ModuleDef::Trait(_) | hir::ModuleDef::TypeAlias(_) => {
            &[Ns::Type]
        }
        hir::ModuleDef::Adt(hir::Adt::Struct(it)) => match it.kind(db) {
            hir::StructKind::Record => &[Ns::Type],
            hir::StructKind::Tuple | hir::StructKind::Unit => &[Ns::Type, Ns::Value],
        },
        hir::ModuleDef::Adt(_) => &[Ns::Type],
        hir::ModuleDef::Function(_) | hir::ModuleDef::Const(_) | hir::ModuleDef::Static(_) => {
            &[Ns::Value]
        }
        hir::ModuleDef::Macro(_) => &[Ns::Macro],
        hir::ModuleDef::EnumVariant(_) | hir::ModuleDef::BuiltinType(_) => return Vec::new(),
    };
    let item = item_id(db, def);
    namespaces
        .iter()
        .map(|&ns| def_binding(name.clone(), ns, kind_of(ns, def), Some(def), &node, item))
        .collect()
}

/// rustc's `old_kind`: what a binding is called when it is the first of two.
fn kind_of(ns: Ns, def: hir::ModuleDef) -> &'static str {
    match (ns, def) {
        (Ns::Value, _) => "value",
        (Ns::Macro, _) => "macro",
        (Ns::Type, hir::ModuleDef::Module(_)) => "module",
        (Ns::Type, hir::ModuleDef::Trait(_)) => "trait",
        (Ns::Type, _) => "type",
    }
}

/// The syntax of a definition: a module's `mod` item, else the item.
fn def_node(cx: &Cx<'_, '_>, def: hir::ModuleDef) -> Option<InFile<SyntaxNode>> {
    let db = cx.db;
    Some(match def {
        hir::ModuleDef::Module(it) => it.declaration_source(db)?.map(|it| it.syntax().clone()),
        hir::ModuleDef::Function(it) => it.source(db)?.map(|it| it.syntax().clone()),
        hir::ModuleDef::Adt(it) => it.source(db)?.map(|it| it.syntax().clone()),
        hir::ModuleDef::EnumVariant(it) => it.source(db)?.map(|it| it.syntax().clone()),
        hir::ModuleDef::Const(it) => it.source(db)?.map(|it| it.syntax().clone()),
        hir::ModuleDef::Static(it) => it.source(db)?.map(|it| it.syntax().clone()),
        hir::ModuleDef::Trait(it) => it.source(db)?.map(|it| it.syntax().clone()),
        hir::ModuleDef::TypeAlias(it) => it.source(db)?.map(|it| it.syntax().clone()),
        hir::ModuleDef::Macro(it) => it.source(db)?.map(|it| match it {
            Either::Left(it) => it.syntax().clone(),
            Either::Right(it) => it.syntax().clone(),
        }),
        hir::ModuleDef::BuiltinType(_) => return None,
    })
}

/// An `extern crate`'s name in the type namespace.
fn extern_crate_binding(cx: &Cx<'_, '_>, decl: hir::ExternCrateDecl) -> Option<Binding> {
    let db = cx.db;
    let name = decl.alias_or_name(db)?.as_str().to_owned();
    if name == "_" {
        return None;
    }
    let source = decl.source(db)?;
    let node = source.value.syntax();
    let res = decl.resolved_crate(db).map(|it| hir::ModuleDef::Module(it.root_module(db)));
    Some(Binding {
        name,
        ns: Ns::Type,
        origin: Origin::ExternCrate(ExternCrate {
            source: decl.name(db).as_str().to_owned(),
            with_attrs: node.text_range(),
            has_attrs: source.value.attrs().next().is_some(),
        }),
        res,
        kind: "extern crate",
        range: source.with_value(item_range(node)),
        head: source.with_value(item_range(node)),
        item: None,
    })
}

/// The single imports of a `use` item, each in every namespace its path resolves in (globs
/// and `_` bind no name that can conflict).
fn import_bindings(cx: &Cx<'_, '_>, item: InFile<ast::Use>) -> Vec<Binding> {
    let mut bindings = Vec::new();
    let Some(tree) = item.value.use_tree() else { return bindings };
    let site = Import {
        source_end: TextSize::from(0),
        use_end: item.value.syntax().text_range().end(),
        with_attrs: item.value.syntax().text_range(),
        has_attrs: item.value.attrs().next().is_some(),
        nested: false,
    };
    let mut stack = vec![(tree, false, None::<ast::Path>)];
    while let Some((tree, nested, parent)) = stack.pop() {
        if let Some(list) = tree.use_tree_list() {
            let prefix = tree.path().or(parent);
            let trees: Vec<ast::UseTree> = list.use_trees().collect();
            stack.extend(trees.into_iter().rev().map(|it| (it, true, prefix.clone())));
            continue;
        }
        if tree.star_token().is_some() {
            continue;
        }
        let Some(path) = tree.path() else { continue };
        let Some(segment) = path.segment() else { continue };
        let is_self = segment.self_token().is_some() && path.qualifier().is_none();
        let source = match is_self {
            true => parent.as_ref().and_then(|it| it.segment()),
            false => Some(segment.clone()),
        };
        let Some(source) = source else { continue };
        let source_name = source.name_ref();
        let name = match tree.rename() {
            Some(rename) if rename.underscore_token().is_some() => continue,
            Some(rename) => match rename.name() {
                Some(it) => it.text().to_string(),
                None => continue,
            },
            None => match &source_name {
                Some(it) => it.text().to_string(),
                None => continue,
            },
        };
        let Some(resolved) = cx.sema.resolve_path_per_ns(&path) else { continue };
        let range = item.with_value(tree.syntax().text_range());
        let site = Import { source_end: source.syntax().text_range().end(), nested, ..site.clone() };
        let namespaces = [
            (Ns::Type, resolved.type_ns),
            (Ns::Value, if is_self { None } else { resolved.value_ns }),
            (Ns::Macro, if is_self { None } else { resolved.macro_ns }),
        ];
        for (ns, res) in namespaces {
            let Some(PathResolution::Def(def)) = res else { continue };
            bindings.push(Binding {
                name: name.clone(),
                ns,
                origin: Origin::Import(site.clone()),
                res: Some(def),
                kind: kind_of(ns, def),
                range,
                head: range,
                item: item_id(cx.db, def),
            });
        }
    }
    bindings
}

/// Whether the `cfg`s of `owner` keep it, its `cfg_attr`s expanded.
fn enabled<N: HasAttrs>(cx: &Cx<'_, '_>, owner: &N) -> bool {
    if owner.attrs().next().is_none() {
        return true;
    }
    let cfg = cx.krate.cfg(cx.db);
    let off = hir_expand::attrs::expand_cfg_attr(
        owner.attrs(),
        || cfg,
        |meta, _| {
            if let ast::Meta::CfgMeta(meta) = meta
                && let Some(predicate) = meta.cfg_predicate()
                && cfg.check(&CfgExpr::parse_from_ast(predicate)) == Some(false)
            {
                return ControlFlow::Break(());
            }
            ControlFlow::Continue(())
        },
    );
    off.is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(text: &str, needle: &str) -> TextRange {
        let at = text.find(needle).unwrap();
        TextRange::at(TextSize::from(at as u32), TextSize::of(needle))
    }

    #[test]
    fn a_nested_duplicate_is_removed_with_the_comma_after_it() {
        let text = "use issue_52891::{d, a, e};";
        let (brace, until) = until_next_binding(text, range(text, "a"), TextSize::of(text));
        assert!(!brace);
        assert_eq!(&text[until], "a, ");
    }

    #[test]
    fn the_last_nested_duplicate_is_removed_with_the_comma_before_it() {
        let text = "use issue_52891::{d, e, a};";
        let (brace, until) = until_next_binding(text, range(text, "a}"), TextSize::of(text));
        assert!(brace);
        assert_eq!(&text[until], "a");
        assert_eq!(&text[to_previous_binding(text, until).unwrap()], ", a");
        let text = "use issue_52891::{self};";
        let self_ = range(text, "self");
        assert_eq!(to_previous_binding(text, self_), None);
    }
}

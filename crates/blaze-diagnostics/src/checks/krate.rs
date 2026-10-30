//! The checks of the crate as a whole: a binary's entry point (rustc's `entry_fn`): no `main`
//! (E0601), two `#[rustc_main]` functions (E0137), a `main` in an `extern` block.
//!
//! Follows rustc's `compiler/rustc_passes/src/entry.rs` and its diagnostics (`NoMainErr`,
//! `ExternMain`, `MultipleRustcMain` in `compiler/rustc_passes/src/diagnostics.rs`),
//! `resolve_main` in `compiler/rustc_resolve/src/lib.rs`, `entry_point_type` in
//! `compiler/rustc_ast/src/entry.rs`, `def_span` in `compiler/rustc_middle/src/hir/map.rs`,
//! and where the crate's span ends: `new_parser_from_source_file` in
//! `compiler/rustc_parse/src/lib.rs`, `parse_mod` in `compiler/rustc_parse/src/parser/item.rs`
//! and the end-of-file token of `Parser::bump` in `compiler/rustc_parse/src/parser/mod.rs`
//! (MIT or Apache-2.0).

use std::ops::ControlFlow;

use base_db::{FileId, TargetKind};
use hir::{HasSource, InFile, ModuleDef, Name};
use hir_def::{Lookup, ModuleDefId, expr_store::Body, item_scope::ImportOrGlob};
use rustc_hash::{FxHashMap, FxHashSet};
use syntax::{
    AstNode, AstToken, SyntaxKind, SyntaxNode, SyntaxNodePtr, SyntaxToken, TextRange, TextSize,
    ast::{self, HasName},
};

use crate::{
    checks::{PASSES, has_crate_attr, modules},
    cx::{Cx, def_range, item_range},
    lower::Found,
    model::{Code, Diag, Level},
};

pub(crate) fn check(cx: &Cx<'_, '_>) -> Vec<Found> {
    entry(cx).into_iter().map(|diag| Found::new(diag, PASSES)).collect()
}

/// rustc's `CrateType::Executable`: a binary, an example, a build script. A test or bench
/// target is built with the test harness, whose `main` is `#[rustc_main]`.
fn is_executable(kind: TargetKind) -> bool {
    matches!(kind, TargetKind::Bin | TargetKind::Example | TargetKind::BuildScript)
}

/// rustc's `entry_fn`: the errors of looking for the crate's `main`.
fn entry(cx: &Cx<'_, '_>) -> Vec<Diag> {
    if !is_executable(cx.krate.target_kind(cx.db)) || has_crate_attr(cx, "no_main") {
        return Vec::new();
    }
    let mut diags = Vec::new();
    let items = free_items(cx);
    let mut rustc_main: Option<&FreeItem> = None;
    let mut non_main_fns = Vec::new();
    for item in &items {
        if item.rustc_main {
            match rustc_main {
                None => rustc_main = Some(item),
                Some(first) => {
                    let span = cx.span(item.def_range());
                    let mut diag = Diag::error(
                        Some("E0137"),
                        "multiple functions with a `#[rustc_main]` attribute",
                        span.clone(),
                    );
                    diag.span_label(cx.span(first.def_range()), "first `#[rustc_main]` function");
                    diag.span_label(span, "additional `#[rustc_main]` function");
                    diags.push(diag);
                }
            }
        } else if item.name.as_deref() == Some("main") && !item.at_root {
            non_main_fns.push(item);
        }
    }
    if rustc_main.is_some() {
        return diags;
    }
    let root = cx.krate.root_module(cx.db);
    let main = main_def(cx, root);
    if let Some(MainDef::Function(function)) = &main {
        let local = function.module(cx.db).krate(cx.db) == cx.krate;
        if local
            && function.extern_block(cx.db).is_some()
            && let Some(node) = function.source(cx.db)
        {
            let span = cx.span(node.as_ref().map(|it| def_range(it.syntax())));
            let mut diag = Diag::error(None, "the `main` function cannot be declared in an `extern` block", span);
            diag.fact(cx.resolves_fact(root, "main"), "what `main` is in the crate root");
            diags.push(diag);
        }
        return diags;
    }
    diags.push(no_main(cx, root, &non_main_fns, main));
    diags
}

/// What `crate::main` names in the value namespace (rustc's `MainDefinition`).
enum MainDef {
    Function(hir::Function),
    /// Something else, with rustc's span of its binding: the item, or the `use` that brings it.
    Other(InFile<TextRange>),
}

/// rustc's `resolve_main`: `main` in the crate root's value namespace, imports and globs
/// included.
fn main_def(cx: &Cx<'_, '_>, root: hir::Module) -> Option<MainDef> {
    let root_id = hir_def::ModuleId::from(root);
    let scope = root_id.def_map(cx.db).module(cx.db, root_id).scope(cx.db);
    let item = scope.get(&Name::new_root("main")).values?;
    if let ModuleDefId::FunctionId(_) = item.def
        && let ModuleDef::Function(function) = ModuleDef::from(item.def)
    {
        return Some(MainDef::Function(function));
    }
    let use_tree = |use_: hir_def::UseId, idx| {
        let ast_id = use_.lookup(cx.db).id;
        let tree = hir_def::src::use_tree_to_ast(cx.db, ast_id, idx);
        InFile::new(ast_id.file_id, tree.syntax().text_range())
    };
    let binding = match item.import {
        Some(ImportOrGlob::Import(import)) => Some(use_tree(import.use_, import.idx)),
        Some(ImportOrGlob::Glob(glob)) => Some(use_tree(glob.use_, glob.idx)),
        None => source(cx, ModuleDef::from(item.def)).map(|node| node.as_ref().map(item_range)),
    };
    binding.map(MainDef::Other)
}

/// rustc's `no_main_err`.
fn no_main(cx: &Cx<'_, '_>, root: hir::Module, non_main_fns: &[&FreeItem], main: Option<MainDef>) -> Diag {
    let message = format!("`main` function not found in crate `{}`", crate_name(cx, root));
    let file = root.as_source_file_id(cx.db).map(|it| it.file_id(cx.db));
    let filename = file.map(|it| cx.file_name(it));
    let at = file.and_then(|file| Some(cx.span_in_file(file, TextRange::empty(crate_end(cx, file)?))));
    let mut diag = match &at {
        Some(at) => Diag::error(Some("E0601"), message, at.clone()),
        None => Diag::new(Level::Error, Some(Code::Error("E0601".to_owned())), message),
    };
    let note = if non_main_fns.is_empty() {
        match &filename {
            Some(filename) => format!("consider adding a `main` function to `{filename}`"),
            None => "consider adding a `main` function at the crate level".to_owned(),
        }
    } else {
        for item in non_main_fns {
            diag.span_note(cx.span(item.def_range()), "here is a function named `main`");
        }
        diag.note("you have one or more functions named `main` not defined at the crate level");
        diag.help("consider moving the `main` function definitions");
        match &filename {
            Some(filename) => format!("the main function must be defined at the crate level (in `{filename}`)"),
            None => "the main function must be defined at the crate level".to_owned(),
        }
    };
    match at {
        Some(at) => {
            diag.span_label(at, note);
        }
        None => {
            diag.note(note);
        }
    }
    if let Some(MainDef::Other(binding)) = main {
        diag.span_label(cx.span(binding), "non-function item at `crate::main` is found");
    }
    diag.fact(cx.resolves_fact(root, "main"), "what `main` is in the crate root");
    for item in non_main_fns {
        diag.fact(cx.resolves_fact(item.module, "main"), "the items named `main` below the crate root");
    }
    diag
}

/// The crate's name: `--crate-name`, else rustc's from the root file's name.
fn crate_name(cx: &Cx<'_, '_>, root: hir::Module) -> String {
    if let Some(name) = cx.krate.display_name(cx.db) {
        return name.crate_name().to_string();
    }
    let file = root.as_source_file_id(cx.db).map(|it| cx.file_name(it.file_id(cx.db))).unwrap_or_default();
    let stem = file.rsplit('/').next().unwrap_or(&file);
    stem.strip_suffix(".rs").unwrap_or(stem).replace('-', "_")
}

/// Where the crate root's span ends (rustc's `inner_span.hi()`): after its last token, or at
/// the end of a file with none; `None` for an empty file, which rustc cannot point into.
fn crate_end(cx: &Cx<'_, '_>, file: FileId) -> Option<TextSize> {
    let text = cx.file_text(file);
    if text.is_empty() {
        return None;
    }
    let root = cx.krate.root_module(cx.db).as_source_file_id(cx.db)?;
    let tree = root.parse(cx.db).tree();
    let mut token = tree.syntax().last_token();
    while let Some(it) = token {
        if !is_rustc_trivia(&it) {
            return Some(it.text_range().end());
        }
        token = it.prev_token();
    }
    Some(TextSize::of(text))
}

/// What rustc's lexer gives no token for: whitespace, comments that are not doc comments,
/// the shebang and the frontmatter.
fn is_rustc_trivia(token: &SyntaxToken) -> bool {
    match token.kind() {
        SyntaxKind::WHITESPACE | SyntaxKind::SHEBANG | SyntaxKind::FRONTMATTER => true,
        SyntaxKind::COMMENT => ast::Comment::cast(token.clone()).is_none_or(|it| it.kind().doc.is_none()),
        _ => false,
    }
}

/// An item rustc's `hir_free_items` gives: a module's, a block's, not an associated or a
/// foreign item.
struct FreeItem {
    name: Option<String>,
    /// Directly in the crate root.
    at_root: bool,
    /// The module it is declared in (a block's for an item in a body).
    module: hir::Module,
    node: InFile<SyntaxNode>,
    rustc_main: bool,
    /// Where it is in the crate's source: rustc's order of items.
    place: Vec<u32>,
}

impl FreeItem {
    /// rustc's `def_span`.
    fn def_range(&self) -> InFile<TextRange> {
        self.node.as_ref().map(free_item_def_range)
    }
}

/// rustc's `def_span` of a free item: a const's or a static's ends with its type, a macro's
/// with its name, the rest as [`def_range`].
fn free_item_def_range(node: &SyntaxNode) -> TextRange {
    let start = item_range(node).start();
    let end = if let Some(it) = ast::Const::cast(node.clone()) {
        it.ty().map(|ty| ty.syntax().text_range().end())
    } else if let Some(it) = ast::Static::cast(node.clone()) {
        it.ty().map(|ty| ty.syntax().text_range().end())
    } else if let Some(it) = ast::MacroRules::cast(node.clone()) {
        it.name().map(|name| name.syntax().text_range().end())
    } else if let Some(it) = ast::MacroDef::cast(node.clone()) {
        it.name().map(|name| name.syntax().text_range().end())
    } else {
        None
    };
    match end {
        Some(end) if end >= start => TextRange::new(start, end),
        _ => def_range(node),
    }
}

/// The crate's free items, in rustc's order.
fn free_items(cx: &Cx<'_, '_>) -> Vec<FreeItem> {
    let root = cx.krate.root_module(cx.db);
    let mut walk = Walk { cx, places: Places::new(cx), root, seen: FxHashSet::default(), items: Vec::new() };
    walk.module(root);
    let mut items = walk.items;
    items.sort_by(|a, b| a.place.cmp(&b.place));
    items
}

struct Walk<'a, 'b, 'db> {
    cx: &'a Cx<'b, 'db>,
    places: Places,
    root: hir::Module,
    seen: FxHashSet<InFile<SyntaxNodePtr>>,
    items: Vec<FreeItem>,
}

impl Walk<'_, '_, '_> {
    fn module(&mut self, module: hir::Module) {
        let db = self.cx.db;
        for def in module.declarations(db) {
            let foreign = match def {
                ModuleDef::Function(it) => it.extern_block(db).is_some(),
                ModuleDef::Static(it) => it.extern_block(db).is_some(),
                _ => false,
            };
            if foreign {
                continue;
            }
            if let Some(node) = source(self.cx, def) {
                self.push(module, def.name(db), node);
            }
            for body in bodies(self.cx, def) {
                self.blocks(body);
            }
        }
        for macro_ in module.legacy_macros(db) {
            if macro_.module(db) != module {
                continue;
            }
            if let Some(node) = macro_.source(db) {
                let node = node.map(|it| it.either(|it| it.syntax().clone(), |it| it.syntax().clone()));
                self.push(module, Some(macro_.name(db)), node);
            }
        }
        let module_id = hir_def::ModuleId::from(module);
        let scope = module_id.def_map(db).module(db, module_id).scope(db);
        for id in scope.extern_crate_decls() {
            let decl = hir::ExternCrateDecl::from(id);
            if let Some(node) = decl.source(db) {
                self.push(module, decl.alias_or_name(db), node.map(|it| it.syntax().clone()));
            }
        }
        for impl_ in module.impl_defs(db) {
            for item in impl_.items(db) {
                if let Some(body) = assoc_body(item) {
                    self.blocks(body);
                }
            }
        }
        for child in module.children(db) {
            self.module(child);
        }
    }

    fn push(&mut self, module: hir::Module, name: Option<Name>, node: InFile<SyntaxNode>) {
        if !self.seen.insert(node.as_ref().map(SyntaxNodePtr::new)) {
            return;
        }
        self.items.push(FreeItem {
            name: name.map(|it| it.as_str().to_owned()),
            at_root: module == self.root,
            module,
            rustc_main: has_attr(self.cx, &node, "rustc_main"),
            place: self.places.of(self.cx, &node),
            node,
        });
    }

    /// The items of the blocks of `body`.
    fn blocks(&mut self, body: hir::DefWithBody) {
        let Ok(id) = hir_def::DefWithBodyId::try_from(body) else { return };
        let blocks: Vec<hir::Module> = Body::of(self.cx.db, id)
            .blocks(self.cx.db)
            .map(|(_, def_map)| hir::Module::from(def_map.root_module_id()))
            .collect();
        for block in blocks {
            self.module(block);
        }
    }
}

/// The bodies of `def`: its own, its variants' discriminants, a trait's default bodies.
fn bodies(cx: &Cx<'_, '_>, def: ModuleDef) -> Vec<hir::DefWithBody> {
    match def {
        ModuleDef::Adt(hir::Adt::Enum(it)) => it.variants(cx.db).into_iter().map(hir::DefWithBody::from).collect(),
        ModuleDef::Trait(it) => it.items(cx.db).into_iter().filter_map(assoc_body).collect(),
        _ => def.as_def_with_body().into_iter().collect(),
    }
}

fn assoc_body(item: hir::AssocItem) -> Option<hir::DefWithBody> {
    match item {
        hir::AssocItem::Function(it) => Some(it.into()),
        hir::AssocItem::Const(it) => Some(it.into()),
        hir::AssocItem::TypeAlias(_) => None,
    }
}

/// The node of a module item as written: the item, a module's declaration.
fn source(cx: &Cx<'_, '_>, def: ModuleDef) -> Option<InFile<SyntaxNode>> {
    fn syntax<N: AstNode>(it: InFile<N>) -> InFile<SyntaxNode> {
        it.map(|it| it.syntax().clone())
    }
    match def {
        ModuleDef::Module(it) => it.declaration_source(cx.db).map(syntax),
        ModuleDef::Function(it) => it.source(cx.db).map(syntax),
        ModuleDef::Adt(it) => it.source(cx.db).map(syntax),
        ModuleDef::Const(it) => it.source(cx.db).map(syntax),
        ModuleDef::Static(it) => it.source(cx.db).map(syntax),
        ModuleDef::Trait(it) => it.source(cx.db).map(syntax),
        ModuleDef::TypeAlias(it) => it.source(cx.db).map(syntax),
        ModuleDef::Macro(it) => {
            it.source(cx.db).map(|it| it.map(|it| it.either(|it| it.syntax().clone(), |it| it.syntax().clone())))
        }
        ModuleDef::EnumVariant(_) | ModuleDef::BuiltinType(_) => None,
    }
}

/// Whether `node` has the attribute `#[name]`, `cfg_attr` expanded.
fn has_attr(cx: &Cx<'_, '_>, node: &InFile<SyntaxNode>, name: &str) -> bool {
    let Some(owner) = ast::AnyHasAttrs::cast(node.value.clone()) else { return false };
    let attrs = ast::attrs_including_inner(&owner);
    hir_expand::attrs::expand_cfg_attr(attrs, || cx.krate.cfg(cx.db), |meta, _| {
        let named = meta.path().and_then(|it| it.as_single_name_ref()).is_some_and(|it| it.text() == name);
        if named { ControlFlow::Break(()) } else { ControlFlow::Continue(()) }
    })
    .is_some()
}

/// Where each file of the crate is in its source as rustc reads it: a module's file where
/// its `mod` item is.
struct Places {
    files: FxHashMap<FileId, Vec<u32>>,
}

impl Places {
    fn new(cx: &Cx<'_, '_>) -> Places {
        let mut places = Places { files: FxHashMap::default() };
        for module in modules(cx) {
            let Some(file) = module.as_source_file_id(cx.db) else { continue };
            let file = file.file_id(cx.db);
            let place = match module.declaration_source(cx.db) {
                Some(declaration) => places.of(cx, &declaration.map(|it| it.syntax().clone())),
                None => Vec::new(),
            };
            places.files.insert(file, place);
        }
        places
    }

    /// Where `node` is: its file's place, then where in the file it is written (for a macro's
    /// output, where the macro is called).
    fn of(&self, cx: &Cx<'_, '_>, node: &InFile<SyntaxNode>) -> Vec<u32> {
        let range = node.as_ref().original_file_range_rooted(cx.db).into_file_id(cx.db);
        let mut place = self.files.get(&range.file_id).cloned().unwrap_or_default();
        place.push(range.range.start().into());
        place
    }
}

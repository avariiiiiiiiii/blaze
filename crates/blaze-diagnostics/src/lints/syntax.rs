//! What rustc's early lint pass, its expansion and its attribute checks lint on the source as
//! written: the crate walked as rustc's early lint pass walks its expanded AST ([`Walk`]),
//! each lint emitted in the order rustc emits it ([`Pass`]).
//!
//! - `check_cfg`: `--check-cfg` ([`CheckCfg`]), `unexpected_cfgs`,
//!   `explicit_builtin_cfgs_in_flags`.
//! - `delims`: `unused_parens`, `unused_braces`.
//! - `attrs`: `unused_attributes`, `ill_formed_attribute_input`, `conflicting_repr_hints`,
//!   `unused_doc_comments`, the feature lints (`incomplete_features`, `internal_features`,
//!   `unstable_features`).
//! - `early`: the rest of the early lint pass (`while_true`, `non_camel_case_types`,
//!   `keyword_idents`, `redundant_semicolons`, ..) and of the lints the lexer, the parser,
//!   expansion and resolution buffer (`text_direction_codepoint_in_comment`,
//!   `semicolon_in_expressions_from_macros`, `unused_labels`, ..).
//!
//! Follows rustc's `compiler/rustc_interface/src/passes.rs` (`pre_expansion_lint` before
//! expansion, `early_lint_checks` after it), `compiler/rustc_lint/src/early.rs`
//! (`EarlyContextAndPass`: `with_lint_attrs` and `check_id`, which emits the lints buffered
//! for a node before the pass's own there; the pre-order walk of `rustc_ast::visit`) and
//! `compiler/rustc_expand/src/config.rs` (`StripUnconfigured`: what `cfg` removes before any
//! lint sees it) (MIT or Apache-2.0).

/// `pub(crate)` for its table of rustc's language features, which `crate::checks` reads too.
pub(crate) mod attrs;
mod check_cfg;
mod delims;
mod early;

pub use check_cfg::CheckCfg;

use std::{collections::hash_map::Entry, ops::ControlFlow};

use blaze_ids::radix;
use blaze_items::{Fact, ItemId};
use cfg::{CfgExpr, CfgOptions};
use hir::{HirFileId, InFile, MacroCallId};
use hir_def::AdtId;
use rustc_hash::FxHashMap;
use syntax::{
    AstNode, SyntaxKind, SyntaxNode, SyntaxNodePtr, TextRange, WalkEvent,
    ast::{self, HasAttrs as _, HasName as _},
};

use crate::{
    cx::Cx,
    lints::CommandLine,
    lower::{Found, Phase},
    model::Diag,
};

/// When rustc emits a lint of this module, relative to its walk of the crate. Lints of one
/// [`Phase`] are emitted by pass: the pre-expansion ones first, then the rest by where the
/// walk comes to their node, a node's buffered lints before the pass's own there and those of
/// leaving it after everything in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Pass {
    /// rustc's pre-expansion lint pass (`pre_expansion_lint`: `keyword_idents`), run on the
    /// crate as parsed, before anything is expanded or resolved.
    PreExpansion,
    /// A lint the lexer, the parser, expansion or resolution buffered for a node
    /// (`buffer_lint`), emitted as the early lint pass comes to the node (`check_id`).
    Buffered,
    /// The early lint pass's own as it comes to a node (`check_item`, `check_expr`, ..), and
    /// `check_attr`'s.
    Early,
    /// The early lint pass's own as it leaves a node (`check_item_post`, `check_crate_post`,
    /// ..).
    Post,
    /// What attribute parsing buffers as the AST is lowered, emitted after every other
    /// diagnostic of its phase, by the item it is of (`emit_delayed_lints`, after
    /// `check_mod_attrs`).
    Delayed,
}

/// A diagnostic found by one of this module's parts, and when rustc emits it.
#[derive(Clone, Debug)]
pub(crate) struct Emitted {
    pub(crate) found: Found,
    pub(crate) pass: Pass,
    /// The node rustc's walk emits it at; with none, it comes before those of its pass that
    /// have one (what resolution says as it runs, before the early lint pass).
    pub(crate) place: Option<InFile<SyntaxNodePtr>>,
}

impl Emitted {
    /// The lint `diag` of `phase`, its level decided at `at`, emitted in `pass` as rustc's walk
    /// comes to `at`.
    pub(crate) fn new(diag: Diag, phase: Phase, pass: Pass, at: InFile<SyntaxNodePtr>) -> Emitted {
        Emitted { found: Found::lint(diag, phase, at), pass, place: Some(at) }
    }

    /// `diag`, an error or warning that is no lint's, emitted in `pass` as rustc's walk comes
    /// to `at` (E0783 of the early lint pass).
    pub(crate) fn placed(diag: Diag, phase: Phase, pass: Pass, at: InFile<SyntaxNodePtr>) -> Emitted {
        Emitted { found: Found::new(diag, phase), pass, place: Some(at) }
    }

    /// `diag`, an error or warning that is no lint's, emitted in `pass` before the walk (the
    /// label-shadowing warning of late resolution).
    pub(crate) fn unplaced(diag: Diag, phase: Phase, pass: Pass) -> Emitted {
        Emitted { found: Found::new(diag, phase), pass, place: None }
    }
}

/// A syntax tree rustc's early lint pass walks: a file of the crate's modules, or what a macro
/// call, an attribute macro or a derive in one expands to.
pub(crate) struct Tree {
    pub(crate) file: HirFileId,
    pub(crate) root: SyntaxNode,
    /// The macro call it is the expansion of, when it is one.
    pub(crate) call: Option<MacroCallId>,
    /// Its nodes rustc's walk comes to, in pre-order: not those `cfg` removes, nor an item an
    /// attribute macro replaces (what that expands to is a tree of its own).
    pub(crate) nodes: Vec<SyntaxNode>,
    /// The outermost nodes `cfg` removes: their `cfg`s are evaluated, nothing in them is.
    pub(crate) off: Vec<SyntaxNode>,
    /// The items an attribute macro replaces with its expansion.
    pub(crate) replaced: Vec<SyntaxNode>,
    /// The ranges of `off` and `replaced`, disjoint, by where they start.
    skips: Vec<TextRange>,
    /// Where the walk comes to the tree: the places of the `mod` items and macro calls it is
    /// reached through, outermost first.
    place: Vec<(u32, u32)>,
}

impl Tree {
    /// Whether the tree is a file's own, not a macro's expansion.
    pub(crate) fn in_file(&self) -> bool {
        self.call.is_none()
    }

    /// Whether `range` is in what rustc's walk never sees: a node `cfg` removes or an item an
    /// attribute macro replaces.
    pub(crate) fn skipped(&self, range: TextRange) -> bool {
        let after = self.skips.partition_point(|it| it.start() <= range.start());
        after > 0 && self.skips[after - 1].contains_range(range)
    }

    /// `node` of the tree, as the [`InFile`] the rest of the crate takes.
    pub(crate) fn ptr(&self, node: &SyntaxNode) -> InFile<SyntaxNodePtr> {
        InFile::new(self.file, SyntaxNodePtr::new(node))
    }
}

/// The crate as rustc's early lint pass walks it: every file of its modules and every
/// expansion in them, in the order the walk comes to them, each with what `cfg` keeps of it.
pub(crate) struct Walk<'db> {
    pub(crate) trees: Vec<Tree>,
    /// The crate's cfg: `--cfg` and the target's.
    pub(crate) cfg: &'db CfgOptions,
    index: FxHashMap<HirFileId, usize>,
    /// Each module of the crate by the order rustc checks it in (`checks::modules`).
    modules: FxHashMap<hir::Module, usize>,
}

impl<'db> Walk<'db> {
    pub(crate) fn of(cx: &Cx<'_, 'db>) -> Walk<'db> {
        let modules = crate::checks::modules(cx);
        let mut walk = Walk {
            trees: Vec::new(),
            cfg: cx.krate.cfg(cx.db),
            index: FxHashMap::default(),
            modules: modules.iter().enumerate().map(|(rank, it)| (*it, rank)).collect(),
        };
        let mut places: FxHashMap<HirFileId, Vec<(u32, u32)>> = FxHashMap::default();
        for module in modules {
            let Some(file) = module.as_source_file_id(cx.db) else { continue };
            let file_id = HirFileId::from(file);
            if walk.index.contains_key(&file_id) {
                continue;
            }
            let mut place = Vec::new();
            if let Some(declared) = module.declaration_source_range(cx.db) {
                let declared = declared.original_node_file_range_rooted(cx.db);
                place = places.get(&HirFileId::from(declared.file_id)).cloned().unwrap_or_default();
                place.push(entered(declared.range));
            }
            places.insert(file_id, place.clone());
            let root = cx.sema.parse(file).syntax().clone();
            walk.add(cx, file_id, root, place);
        }
        walk
    }

    /// Adds the tree `root` of `file`, and the trees of what the macros in it expand to.
    fn add(&mut self, cx: &Cx<'_, 'db>, file: HirFileId, root: SyntaxNode, place: Vec<(u32, u32)>) {
        // The tree's index is taken now: what it expands to is added after it is pushed.
        match self.index.entry(file) {
            Entry::Occupied(_) => return,
            Entry::Vacant(it) => it.insert(self.trees.len()),
        };
        // About one node per 16 bytes of Rust source.
        let mut nodes = Vec::with_capacity(usize::from(root.text_range().len()) / 16);
        let mut off = Vec::new();
        let mut replaced = Vec::new();
        let mut expansions: Vec<((u32, u32), InFile<SyntaxNode>)> = Vec::new();
        let mut events = root.preorder();
        while let Some(event) = events.next() {
            let WalkEvent::Enter(node) = event else { continue };
            let kind = node.kind();
            if ast::AnyHasAttrs::can_cast(kind)
                && !self.on(&ast::AnyHasAttrs::cast(node.clone()).expect("checked by kind"))
            {
                off.push(node);
                events.skip_subtree();
                continue;
            }
            if ast::Item::can_cast(kind)
                && let Some(item) = ast::Item::cast(node.clone())
                && item.attrs().next().is_some()
                && let Some(expanded) = cx.sema.expand_attr_macro(&item)
            {
                expansions.push((entered(node.text_range()), expanded.value));
                replaced.push(node);
                events.skip_subtree();
                continue;
            }
            if kind == SyntaxKind::MACRO_CALL
                && let Some(call) = ast::MacroCall::cast(node.clone())
                && let Some(expanded) = cx.sema.expand_macro_call(&call)
            {
                expansions.push((entered(node.text_range()), expanded));
            }
            if ast::Adt::can_cast(kind)
                && let Some(owner) = ast::AnyHasAttrs::cast(node.clone())
            {
                // What a derive makes comes after the item it is on.
                let after = (u32::from(node.text_range().end()), 1);
                for (meta, _) in self.attrs(&owner) {
                    let is_derive = matches!(&meta, ast::Meta::TokenTreeMeta(it)
                        if it.path().and_then(|it| it.as_single_name_ref()).is_some_and(|it| it.text() == "derive"));
                    if !is_derive {
                        continue;
                    }
                    let Some(derived) = cx.sema.expand_derive_macro(&meta) else { continue };
                    for root in derived.into_iter().flatten() {
                        let file = cx.sema.hir_file_for(&root.value);
                        if file.is_macro() {
                            expansions.push((after, InFile::new(file, root.value)));
                        }
                    }
                }
            }
            nodes.push(node);
        }
        let mut skips: Vec<TextRange> = off.iter().chain(&replaced).map(|it| it.text_range()).collect();
        radix::sort_copy_by_key(&mut skips, |it| radix::pack_32(it.start().into(), it.end().into()));
        self.trees.push(Tree { file, root, call: file.macro_file(), nodes, off, replaced, skips, place: place.clone() });
        for (at, expanded) in expansions {
            let mut place = place.clone();
            place.push(at);
            self.add(cx, expanded.file_id, expanded.value, place);
        }
    }

    /// Whether `cfg` keeps `owner`: each of its `#[cfg]`s, those `cfg_attr` gives included,
    /// holds, as rustc evaluates them in order and stops at the first that does not.
    pub(crate) fn on(&self, owner: &ast::AnyHasAttrs) -> bool {
        let cfg = self.cfg;
        hir_expand::attrs::expand_cfg_attr(
            ast::attrs_including_inner(owner),
            || cfg,
            |meta, _| match meta {
                ast::Meta::CfgMeta(meta)
                    if meta
                        .cfg_predicate()
                        .is_some_and(|it| cfg.check(&CfgExpr::parse_from_ast(it)) == Some(false)) =>
                {
                    ControlFlow::Break(())
                }
                _ => ControlFlow::Continue(()),
            },
        )
        .is_none()
    }

    /// The attributes of `owner` as rustc sees them once `cfg_attr` is expanded, outer then
    /// inner, in order: each with the attribute it is written in.
    pub(crate) fn attrs(&self, owner: &ast::AnyHasAttrs) -> Vec<(ast::Meta, ast::Attr)> {
        let mut attrs = Vec::new();
        hir_expand::attrs::expand_cfg_attr::<()>(
            ast::attrs_including_inner(owner),
            || self.cfg,
            |meta, attr| {
                attrs.push((meta, attr));
                ControlFlow::Continue(())
            },
        );
        attrs
    }

    /// The tree of `file`, if the walk has it.
    pub(crate) fn tree(&self, file: HirFileId) -> Option<&Tree> {
        self.index.get(&file).map(|&it| &self.trees[it])
    }

    /// The trees of the crate's files, not of expansions.
    pub(crate) fn files(&self) -> impl Iterator<Item = &Tree> {
        self.trees.iter().filter(|it| it.in_file())
    }

    /// Where rustc's walk comes to `at` in `pass`: as it enters it, or for [`Pass::Post`] as it
    /// leaves it. A node of an expansion the walk does not have is placed at the call it came
    /// from.
    pub(crate) fn place(&self, cx: &Cx<'_, '_>, at: InFile<SyntaxNodePtr>, pass: Pass) -> Vec<(u32, u32)> {
        let (mut file, mut range) = (at.file_id, at.value.text_range());
        loop {
            if let Some(tree) = self.tree(file) {
                let mut place = tree.place.clone();
                place.push(match pass {
                    Pass::Post => (u32::from(range.end()), 0),
                    _ => entered(range),
                });
                return place;
            }
            let Some(call) = file.macro_file() else { return Vec::new() };
            let node = call.loc(cx.db).to_node(cx.db);
            (file, range) = (node.file_id, node.value.text_range());
        }
    }
}

/// The place of entering a node of `range`: outer nodes before the nodes in them.
fn entered(range: TextRange) -> (u32, u32) {
    (u32::from(range.start()), u32::MAX - u32::from(range.end()))
}

/// Whether rustc's span of `node` is of a macro's making (`Span::from_expansion`): it is in
/// an expansion and not written, whole, in what the call was handed or in an attribute
/// macro's item.
pub(crate) fn from_expansion(cx: &Cx<'_, '_>, node: InFile<&SyntaxNode>) -> bool {
    match node.file_id.macro_file() {
        None => false,
        Some(_) => !node
            .map(|it| it.text_range())
            .original_node_file_range_opt(cx.db)
            .is_some_and(|(_, ctxt)| ctxt.is_root()),
    }
}

/// Records on `diag` what a lint of the source at `at` read: the code of the body it is in,
/// else the signature of the item it is of.
pub(crate) fn source_fact(cx: &Cx<'_, '_>, diag: &mut Diag, at: InFile<SyntaxNodePtr>) {
    if crate::lints::body_item(cx, at).is_some() {
        crate::lints::code_fact(cx, diag, at);
        return;
    }
    if let Some((item, name)) = signature_item(cx, at) {
        diag.fact(Fact::Signature(cx.key(item)), format!("the signature of `{name}`"));
    }
}

/// The innermost item `at` is in or is, and its name, through a macro's expansion to where it
/// was called.
fn signature_item(cx: &Cx<'_, '_>, at: InFile<SyntaxNodePtr>) -> Option<(ItemId, String)> {
    let mut node: InFile<SyntaxNode> = cx.syntax(at);
    loop {
        for ancestor in node.value.ancestors() {
            let name = |it: Option<ast::Name>| it.map(|it| it.text().to_string());
            if let Some(it) = ast::Fn::cast(ancestor.clone()) {
                let def = cx.sema.to_def2(node.with_value(&it))?;
                let id = hir_def::FunctionId::try_from(def).ok()?;
                return Some((ItemId::Function(id), name(it.name())?));
            }
            if let Some(it) = ast::Adt::cast(ancestor.clone()) {
                let id = match cx.sema.to_def2(node.with_value(&it))? {
                    hir::Adt::Struct(it) => AdtId::StructId(it.into()),
                    hir::Adt::Enum(it) => AdtId::EnumId(it.into()),
                    hir::Adt::Union(it) => AdtId::UnionId(it.into()),
                };
                return Some((ItemId::Adt(id), name(it.name())?));
            }
            if let Some(it) = ast::Trait::cast(ancestor.clone()) {
                let def = cx.sema.to_def2(node.with_value(&it))?;
                return Some((ItemId::Trait(def.into()), name(it.name())?));
            }
            if let Some(it) = ast::TypeAlias::cast(ancestor.clone()) {
                let def = cx.sema.to_def2(node.with_value(&it))?;
                return Some((ItemId::Alias(def.into()), name(it.name())?));
            }
            if let Some(it) = ast::Const::cast(ancestor.clone()) {
                let def = cx.sema.to_def2(node.with_value(&it))?;
                return Some((ItemId::Const(def.into()), name(it.name()).unwrap_or_else(|| "_".to_owned())));
            }
            if let Some(it) = ast::Static::cast(ancestor.clone()) {
                let def = cx.sema.to_def2(node.with_value(&it))?;
                return Some((ItemId::Static(def.into()), name(it.name())?));
            }
            if ast::Impl::can_cast(ancestor.kind()) || ast::Module::can_cast(ancestor.kind()) {
                return None;
            }
        }
        let call = node.file_id.macro_file()?;
        node = call.loc(cx.db).to_node(cx.db);
    }
}

/// What rustc's early lint pass, its expansion and its attribute checks lint on the source as
/// written, in the order it emits them.
pub(crate) fn lints(cx: &Cx<'_, '_>, cmd: &CommandLine) -> Vec<Found> {
    let walk = Walk::of(cx);
    let mut emitted = check_cfg::lints(cx, cmd, &walk);
    emitted.extend(early::lints(cx, &walk));
    emitted.extend(delims::lints(cx, &walk));
    emitted.extend(attrs::lints(cx, &walk));
    // Per emitted diagnostic, not per node: each key is made once, and what it costs scales with
    // the output.
    let mut keyed: Vec<_> = emitted
        .into_iter()
        .map(|Emitted { found, pass, place }| {
            let group = match pass {
                Pass::PreExpansion => 0u8,
                Pass::Delayed => 2,
                _ => 1,
            };
            let module = match (found.phase, place) {
                (Phase::Items, Some(at)) => checked_in(cx, &walk.modules, at),
                _ => (0, false),
            };
            let place = place.map(|at| walk.place(cx, at, pass)).unwrap_or_default();
            let rank = found.diag.lint_name().map_or(EARLY_PASSES.len(), pass_rank);
            ((found.phase, group, module, place, pass, rank), found)
        })
        .collect();
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    keyed.into_iter().map(|(_, found)| found).collect()
}

/// Where rustc's `check_mod_attrs` comes to `at`: it checks each module in turn (`modules`,
/// rustc's `par_hir_for_each_module`), in it every item whose module it is, then for the
/// crate root the crate's own attributes. A module file's inner attributes are of its `mod`
/// item, checked in the module that has it.
fn checked_in(cx: &Cx<'_, '_>, modules: &FxHashMap<hir::Module, usize>, at: InFile<SyntaxNodePtr>) -> (usize, bool) {
    let node = cx.syntax(at);
    let rank = |module: hir::Module| modules.get(&module).copied().unwrap_or(usize::MAX);
    if let Some(file) = ast::SourceFile::cast(node.value.clone()) {
        let Some(module) = cx.sema.to_def2(node.with_value(&file)) else { return (usize::MAX, false) };
        return match module.parent(cx.db) {
            None => (rank(module), true),
            Some(parent) => (rank(parent), false),
        };
    }
    for ancestor in cx.sema.ancestors_with_macros_file(node.clone()).skip(1) {
        if let Some(module) = ast::Module::cast(ancestor.value.clone())
            && module.item_list().is_some()
        {
            return (cx.sema.to_def2(ancestor.with_value(&module)).map_or(usize::MAX, rank), false);
        }
        if let Some(file) = ast::SourceFile::cast(ancestor.value.clone()) {
            return (cx.sema.to_def2(ancestor.with_value(&file)).map_or(usize::MAX, rank), false);
        }
    }
    (usize::MAX, false)
}

/// The lints of the passes of rustc's `BuiltinCombinedEarlyLintPass`, in the order it runs
/// them on a node (`compiler/rustc_lint/src/lib.rs`).
const EARLY_PASSES: &[&[&str]] = &[
    &["unused_parens"],
    &["unused_braces"],
    &["unused_import_braces"],
    &["unsafe_code"],
    &["special_module_name"],
    &["anonymous_parameters"],
    &["ellipsis_inclusive_range_patterns"],
    &["non_camel_case_types"],
    &["while_true"],
    &["non_ascii_idents", "uncommon_codepoints", "confusable_idents", "mixed_script_confusables"],
    &["incomplete_features", "internal_features"],
    &["redundant_semicolons"],
    &["unused_doc_comments"],
    &["edition_2024_expr_fragment_specifier"],
    &["ambiguous_negative_literals"],
    &["double_negations"],
];

/// Where `lint`'s pass is among those rustc runs on a node: lints of one node and pass come in
/// that order; what no early pass gives comes after, as it was found.
fn pass_rank(lint: &str) -> usize {
    EARLY_PASSES.iter().position(|it| it.contains(&lint)).unwrap_or(EARLY_PASSES.len())
}

#[cfg(test)]
mod tests {
    use hir::Semantics;
    use syntax::TextSize;

    use super::*;

    /// `check` given the walk of the first workspace crate of `fixture`.
    fn walked(fixture: &str, check: impl FnOnce(&Cx<'_, '_>, &Walk<'_>)) {
        let checked = crate::tests::checked(fixture);
        let db = &checked.db;
        hir::attach_db(db, || {
            let sema = Semantics::new_dyn(db);
            let krate = hir::check::workspace_crates(db)[0];
            let cx = Cx::new(&sema, krate, &checked.names);
            check(&cx, &Walk::of(&cx));
        });
    }

    /// Each function the walk comes to, by name, with its tree and node.
    fn fns<'w>(walk: &'w Walk<'_>) -> Vec<(String, &'w Tree, SyntaxNode)> {
        walk.trees
            .iter()
            .flat_map(|tree| tree.nodes.iter().map(move |node| (tree, node)))
            .filter_map(|(tree, node)| {
                let name = ast::Fn::cast(node.clone())?.name()?.text().to_string();
                Some((name, tree, node.clone()))
            })
            .collect()
    }

    fn place_of(cx: &Cx<'_, '_>, walk: &Walk<'_>, name: &str) -> Vec<(u32, u32)> {
        let fns = fns(walk);
        let (_, tree, node) = fns.iter().find(|(it, ..)| it == name).unwrap_or_else(|| panic!("no `{name}`"));
        walk.place(cx, tree.ptr(node), Pass::Early)
    }

    #[test]
    fn what_cfg_removes_is_not_walked() {
        walked(
            r#"
//- /main.rs crate:main
#[cfg(FALSE)]
fn off() { let x = 1; }
fn on() {}
"#,
            |_, walk| {
                let names: Vec<String> = fns(walk).into_iter().map(|(it, ..)| it).collect();
                assert_eq!(names, ["on"]);
                let tree = walk.files().next().unwrap();
                assert_eq!(tree.off.len(), 1);
                assert!(tree.skipped(tree.off[0].text_range()));
            },
        );
    }

    #[test]
    fn a_module_file_is_walked_where_it_is_declared() {
        walked(
            r#"
//- /main.rs crate:main
fn before() {}
mod a;
fn after() {}
//- /a.rs
fn inside() {}
"#,
            |cx, walk| {
                assert_eq!(walk.files().count(), 2);
                let before = place_of(cx, walk, "before");
                let inside = place_of(cx, walk, "inside");
                let after = place_of(cx, walk, "after");
                assert!(before < inside && inside < after, "{before:?} {inside:?} {after:?}");
            },
        );
    }

    #[test]
    fn an_expansion_is_walked_where_it_is_called() {
        walked(
            r#"
//- /main.rs crate:main
macro_rules! m { ($e:expr) => { fn made() -> i32 { $e } }; }
fn before() {}
m!(1 + 2);
fn after() {}
"#,
            |cx, walk| {
                let fns = fns(walk);
                let (_, tree, made) = fns.iter().find(|(it, ..)| it == "made").unwrap();
                assert!(tree.call.is_some() && !tree.in_file());
                assert!(from_expansion(cx, InFile::new(tree.file, made)));
                let sum = tree.nodes.iter().find(|it| ast::BinExpr::can_cast(it.kind())).unwrap();
                assert!(!from_expansion(cx, InFile::new(tree.file, sum)));
                let (before, made, after) =
                    (place_of(cx, walk, "before"), place_of(cx, walk, "made"), place_of(cx, walk, "after"));
                assert!(before < made && made < after, "{before:?} {made:?} {after:?}");
            },
        );
    }

    #[test]
    fn a_node_is_entered_before_what_is_in_it_and_left_after() {
        let range = |lo: u32, hi: u32| TextRange::new(TextSize::from(lo), TextSize::from(hi));
        let (outer, inner, next) = (range(0, 10), range(2, 5), range(10, 12));
        assert!(entered(outer) < entered(inner));
        let left = (u32::from(outer.end()), 0);
        assert!(entered(inner) < left && left < entered(next));
    }

    #[test]
    fn lints_of_a_node_come_in_the_order_of_rustcs_passes() {
        assert!(pass_rank("unused_parens") < pass_rank("while_true"));
        assert!(pass_rank("non_camel_case_types") < pass_rank("while_true"));
        assert_eq!(pass_rank("unexpected_cfgs"), EARLY_PASSES.len());
    }
}

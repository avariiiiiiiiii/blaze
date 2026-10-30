//! The crate walked as rustc's stability checks walk it: each item-like (rustc's HIR owner:
//! an item, a trait item, an impl item, a foreign item) in the crate's order with the module
//! it is of, and in each the uses the checks look at, as the node they are at.
//!
//! - paths resolved by name resolution: `Checker::visit_path`, `visit_use` (one check of a
//!   constructor imported with its struct), the parent segments (stability alone);
//! - type-relative paths, method calls, field accesses, the fields of struct expressions and
//!   patterns and of tuple-struct patterns, associated item constraints: typeck's and
//!   `hir_ty_lowering`'s `check_stability`;
//! - macro calls, derives and attribute macros: resolution's
//!   `check_stability_and_deprecation`;
//! - each `#[deprecated]` with its target, and each `#[macro_escape]` of a `mod`.
//!
//! Follows `compiler/rustc_passes/src/stability.rs` (`Checker`, `is_unstable_reexport`),
//! `compiler/rustc_hir/src/intravisit.rs` (`walk_use`), `compiler/rustc_middle/src/hir/map.rs`
//! (`hir_module_items`: items, trait items, impl items, foreign items),
//! `compiler/rustc_ast_lowering/src/item.rs` (`lower_use_tree`), `compiler/rustc_hir_typeck/
//! src/method/mod.rs` (`lookup_method`, `resolve_fully_qualified_call`), `expr.rs`
//! (`check_expr_field`, `check_expr_struct_fields`), `pat.rs` (`check_pat_tuple_struct`,
//! `check_struct_pat_fields`), `compiler/rustc_hir_analysis/src/hir_ty_lowering/mod.rs`
//! (`lower_type_relative_path`, `check_assoc_item`), `compiler/rustc_resolve/src/macros.rs`
//! (`check_stability_and_deprecation`) and `compiler/rustc_resolve/src/build_reduced_graph.rs`
//! (`contains_macro_use`) (MIT or Apache-2.0).

use std::ops::ControlFlow;

use cfg::{CfgExpr, CfgOptions};
use either::Either;
use hir::{Adt, HasVisibility as _, InFile, ModuleDef, ModuleSource, PathResolution};
use syntax::{AstNode, SyntaxNode, SyntaxNodePtr, TextRange, ast};

use super::{
    attr,
    def::{Def, trait_item},
    target::{Attribute, GenericParamKind, MethodKind, Target},
};
use crate::{cx::Cx, lower::Phase};

/// What an item-like is in: rustc's `hir_module_items` lists a module's items, trait items,
/// impl items and foreign items, in that order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Container {
    Free,
    Trait,
    Impl { of_trait: bool },
    Foreign,
}

/// An item-like, rustc's HIR owner: what `hir_get_parent_item` of a use in it is.
pub(super) struct Owner {
    pub(super) node: InFile<SyntaxNode>,
    /// The module it is listed in, by where rustc's walk of modules comes to it.
    pub(super) module: usize,
    pub(super) container: Container,
}

/// A use of a definition the stability checks look at.
pub(super) struct Use {
    pub(super) def: Def,
    /// The item-like it is in; `None` at the crate root.
    pub(super) owner: Option<usize>,
    /// The node of the use, where the lint's level is decided.
    pub(super) at: InFile<SyntaxNodePtr>,
    /// Where a deprecation is reported: `method_span.unwrap_or(span)`.
    pub(super) span: InFile<TextRange>,
    /// Where an unstable use is reported: `span`.
    pub(super) whole: InFile<TextRange>,
    /// The use's node is an expression: a deprecation's suggestion is given.
    pub(super) expr: bool,
    pub(super) phase: Phase,
    /// A parent segment of a path or the trait of a `<Type as Trait>`: checked for stability
    /// alone (`check_stability` with no `id`).
    pub(super) stability_only: bool,
    /// For a parent segment, the use of what the whole path resolves to, which is checked
    /// first and decides whether the segments are.
    pub(super) segment_of: Option<usize>,
    /// In what a derive made (`Span::in_derive_expansion`): no deprecation is reported.
    pub(super) derive: bool,
}

/// A macro called, derived or used as an attribute, as written.
pub(super) struct MacroUse {
    pub(super) mac: hir::Macro,
    /// The path as `pprust::path_to_string` prints it.
    pub(super) path: String,
    pub(super) span: InFile<TextRange>,
    pub(super) at: InFile<SyntaxNodePtr>,
    pub(super) derive: bool,
}

/// A `#[macro_escape]` of a `mod`.
pub(super) struct Escape {
    pub(super) range: InFile<TextRange>,
    pub(super) inner: bool,
    pub(super) at: InFile<SyntaxNodePtr>,
}

/// What the walk found, each in the order rustc comes to it.
#[derive(Default)]
pub(super) struct Walked {
    pub(super) owners: Vec<Owner>,
    pub(super) uses: Vec<Use>,
    pub(super) macros: Vec<MacroUse>,
    /// Each `#[deprecated]`, with whether it is on an item of a trait impl.
    pub(super) attrs: Vec<(Attribute, bool)>,
    pub(super) escapes: Vec<Escape>,
}

#[derive(Clone, Copy)]
struct Ctx {
    owner: Option<usize>,
    module: usize,
    /// What the items written here are in.
    container: Container,
    /// In a body (rustc's typeck lowers what is in it): in an expression or a pattern.
    body: bool,
    derive: bool,
}

struct Walker<'a, 'b, 'db> {
    cx: &'a Cx<'b, 'db>,
    cfg: &'db CfgOptions,
    modules: usize,
    walked: Walked,
}

/// The crate's item-likes and uses, in rustc's order.
pub(super) fn walk(cx: &Cx<'_, '_>) -> Walked {
    let db = cx.db;
    let mut walker = Walker { cx, cfg: cx.krate.cfg(db), modules: 1, walked: Walked::default() };
    let file = cx.krate.root_module(db).definition_source(db).file_id;
    let root = InFile::new(file, cx.sema.parse_or_expand(file));
    let ctx = Ctx { owner: None, module: 0, container: Container::Free, body: false, derive: false };
    walker.attributes(&root, Some(Target::Crate), false);
    walker.children(&root, ctx);
    walker.walked
}

impl Walker<'_, '_, '_> {
    /// Whether `cfg` keeps `node`: each of its `#[cfg]`s holds.
    fn on(&self, node: &SyntaxNode) -> bool {
        let Some(owner) = ast::AnyHasAttrs::cast(node.clone()) else { return true };
        let cfg = self.cfg;
        hir_expand::attrs::expand_cfg_attr(ast::attrs_including_inner(&owner), || cfg, |meta, _| match meta {
            ast::Meta::CfgMeta(meta)
                if meta.cfg_predicate().is_some_and(|it| cfg.check(&CfgExpr::parse_from_ast(it)) == Some(false)) =>
            {
                ControlFlow::Break(())
            }
            _ => ControlFlow::Continue(()),
        })
        .is_none()
    }

    fn children(&mut self, node: &InFile<SyntaxNode>, ctx: Ctx) {
        for child in node.value.children() {
            self.visit(node.with_value(child), ctx);
        }
    }

    fn visit(&mut self, node: InFile<SyntaxNode>, ctx: Ctx) {
        if !self.on(&node.value) || ast::Attr::can_cast(node.value.kind()) {
            return;
        }
        if let Some(call) = ast::MacroCall::cast(node.value.clone()) {
            self.macro_call(&node, &call, ctx);
            return;
        }
        if let Some(item) = ast::Item::cast(node.value.clone()) {
            if let Some(expanded) = self.cx.sema.expand_attr_macro(&item) {
                self.attr_macro(&node, &item, ctx);
                self.visit(expanded.value, ctx);
                return;
            }
            self.item(node, item, ctx);
            return;
        }
        let target = other_target(&node.value);
        self.attributes(&node, target, false);
        self.uses_at(&node, ctx);
        let kind = node.value.kind();
        let ctx = Ctx { body: ctx.body || ast::Expr::can_cast(kind) || ast::Pat::can_cast(kind), ..ctx };
        self.children(&node, ctx);
    }

    fn owner(&mut self, node: &InFile<SyntaxNode>, ctx: Ctx) -> usize {
        self.walked.owners.push(Owner { node: node.clone(), module: ctx.module, container: ctx.container });
        self.walked.owners.len() - 1
    }

    fn item(&mut self, node: InFile<SyntaxNode>, item: ast::Item, ctx: Ctx) {
        let owner = self.owner(&node, ctx);
        let inner = Ctx { owner: Some(owner), container: Container::Free, body: false, ..ctx };
        let of_trait_impl = ctx.container == Container::Impl { of_trait: true };
        self.attributes(&node, item_target(&item, ctx.container), of_trait_impl);
        match &item {
            ast::Item::Module(module) => self.module(&node, module, inner),
            ast::Item::Impl(imp) => {
                let list = Ctx { container: Container::Impl { of_trait: imp.trait_().is_some() }, ..inner };
                self.children_with(&node, inner, |it| ast::AssocItemList::can_cast(it.kind()).then_some(list));
            }
            ast::Item::Trait(_) => {
                let list = Ctx { container: Container::Trait, ..inner };
                self.children_with(&node, inner, |it| ast::AssocItemList::can_cast(it.kind()).then_some(list));
            }
            ast::Item::ExternBlock(_) => {
                let list = Ctx { container: Container::Foreign, ..inner };
                self.children_with(&node, inner, |it| ast::ExternItemList::can_cast(it.kind()).then_some(list));
            }
            ast::Item::Struct(_) | ast::Item::Enum(_) | ast::Item::Union(_) => {
                self.children(&node, inner);
                self.derives(&node, ctx);
            }
            _ => self.children(&node, inner),
        }
    }

    /// Visits the children of `node`, each with the context `pick` gives it, else `ctx`.
    fn children_with(&mut self, node: &InFile<SyntaxNode>, ctx: Ctx, pick: impl Fn(&SyntaxNode) -> Option<Ctx>) {
        for child in node.value.children() {
            let ctx = pick(&child).unwrap_or(ctx);
            self.visit(node.with_value(child), ctx);
        }
    }

    fn module(&mut self, node: &InFile<SyntaxNode>, module: &ast::Module, inner: Ctx) {
        let cx = self.cx;
        let contents = Ctx { module: self.modules, ..inner };
        self.modules += 1;
        let mut attrs = attr::attrs(&node.value, self.cfg);
        match module.item_list() {
            Some(_) => self.children(node, contents),
            None => {
                self.children(node, inner);
                if let Some(def) = cx.sema.to_def2(node.with_value(module))
                    && let source = def.definition_source(cx.db)
                    && let ModuleSource::SourceFile(_) = source.value
                {
                    let root = InFile::new(source.file_id, cx.sema.parse_or_expand(source.file_id));
                    if self.on(&root.value) {
                        attrs.extend(attr::attrs(&root.value, self.cfg));
                        self.attributes(&root, Some(Target::Mod), false);
                        self.children(&root, contents);
                    }
                }
            }
        }
        let first = attrs.iter().find(|(meta, _)| attr::is(meta, "macro_use") || attr::is(meta, "macro_escape"));
        if let Some((meta, attr)) = first
            && attr::is(meta, "macro_escape")
        {
            let file = cx.sema.hir_file_for(attr.syntax());
            let range = match attr.meta() {
                Some(it) if it.syntax() == meta.syntax() => attr.syntax().text_range(),
                _ => meta.syntax().text_range(),
            };
            let inner = attr.kind().is_inner();
            self.walked.escapes.push(Escape {
                range: InFile::new(file, range),
                inner,
                at: node.map(|it| SyntaxNodePtr::new(&it)),
            });
        }
    }

    /// The `#[deprecated]`s of `node`, on `target`.
    fn attributes(&mut self, node: &InFile<SyntaxNode>, target: Option<Target>, of_trait_impl: bool) {
        let Some(target) = target else { return };
        for (meta, written) in attr::attrs(&node.value, self.cfg) {
            if attr::is(&meta, "deprecated")
                && let Some(found) = Attribute::new(target, node.as_ref(), &meta, &written)
            {
                self.walked.attrs.push((found, of_trait_impl));
            }
        }
    }

    fn macro_call(&mut self, node: &InFile<SyntaxNode>, call: &ast::MacroCall, ctx: Ctx) {
        let cx = self.cx;
        if let Some(mac) = cx.sema.resolve_macro_call(call)
            && let Some(path) = call.path()
        {
            let at = node.map(|it| SyntaxNodePtr::new(&it));
            let span = node.with_value(path.syntax().text_range());
            self.walked.macros.push(MacroUse { mac, path: written(path.syntax()), span, at, derive: ctx.derive });
        }
        if let Some(expanded) = cx.sema.expand_macro_call(call) {
            self.visit(expanded, ctx);
        }
    }

    fn attr_macro(&mut self, node: &InFile<SyntaxNode>, item: &ast::Item, ctx: Ctx) {
        let cx = self.cx;
        let Some(mac) = cx.sema.resolve_attr_macro_call(item) else { return };
        let name = mac.name(cx.db);
        let name = name.as_str();
        let found = attr::attrs(&node.value, self.cfg).into_iter().find_map(|(meta, _)| {
            let path = meta.path()?;
            (path.segment()?.name_ref()?.text() == name).then_some(path)
        });
        if let Some(path) = found {
            let at = node.map(|it| SyntaxNodePtr::new(&it));
            let span = node.with_value(path.syntax().text_range());
            self.walked.macros.push(MacroUse { mac, path: written(path.syntax()), span, at, derive: ctx.derive });
        }
    }

    /// The derives of the ADT at `node`: each derive macro, then what each made.
    fn derives(&mut self, node: &InFile<SyntaxNode>, ctx: Ctx) {
        let cx = self.cx;
        let at = node.map(|it| SyntaxNodePtr::new(&it));
        for (meta, written_attr) in attr::attrs(&node.value, self.cfg) {
            if !attr::is(&meta, "derive") {
                continue;
            }
            let macros = cx.sema.resolve_derive_macro(&meta).unwrap_or_default();
            let paths = match written_attr.meta() {
                Some(it) if it.syntax() == meta.syntax() => crate::cx::derive_paths(&written_attr),
                _ => Vec::new(),
            };
            for (mac, (range, path)) in macros.into_iter().zip(paths) {
                if let Some(mac) = mac {
                    let span = node.with_value(range);
                    self.walked.macros.push(MacroUse { mac, path, span, at, derive: ctx.derive });
                }
            }
            let Some(derived) = cx.sema.expand_derive_macro(&meta) else { continue };
            for root in derived.into_iter().flatten() {
                let file = cx.sema.hir_file_for(&root.value);
                if file.is_macro() {
                    let ctx = Ctx { derive: true, body: false, container: Container::Free, ..ctx };
                    self.visit(InFile::new(file, root.value), ctx);
                }
            }
        }
    }

    fn push(&mut self, ctx: Ctx, def: Def, at: &SyntaxNode, file: hir::HirFileId, span: TextRange, whole: TextRange) -> &mut Use {
        self.walked.uses.push(Use {
            def,
            owner: ctx.owner,
            at: InFile::new(file, SyntaxNodePtr::new(at)),
            span: InFile::new(file, span),
            whole: InFile::new(file, whole),
            expr: false,
            phase: Phase::Items,
            stability_only: false,
            segment_of: None,
            derive: ctx.derive,
        });
        self.walked.uses.last_mut().expect("just pushed")
    }

    fn uses_at(&mut self, node: &InFile<SyntaxNode>, ctx: Ctx) {
        let value = &node.value;
        if let Some(path) = ast::Path::cast(value.clone()) {
            let in_use = path.syntax().parent().is_some_and(|it| ast::UseTree::can_cast(it.kind()));
            if path.parent_path().is_none() && !in_use {
                let anchor_trait = is_anchor_trait(&path);
                self.path(node.with_value(&path), ctx, anchor_trait);
            }
        } else if let Some(tree) = ast::UseTree::cast(value.clone()) {
            self.use_tree(node.with_value(&tree), ctx);
        } else if let Some(call) = ast::MethodCallExpr::cast(value.clone()) {
            self.method_call(node.with_value(&call), ctx);
        } else if let Some(field) = ast::FieldExpr::cast(value.clone()) {
            self.field(node.with_value(&field), ctx);
        } else if let Some(field) = ast::RecordExprField::cast(value.clone()) {
            let Some((def, _, _)) = self.cx.sema.resolve_record_field(&field) else { return };
            if matches!(def.parent_def(self.cx.db), hir::Variant::EnumVariant(_)) {
                return;
            }
            let range = value.text_range();
            let found = self.push(ctx, Def::Field(def), value, node.file_id, range, range);
            found.phase = Phase::Bodies;
        } else if let Some(field) = ast::RecordPatField::cast(value.clone()) {
            let Some((def, _)) = self.cx.sema.resolve_record_pat_field(&field) else { return };
            let range = value.text_range();
            let found = self.push(ctx, Def::Field(def), value, node.file_id, range, range);
            found.phase = Phase::Bodies;
        } else if let Some(pat) = ast::TupleStructPat::cast(value.clone()) {
            self.tuple_struct_pat(node.with_value(&pat), ctx);
        } else if let Some(arg) = ast::AssocTypeArg::cast(value.clone()) {
            self.constraint(node.with_value(&arg), ctx);
        }
    }

    /// A path as rustc's HIR has it: resolved by name resolution (`Checker::visit_path`, the
    /// parent segments for stability alone), or type-relative (typeck's or `hir_ty_lowering`'s
    /// check of what it resolves to, the type before it a path of its own).
    fn path(&mut self, path: InFile<&ast::Path>, ctx: Ctx, stability_only: bool) {
        let cx = self.cx;
        let db = cx.db;
        let value = path.value;
        let parent = value.syntax().parent();
        if value.qualifier().is_none()
            && value.segment().and_then(|it| it.kind()).is_some_and(|it| matches!(it, ast::PathSegmentKind::SelfTypeKw))
        {
            return;
        }
        let Some(res) = cx.sema.resolve_path(value) else { return };
        let relative = type_relative(cx, value, &res);
        let def = match res {
            PathResolution::Def(ModuleDef::BuiltinType(_)) => None,
            PathResolution::Def(def) => {
                let def = trait_item(db, def);
                let value_ns = parent.as_ref().is_some_and(|it| {
                    ast::PathExpr::can_cast(it.kind())
                        || ast::PathPat::can_cast(it.kind())
                        || ast::TupleStructPat::can_cast(it.kind())
                });
                Some(if value_ns { Def::value(db, def) } else { Def::Item(def) })
            }
            _ => None,
        };
        let whole = value.syntax().text_range();
        let last = value.segment().and_then(|it| it.name_ref()).map_or(whole, |it| it.syntax().text_range());
        let at = parent.clone().unwrap_or_else(|| value.syntax().clone());
        let expr = parent.as_ref().is_some_and(|it| ast::PathExpr::can_cast(it.kind()) || ast::RecordExpr::can_cast(it.kind()));
        if let Some(def) = def {
            let in_type = parent.as_ref().is_some_and(|it| ast::PathType::can_cast(it.kind()));
            let span = match relative && in_type {
                true => whole,
                false => last,
            };
            let phase = if relative && ctx.body { Phase::Bodies } else { Phase::Items };
            let found = self.push(ctx, def, &at, path.file_id, span, whole);
            found.expr = expr;
            found.phase = phase;
            found.stability_only = stability_only;
        }
        let Some(qualifier) = value.qualifier() else { return };
        if relative {
            if !matches!(qualifier.segment().and_then(|it| it.kind()), Some(ast::PathSegmentKind::Type { .. })) {
                self.path(path.with_value(&qualifier), ctx, stability_only);
            }
            return;
        }
        let mut segment = Some(qualifier);
        while let Some(qualifier) = segment {
            if let Some(PathResolution::Def(def)) = cx.sema.resolve_path(&qualifier)
                && !matches!(def, ModuleDef::BuiltinType(_))
            {
                let range = qualifier.segment().map_or(qualifier.syntax().text_range(), |it| it.syntax().text_range());
                let found = self.push(ctx, Def::Item(def), &at, path.file_id, range, range);
                found.stability_only = true;
            }
            segment = qualifier.qualifier();
        }
    }

    /// A use tree as `lower_use_tree` makes items of it: a single import checked in each
    /// namespace it imports in (a constructor and its struct once), a glob by what it imports
    /// from, a list by nothing unless it is empty.
    fn use_tree(&mut self, tree: InFile<&ast::UseTree>, ctx: Ctx) {
        let cx = self.cx;
        let db = cx.db;
        let value = tree.value;
        let Some(path) = value.path() else { return };
        if let Some(list) = value.use_tree_list() {
            let root_only = path.qualifier().is_none() && path.segment().is_some_and(|it| it.coloncolon_token().is_some() && it.name_ref().is_none());
            if list.use_trees().next().is_none() && !root_only {
                self.use_path(tree, &path, None, ctx);
            }
            return;
        }
        if value.star_token().is_some() {
            if let Some(PathResolution::Def(def)) = cx.sema.resolve_path(&path) {
                self.use_path(tree, &path, Some(Def::Item(def)), ctx);
            }
            return;
        }
        let Some(per_ns) = cx.sema.resolve_path_per_ns(&path) else { return };
        let def_of = |it: Option<PathResolution<'_>>| match it {
            Some(PathResolution::Def(def)) if !matches!(def, ModuleDef::BuiltinType(_)) => Some(def),
            _ => None,
        };
        let (types, values, macros) = (def_of(per_ns.type_ns), def_of(per_ns.value_ns), def_of(per_ns.macro_ns));
        let mut defs = Vec::new();
        match (types, values) {
            (Some(ty), Some(val)) if ty == val && matches!(Def::value(db, val), Def::Ctor(_)) => {
                defs.push(Def::value(db, val));
            }
            _ => {
                defs.extend(types.map(Def::Item));
                defs.extend(values.map(|it| Def::value(db, it)));
            }
        }
        defs.extend(macros.map(Def::Item));
        for def in defs {
            self.use_path(tree, &path, Some(def), ctx);
        }
    }

    /// The check of a `use` path resolving to `def` (when it is checked), and of its parent
    /// segments, those of the trees it is nested in first.
    fn use_path(&mut self, tree: InFile<&ast::UseTree>, path: &ast::Path, def: Option<Def>, ctx: Ctx) {
        let cx = self.cx;
        let file = tree.file_id;
        let at = tree.value.syntax().clone();
        let whole = path.syntax().text_range();
        let last = path.segment().map_or(whole, |it| it.syntax().text_range());
        let def = match def {
            Some(def) => Some(def),
            None => match cx.sema.resolve_path(path) {
                Some(PathResolution::Def(def)) => Some(Def::Item(def)),
                _ => None,
            },
        };
        let Some(def) = def else { return };
        self.push(ctx, trait_item_def(cx, def), &at, file, last, whole);
        let mut parents = Vec::new();
        let mut segment = path.qualifier();
        while let Some(qualifier) = segment {
            segment = qualifier.qualifier();
            parents.push(qualifier);
        }
        for outer in tree.value.syntax().ancestors().skip(1).filter_map(ast::UseTree::cast) {
            let Some(path) = outer.path() else { continue };
            let mut segment = Some(path);
            while let Some(qualifier) = segment {
                segment = qualifier.qualifier();
                parents.push(qualifier);
            }
        }
        for parent in parents {
            if let Some(PathResolution::Def(def)) = cx.sema.resolve_path(&parent)
                && !matches!(def, ModuleDef::BuiltinType(_))
            {
                let range = parent.segment().map_or(parent.syntax().text_range(), |it| it.syntax().text_range());
                let found = self.push(ctx, Def::Item(def), &at, file, range, range);
                found.stability_only = true;
            }
        }
    }

    /// `lookup_method`'s check of the method picked, at its name.
    fn method_call(&mut self, call: InFile<&ast::MethodCallExpr>, ctx: Ctx) {
        let cx = self.cx;
        let Some(function) = cx.sema.resolve_method_call(call.value) else { return };
        let Some(name) = call.value.name_ref() else { return };
        let def = Def::Item(trait_item(cx.db, ModuleDef::Function(function)));
        let range = name.syntax().text_range();
        let found = self.push(ctx, def, call.value.syntax(), call.file_id, range, range);
        found.expr = true;
        found.phase = Phase::Bodies;
    }

    /// `check_expr_field`'s check of a field it can access, at the whole expression.
    fn field(&mut self, field: InFile<&ast::FieldExpr>, ctx: Ctx) {
        let cx = self.cx;
        let Some(Either::Left(def)) = cx.sema.resolve_field(field.value) else { return };
        let Some(scope) = cx.sema.scope(field.value.syntax()) else { return };
        if !def.is_visible_from(cx.db, scope.module()) {
            return;
        }
        let range = field.value.syntax().text_range();
        let found = self.push(ctx, Def::Field(def), field.value.syntax(), field.file_id, range, range);
        found.expr = true;
        found.phase = Phase::Bodies;
    }

    /// `check_pat_tuple_struct`'s check of the field each subpattern is of
    /// (`enumerate_and_adjust` past a `..`), when their number fits.
    fn tuple_struct_pat(&mut self, pat: InFile<&ast::TupleStructPat>, ctx: Ctx) {
        let cx = self.cx;
        let Some(fields) = cx.sema.resolve_tuple_struct_pat_fields(pat.value) else { return };
        let subpats: Vec<ast::Pat> = pat.value.fields().collect();
        let rest = subpats.iter().position(|it| matches!(it, ast::Pat::RestPat(_)));
        let count = subpats.len() - usize::from(rest.is_some());
        if count > fields.len() || (count < fields.len() && rest.is_none()) {
            return;
        }
        for (index, subpat) in subpats.iter().enumerate() {
            let field = match rest {
                Some(rest) if index == rest => continue,
                Some(rest) if index > rest => index - 1 + (fields.len() - count),
                _ => index,
            };
            let range = subpat.syntax().text_range();
            let found = self.push(ctx, Def::Field(fields[field].0), subpat.syntax(), pat.file_id, range, range);
            found.phase = Phase::Bodies;
        }
    }

    /// `lower_assoc_item_constraint`'s check of the associated type a constraint names, at
    /// the whole constraint.
    fn constraint(&mut self, arg: InFile<&ast::AssocTypeArg>, ctx: Ctx) {
        let cx = self.cx;
        let db = cx.db;
        let Some(name) = arg.value.name_ref() else { return };
        let Some(path) = arg.value.syntax().ancestors().skip(1).find_map(ast::Path::cast) else { return };
        let Some(PathResolution::Def(ModuleDef::Trait(trait_))) = cx.sema.resolve_path(&path) else { return };
        let found = trait_.items_with_supertraits(db).into_iter().find_map(|it| match it {
            hir::AssocItem::TypeAlias(it) if it.name(db).as_str() == name.text().trim_start_matches("r#") => Some(it),
            _ => None,
        });
        let Some(alias) = found else { return };
        let range = arg.value.syntax().text_range();
        let def = Def::Item(ModuleDef::TypeAlias(alias));
        let found = self.push(ctx, def, arg.value.syntax(), arg.file_id, range, range);
        if ctx.body {
            found.phase = Phase::Bodies;
        }
    }
}

/// `def`, an item of a trait impl taken as its trait's.
fn trait_item_def(cx: &Cx<'_, '_>, def: Def) -> Def {
    match def {
        Def::Item(it) => Def::Item(trait_item(cx.db, it)),
        it => it,
    }
}

/// Whether rustc resolves `path` in typeck rather than in name resolution: its qualifier is
/// a type (`Type::item`, `<Type>::item`, `Self::item`, `T::Assoc`, an enum's associated item),
/// not a module, a trait or `<Type as Trait>`.
fn type_relative(cx: &Cx<'_, '_>, path: &ast::Path, res: &PathResolution<'_>) -> bool {
    let Some(qualifier) = path.qualifier() else { return false };
    match qualifier.segment().and_then(|it| it.kind()) {
        Some(ast::PathSegmentKind::Type { trait_ref, .. }) => trait_ref.is_none(),
        Some(ast::PathSegmentKind::SelfTypeKw) if qualifier.qualifier().is_none() => true,
        _ => match cx.sema.resolve_path(&qualifier) {
            Some(PathResolution::Def(ModuleDef::Module(_) | ModuleDef::Trait(_))) => false,
            Some(PathResolution::Def(ModuleDef::Adt(Adt::Enum(_)))) => {
                !matches!(res, PathResolution::Def(ModuleDef::EnumVariant(_)))
            }
            Some(PathResolution::Def(
                ModuleDef::Adt(_) | ModuleDef::TypeAlias(_) | ModuleDef::BuiltinType(_),
            ))
            | Some(PathResolution::SelfType(_) | PathResolution::TypeParam(_)) => true,
            _ => false,
        },
    }
}

/// Whether `path` is the trait of a `<Type as Trait>` qualifier: a parent segment of the
/// path it qualifies.
fn is_anchor_trait(path: &ast::Path) -> bool {
    let Some(ty) = path.syntax().parent().and_then(ast::PathType::cast) else { return false };
    let Some(segment) = ty.syntax().ancestors().skip(1).take(2).find_map(ast::PathSegment::cast) else {
        return false;
    };
    matches!(segment.kind(), Some(ast::PathSegmentKind::Type { trait_ref: Some(it), .. }) if it == ty)
}

/// A path as `pprust::path_to_string` prints it: its tokens with no space.
fn written(node: &SyntaxNode) -> String {
    node.descendants_with_tokens()
        .filter_map(|it| it.into_token())
        .filter(|it| !it.kind().is_trivia())
        .map(|it| it.text().to_owned())
        .collect()
}

/// rustc's `Target` of an item written in `container`.
fn item_target(item: &ast::Item, container: Container) -> Option<Target> {
    Some(match item {
        ast::Item::Const(_) => match container {
            Container::Free | Container::Foreign => Target::Const,
            _ => Target::AssocConst,
        },
        ast::Item::Enum(_) => Target::Enum,
        ast::Item::ExternBlock(_) => Target::ForeignMod,
        ast::Item::ExternCrate(_) => Target::ExternCrate,
        ast::Item::Fn(it) => match container {
            Container::Free => Target::Fn,
            Container::Foreign => Target::ForeignFn,
            Container::Impl { of_trait: false } => Target::Method(MethodKind::Inherent),
            Container::Impl { of_trait: true } => Target::Method(MethodKind::TraitImpl),
            Container::Trait => Target::Method(MethodKind::Trait { body: it.body().is_some() }),
        },
        ast::Item::Impl(it) => Target::Impl { of_trait: it.trait_().is_some() },
        ast::Item::MacroCall(_) => Target::MacroCall,
        ast::Item::MacroRules(_) | ast::Item::MacroDef(_) => Target::MacroDef,
        ast::Item::Module(_) => Target::Mod,
        ast::Item::Static(_) => match container {
            Container::Foreign => Target::ForeignStatic,
            _ => Target::Static,
        },
        ast::Item::Struct(_) => Target::Struct,
        ast::Item::Trait(_) => Target::Trait,
        ast::Item::TypeAlias(_) => match container {
            Container::Free => Target::TyAlias,
            Container::Foreign => Target::ForeignTy,
            _ => Target::AssocTy,
        },
        ast::Item::Union(_) => Target::Union,
        ast::Item::Use(_) => Target::Use,
        _ => return None,
    })
}

/// rustc's `Target` of a node that is no item.
fn other_target(node: &SyntaxNode) -> Option<Target> {
    let kind = node.kind();
    Some(if ast::Variant::can_cast(kind) {
        Target::Variant
    } else if ast::RecordField::can_cast(kind) || ast::TupleField::can_cast(kind) {
        Target::Field
    } else if ast::ClosureExpr::can_cast(kind) {
        Target::Closure
    } else if ast::MatchArm::can_cast(kind) {
        Target::Arm
    } else if ast::Param::can_cast(kind) || ast::SelfParam::can_cast(kind) {
        Target::Param
    } else if ast::TypeParam::can_cast(kind) {
        Target::GenericParam(GenericParamKind::Type)
    } else if ast::LifetimeParam::can_cast(kind) {
        Target::GenericParam(GenericParamKind::Lifetime)
    } else if ast::ConstParam::can_cast(kind) {
        Target::GenericParam(GenericParamKind::Const)
    } else if ast::RecordPatField::can_cast(kind) {
        Target::PatField
    } else if ast::RecordExprField::can_cast(kind) {
        Target::ExprField
    } else if ast::WherePred::can_cast(kind) {
        Target::WherePredicate
    } else if ast::LetStmt::can_cast(kind) || ast::ExprStmt::can_cast(kind) {
        Target::Statement
    } else if ast::Expr::can_cast(kind) {
        Target::Expression
    } else {
        return None;
    })
}
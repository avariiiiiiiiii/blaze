//! The items of a crate as rustc's HIR has them, in the order its passes visit them: module
//! by module (`crate::checks::modules`), each module's items in source order with the items of
//! their bodies, impls, traits and `extern` blocks after each, what macro calls expand to in
//! place of the calls, and none that `cfg` turns off. Each item has the nodes that are its own:
//! its syntax and what macros in it expand to, without the items in it.
//!
//! Follows `compiler/rustc_lint/src/late.rs` (`late_lint_mod_inner`, `visit_item`,
//! `visit_nested_item`: each module on its own, items in bodies walked with the body)
//! (MIT or Apache-2.0).

use std::ops::ControlFlow;

use blaze_ids::radix;
use cfg::CfgExpr;
use hir::{HirFileId, InFile, ModuleSource};
use rustc_hash::FxHashMap;
use smallvec::SmallVec;
use syntax::{
    AstNode, SyntaxKind, SyntaxNode, SyntaxNodePtr, TextRange,
    ast::{self, HasAttrs, HasModuleItem},
};

use crate::cx::Cx;

/// What an item is an item of.
#[derive(Clone, Debug)]
pub(super) enum Container {
    /// A module's or a block's own item.
    Module,
    Trait(InFile<ast::Trait>),
    Impl(InFile<ast::Impl>),
    Extern(InFile<ast::ExternBlock>),
}

/// An item, with where it is and the nodes that are its own.
#[derive(Clone, Debug)]
pub(super) struct Item {
    pub(super) node: InFile<ast::Item>,
    pub(super) container: Container,
    /// The module it is declared in, not counting blocks.
    pub(super) module: hir::Module,
    /// The index of the item whose body it is written in, if it is in one.
    pub(super) in_body: Option<usize>,
    /// One past the index of the last item in it (its associated, foreign and body items and
    /// theirs): those after it up to `end` are what it holds.
    pub(super) end: usize,
    /// Its own nodes, in source order (preorder), itself first; in what the macros it calls
    /// expand to, too; without the items in it and what is in them.
    pub(super) nodes: Vec<InFile<SyntaxNode>>,
}

impl Item {
    /// The node rustc emits an item's lints at.
    pub(super) fn ptr(&self) -> InFile<SyntaxNodePtr> {
        self.node.as_ref().map(|it| SyntaxNodePtr::new(it.syntax()))
    }

    pub(super) fn syntax(&self) -> InFile<SyntaxNode> {
        self.node.as_ref().map(|it| it.syntax().clone())
    }

    /// Its own nodes that are `N`, each kind tested before it is cast.
    pub(super) fn own<N: AstNode>(&self) -> impl Iterator<Item = InFile<N>> + '_ {
        self.nodes
            .iter()
            .filter(|it| N::can_cast(it.value.kind()))
            .filter_map(|it| N::cast(it.value.clone()).map(|n| it.with_value(n)))
    }

    /// The items it holds, among `items` (all of the walk's, it at `index`).
    pub(super) fn held<'i>(&self, items: &'i [Item], index: usize) -> &'i [Item] {
        &items[index + 1..self.end]
    }
}

/// A key that sorts as the AST has things: the offsets of the declarations of a module and the
/// modules it is in from the crate's root, then an offset in it. Inline up to three levels.
pub(super) type Order = SmallVec<[u32; 4]>;

/// Each module's place in the crate as the AST has it (the offsets of the declarations of it
/// and the modules it is in, from the crate's root), made once per module: one FxHash lookup
/// per key after that.
#[derive(Default)]
pub(super) struct Orders {
    of: FxHashMap<hir::Module, Order>,
}

impl Orders {
    /// The key that sorts what is at `lo` in `module` as the AST has it, each module's items in
    /// place of its `mod`: rustc's order of `DefIndex`es and of the early lint pass.
    pub(super) fn key(&mut self, cx: &Cx<'_, '_>, module: hir::Module, lo: u32) -> Order {
        let mut key = self.of.entry(module).or_insert_with(|| module_order(cx, module)).clone();
        key.push(lo);
        key
    }
}

fn module_order(cx: &Cx<'_, '_>, module: hir::Module) -> Order {
    let mut order = Order::new();
    let mut at = Some(module);
    while let Some(module) = at {
        if let Some(range) = module.declaration_source_range(cx.db) {
            order.push(cx.span(range).lo);
        }
        at = module.parent(cx.db);
    }
    order.reverse();
    order
}

/// The fields of a struct, a union or an enum's variants, in the order they are written.
pub(super) fn fields(item: &Item) -> Vec<InFile<SyntaxNode>> {
    let mut fields: Vec<InFile<SyntaxNode>> = item
        .own::<ast::RecordField>()
        .map(|it| it.map(|it| it.syntax().clone()))
        .chain(item.own::<ast::TupleField>().map(|it| it.map(|it| it.syntax().clone())))
        .collect();
    radix::sort_by_key(&mut fields, |it| {
        radix::pack_32(u32::from(it.file_id != item.node.file_id), it.value.text_range().start().into())
    });
    fields
}

/// The items of `cx.krate` in the order rustc visits them.
pub(super) fn items(cx: &Cx<'_, '_>) -> Vec<Item> {
    let mut walk = Walk { cx, items: Vec::new() };
    for module in crate::checks::modules(cx) {
        let source = module.definition_source(cx.db);
        let file = source.file_id;
        let root = cx.sema.parse_or_expand(file);
        let list: Vec<ast::Item> = match &source.value {
            ModuleSource::SourceFile(it) => {
                let ptr = SyntaxNodePtr::new(it.syntax());
                ast::SourceFile::cast(ptr.to_node(&root)).map(|it| it.items().collect()).unwrap_or_default()
            }
            ModuleSource::Module(it) => {
                let ptr = SyntaxNodePtr::new(it.syntax());
                ast::Module::cast(ptr.to_node(&root))
                    .and_then(|it| it.item_list())
                    .map(|it| it.items().collect())
                    .unwrap_or_default()
            }
            ModuleSource::BlockExpr(_) => continue,
        };
        for item in list {
            walk.item(InFile::new(file, item), &Container::Module, module, None);
        }
    }
    walk.items
}

struct Walk<'c, 'a, 'db> {
    cx: &'c Cx<'a, 'db>,
    items: Vec<Item>,
}

impl Walk<'_, '_, '_> {
    /// Visits `item`, written in `container`, and what is in it.
    fn item(&mut self, item: InFile<ast::Item>, container: &Container, module: hir::Module, in_body: Option<usize>) {
        if !enabled(self.cx, &item.value) {
            return;
        }
        if let ast::Item::MacroCall(call) = &item.value {
            if let Some(expansion) = self.expand(item.with_value(call)) {
                self.expansion(expansion, container, module, in_body);
            }
            return;
        }
        if self.cx.sema.is_attr_macro_call(item.as_ref()) {
            if let Some(expansion) = self.cx.sema.expand_attr_macro(&item.value) {
                self.expansion(expansion.value, container, module, in_body);
            }
            return;
        }
        let index = self.items.len();
        self.items.push(Item {
            node: item.clone(),
            container: container.clone(),
            module,
            in_body,
            end: index + 1,
            nodes: vec![item.as_ref().map(|it| it.syntax().clone())],
        });
        let file = item.file_id;
        let inner = match &item.value {
            // Another module's items, visited with it.
            ast::Item::Module(_) | ast::Item::MacroRules(_) | ast::Item::MacroDef(_) => return,
            ast::Item::Impl(it) => Some(Container::Impl(item.with_value(it.clone()))),
            ast::Item::Trait(it) => Some(Container::Trait(item.with_value(it.clone()))),
            ast::Item::ExternBlock(it) => Some(Container::Extern(item.with_value(it.clone()))),
            _ => None,
        };
        for child in item.value.syntax().children() {
            let list = matches!(child.kind(), SyntaxKind::ASSOC_ITEM_LIST | SyntaxKind::EXTERN_ITEM_LIST);
            match &inner {
                Some(container) if list => {
                    self.items[index].nodes.push(InFile::new(file, child.clone()));
                    for it in child.children() {
                        self.node(file, &it, index, Some(container), module);
                    }
                }
                _ => self.node(file, &child, index, None, module),
            }
        }
        self.items[index].end = self.items.len();
    }

    /// What a macro call or an attribute macro of an item expands to, in place of it.
    fn expansion(&mut self, root: InFile<SyntaxNode>, container: &Container, module: hir::Module, in_body: Option<usize>) {
        for child in root.value.children() {
            match ast::Item::cast(child.clone()) {
                Some(item) if is_item(&child) => self.item(root.with_value(item), container, module, in_body),
                _ => {}
            }
        }
    }

    /// `node`, under the item `owner`: one of its own, or an item in it, which is of
    /// `direct` when it is one of the items of an impl, a trait or an `extern` block `owner`
    /// is, else of `owner`'s body.
    fn node(&mut self, file: HirFileId, node: &SyntaxNode, owner: usize, direct: Option<&Container>, module: hir::Module) {
        let kind = node.kind();
        if ast::AnyHasAttrs::can_cast(kind)
            && let Some(it) = ast::AnyHasAttrs::cast(node.clone())
            && !enabled(self.cx, &it)
        {
            return;
        }
        if is_item(node) {
            let item = ast::Item::cast(node.clone()).expect("an item");
            match direct {
                Some(container) => self.item(InFile::new(file, item), container, module, self.items[owner].in_body),
                None => self.item(InFile::new(file, item), &Container::Module, module, Some(owner)),
            }
            return;
        }
        if kind == SyntaxKind::MACRO_CALL
            && let Some(call) = ast::MacroCall::cast(node.clone())
        {
            self.items[owner].nodes.push(InFile::new(file, node.clone()));
            let Some(expansion) = self.expand(InFile::new(file, &call)) else { return };
            match expansion.value.kind() {
                SyntaxKind::MACRO_ITEMS | SyntaxKind::MACRO_STMTS => {
                    for child in expansion.value.children() {
                        self.node(expansion.file_id, &child, owner, direct, module);
                    }
                }
                _ => self.node(expansion.file_id, &expansion.value, owner, None, module),
            }
            return;
        }
        self.items[owner].nodes.push(InFile::new(file, node.clone()));
        for child in node.children() {
            self.node(file, &child, owner, None, module);
        }
    }

    fn expand(&self, call: InFile<&ast::MacroCall>) -> Option<InFile<SyntaxNode>> {
        let id = self.cx.sema.to_def2(call)?;
        let file: HirFileId = id.into();
        Some(InFile::new(file, self.cx.sema.parse_or_expand(file)))
    }
}

/// Whether `node` is an item of HIR's: an item, an associated item or an item of an `extern`
/// block, not a macro call (which is what it expands to) nor an `asm!` in a body.
fn is_item(node: &SyntaxNode) -> bool {
    match node.kind() {
        SyntaxKind::MACRO_CALL => false,
        SyntaxKind::ASM_EXPR => matches!(
            node.parent().map(|it| it.kind()),
            Some(SyntaxKind::SOURCE_FILE | SyntaxKind::ITEM_LIST | SyntaxKind::MACRO_ITEMS)
        ),
        _ => ast::Item::can_cast(node.kind()),
    }
}

/// Whether the `cfg`s of `owner` keep it, its `cfg_attr`s expanded.
pub(super) fn enabled<N: HasAttrs>(cx: &Cx<'_, '_>, owner: &N) -> bool {
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

/// The attributes named `name` on `owner`, its `cfg_attr`s expanded: each with the span rustc
/// gives it (the whole `#[..]`, or what is in a `cfg_attr`).
pub(super) fn attrs_named<N: HasAttrs>(cx: &Cx<'_, '_>, owner: &N, name: &str) -> Vec<(ast::Meta, TextRange)> {
    let mut found = Vec::new();
    if owner.attrs().next().is_none() {
        return found;
    }
    let cfg = cx.krate.cfg(cx.db);
    hir_expand::attrs::expand_cfg_attr::<()>(
        owner.attrs(),
        || cfg,
        |meta, attr| {
            if meta.simple_name().is_some_and(|it| it == name) {
                let range = match meta.syntax().parent().is_some_and(|it| &it == attr.syntax()) {
                    true => attr.syntax().text_range(),
                    false => meta.syntax().text_range(),
                };
                found.push((meta, range));
            }
            ControlFlow::Continue(())
        },
    );
    found
}

/// The first attribute named `name` on `owner`, as [`attrs_named`] gives it.
pub(super) fn attr<N: HasAttrs>(cx: &Cx<'_, '_>, owner: &N, name: &str) -> Option<(ast::Meta, TextRange)> {
    attrs_named(cx, owner, name).into_iter().next()
}

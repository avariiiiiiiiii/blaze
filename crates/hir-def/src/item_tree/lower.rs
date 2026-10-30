//! AST -> `ItemTree` lowering code.

use std::{
    cell::{Cell, OnceCell},
    mem,
};

use base_db::{Crate, FxIndexSet, SourceDatabase};
use cfg::{CfgExpr, CfgOptions};
use hir_expand::{
    HirFileId,
    attrs::Attr,
    mod_path::PathKind,
    name::{AsName, Name},
    span_map::SpanMap,
};
use la_arena::Arena;
use span::{
    AstIdMap, FileAstId, NO_DOWNMAP_ERASED_FILE_AST_ID_MARKER, Span, SpanAnchor, SyntaxContext,
};
use syntax::{
    AstNode, SourceFile, TextRange,
    ast::{self, HasModuleItem, HasName, StructKind},
};

use crate::item_tree::{
    AliasId, AttrOwner, Const, Enum, ExternBlock, ExternCrate, Function, IdRange, Impl,
    ImportAlias, ItemKind, ItemTree, Macro2, MacroCall, MacroRules, Mod, ModItemId,
    ModPath, NameId, PathId, RawVisibility, RawVisibilityId, Static, Struct, Trait, TypeAlias,
    Union, Use, UseTree, UseTreeId, VisibilityExplicitness,
    attrs::lower_attrs_into,
};

/// The index of the next element of an array `len` long, as a tree's `u32` ids count. The top
/// few `u32`s are the ids' sentinels ([`NameId::NONE`], [`ModItemId::TOP_OWNER`], ...).
fn id(len: usize) -> u32 {
    match u32::try_from(len) {
        Ok(id) if id < u32::MAX - 4 => id,
        _ => panic!("an item tree with {len} elements in one array: more than `u32` ids name"),
    }
}

fn push<T>(column: &mut Vec<T>, value: T) -> u32 {
    let at = id(column.len());
    column.push(value);
    at
}

/// Every column moved out into an exact-size one, `column` left empty with its capacity: one
/// allocation per non-empty column, none for an empty one.
fn column<T>(column: &mut Vec<T>) -> Box<[T]> {
    column.drain(..).collect()
}

thread_local! {
    /// The worker's builder, empty between trees and keeping its columns' capacity, so a
    /// worker grows no column again once it has lowered a tree as big. A tree lowered while
    /// another is (none is today) takes a fresh one.
    static SCRATCH: Cell<TreeBuilder> = Cell::new(TreeBuilder::default());
}

/// The most items a kept [`SCRATCH`] builder has room for: about 100 KiB of columns.
const SCRATCH_ITEMS: usize = 1 << 13;

/// An [`ItemTree`] being built: its columns, growing.
#[derive(Default)]
struct TreeBuilder {
    /// The items of the lists being lowered, innermost last: a list is lowered onto the end
    /// and moved to `children` when it is done ([`TreeBuilder::items_since`]); what is left at
    /// the end is the top level.
    pending_items: Vec<ModItemId>,
    /// The same for the lists of `use` trees.
    pending_use_trees: Vec<UseTreeId>,
    /// The visibilities a tuple struct's constructor is the least of, while its fields are read.
    ctor_scratch: Vec<RawVisibility>,

    kinds: Vec<ItemKind>,
    data: Vec<u32>,
    ast_ids: Vec<FileAstId<ast::Item>>,
    children: Vec<ModItemId>,

    consts: Vec<Const>,
    enums: Vec<Enum>,
    extern_blocks: Vec<ExternBlock>,
    extern_crates: Vec<ExternCrate>,
    functions: Vec<Function>,
    impls: Vec<Impl>,
    macro2s: Vec<Macro2>,
    macro_calls: Vec<MacroCall>,
    macro_rules: Vec<MacroRules>,
    mods: Vec<Mod>,
    statics: Vec<Static>,
    structs: Vec<Struct>,
    traits: Vec<Trait>,
    type_aliases: Vec<TypeAlias>,
    unions: Vec<Union>,
    uses: Vec<Use>,

    use_trees: Vec<UseTree>,
    use_tree_lists: Vec<UseTreeId>,
    ctor_vis: Vec<RawVisibilityId>,

    names: FxIndexSet<Name>,
    paths: FxIndexSet<ModPath>,
    vis: FxIndexSet<RawVisibility>,

    attr_owners: Vec<AttrOwner>,
    attrs: Vec<Attr>,
    cfgs: Vec<CfgExpr>,
}

impl TreeBuilder {
    /// The tree built, the pending items its top level; leaves the builder empty, its capacity
    /// kept.
    fn finish(&mut self) -> ItemTree {
        debug_assert!(self.pending_use_trees.is_empty() && self.ctor_scratch.is_empty());
        self.attr_owners.sort_unstable_by_key(|owner| owner.owner);
        ItemTree {
            top_level: column(&mut self.pending_items),
            kinds: column(&mut self.kinds),
            data: column(&mut self.data),
            ast_ids: column(&mut self.ast_ids),
            children: column(&mut self.children),
            consts: column(&mut self.consts),
            enums: column(&mut self.enums),
            extern_blocks: column(&mut self.extern_blocks),
            extern_crates: column(&mut self.extern_crates),
            functions: column(&mut self.functions),
            impls: column(&mut self.impls),
            macro2s: column(&mut self.macro2s),
            macro_calls: column(&mut self.macro_calls),
            macro_rules: column(&mut self.macro_rules),
            mods: column(&mut self.mods),
            statics: column(&mut self.statics),
            structs: column(&mut self.structs),
            traits: column(&mut self.traits),
            type_aliases: column(&mut self.type_aliases),
            unions: column(&mut self.unions),
            uses: column(&mut self.uses),
            use_trees: column(&mut self.use_trees),
            use_tree_lists: column(&mut self.use_tree_lists),
            ctor_vis: column(&mut self.ctor_vis),
            names: self.names.drain(..).collect(),
            paths: self.paths.drain(..).collect(),
            vis: self.vis.drain(..).collect(),
            attr_owners: column(&mut self.attr_owners),
            attrs: column(&mut self.attrs),
            cfgs: column(&mut self.cfgs),
        }
    }

    /// A new item of `kind`, its payload at `data` of its column.
    fn item(&mut self, kind: ItemKind, ast_id: FileAstId<ast::Item>, data: u32) -> ModItemId {
        let index = id(self.kinds.len());
        self.kinds.push(kind);
        self.data.push(data);
        self.ast_ids.push(ast_id);
        ModItemId::new(index)
    }

    fn name(&mut self, name: Name) -> NameId {
        NameId { raw: id(self.names.insert_full(name).0) }
    }

    fn path(&mut self, path: ModPath) -> PathId {
        PathId { raw: id(self.paths.insert_full(path).0) }
    }

    fn alias(&mut self, alias: Option<ImportAlias>) -> AliasId {
        match alias {
            None => AliasId::NONE,
            Some(ImportAlias::Underscore) => AliasId::UNDERSCORE,
            Some(ImportAlias::Alias(name)) => AliasId { raw: self.name(name).raw },
        }
    }

    fn visibility(&mut self, vis: RawVisibility) -> RawVisibilityId {
        match &vis {
            RawVisibility::Public => RawVisibilityId::PUB,
            RawVisibility::PubSelf(VisibilityExplicitness::Explicit) => {
                RawVisibilityId::PRIV_EXPLICIT
            }
            RawVisibility::PubSelf(VisibilityExplicitness::Implicit) => {
                RawVisibilityId::PRIV_IMPLICIT
            }
            RawVisibility::PubCrate => RawVisibilityId::PUB_CRATE,
            _ => RawVisibilityId { raw: id(self.vis.insert_full(vis).0) },
        }
    }

    /// The list of the items pending from `mark`, moved to `children`.
    fn items_since(&mut self, mark: usize) -> IdRange {
        let start = id(self.children.len());
        let len = id(self.pending_items.len() - mark);
        self.children.extend(self.pending_items.drain(mark..));
        IdRange { start, len }
    }

    fn use_tree(&mut self, tree: UseTree) -> UseTreeId {
        UseTreeId { raw: push(&mut self.use_trees, tree) }
    }

    /// The list of the `use` trees pending from `mark`, moved to `use_tree_lists`.
    fn use_tree_list_since(&mut self, mark: usize) -> IdRange {
        let start = id(self.use_tree_lists.len());
        let len = id(self.pending_use_trees.len() - mark);
        self.use_tree_lists.extend(self.pending_use_trees.drain(mark..));
        IdRange { start, len }
    }

    fn ctor_vis(&mut self, vis: impl IntoIterator<Item = RawVisibility>) -> IdRange {
        let start = self.ctor_vis.len();
        for vis in vis {
            let vis = self.visibility(vis);
            self.ctor_vis.push(vis);
        }
        IdRange { start: id(start), len: id(self.ctor_vis.len() - start) }
    }

    /// Gives `owner` the attributes from `start` of `attrs`, disabled by `cfg` if one is
    /// given. An owner with no attributes and no `cfg` is left out.
    fn add_attrs(&mut self, owner: ModItemId, start: usize, cfg: Option<CfgExpr>) {
        let len = self.attrs.len() - start;
        if len == 0 && cfg.is_none() {
            return;
        }
        let cfg = match cfg {
            Some(cfg) => push(&mut self.cfgs, cfg),
            None => AttrOwner::ENABLED,
        };
        let attrs = IdRange { start: id(start), len: id(len) };
        self.attr_owners.push(AttrOwner { owner, attrs, cfg });
    }
}

pub(super) struct Ctx<'db> {
    pub(super) db: &'db dyn SourceDatabase,
    tree: TreeBuilder,
    source_ast_id_map: &'db AstIdMap,
    span_map: OnceCell<SpanMap<'db>>,
    file: HirFileId,
    cfg_options: OnceCell<&'db CfgOptions>,
    krate: Crate,
}

impl<'db> Ctx<'db> {
    pub(super) fn new(db: &'db dyn SourceDatabase, file: HirFileId, krate: Crate) -> Self {
        Self {
            db,
            tree: SCRATCH.take(),
            source_ast_id_map: file.ast_id_map(db),
            file,
            cfg_options: OnceCell::new(),
            span_map: OnceCell::new(),
            krate,
        }
    }

    pub(super) fn span_map(&self) -> SpanMap<'db> {
        *self.span_map.get_or_init(|| self.file.span_map(self.db))
    }

    /// Lowers `node`'s attributes into the tree's, and returns where they start and the `cfg`
    /// that disables `node`, if one does.
    fn lower_attrs_of(&mut self, node: &dyn ast::HasAttrs) -> (usize, Option<CfgExpr>) {
        let span_map = self.span_map();
        let (db, krate, cfg_options) = (self.db, self.krate, &self.cfg_options);
        let start = self.tree.attrs.len();
        let cfg = lower_attrs_into(
            db,
            node,
            &|| *cfg_options.get_or_init(|| krate.cfg_options(db)),
            span_map,
            &mut self.tree.attrs,
        );
        (start, cfg)
    }

    fn lower_attrs(&mut self, owner: ModItemId, node: &dyn ast::HasAttrs) {
        let (start, cfg) = self.lower_attrs_of(node);
        self.tree.add_attrs(owner, start, cfg);
    }

    /// Whether a `cfg` among `node`'s attributes is false; the attributes are not kept.
    fn cfg_disabled(&mut self, node: &dyn ast::HasAttrs) -> bool {
        let (start, cfg) = self.lower_attrs_of(node);
        self.tree.attrs.truncate(start);
        cfg.is_some()
    }

    /// Lowers the source file's inner attributes as the top level's: after `extra`, the crate
    /// attributes given on the command line (of the crate whose root `file` is), if any. A
    /// false `cfg` among the extra attributes stops there: the file's are not lowered.
    pub(super) fn lower_top_attrs(
        &mut self,
        file: &SourceFile,
        extra: Option<(SourceFile, span::EditionedFileId)>,
    ) {
        let start = self.tree.attrs.len();
        let mut cfg = None;
        if let Some((crate_attrs, file_id)) = extra {
            let (db, krate, cfg_options) = (self.db, self.krate, &self.cfg_options);
            cfg = lower_attrs_into(
                db,
                &crate_attrs,
                &|| *cfg_options.get_or_init(|| krate.cfg_options(db)),
                FakeSpanMap { file_id },
                &mut self.tree.attrs,
            );
        }
        if cfg.is_none() {
            cfg = self.lower_attrs_of(file).1;
        }
        self.tree.add_attrs(ModItemId::TOP_OWNER, start, cfg);
    }

    /// The tree, its top level the items pending; the builder goes back to [`SCRATCH`] unless
    /// it grew past [`SCRATCH_ITEMS`] (a generated file), so a worker keeps a bounded amount.
    fn finish(mut self) -> ItemTree {
        let tree = self.tree.finish();
        if self.tree.kinds.capacity() <= SCRATCH_ITEMS {
            SCRATCH.set(mem::take(&mut self.tree));
        }
        tree
    }

    /// Lowers `item` onto the pending list being lowered, if it is an item.
    fn pend(&mut self, item: &ast::Item) {
        if let Some(id) = self.lower_mod_item(item) {
            self.tree.pending_items.push(id);
        }
    }

    pub(super) fn lower_module_items(mut self, item_owner: &dyn HasModuleItem) -> ItemTree {
        for item in item_owner.items() {
            self.pend(&item);
        }
        self.finish()
    }

    pub(super) fn lower_macro_stmts(mut self, stmts: ast::MacroStmts) -> ItemTree {
        for stmt in stmts.statements() {
            let item = match stmt {
                ast::Stmt::Item(item) => item,
                // Macro calls can be both items and expressions. The syntax library always treats
                // them as expressions here, so we undo that.
                ast::Stmt::ExprStmt(es) => match es.expr() {
                    Some(ast::Expr::MacroExpr(expr)) => {
                        cov_mark::hit!(macro_call_in_macro_stmts_is_added_to_item_tree);
                        let Some(call) = expr.macro_call() else { continue };
                        call.into()
                    }
                    _ => continue,
                },
                _ => continue,
            };
            self.pend(&item);
        }

        if let Some(ast::Expr::MacroExpr(tail_macro)) = stmts.expr()
            && let Some(call) = tail_macro.macro_call()
        {
            cov_mark::hit!(macro_stmt_with_trailing_macro_expr);
            self.pend(&call.into());
        }

        self.finish()
    }

    pub(super) fn lower_block(mut self, block: &ast::BlockExpr) -> ItemTree {
        self.lower_attrs(ModItemId::TOP_OWNER, block);
        for stmt in block.statements() {
            let id = match stmt {
                ast::Stmt::Item(item) => self.lower_mod_item(&item),
                // Macro calls can be both items and expressions. The syntax library always treats
                // them as expressions here, so we undo that.
                ast::Stmt::ExprStmt(es) => match es.expr() {
                    Some(ast::Expr::MacroExpr(expr)) => self.lower_macro_stmt(&expr),
                    _ => None,
                },
                _ => None,
            };
            if let Some(id) = id {
                self.tree.pending_items.push(id);
            }
        }
        if let Some(ast::Expr::MacroExpr(expr)) = block.tail_expr()
            && let Some(mod_item) = self.lower_macro_stmt(&expr)
        {
            self.tree.pending_items.push(mod_item);
        }
        self.finish()
    }

    /// A macro call written as a statement is an item of the block when it expands to items.
    /// Its attributes are on the expression: one that is configured out is no item at all.
    fn lower_macro_stmt(&mut self, expr: &ast::MacroExpr) -> Option<ModItemId> {
        let call = expr.macro_call()?;
        if self.cfg_disabled(&ast::Expr::MacroExpr(expr.clone())) {
            return None;
        }
        self.lower_mod_item(&call.into())
    }

    fn lower_mod_item(&mut self, item: &ast::Item) -> Option<ModItemId> {
        let mod_item = match item {
            ast::Item::Struct(ast) => self.lower_struct(ast)?,
            ast::Item::Union(ast) => self.lower_union(ast)?,
            ast::Item::Enum(ast) => self.lower_enum(ast)?,
            ast::Item::Fn(ast) => self.lower_function(ast)?,
            ast::Item::TypeAlias(ast) => self.lower_type_alias(ast)?,
            ast::Item::Static(ast) => self.lower_static(ast)?,
            ast::Item::Const(ast) => self.lower_const(ast),
            ast::Item::Module(ast) => self.lower_module(ast)?,
            ast::Item::Trait(ast) => self.lower_trait(ast)?,
            ast::Item::Impl(ast) => self.lower_impl(ast),
            ast::Item::Use(ast) => self.lower_use(ast)?,
            ast::Item::ExternCrate(ast) => self.lower_extern_crate(ast)?,
            ast::Item::MacroCall(ast) => self.lower_macro_call(ast)?,
            ast::Item::MacroRules(ast) => self.lower_macro_rules(ast)?,
            ast::Item::MacroDef(ast) => self.lower_macro_def(ast)?,
            ast::Item::ExternBlock(ast) => self.lower_extern_block(ast),
            // FIXME: Handle `global_asm!()`.
            ast::Item::AsmExpr(_) => return None,
        };
        self.lower_attrs(mod_item, item);

        Some(mod_item)
    }

    fn lower_struct(&mut self, strukt: &ast::Struct) -> Option<ModItemId> {
        let visibility = self.lower_visibility_from_ast(strukt);
        let visibility_id = self.tree.visibility(visibility.clone());
        let name = strukt.name()?.as_name();
        let ast_id = self.source_ast_id_map.ast_id(strukt);
        let value_ns_ctor = match strukt.kind() {
            StructKind::Record(_) => IdRange::EMPTY,
            StructKind::Unit => self.tree.ctor_vis([visibility]),
            StructKind::Tuple(fields) => self.tuple_struct_ctor_vis(visibility, fields),
        };
        let res = Struct { name: self.tree.name(name), visibility: visibility_id, value_ns_ctor };
        let data = push(&mut self.tree.structs, res);
        Some(self.tree.item(ItemKind::Struct, ast_id.upcast(), data))
    }

    /// The visibilities a tuple struct's constructor is the least of, in `ctor_vis`.
    fn tuple_struct_ctor_vis(
        &mut self,
        struct_vis: RawVisibility,
        fields: ast::TupleFieldList,
    ) -> IdRange {
        if let RawVisibility::PubSelf(_) = struct_vis {
            // `pub(self)` <= anything.
            return self.tree.ctor_vis([struct_vis]);
        }

        // The least visibility so far: one, or several distinct `pub(in ..)`s (whose paths are
        // not ordered before name resolution), in the order of their fields.
        let mut least = mem::take(&mut self.tree.ctor_scratch);
        least.push(struct_vis);
        for field in fields.fields() {
            let field_vis = self.lower_visibility_from_ast(&field);
            match (&field_vis, &least[0]) {
                (RawVisibility::PubSelf(_), _) => {
                    // If there is a field private, the minimal visibility is private.
                    least.clear();
                    least.push(field_vis);
                    break;
                }
                (_, RawVisibility::Public) => {
                    // anything <= `pub`.
                    least[0] = field_vis;
                }
                (RawVisibility::Module(..), RawVisibility::PubCrate) => {
                    // `pub(in module)` <= `pub(crate)`.
                    least[0] = field_vis;
                }
                (
                    RawVisibility::Public | RawVisibility::PubCrate,
                    RawVisibility::Module(..) | RawVisibility::PubCrate,
                ) => {
                    // `pub(in module) | pub(crate)` <= `pub(crate) | pub`.
                }
                (_, RawVisibility::PubSelf(_)) => {
                    unreachable!("we early-exit on `PubSelf`");
                }
                (RawVisibility::Module(..), RawVisibility::Module(..)) => {
                    if !least.contains(&field_vis) {
                        least.push(field_vis);
                    }
                }
            }
        }

        // A list of one is that visibility.
        let range = self.tree.ctor_vis(least.drain(..));
        self.tree.ctor_scratch = least;
        range
    }

    fn lower_union(&mut self, union: &ast::Union) -> Option<ModItemId> {
        let visibility = self.lower_visibility(union);
        let name = union.name()?.as_name();
        let ast_id = self.source_ast_id_map.ast_id(union);
        let res = Union { name: self.tree.name(name), visibility };
        let data = push(&mut self.tree.unions, res);
        Some(self.tree.item(ItemKind::Union, ast_id.upcast(), data))
    }

    fn lower_enum(&mut self, enum_: &ast::Enum) -> Option<ModItemId> {
        let visibility = self.lower_visibility(enum_);
        let name = enum_.name()?.as_name();
        let ast_id = self.source_ast_id_map.ast_id(enum_);
        let res = Enum { name: self.tree.name(name), visibility };
        let data = push(&mut self.tree.enums, res);
        Some(self.tree.item(ItemKind::Enum, ast_id.upcast(), data))
    }

    fn lower_function(&mut self, func: &ast::Fn) -> Option<ModItemId> {
        let visibility = self.lower_visibility(func);
        let name = func.name()?.as_name();

        let ast_id = self.source_ast_id_map.ast_id(func);

        let res = Function { name: self.tree.name(name), visibility };
        let data = push(&mut self.tree.functions, res);
        Some(self.tree.item(ItemKind::Function, ast_id.upcast(), data))
    }

    fn lower_type_alias(&mut self, type_alias: &ast::TypeAlias) -> Option<ModItemId> {
        let name = type_alias.name()?.as_name();
        let visibility = self.lower_visibility(type_alias);
        let ast_id = self.source_ast_id_map.ast_id(type_alias);
        let res = TypeAlias { name: self.tree.name(name), visibility };
        let data = push(&mut self.tree.type_aliases, res);
        Some(self.tree.item(ItemKind::TypeAlias, ast_id.upcast(), data))
    }

    fn lower_static(&mut self, static_: &ast::Static) -> Option<ModItemId> {
        let name = static_.name()?.as_name();
        let visibility = self.lower_visibility(static_);
        let ast_id = self.source_ast_id_map.ast_id(static_);
        let res = Static { name: self.tree.name(name), visibility };
        let data = push(&mut self.tree.statics, res);
        Some(self.tree.item(ItemKind::Static, ast_id.upcast(), data))
    }

    fn lower_const(&mut self, konst: &ast::Const) -> ModItemId {
        let name = match konst.name() {
            Some(name) => self.tree.name(name.as_name()),
            None => NameId::NONE,
        };
        let visibility = self.lower_visibility(konst);
        let ast_id = self.source_ast_id_map.ast_id(konst);
        let data = push(&mut self.tree.consts, Const { name, visibility });
        self.tree.item(ItemKind::Const, ast_id.upcast(), data)
    }

    fn lower_module(&mut self, module: &ast::Module) -> Option<ModItemId> {
        let name = module.name()?.as_name();
        let visibility = self.lower_visibility(module);
        let items = if module.semicolon_token().is_some() {
            IdRange::NONE
        } else {
            let mark = self.tree.pending_items.len();
            match module.item_list() {
                Some(list) => {
                    for item in list.items() {
                        self.pend(&item);
                    }
                }
                None => {
                    cov_mark::hit!(name_res_works_for_broken_modules);
                }
            }
            self.tree.items_since(mark)
        };
        let ast_id = self.source_ast_id_map.ast_id(module);
        let res = Mod { name: self.tree.name(name), visibility, items };
        let data = push(&mut self.tree.mods, res);
        Some(self.tree.item(ItemKind::Mod, ast_id.upcast(), data))
    }

    fn lower_trait(&mut self, trait_def: &ast::Trait) -> Option<ModItemId> {
        let name = trait_def.name()?.as_name();
        let visibility = self.lower_visibility(trait_def);
        let ast_id = self.source_ast_id_map.ast_id(trait_def);

        let def = Trait { name: self.tree.name(name), visibility };
        let data = push(&mut self.tree.traits, def);
        Some(self.tree.item(ItemKind::Trait, ast_id.upcast(), data))
    }

    fn lower_impl(&mut self, impl_def: &ast::Impl) -> ModItemId {
        let ast_id = self.source_ast_id_map.ast_id(impl_def);
        // Note that trait impls don't get implicit `Self` unlike traits, because here they are a
        // type alias rather than a type parameter, so this is handled by the resolver.
        let res = Impl { is_trait_impl: impl_def.trait_().is_some() };
        let data = push(&mut self.tree.impls, res);
        self.tree.item(ItemKind::Impl, ast_id.upcast(), data)
    }

    fn lower_use(&mut self, use_item: &ast::Use) -> Option<ModItemId> {
        let visibility = self.lower_visibility(use_item);
        let ast_id = self.source_ast_id_map.ast_id(use_item);
        let span_map = self.span_map();
        let mut lowering = UseTreeLowering { db: self.db, mapping: None, tree: &mut self.tree };
        let use_tree = lowering.lower_use_tree(use_item.use_tree()?, &mut |range| {
            span_map.span_for_range(range).ctx
        })?;

        let data = push(&mut self.tree.uses, Use { visibility, use_tree });
        Some(self.tree.item(ItemKind::Use, ast_id.upcast(), data))
    }

    fn lower_extern_crate(&mut self, extern_crate: &ast::ExternCrate) -> Option<ModItemId> {
        let name = extern_crate.name_ref()?.as_name();
        let alias = extern_crate.rename().map(|a| {
            a.name().map(|it| it.as_name()).map_or(ImportAlias::Underscore, ImportAlias::Alias)
        });
        let visibility = self.lower_visibility(extern_crate);
        let ast_id = self.source_ast_id_map.ast_id(extern_crate);

        let res = ExternCrate {
            name: self.tree.name(name),
            alias: self.tree.alias(alias),
            visibility,
        };
        let data = push(&mut self.tree.extern_crates, res);
        Some(self.tree.item(ItemKind::ExternCrate, ast_id.upcast(), data))
    }

    fn lower_macro_call(&mut self, m: &ast::MacroCall) -> Option<ModItemId> {
        let span_map = self.span_map();
        let path = m.path()?;
        let range = path.syntax().text_range();
        let path =
            ModPath::from_src(self.db, path, &mut |range| span_map.span_for_range(range).ctx)?;
        let ast_id = self.source_ast_id_map.ast_id(m);
        let expand_to = hir_expand::ExpandTo::from_call_site(m);
        let res = MacroCall {
            path: self.tree.path(path),
            expand_to,
            ctxt: span_map.span_for_range(range).ctx,
        };
        let data = push(&mut self.tree.macro_calls, res);
        Some(self.tree.item(ItemKind::MacroCall, ast_id.upcast(), data))
    }

    fn lower_macro_rules(&mut self, m: &ast::MacroRules) -> Option<ModItemId> {
        let name = m.name()?;
        let ast_id = self.source_ast_id_map.ast_id(m);

        let res = MacroRules { name: self.tree.name(name.as_name()) };
        let data = push(&mut self.tree.macro_rules, res);
        Some(self.tree.item(ItemKind::MacroRules, ast_id.upcast(), data))
    }

    fn lower_macro_def(&mut self, m: &ast::MacroDef) -> Option<ModItemId> {
        let name = m.name()?;

        let ast_id = self.source_ast_id_map.ast_id(m);
        let visibility = self.lower_visibility(m);

        let res = Macro2 { name: self.tree.name(name.as_name()), visibility };
        let data = push(&mut self.tree.macro2s, res);
        Some(self.tree.item(ItemKind::Macro2, ast_id.upcast(), data))
    }

    fn lower_extern_block(&mut self, block: &ast::ExternBlock) -> ModItemId {
        let ast_id = self.source_ast_id_map.ast_id(block);
        let mark = self.tree.pending_items.len();
        for item in block.extern_item_list().into_iter().flat_map(|list| list.extern_items()) {
            // Note: All items in an `extern` block need to be lowered as if they're outside of one
            // (in other words, the knowledge that they're in an extern block must not be used).
            // This is because an extern block can contain macros whose ItemTree's top-level items
            // should be considered to be in an extern block too.
            let mod_item = match &item {
                ast::ExternItem::Fn(ast) => self.lower_function(ast),
                ast::ExternItem::Static(ast) => self.lower_static(ast),
                ast::ExternItem::TypeAlias(ty) => self.lower_type_alias(ty),
                ast::ExternItem::MacroCall(call) => self.lower_macro_call(call),
            };
            let Some(mod_item) = mod_item else { continue };
            self.lower_attrs(mod_item, &item);
            self.tree.pending_items.push(mod_item);
        }
        let children = self.tree.items_since(mark);

        let data = push(&mut self.tree.extern_blocks, ExternBlock { children });
        self.tree.item(ItemKind::ExternBlock, ast_id.upcast(), data)
    }

    fn lower_visibility(&mut self, item: &dyn ast::HasVisibility) -> RawVisibilityId {
        let vis = self.lower_visibility_from_ast(item);
        self.tree.visibility(vis)
    }

    fn lower_visibility_from_ast(&self, item: &dyn ast::HasVisibility) -> RawVisibility {
        visibility_from_ast(self.db, item.visibility(), &mut |range| {
            self.span_map().span_for_range(range).ctx
        })
    }
}

/// Spans for the crate attributes given on the command line, which no file holds.
#[derive(Copy, Clone)]
struct FakeSpanMap {
    file_id: span::EditionedFileId,
}

impl syntax_bridge::SpanMapper for FakeSpanMap {
    fn span_for(&self, range: TextRange) -> Span {
        Span {
            range,
            anchor: SpanAnchor {
                file_id: self.file_id,
                ast_id: NO_DOWNMAP_ERASED_FILE_AST_ID_MARKER,
            },
            ctx: SyntaxContext::root(self.file_id.edition()),
        }
    }
}

struct UseTreeLowering<'a> {
    db: &'a dyn SourceDatabase,
    /// The lowered trees' syntax, in the order [`Use::expand`] numbers them: a tree before its
    /// children. Only the source map ([`lower_use_tree`]) wants it.
    mapping: Option<&'a mut Arena<ast::UseTree>>,
    tree: &'a mut TreeBuilder,
}

impl UseTreeLowering<'_> {
    fn map(&mut self, tree: &ast::UseTree) {
        if let Some(mapping) = &mut self.mapping {
            mapping.alloc(tree.clone());
        }
    }

    /// Lowers `tree` into the tree's `use` trees, its children before it.
    fn lower_use_tree(
        &mut self,
        tree: ast::UseTree,
        span_for_range: &mut dyn FnMut(::tt::TextRange) -> SyntaxContext,
    ) -> Option<UseTreeId> {
        if let Some(use_tree_list) = tree.use_tree_list() {
            let prefix = match tree.path() {
                // E.g. use something::{{{inner}}};
                None => None,
                // E.g. `use something::{inner}` (prefix is `None`, path is `something`)
                // or `use something::{path::{inner::{innerer}}}` (prefix is `something::path`, path is `inner`)
                Some(path) => {
                    match ModPath::from_src(self.db, path, span_for_range) {
                        Some(it) => Some(it),
                        None => {
                            // FIXME: report errors somewhere
                            return None;
                        }
                    }
                }
            };

            self.map(&tree);
            let mark = self.tree.pending_use_trees.len();
            for tree in use_tree_list.use_trees() {
                if let Some(id) = self.lower_use_tree(tree, span_for_range) {
                    self.tree.pending_use_trees.push(id);
                }
            }

            let path = match prefix {
                Some(prefix) => self.tree.path(prefix),
                None => PathId::NONE,
            };
            let list = self.tree.use_tree_list_since(mark);
            Some(self.tree.use_tree(UseTree {
                kind: UseTree::PREFIXED,
                path,
                alias: AliasId::NONE,
                list,
            }))
        } else {
            let is_glob = tree.star_token().is_some();
            let path = match tree.path() {
                Some(path) => Some(ModPath::from_src(self.db, path, span_for_range)?),
                // `use ::*;` globs the crate root.
                None if is_glob && tree.coloncolon_token().is_some() => {
                    Some(ModPath::from_kind(PathKind::Abs))
                }
                None => None,
            };
            let alias = tree.rename().map(|a| {
                a.name().map(|it| it.as_name()).map_or(ImportAlias::Underscore, ImportAlias::Alias)
            });
            if alias.is_some() && is_glob {
                return None;
            }

            match (path, alias, is_glob) {
                (path, None, true) => {
                    if path.is_none() {
                        cov_mark::hit!(glob_enum_group);
                    }
                    self.map(&tree);
                    let path = match path {
                        Some(path) => self.tree.path(path),
                        None => PathId::NONE,
                    };
                    Some(self.tree.use_tree(UseTree {
                        kind: UseTree::GLOB,
                        path,
                        alias: AliasId::NONE,
                        list: IdRange::EMPTY,
                    }))
                }
                // Globs can't be renamed
                (_, Some(_), true) | (None, None, false) => None,
                // `bla::{ as Name}` is invalid
                (None, Some(_), false) => None,
                (Some(path), alias, false) => {
                    self.map(&tree);
                    let path = self.tree.path(path);
                    let alias = self.tree.alias(alias);
                    Some(self.tree.use_tree(UseTree {
                        kind: UseTree::SINGLE,
                        path,
                        alias,
                        list: IdRange::EMPTY,
                    }))
                }
            }
        }
    }
}

/// The syntax of `tree`'s `use` trees, in the order [`Use::expand`] numbers them.
pub(crate) fn lower_use_tree(
    db: &dyn SourceDatabase,
    tree: ast::UseTree,
    span_for_range: &mut dyn FnMut(::tt::TextRange) -> SyntaxContext,
) -> Option<Arena<ast::UseTree>> {
    let mut scratch = TreeBuilder::default();
    let mut mapping = Arena::new();
    let mut lowering = UseTreeLowering { db, mapping: Some(&mut mapping), tree: &mut scratch };
    lowering.lower_use_tree(tree, span_for_range)?;
    Some(mapping)
}

fn private_vis() -> RawVisibility {
    RawVisibility::PubSelf(VisibilityExplicitness::Implicit)
}

pub(crate) fn visibility_from_ast(
    db: &dyn SourceDatabase,
    node: Option<ast::Visibility>,
    span_for_range: &mut dyn FnMut(::tt::TextRange) -> SyntaxContext,
) -> RawVisibility {
    let Some(node) = node else { return private_vis() };
    let path = match node.kind() {
        ast::VisibilityKind::In(path) => {
            let path = ModPath::from_src(db, path, span_for_range);
            match path {
                None => return private_vis(),
                Some(path) => path,
            }
        }
        ast::VisibilityKind::PubCrate => return RawVisibility::PubCrate,
        ast::VisibilityKind::PubSuper => ModPath::from_kind(PathKind::Super(1)),
        ast::VisibilityKind::PubSelf => {
            return RawVisibility::PubSelf(VisibilityExplicitness::Explicit);
        }
        ast::VisibilityKind::Pub => return RawVisibility::Public,
    };
    RawVisibility::Module(path, VisibilityExplicitness::Explicit)
}

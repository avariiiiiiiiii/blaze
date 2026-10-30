//! The crate's definitions as the dead-code pass sees them: rustc's owners (items, associated
//! items, foreign items, the impls a builtin derive makes) with their variants and fields, the
//! free and foreign items of each module in the order `check_mod_deathness` visits them, each
//! definition's parent (`local_parent`), and the owners in `hir_crate_items` order.
//!
//! Follows `compiler/rustc_middle/src/hir/map.rs` (`ItemCollector`),
//! `compiler/rustc_middle/src/hir/mod.rs` (`hir_crate_items`, `hir_module_items`) and
//! `compiler/rustc_builtin_macros/src/deriving/` (where a derive puts its impl)
//! (MIT or Apache-2.0).

use std::ops::ControlFlow;

use blaze_ids::radix;
use cfg::CfgExpr;
use hir::{HirFileId, InFile, ModuleSource};
use hir_def::{
    AdtId, BuiltinDeriveImplId, ConstId, EnumVariantId, FieldId, FunctionId, HasModule, ImplId,
    StaticId, TraitId, TypeAliasId, builtin_derive::BuiltinDeriveImplTrait,
};
use rustc_hash::{FxHashMap, FxHashSet};
use syntax::{
    AstNode, SyntaxKind, SyntaxNode, SyntaxNodePtr,
    ast::{self, HasAttrs, HasModuleItem, HasName},
};

use crate::cx::Cx;

pub(super) type DefIdx = usize;

/// What a definition is, by the front end's ids. A `use` has none and is its node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Id {
    Fn(FunctionId),
    Const(ConstId),
    Static(StaticId),
    Adt(AdtId),
    Variant(EnumVariantId),
    Field(FieldId),
    Trait(TraitId),
    Alias(TypeAliasId),
    Impl(ImplId),
    /// The impl a builtin derive makes.
    Derive(BuiltinDeriveImplId),
    /// The method of that impl, for the traits that have one.
    DeriveMethod(BuiltinDeriveImplId),
    Use(InFile<SyntaxNodePtr>),
}

/// rustc's `DefKind`, as far as the pass tells them apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Fn,
    AssocFn { method: bool },
    Const,
    AssocConst,
    Static,
    Struct,
    Union,
    Enum,
    Variant,
    Field,
    Trait,
    TraitAlias,
    TyAlias,
    AssocTy,
    ForeignTy,
    Impl { of_trait: bool },
    Use,
}

impl Kind {
    /// rustc's `def_descr`.
    pub(super) fn descr(self) -> &'static str {
        match self {
            Kind::Fn => "function",
            Kind::AssocFn { method: true } => "method",
            Kind::AssocFn { method: false } => "associated function",
            Kind::Const => "constant",
            Kind::AssocConst => "associated constant",
            Kind::Static => "static",
            Kind::Struct => "struct",
            Kind::Union => "union",
            Kind::Enum => "enum",
            Kind::Variant => "variant",
            Kind::Field => "field",
            Kind::Trait => "trait",
            Kind::TraitAlias => "trait alias",
            Kind::TyAlias => "type alias",
            Kind::AssocTy => "associated type",
            Kind::ForeignTy => "foreign type",
            Kind::Impl { .. } => "implementation",
            Kind::Use => "import",
        }
    }
}

/// rustc's `local_parent` of a definition, as far as liveness asks: the module, a definition,
/// or a closure or anonymous constant (by its node), which is live once it is visited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Parent {
    Module,
    Def(DefIdx),
    Anon(InFile<SyntaxNodePtr>),
}

#[derive(Debug)]
pub(super) struct Def {
    pub(super) id: Id,
    pub(super) kind: Kind,
    /// Its node; none for what a builtin derive makes.
    pub(super) node: Option<InFile<SyntaxNodePtr>>,
    /// rustc's `opt_item_name`: none for an impl or a `use`.
    pub(super) name: Option<String>,
    pub(super) parent: Parent,
    /// A trait's or an impl's associated items, an enum's variants, a struct's, a union's or a
    /// variant's fields, in order.
    pub(super) children: Vec<DefIdx>,
    pub(super) foreign: bool,
}

/// A module's items, as `hir_module_items` has them.
#[derive(Debug, Default)]
pub(super) struct ModuleItems {
    pub(super) free: Vec<DefIdx>,
    pub(super) foreign: Vec<DefIdx>,
}

#[derive(Debug)]
pub(super) struct Defs {
    pub(super) defs: Vec<Def>,
    ids: FxHashMap<Id, DefIdx>,
    /// In `crate::checks::modules` order.
    pub(super) modules: Vec<ModuleItems>,
    pub(super) module_ids: Vec<hir::Module>,
    /// The index in `modules` of the module each definition is in.
    pub(super) module_of: Vec<usize>,
    /// `hir_crate_items(()).owners()`: free items, trait items, impl items, foreign items.
    pub(super) owners: Vec<DefIdx>,
    /// What is live is not known: a `global_asm!` names an item in a `sym` or `const` operand
    /// (the front end keeps no `global_asm!`), or a `pub macro` makes the items of its module
    /// reachable (`update_reachability_from_macro`, not ported).
    pub(super) unported: bool,
}

impl Defs {
    pub(super) fn of(cx: &Cx<'_, '_>) -> Defs {
        let mut walk = Walk {
            cx,
            defs: Vec::new(),
            ids: FxHashMap::default(),
            modules: Vec::new(),
            module_of: Vec::new(),
            trait_items: Vec::new(),
            impl_items: Vec::new(),
            free_owners: Vec::new(),
            foreign_owners: Vec::new(),
            derives: FxHashMap::default(),
            scopes: FxHashSet::default(),
            unported: false,
        };
        let module_ids = crate::checks::modules(cx);
        for &module in &module_ids {
            walk.modules.push(ModuleItems::default());
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
                walk.item(InFile::new(file, item), Container::Module, Parent::Module);
            }
        }
        let mut owners = walk.free_owners;
        owners.extend(walk.trait_items);
        owners.extend(walk.impl_items);
        owners.extend(walk.foreign_owners);
        Defs {
            defs: walk.defs,
            ids: walk.ids,
            modules: walk.modules,
            module_ids,
            module_of: walk.module_of,
            owners,
            unported: walk.unported,
        }
    }

    pub(super) fn get(&self, id: Id) -> Option<DefIdx> {
        self.ids.get(&id).copied()
    }

    pub(super) fn len(&self) -> usize {
        self.defs.len()
    }
}

impl std::ops::Index<DefIdx> for Defs {
    type Output = Def;
    fn index(&self, index: DefIdx) -> &Def {
        &self.defs[index]
    }
}

/// What an item is an item of.
#[derive(Clone, Copy)]
enum Container {
    Module,
    Trait(DefIdx),
    Impl(DefIdx),
    Extern,
}

struct Walk<'c, 'a, 'db> {
    cx: &'c Cx<'a, 'db>,
    defs: Vec<Def>,
    ids: FxHashMap<Id, DefIdx>,
    modules: Vec<ModuleItems>,
    module_of: Vec<usize>,
    free_owners: Vec<DefIdx>,
    trait_items: Vec<DefIdx>,
    impl_items: Vec<DefIdx>,
    foreign_owners: Vec<DefIdx>,
    /// The builtin derives of each ADT, of the scopes read so far.
    derives: FxHashMap<AdtId, Vec<BuiltinDeriveImplId>>,
    scopes: FxHashSet<hir_def::ModuleId>,
    unported: bool,
}

impl Walk<'_, '_, '_> {
    fn item(&mut self, item: InFile<ast::Item>, container: Container, parent: Parent) {
        if !enabled(self.cx, &item.value) {
            return;
        }
        if let ast::Item::MacroCall(call) = &item.value {
            if let Some(expansion) = self.expand(item.with_value(call)) {
                self.expansion(expansion, container, parent);
            }
            return;
        }
        if self.cx.sema.is_attr_macro_call(item.as_ref()) {
            if let Some(expansion) = self.cx.sema.expand_attr_macro(&item.value) {
                self.expansion(expansion.value, container, parent);
            }
            return;
        }
        if let ast::Item::MacroDef(it) = &item.value
            && it.visibility().is_some()
        {
            self.unported = true;
        }
        let file = item.file_id;
        let def = self.define(&item, container, parent);
        let (list_container, list_kind) = match (&item.value, def) {
            (ast::Item::Module(_) | ast::Item::MacroRules(_) | ast::Item::MacroDef(_), _)
            | (ast::Item::Trait(_) | ast::Item::Impl(_), None) => return,
            (ast::Item::Trait(_), Some(def)) => (Some(Container::Trait(def)), SyntaxKind::ASSOC_ITEM_LIST),
            (ast::Item::Impl(_), Some(def)) => (Some(Container::Impl(def)), SyntaxKind::ASSOC_ITEM_LIST),
            (ast::Item::ExternBlock(_), _) => (Some(Container::Extern), SyntaxKind::EXTERN_ITEM_LIST),
            _ => (None, SyntaxKind::ASSOC_ITEM_LIST),
        };
        let inner = match def {
            Some(def) => Parent::Def(def),
            None => parent,
        };
        for child in item.value.syntax().children() {
            match list_container {
                Some(container) if child.kind() == list_kind => {
                    let item_parent = match container {
                        Container::Extern => parent,
                        _ => inner,
                    };
                    for it in child.children() {
                        self.list_node(file, &it, container, item_parent);
                    }
                }
                _ => self.node(file, &child, inner),
            }
        }
    }

    /// An item of the associated item list of a trait or an impl, or of an `extern` block.
    fn list_node(&mut self, file: HirFileId, node: &SyntaxNode, container: Container, parent: Parent) {
        if let Some(it) = ast::AnyHasAttrs::cast(node.clone())
            && !enabled(self.cx, &it)
        {
            return;
        }
        if let Some(item) = ast::Item::cast(node.clone()) {
            self.item(InFile::new(file, item), container, parent);
        }
    }

    /// What a macro call or an attribute macro of an item expands to, in place of it.
    fn expansion(&mut self, root: InFile<SyntaxNode>, container: Container, parent: Parent) {
        for child in root.value.children() {
            if let Some(item) = ast::Item::cast(child.clone())
                && is_item(&child)
            {
                self.item(root.with_value(item), container, parent);
            }
        }
    }

    /// `node`, inside the item whose nested items have `parent`.
    fn node(&mut self, file: HirFileId, node: &SyntaxNode, parent: Parent) {
        if let Some(it) = ast::AnyHasAttrs::cast(node.clone())
            && !enabled(self.cx, &it)
        {
            return;
        }
        if is_item(node) {
            if let Some(item) = ast::Item::cast(node.clone()) {
                self.item(InFile::new(file, item), Container::Module, parent);
            } else if node.kind() == SyntaxKind::ASM_EXPR {
                self.global_asm(node);
            }
            return;
        }
        if let Some(call) = ast::MacroCall::cast(node.clone()) {
            let Some(expansion) = self.expand(InFile::new(file, &call)) else { return };
            match expansion.value.kind() {
                SyntaxKind::MACRO_ITEMS | SyntaxKind::MACRO_STMTS => {
                    for child in expansion.value.children() {
                        self.node(expansion.file_id, &child, parent);
                    }
                }
                _ => self.node(expansion.file_id, &expansion.value, parent),
            }
            return;
        }
        let parent = match is_anon(node) {
            true => Parent::Anon(InFile::new(file, SyntaxNodePtr::new(node))),
            false => parent,
        };
        for child in node.children() {
            self.node(file, &child, parent);
        }
    }

    fn global_asm(&mut self, node: &SyntaxNode) {
        let names = node.descendants().any(|it| matches!(it.kind(), SyntaxKind::ASM_SYM | SyntaxKind::ASM_CONST));
        self.unported |= names;
    }

    fn expand(&self, call: InFile<&ast::MacroCall>) -> Option<InFile<SyntaxNode>> {
        let id = self.cx.sema.to_def2(call)?;
        let file: HirFileId = id.into();
        Some(InFile::new(file, self.cx.sema.parse_or_expand(file)))
    }

    fn push(&mut self, def: Def) -> DefIdx {
        let index = self.defs.len();
        self.ids.insert(def.id, index);
        self.defs.push(def);
        self.module_of.push(self.modules.len() - 1);
        index
    }

    /// Records `item` as a definition with its variants, fields and derives.
    fn define(&mut self, item: &InFile<ast::Item>, container: Container, parent: Parent) -> Option<DefIdx> {
        let cx = self.cx;
        let db = cx.db;
        let node = Some(item.as_ref().map(|it| SyntaxNodePtr::new(it.syntax())));
        let assoc = matches!(container, Container::Trait(_) | Container::Impl(_));
        let foreign = matches!(container, Container::Extern);
        let parent = match container {
            Container::Trait(it) | Container::Impl(it) => Parent::Def(it),
            Container::Module | Container::Extern => parent,
        };
        let name = |it: &dyn HasName| it.name().map(|it| it.text().trim_start_matches("r#").to_owned());
        let def = |id, kind, name| Def { id, kind, node, name, parent, children: Vec::new(), foreign };
        let (def, adt) = match &item.value {
            ast::Item::Fn(it) => {
                let function = cx.sema.to_def2(item.with_value(it))?;
                let id = FunctionId::try_from(function).ok()?;
                let kind = match assoc {
                    true => Kind::AssocFn { method: function.has_self_param(db) },
                    false => Kind::Fn,
                };
                (def(Id::Fn(id), kind, name(it)), None)
            }
            ast::Item::Const(it) => {
                let id = ConstId::from(cx.sema.to_def2(item.with_value(it))?);
                let kind = if assoc { Kind::AssocConst } else { Kind::Const };
                (def(Id::Const(id), kind, Some(name(it).unwrap_or_else(|| "_".to_owned()))), None)
            }
            ast::Item::Static(it) => {
                let id = StaticId::from(cx.sema.to_def2(item.with_value(it))?);
                (def(Id::Static(id), Kind::Static, name(it)), None)
            }
            ast::Item::Struct(it) => {
                let id = AdtId::StructId(cx.sema.to_def2(item.with_value(it))?.into());
                (def(Id::Adt(id), Kind::Struct, name(it)), Some(id))
            }
            ast::Item::Union(it) => {
                let id = AdtId::UnionId(cx.sema.to_def2(item.with_value(it))?.into());
                (def(Id::Adt(id), Kind::Union, name(it)), Some(id))
            }
            ast::Item::Enum(it) => {
                let id = AdtId::EnumId(cx.sema.to_def2(item.with_value(it))?.into());
                (def(Id::Adt(id), Kind::Enum, name(it)), Some(id))
            }
            ast::Item::Trait(it) => {
                let id = TraitId::from(cx.sema.to_def2(item.with_value(it))?);
                let kind = if it.eq_token().is_some() { Kind::TraitAlias } else { Kind::Trait };
                (def(Id::Trait(id), kind, name(it)), None)
            }
            ast::Item::TypeAlias(it) => {
                let id = TypeAliasId::from(cx.sema.to_def2(item.with_value(it))?);
                let kind = match (assoc, foreign) {
                    (true, _) => Kind::AssocTy,
                    (_, true) => Kind::ForeignTy,
                    _ => Kind::TyAlias,
                };
                (def(Id::Alias(id), kind, name(it)), None)
            }
            ast::Item::Impl(it) => {
                let id = ImplId::try_from(cx.sema.to_def2(item.with_value(it))?).ok()?;
                (def(Id::Impl(id), Kind::Impl { of_trait: it.trait_().is_some() }, None), None)
            }
            ast::Item::Use(_) => {
                let ptr = item.as_ref().map(|it| SyntaxNodePtr::new(it.syntax()));
                (def(Id::Use(ptr), Kind::Use, None), None)
            }
            _ => return None,
        };
        let index = self.push(def);
        match container {
            Container::Trait(owner) | Container::Impl(owner) => {
                self.defs[owner].children.push(index);
                match container {
                    Container::Trait(_) => self.trait_items.push(index),
                    _ => self.impl_items.push(index),
                }
            }
            Container::Extern => {
                self.foreign_owners.push(index);
                self.current().foreign.push(index);
            }
            Container::Module => {
                self.free_owners.push(index);
                self.current().free.push(index);
            }
        }
        if let Some(adt) = adt {
            self.fields_and_variants(item, index);
            self.derived(adt, parent);
        }
        Some(index)
    }

    fn current(&mut self) -> &mut ModuleItems {
        self.modules.last_mut().expect("items are walked within a module")
    }

    fn fields_and_variants(&mut self, item: &InFile<ast::Item>, adt: DefIdx) {
        let fields = match &item.value {
            ast::Item::Struct(it) => it.field_list(),
            ast::Item::Union(it) => it.record_field_list().map(ast::FieldList::RecordFieldList),
            ast::Item::Enum(it) => {
                for variant in it.variant_list().into_iter().flat_map(|it| it.variants()) {
                    if !enabled(self.cx, &variant) {
                        continue;
                    }
                    let Some(id) = self.cx.sema.to_def2(item.with_value(&variant)) else { continue };
                    let index = self.push(Def {
                        id: Id::Variant(id.into()),
                        kind: Kind::Variant,
                        node: Some(item.with_value(SyntaxNodePtr::new(variant.syntax()))),
                        name: variant.name().map(|it| it.text().trim_start_matches("r#").to_owned()),
                        parent: Parent::Def(adt),
                        children: Vec::new(),
                        foreign: false,
                    });
                    self.defs[adt].children.push(index);
                    self.fields(item.file_id, variant.field_list(), index);
                }
                return;
            }
            _ => return,
        };
        self.fields(item.file_id, fields, adt);
    }

    fn fields(&mut self, file: HirFileId, list: Option<ast::FieldList>, owner: DefIdx) {
        let nodes: Vec<SyntaxNode> = match list {
            Some(ast::FieldList::RecordFieldList(it)) => it.fields().map(|it| it.syntax().clone()).collect(),
            Some(ast::FieldList::TupleFieldList(it)) => it.fields().map(|it| it.syntax().clone()).collect(),
            None => return,
        };
        let db = self.cx.db;
        for node in nodes {
            let field = if let Some(it) = ast::RecordField::cast(node.clone()) {
                if !enabled(self.cx, &it) {
                    continue;
                }
                self.cx.sema.to_def2(InFile::new(file, &it))
            } else if let Some(it) = ast::TupleField::cast(node.clone()) {
                if !enabled(self.cx, &it) {
                    continue;
                }
                self.cx.sema.to_def2(InFile::new(file, &it))
            } else {
                None
            };
            let Some(field) = field else { continue };
            let id = FieldId::from(field);
            let name = field.name(db).as_str().trim_start_matches("r#").to_owned();
            let index = self.push(Def {
                id: Id::Field(id),
                kind: Kind::Field,
                node: Some(InFile::new(file, SyntaxNodePtr::new(&node))),
                name: Some(name),
                parent: Parent::Def(owner),
                children: Vec::new(),
                foreign: false,
            });
            self.defs[owner].children.push(index);
        }
    }

    /// The impls the builtin derives of `adt` make, after it, and their methods.
    fn derived(&mut self, adt: AdtId, parent: Parent) {
        let db = self.cx.db;
        let module = adt.module(db);
        if self.scopes.insert(module) {
            let scope = module.def_map(db).module(db, module).scope(db);
            for derive in scope.builtin_derive_impls() {
                self.derives.entry(derive.loc(db).adt).or_default().push(derive);
            }
        }
        let Some(mut derives) = self.derives.get(&adt).cloned() else { return };
        radix::sort_by_key(&mut derives, |it| {
            let loc = it.loc(db);
            radix::pack_32(loc.derive_attr_id.item_tree_index(), loc.derive_index)
        });
        for derive in derives {
            let trait_ = derive.loc(db).trait_;
            let index = self.push(Def {
                id: Id::Derive(derive),
                kind: Kind::Impl { of_trait: true },
                node: None,
                name: None,
                parent,
                children: Vec::new(),
                foreign: false,
            });
            self.free_owners.push(index);
            self.current().free.push(index);
            if let Some(method) = trait_.all_methods().first() {
                let method = self.push(Def {
                    id: Id::DeriveMethod(derive),
                    kind: Kind::AssocFn { method: !matches!(trait_, BuiltinDeriveImplTrait::Default) },
                    node: None,
                    name: Some(method.name().as_str().to_owned()),
                    parent: Parent::Def(index),
                    children: Vec::new(),
                    foreign: false,
                });
                self.defs[index].children.push(method);
                self.impl_items.push(method);
            }
        }
    }
}

/// Whether `node` is an item of HIR's: an item, an associated item or an item of an `extern`
/// block, not a macro call (which is what it expands to); a `global_asm!` is one.
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

/// Whether `node` is a definition of its own in rustc's HIR that items in it have as their
/// parent: a closure, an `async` or `gen` block (a coroutine), a `const` block, or an
/// anonymous constant (an array's length, a const argument, a discriminant).
pub(super) fn is_anon(node: &SyntaxNode) -> bool {
    match node.kind() {
        SyntaxKind::CLOSURE_EXPR => true,
        SyntaxKind::BLOCK_EXPR => ast::BlockExpr::cast(node.clone()).is_some_and(|it| {
            it.const_token().is_some() || it.async_token().is_some() || it.gen_token().is_some()
        }),
        _ if !ast::Expr::can_cast(node.kind()) => false,
        _ => match node.parent() {
            Some(parent) => match parent.kind() {
                SyntaxKind::CONST_ARG | SyntaxKind::VARIANT => true,
                SyntaxKind::ARRAY_EXPR => {
                    let has_semi = parent.children_with_tokens().any(|it| it.kind() == SyntaxKind::SEMICOLON);
                    has_semi && parent.children().filter(|it| ast::Expr::can_cast(it.kind())).nth(1).as_ref() == Some(node)
                }
                _ => false,
            },
            None => false,
        },
    }
}

/// The node of `node` or of its nearest ancestor that [`is_anon`].
pub(super) fn anon_of(node: &InFile<SyntaxNode>) -> Option<InFile<SyntaxNodePtr>> {
    let found = node.value.ancestors().find(is_anon)?;
    Some(node.with_value(SyntaxNodePtr::new(&found)))
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

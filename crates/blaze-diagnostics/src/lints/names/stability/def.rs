//! What a use names, as rustc's stability checks see it: an item, a field or a constructor;
//! its kind as rustc names it (`def_descr`), its path as rustc prints it with no trimming
//! (`with_no_trimmed_paths!(def_path_str(..))`), where it is written and the fact of it.
//!
//! Follows `compiler/rustc_middle/src/ty/mod.rs` (`def_descr`), `compiler/rustc_hir/src/def.rs`
//! (`DefKind::descr`), `compiler/rustc_middle/src/ty/print/pretty.rs` (`def_path_str`,
//! `try_print_visible_def_path`, `path_crate`) and `compiler/rustc_middle/src/ty/print/mod.rs`
//! (`default_print_def_path`, `default_print_impl_path`) (MIT or Apache-2.0).

use blaze_items::ItemId;
use hir::{
    Adt, AsAssocItem as _, AssocItem, AssocItemContainer, HasSource as _, HasVisibility as _, InFile, ModuleDef,
    StructKind, Variant,
};
use syntax::{AstNode, SyntaxNode, ast, ast::HasName as _};

use crate::cx::Cx;

/// What a use names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Def {
    Item(ModuleDef),
    Field(hir::Field),
    /// The constructor of a tuple or unit struct or variant, which a path in the value
    /// namespace names.
    Ctor(Variant),
}

impl Def {
    /// `variant` as the item it is: the struct, union or enum variant.
    pub(super) fn variant(variant: Variant) -> Def {
        Def::Item(match variant {
            Variant::Struct(it) => ModuleDef::Adt(Adt::Struct(it)),
            Variant::Union(it) => ModuleDef::Adt(Adt::Union(it)),
            Variant::EnumVariant(it) => ModuleDef::EnumVariant(it),
        })
    }

    /// What a path in the value namespace resolving to `def` names: the constructor of a
    /// tuple or unit struct or variant, else `def`.
    pub(super) fn value(db: &dyn hir::db::HirDatabase, def: ModuleDef) -> Def {
        let variant = match def {
            ModuleDef::Adt(Adt::Struct(it)) => Variant::Struct(it),
            ModuleDef::EnumVariant(it) => Variant::EnumVariant(it),
            _ => return Def::Item(def),
        };
        match kind(db, variant) {
            StructKind::Record => Def::Item(def),
            StructKind::Tuple | StructKind::Unit => Def::Ctor(variant),
        }
    }

    /// The item `self` is or is of.
    pub(super) fn item(self, db: &dyn hir::db::HirDatabase) -> ModuleDef {
        match self {
            Def::Item(it) => it,
            Def::Field(it) => Def::variant(it.parent_def(db)).item(db),
            Def::Ctor(it) => Def::variant(it).item(db),
        }
    }

    pub(super) fn krate(self, db: &dyn hir::db::HirDatabase) -> Option<hir::Crate> {
        match self.item(db) {
            ModuleDef::Module(it) => Some(it.krate(db)),
            it => Some(it.module(db)?.krate(db)),
        }
    }

    /// rustc's `def_descr`.
    pub(super) fn descr(self, db: &dyn hir::db::HirDatabase) -> &'static str {
        match self {
            Def::Field(_) => "field",
            Def::Ctor(variant) => match (variant, kind(db, variant)) {
                (Variant::EnumVariant(_), StructKind::Tuple) => "tuple variant",
                (Variant::EnumVariant(_), _) => "unit variant",
                (_, StructKind::Tuple) => "tuple struct",
                _ => "unit struct",
            },
            Def::Item(item) => match item {
                ModuleDef::Module(it) if it.is_crate_root(db) => "crate",
                ModuleDef::Module(_) => "module",
                ModuleDef::Function(it) => match it.as_assoc_item(db) {
                    Some(_) if it.has_self_param(db) => "method",
                    Some(_) => "associated function",
                    None => "function",
                },
                ModuleDef::Adt(Adt::Struct(_)) => "struct",
                ModuleDef::Adt(Adt::Union(_)) => "union",
                ModuleDef::Adt(Adt::Enum(_)) => "enum",
                ModuleDef::EnumVariant(_) => "variant",
                ModuleDef::Const(it) => match it.as_assoc_item(db) {
                    Some(_) => "associated constant",
                    None => "constant",
                },
                ModuleDef::Static(_) => "static",
                ModuleDef::Trait(_) => "trait",
                ModuleDef::TypeAlias(it) => match it.as_assoc_item(db) {
                    Some(_) => "associated type",
                    None => "type alias",
                },
                ModuleDef::BuiltinType(_) => "builtin type",
                ModuleDef::Macro(it) => match it.kind(db) {
                    hir::MacroKind::Derive | hir::MacroKind::DeriveBuiltIn => "derive macro",
                    hir::MacroKind::Attr | hir::MacroKind::AttrBuiltIn => "attribute macro",
                    _ => "macro",
                },
            },
        }
    }

    /// Whether rustc checks the stability of `self` across crates: it is public
    /// (`skip_stability_check_due_to_privacy`).
    pub(super) fn is_public(self, db: &dyn hir::db::HirDatabase) -> bool {
        let visibility = match self {
            Def::Field(it) => it.visibility(db),
            Def::Ctor(it) => match it {
                Variant::Struct(it) => it.visibility(db),
                Variant::Union(it) => it.visibility(db),
                Variant::EnumVariant(it) => it.visibility(db),
            },
            Def::Item(item) => match item {
                ModuleDef::Module(it) => it.visibility(db),
                ModuleDef::Function(it) => it.visibility(db),
                ModuleDef::Adt(it) => it.visibility(db),
                ModuleDef::EnumVariant(it) => it.visibility(db),
                ModuleDef::Const(it) => it.visibility(db),
                ModuleDef::Static(it) => it.visibility(db),
                ModuleDef::Trait(it) => it.visibility(db),
                ModuleDef::TypeAlias(it) => it.visibility(db),
                ModuleDef::Macro(it) => it.visibility(db),
                ModuleDef::BuiltinType(_) => return true,
            },
        };
        visibility == hir::Visibility::Public
    }

    /// The item whose signature holds what `self` is declared with, for a fact.
    pub(super) fn item_id(self, db: &dyn hir::db::HirDatabase) -> Option<ItemId> {
        Some(match self.item(db) {
            ModuleDef::Function(it) => ItemId::Function(hir_def::FunctionId::try_from(it).ok()?),
            ModuleDef::Adt(it) => ItemId::Adt(it.into()),
            ModuleDef::EnumVariant(it) => ItemId::Adt(Adt::Enum(it.parent_enum(db)).into()),
            ModuleDef::Const(it) => ItemId::Const(it.into()),
            ModuleDef::Static(it) => ItemId::Static(it.into()),
            ModuleDef::Trait(it) => ItemId::Trait(it.into()),
            ModuleDef::TypeAlias(it) => ItemId::Alias(it.into()),
            ModuleDef::Macro(it) => ItemId::Macro(it.into()),
            ModuleDef::Module(_) | ModuleDef::BuiltinType(_) => return None,
        })
    }

    /// The node `self` is written as, whose attributes are its own.
    pub(super) fn source(self, cx: &Cx<'_, '_>) -> Option<InFile<SyntaxNode>> {
        let db = cx.db;
        fn node<N: AstNode>(it: Option<InFile<N>>) -> Option<InFile<SyntaxNode>> {
            it.map(|it| it.map(|it| it.syntax().clone()))
        }
        match self {
            Def::Field(it) => it.source(db).map(|it| {
                it.map(|it| match it {
                    hir::FieldSource::Named(it) => it.syntax().clone(),
                    hir::FieldSource::Pos(it) => it.syntax().clone(),
                })
            }),
            Def::Ctor(it) => Def::variant(it).source(cx),
            Def::Item(item) => match item {
                ModuleDef::Module(it) => match it.declaration_source(db) {
                    Some(decl) => node(Some(decl)),
                    None => Some(it.definition_source(db).map(|it| it.node())),
                },
                ModuleDef::Function(it) => node(it.source(db)),
                ModuleDef::Adt(Adt::Struct(it)) => node(it.source(db)),
                ModuleDef::Adt(Adt::Union(it)) => node(it.source(db)),
                ModuleDef::Adt(Adt::Enum(it)) => node(it.source(db)),
                ModuleDef::EnumVariant(it) => node(it.source(db)),
                ModuleDef::Const(it) => node(it.source(db)),
                ModuleDef::Static(it) => node(it.source(db)),
                ModuleDef::Trait(it) => node(it.source(db)),
                ModuleDef::TypeAlias(it) => node(it.source(db)),
                ModuleDef::Macro(it) => it.source(db).map(|it| {
                    it.map(|it| match it {
                        either::Either::Left(it) => it.syntax().clone(),
                        either::Either::Right(it) => it.syntax().clone(),
                    })
                }),
                ModuleDef::BuiltinType(_) => None,
            },
        }
    }
}

/// The shape of `variant`'s fields.
pub(super) fn kind(db: &dyn hir::db::HirDatabase, variant: Variant) -> StructKind {
    match variant {
        Variant::Struct(it) => it.kind(db),
        Variant::Union(it) => it.kind(db),
        Variant::EnumVariant(it) => it.kind(db),
    }
}

/// The item `item` is, if it is the item of a trait impl: the trait's item of its name, whose
/// stability and deprecation are the ones that count (`AssocContainer::TraitImpl`'s
/// `trait_item_def_id`).
pub(super) fn trait_item(db: &dyn hir::db::HirDatabase, item: ModuleDef) -> ModuleDef {
    let assoc = match item {
        ModuleDef::Function(it) => it.as_assoc_item(db),
        ModuleDef::Const(it) => it.as_assoc_item(db),
        ModuleDef::TypeAlias(it) => it.as_assoc_item(db),
        _ => None,
    };
    let Some(assoc) = assoc else { return item };
    let AssocItemContainer::Impl(imp) = assoc.container(db) else { return item };
    let Some(trait_) = imp.trait_(db) else { return item };
    let Some(name) = assoc.name(db) else { return item };
    let found = trait_.items(db).into_iter().find(|it| {
        it.name(db).as_ref() == Some(&name)
            && matches!(
                (it, assoc),
                (AssocItem::Function(_), AssocItem::Function(_))
                    | (AssocItem::Const(_), AssocItem::Const(_))
                    | (AssocItem::TypeAlias(_), AssocItem::TypeAlias(_))
            )
    });
    match found {
        Some(AssocItem::Function(it)) => ModuleDef::Function(it),
        Some(AssocItem::Const(it)) => ModuleDef::Const(it),
        Some(AssocItem::TypeAlias(it)) => ModuleDef::TypeAlias(it),
        None => item,
    }
}

/// rustc's `with_no_trimmed_paths!(def_path_str(def))`: an item of this crate by its path
/// from the crate root, with no prefix; another crate's by the path it is visible through
/// from here; an associated item by what it is of, an inherent impl by its type when the
/// impl is next to the type and as `<impl T>` in its parent else; a field or variant by
/// what it is of; a constructor as its struct or variant.
pub(super) fn path(cx: &Cx<'_, '_>, def: Def) -> Option<String> {
    let db = cx.db;
    let name = |it: hir::Name| it.display(db, cx.edition).to_string();
    match def {
        Def::Ctor(it) => path(cx, Def::variant(it)),
        Def::Field(it) => {
            Some(format!("{}::{}", path(cx, Def::variant(it.parent_def(db)))?, name(it.name(db))))
        }
        Def::Item(ModuleDef::EnumVariant(it)) => {
            let parent = path(cx, Def::Item(ModuleDef::Adt(Adt::Enum(it.parent_enum(db)))))?;
            Some(format!("{parent}::{}", name(it.name(db))))
        }
        Def::Item(item) => {
            let assoc = match item {
                ModuleDef::Function(it) => it.as_assoc_item(db),
                ModuleDef::Const(it) => it.as_assoc_item(db),
                ModuleDef::TypeAlias(it) => it.as_assoc_item(db),
                _ => None,
            };
            if let Some(assoc) = assoc {
                let own = name(assoc.name(db)?);
                let value = !matches!(assoc, AssocItem::TypeAlias(_));
                let parent = match assoc.container(db) {
                    AssocItemContainer::Trait(it) => path(cx, Def::Item(ModuleDef::Trait(it)))?,
                    AssocItemContainer::Impl(it) => impl_path(cx, it, value)?,
                };
                return Some(format!("{parent}::{own}"));
            }
            if def.krate(db)? == cx.krate {
                return node_path(cx, def.source(cx)?);
            }
            Some(visible_path(cx, item))
        }
    }
}

/// The path an item of another crate is visible through from this crate's root
/// (`try_print_visible_def_path`): through the name a dependency is known by here, else
/// the item's own path in its crate.
fn visible_path(cx: &Cx<'_, '_>, item: ModuleDef) -> String {
    let db = cx.db;
    let cfg = hir::FindPathConfig { prefer_no_std: false, prefer_prelude: false, prefer_absolute: false, allow_unstable: true };
    let root = cx.krate.root_module(db);
    let deps = cx.krate.dependencies(db);
    if let ModuleDef::Module(it) = item
        && it.is_crate_root(db)
        && let Some(dep) = deps.iter().find(|dep| dep.krate == it.krate(db))
    {
        return dep.name.display(db, cx.edition).to_string();
    }
    if let Some(found) = root.find_use_path(db, hir::ItemInNs::from(item), hir::PrefixKind::Plain, cfg)
        && found.kind == hir::PathKind::Plain
        && found.segments().first().is_some_and(|first| deps.iter().any(|dep| &dep.name == first))
    {
        return found.display(db, cx.edition).to_string();
    }
    let krate = item.module(db).map(|it| it.krate(db));
    let crate_name = krate.and_then(|it| it.display_name(db)).map(|it| it.crate_name().to_string()).unwrap_or_default();
    match item {
        ModuleDef::Module(it) if it.is_crate_root(db) => crate_name,
        _ => match item.canonical_path(db, cx.edition) {
            Some(path) => format!("{crate_name}::{path}"),
            None => crate_name,
        },
    }
}

/// The path of an impl's items' parent: the impl's type when it is an ADT next to the impl
/// (`Type::<Args>` in a value path), else `parent::<impl Type>`.
fn impl_path(cx: &Cx<'_, '_>, imp: hir::Impl, value: bool) -> Option<String> {
    let db = cx.db;
    let self_ty = imp.self_ty(db);
    let source = imp.source(db);
    if let Some(adt) = self_ty.as_adt()
        && adt.module(db) == imp.module(db)
    {
        let mut printed = path(cx, Def::Item(ModuleDef::Adt(adt)))?;
        let args = source.as_ref().and_then(|it| match it.value.self_ty()? {
            ast::Type::PathType(it) => it.path()?.segment()?.generic_arg_list(),
            _ => None,
        });
        if let Some(args) = args {
            let args = args.generic_args().map(|it| it.syntax().text().to_string()).collect::<Vec<_>>().join(", ");
            printed.push_str(if value { "::<" } else { "<" });
            printed.push_str(&args);
            printed.push('>');
        }
        return Some(printed);
    }
    let ty = match self_ty.as_adt() {
        Some(adt) => path(cx, Def::Item(ModuleDef::Adt(adt)))?,
        None => cx.ty(&self_ty),
    };
    let parent = match source.filter(|_| imp.module(db).krate(db) == cx.krate) {
        Some(source) => match def_parent(cx, source.map(|it| it.syntax().clone())) {
            Some(parent) => Some(node_path(cx, parent)?),
            None => None,
        },
        None => {
            let module = imp.module(db);
            (!module.is_crate_root(db) || module.krate(db) != cx.krate)
                .then(|| visible_path(cx, ModuleDef::Module(module)))
        }
    };
    Some(match parent {
        Some(parent) => format!("{parent}::<impl {ty}>"),
        None => format!("<impl {ty}>"),
    })
}

/// The path of the definition of this crate written at `node`, from the crate root.
fn node_path(cx: &Cx<'_, '_>, node: InFile<SyntaxNode>) -> Option<String> {
    let own = match ast::Impl::cast(node.value.clone()) {
        Some(it) => {
            let imp = cx.sema.to_def2(node.with_value(&it))?;
            return impl_path(cx, imp, false);
        }
        None => segment(cx, &node)?,
    };
    match def_parent(cx, node) {
        Some(parent) => Some(format!("{}::{own}", node_path(cx, parent)?)),
        None => Some(own),
    }
}

/// The segment the definition at `node` adds to its parent's path.
fn segment(cx: &Cx<'_, '_>, node: &InFile<SyntaxNode>) -> Option<String> {
    if ast::ClosureExpr::can_cast(node.value.kind()) {
        let parent = def_parent(cx, node.clone());
        let index = closure_index(cx, parent.as_ref(), node);
        return Some(format!("{{closure#{index}}}"));
    }
    if let Some(it) = ast::Const::cast(node.value.clone())
        && it.name().is_none()
    {
        return Some("_".to_owned());
    }
    let named = ast::AnyHasName::cast(node.value.clone())?;
    Some(named.name()?.text().to_string())
}

/// Which of the closures of `parent` (the crate root when `None`) `closure` is, in the order
/// rustc numbers them: as they are written.
fn closure_index(cx: &Cx<'_, '_>, parent: Option<&InFile<SyntaxNode>>, closure: &InFile<SyntaxNode>) -> usize {
    let Some(parent) = parent.filter(|it| it.file_id == closure.file_id) else { return 0 };
    let mut index = 0;
    for node in parent.value.descendants() {
        if node == closure.value {
            return index;
        }
        if ast::ClosureExpr::can_cast(node.kind())
            && def_parent(cx, closure.with_value(node.clone())).is_some_and(|it| it.value == parent.value)
        {
            index += 1;
        }
    }
    index
}

/// Whether a definition at `node` is the parent of the definitions written in it.
pub(super) fn is_def_parent(node: &SyntaxNode) -> bool {
    use syntax::SyntaxKind::*;
    matches!(
        node.kind(),
        MODULE | FN | CONST | STATIC | STRUCT | ENUM | UNION | VARIANT | TRAIT | IMPL | TYPE_ALIAS | CLOSURE_EXPR
    )
}

/// The definition `node`'s is in (`opt_local_parent`): the nearest enclosing one, through a
/// macro's expansion to where it was called and from a module's file to its `mod`; `None`
/// at the crate root.
pub(super) fn def_parent(cx: &Cx<'_, '_>, node: InFile<SyntaxNode>) -> Option<InFile<SyntaxNode>> {
    parent(cx, node, is_def_parent)
}

/// [`def_parent`], or the `extern` block in between: what deprecation is inherited from.
pub(super) fn attr_parent(cx: &Cx<'_, '_>, node: InFile<SyntaxNode>) -> Option<InFile<SyntaxNode>> {
    parent(cx, node, |it| is_def_parent(it) || it.kind() == syntax::SyntaxKind::EXTERN_BLOCK)
}

fn parent(
    cx: &Cx<'_, '_>,
    node: InFile<SyntaxNode>,
    stop: impl Fn(&SyntaxNode) -> bool,
) -> Option<InFile<SyntaxNode>> {
    let db = cx.db;
    let (mut file, mut at) = (node.file_id, node.value.parent());
    loop {
        while let Some(node) = at {
            if stop(&node) {
                return Some(InFile::new(file, node));
            }
            at = node.parent();
        }
        match file.macro_file() {
            Some(call) => {
                let call = call.loc(db).to_node(db);
                (file, at) = (call.file_id, call.value.parent());
            }
            None => {
                let module = cx.sema.hir_file_to_module_def(file)?;
                let decl = module.declaration_source(db)?;
                return Some(decl.map(|it| it.syntax().clone()));
            }
        }
    }
}

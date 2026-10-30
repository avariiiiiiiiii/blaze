//! What rustc's resolver checks of the generic parameters of an item and of the `for<>`
//! binders in its signature: a name declared twice, the parameters of the impl or trait
//! around included (E0403); a lifetime parameter named `'static` (E0262); a default naming a
//! parameter declared after it, or itself (E0128).
//!
//! Follows rustc's `compiler/rustc_resolve/src/late.rs` (`with_generic_param_rib`,
//! `visit_generic_params`), `ident.rs` (`validate_res_from_ribs`) and its
//! `NameAlreadyUsedInParameterList`, `StaticLifetimeIsReserved` and
//! `ForwardDeclaredGenericParam` (MIT or Apache-2.0).

use blaze_items::{Fact, ItemId};
use hir::{HasSource as _, InFile};
use hir_def::ImplId;
use rustc_hash::{FxHashMap, FxHashSet};
use syntax::{
    AstNode, SyntaxNode, WalkEvent,
    ast::{self, HasGenericParams as _, HasName as _},
};

use super::item_id;
use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::{Diag, Span},
};

pub(super) fn check(cx: &Cx<'_, '_>, scopes: &[hir::Module]) -> Vec<Found> {
    let mut found = Vec::new();
    let none = Outer::default();
    for &module in scopes {
        for def in module.declarations(cx.db) {
            match def {
                hir::ModuleDef::Trait(it) => {
                    let Some(source) = it.source(cx.db) else { continue };
                    let item = item_id(cx.db, def);
                    let node = source.map(|it| it.syntax().clone());
                    let outer = check_item(cx, item, node.as_ref(), &none, &mut found);
                    for assoc in it.items(cx.db) {
                        check_assoc(cx, assoc, &outer, &mut found);
                    }
                }
                hir::ModuleDef::Function(it) => {
                    let Some(source) = it.source(cx.db) else { continue };
                    let node = source.map(|it| it.syntax().clone());
                    check_item(cx, item_id(cx.db, def), node.as_ref(), &none, &mut found);
                }
                hir::ModuleDef::Adt(it) => {
                    let Some(source) = it.source(cx.db) else { continue };
                    let node = source.map(|it| it.syntax().clone());
                    check_item(cx, item_id(cx.db, def), node.as_ref(), &none, &mut found);
                }
                hir::ModuleDef::TypeAlias(it) => {
                    let Some(source) = it.source(cx.db) else { continue };
                    let node = source.map(|it| it.syntax().clone());
                    check_item(cx, item_id(cx.db, def), node.as_ref(), &none, &mut found);
                }
                _ => {}
            }
        }
        for imp in module.impl_defs(cx.db) {
            let Some(source) = imp.source(cx.db) else { continue };
            let item = ImplId::try_from(imp).ok().map(ItemId::Impl);
            let node = source.map(|it| it.syntax().clone());
            let outer = check_item(cx, item, node.as_ref(), &none, &mut found);
            for assoc in imp.items(cx.db) {
                check_assoc(cx, assoc, &outer, &mut found);
            }
        }
    }
    found
}

fn check_assoc(cx: &Cx<'_, '_>, assoc: hir::AssocItem, outer: &Outer, found: &mut Vec<Found>) {
    let node = match assoc {
        hir::AssocItem::Function(it) => it.source(cx.db).map(|it| it.map(|it| it.syntax().clone())),
        hir::AssocItem::Const(it) => it.source(cx.db).map(|it| it.map(|it| it.syntax().clone())),
        hir::AssocItem::TypeAlias(it) => it.source(cx.db).map(|it| it.map(|it| it.syntax().clone())),
    };
    let Some(node) = node else { return };
    check_item(cx, item_id(cx.db, assoc.into()), node.as_ref(), outer, found);
}

/// The names in scope where a parameter list starts, up to the module or block around it:
/// rustc's `seen_bindings` of the type and value ribs, each with where it was first declared,
/// and its `seen_lifetimes`.
#[derive(Clone, Default)]
struct Outer {
    names: Vec<(String, Span)>,
    lifetimes: FxHashSet<String>,
}

/// Checks the parameters of `node` and of the binders in it, and returns the names they put
/// in scope of the items inside it.
fn check_item(
    cx: &Cx<'_, '_>,
    item: Option<ItemId>,
    node: InFile<&SyntaxNode>,
    outer: &Outer,
    found: &mut Vec<Found>,
) -> Outer {
    let fact = item.map(|it| Fact::Signature(cx.key(it)));
    let mut inner = outer.clone();
    if let Some(list) = ast::AnyHasGenericParams::cast(node.value.clone()).and_then(|it| it.generic_param_list()) {
        check_list(cx, node.with_value(&list), outer, fact.as_ref(), found);
        forward(cx, node.with_value(&list), fact.as_ref(), found);
        declare(cx, node.with_value(&list), &mut inner);
    }
    let mut walk = node.value.preorder();
    walk.next();
    while let Some(event) = walk.next() {
        let WalkEvent::Enter(child) = event else { continue };
        if ast::Item::can_cast(child.kind())
            || ast::AssocItemList::can_cast(child.kind())
            || ast::BlockExpr::can_cast(child.kind())
        {
            walk.skip_subtree();
            continue;
        }
        let Some(binder) = ast::ForBinder::cast(child) else { continue };
        let Some(list) = binder.generic_param_list() else { continue };
        check_list(cx, node.with_value(&list), &inner, fact.as_ref(), found);
    }
    inner
}

/// The type, const and lifetime parameters of `list` put in scope for what is inside.
fn declare(cx: &Cx<'_, '_>, list: InFile<&ast::GenericParamList>, outer: &mut Outer) {
    for param in list.value.generic_params() {
        match param {
            ast::GenericParam::LifetimeParam(it) => {
                if let Some(lifetime) = it.lifetime() {
                    outer.lifetimes.insert(lifetime.text().to_owned());
                }
            }
            ast::GenericParam::TypeParam(it) => declare_name(cx, list, it.name(), outer),
            ast::GenericParam::ConstParam(it) => declare_name(cx, list, it.name(), outer),
        }
    }
}

fn declare_name(cx: &Cx<'_, '_>, list: InFile<&ast::GenericParamList>, name: Option<ast::Name>, outer: &mut Outer) {
    let Some(name) = name else { return };
    let span = cx.span(list.with_value(name.syntax().text_range()));
    outer.names.push((name.text().to_owned(), span));
}

/// rustc's `with_generic_param_rib` over one parameter list: E0403 and E0262.
fn check_list(
    cx: &Cx<'_, '_>,
    list: InFile<&ast::GenericParamList>,
    outer: &Outer,
    fact: Option<&Fact>,
    found: &mut Vec<Found>,
) {
    // The outermost declaration of a name wins, as rustc extends its map rib by rib outwards.
    let mut seen: FxHashMap<String, Span> = FxHashMap::default();
    for (name, span) in outer.names.iter().rev() {
        seen.insert(name.clone(), span.clone());
    }
    for param in list.value.generic_params() {
        let (text, range, lifetime) = match &param {
            ast::GenericParam::LifetimeParam(it) => {
                let Some(lifetime) = it.lifetime() else { continue };
                (lifetime.text().to_owned(), lifetime.syntax().text_range(), true)
            }
            ast::GenericParam::TypeParam(it) => {
                let Some(name) = it.name() else { continue };
                (name.text().to_owned(), name.syntax().text_range(), false)
            }
            ast::GenericParam::ConstParam(it) => {
                let Some(name) = it.name() else { continue };
                (name.text().to_owned(), name.syntax().text_range(), false)
            }
        };
        // A lifetime shadowing one of the item around is E0496, which is not this check's.
        if lifetime && outer.lifetimes.contains(&text) {
            continue;
        }
        let span = cx.span(list.with_value(range));
        if let Some(first) = seen.get(&text) {
            let mut diag = Diag::error(
                Some("E0403"),
                format!(
                    "the name `{text}` is already used for a generic parameter in this item's generic parameters"
                ),
                span.clone(),
            );
            diag.span_label(span, "already used");
            diag.span_label(first.clone(), format!("first use of `{text}`"));
            push(diag, fact, found);
            continue;
        }
        seen.insert(text.clone(), span.clone());
        if lifetime && text == "'static" {
            let mut diag = Diag::error(
                Some("E0262"),
                format!("invalid lifetime parameter name: `{text}`"),
                span.clone(),
            );
            diag.span_label(span, "'static is a reserved lifetime name");
            push(diag, fact, found);
        }
    }
}

/// rustc's `ForwardGenericParamBan` ribs: a default may name only the parameters before it.
fn forward(cx: &Cx<'_, '_>, list: InFile<&ast::GenericParamList>, fact: Option<&Fact>, found: &mut Vec<Found>) {
    let mut types: FxHashSet<String> = FxHashSet::default();
    let mut consts: FxHashSet<String> = FxHashSet::default();
    for param in list.value.type_or_const_params() {
        match param {
            ast::TypeOrConstParam::Type(it) => types.extend(it.name().map(|it| it.text().to_owned())),
            ast::TypeOrConstParam::Const(it) => consts.extend(it.name().map(|it| it.text().to_owned())),
        }
    }
    for param in list.value.type_or_const_params() {
        match param {
            ast::TypeOrConstParam::Type(it) => {
                if let Some(default) = it.default_type() {
                    banned(cx, list.with_value(default.syntax()), &types, &consts, fact, found);
                }
                if let Some(name) = it.name() {
                    types.remove(name.text());
                }
            }
            ast::TypeOrConstParam::Const(it) => {
                if let Some(default) = it.default_val() {
                    banned(cx, list.with_value(default.syntax()), &types, &consts, fact, found);
                }
                if let Some(name) = it.name() {
                    consts.remove(name.text());
                }
            }
        }
    }
}

/// Each path in `default` whose first segment names a banned parameter of its namespace. A
/// type argument that names no type is looked up as a value, as rustc does.
fn banned(
    cx: &Cx<'_, '_>,
    default: InFile<&SyntaxNode>,
    types: &FxHashSet<String>,
    consts: &FxHashSet<String>,
    fact: Option<&Fact>,
    found: &mut Vec<Found>,
) {
    for path in default.value.descendants().filter_map(ast::Path::cast) {
        if path.qualifier().is_some() {
            continue;
        }
        let Some(segment) = path.segment() else { continue };
        if segment.coloncolon_token().is_some() || segment.type_anchor().is_some() {
            continue;
        }
        let Some(name) = segment.name_ref() else { continue };
        let Some(parent) = path.syntax().parent() else { continue };
        let text = name.text();
        let hit = if ast::PathExpr::can_cast(parent.kind()) {
            consts.contains(text)
        } else if ast::Path::can_cast(parent.kind()) {
            types.contains(text)
        } else if ast::PathType::can_cast(parent.kind()) {
            types.contains(text)
                || (consts.contains(text)
                    && parent.parent().is_some_and(|it| ast::TypeArg::can_cast(it.kind())))
        } else {
            false
        };
        if !hit {
            continue;
        }
        let span = cx.span(default.with_value(name.syntax().text_range()));
        let mut diag = Diag::error(
            Some("E0128"),
            "generic parameter defaults cannot reference parameters before they are declared",
            span.clone(),
        );
        diag.span_label(span, format!("cannot reference `{text}` before it is declared"));
        push(diag, fact, found);
    }
}

fn push(mut diag: Diag, fact: Option<&Fact>, found: &mut Vec<Found>) {
    if let Some(fact) = fact {
        diag.fact(fact.clone(), "the generic parameters of the item");
    }
    found.push(Found::new(diag, Phase::Resolve));
}

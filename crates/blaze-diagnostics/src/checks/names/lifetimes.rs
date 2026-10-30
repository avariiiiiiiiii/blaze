//! A named lifetime of a signature that no binder of its item declares: E0261 "use of
//! undeclared lifetime name", or E0401 when an item around declares it; with `'static` for a
//! misspelling of it, else the lifetime introduced at each binder of the item, innermost first.
//! A lifetime in the type of a const parameter is E0770, not this.
//!
//! Follows rustc's `compiler/rustc_resolve/src/late.rs` (`resolve_lifetime`, the lifetime ribs
//! of `visit_ty`, `visit_poly_trait_ref`, `visit_where_predicate` and `resolve_item`,
//! `LifetimeBinderKind::descr`) and `late/diagnostics.rs` (`emit_undeclared_lifetime_error`,
//! `suggest_introducing_lifetime_filtered`) (MIT or Apache-2.0).

use hir::InFile;
use syntax::{
    AstNode, SyntaxNode, TextRange,
    ast::{self, HasGenericParams as _, HasName as _, HasTypeBounds as _},
};

use super::{Signature, outer};
use crate::{
    cx::Cx,
    lints::edit_distance::edit_distance,
    lower::{Found, Phase},
    model::{Applicability, Diag},
};

/// rustc's `LifetimeBinderKind`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    FnPtrType,
    PolyTrait,
    WhereBound,
    Item,
    ConstItem,
    ImplAssocType,
    ImplBlock,
    Function,
}

impl Kind {
    fn descr(self) -> &'static str {
        match self {
            Kind::FnPtrType => "type",
            Kind::PolyTrait | Kind::WhereBound => "bound",
            Kind::Item | Kind::ConstItem => "item",
            Kind::ImplAssocType => "associated type",
            Kind::ImplBlock => "impl block",
            Kind::Function => "function",
        }
    }

    fn is_higher_ranked(self) -> bool {
        matches!(self, Kind::FnPtrType | Kind::PolyTrait | Kind::WhereBound)
    }
}

/// A lifetime rib of rustc's: what it declares, and its span (`for<..>` or `<..>`, or empty
/// where they would go).
struct Binder {
    kind: Kind,
    list: Option<ast::GenericParamList>,
    span: TextRange,
    node: SyntaxNode,
}

impl Binder {
    fn of(node: &SyntaxNode) -> Option<Binder> {
        let binder = |kind, for_binder: Option<ast::ForBinder>, at: TextRange| {
            let list = for_binder.as_ref().and_then(|it| it.generic_param_list());
            let span = for_binder.map_or(TextRange::empty(at.start()), |it| it.syntax().text_range());
            Some(Binder { kind, list, span, node: node.clone() })
        };
        if let Some(it) = ast::ForType::cast(node.clone()) {
            return binder(Kind::FnPtrType, it.for_binder(), it.syntax().text_range());
        }
        if let Some(it) = ast::FnPtrType::cast(node.clone()) {
            if node.parent().is_some_and(|it| ast::ForType::can_cast(it.kind())) {
                return None;
            }
            return binder(Kind::FnPtrType, None, it.syntax().text_range());
        }
        if let Some(it) = ast::TypeBound::cast(node.clone()) {
            let path = match it.ty()? {
                ast::Type::PathType(ty) => ty.syntax().text_range(),
                _ => return None,
            };
            return binder(Kind::PolyTrait, it.for_binder(), path);
        }
        if let Some(it) = ast::WherePred::cast(node.clone()) {
            it.ty()?;
            return binder(Kind::WhereBound, it.for_binder(), it.syntax().text_range());
        }
        let kind = match ast::Item::cast(node.clone())? {
            ast::Item::Fn(_) => Kind::Function,
            ast::Item::Struct(_)
            | ast::Item::Enum(_)
            | ast::Item::Union(_)
            | ast::Item::Trait(_)
            | ast::Item::TraitAlias(_) => Kind::Item,
            ast::Item::TypeAlias(_) => {
                let in_impl = node
                    .parent()
                    .filter(|it| ast::AssocItemList::can_cast(it.kind()))
                    .and_then(|it| it.parent())
                    .is_some_and(|it| ast::Impl::can_cast(it.kind()));
                if in_impl { Kind::ImplAssocType } else { Kind::Item }
            }
            ast::Item::Const(_) => Kind::ConstItem,
            ast::Item::Impl(_) => Kind::ImplBlock,
            _ => return None,
        };
        let list = ast::AnyHasGenericParams::cast(node.clone())?.generic_param_list();
        let span = match &list {
            Some(it) => it.syntax().text_range(),
            None => {
                let after = match ast::AnyHasName::cast(node.clone()).and_then(|it| it.name()) {
                    Some(name) => name.syntax().text_range().end(),
                    None => ast::Impl::cast(node.clone())?.impl_token()?.text_range().end(),
                };
                TextRange::empty(after)
            }
        };
        Some(Binder { kind, list, span, node: node.clone() })
    }

    fn declares(&self, name: &str) -> Option<ast::Lifetime> {
        declared(self.list.as_ref()?, name)
    }
}

fn declared(list: &ast::GenericParamList, name: &str) -> Option<ast::Lifetime> {
    list.lifetime_params().filter_map(|it| it.lifetime()).find(|it| it.text() == name)
}

pub(super) fn check(cx: &Cx<'_, '_>, signature: &Signature, lifetime: &ast::Lifetime) -> Option<Found> {
    let name = lifetime.text();
    if name == "'static" || name == "'_" {
        return None;
    }
    if lifetime.syntax().parent().is_some_and(|it| ast::LifetimeParam::can_cast(it.kind())) {
        return None;
    }
    let inner = outer::inner_item(&signature.node.value);
    let mut binders: Vec<Binder> = Vec::new();
    for node in lifetime.syntax().ancestors() {
        if ast::ConstParam::can_cast(node.kind()) {
            return None;
        }
        if let Some(binder) = Binder::of(&node) {
            if binder.declares(name).is_some() {
                return None;
            }
            binders.push(binder);
        }
        if node == inner {
            break;
        }
    }
    let outer = inner
        .ancestors()
        .skip(1)
        .take_while(|it| !ast::Module::can_cast(it.kind()) && !ast::SourceFile::can_cast(it.kind()))
        .filter_map(|it| Binder::of(&it))
        .find_map(|it| it.declares(name));

    let file = signature.node.file_id;
    let at = |range: TextRange| cx.span(InFile::new(file, range));
    let span = at(lifetime.syntax().text_range());
    let mut diag = match &outer {
        Some(outer) => {
            let mut diag = Diag::error(Some("E0401"), "can't use generic parameters from outer item", span.clone());
            diag.span_label(span.clone(), "use of generic parameter from outer item");
            diag.span_label(at(outer.syntax().text_range()), "lifetime parameter from outer item");
            diag
        }
        None => {
            let mut diag = Diag::error(Some("E0261"), format!("use of undeclared lifetime name `{name}`"), span.clone());
            diag.span_label(span.clone(), "undeclared lifetime");
            diag
        }
    };
    if edit_distance(name, "'static", 2).is_some() {
        diag.suggestion_verbose(span, "you may have misspelled the `'static` lifetime", "'static", Applicability::MachineApplicable);
    } else {
        let generic_const_items = hir_def::unstable_features::UnstableFeatures::query(cx.db, cx.krate.base())
            .iter()
            .any(|it| it.as_str() == "generic_const_items");
        let mut noted = false;
        for binder in &binders {
            if binder.kind == Kind::ImplAssocType || (binder.kind == Kind::ConstItem && !generic_const_items) {
                continue;
            }
            let higher_ranked = binder.kind.is_higher_ranked();
            let mut removed: Vec<TextRange> = Vec::new();
            let (insert, sugg) = if binder.span.is_empty() {
                let mut names = vec![name.to_owned()];
                if binder.kind == Kind::WhereBound
                    && let Some(pred) = ast::WherePred::cast(binder.node.clone())
                    && let Some(bounds) = pred.type_bound_list()
                {
                    for bound in bounds.bounds() {
                        let (Some(for_binder), Some(ast::Type::PathType(ty))) = (bound.for_binder(), bound.ty()) else {
                            continue;
                        };
                        let start = bound.syntax().text_range().start();
                        removed.push(TextRange::new(start, ty.syntax().text_range().start()));
                        for param in for_binder.generic_param_list().iter().flat_map(|it| it.lifetime_params()) {
                            if let Some(it) = param.lifetime()
                                && !names.iter().any(|name| *name == it.text())
                            {
                                names.push(it.text().to_string());
                            }
                        }
                    }
                }
                let names = names.join(", ");
                let sugg = if higher_ranked { format!("for<{names}> ") } else { format!("<{names}>") };
                (binder.span, sugg)
            } else {
                let Some(angle) = binder.list.as_ref().and_then(|it| it.l_angle_token()) else { continue };
                (TextRange::empty(angle.text_range().end()), format!("{name}, "))
            };
            let message = if higher_ranked {
                format!("consider making the {} lifetime-generic with a new `{name}` lifetime", binder.kind.descr())
            } else {
                format!("consider introducing lifetime `{name}` here")
            };
            let parts = std::iter::once((at(insert), sugg)).chain(removed.into_iter().map(|it| (at(it), String::new()))).collect();
            diag.multipart_suggestion(message, parts, Applicability::MaybeIncorrect);
            if higher_ranked && !noted {
                noted = true;
                diag.note("for more information on higher-ranked polymorphism, visit https://doc.rust-lang.org/nomicon/hrtb.html");
            }
        }
    }
    if let Some((fact, what)) = &signature.fact {
        diag.fact(fact.clone(), what.clone());
    }
    Some(Found::new(diag, Phase::Resolve))
}

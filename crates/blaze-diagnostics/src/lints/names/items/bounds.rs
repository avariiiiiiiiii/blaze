//! What rustc's late passes lint of bounds:
//!
//! - `type_alias_bounds`: the bounds of a type alias's parameters and its `where` clause, which
//!   are not checked where it is used (unless `checked_type_aliases` is on or it names an
//!   `impl Trait`): one lint for the `where` clause, then one for the bounds, each removed by
//!   the suggestion, with `<T as /* Trait */>` suggested for each `T::Assoc` of the type;
//! - `drop_bounds`: a `T: Drop` bound of an item, `T` not an opaque type;
//! - `dyn_drop`: a `dyn Drop` type in an item or its body.
//!
//! Neither of the last two is given when `core` has no `needs_drop`, as rustc does.
//!
//! Follows `compiler/rustc_lint/src/builtin.rs` (`TypeAliasBounds`, `ShorthandAssocTyCollector`),
//! `compiler/rustc_lint/src/traits.rs` (`DropTraitConstraints`),
//! `compiler/rustc_hir_analysis/src/collect/type_of.rs` (`type_alias_is_checked`) and
//! `compiler/rustc_lint/src/lints.rs` (`BuiltinTypeAliasBounds`, `DropTraitConstraintsDiag`,
//! `DropGlue`) (MIT or Apache-2.0).

use hir::{InFile, ModuleDef, PathResolution};
use syntax::{
    AstNode, SyntaxNode, SyntaxNodePtr, TextRange,
    ast::{self, HasGenericArgs, HasGenericParams, HasName, HasTypeBounds},
};

use super::walk::{Container, Item};
use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::{Applicability, Diag, Span},
};

pub(super) fn type_alias(cx: &Cx<'_, '_>, item: &Item) -> Vec<Found> {
    let ast::Item::TypeAlias(alias) = &item.node.value else { return Vec::new() };
    if !matches!(item.container, Container::Module) {
        return Vec::new();
    }
    let params: Vec<ast::GenericParam> = alias.generic_param_list().map(|it| it.generic_params().collect()).unwrap_or_default();
    let inline: Vec<(TextRange, Vec<TextRange>)> = params.iter().filter_map(inline_bounds).collect();
    let clause = alias.where_clause();
    let preds: Vec<ast::WherePred> = clause.iter().flat_map(|it| it.predicates()).collect();
    if inline.is_empty() && preds.is_empty() {
        return Vec::new();
    }
    let ty = alias.ty();
    let checked = super::feature(cx, "checked_type_aliases")
        || ty.as_ref().is_some_and(|it| it.syntax().descendants().any(|it| ast::ImplTraitType::can_cast(it.kind())));
    if checked {
        return Vec::new();
    }
    let span = |range: TextRange| cx.span(item.node.with_value(range));
    let qselves: Vec<Span> = ty.iter().flat_map(|ty| shorthand_qselves(cx, ty.syntax())).map(&span).collect();
    let mut found = Vec::new();
    if let (Some(clause), false) = (&clause, preds.is_empty()) {
        let spans: Vec<Span> = preds.iter().map(|it| span(it.syntax().text_range())).collect();
        let affects = preds.iter().any(affects_object_lifetime_defaults);
        let removed = vec![(span(clause.syntax().text_range()), String::new())];
        found.push(bounds_lint(cx, item, true, spans, removed, &qselves, affects));
    }
    if !inline.is_empty() {
        let spans: Vec<Span> = inline.iter().flat_map(|(_, bounds)| bounds.iter().map(|it| span(*it))).collect();
        let affects = params.iter().any(|it| matches!(it, ast::GenericParam::TypeParam(p) if outlives(p.type_bound_list())));
        let removed = inline.iter().map(|(pred, _)| (span(*pred), String::new())).collect();
        found.push(bounds_lint(cx, item, false, spans, removed, &qselves, affects));
    }
    found
}

#[allow(clippy::too_many_arguments)]
fn bounds_lint(
    cx: &Cx<'_, '_>,
    item: &Item,
    in_where_clause: bool,
    spans: Vec<Span>,
    removed: Vec<(Span, String)>,
    qselves: &[Span],
    affects_object_lifetime_defaults: bool,
) -> Found {
    let message = match in_where_clause {
        true => "where clauses on type aliases are not enforced",
        false => "bounds on generic parameters in type aliases are not enforced",
    };
    let label = spans.last().cloned().expect("a bound");
    let mut diag = Diag::lint("type_alias_bounds", message, spans[0].clone());
    diag.span.primary = spans;
    diag.span_label(label, "will not be checked at usage sites of the type alias");
    diag.note(
        "this is a known limitation of the type checker that may be lifted in a future edition.\n\
         see issue #112792 <https://github.com/rust-lang/rust/issues/112792> for more information",
    );
    diag.help("add `#![feature(checked_type_aliases)]` to the crate attributes to enable the desired semantics");
    let applicability = match !qselves.is_empty() || affects_object_lifetime_defaults {
        true => Applicability::MaybeIncorrect,
        false => Applicability::MachineApplicable,
    };
    let remove = match (in_where_clause, removed.len()) {
        (true, _) => "remove this where clause",
        (false, 1) => "remove this bound",
        (false, _) => "remove these bounds",
    };
    diag.multipart_suggestion(remove, removed, applicability);
    for qself in qselves {
        diag.multipart_suggestion(
            "fully qualify this associated type",
            vec![(qself.shrink_to_lo(), "<".to_owned()), (qself.shrink_to_hi(), " as /* Trait */>".to_owned())],
            Applicability::HasPlaceholders,
        );
    }
    crate::lints::code_fact(cx, &mut diag, item.ptr());
    if let Some((id, name)) = super::item_id(cx, item) {
        diag.fact(blaze_items::Fact::Signature(cx.key(id)), format!("the signature of `{name}`"));
    }
    Found::lint(diag, Phase::Late, item.ptr())
}

/// A parameter's inline bounds: the predicate HIR makes of them (from after the parameter's
/// name to the end of its bounds) and each bound.
fn inline_bounds(param: &ast::GenericParam) -> Option<(TextRange, Vec<TextRange>)> {
    let (name_end, list) = match param {
        ast::GenericParam::TypeParam(it) => (it.name()?.syntax().text_range().end(), it.type_bound_list()?),
        ast::GenericParam::LifetimeParam(it) => (it.lifetime()?.syntax().text_range().end(), it.type_bound_list()?),
        ast::GenericParam::ConstParam(_) => return None,
    };
    let bounds: Vec<TextRange> = list.bounds().map(|it| it.syntax().text_range()).collect();
    let last = bounds.last()?;
    Some((TextRange::new(name_end, last.end()), bounds))
}

/// `T: 'a`, with `T` a type parameter and no `for<..>`: a bound that sets the default lifetime
/// of trait objects passed to the alias.
fn affects_object_lifetime_defaults(pred: &ast::WherePred) -> bool {
    if pred.for_binder().is_some() {
        return false;
    }
    let param = match pred.ty() {
        Some(ast::Type::PathType(it)) => it.path().is_some_and(|p| p.qualifier().is_none() && p.segment().is_some_and(|s| s.generic_arg_list().is_none())),
        _ => false,
    };
    param && outlives(pred.type_bound_list())
}

fn outlives(list: Option<ast::TypeBoundList>) -> bool {
    list.into_iter().flat_map(|it| it.bounds()).any(|it| it.lifetime().is_some())
}

/// The `T` of each `T::Assoc` in a type, `T` a type parameter.
fn shorthand_qselves(cx: &Cx<'_, '_>, ty: &SyntaxNode) -> Vec<TextRange> {
    let mut found = Vec::new();
    for path in ty.descendants().filter_map(ast::Path::cast) {
        let Some(qualifier) = path.qualifier() else { continue };
        if qualifier.qualifier().is_some() || path.segment().and_then(|it| it.type_anchor()).is_some() {
            continue;
        }
        if let Some(PathResolution::TypeParam(_)) = cx.sema.resolve_path(&qualifier) {
            found.push(qualifier.syntax().text_range());
        }
    }
    found
}

/// `drop_bounds` of an item's bounds, then `dyn_drop` of its `dyn Drop` types.
pub(super) fn drop(cx: &Cx<'_, '_>, item: &Item) -> Vec<Found> {
    let Some(drop) = hir_def::lang_item::lang_items(cx.db, cx.krate.into()).Drop else { return Vec::new() };
    let Some(needs_drop) = super::core_path(cx, "mem", "needs_drop") else { return Vec::new() };
    let is_drop = |path: Option<ast::Path>| {
        path.and_then(|it| cx.sema.resolve_path(&it)).is_some_and(|it| {
            matches!(it, PathResolution::Def(ModuleDef::Trait(t)) if hir_def::TraitId::from(t) == drop)
        })
    };
    let bound_path = |bound: &ast::TypeBound| match bound.ty() {
        Some(ast::Type::PathType(it)) => it.path(),
        _ => None,
    };
    let mut found = Vec::new();
    if matches!(item.container, Container::Module) {
        for (bounded, bound) in clauses(&item.node) {
            if !is_drop(bound_path(&bound.value)) {
                continue;
            }
            let clause = format!("{bounded}: Drop");
            let message = format!(
                "bounds on `{clause}` are most likely incorrect, consider instead using `{needs_drop}` to detect whether a type can be trivially dropped"
            );
            let mut diag = Diag::lint("drop_bounds", message, cx.span(bound.as_ref().map(|it| it.syntax().text_range())));
            crate::lints::code_fact(cx, &mut diag, item.ptr());
            found.push(Found::lint(diag, Phase::Late, item.ptr()));
        }
    }
    for ty in item.own::<ast::DynTraitType>() {
        for bound in ty.value.type_bound_list().into_iter().flat_map(|it| it.bounds()) {
            if !is_drop(bound_path(&bound)) {
                continue;
            }
            let message = format!(
                "types that do not implement `Drop` can still have drop glue, consider instead using `{needs_drop}` to detect whether a type is trivially dropped"
            );
            let mut diag = Diag::lint("dyn_drop", message, cx.span(ty.with_value(bound.syntax().text_range())));
            let at = ty.as_ref().map(|it| SyntaxNodePtr::new(it.syntax()));
            crate::lints::code_fact(cx, &mut diag, at);
            found.push(Found::lint(diag, Phase::Late, at));
        }
    }
    found
}

/// An item's explicit trait bounds, each with the type it bounds as rustc prints the clause:
/// its parameters' bounds, its `where` clause's, a trait's supertraits (`Self`), and its
/// `impl Trait` parameters; in that order.
fn clauses(item: &InFile<ast::Item>) -> Vec<(String, InFile<ast::TypeBound>)> {
    let mut found = Vec::new();
    let node = item.value.syntax();
    let params = node.children().find_map(ast::GenericParamList::cast);
    for param in params.iter().flat_map(|it| it.type_or_const_params()) {
        if let ast::TypeOrConstParam::Type(param) = param
            && let Some(name) = param.name()
        {
            for bound in param.type_bound_list().into_iter().flat_map(|it| it.bounds()) {
                found.push((name.text().to_string(), item.with_value(bound)));
            }
        }
    }
    if let ast::Item::Trait(it) = &item.value {
        for bound in it.type_bound_list().into_iter().flat_map(|it| it.bounds()) {
            found.push(("Self".to_owned(), item.with_value(bound)));
        }
    }
    let clause = node.children().find_map(ast::WhereClause::cast);
    for pred in clause.iter().flat_map(|it| it.predicates()) {
        let Some(ty) = pred.ty() else { continue };
        for bound in pred.type_bound_list().into_iter().flat_map(|it| it.bounds()) {
            found.push((ty.syntax().text().to_string(), item.with_value(bound)));
        }
    }
    if let ast::Item::Fn(f) = &item.value {
        for ty in f.param_list().into_iter().flat_map(|it| it.params()).filter_map(|it| it.ty()) {
            for apit in ty.syntax().descendants().filter_map(ast::ImplTraitType::cast) {
                for bound in apit.type_bound_list().into_iter().flat_map(|it| it.bounds()) {
                    found.push((apit.syntax().text().to_string(), item.with_value(bound)));
                }
            }
        }
    }
    found
}

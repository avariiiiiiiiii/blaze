//! E0392: a type or lifetime parameter of a struct, enum or union that nothing constrains, so
//! its variance is bivariant; or one used only through the type itself ("is only used
//! recursively").
//!
//! Follows rustc's `compiler/rustc_hir_analysis/src/check/wfcheck.rs`
//! (`check_variances_for_type_defn`, `report_bivariance`, `HasErrorDeep`,
//! `IsProbablyCyclical`, `CollectUsageSpans`), `constrained_generic_params.rs`
//! (`identify_constrained_generic_params`, `setup_constraining_predicates`, `parameters_for`)
//! and `diagnostics.rs` (`UnusedGenericParameter`, `RecursiveGenericParameter`,
//! `UnusedGenericParameterHelp`) (MIT or Apache-2.0). The variances are hir-ty's
//! `variances_of`.

use blaze_items::{Fact, ItemId};
use hir::PathResolution;
use rustc_hash::FxHashSet;
use syntax::{
    AstNode, SyntaxNode, TextRange, WalkEvent,
    ast::{self, HasGenericArgs as _, HasGenericParams as _, HasName as _, HasTypeBounds as _},
};

use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::{Diag, MultiSpan, Span},
};

/// A predicate `<inputs as Trait>::Assoc == outputs`: the parameters it reads, and those it
/// constrains once all of those are.
struct Projection {
    inputs: FxHashSet<hir::GenericParam>,
    outputs: FxHashSet<hir::GenericParam>,
}

pub(super) fn check(cx: &Cx<'_, '_>, adt: hir::Adt) -> Vec<Found> {
    let db = cx.db;
    let Some(source) = cx.sema.source(adt) else { return Vec::new() };
    let Some(list) = source.value.generic_param_list() else { return Vec::new() };
    let params: Vec<(hir::GenericParam, ast::GenericParam)> =
        list.generic_params().filter_map(|it| Some((cx.sema.to_def(&it)?, it))).collect();
    let mut constrained: FxHashSet<hir::GenericParam> = params
        .iter()
        .filter(|(param, _)| param.variance(db) != Some(hir::Variance::Bivariant))
        .map(|(param, _)| *param)
        .collect();
    if params.iter().all(|(param, _)| constrained.contains(param)) {
        return Vec::new();
    }
    identify_constrained(cx, &source.value, &mut constrained);
    if params.iter().all(|(param, _)| constrained.contains(param)) || has_error_deep(cx, adt) {
        return Vec::new();
    }

    let fields = field_types(&source.value);
    let phantom_data = hir_def::lang_item::lang_items(db, cx.krate.base())
        .PhantomData
        .map(|it| cx.def_path(hir::ModuleDef::Adt(hir::Adt::Struct(it.into()))));
    let what = format!("the fields of `{}`", cx.def_path(hir::ModuleDef::Adt(adt)));
    let fact = Fact::Layout(cx.key(ItemId::Adt(adt.into())));
    let mut cyclical = None;
    let mut found = Vec::new();
    for (param, node) in &params {
        if constrained.contains(param) {
            continue;
        }
        let (name, kind, range) = match node {
            ast::GenericParam::TypeParam(it) => {
                let Some(name) = it.name() else { continue };
                (name.text().to_string(), "type parameter", name.syntax().text_range())
            }
            ast::GenericParam::LifetimeParam(it) => {
                let Some(lifetime) = it.lifetime() else { continue };
                (lifetime.text().to_string(), "lifetime parameter", lifetime.syntax().text_range())
            }
            ast::GenericParam::ConstParam(_) => continue,
        };
        let span = cx.span(source.with_value(range));
        let usages: Vec<Span> = usage_spans(cx, &fields, *param)
            .into_iter()
            .map(|it| cx.span(source.with_value(it)))
            .collect();
        let help = match &phantom_data {
            Some(phantom) => format!(
                "consider removing `{name}`, referring to it in a field, or using a marker such as `{phantom}`"
            ),
            None => format!("consider removing `{name}` or referring to it in a field"),
        };
        if !usages.is_empty() && *cyclical.get_or_insert_with(|| is_probably_cyclical(cx, adt)) {
            let mut diag = Diag::new(
                crate::model::Level::Error,
                None,
                format!("{kind} `{name}` is only used recursively"),
            );
            diag.span = MultiSpan { primary: usages, labels: Vec::new() };
            diag.span_label(span, format!("{kind} must be used non-recursively in the definition"));
            diag.help(help);
            diag.note("all type parameters must be used in a non-recursive way in order to constrain their variance");
            diag.fact(fact.clone(), what.clone());
            found.push(Found::new(diag, Phase::Items));
            continue;
        }
        let mut diag = Diag::error(Some("E0392"), format!("{kind} `{name}` is never used"), span.clone());
        diag.span_label(span, format!("unused {kind}"));
        for usage in usages {
            diag.span_label(usage, format!("`{name}` is named here, but is likely unused in the containing type"));
        }
        diag.help(help);
        if let ast::GenericParam::TypeParam(it) = node
            && !is_explicitly_bounded(cx, &source.value, it, *param)
        {
            diag.help(format!(
                "if you intended `{name}` to be a const parameter, use `const {name}: /* Type */` instead"
            ));
        }
        diag.fact(fact.clone(), what.clone());
        found.push(Found::new(diag, Phase::Items));
    }
    found
}

/// rustc's `identify_constrained_generic_params` of an ADT's predicates: a parameter an
/// associated type binding equates with is constrained once all that the projection reads are.
fn identify_constrained(cx: &Cx<'_, '_>, node: &ast::Adt, constrained: &mut FxHashSet<hir::GenericParam>) {
    let mut projections = Vec::new();
    if let Some(list) = node.generic_param_list() {
        for param in list.type_or_const_params() {
            let ast::TypeOrConstParam::Type(param) = param else { continue };
            let (Some(def), Some(bounds)) = (cx.sema.to_def(&param), param.type_bound_list()) else {
                continue;
            };
            let inputs = FxHashSet::from_iter([hir::GenericParam::TypeParam(def)]);
            bound_projections(cx, &inputs, &bounds, &mut projections);
        }
    }
    if let Some(clause) = node.where_clause() {
        for pred in clause.predicates() {
            let (Some(ty), Some(bounds)) = (pred.ty(), pred.type_bound_list()) else { continue };
            let mut inputs = FxHashSet::default();
            params_in(cx, ty.syntax(), true, &mut inputs);
            bound_projections(cx, &inputs, &bounds, &mut projections);
        }
    }
    loop {
        let mut changed = false;
        for projection in &projections {
            if projection.inputs.iter().all(|it| constrained.contains(it)) {
                for &output in &projection.outputs {
                    changed |= constrained.insert(output);
                }
            }
        }
        if !changed {
            break;
        }
    }
}

/// The projections the associated type bindings of `bounds` on a type reading `inputs` make,
/// those of associated type bounds (`Trait<Assoc: Other<B = U>>`) included.
fn bound_projections(
    cx: &Cx<'_, '_>,
    inputs: &FxHashSet<hir::GenericParam>,
    bounds: &ast::TypeBoundList,
    out: &mut Vec<Projection>,
) {
    for bound in bounds.bounds() {
        let Some(ast::Type::PathType(ty)) = bound.ty() else { continue };
        let Some(segment) = ty.path().and_then(|it| it.segment()) else { continue };
        let Some(args) = segment.generic_arg_list() else { continue };
        let mut trait_inputs = inputs.clone();
        for arg in args.generic_args() {
            if !matches!(arg, ast::GenericArg::AssocTypeArg(_)) {
                params_in(cx, arg.syntax(), true, &mut trait_inputs);
            }
        }
        for arg in args.generic_args() {
            let ast::GenericArg::AssocTypeArg(arg) = arg else { continue };
            let mut inputs = trait_inputs.clone();
            if let Some(own) = arg.generic_arg_list() {
                params_in(cx, own.syntax(), true, &mut inputs);
            }
            if let Some(ty) = arg.ty() {
                let mut outputs = FxHashSet::default();
                params_in(cx, ty.syntax(), false, &mut outputs);
                out.push(Projection { inputs: inputs.clone(), outputs });
            }
            if let Some(list) = arg.type_bound_list() {
                bound_projections(cx, &inputs, &list, out);
            }
        }
    }
}

/// rustc's `parameters_for`: the parameters named in `node`, those inside a projection or an
/// unevaluated constant only with `include_nonconstraining`.
fn params_in(
    cx: &Cx<'_, '_>,
    node: &SyntaxNode,
    include_nonconstraining: bool,
    out: &mut FxHashSet<hir::GenericParam>,
) {
    let mut walk = node.preorder();
    while let Some(event) = walk.next() {
        let WalkEvent::Enter(node) = event else { continue };
        if !include_nonconstraining {
            let skip = match ast::Type::cast(node.clone()) {
                Some(ast::Type::PathType(ty)) => ty.path().is_some_and(|it| is_projection(cx, &it)),
                _ => ast::ConstArg::cast(node.clone())
                    .is_some_and(|it| !matches!(it.expr(), Some(ast::Expr::PathExpr(_)))),
            };
            if skip {
                walk.skip_subtree();
                continue;
            }
        }
        if let Some(lifetime) = ast::Lifetime::cast(node.clone()) {
            if let Some(param) = cx.sema.resolve_lifetime_param(&lifetime) {
                out.insert(hir::GenericParam::LifetimeParam(param));
            }
            continue;
        }
        let Some(path) = ast::Path::cast(node) else { continue };
        if path.qualifier().is_some() {
            continue;
        }
        match cx.sema.resolve_path(&path) {
            Some(PathResolution::TypeParam(it)) => {
                out.insert(hir::GenericParam::TypeParam(it));
            }
            Some(PathResolution::ConstParam(it)) => {
                out.insert(hir::GenericParam::ConstParam(it));
            }
            _ => {}
        }
    }
}

/// Whether the type `path` names is an associated type of another: `<T as Trait>::A`, `T::A`.
fn is_projection(cx: &Cx<'_, '_>, path: &ast::Path) -> bool {
    let Some(qualifier) = path.qualifier() else { return false };
    let first = qualifier.first_segment();
    if first.is_some_and(|it| it.type_anchor().is_some()) {
        return true;
    }
    matches!(
        cx.sema.resolve_path(&qualifier),
        Some(
            PathResolution::TypeParam(_)
                | PathResolution::SelfType(_)
                | PathResolution::Def(
                    hir::ModuleDef::Adt(_) | hir::ModuleDef::BuiltinType(_) | hir::ModuleDef::TypeAlias(_)
                )
        )
    )
}

/// The written types of the fields of every variant.
fn field_types(node: &ast::Adt) -> Vec<ast::Type> {
    let lists: Vec<ast::FieldList> = match node {
        ast::Adt::Struct(it) => it.field_list().into_iter().collect(),
        ast::Adt::Union(it) => it.record_field_list().map(ast::FieldList::RecordFieldList).into_iter().collect(),
        ast::Adt::Enum(it) => it
            .variant_list()
            .into_iter()
            .flat_map(|it| it.variants())
            .filter_map(|it| it.field_list())
            .collect(),
    };
    let mut types = Vec::new();
    for list in lists {
        match list {
            ast::FieldList::RecordFieldList(it) => types.extend(it.fields().filter_map(|it| it.ty())),
            ast::FieldList::TupleFieldList(it) => types.extend(it.fields().filter_map(|it| it.ty())),
        }
    }
    types
}

/// rustc's `CollectUsageSpans`: where the fields name `param`, or `Self`.
fn usage_spans(cx: &Cx<'_, '_>, fields: &[ast::Type], param: hir::GenericParam) -> Vec<TextRange> {
    let mut spans = Vec::new();
    for ty in fields {
        for ty in ty.syntax().descendants().filter_map(ast::PathType::cast) {
            let Some(path) = ty.path() else { continue };
            let Some(first) = path.first_segment() else { continue };
            let Some(first_path) = first.syntax().parent().and_then(ast::Path::cast) else { continue };
            if first.generic_arg_list().is_some() || first.type_anchor().is_some() {
                continue;
            }
            if first.self_type_token().is_some() {
                spans.push(first_path.syntax().text_range());
                continue;
            }
            if let hir::GenericParam::TypeParam(param) = param
                && cx.sema.resolve_path(&first_path) == Some(PathResolution::TypeParam(param))
            {
                spans.push(first_path.syntax().text_range());
            }
        }
    }
    spans
}

/// Whether a bound is written on the parameter itself, inline or in the where clause.
fn is_explicitly_bounded(cx: &Cx<'_, '_>, node: &ast::Adt, param: &ast::TypeParam, def: hir::GenericParam) -> bool {
    if param.type_bound_list().is_some() {
        return true;
    }
    let Some(clause) = node.where_clause() else { return false };
    let hir::GenericParam::TypeParam(def) = def else { return false };
    clause.predicates().any(|pred| {
        if pred.type_bound_list().is_some()
            && let Some(ast::Type::PathType(ty)) = pred.ty()
            && let Some(path) = ty.path()
            && path.qualifier().is_none()
        {
            cx.sema.resolve_path(&path) == Some(PathResolution::TypeParam(def))
        } else {
            false
        }
    })
}

/// rustc's `HasErrorDeep`: whether the fields, and those of the types they hold, have a type
/// that is an error.
fn has_error_deep(cx: &Cx<'_, '_>, adt: hir::Adt) -> bool {
    let mut error = false;
    walk_fields(cx, adt, |ty, _| {
        error |= ty.is_unknown();
        error
    });
    error
}

/// rustc's `IsProbablyCyclical`: whether a field, or one of a type it holds, holds `adt`.
fn is_probably_cyclical(cx: &Cx<'_, '_>, adt: hir::Adt) -> bool {
    let mut cyclical = false;
    walk_fields(cx, adt, |_, it| {
        cyclical |= it == Some(adt);
        cyclical
    });
    cyclical
}

/// Every type in the fields of `adt` and, once each, of the ADTs they hold, until `stop` says.
fn walk_fields(cx: &Cx<'_, '_>, adt: hir::Adt, mut stop: impl FnMut(&hir::Type<'_>, Option<hir::Adt>) -> bool) {
    let mut seen = FxHashSet::from_iter([adt]);
    let mut queue = vec![adt];
    while let Some(adt) = queue.pop() {
        for field in adt_fields(cx, adt) {
            let mut done = false;
            field.ty(cx.db).walk(cx.db, |ty| {
                if done {
                    return;
                }
                let inner = ty.as_adt();
                done = stop(&ty, inner);
                if let Some(inner) = inner
                    && seen.insert(inner)
                {
                    queue.push(inner);
                }
            });
            if done {
                return;
            }
        }
    }
}

fn adt_fields(cx: &Cx<'_, '_>, adt: hir::Adt) -> Vec<hir::Field> {
    match adt {
        hir::Adt::Struct(it) => it.fields(cx.db),
        hir::Adt::Union(it) => it.fields(cx.db),
        hir::Adt::Enum(it) => it.variants(cx.db).into_iter().flat_map(|it| it.fields(cx.db)).collect(),
    }
}
//! E0392: a type or lifetime parameter of a struct, enum or union that nothing constrains, so
//! its variance is bivariant; or one used only through the type itself ("is only used
//! recursively"). E0091: a type parameter of a type alias its type never names.
//!
//! Follows rustc's `compiler/rustc_hir_analysis/src/check/check.rs`
//! (`check_type_alias_type_params_are_used`), `wfcheck.rs`
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
    let invalid = Invalid::of(&list);
    if invalid.named_in(&fields) {
        return Vec::new();
    }
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
                if invalid.lifetimes.contains(&lifetime.syntax().text_range()) {
                    continue;
                }
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

/// E0091: rustc's `check_type_alias_type_params_are_used`, a type parameter of a type alias its
/// type never names. A checked alias (`checked_type_aliases`, or one holding an `impl Trait`) is
/// checked as an item instead.
pub(super) fn check_alias(cx: &Cx<'_, '_>, alias: hir::TypeAlias) -> Vec<Found> {
    let db = cx.db;
    let Some(source) = cx.sema.source(alias) else { return Vec::new() };
    let Some(list) = source.value.generic_param_list() else { return Vec::new() };
    let params: Vec<(hir::GenericParam, ast::TypeParam)> = list
        .type_or_const_params()
        .filter_map(|it| match it {
            ast::TypeOrConstParam::Type(it) => Some((hir::GenericParam::TypeParam(cx.sema.to_def(&it)?), it)),
            ast::TypeOrConstParam::Const(_) => None,
        })
        .collect();
    let Some(ty) = source.value.ty() else { return Vec::new() };
    if params.is_empty()
        || ty.syntax().descendants().any(|it| ast::ImplTraitType::can_cast(it.kind()))
        || Invalid::of(&list).named_in(std::slice::from_ref(&ty))
    {
        return Vec::new();
    }
    let checked = hir_def::unstable_features::UnstableFeatures::query(db, cx.krate.base())
        .iter()
        .any(|it| it.as_str() == "checked_type_aliases");
    if checked {
        return Vec::new();
    }
    let mut error = false;
    alias.ty(db).walk(db, |ty| error |= ty.is_unknown());
    if error {
        return Vec::new();
    }
    let mut used = FxHashSet::default();
    params_in(cx, ty.syntax(), true, &mut used);

    // rustc reads the bounds after `Sized` elaboration: a parameter without `?Sized` has an
    // implicit bound, so none at all means every parameter is `?Sized` and bounded by nothing.
    let bounds: Vec<(bool, bool)> =
        params.iter().map(|(def, node)| written_bounds(cx, &source.value, node, *def)).collect();
    let none = bounds.iter().all(|&(relaxed, written)| relaxed && !written);
    let what = format!("the type of `{}`", cx.def_path(hir::ModuleDef::TypeAlias(alias)));
    let fact = Fact::Signature(cx.key(ItemId::Alias(alias.into())));
    let mut found = Vec::new();
    for ((param, node), (_, written)) in params.iter().zip(bounds) {
        if used.contains(param) {
            continue;
        }
        let Some(name) = node.name() else { continue };
        let name_text = name.text();
        let span = cx.span(source.with_value(name.syntax().text_range()));
        let mut diag =
            Diag::error(Some("E0091"), format!("type parameter `{name_text}` is never used"), span.clone());
        diag.span_label(span, "unused type parameter");
        diag.help(format!("consider removing `{name_text}` or referring to it in the body of the type alias"));
        if !(none || written) {
            diag.help(format!(
                "if you intended `{name_text}` to be a const parameter, use `const {name_text}: /* Type */` instead"
            ));
        }
        diag.fact(fact.clone(), what.clone());
        found.push(Found::new(diag, Phase::Items));
    }
    found
}

/// Whether `param` of an alias is relaxed with `?Sized`, and whether a bound other than a
/// relaxed one is written on it, inline or in the where clause.
fn written_bounds(cx: &Cx<'_, '_>, node: &ast::TypeAlias, param: &ast::TypeParam, def: hir::GenericParam) -> (bool, bool) {
    let mut lists: Vec<ast::TypeBoundList> = param.type_bound_list().into_iter().collect();
    if let (Some(clause), hir::GenericParam::TypeParam(def)) = (node.where_clause(), def) {
        for pred in clause.predicates() {
            let Some(ast::Type::PathType(ty)) = pred.ty() else { continue };
            let Some(path) = ty.path() else { continue };
            if path.qualifier().is_none()
                && cx.sema.resolve_path(&path) == Some(PathResolution::TypeParam(def))
            {
                lists.extend(pred.type_bound_list());
            }
        }
    }
    let (mut relaxed, mut written) = (false, false);
    for bound in lists.iter().flat_map(|it| it.bounds()) {
        if bound.question_mark_token().is_some() {
            relaxed = true;
        } else {
            written = true;
        }
    }
    (relaxed, written)
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

/// The parameters rustc's resolver turns into errors (E0403, E0262, E0637): a type or const
/// name declared twice, whose every use is then an error, and a lifetime declared again or
/// named `'static` or `'_`, whose name is `ParamName::Error`.
struct Invalid {
    names: FxHashSet<String>,
    lifetimes: FxHashSet<TextRange>,
}

impl Invalid {
    fn of(list: &ast::GenericParamList) -> Invalid {
        let mut seen = FxHashSet::default();
        let mut invalid = Invalid { names: FxHashSet::default(), lifetimes: FxHashSet::default() };
        for param in list.generic_params() {
            match param {
                ast::GenericParam::LifetimeParam(it) => {
                    let Some(lifetime) = it.lifetime() else { continue };
                    let text = lifetime.text();
                    if !seen.insert(text.to_owned()) || text == "'static" || text == "'_" {
                        invalid.lifetimes.insert(lifetime.syntax().text_range());
                    }
                }
                ast::GenericParam::TypeParam(it) => invalid.name(&mut seen, it.name()),
                ast::GenericParam::ConstParam(it) => invalid.name(&mut seen, it.name()),
            }
        }
        invalid
    }

    fn name(&mut self, seen: &mut FxHashSet<String>, name: Option<ast::Name>) {
        let Some(name) = name else { return };
        if !seen.insert(name.text().to_owned()) {
            self.names.insert(name.text().to_owned());
        }
    }

    /// Whether `fields` name a type or const declared twice, rustc's `Res::Err`.
    fn named_in(&self, fields: &[ast::Type]) -> bool {
        !self.names.is_empty()
            && fields.iter().flat_map(|ty| ty.syntax().descendants().filter_map(ast::Path::cast)).any(|path| {
                path.qualifier().is_none()
                    && path.segment().and_then(|it| it.name_ref()).is_some_and(|it| self.names.contains(it.text()))
            })
    }
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
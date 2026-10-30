//! E0609, E0610 and E0615: `x.f` where the type of `x` has no field `f`: no such field
//! (E0609), a primitive type, which has no fields (E0610), or a method taken as a value
//! (E0615). Also the "a field with a similar name exists" rule its siblings share: rustc's
//! `find_best_match_for_name`.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/expr.rs` (`check_expr_field`,
//! `ban_nonexisting_field`, `no_such_field_err`, `ban_take_value_of_method`,
//! `maybe_suggest_array_indexing`, `suggest_first_deref_field`, `point_at_param_definition`,
//! `get_field_candidates_considering_privacy_for_diag`,
//! `check_for_nested_field_satisfying_condition_for_diag`, `name_series_display`,
//! `expr_in_place`), `compiler/rustc_hir_typeck/src/method/mod.rs` (`suggest_method_call`),
//! `compiler/rustc_hir_typeck/src/errors.rs` (`NoFieldOnType`) and
//! `compiler/rustc_span/src/edit_distance.rs` (MIT or Apache-2.0).

use std::{cmp, mem};

use blaze_items::{Fact, ItemId};
use hir::{HasVisibility as _, InFile};
use syntax::{
    AstNode, TextRange,
    ast::{self, FieldKind},
};

use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{Found, Phase},
    model::{Applicability, Diag, Span, SuggestionStyle},
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::UnresolvedField<'_>) -> Vec<Found> {
    let node = cx.node(d.expr);
    let Some(ast::Expr::FieldExpr(field_expr)) = node.value.clone().left() else { return vec![] };
    let Some(field_range) = field_range(&field_expr) else { return vec![] };
    let Some(base) = field_expr.expr() else { return vec![] };
    let at = |range: TextRange| cx.span(InFile::new(node.file_id, range));
    let field_span = at(field_range);
    let field = d.name.display(cx.db, cx.edition).to_string();
    let receiver = &d.receiver;
    let ty = cx.ty(receiver);
    let module = cx.sema.scope(field_expr.syntax()).map(|it| it.module());

    let mut diag = if d.method_with_same_name_exists {
        take_value_of_method(cx, &field_expr, &field, &ty, field_span.clone(), &at)
    } else if !is_primitive(receiver) {
        let mut diag = no_such_field(cx, d, &field_expr, &base, &field, &ty, field_span.clone(), module, &at);
        let peeled = receiver.strip_references();
        if let Some((_, len)) = peeled.as_array(cx.db) {
            diag.span_label(field_span.clone(), "unknown field");
            if let Ok(index) = field.parse::<u64>() {
                let applicability = match (len as u64) < index {
                    true => Applicability::MachineApplicable,
                    false => Applicability::MaybeIncorrect,
                };
                let between = at(TextRange::new(base.syntax().text_range().end(), field_range.start()));
                diag.multipart_suggestion(
                    "instead of using tuple indexing, use array indexing",
                    vec![(between, "[".to_owned()), (field_span.shrink_to_hi(), "]".to_owned())],
                    applicability,
                );
            }
        } else if peeled.is_raw_ptr() {
            diag.span_label(field_span.clone(), "unknown field");
            let base_span = at(base.syntax().text_range());
            if base_span.expansion.is_none() && field_span.expansion.is_none() {
                let val = match cx.snippet(&base_span) {
                    Some(text) if text.len() < 20 => format!("`{text}`"),
                    _ => "the value".to_owned(),
                };
                let between = at(TextRange::new(base.syntax().text_range().end(), field_range.start()));
                diag.multipart_suggestion(
                    format!("{val} is a raw pointer; try dereferencing it"),
                    vec![(base_span.shrink_to_lo(), "(*".to_owned()), (between, ").".to_owned())],
                    Applicability::MaybeIncorrect,
                );
            }
        } else if let Some(param) = peeled.as_type_param(cx.db) {
            diag.span_label(field_span.clone(), "unknown field");
            if let Some(span) = type_param_span(cx, param) {
                let name = param.name(cx.db).display(cx.db, cx.edition).to_string();
                diag.span_label(span, format!("type parameter '{name}' declared here"));
            }
        } else {
            diag.span_label(field_span.clone(), "unknown field");
        }
        diag
    } else {
        let message = format!("`{ty}` is a primitive type and therefore doesn't have fields");
        let mut diag = Diag::error(Some("E0610"), message, field_span.clone());
        let unsuffixed_int = match &base {
            ast::Expr::Literal(lit) => match lit.kind() {
                ast::LiteralKind::IntNumber(int) => int.suffix().is_none(),
                _ => false,
            },
            _ => false,
        };
        if receiver.is_int_or_uint() && unsuffixed_int && at(base.syntax().text_range()).expansion.is_none() {
            if is_valid_float_suffix(&field) {
                diag.suggestion_verbose(
                    field_span.shrink_to_lo(),
                    "if intended to be a floating point literal, consider adding a `0` after the period",
                    "0",
                    Applicability::MaybeIncorrect,
                );
            } else if let Some(suffix) = partial_float_suffix(&field) {
                diag.suggestion_verbose(
                    field_span.clone(),
                    format!(
                        "if intended to be a floating point literal, consider adding a `0` after the period and a `{suffix}` suffix"
                    ),
                    format!("0{suffix}"),
                    Applicability::MaybeIncorrect,
                );
            }
        }
        diag
    };
    if let Some(adt) = receiver.strip_references().as_adt() {
        let name = adt.name(cx.db).display(cx.db, cx.edition).to_string();
        diag.fact(Fact::Signature(cx.key(ItemId::Adt(adt.into()))), format!("the fields of `{name}`"));
    }
    diag.fact_opt(cx.method_fact(receiver, d.name.as_str()), format!("what `{field}` is on `{ty}`"));
    code_fact(cx, &mut diag, d.expr.map(|it| it.syntax_node_ptr()));
    vec![Found::new(diag, Phase::Bodies)]
}

/// The range of the name or index after the `.` of `x.f`.
pub(crate) fn field_range(field_expr: &ast::FieldExpr) -> Option<TextRange> {
    Some(match field_expr.field_access()? {
        FieldKind::Name(name) => name.syntax().text_range(),
        FieldKind::Index(token) => token.text_range(),
    })
}

/// rustc's `Ty::is_primitive_ty`: `bool`, `char`, `str`, the integers and the floats.
fn is_primitive(ty: &hir::Type<'_>) -> bool {
    ty.is_scalar() || ty.is_str()
}

/// E0615 (`ban_take_value_of_method`).
fn take_value_of_method(
    cx: &Cx<'_, '_>,
    field_expr: &ast::FieldExpr,
    field: &str,
    ty: &str,
    field_span: Span,
    at: &dyn Fn(TextRange) -> Span,
) -> Diag {
    let message = format!("attempted to take value of method `{field}` on type `{ty}`");
    let mut diag = Diag::error(Some("E0615"), message, field_span.clone());
    diag.span_label(field_span, "method, not a field");
    // rustc's expression of `(x.f)` is `x.f` with the span of the parentheses.
    let mut outer = ast::Expr::FieldExpr(field_expr.clone());
    while let Some(paren) = outer.syntax().parent().and_then(ast::ParenExpr::cast) {
        outer = ast::Expr::ParenExpr(paren);
    }
    let is_wrapped = matches!(outer, ast::Expr::ParenExpr(_));
    let range = outer.syntax().text_range();
    let is_call = outer
        .syntax()
        .parent()
        .and_then(ast::CallExpr::cast)
        .and_then(|call| call.expr())
        .is_some_and(|callee| callee.syntax() == outer.syntax());
    let one = syntax::TextSize::from(1);
    if is_call && is_wrapped {
        diag.multipart_suggestion(
            "remove wrapping parentheses to call the method",
            vec![
                (at(TextRange::new(range.start(), range.start() + one)), String::new()),
                (at(TextRange::new(range.end() - one, range.end())), String::new()),
            ],
            Applicability::MachineApplicable,
        );
    } else if !in_place(&outer) {
        let span = match is_wrapped {
            true => at(TextRange::new(range.start() + one, range.end() - one)),
            false => at(range),
        };
        let params = method_params(cx, field_expr, field).unwrap_or(0);
        suggest_method_call(&mut diag, "use parentheses to call the method", params, span);
    } else if let Some(base) = field_expr.expr()
        && let Some(receiver) = cx.sema.type_of_expr(&base).map(|it| it.original)
        && let Some((pointee, _)) = receiver.as_raw_ptr()
        && pointee.as_adt().is_some_and(|adt| !matches!(adt, hir::Adt::Enum(_)))
        && pointee.fields(cx.db).iter().any(|(it, _)| it.name(cx.db).as_str() == field)
    {
        let base = at(base.syntax().text_range());
        diag.multipart_suggestion(
            "to access the field, dereference first",
            vec![(base.shrink_to_lo(), "(*".to_owned()), (base.shrink_to_hi(), ")".to_owned())],
            Applicability::MaybeIncorrect,
        );
    } else {
        diag.help("methods are immutable and cannot be assigned to");
    }
    diag
}

/// How many arguments besides `self` the method `name` the receiver of `x.name` would call
/// takes.
fn method_params(cx: &Cx<'_, '_>, field_expr: &ast::FieldExpr, name: &str) -> Option<usize> {
    let base = field_expr.expr()?;
    let receiver = cx.sema.type_of_expr(&base)?.original;
    let scope = cx.sema.scope(field_expr.syntax())?;
    let name = hir::Name::new_symbol_root(hir::Symbol::intern(name));
    receiver.iterate_method_candidates(cx.db, &scope, Some(&name), |f| Some(f.params_without_self(cx.db).len()))
}

/// rustc's `suggest_method_call`: `()` after `span`, with a `_` for each argument.
pub(crate) fn suggest_method_call(diag: &mut Diag, message: &str, params: usize, span: Span) {
    let code = format!("({})", vec!["_"; params].join(", "));
    let applicability = match params > 0 {
        true => Applicability::HasPlaceholders,
        false => Applicability::MaybeIncorrect,
    };
    diag.suggestion_verbose(span.shrink_to_hi(), message, code, applicability);
}

/// rustc's `expr_in_place`: whether `expr` is in the left side of an assignment.
pub(crate) fn in_place(expr: &ast::Expr) -> bool {
    let mut node = expr.syntax().clone();
    while let Some(parent) = node.parent().and_then(ast::Expr::cast) {
        if let ast::Expr::BinExpr(bin) = &parent
            && matches!(bin.op_kind(), Some(ast::BinaryOp::Assignment { .. }))
            && bin.lhs().is_some_and(|lhs| lhs.syntax() == &node)
        {
            return true;
        }
        node = parent.syntax().clone();
    }
    false
}

/// E0609 (`no_such_field_err`), without the label `ban_nonexisting_field` adds.
#[allow(clippy::too_many_arguments)]
fn no_such_field(
    cx: &Cx<'_, '_>,
    d: &hir::UnresolvedField<'_>,
    field_expr: &ast::FieldExpr,
    base: &ast::Expr,
    field: &str,
    ty: &str,
    field_span: Span,
    module: Option<hir::Module>,
    at: &dyn Fn(TextRange) -> Span,
) -> Diag {
    let message = format!("no field `{field}` on type `{ty}`");
    let mut diag = Diag::error(Some("E0609"), message, field_span.clone());
    let peeled = d.receiver.strip_references();
    let base_range = base.syntax().text_range();
    if peeled.as_adt().is_some() {
        let found = peeled.iterate_assoc_items(cx.db, |item| match item {
            hir::AssocItem::Function(f)
                if f.name(cx.db) == d.name
                    && !f.has_self_param(cx.db)
                    && matches!(item.container(cx.db), hir::AssocItemContainer::Impl(it) if it.trait_(cx.db).is_none()) =>
            {
                Some(f)
            }
            _ => None,
        });
        if let Some(f) = found {
            diag.span_label(field_span.clone(), "this is an associated function, not a method");
            diag.note(
                "found the following associated function; to be used as method, it must have a `self` parameter",
            );
            if let Some(span) = fn_def_span(cx, f)
                && let Some(impl_ty) = hir::AssocItem::Function(f).implementing_ty(cx.db)
            {
                let impl_ty = cx.ty(&impl_ty);
                diag.span_note(span, format!("the candidate is defined in an impl for the type `{impl_ty}`"));
            }
            diag.multipart_suggestion(
                "use associated function syntax instead",
                vec![
                    (at(base_range), cx.ty(&peeled)),
                    (at(TextRange::new(base_range.end(), field_expr_field_start(field_expr))), "::".to_owned()),
                ],
                Applicability::MaybeIncorrect,
            );
            return diag;
        }
    }

    let (ty, unwrap) = match option_or_result_arg(cx, &d.receiver) {
        Some(arg) => (arg, "unwrap()."),
        None => (d.receiver.clone(), ""),
    };
    let same_context = at(field_expr.syntax().text_range()).expansion == field_span.expansion;
    for found in field_candidates(cx, &ty, module) {
        let names: Vec<String> = found.iter().map(|(name, _)| name.clone()).collect();
        let mut paths: Vec<String> = found
            .into_iter()
            .filter_map(|candidate| nested_field(cx, field, candidate, Vec::new(), module))
            .map(|mut path| {
                path.pop();
                path.iter().map(|it| format!("{it}.")).collect::<String>()
            })
            .collect();
        paths.sort();
        let similar = find_best_match_for_name(&names, field, None);
        if !paths.is_empty() && same_context {
            let (some, have) = match paths.len() > 1 {
                true => ("some", "have"),
                false => ("one", "has"),
            };
            diag.suggestions(
                field_span.shrink_to_lo(),
                format!("{some} of the expressions' fields {have} a field of the same name"),
                paths.iter().map(|path| format!("{unwrap}{path}")),
                Applicability::MaybeIncorrect,
                SuggestionStyle::ShowCode,
            );
        } else if let Some(similar) = similar
            && !(field.parse::<usize>().is_ok() && similar.parse::<usize>().is_ok())
        {
            diag.suggestion_verbose(
                field_span.clone(),
                "a field with a similar name exists",
                format!("{unwrap}{similar}"),
                Applicability::MaybeIncorrect,
            );
        } else if !names.is_empty() {
            let is = if names.len() == 1 { " is" } else { "s are" };
            diag.note(format!("available field{is}: {}", name_series_display(&names)));
        }
    }
    diag
}

fn field_expr_field_start(field_expr: &ast::FieldExpr) -> syntax::TextSize {
    field_range(field_expr).map_or(field_expr.syntax().text_range().end(), |it| it.start())
}

/// The type argument of an `Option` or a `Result`, which rustc looks for a field in behind an
/// `unwrap()`.
fn option_or_result_arg<'db>(cx: &Cx<'_, 'db>, ty: &hir::Type<'db>) -> Option<hir::Type<'db>> {
    let (adt, args) = ty.as_adt_with_args()?;
    let hir::Adt::Enum(it) = adt else { return None };
    let items = hir_def::lang_item::lang_items(cx.db, cx.krate.base());
    let option = items.Option.map(hir::Enum::from);
    let result = items.ResultOk.map(|it| hir::EnumVariant::from(it).parent_enum(cx.db));
    if option != Some(it) && result != Some(it) {
        return None;
    }
    args.into_iter().next().flatten()
}

/// rustc's `get_field_candidates_considering_privacy_for_diag`: for each type `ty` derefs to
/// (itself first) that is a struct, a union or a tuple, its fields visible from `module`, when
/// it has any.
fn field_candidates<'db>(
    cx: &Cx<'_, 'db>,
    ty: &hir::Type<'db>,
    module: Option<hir::Module>,
) -> Vec<Vec<(String, hir::Type<'db>)>> {
    let visible = |field: &hir::Field| module.is_none_or(|module| field.is_visible_from(cx.db, module));
    ty.autoderef(cx.db)
        .filter_map(|ty| {
            if ty.is_tuple() {
                let fields = ty.tuple_fields(cx.db).into_iter().take(100);
                return Some(fields.enumerate().map(|(i, ty)| (i.to_string(), ty)).collect());
            }
            if !matches!(ty.as_adt()?, hir::Adt::Struct(_) | hir::Adt::Union(_)) {
                return None;
            }
            let fields = ty.fields(cx.db);
            if fields.iter().all(|(field, _)| !visible(field)) {
                return None;
            }
            let fields = fields.into_iter().filter(|(field, _)| visible(field)).take(100);
            Some(fields.map(|(field, ty)| (field.name(cx.db).as_str().to_owned(), ty)).collect())
        })
        .collect()
}

/// rustc's `check_for_nested_field_satisfying_condition_for_diag`: the path of fields, at most
/// three deep below `candidate`, to a field named `field`.
fn nested_field<'db>(
    cx: &Cx<'_, 'db>,
    field: &str,
    (name, ty): (String, hir::Type<'db>),
    mut path: Vec<String>,
    module: Option<hir::Module>,
) -> Option<Vec<String>> {
    if path.len() > 3 {
        return None;
    }
    let matches = name == field;
    path.push(name);
    if matches {
        return Some(path);
    }
    for nested in field_candidates(cx, &ty, module) {
        for candidate in nested {
            if let Some(path) = nested_field(cx, field, candidate, path.clone(), module) {
                return Some(path);
            }
        }
    }
    None
}

/// The span of a type parameter's name, as rustc points at its declaration; none for
/// `impl Trait` in argument position.
pub(crate) fn type_param_span(cx: &Cx<'_, '_>, param: hir::TypeParam) -> Option<Span> {
    if param.is_implicit(cx.db) {
        return None;
    }
    let source = cx.sema.source(param.merge())?;
    let name = match &source.value {
        either::Either::Left(ast::TypeOrConstParam::Type(it)) => it.name()?,
        _ => return None,
    };
    Some(cx.span(InFile::new(source.file_id, name.syntax().text_range())))
}

/// rustc's `def_span` of a function: its signature.
pub(crate) fn fn_def_span(cx: &Cx<'_, '_>, f: hir::Function) -> Option<Span> {
    let source = cx.sema.source(f)?;
    Some(cx.span(InFile::new(source.file_id, crate::cx::def_range(source.value.syntax()))))
}

fn is_valid_float_suffix(field: &str) -> bool {
    if field == "f32" || field == "f64" {
        return true;
    }
    let mut chars = field.chars().peekable();
    if let Some('e' | 'E') = chars.peek() {
        chars.next();
        if let Some(c) = chars.peek()
            && !c.is_numeric()
            && *c != '-'
            && *c != '+'
        {
            return false;
        }
        while let Some(c) = chars.peek() {
            if !c.is_numeric() {
                break;
            }
            chars.next();
        }
    }
    let suffix = chars.collect::<String>();
    suffix.is_empty() || suffix == "f32" || suffix == "f64"
}

fn partial_float_suffix(field: &str) -> Option<&'static str> {
    let lower = field.to_lowercase();
    let matches = !field.is_empty()
        && lower.starts_with(['f', 'l'])
        && field[1..].chars().all(|c| c.is_ascii_digit());
    match matches {
        true if lower.starts_with('f') => Some("f32"),
        true => Some("f64"),
        false => None,
    }
}

/// rustc's `name_series_display`: the first five names (six when there are six), then how
/// many others there are.
pub(crate) fn name_series_display(names: &[String]) -> String {
    let limit = if names.len() == 6 { 6 } else { 5 };
    let mut display =
        names.iter().take(limit).map(|it| format!("`{it}`")).collect::<Vec<_>>().join(", ");
    if names.len() > limit {
        display = format!("{} ... and {} others", display, names.len() - limit);
    }
    display
}

/// rustc's `find_best_match_for_name`: a candidate equal to `lookup` but for case, else the
/// closest by [`edit_distance`] within `dist` (a third of `lookup`'s length, at least one, by
/// default), else one with the same `_`-separated words in another order.
pub(crate) fn find_best_match_for_name(
    candidates: &[String],
    lookup: &str,
    dist: Option<usize>,
) -> Option<String> {
    let lookup_uppercase = lookup.to_uppercase();
    if let Some(c) = candidates.iter().find(|c| c.to_uppercase() == lookup_uppercase) {
        return Some(c.clone());
    }
    let lookup_len = lookup.chars().count();
    let mut dist = dist.unwrap_or_else(|| cmp::max(lookup_len, 3) / 3);
    let mut best = None;
    for c in candidates {
        match edit_distance(lookup, c, dist) {
            Some(0) => return Some(c.clone()),
            Some(d) => {
                dist = d - 1;
                best = Some(c.clone());
            }
            None => {}
        }
    }
    if best.is_some() {
        return best;
    }
    let lookup_words = sort_by_words(lookup);
    candidates.iter().fold(None, |result, candidate| match sort_by_words(candidate) == lookup_words {
        true => Some(candidate.clone()),
        false => result,
    })
}

fn sort_by_words(name: &str) -> Vec<&str> {
    let mut words: Vec<&str> = name.split('_').collect();
    words.sort_unstable();
    words
}

/// rustc's restricted Damerau-Levenshtein distance of `a` and `b`, or `None` if it is over
/// `limit`.
pub(crate) fn edit_distance(a: &str, b: &str, limit: usize) -> Option<usize> {
    let a = a.chars().collect::<Vec<_>>();
    let b = b.chars().collect::<Vec<_>>();
    let mut a = &a[..];
    let mut b = &b[..];
    if a.len() < b.len() {
        mem::swap(&mut a, &mut b);
    }
    let min_dist = a.len() - b.len();
    if min_dist > limit {
        return None;
    }
    while let Some(((b_char, b_rest), (a_char, a_rest))) = b.split_first().zip(a.split_first())
        && a_char == b_char
    {
        a = a_rest;
        b = b_rest;
    }
    while let Some(((b_char, b_rest), (a_char, a_rest))) = b.split_last().zip(a.split_last())
        && a_char == b_char
    {
        a = a_rest;
        b = b_rest;
    }
    if b.is_empty() {
        return Some(min_dist);
    }
    let mut prev_prev = vec![usize::MAX; b.len() + 1];
    let mut prev = (0..=b.len()).collect::<Vec<_>>();
    let mut current = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        current[0] = i;
        let a_idx = i - 1;
        for j in 1..=b.len() {
            let b_idx = j - 1;
            let substitution_cost = if a[a_idx] == b[b_idx] { 0 } else { 1 };
            current[j] =
                cmp::min(prev[j] + 1, cmp::min(current[j - 1] + 1, prev[j - 1] + substitution_cost));
            if (i > 1) && (j > 1) && (a[a_idx] == b[b_idx - 1]) && (a[a_idx - 1] == b[b_idx]) {
                current[j] = cmp::min(current[j], prev_prev[j - 2] + 1);
            }
        }
        [prev_prev, prev, current] = [prev, current, prev_prev];
    }
    let distance = prev[b.len()];
    (distance <= limit).then_some(distance)
}

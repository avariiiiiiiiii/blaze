//! An `as` rustc rejects, each `hir::CastError` as rustc words it: E0606 "casting `&u8` as
//! `u8` is invalid" (with the dereference or borrow to remove, the pointer or integer to cast
//! through, and why two pointers' metadata differ), E0054 "cannot cast `i32` as `bool`" (compare
//! with zero), E0604 "only `u8` can be cast as `char`" (`char::from_u32`), E0605
//! "non-primitive cast" (the borrow or `From` to use), E0607 a thin to a wide pointer, E0606 an
//! integer to a wide pointer, and E0804 an auto trait added to a `dyn` through a pointer cast.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/cast.rs` (`report_cast_error`,
//! `try_suggest_collection_to_bool`) and `diagnostics.rs` (`CannotCastToBool`, `IntToWide`,
//! `CastThinPointerToWidePointer`, `PtrCastAddAutoToObject`) (MIT or Apache-2.0).

use hir::{CastError, InFile};
use hir_ty::next_solver::TyKind;
use intern::{Symbol, sym};
use rustc_type_ir::inherent::{BoundExistentialPredicates as _, IntoKind as _};
use syntax::{AstNode, TextRange, ast};

use super::binary_operator_cannot_be_applied::{adt_def_span, code_fact};
use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::{Applicability, Diag, Span},
};

/// The parts of the cast rustc's spans name: the whole `x as T`, `x` and `T`.
struct Spans {
    span: Span,
    expr: Span,
    cast: Span,
}

pub(crate) fn lower<'db>(cx: &Cx<'_, 'db>, d: &hir::InvalidCast<'db>) -> Vec<Found> {
    let node = cx.node(d.expr);
    let file = node.file_id;
    let at = |range: TextRange| cx.span(InFile::new(file, range));
    let span = cx.span_of_ptr(d.expr);
    let cast = match node.value.clone().left() {
        Some(ast::Expr::CastExpr(it)) => Some(it),
        _ => None,
    };
    let operand = cast.as_ref().and_then(|it| it.expr());
    let spans = Spans {
        expr: operand.as_ref().map_or_else(|| span.clone(), |it| at(it.syntax().text_range())),
        cast: cast
            .as_ref()
            .and_then(|it| it.ty())
            .map_or_else(|| span.clone(), |it| at(it.syntax().text_range())),
        span,
    };
    let e = cx.ty(&d.expr_ty);
    let c = cx.ty(&d.cast_ty);
    let invalid = || Diag::error(Some("E0606"), format!("casting `{e}` as `{c}` is invalid"), spans.span.clone());
    let operand_is_borrow = matches!(operand, Some(ast::Expr::RefExpr(_)));

    let mut diag = match d.error {
        CastError::NeedDeref => {
            let mut diag = invalid();
            match &operand {
                Some(ast::Expr::RefExpr(borrow)) => {
                    let inner = peel_borrows(borrow);
                    let start = borrow.syntax().text_range().start();
                    diag.suggestion_verbose(
                        at(TextRange::new(start, inner.syntax().text_range().start())),
                        "remove the unneeded borrow",
                        "",
                        Applicability::MachineApplicable,
                    );
                }
                _ => {
                    diag.suggestion_verbose(
                        spans.expr.shrink_to_lo(),
                        "dereference the expression",
                        "*",
                        Applicability::MachineApplicable,
                    );
                }
            }
            diag
        }
        CastError::NeedViaPtr | CastError::NeedViaThinPtr => {
            let mut diag = invalid();
            if d.cast_ty.is_int_or_uint() {
                let enum_ = (!operand_is_borrow)
                    .then(|| d.expr_ty.as_reference())
                    .flatten()
                    .and_then(|(inner, _)| match inner.as_adt() {
                        Some(hir::Adt::Enum(it)) if payload_free(cx, it) => Some((it, inner)),
                        _ => None,
                    });
                if let Some((enum_, inner)) = enum_ {
                    diag.suggestion_verbose(
                        spans.expr.shrink_to_lo(),
                        "try dereferencing before the cast",
                        "*",
                        Applicability::MaybeIncorrect,
                    );
                    if !inner.is_copy(cx.db)
                        && let Some(def) = adt_def_span(cx, hir::Adt::Enum(enum_))
                    {
                        diag.suggestion_verbose(
                            def.shrink_to_lo(),
                            "add `#[derive(Copy, Clone)]` to the enum definition",
                            "#[derive(Copy, Clone)]\n",
                            Applicability::MaybeIncorrect,
                        );
                    }
                } else {
                    let through = match d.error {
                        CastError::NeedViaPtr => "a raw pointer",
                        _ => "a thin pointer",
                    };
                    diag.help(format!("cast through {through} first"));
                }
            }
            collection_to_bool(cx, d, &spans, &mut diag);
            diag
        }
        CastError::NeedViaInt => {
            let mut diag = invalid();
            diag.help("cast through an integer first");
            diag
        }
        CastError::IllegalCast | CastError::Unknown => invalid(),
        CastError::DifferingKinds => {
            let mut diag = invalid();
            let src = pointee(&d.expr_ty).map(|it| metadata(cx, &it));
            let dst = pointee(&d.cast_ty).map(|it| metadata(cx, &it));
            use Metadata::*;
            let note = match (src, dst) {
                (Some(VTable), Some(VTable)) => Some("the trait objects may have different vtables"),
                (Some(Param | Alias), Some(Param | Alias | VTable | Length))
                | (Some(VTable | Length), Some(Param | Alias)) => {
                    Some("the pointers may have different metadata")
                }
                (Some(VTable), Some(Length)) | (Some(Length), Some(VTable)) => {
                    Some("the pointers have different metadata")
                }
                _ => None,
            };
            if let Some(note) = note {
                diag.note(note);
            }
            diag
        }
        CastError::CastToBool => {
            let mut diag = Diag::error(Some("E0054"), format!("cannot cast `{e}` as `bool`"), spans.span.clone());
            if is_numeric(&d.expr_ty) {
                diag.suggestion_verbose(
                    spans.expr.shrink_to_hi().with_hi(spans.span.hi),
                    "compare with zero instead",
                    " != 0",
                    Applicability::MachineApplicable,
                );
            } else {
                diag.span_label(spans.span.clone(), "unsupported cast");
            }
            diag
        }
        CastError::CastToChar => {
            let mut diag = Diag::error(
                Some("E0604"),
                format!("only `u8` can be cast as `char`, not `{e}`"),
                spans.span.clone(),
            );
            diag.span_label(spans.span.clone(), "invalid cast");
            if is_numeric(&d.expr_ty) {
                let builtin = d.expr_ty.as_builtin().map(|it| it.name().as_str().to_owned());
                match builtin.as_deref() {
                    Some("u32") => {
                        diag.multipart_suggestion(
                            "consider using `char::from_u32` instead",
                            vec![
                                (spans.expr.shrink_to_lo(), "char::from_u32(".to_owned()),
                                (spans.expr.shrink_to_hi().to(&spans.cast), ")".to_owned()),
                            ],
                            Applicability::MachineApplicable,
                        );
                    }
                    Some("i8") => {
                        diag.span_help(spans.span.clone(), "consider casting from `u8` instead");
                    }
                    _ => {
                        diag.span_help(
                            spans.span.clone(),
                            "consider using `char::from_u32` instead (via a `u32`)",
                        );
                    }
                }
            }
            diag
        }
        CastError::NonScalar => non_scalar(cx, d, &spans, &operand, &e, &c),
        CastError::SizedUnsizedCast => Diag::error(
            Some("E0607"),
            format!("cannot cast thin pointer `{e}` to wide pointer `{c}`"),
            spans.span.clone(),
        ),
        CastError::IntToWideCast => {
            let kind = pointee(&d.cast_ty).map(|it| metadata(cx, &it));
            let known = match kind {
                Some(Metadata::VTable) => Some("a vtable"),
                Some(Metadata::Length) => Some("a length"),
                _ => None,
            };
            let wide = if known.is_some() { "is" } else { "may be" };
            let mut diag = Diag::error(
                Some("E0606"),
                format!("cannot cast `{e}` to a pointer that {wide} wide"),
                spans.cast.clone(),
            );
            diag.span_label(
                spans.cast.clone(),
                format!(
                    "creating a `{c}` requires both an address and {}",
                    known.unwrap_or("type-specific metadata")
                ),
            );
            diag.span_label(
                spans.expr.clone(),
                "consider casting this expression to `*const ()`, then using `core::ptr::from_raw_parts`",
            );
            if known.is_none()
                && let Some((pointee, _)) = d.cast_ty.as_raw_ptr()
                && let Some(param) = pointee.as_type_param(cx.db)
            {
                diag.note(format!(
                    "the type parameter `{}` is not known to be `Sized`, so this pointer may be wide",
                    param.name(cx.db).display(cx.db, cx.edition)
                ));
            }
            diag
        }
        CastError::PtrPtrAddingAutoTraits => {
            let mut traits = added_auto_traits(cx, d);
            traits.sort();
            let list = match traits.as_slice() {
                [one] => format!("auto trait {one}"),
                _ => format!("auto traits {}", symbol_list(&traits)),
            };
            let mut diag = Diag::error(
                Some("E0804"),
                format!("cannot add {list} to dyn bound via pointer cast"),
                spans.span.clone(),
            );
            diag.span_label(spans.span.clone(), "unsupported cast");
            diag.note("this could allow UB elsewhere");
            diag.help("use `transmute` if you're sure this is sound");
            diag
        }
    };
    code_fact(cx, d.expr.map(|it| it.syntax_node_ptr()), &mut diag);
    vec![Found::new(diag, Phase::Bodies)]
}

/// rustc's `CastError::NonScalar`: E0605, with the borrow or `From` to use instead.
fn non_scalar<'db>(
    cx: &Cx<'_, 'db>,
    d: &hir::InvalidCast<'db>,
    spans: &Spans,
    operand: &Option<ast::Expr>,
    e: &str,
    c: &str,
) -> Diag {
    let mut diag = Diag::error(Some("E0605"), format!("non-primitive cast: `{e}` as `{c}`"), spans.span.clone());
    if let Some(ast::Expr::RefExpr(_)) = operand
        && let Some(snippet) = cx.snippet(&spans.expr)
    {
        diag.note(format!("casting reference expression `{snippet}` because `&` binds tighter than `as`"));
    }

    let prefix = |it: hir::Mutability| match it {
        hir::Mutability::Mut => "mut ",
        hir::Mutability::Shared => "",
    };
    let coerces = |from: &hir::Type<'db>, mutability| {
        from.add_reference(cx.db, mutability).could_coerce_to(cx.db, &d.cast_ty)
    };
    let mut sugg: Option<(String, bool)> = None;
    let mut sugg_mutref = false;
    if let Some((cast_inner, mutability)) = d.cast_ty.as_reference() {
        if let Some((pointee, _)) = d.expr_ty.as_raw_ptr()
            && coerces(&pointee, mutability)
        {
            sugg = Some((format!("&{}*", prefix(mutability)), cx.ty(&cast_inner) == cx.ty(&pointee)));
        } else if let Some((expr_inner, hir::Mutability::Shared)) = d.expr_ty.as_reference()
            && mutability == hir::Mutability::Mut
            && coerces(&expr_inner, hir::Mutability::Mut)
        {
            sugg_mutref = true;
        }
        if !sugg_mutref && sugg.is_none() && coerces(&d.expr_ty, mutability) {
            sugg = Some((format!("&{}", prefix(mutability)), false));
        }
    } else if let Some((_, mutability)) = d.cast_ty.as_raw_ptr()
        && coerces(&d.expr_ty, mutability)
    {
        sugg = Some((format!("&{}", prefix(mutability)), false));
    }

    if sugg_mutref {
        diag.span_label(spans.span.clone(), "invalid cast");
        diag.span_note(spans.expr.clone(), "this reference is immutable");
        diag.span_note(spans.cast.clone(), "trying to cast to a mutable reference type");
    } else if let Some((code, remove_cast)) = sugg {
        diag.span_label(spans.span.clone(), "invalid cast");
        let has_parens = cx.snippet(&spans.expr).is_some_and(|it| it.starts_with('('));
        let needs_parens = !has_parens && matches!(operand, Some(ast::Expr::CastExpr(_)));
        let mut parts = vec![(spans.expr.shrink_to_lo(), code)];
        if needs_parens {
            parts[0].1 += "(";
            parts.push((spans.expr.shrink_to_hi(), ")".to_owned()));
        }
        if remove_cast {
            parts.push((spans.expr.shrink_to_hi().to(&spans.cast), String::new()));
        }
        diag.multipart_suggestion("consider borrowing the value", parts, Applicability::MachineApplicable);
    } else if !(d.cast_ty.is_fn() || d.cast_ty.is_closure()) {
        if let Some(from) = core_trait(cx, sym::convert, sym::From)
            && d.cast_ty.impls_trait(cx.db, from, std::slice::from_ref(&d.expr_ty))
        {
            let to = match d.cast_ty.as_adt() {
                Some(adt) => {
                    let args: Vec<String> = d.cast_ty.type_arguments().map(|it| cx.ty(&it)).collect();
                    let path = cx.def_path(hir::ModuleDef::Adt(adt));
                    if args.is_empty() { path } else { format!("{path}::<{}>", args.join(", ")) }
                }
                None => c.to_owned(),
            };
            diag.multipart_suggestion(
                "consider using the `From` trait instead",
                vec![
                    (spans.expr.shrink_to_lo(), format!("{to}::from(")),
                    (spans.expr.shrink_to_hi().to(&spans.cast), ")".to_owned()),
                ],
                Applicability::MaybeIncorrect,
            );
        }
        if matches!(d.expr_ty.as_adt(), Some(hir::Adt::Enum(_))) && is_numeric(&d.cast_ty) {
            diag.span_label(
                spans.span.clone(),
                "an `as` expression can be used to convert enum types to numeric types only if the enum type is unit-only or field-less",
            );
            diag.note("see https://doc.rust-lang.org/reference/items/enumerations.html#casting for more information");
        } else {
            diag.span_label(
                spans.span.clone(),
                "an `as` expression can only be used to convert between primitive types or to coerce to a specific trait object",
            );
        }
    } else {
        diag.span_label(spans.span.clone(), "invalid cast");
    }
    collection_to_bool(cx, d, spans, &mut diag);
    diag
}

/// rustc's `try_suggest_collection_to_bool`: `s as bool` of a string or a slice.
fn collection_to_bool<'db>(cx: &Cx<'_, 'db>, d: &hir::InvalidCast<'db>, spans: &Spans, diag: &mut Diag) {
    if !d.cast_ty.is_bool() {
        return;
    }
    let Some(deref) = d.expr_ty.autoderef(cx.db).find(|it| it.is_str() || it.is_slice()) else { return };
    let deref_ty = cx.ty(&deref);
    if deref_ty != cx.ty(&d.expr_ty.strip_references()) {
        diag.span_note(
            spans.expr.clone(),
            format!("this expression `Deref`s to `{deref_ty}` which implements `is_empty`"),
        );
    }
    diag.multipart_suggestion(
        format!(
            "consider using the `is_empty` method on `{}` to determine if it contains anything",
            cx.ty(&d.expr_ty)
        ),
        vec![
            (spans.expr.shrink_to_lo(), "!".to_owned()),
            (spans.span.with_lo(spans.expr.hi), ".is_empty()".to_owned()),
        ],
        Applicability::MaybeIncorrect,
    );
}

/// rustc's `Expr::peel_borrows`.
fn peel_borrows(borrow: &ast::RefExpr) -> ast::Expr {
    let mut inner = ast::Expr::RefExpr(borrow.clone());
    while let ast::Expr::RefExpr(it) = &inner
        && let Some(next) = it.expr()
    {
        inner = next;
    }
    inner
}

/// rustc's `AdtDef::is_payloadfree`: no variant has fields.
fn payload_free(cx: &Cx<'_, '_>, enum_: hir::Enum) -> bool {
    enum_.variants(cx.db).iter().all(|it| it.fields(cx.db).is_empty())
}

fn is_numeric(ty: &hir::Type<'_>) -> bool {
    ty.is_int_or_uint() || ty.is_float()
}

/// The type a raw pointer or a reference points to.
fn pointee<'db>(ty: &hir::Type<'db>) -> Option<hir::Type<'db>> {
    ty.as_raw_ptr().or_else(|| ty.as_reference()).map(|(it, _)| it)
}

/// rustc's `PointerKind` of a pointer to a type: the metadata it carries.
#[derive(Clone, Copy)]
enum Metadata {
    Thin,
    VTable,
    Length,
    Param,
    Alias,
}

fn metadata<'db>(cx: &Cx<'_, 'db>, ty: &hir::Type<'db>) -> Metadata {
    if matches!(ty.to_next_solver().kind(), TyKind::Dynamic(..)) {
        return Metadata::VTable;
    }
    if ty.is_slice() || ty.is_str() {
        return Metadata::Length;
    }
    let sized = hir::Trait::lang(cx.db, cx.krate, hir::LangItem::Sized)
        .is_some_and(|it| ty.impls_trait(cx.db, it, &[]));
    if sized {
        return Metadata::Thin;
    }
    if ty.as_type_param(cx.db).is_some() {
        return Metadata::Param;
    }
    if let Some(hir::Adt::Struct(_)) = ty.as_adt() {
        return match ty.fields(cx.db).pop() {
            Some((_, last)) => metadata(cx, &last),
            None => Metadata::Thin,
        };
    }
    if ty.is_tuple() {
        return match ty.tuple_fields(cx.db).pop() {
            Some(last) => metadata(cx, &last),
            None => Metadata::Thin,
        };
    }
    Metadata::Alias
}

/// The auto traits the `dyn` the cast points to has and the `dyn` it casts from neither has nor
/// implies through its principal's supertraits.
fn added_auto_traits<'db>(cx: &Cx<'_, 'db>, d: &hir::InvalidCast<'db>) -> Vec<String> {
    let autos = |ty: &hir::Type<'db>| -> Vec<hir::Trait> {
        match pointee(ty).map(|it| it.to_next_solver().kind()) {
            Some(TyKind::Dynamic(preds, _)) => {
                preds.auto_traits().into_iter().map(|it| hir::Trait::from(hir_def::TraitId::from(it))).collect()
            }
            _ => Vec::new(),
        }
    };
    let mut src = autos(&d.expr_ty);
    if let Some(principal) = pointee(&d.expr_ty).and_then(|it| it.as_dyn_trait()) {
        src.extend(principal.all_supertraits(cx.db).into_iter().filter(|it| it.is_auto(cx.db)));
    }
    autos(&d.cast_ty)
        .into_iter()
        .filter(|it| !src.contains(it))
        .map(|it| format!("`{}`", cx.def_path(hir::ModuleDef::Trait(it))))
        .collect()
}

/// rustc's `DiagSymbolList`, as its English list formatter joins it: `a and b`, `a, b, and c`.
fn symbol_list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [first, second] => format!("{first} and {second}"),
        [init @ .., last] => format!("{}, and {last}", init.join(", ")),
    }
}

/// The trait `core::{module}::{name}`, as rustc's diagnostic items find it: core is the crate of
/// the `Sized` lang item, and the path is two hashed scope lookups of interned names.
fn core_trait(cx: &Cx<'_, '_>, module: Symbol, name: Symbol) -> Option<hir::Trait> {
    let sized = hir_def::lang_item::lang_items(cx.db, cx.krate.base()).Sized?;
    let core = hir::Trait::from(sized).module(cx.db).krate(cx.db);
    let path = [hir::Name::new_symbol_root(module), hir::Name::new_symbol_root(name)];
    core.root_module(cx.db).resolve_mod_path(cx.db, path)?.find_map(|it| match it {
        hir::ItemInNs::Types(hir::ModuleDef::Trait(it)) => Some(it),
        _ => None,
    })
}

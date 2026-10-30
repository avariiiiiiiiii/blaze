//! The checks of impls of the builtin traits that hir does not make: `Copy` of a type that
//! cannot be (E0206), with a field that is not `Copy` (E0204) or with a destructor (E0184);
//! `Drop` of what is not a local struct, enum or union (E0120), a negative `Drop` impl, and a
//! `Drop` impl of an instantiation of its type rather than all of it (E0366).
//!
//! A field of an enum whose type has the enum's parameters in it, other than one of them
//! alone, is the front end's to type in the impl's environment (`hir::Type::fields` gives
//! only a struct's or a union's); asked of it in `v2/requests/diagnostics.md`
//! (`diagnostics-items-impls`), as are the notes of a field whose own `Copy` impl wants more
//! than the impl gives (E0204's `ImplForTyRequires`) and E0367.
//!
//! Follows rustc's `compiler/rustc_hir_analysis/src/coherence/builtin.rs`
//! (`visit_implementation_of_copy`, `visit_implementation_of_drop`, `infringing_fields_error`),
//! `compiler/rustc_trait_selection/src/traits/misc.rs` (`type_allowed_to_implement_copy`,
//! `all_fields_implement_trait`), `compiler/rustc_hir_analysis/src/check/always_applicable.rs`
//! (`check_drop_impl`, `ensure_impl_params_and_item_params_correspond`),
//! `compiler/rustc_middle/src/ty/diagnostics.rs` (`suggest_constraining_type_params`) and the
//! structs `TraitCannotImplForTy`, `CopyImplOnNonAdt`, `CopyImplOnTypeWithDtor`,
//! `DropImplOnWrongItem` and `DropImplPolarity` of `compiler/rustc_hir_analysis/src/
//! diagnostics.rs` (MIT or Apache-2.0).

use blaze_items::{Fact, ItemId};
use either::Either;
use hir::HasSource as _;
use hir_def::lang_item::{LangItems, lang_items};
use rustc_hash::FxHashSet;
use syntax::{
    AstNode as _,
    ast::{self, HasGenericArgs as _, HasGenericParams as _, HasName as _, HasTypeBounds as _},
};

use super::Origin;
use crate::{
    cx::{Cx, def_range, item_range},
    lower::{Found, Phase, incoherent_impl::{ImplAt, assoc_def_span}},
    model::{Applicability, Diag, Span},
};

/// The builtin traits' checks of a crate's impls, with what they share across impls.
pub(super) struct Builtin<'a, 'b, 'db> {
    cx: &'a Cx<'b, 'db>,
    lang: LangItems,
    /// The `Drop` impls of each type, found once.
    destructors: Option<Vec<(hir::Adt, hir::Impl)>>,
}

impl<'a, 'b, 'db> Builtin<'a, 'b, 'db> {
    pub(super) fn new(cx: &'a Cx<'b, 'db>) -> Self {
        Builtin { cx, lang: lang_items(cx.db, cx.krate.base()), destructors: None }
    }

    /// rustc's `builtin::check_trait` of one impl of `trait_`.
    pub(super) fn check(&mut self, impl_: hir::Impl, origin: &Origin, trait_: hir::Trait) -> Vec<Found> {
        let id = hir_def::TraitId::from(trait_);
        let self_ty = impl_.self_ty(self.cx.db);
        // rustc skips an impl whose header has an error in it (`error_reported`).
        if self_ty.contains_unknown() {
            return Vec::new();
        }
        if Some(id) == self.lang.Copy {
            self.copy(impl_, origin, &self_ty)
        } else if Some(id) == self.lang.Drop {
            match origin.written() {
                Some(at) => drop_impl(self.cx, at, &self_ty),
                None => Vec::new(),
            }
        } else {
            Vec::new()
        }
    }

    /// `visit_implementation_of_copy`.
    fn copy(&mut self, impl_: hir::Impl, origin: &Origin, self_ty: &hir::Type<'db>) -> Vec<Found> {
        let cx = self.cx;
        let db = cx.db;
        if impl_.is_negative(db) {
            return Vec::new();
        }
        let builtin_copy = (self_ty.as_builtin().is_some() && !self_ty.is_str())
            || self_ty.is_raw_ptr()
            || self_ty.is_never()
            || (self_ty.is_reference() && !self_ty.is_mutable_reference())
            || self_ty.is_array();
        if builtin_copy {
            return Vec::new();
        }
        let Some(span) = origin.self_ty_span(cx) else { return Vec::new() };
        let Some(adt) = self_ty.as_adt() else {
            let mut diag = Diag::error(Some("E0206"), "the trait `Copy` cannot be implemented for this type", span.clone());
            diag.span_label(span, "type is not a structure or enumeration");
            diag.fact(origin.coherence(cx), "the header of the impl");
            return vec![Found::new(diag, Phase::Items)];
        };
        let infringing = infringing_fields(cx, adt, self_ty);
        if !infringing.is_empty() {
            return vec![Found::new(infringing_fields_error(cx, origin, span, adt, &infringing), Phase::Items)];
        }
        let Some(dtor) = self.destructor(adt) else { return Vec::new() };
        let mut diag = Diag::error(
            Some("E0184"),
            "the trait `Copy` cannot be implemented for this type; the type has a destructor",
            span.clone(),
        );
        diag.span_label(span, "`Copy` not allowed on types with destructors");
        let drop_fn = dtor.items(db).into_iter().find(|item| {
            matches!(item, hir::AssocItem::Function(f) if f.name(db).as_str() == "drop")
        });
        let at = match drop_fn {
            Some(item) => assoc_def_span(cx, item),
            None => ImplAt::of(cx, dtor).map(|it| it.span(cx)),
        };
        if let Some(at) = at {
            diag.span_note(at, "destructor declared here");
        }
        diag.fact(origin.coherence(cx), "the header of the impl");
        if let Some(at) = ImplAt::of(cx, dtor) {
            diag.fact(at.coherence(cx), format!("the `Drop` impl of `{}`", cx.ty(self_ty)));
        }
        vec![Found::new(diag, Phase::Items)]
    }

    /// rustc's `adt_destructor`: the `Drop` impl of `adt`, if it has one.
    fn destructor(&mut self, adt: hir::Adt) -> Option<hir::Impl> {
        let db = self.cx.db;
        let drop_trait = self.lang.Drop.map(hir::Trait::from);
        let all = self.destructors.get_or_insert_with(|| match drop_trait {
            Some(trait_) => hir::Impl::all_for_trait(db, trait_)
                .into_iter()
                .filter(|it| !it.is_negative(db))
                .filter_map(|it| Some((it.self_ty(db).as_adt()?, it)))
                .collect(),
            None => Vec::new(),
        });
        all.iter().find(|(it, _)| *it == adt).map(|&(_, impl_)| impl_)
    }
}

/// A field that is not `Copy` in the impl's environment, with its type there.
struct Infringing<'db> {
    field: hir::Field,
    ty: hir::Type<'db>,
}

/// `all_fields_implement_trait` for `Copy`: each field of each variant whose type in the
/// impl's environment is not `Copy`.
fn infringing_fields<'db>(cx: &Cx<'_, 'db>, adt: hir::Adt, self_ty: &hir::Type<'db>) -> Vec<Infringing<'db>> {
    let db = cx.db;
    let fields: Vec<(hir::Field, hir::Type<'db>)> = match adt {
        hir::Adt::Struct(_) | hir::Adt::Union(_) => self_ty.fields(db),
        hir::Adt::Enum(it) => {
            let params: Vec<hir::TypeParam> = hir::GenericDef::Adt(adt)
                .type_or_const_params(db)
                .into_iter()
                .filter_map(|it| it.as_type_param(db))
                .collect();
            let args: Vec<hir::Type<'db>> = self_ty.type_arguments().collect();
            it.variants(db)
                .into_iter()
                .flat_map(|variant| variant.fields(db))
                .filter_map(|field| {
                    let ty = field.ty(db);
                    if ty.generic_params(db).is_empty() {
                        return Some((field, ty));
                    }
                    // A parameter alone is the impl's argument for it.
                    let param = ty.as_type_param(db)?;
                    let index = params.iter().position(|it| *it == param)?;
                    Some((field, args.get(index)?.clone()))
                })
                .collect()
        }
    };
    fields
        .into_iter()
        .filter(|(_, ty)| !ty.contains_unknown() && !ty.is_copy(db))
        .map(|(field, ty)| Infringing { field, ty })
        .collect()
}

/// `infringing_fields_error` for `Copy`: E0204, a label on each field (one per type), and the
/// bound to add to a parameter that is a field's type.
fn infringing_fields_error(
    cx: &Cx<'_, '_>,
    origin: &Origin,
    span: Span,
    adt: hir::Adt,
    infringing: &[Infringing<'_>],
) -> Diag {
    let db = cx.db;
    let mut diag = Diag::error(Some("E0204"), "the trait `Copy` cannot be implemented for this type", span);
    let mut seen = FxHashSet::default();
    let mut params = Vec::new();
    let adt_key = cx.key(ItemId::Adt(adt.into()));
    for Infringing { field, ty } in infringing {
        let printed = cx.ty(ty);
        if !seen.insert(printed.clone()) {
            continue;
        }
        if let Some(source) = field.source(db) {
            let at = cx.span(source.as_ref().map(|it| item_range(it.syntax())));
            diag.span_label(at, "this field does not implement `Copy`");
        }
        if let Some(param) = ty.as_type_param(db)
            && !params.contains(&param)
        {
            params.push(param);
        }
        diag.fact(Fact::Field { ty: adt_key, field: field.index() as u32 }, format!("the type of a field of `{}`", cx.def_path(hir::ModuleDef::Adt(adt))));
    }
    if let Origin::Written(at) = origin {
        for param in params {
            restrict(cx, &mut diag, at, param);
        }
    }
    if let Some(copy) = lang_items(db, cx.krate.base()).Copy {
        for Infringing { ty, .. } in infringing {
            diag.fact_opt(cx.implements_fact(copy.into(), ty), format!("whether `{}` is `Copy`", cx.ty(ty)));
        }
    }
    diag.fact(origin.coherence(cx), "the header of the impl");
    diag
}

/// `suggest_constraining_type_params` of `param: Copy` where the impl declares `param`.
fn restrict(cx: &Cx<'_, '_>, diag: &mut Diag, at: &ImplAt, param: hir::TypeParam) {
    let Some(source) = param.merge().source(cx.db) else { return };
    let Either::Left(ast::TypeOrConstParam::Type(type_param)) = &source.value else { return };
    let Some(name) = type_param.name() else { return };
    let name = name.text().to_string();
    let span = |range| cx.span(source.with_value(range));
    let post = "trait `Copy`";
    let where_clause = at.node.value.where_clause();
    let where_bounds = where_clause.iter().flat_map(|it| it.predicates()).find_map(|pred| {
        let ty = pred.ty()?;
        (ty.syntax().text() == name.as_str()).then(|| pred.type_bound_list()?.bounds().last())?
    });
    let inline_last = type_param.type_bound_list().and_then(|it| it.bounds().last());
    let (at_span, message, code) = if let Some(last) = inline_last.or(where_bounds) {
        (
            span(last.syntax().text_range()).shrink_to_hi(),
            format!("consider further restricting type parameter `{name}` with {post}"),
            " + Copy".to_owned(),
        )
    } else if let Some(last) = where_clause.as_ref().and_then(|it| it.predicates().last()) {
        (
            span(last.syntax().text_range()).shrink_to_hi(),
            format!("consider further restricting type parameter `{name}` with {post}"),
            format!(", {name}: Copy"),
        )
    } else if let Some(colon) = type_param.colon_token() {
        (
            span(colon.text_range()).shrink_to_hi(),
            format!("consider restricting type parameter `{name}` with {post}"),
            " Copy".to_owned(),
        )
    } else {
        (
            span(type_param.syntax().text_range()).shrink_to_hi(),
            format!("consider restricting type parameter `{name}` with {post}"),
            ": Copy".to_owned(),
        )
    };
    diag.suggestion_verbose(at_span, message, code, Applicability::MachineApplicable);
}

/// `visit_implementation_of_drop` and `check_drop_impl`.
fn drop_impl(cx: &Cx<'_, '_>, at: &ImplAt, self_ty: &hir::Type<'_>) -> Vec<Found> {
    let db = cx.db;
    let local_adt = self_ty.as_adt().filter(|adt| adt.module(db).krate(db) == cx.krate);
    let Some(adt) = local_adt else {
        let span = at.self_ty_span(cx);
        let mut diag = Diag::error(
            Some("E0120"),
            "the `Drop` trait may only be implemented for local structs, enums, and unions",
            span.clone(),
        );
        diag.span_label(span, "must be a struct, enum, or union in the current crate");
        diag.fact(at.coherence(cx), "the header of the impl");
        return vec![Found::new(diag, Phase::Items)];
    };
    if at.impl_.is_negative(db) {
        let mut diag = Diag::error(None, "negative `Drop` impls are not supported", at.span(cx));
        diag.fact(at.coherence(cx), "the header of the impl");
        return vec![Found::new(diag, Phase::Items)];
    }
    let Some(arg) = not_unique_param(cx, at, self_ty) else { return Vec::new() };
    let mut diag = Diag::error(Some("E0366"), "`Drop` impls cannot be specialized", at.span(cx));
    match arg {
        NotUnique::Duplicate(arg) => diag.note(format!("`{arg}` is mentioned multiple times")),
        NotUnique::NotParam(arg) => diag.note(format!("`{arg}` is not a generic parameter")),
    };
    if let Some(source) = adt.source(db) {
        let descr = match adt {
            hir::Adt::Struct(_) => "struct",
            hir::Adt::Enum(_) => "enum",
            hir::Adt::Union(_) => "union",
        };
        diag.span_note(
            cx.span(source.as_ref().map(|it| def_range(it.syntax()))),
            format!("use the same sequence of generic lifetime, type and const parameters as the {descr} definition"),
        );
    }
    diag.fact(Fact::DropCheck(at.key(cx)), "the header of the `Drop` impl");
    diag.fact(Fact::Signature(cx.key(ItemId::Adt(adt.into()))), format!("the generics of `{}`", cx.def_path(hir::ModuleDef::Adt(adt))));
    vec![Found::new(diag, Phase::Items)]
}

/// rustc's `NotUniqueParam`, with the argument as rustc prints it.
enum NotUnique {
    Duplicate(String),
    NotParam(String),
}

/// `uses_unique_generic_params` of the self type's arguments, in order: each lifetime a
/// parameter of the impl (`'_` is a fresh one), each type and const one of its parameters,
/// none twice.
fn not_unique_param<'db>(cx: &Cx<'_, 'db>, at: &ImplAt, self_ty: &hir::Type<'db>) -> Option<NotUnique> {
    let db = cx.db;
    let mut types = self_ty.type_arguments();
    let written = match at.node.value.self_ty() {
        Some(ast::Type::PathType(ty)) => ty.path().and_then(|it| it.segment()).and_then(|it| it.generic_arg_list()),
        _ => None,
    };
    // A self type written as an alias gives its type arguments only.
    let by_alias = match at.node.value.self_ty() {
        Some(ast::Type::PathType(ty)) => ty
            .path()
            .and_then(|path| cx.sema.resolve_path(&path))
            .is_some_and(|it| matches!(it, hir::PathResolution::Def(hir::ModuleDef::TypeAlias(_)))),
        _ => false,
    };
    let const_params: Vec<String> = at
        .node
        .value
        .generic_param_list()
        .iter()
        .flat_map(|it| it.generic_params())
        .filter_map(|it| match it {
            ast::GenericParam::ConstParam(it) => Some(it.name()?.text().to_string()),
            _ => None,
        })
        .collect();
    let mut seen_lifetimes = FxHashSet::default();
    let mut seen_types = Vec::new();
    let mut seen_consts = FxHashSet::default();
    let mut type_arg = |ty: hir::Type<'db>| -> Option<NotUnique> {
        match ty.as_type_param(db) {
            Some(param) if seen_types.contains(&param) => Some(NotUnique::Duplicate(cx.ty(&ty))),
            Some(param) => {
                seen_types.push(param);
                None
            }
            None => Some(NotUnique::NotParam(cx.ty(&ty))),
        }
    };
    let args = match written {
        Some(list) if !by_alias => list.generic_args().collect(),
        _ => Vec::new(),
    };
    if args.is_empty() {
        return types.find_map(&mut type_arg);
    }
    for arg in args {
        let found = match arg {
            ast::GenericArg::LifetimeArg(it) => {
                let text = it.lifetime().map(|it| it.text().to_string()).unwrap_or_default();
                match text.as_str() {
                    "'_" => None,
                    "'static" => Some(NotUnique::NotParam(text)),
                    _ if !seen_lifetimes.insert(text.clone()) => Some(NotUnique::Duplicate(text)),
                    _ => None,
                }
            }
            ast::GenericArg::TypeArg(_) => types.next().and_then(&mut type_arg),
            ast::GenericArg::ConstArg(it) => {
                let text = it.syntax().text().to_string();
                let name = text.trim_start_matches('{').trim_end_matches('}').trim().to_owned();
                if !const_params.contains(&name) {
                    Some(NotUnique::NotParam(text))
                } else if !seen_consts.insert(name.clone()) {
                    Some(NotUnique::Duplicate(name))
                } else {
                    None
                }
            }
            ast::GenericArg::AssocTypeArg(_) => None,
        };
        if found.is_some() {
            return found;
        }
    }
    None
}

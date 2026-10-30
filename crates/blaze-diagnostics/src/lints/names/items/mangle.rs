//! `no_mangle_const_items` (deny) and `no_mangle_generic_items`: a `#[no_mangle]` constant,
//! and a `#[no_mangle]` or `#[export_name]` function (free, or of an impl) generic over types or
//! consts, its own parameters or its impl's, an `impl Trait` parameter among them.
//!
//! Follows `compiler/rustc_lint/src/builtin.rs` (`InvalidNoMangleItems`) and
//! `compiler/rustc_middle/src/ty/generics.rs` (`requires_monomorphization`) (MIT or Apache-2.0).

use syntax::{
    AstNode, SyntaxNodePtr, TextRange,
    ast::{self, HasGenericParams, HasName},
};

use super::{
    def_span,
    walk::{self, Container, Item},
};
use crate::{
    cx::{Cx, item_range},
    lower::{Found, Phase},
    model::{Applicability, Diag, SuggestionStyle},
};

pub(super) fn item(cx: &Cx<'_, '_>, item: &Item) -> Vec<Found> {
    match &item.node.value {
        ast::Item::Fn(it) if matches!(item.container, Container::Module | Container::Impl(_)) => {
            let Some(attr) = attr_span(cx, it, "export_name").or_else(|| attr_span(cx, it, "no_mangle")) else {
                return Vec::new();
            };
            let impl_generic = match &item.container {
                Container::Impl(imp) => generic(imp.value.generic_param_list()),
                _ => false,
            };
            if !impl_generic && !generic(it.generic_param_list()) && !impl_trait_param(it) {
                return Vec::new();
            }
            let span = cx.span(item.node.with_value(def_span(it.syntax())));
            let mut diag = Diag::lint("no_mangle_generic_items", "functions generic over types or consts must be mangled", span);
            let attr = cx.span(item.node.with_value(attr));
            diag.suggestion_with_style(
                vec![(attr, String::new())],
                "remove this attribute",
                Applicability::MaybeIncorrect,
                SuggestionStyle::HideCodeInline,
            );
            crate::lints::code_fact(cx, &mut diag, item.ptr());
            vec![Found::lint(diag, Phase::Late, item.ptr())]
        }
        ast::Item::Const(it) if matches!(item.container, Container::Module) => {
            if attr_span(cx, it, "no_mangle").is_none() {
                return Vec::new();
            }
            let range = item_range(it.syntax());
            let span = cx.span(item.node.with_value(range));
            let mut diag = Diag::lint("no_mangle_const_items", "const items should never be `#[no_mangle]`", span);
            let plain = it.generic_param_list().is_none() && it.where_clause().is_none();
            if plain && let Some(name) = it.name() {
                let until = TextRange::new(range.start(), name.syntax().text_range().start());
                let until = cx.span(item.node.with_value(until));
                diag.suggestion(until, "try a static value", "pub static ", Applicability::MachineApplicable);
            }
            let at = item.node.as_ref().map(|it| SyntaxNodePtr::new(it.syntax()));
            crate::lints::code_fact(cx, &mut diag, at);
            vec![Found::lint(diag, Phase::Late, at)]
        }
        _ => Vec::new(),
    }
}

/// The span of the attribute `name` on `owner`: the whole `#[..]`, through an `unsafe(..)`.
fn attr_span<N: syntax::ast::HasAttrs>(cx: &Cx<'_, '_>, owner: &N, name: &str) -> Option<TextRange> {
    let (meta, range) = walk::attr(cx, owner, name)?;
    let wrapped = meta.syntax().parent().and_then(ast::UnsafeMeta::cast);
    match wrapped.and_then(|it| it.syntax().parent()).and_then(ast::Attr::cast) {
        Some(attr) => Some(attr.syntax().text_range()),
        None => Some(range),
    }
}

/// Whether a parameter list has a type or a const parameter.
fn generic(params: Option<ast::GenericParamList>) -> bool {
    params.is_some_and(|it| it.generic_params().any(|it| !matches!(it, ast::GenericParam::LifetimeParam(_))))
}

/// Whether a parameter's type is an `impl Trait`, which is a type parameter of the function.
fn impl_trait_param(f: &ast::Fn) -> bool {
    f.param_list().into_iter().flat_map(|it| it.params()).filter_map(|it| it.ty()).any(|ty| {
        ty.syntax().descendants().any(|it| {
            ast::ImplTraitType::can_cast(it.kind())
                && !it.ancestors().any(|it| ast::FnPtrType::can_cast(it.kind()) || ast::PathType::cast(it.clone()).is_some_and(|p| is_fn_sugar(&p)))
        })
    })
}

/// A `Fn(..) -> ..` path, in whose arguments an `impl Trait` is an error, not a parameter.
fn is_fn_sugar(ty: &ast::PathType) -> bool {
    ty.path().and_then(|it| it.segment()).is_some_and(|it| it.parenthesized_arg_list().is_some())
}

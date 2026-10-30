//! A name that names nothing in its item but a generic parameter or `Self` of an item around
//! it: E0401 "can't use generic parameters from outer item" (or "can't use `Self` from outer
//! item"), with the parameter, the inner item and a local parameter to introduce. `Self` with no
//! item around to give it is E0411.
//!
//! Follows rustc's `compiler/rustc_resolve/src/ident.rs` (`validate_res_from_ribs`),
//! `diagnostics/impls.rs` (`GenericParamsFromOuterItem` of `into_struct_error`),
//! `late/diagnostics.rs` (`suggest_self_ty`) and `diagnostics/mod.rs`
//! (`GenericParamsFromOuterItem` and its subdiagnostics) (MIT or Apache-2.0).

use hir::InFile;
use syntax::{
    AstNode, SyntaxNode, TextRange, TextSize,
    ast::{self, HasGenericParams as _, HasName as _},
};

use super::Signature;
use crate::{
    cx::Cx,
    model::{Applicability, Diag, Span},
};

/// The E0401 or E0411 of `name` written at `path`, when that is what it is; `last` whether it
/// is the whole path (a qualifier `Self::` with nothing to give it is E0433 instead).
pub(super) fn check(cx: &Cx<'_, '_>, signature: &Signature, path: &ast::Path, name: &str, last: bool) -> Option<Diag> {
    let file = signature.node.file_id;
    let at = |range: TextRange| cx.span(InFile::new(file, range));
    let span = at(path.segment()?.syntax().text_range());
    let inner = inner_item(&signature.node.value);
    if name == "Self" {
        let outer = inner.ancestors().skip(1).take_while(|it| !is_module(it)).find_map(SelfGiver::of);
        return match outer {
            Some(giver) => Some(self_from_outer(cx, file, &inner, giver, span)),
            None if last => Some(self_not_allowed(cx, file, &inner, span)),
            None => None,
        };
    }
    let param = inner
        .ancestors()
        .skip(1)
        .take_while(|it| !is_module(it))
        .filter_map(|it| ast::AnyHasGenericParams::cast(it)?.generic_param_list())
        .find_map(|list| list.type_or_const_params().find(|it| it.name().is_some_and(|it| it.text() == name)))?;
    let mut diag = Diag::error(Some("E0401"), "can't use generic parameters from outer item", span.clone());
    diag.span_label(span, "use of generic parameter from outer item");
    let (kind, name_range) = match &param {
        ast::TypeOrConstParam::Type(it) => ("type", it.name()?.syntax().text_range()),
        ast::TypeOrConstParam::Const(it) => ("const", it.name()?.syntax().text_range()),
    };
    diag.span_label(at(name_range), format!("{kind} parameter from outer item"));
    if let Some((label, descr)) = item_label(&inner) {
        diag.span_label(at(label), format!("generic parameter used in this inner {descr}"));
    }
    diag.note("nested items are independent from their parent item for everything except for privacy and name resolution");
    let keyword = static_or_const(&inner);
    if let Some(keyword) = keyword {
        diag.note(format!("a `{keyword}` is a separate item from the item that contains it"));
        if keyword == "const" {
            let start = inner.text_range().start();
            let text = inner.text().to_string();
            let word = text.split(char::is_whitespace).next().unwrap_or("");
            diag.suggestion_verbose(
                at(TextRange::at(start, TextSize::of(word))),
                "try using a local `let` binding instead",
                "let",
                Applicability::MaybeIncorrect,
            );
        }
    } else if let Some((insert, snippet)) = introduce(&inner, name) {
        diag.suggestion_verbose(at(insert), "try introducing a local generic parameter here", snippet, Applicability::MaybeIncorrect);
    }
    Some(diag)
}

/// The item whose rib rustc crosses to the outer one: the signature's item, or the impl or
/// trait an associated item is in.
pub(super) fn inner_item(node: &SyntaxNode) -> SyntaxNode {
    match node.parent().filter(|it| ast::AssocItemList::can_cast(it.kind())).and_then(|it| it.parent()) {
        Some(container) => container,
        None => node.clone(),
    }
}

fn is_module(node: &SyntaxNode) -> bool {
    ast::Module::can_cast(node.kind()) || ast::SourceFile::can_cast(node.kind())
}

/// An item around that gives `Self`: an impl (rustc's `SelfTyAlias`) or a trait
/// (`SelfTyParam`).
enum SelfGiver {
    Impl(ast::Impl),
    Trait,
}

impl SelfGiver {
    fn of(node: SyntaxNode) -> Option<SelfGiver> {
        if let Some(it) = ast::Impl::cast(node.clone()) {
            return Some(SelfGiver::Impl(it));
        }
        ast::Trait::can_cast(node.kind()).then_some(SelfGiver::Trait)
    }
}

fn self_from_outer(cx: &Cx<'_, '_>, file: hir::HirFileId, inner: &SyntaxNode, giver: SelfGiver, span: Span) -> Diag {
    let at = |range: TextRange| cx.span(InFile::new(file, range));
    let mut diag = Diag::error(Some("E0401"), "can't use `Self` from outer item", span.clone());
    diag.span_label(span.clone(), "use of `Self` from outer item");
    let mut direct = None;
    match giver {
        SelfGiver::Impl(imp) => {
            if let Some(token) = imp.impl_token() {
                diag.span_label(at(token.text_range()), "`Self` type implicitly declared here, by this `impl`");
            }
            direct = imp.self_ty().map(|it| it.syntax().to_string());
        }
        SelfGiver::Trait => {
            diag.span_label(span.clone(), "can't use `Self` here");
        }
    }
    if let Some((label, descr)) = item_label(inner) {
        diag.span_label(at(label), format!("`Self` used in this inner {descr}"));
    }
    diag.note("nested items are independent from their parent item for everything except for privacy and name resolution");
    if let Some(keyword) = static_or_const(inner) {
        diag.note(format!("a `{keyword}` is a separate item from the item that contains it"));
    }
    if let Some(snippet) = direct {
        diag.suggestion_verbose(span, "refer to the type directly here instead", snippet, Applicability::MaybeIncorrect);
    }
    diag
}

/// E0411 (`suggest_self_ty`): `Self` where no item gives it.
fn self_not_allowed(cx: &Cx<'_, '_>, file: hir::HirFileId, inner: &SyntaxNode, span: Span) -> Diag {
    let mut diag = Diag::error(Some("E0411"), "cannot find type `Self` in this scope", span.clone());
    diag.span_label(span, "`Self` is only available in impls, traits, and type definitions");
    if let Some((label, _)) = item_label(inner)
        && let Some((article, descr)) = ast_item_descr(inner)
    {
        diag.span_label(cx.span(InFile::new(file, label)), format!("`Self` not allowed in {article} {descr}"));
    }
    diag
}

/// The name of an item and rustc's `ItemKind::descr` of it, for the inner item's label.
fn item_label(node: &SyntaxNode) -> Option<(TextRange, &'static str)> {
    let name = ast::AnyHasName::cast(node.clone())?.name()?;
    let descr = match ast::Item::cast(node.clone())? {
        ast::Item::Fn(_) => "function",
        ast::Item::Struct(_) => "struct",
        ast::Item::Enum(_) => "enum",
        ast::Item::Union(_) => "union",
        ast::Item::Trait(_) => "trait",
        ast::Item::TypeAlias(_) => "type alias",
        ast::Item::Const(_) => "constant item",
        ast::Item::Static(_) => "static item",
        _ => return None,
    };
    Some((name.syntax().text_range(), descr))
}

/// rustc's `ItemKind::article` and `descr`.
fn ast_item_descr(node: &SyntaxNode) -> Option<(&'static str, &'static str)> {
    Some(match ast::Item::cast(node.clone())? {
        ast::Item::Fn(_) => ("a", "function"),
        ast::Item::Struct(_) => ("a", "struct"),
        ast::Item::Enum(_) => ("an", "enum"),
        ast::Item::Union(_) => ("a", "union"),
        ast::Item::Trait(_) => ("a", "trait"),
        ast::Item::TypeAlias(_) => ("a", "type alias"),
        ast::Item::Const(_) => ("a", "constant item"),
        ast::Item::Static(_) => ("a", "static item"),
        ast::Item::Impl(_) => ("an", "implementation"),
        _ => return None,
    })
}

fn static_or_const(node: &SyntaxNode) -> Option<&'static str> {
    if ast::Static::can_cast(node.kind()) {
        Some("static")
    } else if ast::Const::can_cast(node.kind()) {
        Some("const")
    } else {
        None
    }
}

/// rustc's `GenericParamsFromOuterItemSugg`: `name, ` after the `<` of the inner item's
/// parameters, or `<name>` where they would go.
fn introduce(inner: &SyntaxNode, name: &str) -> Option<(TextRange, String)> {
    let item = ast::AnyHasGenericParams::cast(inner.clone())?;
    if let Some(list) = item.generic_param_list() {
        let angle = list.l_angle_token()?;
        return Some((TextRange::empty(angle.text_range().end()), format!("{name}, ")));
    }
    let after = match ast::AnyHasName::cast(inner.clone()).and_then(|it| it.name()) {
        Some(it) => it.syntax().text_range().end(),
        None => ast::Impl::cast(inner.clone())?.impl_token()?.text_range().end(),
    };
    Some((TextRange::empty(after), format!("<{name}>")))
}

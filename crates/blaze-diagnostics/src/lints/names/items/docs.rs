//! `missing_docs`: an exported item, associated item, foreign item, variant or named field
//! with no doc comment, no `#[doc = ..]` and no `#[doc(hidden)]`, and the crate with none.
//! Impls, `use`s and `extern crate`s are not linted, nor the items of a trait impl or of an
//! inherent impl of a `#[doc(hidden)]` type.
//!
//! Follows `compiler/rustc_lint/src/builtin.rs` (`MissingDoc`, `has_doc`) and
//! `compiler/rustc_middle/src/ty/mod.rs` (`article_and_description`, `def_descr`) (MIT or
//! Apache-2.0).

use blaze_items::Fact;
use hir::{InFile, ModuleSource};
use syntax::{
    AstNode, NodeOrToken, SyntaxKind, SyntaxNode, SyntaxNodePtr, TextRange, TextSize, T,
    ast::{self, HasAttrs},
};

use super::{
    def_span,
    visibility::{self, Def, Effective},
    walk::{self, Container, Item},
};
use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::Diag,
};

/// `check_crate`: the crate's documentation.
pub(super) fn krate(cx: &Cx<'_, '_>) -> Vec<Found> {
    let root = cx.krate.root_module(cx.db);
    let source = root.definition_source(cx.db);
    let ModuleSource::SourceFile(_) = source.value else { return Vec::new() };
    let file = source.file_id;
    let Some(source_file) = ast::SourceFile::cast(cx.sema.parse_or_expand(file)) else { return Vec::new() };
    if has_doc(cx, &source_file, Some(source_file.syntax())) {
        return Vec::new();
    }
    let Some(range) = tokens_range(source_file.syntax()) else { return Vec::new() };
    let mut diag = Diag::lint("missing_docs", "missing documentation for the crate", cx.span(InFile::new(file, range)));
    diag.fact(Fact::Name(cx.module_key(root)), "the attributes of the crate");
    vec![Found::lint(diag, Phase::Late, InFile::new(file, SyntaxNodePtr::new(source_file.syntax())))]
}

/// `check_item`, `check_trait_item`, `check_impl_item`, `check_foreign_item`, then the
/// variants and fields among the item's own nodes.
pub(super) fn item(cx: &Cx<'_, '_>, item: &Item, effective: &Effective) -> Vec<Found> {
    let mut found = Vec::new();
    let node = &item.node.value;
    if matches!(node, ast::Item::Impl(_) | ast::Item::Use(_) | ast::Item::ExternCrate(_)) {
        return found;
    }
    if let Container::Impl(imp) = &item.container
        && imp.value.trait_().is_some()
    {
        return found;
    }
    // Asked last: only an exported item with no docs of its own needs it.
    let hidden_impl = || matches!(&item.container, Container::Impl(imp) if hidden_self_ty(cx, imp));
    let Some(desc) = describe(item) else { return found };
    let exported = match node {
        ast::Item::MacroRules(it) => walk::attr(cx, it, "macro_export").is_some(),
        ast::Item::ExternBlock(_) => false,
        _ => visibility::def(cx, item).is_some_and(|def| effective.is_exported(def)),
    };
    let inner = match node {
        ast::Item::Module(it) => it.item_list().map(|it| it.syntax().clone()),
        _ => None,
    };
    if exported && !has_doc(cx, node, inner.as_ref()) && !hidden_impl() {
        found.push(lint(cx, item.syntax(), desc));
    }
    if !matches!(node, ast::Item::Struct(_) | ast::Item::Union(_) | ast::Item::Enum(_)) {
        return found;
    }
    for own in &item.nodes[1..] {
        match own.value.kind() {
            // A variant `pub use` names is exported without its enum.
            SyntaxKind::VARIANT => {
                let Some(variant) = ast::Variant::cast(own.value.clone()) else { continue };
                let def = cx.sema.to_def2(own.with_value(&variant));
                let exported = def.is_some_and(|it| effective.is_exported(Def::Item(hir::ModuleDef::EnumVariant(it))));
                if exported && !has_doc(cx, &variant, None) {
                    found.push(lint(cx, own.clone(), ("a", "variant")));
                }
            }
            // A field is exported only with the type it is of.
            SyntaxKind::RECORD_FIELD if exported => {
                let Some(field) = ast::RecordField::cast(own.value.clone()) else { continue };
                let def = cx.sema.to_def2(own.with_value(&field));
                let exported = def.is_some_and(|it| effective.is_exported(Def::Field(it)));
                if exported && !has_doc(cx, &field, None) {
                    found.push(lint(cx, own.clone(), ("a", "struct field")));
                }
            }
            _ => {}
        }
    }
    found
}

fn lint(cx: &Cx<'_, '_>, node: InFile<SyntaxNode>, (article, desc): (&str, &str)) -> Found {
    let span = cx.span(node.with_value(def_span(&node.value)));
    let mut diag = Diag::lint("missing_docs", format!("missing documentation for {article} {desc}"), span);
    crate::lints::code_fact(cx, &mut diag, node.as_ref().map(SyntaxNodePtr::new));
    Found::lint(diag, Phase::Late, node.map(|it| SyntaxNodePtr::new(&it)))
}

/// rustc's `has_doc`: a doc comment or `#[doc = ..]` on `owner` (or, inner, in `inner`), or
/// a `#[doc(hidden)]`.
fn has_doc<N: HasAttrs>(cx: &Cx<'_, '_>, owner: &N, inner: Option<&SyntaxNode>) -> bool {
    let outer_comment = owner.syntax().children().any(|it| is_doc_comment(&it, T![outer_doc_comment]));
    let inner_comment = inner.is_some_and(|it| it.children().any(|it| is_doc_comment(&it, T![inner_doc_comment])));
    if outer_comment || inner_comment {
        return true;
    }
    let documented = |metas: Vec<(ast::Meta, TextRange)>| {
        metas.into_iter().any(|(meta, _)| match meta {
            ast::Meta::KeyValueMeta(_) => true,
            ast::Meta::TokenTreeMeta(it) => it.token_tree().is_some_and(|tt| {
                tt.syntax()
                    .children_with_tokens()
                    .filter_map(NodeOrToken::into_token)
                    .any(|token| token.kind() == SyntaxKind::IDENT && token.text() == "hidden")
            }),
            _ => false,
        })
    };
    if documented(walk::attrs_named(cx, owner, "doc")) {
        return true;
    }
    inner.and_then(ast::ItemList::cast).is_some_and(|list| documented(walk::attrs_named(cx, &list, "doc")))
}

fn is_doc_comment(node: &SyntaxNode, kind: SyntaxKind) -> bool {
    node.kind() == SyntaxKind::DOC_COMMENT
        && node.children_with_tokens().any(|it| it.kind() == kind)
}

/// From the first token of a file to the end of its last: the span of the crate.
fn tokens_range(node: &SyntaxNode) -> Option<TextRange> {
    let mut tokens = node.descendants_with_tokens().filter_map(NodeOrToken::into_token).filter(|it| !it.kind().is_trivia());
    let first: TextSize = tokens.next()?.text_range().start();
    let last = node
        .descendants_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .filter(|it| !it.kind().is_trivia())
        .last()
        .map_or(first, |it| it.text_range().end());
    Some(TextRange::new(first, last))
}

/// Whether the self type of an inherent impl is a `#[doc(hidden)]` type.
fn hidden_self_ty(cx: &Cx<'_, '_>, imp: &InFile<ast::Impl>) -> bool {
    let Some(def) = cx.sema.to_def2(imp.as_ref()) else { return false };
    let Some(adt) = def.self_ty(cx.db).as_adt() else { return false };
    hir::HasAttrs::attrs(adt, cx.db).is_doc_hidden()
}

/// rustc's `article_and_description` of an item.
fn describe(item: &Item) -> Option<(&'static str, &'static str)> {
    let assoc = matches!(item.container, Container::Trait(_) | Container::Impl(_));
    Some(match &item.node.value {
        ast::Item::Fn(it) if assoc => match it.param_list().and_then(|it| it.self_param()) {
            Some(_) => ("a", "method"),
            None => ("an", "associated function"),
        },
        ast::Item::Fn(_) => ("a", "function"),
        ast::Item::Const(_) if assoc => ("an", "associated constant"),
        ast::Item::Const(_) => ("a", "constant"),
        ast::Item::TypeAlias(_) if assoc => ("an", "associated type"),
        ast::Item::TypeAlias(_) if matches!(item.container, Container::Extern(_)) => ("a", "foreign type"),
        ast::Item::TypeAlias(_) => ("a", "type alias"),
        ast::Item::Static(_) => ("a", "static"),
        ast::Item::Struct(_) => ("a", "struct"),
        ast::Item::Enum(_) => ("an", "enum"),
        ast::Item::Union(_) => ("a", "union"),
        ast::Item::Trait(it) if it.eq_token().is_some() => ("a", "trait alias"),
        ast::Item::Trait(_) => ("a", "trait"),
        ast::Item::Module(_) => ("a", "module"),
        ast::Item::MacroRules(_) => ("a", "macro"),
        ast::Item::MacroDef(_) => ("a", "macro"),
        _ => return None,
    })
}

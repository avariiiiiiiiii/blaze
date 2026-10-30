//! The lint attributes of a node as rustc reads them: with `cfg_attr` expanded, outer then
//! inner, each list read as `meta_item_list` reads it; which nodes have them; the node whose
//! attributes come next going out from one, through the macro call an expansion is of and the
//! `mod` a file is of.
//!
//! Follows rustc's `compiler/rustc_ast/src/attr/mod.rs` (`meta_item_list`,
//! `MetaItemKind::list_from_tokens`, `is_automatically_derived_attr`, `is_doc_hidden`),
//! `compiler/rustc_expand/src/expand.rs` (`InvocationCollector`: the attributes of a macro
//! call are not the expansion's; a derive's output is beside the item, an attribute macro's in
//! its place), `compiler/rustc_hir/src/intravisit.rs` and `compiler/rustc_ast/src/visit.rs`
//! (which nodes have lint attributes) (MIT or Apache-2.0).

use std::ops::ControlFlow;

use cfg::CfgOptions;
use hir::{InFile, Semantics, db::HirDatabase};
use hir_expand::MacroCallKind;
use span::Edition;
use syntax::{
    AstNode, AstToken, NodeOrToken, SyntaxElement, SyntaxKind, SyntaxNode, SyntaxNodePtr,
    TextRange, ast,
};

use crate::{
    lints::registry::Level,
    model::{MacroKind, Span},
};

/// An attribute of a node as rustc's lint levels read it.
pub(super) enum Read {
    /// `#[automatically_derived]` (`single_use_lifetimes`) or `#[doc(hidden)]`
    /// (`missing_docs`): a lint allowed with no attribute naming it.
    Allows(&'static str),
    /// `#[allow(..)]`, `#[expect(..)]`, `#[warn(..)]`, `#[deny(..)]`, `#[forbid(..)]`.
    Level(Level, Vec<Item>),
}

/// An element of a lint attribute's list: rustc's `MetaItemInner`.
pub(super) struct Item {
    pub(super) range: TextRange,
    pub(super) kind: ItemKind,
}

pub(super) enum ItemKind {
    /// A path: a lint, a group or a tool's lint.
    Word(Vec<Segment>),
    /// `path = literal`: the literal's value when it is a string, and where the literal is.
    NameValue { path: Vec<Segment>, string: Option<String>, value: TextRange },
    /// `path(..)`.
    List(Vec<Segment>),
    /// A literal alone.
    Lit,
}

impl ItemKind {
    /// Whether it is `reason = ..`.
    pub(super) fn is_reason(&self) -> bool {
        matches!(self, ItemKind::NameValue { path, .. } if is_word(path, "reason"))
    }
}

/// A segment of a path, as rustc's `Ident` has it.
#[derive(Clone)]
pub(super) struct Segment {
    pub(super) name: String,
    pub(super) range: TextRange,
}

/// Whether `path` is the one segment `name`.
fn is_word(path: &[Segment], name: &str) -> bool {
    matches!(path, [only] if only.name == name)
}

/// `path` as rustc's `pprust::path_to_string` writes it.
pub(super) fn path_string(path: &[Segment]) -> String {
    path.iter().map(|it| it.name.as_str()).collect::<Vec<_>>().join("::")
}

/// The node whose attributes set lint levels for what is in it, if `node` is one. Not a macro
/// call: its attributes are checked and dropped, not given to its expansion. Not the list an
/// item, a `match` or a function's body has its inner attributes in: they are the item's.
/// `generics`: whether a generic parameter is one, as it is to the early lint pass and not to
/// the HIR.
pub(super) fn owner(node: &SyntaxNode, generics: bool) -> Option<ast::AnyHasAttrs> {
    use SyntaxKind::*;
    match node.kind() {
        MACRO_CALL | MACRO_EXPR | MACRO_PAT | MACRO_TYPE | MACRO_STMTS | MACRO_ITEMS => return None,
        TYPE_PARAM | CONST_PARAM | LIFETIME_PARAM if !generics => return None,
        _ => {}
    }
    let owner = ast::AnyHasAttrs::cast(node.clone())?;
    let inner_of_outer = node.ancestors().skip(1).take(2).any(|it| {
        ast::AnyHasAttrs::cast(it)
            .is_some_and(|it| ast::HasAttrs::inner_attributes_node(&it).is_some_and(|inner| &inner == node))
    });
    (!inner_of_outer).then_some(owner)
}

/// The lint attributes of `owner`, `cfg_attr` expanded against `cfg`, outer then inner, in
/// order.
pub(super) fn read(owner: &ast::AnyHasAttrs, cfg: &CfgOptions, edition: Edition) -> Vec<Read> {
    let mut found = Vec::new();
    let mut each = |meta: ast::Meta, _| {
        if let Some(read) = read_meta(&meta, edition) {
            found.push(read);
        }
        ControlFlow::<()>::Continue(())
    };
    if owner.syntax().kind() == SyntaxKind::STMT_LIST {
        hir_expand::attrs::expand_cfg_attr(ast::HasAttrs::attrs(owner), || cfg, &mut each);
    } else {
        hir_expand::attrs::expand_cfg_attr(ast::attrs_including_inner(owner), || cfg, &mut each);
    }
    found
}

fn read_meta(meta: &ast::Meta, edition: Edition) -> Option<Read> {
    let name = |path: Option<ast::Path>| {
        path.and_then(|it| it.as_single_name_ref()).map(|it| it.text().to_string())
    };
    match meta {
        ast::Meta::PathMeta(it) if name(it.path()).as_deref() == Some("automatically_derived") => {
            Some(Read::Allows("single_use_lifetimes"))
        }
        ast::Meta::TokenTreeMeta(it) => {
            let name = name(it.path())?;
            let items = list(&it.token_tree()?, edition);
            if name == "doc" {
                let hidden = items?.iter().any(|it| matches!(&it.kind, ItemKind::Word(path) if is_word(path, "hidden")));
                return hidden.then_some(Read::Allows("missing_docs"));
            }
            Some(Read::Level(Level::of_attr(&name)?, items?))
        }
        _ => None,
    }
}

/// A parenthesized list read as rustc's `MetaItemKind::list_from_tokens` reads it: `None`
/// when it is not one, and rustc's lint levels pass the attribute by.
pub(super) fn list(tt: &ast::TokenTree, edition: Edition) -> Option<Vec<Item>> {
    let children: Vec<SyntaxElement> =
        tt.syntax().children_with_tokens().filter(|it| !it.kind().is_trivia()).collect();
    let (first, last) = (children.first()?, children.last()?);
    if first.kind() != SyntaxKind::L_PAREN || last.kind() != SyntaxKind::R_PAREN || children.len() < 2 {
        return None;
    }
    let inner = &children[1..children.len() - 1];
    let mut items = Vec::new();
    let mut rest = inner;
    while !rest.is_empty() {
        let end = rest.iter().position(|it| it.kind() == SyntaxKind::COMMA).unwrap_or(rest.len());
        items.push(item(&rest[..end], edition)?);
        rest = match rest.get(end) {
            Some(_) => &rest[end + 1..],
            None => &[],
        };
    }
    Some(items)
}

fn item(elements: &[SyntaxElement], edition: Edition) -> Option<Item> {
    let range = TextRange::new(elements.first()?.text_range().start(), elements.last()?.text_range().end());
    if let [NodeOrToken::Token(token)] = elements
        && is_literal(token.kind())
    {
        return Some(Item { range, kind: ItemKind::Lit });
    }
    let (path, rest) = path(elements, edition)?;
    let kind = match rest {
        [] => ItemKind::Word(path),
        [NodeOrToken::Token(eq), NodeOrToken::Token(value)]
            if eq.kind() == SyntaxKind::EQ && is_literal(value.kind()) =>
        {
            let string = ast::String::cast(value.clone()).and_then(|it| it.value().ok()).map(|it| it.into_owned());
            ItemKind::NameValue { path, string, value: value.text_range() }
        }
        [NodeOrToken::Node(tt)] if tt.kind() == SyntaxKind::TOKEN_TREE => {
            let open = tt.first_token().map(|it| it.kind());
            if open != Some(SyntaxKind::L_PAREN) {
                return None;
            }
            ItemKind::List(path)
        }
        _ => return None,
    };
    Some(Item { range, kind })
}

/// The path `elements` start with and what follows it. A path that starts with `::` has
/// rustc's `{{root}}` for its first segment, at the `::`.
fn path(elements: &[SyntaxElement], edition: Edition) -> Option<(Vec<Segment>, &[SyntaxElement])> {
    let mut segments = Vec::new();
    let mut rest = elements;
    if let Some((range, after)) = colons(rest) {
        segments.push(Segment { name: "{{root}}".to_owned(), range });
        rest = after;
    }
    loop {
        let (NodeOrToken::Token(token), after) = rest.split_first()? else { return None };
        if !is_segment(token.kind(), edition) {
            return None;
        }
        segments.push(Segment { name: token.text().to_owned(), range: token.text_range() });
        rest = after;
        match colons(rest) {
            Some((_, after)) => rest = after,
            None => return Some((segments, rest)),
        }
    }
}

/// `::` at the start of `elements`, as one token or two `:` together.
fn colons(elements: &[SyntaxElement]) -> Option<(TextRange, &[SyntaxElement])> {
    match elements {
        [NodeOrToken::Token(it), rest @ ..] if it.kind() == SyntaxKind::COLON2 => Some((it.text_range(), rest)),
        [NodeOrToken::Token(a), NodeOrToken::Token(b), rest @ ..]
            if a.kind() == SyntaxKind::COLON
                && b.kind() == SyntaxKind::COLON
                && a.text_range().end() == b.text_range().start() =>
        {
            Some((a.text_range().cover(b.text_range()), rest))
        }
        _ => None,
    }
}

fn is_segment(kind: SyntaxKind, edition: Edition) -> bool {
    use SyntaxKind::*;
    kind == IDENT
        || matches!(kind, SELF_KW | SUPER_KW | CRATE_KW | SELF_TYPE_KW)
        || (kind.is_any_identifier() && !kind.is_keyword(edition))
}

fn is_literal(kind: SyntaxKind) -> bool {
    kind.is_literal() || matches!(kind, SyntaxKind::TRUE_KW | SyntaxKind::FALSE_KW)
}

/// The tools of the crate's `#![register_tool(..)]`s.
pub(super) fn registered_tools(root: &ast::AnyHasAttrs, cfg: &CfgOptions, edition: Edition) -> Vec<String> {
    let mut tools = Vec::new();
    hir_expand::attrs::expand_cfg_attr::<()>(ast::attrs_including_inner(root), || cfg, |meta, _| {
        if let ast::Meta::TokenTreeMeta(meta) = meta
            && meta.path().and_then(|it| it.as_single_name_ref()).is_some_and(|it| it.text() == "register_tool")
            && let Some(items) = meta.token_tree().and_then(|it| list(&it, edition))
        {
            for item in items {
                if let ItemKind::Word(path) = item.kind
                    && let [tool] = &path[..]
                {
                    tools.push(tool.name.clone());
                }
            }
        }
        ControlFlow::Continue(())
    });
    tools
}

/// The node whose attributes come next going out from `node`: its parent; for the root of a
/// macro's expansion, what is around the call (around the item, for a derive or an attribute
/// macro); for the root of a module's file, its `mod` item.
pub(super) fn parent(
    sema: &Semantics<'_, dyn HirDatabase>,
    node: &InFile<SyntaxNode>,
) -> Option<InFile<SyntaxNode>> {
    if let Some(parent) = node.value.parent() {
        return Some(node.with_value(parent));
    }
    let Some(call) = node.file_id.macro_file() else {
        return sema.find_parent_file(node.file_id);
    };
    let loc = call.loc(sema.db);
    let at = loc.to_node(sema.db);
    let around = match loc.kind {
        MacroCallKind::FnLike { .. } => {
            at.value.ancestors().skip(1).find(|it| !matches!(it.kind(), SyntaxKind::MACRO_EXPR | SyntaxKind::MACRO_PAT | SyntaxKind::MACRO_TYPE))
        }
        MacroCallKind::Derive { .. } => {
            at.value.ancestors().find(|it| ast::Adt::can_cast(it.kind())).and_then(|it| it.parent())
        }
        MacroCallKind::Attr { .. } => at.value.parent(),
    };
    Some(at.with_value(around?))
}

/// Where rustc's walk of the crate comes to `node`: the places of the `mod` items and macro
/// calls it is reached through, outermost first, then its own. What a derive makes comes after
/// the item it is on.
pub(super) fn place(sema: &Semantics<'_, dyn HirDatabase>, node: &InFile<SyntaxNode>) -> Vec<(u32, u32)> {
    let mut place = vec![entered(node.value.text_range())];
    let mut file = node.file_id;
    loop {
        if let Some(call) = file.macro_file() {
            let loc = call.loc(sema.db);
            let at = loc.to_node(sema.db);
            place.push(match loc.kind {
                MacroCallKind::Derive { .. } => {
                    let item = at.value.ancestors().find(|it| ast::Adt::can_cast(it.kind()));
                    let end = item.map_or(at.value.text_range(), |it| it.text_range()).end();
                    (u32::from(end), 1)
                }
                _ => entered(at.value.text_range()),
            });
            file = at.file_id;
            continue;
        }
        match sema.find_parent_file(file) {
            Some(declared) => {
                place.push(entered(declared.value.text_range()));
                file = declared.file_id;
            }
            None => break,
        }
    }
    place.reverse();
    place
}

/// The place of entering a node of `range`: outer nodes before the nodes in them.
fn entered(range: TextRange) -> (u32, u32) {
    (u32::from(range.start()), u32::MAX - u32::from(range.end()))
}

/// `node` of its file, as a key.
pub(super) fn key(node: &InFile<SyntaxNode>) -> InFile<SyntaxNodePtr> {
    node.as_ref().map(SyntaxNodePtr::new)
}

/// rustc's `Span::in_external_macro`: made by an attribute or derive macro, or by a macro
/// of another crate, rather than handed to it. [`crate::cx::Cx::span`] puts a span a macro of
/// another crate makes where the macro is called, as rustc writes it, and one of this
/// crate's macros where the macro has it.
pub(super) fn external(span: &Span) -> bool {
    let Some(expansion) = &span.expansion else { return false };
    match expansion.kind {
        MacroKind::Attr | MacroKind::Derive => true,
        MacroKind::Bang => {
            let call = &expansion.call_site;
            span.file == call.file && span.lo == call.lo && span.hi == call.hi
        }
    }
}

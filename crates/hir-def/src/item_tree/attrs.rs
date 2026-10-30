//! Defines attribute helpers for name resolution.
//!
//! Notice we don't preserve all attributes for name resolution, to save space:
//! for example, we skip doc comments (desugared to `#[doc = "..."]` attributes)
//! and `#[inline]`. The filtered attributes are listed in [`hir_expand::attrs`].

use std::{
    borrow::Cow,
    convert::Infallible,
    ops::{self, ControlFlow},
};

use base_db::SourceDatabase;
use cfg::{CfgExpr, CfgOptions};
use either::Either;
use hir_expand::{
    attrs::{Attr, AttrId, AttrInput, collect_item_tree_attrs},
    mod_path::ModPath,
    name::Name,
};
use intern::{Interned, Symbol, sym};
use syntax::{AstNode, SyntaxKind, SyntaxNode, ast};
use syntax_bridge::DocCommentDesugarMode;
use tt::token_to_literal;

/// The attributes of one item, owned: what lowering one item's attributes alone gives (an
/// [`ItemTree`](super::ItemTree) keeps every item's in one array, read as [`AttrsOrCfgRef`]).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AttrsOrCfg {
    Enabled {
        attrs: AttrsOwned,
    },
    /// This only collects the attributes up to the disabled `cfg` (this is what needed for crate-level attributes.)
    CfgDisabled(Box<(CfgExpr, AttrsOwned)>),
}

impl Default for AttrsOrCfg {
    #[inline]
    fn default() -> Self {
        AttrsOrCfg::Enabled { attrs: AttrsOwned(Box::new([])) }
    }
}

impl AttrsOrCfg {
    pub(crate) fn lower<'a, S>(
        db: &dyn SourceDatabase,
        owner: &dyn ast::HasAttrs,
        cfg_options: &dyn Fn() -> &'a CfgOptions,
        span_map: S,
    ) -> AttrsOrCfg
    where
        S: syntax_bridge::SpanMapper + Copy,
    {
        let mut attrs = Vec::new();
        let cfg = lower_attrs_into(db, owner, cfg_options, span_map, &mut attrs);
        let attrs = AttrsOwned(attrs.into_boxed_slice());
        match cfg {
            Some(cfg) => AttrsOrCfg::CfgDisabled(Box::new((cfg, attrs))),
            None => AttrsOrCfg::Enabled { attrs },
        }
    }
}

/// Appends `owner`'s attributes to `attrs`, and returns the `cfg` that disables it, if one
/// does: the attributes appended are then those before that `cfg` (what crate-level attributes
/// need), so lowering a second owner after a disabled one is never needed.
pub(super) fn lower_attrs_into<'a, S>(
    db: &dyn SourceDatabase,
    owner: &dyn ast::HasAttrs,
    cfg_options: &dyn Fn() -> &'a CfgOptions,
    span_map: S,
    attrs: &mut Vec<Attr>,
) -> Option<CfgExpr>
where
    S: syntax_bridge::SpanMapper + Copy,
{
    let result = collect_item_tree_attrs::<Infallible>(owner, cfg_options, |meta, _| {
        // NOTE: We cannot early return from this function, *every* attribute must be pushed, otherwise we'll mess the `AttrId`
        // tracking.
        let path = meta.path();
        let path_range = path
            .as_ref()
            .map(|path| path.syntax().text_range())
            .unwrap_or_else(|| meta.syntax().text_range());
        let (span, input) = match &meta {
            ast::Meta::KeyValueMeta(meta) => {
                let span = span_map.span_for(path_range);
                let input = meta.expr().and_then(|value| {
                    if let ast::Expr::Literal(value) = value {
                        Some(Box::new(AttrInput::Literal(token_to_literal(
                            value.token().text(),
                            span_map.span_for(value.syntax().text_range()),
                        ))))
                    } else {
                        None
                    }
                });
                (span, input)
            }
            ast::Meta::TokenTreeMeta(meta) => {
                let span = span_map.span_for(path_range);
                let tt = syntax_bridge::syntax_node_to_token_tree(
                    &meta
                        .token_tree()
                        .map(|it| it.syntax().clone())
                        .unwrap_or_else(|| meta.syntax().clone()),
                    span_map,
                    span,
                    DocCommentDesugarMode::ProcMacro,
                );
                let input = Some(Box::new(AttrInput::TokenTree(tt)));
                (span, input)
            }
            ast::Meta::PathMeta(_) => {
                let span = span_map.span_for(path_range);
                (span, None)
            }
            ast::Meta::CfgMeta(_) | ast::Meta::CfgAttrMeta(_) | ast::Meta::UnsafeMeta(_) => {
                unreachable!(
                    "`cfg`, `cfg_attr` and `unsafe(...)` are handled in `collect_item_tree_attrs()`"
                )
            }
        };

        let path = path.and_then(|path| {
            ModPath::from_src(db, path, &mut |range| span_map.span_for(range).ctx)
        });
        let path = path.unwrap_or_else(|| Name::missing().into());

        attrs.push(Attr { path: Interned::new(path), input, ctxt: span.ctx });
        ControlFlow::Continue(())
    });
    match result {
        Some(Either::Right(cfg)) => Some(cfg),
        None => None,
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct AttrsOwned(Box<[Attr]>);

#[derive(Debug, Clone, Copy)]
pub(crate) struct Attrs<'a>(&'a [Attr]);

impl ops::Deref for Attrs<'_> {
    type Target = [Attr];

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.0
    }
}

/// The attributes of an item of an [`ItemTree`](super::ItemTree), borrowed from its columns.
#[derive(Debug, Clone, Copy)]
pub(crate) enum AttrsOrCfgRef<'a> {
    Enabled { attrs: Attrs<'a> },
    /// The `cfg` that is false, and the attributes before it.
    CfgDisabled(&'a CfgExpr, Attrs<'a>),
}

impl AttrsOwned {
    #[inline]
    pub(crate) fn as_ref(&self) -> Attrs<'_> {
        Attrs(&self.0)
    }
}

impl<'a> Attrs<'a> {
    pub(crate) const EMPTY: Self = Attrs(&[]);

    #[inline]
    pub(super) fn new(attrs: &'a [Attr]) -> Attrs<'a> {
        Attrs(attrs)
    }

    #[inline]
    pub(crate) fn by_key(self, key: Symbol) -> AttrQuery<'a> {
        AttrQuery { attrs: self, key }
    }

    #[inline]
    pub(crate) fn iter(self) -> impl Iterator<Item = (AttrId, &'a Attr)> {
        self.0.iter().enumerate().map(|(id, attr)| (AttrId::from_item_tree_index(id as u32), attr))
    }

    #[inline]
    pub(crate) fn iter_after(
        self,
        after: Option<AttrId>,
    ) -> impl Iterator<Item = (AttrId, &'a Attr)> {
        let skip = after.map_or(0, |after| after.item_tree_index() + 1);
        self.0[skip as usize..]
            .iter()
            .enumerate()
            .map(move |(id, attr)| (AttrId::from_item_tree_index(id as u32 + skip), attr))
    }

    #[inline]
    pub(crate) fn is_proc_macro(&self) -> bool {
        self.by_key(sym::proc_macro).exists()
    }

    #[inline]
    pub(crate) fn is_proc_macro_attribute(&self) -> bool {
        self.by_key(sym::proc_macro_attribute).exists()
    }
}
#[derive(Debug, Clone)]
pub(crate) struct AttrQuery<'attr> {
    attrs: Attrs<'attr>,
    key: Symbol,
}

impl<'attr> AttrQuery<'attr> {
    #[inline]
    pub(crate) fn tt_values(self) -> impl Iterator<Item = &'attr crate::tt::TopSubtree> {
        self.attrs().filter_map(|attr| attr.token_tree_value())
    }

    #[inline]
    pub(crate) fn string_value_with_span(self) -> Option<(&'attr str, span::Span)> {
        self.attrs().find_map(|attr| attr.string_value_with_span())
    }

    #[inline]
    pub(crate) fn string_value_unescape(self) -> Option<Cow<'attr, str>> {
        self.attrs().find_map(|attr| attr.string_value_unescape())
    }

    #[inline]
    pub(crate) fn exists(self) -> bool {
        self.attrs().next().is_some()
    }

    #[inline]
    pub(crate) fn attrs(self) -> impl Iterator<Item = &'attr Attr> + Clone {
        let key = self.key;
        self.attrs.0.iter().filter(move |attr| attr.path.as_ident().is_some_and(|s| *s == key))
    }
}

/// A `fn` whose body is entered for its inner attributes only when the body may have one.
/// An outline's lazy body (#1294) is parsed when a cursor enters it, and lowering an item tree
/// lowers no body: a body that surely starts with no `#!` stays unparsed.
#[derive(Clone)]
pub(super) struct OutlineFn(pub(super) ast::Fn);

impl AstNode for OutlineFn {
    fn can_cast(kind: SyntaxKind) -> bool {
        ast::Fn::can_cast(kind)
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        ast::Fn::cast(syntax).map(OutlineFn)
    }

    fn syntax(&self) -> &SyntaxNode {
        self.0.syntax()
    }
}

impl ast::HasAttrs for OutlineFn {
    fn inner_attributes_node(&self) -> Option<SyntaxNode> {
        if self.0.body_is_lazy() {
            let body = self.0.body()?;
            let Some((tree, at)) = body.syntax().flat() else {
                unreachable!("a lazy body outside a flat tree")
            };
            if !may_start_with_inner_attr(&tree.source()[tree.range(at)]) {
                return None;
            }
        }
        <ast::Fn as ast::HasAttrs>::inner_attributes_node(&self.0)
    }
}

/// Whether the text of a block, its `{` included, may start with an inner attribute after
/// whitespace and comments, as the parser's `inner_attrs` takes them: false only when it surely
/// does not. A non-ASCII character, an unterminated comment or a text that is not a block is a
/// maybe.
fn may_start_with_inner_attr(block: &str) -> bool {
    let Some(mut rest) = block.strip_prefix('{') else { return true };
    loop {
        rest = rest.trim_start_matches(|c: char| matches!(c, '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' '));
        if let Some(comment) = rest.strip_prefix("//") {
            match comment.find('\n') {
                Some(end) => rest = &comment[end..],
                None => return true,
            }
        } else if let Some(comment) = rest.strip_prefix("/*") {
            match block_comment_len(comment) {
                Some(end) => rest = &comment[end..],
                None => return true,
            }
        } else {
            return rest.chars().next().is_none_or(|c| c == '#' || !c.is_ascii());
        }
    }
}

/// How long the rest of a block comment is, after its opening `/*`, to just past the `*/` that
/// closes it, nested comments included as the lexer counts them.
fn block_comment_len(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let (mut depth, mut at) = (1usize, 0);
    while at + 1 < bytes.len() {
        match (bytes[at], bytes[at + 1]) {
            (b'/', b'*') => {
                depth += 1;
                at += 2;
            }
            (b'*', b'/') => {
                depth -= 1;
                at += 2;
                if depth == 0 {
                    return Some(at);
                }
            }
            _ => at += 1,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::may_start_with_inner_attr;

    #[test]
    fn a_block_may_start_with_an_inner_attribute() {
        for maybe in [
            "{ #![allow(unused)] }",
            "{#!}",
            "{ // a comment\n #![a] }",
            "{ /* a /* nested */ comment */ #![a] }",
            "{ //! inner docs\n#![a] }",
            "{ /// outer docs\n #[a] x }",
            "{ \u{2028} x }",
            "{ /* unterminated ",
            "{ // no newline",
            "not a block",
            "{",
        ] {
            assert!(may_start_with_inner_attr(maybe), "{maybe:?}");
        }
    }

    #[test]
    fn a_block_surely_starts_with_no_inner_attribute() {
        for not in [
            "{}",
            "{ }",
            "{ let x = 1; #![a] }",
            "{ // #![a]\n x }",
            "{ /* #![a] /* #![b] */ #![c] */ x }",
            "{\r\n\t\u{0B}\u{0C} foo() }",
        ] {
            assert!(!may_start_with_inner_attr(not), "{not:?}");
        }
    }
}

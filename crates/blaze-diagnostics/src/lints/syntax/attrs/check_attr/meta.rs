//! The arguments of an attribute as rustc parses them into meta items (`MetaItemInner`,
//! `MetaItem`, `MetaItemKind`), from the tokens of `#[path(..)]`: what `meta_item_list` and the
//! attribute parsers' `MetaItemListParser` see.
//!
//! Follows rustc's `compiler/rustc_parse/src/parser/attr.rs` (`parse_meta_seq_top`,
//! `parse_meta_item_inner`, `parse_meta_item`, `parse_meta_item_kind`) and
//! `compiler/rustc_ast/src/attr/mod.rs` (`MetaItemKind::list_from_tokens`) (MIT or Apache-2.0).

use syntax::{AstNode, NodeOrToken, SyntaxElement, SyntaxKind, SyntaxToken, TextRange, ast};

/// One element of a meta-item list: rustc's `MetaItemInner`.
#[derive(Clone, Debug)]
pub(super) enum Inner {
    Item(Item),
    /// A literal: `8`, `"x"`, `true`.
    Lit(SyntaxToken),
}

/// `path`, `path(..)` or `path = lit`: rustc's `MetaItem`.
#[derive(Clone, Debug)]
pub(super) struct Item {
    /// Its path's segments, without a leading `::`.
    segments: Vec<SyntaxToken>,
    /// Whether its path starts with `::`.
    rooted: bool,
    pub(super) args: ItemArgs,
    /// From its path to the end of its arguments: rustc's `MetaItem::span`.
    pub(super) range: TextRange,
}

/// rustc's `MetaItemKind`.
#[derive(Clone, Debug)]
pub(super) enum ItemArgs {
    Word,
    List(ast::TokenTree),
    NameValue(SyntaxToken),
}

impl Inner {
    pub(super) fn item(&self) -> Option<&Item> {
        match self {
            Inner::Item(item) => Some(item),
            Inner::Lit(_) => None,
        }
    }

    pub(super) fn range(&self) -> TextRange {
        match self {
            Inner::Item(item) => item.range,
            Inner::Lit(lit) => lit.text_range(),
        }
    }
}

impl Item {
    /// Its path when it is one plain word: rustc's `word_sym` of it.
    pub(super) fn word(&self) -> Option<&str> {
        match self.segments.as_slice() {
            [it] if !self.rooted => Some(it.text()),
            _ => None,
        }
    }

    /// Whether its path is the one word `name` (rustc's `path == sym::name`).
    pub(super) fn is(&self, name: &str) -> bool {
        self.word() == Some(name)
    }

    pub(super) fn is_word(&self) -> bool {
        matches!(self.args, ItemArgs::Word)
    }
}

/// The items of `tt` when it is `(..)` and its tokens are a meta-item list, as rustc's
/// `meta_item_list` gives them; `None` for `[..]`, `{..}` or tokens that are not one.
pub(super) fn list(tt: &ast::TokenTree) -> Option<Vec<Inner>> {
    let mut elements: Vec<SyntaxElement> =
        tt.syntax().children_with_tokens().filter(|it| !it.kind().is_trivia()).collect();
    if elements.last()?.kind() != SyntaxKind::R_PAREN || elements.first()?.kind() != SyntaxKind::L_PAREN {
        return None;
    }
    elements.pop();
    let mut cursor = Cursor { elements: &elements, at: 1 };
    let mut items = Vec::new();
    while !cursor.done() {
        items.push(cursor.inner()?);
        if cursor.done() {
            break;
        }
        cursor.token(SyntaxKind::COMMA)?;
    }
    Some(items)
}

struct Cursor<'a> {
    elements: &'a [SyntaxElement],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn done(&self) -> bool {
        self.at >= self.elements.len()
    }

    fn peek(&self) -> Option<&'a SyntaxElement> {
        self.elements.get(self.at)
    }

    fn peek_token(&self) -> Option<&'a SyntaxToken> {
        self.peek()?.as_token()
    }

    /// The next token, when it is of `kind`.
    fn token(&mut self, kind: SyntaxKind) -> Option<SyntaxToken> {
        let token = self.peek_token().filter(|it| it.kind() == kind)?.clone();
        self.at += 1;
        Some(token)
    }

    /// `::`, written as one token or as two `:` with nothing between them.
    fn path_sep(&mut self) -> Option<()> {
        let first = self.peek_token()?;
        match first.kind() {
            SyntaxKind::COLON2 => {
                self.at += 1;
                Some(())
            }
            SyntaxKind::COLON => {
                let second = self.elements.get(self.at + 1)?.as_token()?;
                if second.kind() != SyntaxKind::COLON || second.text_range().start() != first.text_range().end() {
                    return None;
                }
                self.at += 2;
                Some(())
            }
            _ => None,
        }
    }

    fn at_path_sep(&self) -> bool {
        self.peek_token().is_some_and(|it| matches!(it.kind(), SyntaxKind::COLON2 | SyntaxKind::COLON))
    }

    /// rustc's `parse_meta_item_inner`: a literal, else a meta item.
    fn inner(&mut self) -> Option<Inner> {
        if let Some(lit) = self.peek_token().filter(|it| is_lit(it.kind())).cloned() {
            self.at += 1;
            return Some(Inner::Lit(lit));
        }
        self.item().map(Inner::Item)
    }

    /// rustc's `parse_meta_item`: a path, then `(..)`, `= lit` or nothing.
    fn item(&mut self) -> Option<Item> {
        let start = self.peek()?.text_range().start();
        let rooted = self.at_path_sep();
        if rooted {
            self.path_sep()?;
        }
        let mut segments = vec![self.segment()?];
        while self.at_path_sep() {
            self.path_sep()?;
            segments.push(self.segment()?);
        }
        let mut end = segments.last()?.text_range().end();
        let args = match self.peek() {
            Some(NodeOrToken::Node(node)) if node.kind() == SyntaxKind::TOKEN_TREE => {
                let tt = ast::TokenTree::cast(node.clone())?;
                tt.l_paren_token()?;
                end = node.text_range().end();
                self.at += 1;
                ItemArgs::List(tt)
            }
            Some(NodeOrToken::Token(token)) if token.kind() == SyntaxKind::EQ => {
                self.at += 1;
                let lit = self.peek_token().filter(|it| is_lit(it.kind()))?.clone();
                self.at += 1;
                end = lit.text_range().end();
                ItemArgs::NameValue(lit)
            }
            _ => ItemArgs::Word,
        };
        Some(Item { segments, rooted, args, range: TextRange::new(start, end) })
    }

    /// One segment of a path: an identifier, or `self`, `super`, `crate`, `Self`.
    fn segment(&mut self) -> Option<SyntaxToken> {
        let token = self.peek_token()?.clone();
        match token.kind() {
            SyntaxKind::IDENT
            | SyntaxKind::SELF_KW
            | SyntaxKind::SUPER_KW
            | SyntaxKind::CRATE_KW
            | SyntaxKind::SELF_TYPE_KW => {
                self.at += 1;
                Some(token)
            }
            _ => None,
        }
    }
}

/// Whether a token is a literal rustc takes as a `MetaItemLit`.
fn is_lit(kind: SyntaxKind) -> bool {
    kind.is_literal() || matches!(kind, SyntaxKind::TRUE_KW | SyntaxKind::FALSE_KW)
}

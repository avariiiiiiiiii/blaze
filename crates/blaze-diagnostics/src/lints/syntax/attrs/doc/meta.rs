//! rustc's meta-item parser over an attribute's token tree, as `#[doc(..)]`'s parser reads it:
//! literals, paths, `path(..)` lists and `path = literal`. Where rustc's parser gives an error
//! (a missing comma, a non-literal value, a keyword for a name) the list is `None`: rustc gives
//! that attribute no lints.
//!
//! Follows rustc's `compiler/rustc_attr_parsing/src/parser.rs` (`MetaItemListParserContext`:
//! `parse_meta_item_inner`, `parse_meta_item`, `should_continue_parsing_meta_items`;
//! `ArgParser::span`, `MetaItemParser::span`, `PathParser::word_sym`) (MIT or Apache-2.0).

use syntax::{
    AstNode, AstToken, NodeOrToken, SyntaxKind, SyntaxNode, SyntaxToken, TextRange,
    ast::{self, IsString as _},
};

/// A literal: rustc's `MetaItemLit`.
#[derive(Clone, Debug)]
pub(crate) struct Lit {
    pub(crate) token: SyntaxToken,
}

impl Lit {
    fn of(token: &SyntaxToken) -> Option<Lit> {
        is_lit(token.kind()).then(|| Lit { token: token.clone() })
    }

    pub(crate) fn range(&self) -> TextRange {
        self.token.text_range()
    }

    /// rustc's `value_as_str`: the value of a string literal, unescaped; `None` for any other
    /// literal and for one that does not unescape.
    pub(crate) fn str(&self) -> Option<String> {
        if self.token.kind() != SyntaxKind::STRING || self.suffixed() {
            return None;
        }
        ast::String::cast(self.token.clone())?.value().ok().map(|it| it.into_owned())
    }

    pub(crate) fn is_bool(&self) -> bool {
        matches!(self.token.kind(), SyntaxKind::TRUE_KW | SyntaxKind::FALSE_KW)
    }

    /// rustc's `MetaItemLit::symbol`: the literal as written, without its quotes, prefix and
    /// suffix.
    pub(crate) fn symbol(&self) -> String {
        let text = self.token.text();
        let token = self.token.clone();
        match self.token.kind() {
            SyntaxKind::STRING => ast::String::cast(token).map(|it| it.text_without_quotes().to_owned()),
            SyntaxKind::BYTE_STRING => ast::ByteString::cast(token).map(|it| it.text_without_quotes().to_owned()),
            SyntaxKind::C_STRING => ast::CString::cast(token).map(|it| it.text_without_quotes().to_owned()),
            SyntaxKind::CHAR | SyntaxKind::BYTE => {
                let (Some(open), Some(close)) = (text.find('\''), text.rfind('\'')) else { return text.to_owned() };
                text.get(open + 1..close).map(str::to_owned)
            }
            SyntaxKind::INT_NUMBER => ast::IntNumber::cast(token).map(|it| {
                let (prefix, digits, _) = it.split_into_parts();
                format!("{prefix}{digits}")
            }),
            SyntaxKind::FLOAT_NUMBER => ast::FloatNumber::cast(token).map(|it| it.split_into_parts().0.to_owned()),
            _ => None,
        }
        .unwrap_or_else(|| text.to_owned())
    }

    /// Whether it has a suffix (`1u8`, `"a"x`): rustc's `!is_unsuffixed()`.
    pub(crate) fn suffixed(&self) -> bool {
        let text = self.token.text();
        let token = self.token.clone();
        match self.token.kind() {
            SyntaxKind::INT_NUMBER => ast::IntNumber::cast(token).is_some_and(|it| it.suffix().is_some()),
            SyntaxKind::FLOAT_NUMBER => ast::FloatNumber::cast(token).is_some_and(|it| it.suffix().is_some()),
            SyntaxKind::STRING | SyntaxKind::BYTE_STRING | SyntaxKind::C_STRING => {
                text.rfind('"').is_some_and(|close| !text[close + 1..].trim_start_matches('#').is_empty())
            }
            SyntaxKind::CHAR | SyntaxKind::BYTE => text.rfind('\'').is_some_and(|close| close + 1 < text.len()),
            _ => false,
        }
    }
}

/// A meta item's path: rustc's `PathParser`.
#[derive(Clone, Debug)]
pub(crate) struct Path {
    /// Its segments; a leading `::` is rustc's `{{root}}` segment.
    pub(crate) segments: Vec<String>,
    pub(crate) range: TextRange,
}

impl Path {
    /// rustc's `word_sym`: the name of a path of one segment.
    pub(crate) fn word(&self) -> Option<&str> {
        match self.segments.as_slice() {
            [it] => Some(it),
            _ => None,
        }
    }

    /// The path as rustc's messages print it: its segments joined by `::`.
    pub(crate) fn name(&self) -> String {
        self.segments.join("::")
    }
}

/// A meta item's arguments: rustc's `ArgParser`.
#[derive(Clone, Debug)]
pub(crate) enum MetaArgs {
    None,
    List(List),
    NameValue { eq: TextRange, value: Lit },
}

impl MetaArgs {
    /// rustc's `ArgParser::span`: the list with its parentheses, or `= value`.
    pub(crate) fn range(&self) -> Option<TextRange> {
        match self {
            MetaArgs::None => None,
            MetaArgs::List(list) => Some(list.range),
            MetaArgs::NameValue { eq, value } => Some(TextRange::new(eq.start(), value.range().end())),
        }
    }
}

/// `path`, `path(..)` or `path = literal`: rustc's `MetaItemParser`.
#[derive(Clone, Debug)]
pub(crate) struct MetaItem {
    pub(crate) path: Path,
    pub(crate) args: MetaArgs,
}

impl MetaItem {
    /// rustc's `MetaItemParser::span`: from its path to the end of its arguments.
    pub(crate) fn range(&self) -> TextRange {
        match self.args.range() {
            Some(args) => TextRange::new(self.path.range.start(), args.end()),
            None => self.path.range,
        }
    }
}

/// An item of a list: rustc's `MetaItemOrLitParser`.
#[derive(Clone, Debug)]
pub(crate) enum Item {
    Meta(MetaItem),
    Lit(Lit),
}

impl Item {
    pub(crate) fn range(&self) -> TextRange {
        match self {
            Item::Meta(it) => it.range(),
            Item::Lit(it) => it.range(),
        }
    }
}

/// `(..)` and the items in it: rustc's `MetaItemListParser`.
#[derive(Clone, Debug)]
pub(crate) struct List {
    pub(crate) items: Vec<Item>,
    /// With its parentheses.
    pub(crate) range: TextRange,
}

/// The list `tree` is, or `None` when it is not one rustc parses: another delimiter than `(`,
/// or anything but meta items and literals separated by commas in it.
pub(crate) fn list(tree: &ast::TokenTree) -> Option<List> {
    let elements: Vec<NodeOrToken<SyntaxNode, SyntaxToken>> =
        tree.syntax().children_with_tokens().filter(|it| !it.kind().is_trivia()).collect();
    let (first, last) = (elements.first()?, elements.last()?);
    if elements.len() < 2 || first.kind() != SyntaxKind::L_PAREN || last.kind() != SyntaxKind::R_PAREN {
        return None;
    }
    let mut parser = Parser { elements: &elements[1..elements.len() - 1], pos: 0 };
    let mut items = Vec::new();
    while !parser.at_end() {
        items.push(parser.item()?);
        if parser.at_end() {
            break;
        }
        if !parser.eat(SyntaxKind::COMMA) {
            return None;
        }
    }
    Some(List { items, range: tree.syntax().text_range() })
}

/// Whether a token is a literal of a meta item: rustc's `eat_token_lit` (`true` and `false`
/// included).
fn is_lit(kind: SyntaxKind) -> bool {
    kind.is_literal() || matches!(kind, SyntaxKind::TRUE_KW | SyntaxKind::FALSE_KW)
}

/// Whether a token is a segment of a path rustc's `parse_path` takes: an identifier or a path
/// keyword (`self`, `super`, `crate`, `Self`).
fn is_segment(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::IDENT | SyntaxKind::SELF_KW | SyntaxKind::SUPER_KW | SyntaxKind::CRATE_KW | SyntaxKind::SELF_TYPE_KW
    )
}

struct Parser<'a> {
    elements: &'a [NodeOrToken<SyntaxNode, SyntaxToken>],
    pos: usize,
}

impl Parser<'_> {
    fn at_end(&self) -> bool {
        self.pos >= self.elements.len()
    }

    fn token(&self, at: usize) -> Option<&SyntaxToken> {
        self.elements.get(at).and_then(NodeOrToken::as_token)
    }

    fn eat(&mut self, kind: SyntaxKind) -> bool {
        let at = self.token(self.pos).is_some_and(|it| it.kind() == kind);
        self.pos += usize::from(at);
        at
    }

    /// The range of a `::` at the cursor, written as one token or as two `:` with nothing
    /// between them, and how many tokens it is.
    fn colon2(&self) -> Option<(TextRange, usize)> {
        let first = self.token(self.pos)?;
        match first.kind() {
            SyntaxKind::COLON2 => Some((first.text_range(), 1)),
            SyntaxKind::COLON => {
                let second = self.token(self.pos + 1)?;
                (second.kind() == SyntaxKind::COLON && second.text_range().start() == first.text_range().end())
                    .then(|| (first.text_range().cover(second.text_range()), 2))
            }
            _ => None,
        }
    }

    fn segment(&mut self) -> Option<(String, TextRange)> {
        let token = self.token(self.pos).filter(|it| is_segment(it.kind()))?.clone();
        self.pos += 1;
        let text = token.text();
        Some((text.strip_prefix("r#").unwrap_or(text).to_owned(), token.text_range()))
    }

    /// rustc's `parse_path(PathStyle::Mod)`.
    fn path(&mut self) -> Option<Path> {
        let mut segments = Vec::new();
        let start = match self.colon2() {
            Some((range, len)) => {
                self.pos += len;
                segments.push("{{root}}".to_owned());
                range.start()
            }
            None => self.token(self.pos)?.text_range().start(),
        };
        let (name, mut end) = self.segment()?;
        segments.push(name);
        while let Some((_, len)) = self.colon2() {
            self.pos += len;
            let (name, range) = self.segment()?;
            segments.push(name);
            end = range;
        }
        Some(Path { segments, range: TextRange::new(start, end.end()) })
    }

    /// rustc's `parse_meta_item_inner`.
    fn item(&mut self) -> Option<Item> {
        if let Some(lit) = self.token(self.pos).and_then(Lit::of) {
            self.pos += 1;
            return Some(Item::Lit(lit));
        }
        let path = self.path()?;
        let args = match self.elements.get(self.pos) {
            Some(NodeOrToken::Node(node)) if node.first_token().is_some_and(|it| it.kind() == SyntaxKind::L_PAREN) => {
                let tree = ast::TokenTree::cast(node.clone())?;
                self.pos += 1;
                MetaArgs::List(list(&tree)?)
            }
            Some(NodeOrToken::Token(token)) if token.kind() == SyntaxKind::EQ => {
                let eq = token.text_range();
                let value = self.token(self.pos + 1).and_then(Lit::of)?;
                self.pos += 2;
                MetaArgs::NameValue { eq, value }
            }
            _ => MetaArgs::None,
        };
        Some(Item::Meta(MetaItem { path, args }))
    }
}

#[cfg(test)]
mod tests {
    use syntax::{Edition, SourceFile};

    use super::*;

    fn first_list(text: &str) -> Option<List> {
        let file = SourceFile::parse(text, Edition::CURRENT).tree();
        let tree = file.syntax().descendants().find_map(ast::TokenTree::cast).unwrap();
        list(&tree)
    }

    fn shape(list: &List) -> Vec<String> {
        list.items
            .iter()
            .map(|it| match it {
                Item::Lit(lit) => format!("lit {}", lit.symbol()),
                Item::Meta(meta) => match &meta.args {
                    MetaArgs::None => meta.path.name(),
                    MetaArgs::List(list) => format!("{}({})", meta.path.name(), shape(list).join(", ")),
                    MetaArgs::NameValue { value, .. } => format!("{} = {}", meta.path.name(), value.symbol()),
                },
            })
            .collect()
    }

    #[test]
    fn a_list_parses_as_rustcs_meta_items() {
        let list = first_list(r#"#[doc(foo::bar, crate::bar::baz = "bye", test(attr(allow(x))), 123,)] fn f() {}"#)
            .unwrap();
        assert_eq!(shape(&list), ["foo::bar", "crate::bar::baz = bye", "test(attr(allow(x)))", "lit 123"]);
    }

    #[test]
    fn spans_are_rustcs() {
        let text = r#"#[doc(hidden = true, inline("x"))] fn f() {}"#;
        let list = first_list(text).unwrap();
        let ranges: Vec<&str> = list
            .items
            .iter()
            .map(|it| match it {
                Item::Meta(meta) => &text[meta.args.range().unwrap()],
                Item::Lit(_) => unreachable!(),
            })
            .collect();
        assert_eq!(ranges, ["= true", "(\"x\")"]);
        assert_eq!(&text[list.range], r#"(hidden = true, inline("x"))"#);
    }

    #[test]
    fn what_rustc_cannot_parse_is_none() {
        assert!(first_list("#[doc(a b)] fn f() {}").is_none());
        assert!(first_list("#[doc(a = b)] fn f() {}").is_none());
        assert!(first_list("#[doc(-1)] fn f() {}").is_none());
        assert!(first_list("#[doc[a]] fn f() {}").is_none());
        assert!(first_list("#[doc(type)] fn f() {}").is_none());
    }

    #[test]
    fn a_literal_is_read_as_rustc_reads_it() {
        let list = first_list(r#"#[doc(a = "x\n", b = r"y", c = 1u8, d = true)] fn f() {}"#).unwrap();
        let values: Vec<(String, Option<String>, bool)> = list
            .items
            .iter()
            .map(|it| match it {
                Item::Meta(MetaItem { args: MetaArgs::NameValue { value, .. }, .. }) => {
                    (value.symbol(), value.str(), value.suffixed())
                }
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(
            values,
            [
                ("x\\n".to_owned(), Some("x\n".to_owned()), false),
                ("y".to_owned(), Some("y".to_owned()), false),
                ("1".to_owned(), None, true),
                ("true".to_owned(), None, false),
            ]
        );
    }
}

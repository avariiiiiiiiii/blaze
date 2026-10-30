//! The arguments of a diagnostic attribute as rustc's attribute parser reads them: a list of
//! meta items and literals, parsed without recovery, any error making it the empty list.
//!
//! Follows rustc's `compiler/rustc_attr_parsing/src/parser.rs` (`ArgParser::from_attr_args`,
//! `MetaItemListParserContext`, `expr_to_lit`) (MIT or Apache-2.0).

use syntax::{AstNode, AstToken, NodeOrToken, SyntaxKind, SyntaxToken, TextRange, ast};

use super::super::Args as Written;

/// rustc's `ArgParser`.
#[derive(Clone, Debug)]
pub(super) enum Args {
    NoArgs,
    List(List),
    /// `= value`; `range` is from the `=` to the end of the value.
    NameValue { range: TextRange, value: Lit },
}

impl Args {
    /// rustc's `ArgParser::span`.
    pub(super) fn range(&self) -> Option<TextRange> {
        match self {
            Args::NoArgs => None,
            Args::List(list) => Some(list.range),
            Args::NameValue { range, .. } => Some(*range),
        }
    }
}

/// rustc's `MetaItemListParser`: its items and where its delimiters are.
#[derive(Clone, Debug)]
pub(super) struct List {
    pub(super) items: Vec<Item>,
    pub(super) range: TextRange,
}

#[derive(Clone, Debug)]
pub(super) enum Item {
    Meta(MetaItem),
    Lit(Lit),
}

impl Item {
    pub(super) fn range(&self) -> TextRange {
        match self {
            Item::Meta(it) => it.range(),
            Item::Lit(it) => it.range(),
        }
    }
}

/// rustc's `MetaItemParser`.
#[derive(Clone, Debug)]
pub(super) struct MetaItem {
    /// Each segment's name (a raw one's without the `r#`) and where it is written.
    pub(super) path: Vec<(String, TextRange)>,
    pub(super) path_range: TextRange,
    pub(super) args: Args,
}

impl MetaItem {
    /// The name of a path of one segment.
    pub(super) fn ident(&self) -> Option<&str> {
        match &self.path[..] {
            [(name, _)] => Some(name),
            _ => None,
        }
    }

    pub(super) fn range(&self) -> TextRange {
        match self.args.range() {
            Some(args) => self.path_range.cover(args),
            None => self.path_range,
        }
    }

    /// The path as rustc prints it.
    pub(super) fn path_text(&self) -> String {
        let mut text = String::new();
        for (i, (name, _)) in self.path.iter().enumerate() {
            if i > 0 {
                text.push_str("::");
            }
            text.push_str(name);
        }
        text
    }
}

/// An unsuffixed literal: its token, and a string literal's value.
#[derive(Clone, Debug)]
pub(super) struct Lit {
    pub(super) token: SyntaxToken,
    pub(super) str: Option<String>,
}

impl Lit {
    pub(super) fn range(&self) -> TextRange {
        self.token.text_range()
    }

    /// The literal as it is written: rustc's snippet of its span.
    pub(super) fn text(&self) -> &str {
        self.token.text()
    }

    /// The token `token`, when it is an unsuffixed literal rustc takes (a string literal's
    /// escapes valid).
    fn of(token: &SyntaxToken) -> Option<Lit> {
        let text = token.text();
        let quoted = || text.ends_with(['"', '#']);
        let (valid, str) = match token.kind() {
            SyntaxKind::STRING => match ast::String::cast(token.clone())?.value() {
                Ok(value) => (quoted(), Some(value.into_owned())),
                Err(_) => (false, None),
            },
            SyntaxKind::BYTE_STRING => (quoted() && ast::ByteString::cast(token.clone())?.value().is_ok(), None),
            SyntaxKind::C_STRING => (quoted() && ast::CString::cast(token.clone())?.value().is_ok(), None),
            SyntaxKind::CHAR => (text.ends_with('\'') && ast::Char::cast(token.clone())?.value().is_ok(), None),
            SyntaxKind::BYTE => (text.ends_with('\'') && ast::Byte::cast(token.clone())?.value().is_ok(), None),
            SyntaxKind::INT_NUMBER => (ast::IntNumber::cast(token.clone())?.suffix().is_none(), None),
            SyntaxKind::FLOAT_NUMBER => (ast::FloatNumber::cast(token.clone())?.suffix().is_none(), None),
            SyntaxKind::TRUE_KW | SyntaxKind::FALSE_KW => (true, None),
            _ => (false, None),
        };
        valid.then(|| Lit { token: token.clone(), str })
    }

    /// Whether `token` is a literal token, suffixed or not.
    fn is_literal(token: &SyntaxToken) -> bool {
        matches!(
            token.kind(),
            SyntaxKind::STRING
                | SyntaxKind::BYTE_STRING
                | SyntaxKind::C_STRING
                | SyntaxKind::CHAR
                | SyntaxKind::BYTE
                | SyntaxKind::INT_NUMBER
                | SyntaxKind::FLOAT_NUMBER
                | SyntaxKind::TRUE_KW
                | SyntaxKind::FALSE_KW
        )
    }
}

/// The arguments of an attribute: `None` when rustc drops the attribute, its value not an
/// unsuffixed literal. A `#[diagnostic::..]` attribute's list may have any delimiter and
/// is empty where it does not parse; another's (`#[rustc_on_unimplemented]`) is dropped then.
pub(super) fn args(written: Written, diagnostic: bool) -> Option<Args> {
    Some(match written {
        Written::Word => Args::NoArgs,
        Written::List(tt) => {
            let range = tt.syntax().text_range();
            let items = if diagnostic {
                items(&tt).unwrap_or_default()
            } else {
                tt.left_delimiter_token().filter(|it| it.kind() == SyntaxKind::L_PAREN)?;
                items(&tt)?
            };
            Args::List(List { items, range })
        }
        Written::NameValue(expr) => {
            let ast::Expr::Literal(lit) = expr? else { return None };
            let token = lit.token();
            let value = Lit::of(&token)?;
            let eq = lit.syntax().parent().and_then(|it| {
                it.children_with_tokens().filter_map(NodeOrToken::into_token).find(|it| it.kind() == SyntaxKind::EQ)
            });
            let range = match eq {
                Some(eq) => eq.text_range().cover(value.range()),
                None => value.range(),
            };
            Args::NameValue { range, value }
        }
    })
}

/// A token of a token tree, trivia left out, or a delimited group in it.
type Tree = NodeOrToken<ast::TokenTree, SyntaxToken>;

/// The items of the delimited `tt`, `None` where rustc's parser stops with an error: rustc's
/// `MetaItemListParserContext::parse` and, for a nested list, `parse_paren_comma_seq`.
fn items(tt: &ast::TokenTree) -> Option<Vec<Item>> {
    let trees = inside(tt);
    let mut parser = Parser { trees: &trees, at: 0 };
    let mut items = Vec::with_capacity(1);
    while !parser.at_end() {
        items.push(parser.item()?);
        if parser.eat(SyntaxKind::COMMA).is_none() && !parser.at_end() {
            return None;
        }
    }
    Some(items)
}

/// What is between the delimiters of `tt`.
fn inside(tt: &ast::TokenTree) -> Vec<Tree> {
    let mut trees: Vec<Tree> = tt
        .token_trees_and_tokens()
        .filter(|it| match it {
            NodeOrToken::Token(token) => !token.kind().is_trivia(),
            NodeOrToken::Node(_) => true,
        })
        .collect();
    if let Some(NodeOrToken::Token(first)) = trees.first()
        && first.kind().is_l_delimiter()
    {
        trees.remove(0);
    }
    if let Some(NodeOrToken::Token(last)) = trees.last()
        && last.kind().is_r_delimiter()
    {
        trees.pop();
    }
    trees
}

struct Parser<'a> {
    trees: &'a [Tree],
    at: usize,
}

impl Parser<'_> {
    fn at_end(&self) -> bool {
        self.at == self.trees.len()
    }

    fn token(&self) -> Option<&SyntaxToken> {
        match self.trees.get(self.at)? {
            NodeOrToken::Token(token) => Some(token),
            NodeOrToken::Node(_) => None,
        }
    }

    fn eat(&mut self, kind: SyntaxKind) -> Option<SyntaxToken> {
        let token = self.token().filter(|it| it.kind() == kind)?.clone();
        self.at += 1;
        Some(token)
    }

    /// `::`, written as one token or two `:` next to each other.
    fn eat_path_sep(&mut self) -> Option<TextRange> {
        if let Some(token) = self.eat(SyntaxKind::COLON2) {
            return Some(token.text_range());
        }
        let first = self.token().filter(|it| it.kind() == SyntaxKind::COLON)?;
        let second = match self.trees.get(self.at + 1)? {
            NodeOrToken::Token(it) if it.kind() == SyntaxKind::COLON => it,
            _ => return None,
        };
        if first.text_range().end() != second.text_range().start() {
            return None;
        }
        let range = first.text_range().cover(second.text_range());
        self.at += 2;
        Some(range)
    }

    /// rustc's `parse_meta_item_inner`.
    fn item(&mut self) -> Option<Item> {
        if let Some(token) = self.token()
            && Lit::is_literal(token)
        {
            let lit = Lit::of(token)?;
            self.at += 1;
            return Some(Item::Lit(lit));
        }
        self.meta_item().map(Item::Meta)
    }

    /// rustc's `parse_meta_item`, its path parsed as `PathStyle::Mod`.
    fn meta_item(&mut self) -> Option<MetaItem> {
        let leading = self.eat_path_sep();
        let mut path = Vec::with_capacity(2);
        loop {
            path.push(self.segment()?);
            let at = self.at;
            if self.eat_path_sep().is_none() {
                break;
            }
            if !self.token().is_some_and(is_segment) {
                self.at = at;
                return None;
            }
        }
        let start = leading.unwrap_or(path[0].1);
        let path_range = start.cover(path[path.len() - 1].1);
        let args = match self.trees.get(self.at) {
            Some(NodeOrToken::Node(group))
                if group.left_delimiter_token().is_some_and(|it| it.kind() == SyntaxKind::L_PAREN) =>
            {
                self.at += 1;
                let range = group.syntax().text_range();
                let items = items(group)?;
                Args::List(List { items, range })
            }
            _ => match self.eat(SyntaxKind::EQ) {
                Some(eq) => {
                    let value = Lit::of(self.token()?)?;
                    self.at += 1;
                    Args::NameValue { range: eq.text_range().cover(value.range()), value }
                }
                None => Args::NoArgs,
            },
        };
        Some(MetaItem { path, path_range, args })
    }

    /// A path segment: an identifier that is not a reserved keyword, or `self`, `Self`,
    /// `super` or `crate`.
    fn segment(&mut self) -> Option<(String, TextRange)> {
        let token = self.token().filter(|it| is_segment(it))?.clone();
        self.at += 1;
        let text = token.text();
        let name = text.strip_prefix("r#").unwrap_or(text);
        Some((name.to_owned(), token.text_range()))
    }
}

fn is_segment(token: &SyntaxToken) -> bool {
    matches!(
        token.kind(),
        SyntaxKind::IDENT | SyntaxKind::SELF_KW | SyntaxKind::SELF_TYPE_KW | SyntaxKind::SUPER_KW | SyntaxKind::CRATE_KW
    )
}

//! The proc-macro server token stream implementation.

use core::fmt;
use std::{mem, ops::Range, sync::Arc};

use blaze_mem::{Frame, Tag, frame};
use intern::Symbol;
use rustc_lexer::{DocStyle, LiteralKind};
use rustc_proc_macro::Delimiter;

use crate::bridge::{DelimSpan, Group, Ident, IdentKind, LitKind, Literal, Punct, TokenTree};

/// Trait for allowing tests to parse tokenstreams with dynamic span ranges
pub(crate) trait SpanLike {
    fn derive_ranged(&self, range: std::ops::Range<usize>) -> Self;
}

#[derive(Clone)]
pub struct TokenStream<S>(pub(crate) Arc<Vec<TokenTree<S>>>);

impl<S> Default for TokenStream<S> {
    fn default() -> Self {
        Self(Default::default())
    }
}

impl<S> TokenStream<S> {
    pub fn new(tts: Vec<TokenTree<S>>) -> TokenStream<S> {
        // alloc: a stream's one shared handle, which the macro owns with its trees.
        TokenStream(Arc::new(tts))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn iter(&self) -> TokenStreamIter<'_, S> {
        TokenStreamIter::new(self)
    }

    pub fn as_single_group(&self) -> Option<&Group<S>> {
        match &**self.0 {
            [TokenTree::Group(group)] => Some(group),
            _ => None,
        }
    }

    /// The stream `s` lexes to, each token at `span` ranged to it: the bridge's
    /// `TokenStream::from_str`. The open groups' trees wait in the call's frame, and each
    /// group's move into a `Vec` of their number once it closes.
    pub(crate) fn from_str(s: &str, span: S) -> Result<Self, String>
    where
        S: SpanLike + Copy,
    {
        frame::task(Tag::Expand, |frame| Self::from_str_in(s, span, frame))
    }

    fn from_str_in(s: &str, span: S, frame: &Frame) -> Result<Self, String>
    where
        S: SpanLike + Copy,
    {
        let mut pending: Vec<TokenTree<S>, &Frame> = Vec::new_in(frame);
        // Each open group's delimiter, its opening range and where its trees start in `pending`.
        let mut groups: Vec<(Delimiter, Range<usize>, usize), &Frame> = Vec::new_in(frame);
        groups.push((Delimiter::None, 0..0, 0));
        let mut offset = 0;
        let mut tokens = rustc_lexer::tokenize(s, rustc_lexer::FrontmatterAllowed::No).peekable();
        while let Some(token) = tokens.next() {
            let range = offset..offset + token.len as usize;
            offset += token.len as usize;

            let mut is_joint = || {
                tokens.peek().is_some_and(|token| {
                    matches!(
                        token.kind,
                        rustc_lexer::TokenKind::RawLifetime
                            | rustc_lexer::TokenKind::GuardedStrPrefix
                            | rustc_lexer::TokenKind::Lifetime { .. }
                            | rustc_lexer::TokenKind::Semi
                            | rustc_lexer::TokenKind::Comma
                            | rustc_lexer::TokenKind::Dot
                            | rustc_lexer::TokenKind::OpenParen
                            | rustc_lexer::TokenKind::CloseParen
                            | rustc_lexer::TokenKind::OpenBrace
                            | rustc_lexer::TokenKind::CloseBrace
                            | rustc_lexer::TokenKind::OpenBracket
                            | rustc_lexer::TokenKind::CloseBracket
                            | rustc_lexer::TokenKind::At
                            | rustc_lexer::TokenKind::Pound
                            | rustc_lexer::TokenKind::Tilde
                            | rustc_lexer::TokenKind::Question
                            | rustc_lexer::TokenKind::Colon
                            | rustc_lexer::TokenKind::Dollar
                            | rustc_lexer::TokenKind::Eq
                            | rustc_lexer::TokenKind::Bang
                            | rustc_lexer::TokenKind::Lt
                            | rustc_lexer::TokenKind::Gt
                            | rustc_lexer::TokenKind::Minus
                            | rustc_lexer::TokenKind::And
                            | rustc_lexer::TokenKind::Or
                            | rustc_lexer::TokenKind::Plus
                            | rustc_lexer::TokenKind::Star
                            | rustc_lexer::TokenKind::Slash
                            | rustc_lexer::TokenKind::Percent
                            | rustc_lexer::TokenKind::Caret
                    )
                })
            };

            let Some(&(open_delim, ..)) = groups.last() else {
                return refuse("Unbalanced delimiters");
            };
            match token.kind {
                rustc_lexer::TokenKind::OpenParen => {
                    groups.push((Delimiter::Parenthesis, range, pending.len()))
                }
                rustc_lexer::TokenKind::CloseParen if open_delim != Delimiter::Parenthesis => {
                    return refuse(if open_delim == Delimiter::None {
                        "Unexpected ')'"
                    } else {
                        "Expected ')'"
                    });
                }
                rustc_lexer::TokenKind::OpenBrace => {
                    groups.push((Delimiter::Brace, range, pending.len()))
                }
                rustc_lexer::TokenKind::CloseBrace if open_delim != Delimiter::Brace => {
                    return refuse(if open_delim == Delimiter::None {
                        "Unexpected '}'"
                    } else {
                        "Expected '}'"
                    });
                }
                rustc_lexer::TokenKind::OpenBracket => {
                    groups.push((Delimiter::Bracket, range, pending.len()))
                }
                rustc_lexer::TokenKind::CloseBracket if open_delim != Delimiter::Bracket => {
                    return refuse(if open_delim == Delimiter::None {
                        "Unexpected ']'"
                    } else {
                        "Expected ']'"
                    });
                }
                rustc_lexer::TokenKind::CloseParen
                | rustc_lexer::TokenKind::CloseBrace
                | rustc_lexer::TokenKind::CloseBracket => {
                    let (delimiter, open_range, start) =
                        groups.pop().expect("the group a close matched is open");
                    let trees = owned(&mut pending, start);
                    pending.push(TokenTree::Group(Group {
                        delimiter,
                        stream: if trees.is_empty() { None } else { Some(TokenStream::new(trees)) },
                        span: DelimSpan {
                            entire: span.derive_ranged(open_range.start..range.end),
                            open: span.derive_ranged(open_range),
                            close: span.derive_ranged(range),
                        },
                    }));
                }
                rustc_lexer::TokenKind::LineComment { doc_style: None }
                | rustc_lexer::TokenKind::BlockComment { doc_style: None, terminated: _ } => {
                    continue;
                }
                rustc_lexer::TokenKind::LineComment { doc_style: Some(doc_style) } => {
                    let text = &s[range.start + 3..range.end];
                    push_doc(&mut pending, doc_style, text, span, range, frame);
                }
                rustc_lexer::TokenKind::BlockComment { doc_style: Some(doc_style), terminated } => {
                    let text =
                        &s[range.start + 3..if terminated { range.end - 2 } else { range.end }];
                    push_doc(&mut pending, doc_style, text, span, range, frame);
                }
                rustc_lexer::TokenKind::Whitespace => continue,
                rustc_lexer::TokenKind::Frontmatter { .. } => unreachable!(),
                rustc_lexer::TokenKind::Unknown => {
                    // alloc: error path, naming the token.
                    return Err(format!("Unknown token: `{}`", &s[range]));
                }
                rustc_lexer::TokenKind::UnknownPrefix => {
                    // alloc: error path, naming the prefix.
                    return Err(format!("Unknown prefix: `{}`", &s[range]));
                }
                rustc_lexer::TokenKind::UnknownPrefixLifetime => {
                    // alloc: error path, naming the prefix.
                    return Err(format!("Unknown lifetime prefix: `{}`", &s[range]));
                }
                // FIXME: Error on edition >= 2024 ... I dont think the proc-macro server can fetch editions currently
                // and whose edition is this?
                rustc_lexer::TokenKind::GuardedStrPrefix => {
                    pending.push(TokenTree::Punct(Punct {
                        ch: s.as_bytes()[range.start],
                        joint: true,
                        span: span.derive_ranged(range.start..range.start + 1),
                    }));
                    pending.push(TokenTree::Punct(Punct {
                        ch: s.as_bytes()[range.start + 1],
                        joint: is_joint(),
                        span: span.derive_ranged(range.start + 1..range.end),
                    }))
                }
                rustc_lexer::TokenKind::Ident => pending.push(TokenTree::Ident(Ident {
                    sym: Symbol::intern(&s[range.start..range.end]),
                    kind: IdentKind::Normal,
                    span: span.derive_ranged(range),
                })),
                rustc_lexer::TokenKind::InvalidIdent => {
                    // alloc: error path, naming the identifier.
                    return Err(format!("Invalid identifier: `{}`", &s[range]));
                }
                rustc_lexer::TokenKind::RawIdent => {
                    let range = range.start + 2..range.end;
                    pending.push(TokenTree::Ident(Ident {
                        sym: Symbol::intern(&s[range.start..range.end]),
                        kind: IdentKind::Raw,
                        span: span.derive_ranged(range),
                    }))
                }
                rustc_lexer::TokenKind::ForcedKeywordIdent => {
                    let range = range.start + 2..range.end;
                    pending.push(TokenTree::Ident(Ident {
                        sym: Symbol::intern(&s[range.start..range.end]),
                        kind: IdentKind::ForcedKeyword,
                        span: span.derive_ranged(range),
                    }))
                }
                rustc_lexer::TokenKind::Literal { kind, suffix_start } => {
                    pending.push(TokenTree::Literal(literal_from_lexer(
                        &s[range.start..range.end],
                        span.derive_ranged(range),
                        kind,
                        suffix_start,
                    )))
                }
                rustc_lexer::TokenKind::RawLifetime => {
                    let range = range.start + 1 + 2..range.end;
                    pending.push(TokenTree::Punct(Punct {
                        ch: b'\'',
                        joint: true,
                        span: span.derive_ranged(range.start..range.start + 1),
                    }));
                    pending.push(TokenTree::Ident(Ident {
                        sym: Symbol::intern(&s[range.start..range.end]),
                        kind: IdentKind::Raw,
                        span: span.derive_ranged(range),
                    }))
                }
                rustc_lexer::TokenKind::Lifetime { starts_with_number } => {
                    if starts_with_number {
                        return refuse("Lifetime cannot start with a number");
                    }
                    let range = range.start + 1..range.end;
                    pending.push(TokenTree::Punct(Punct {
                        ch: b'\'',
                        joint: true,
                        span: span.derive_ranged(range.start..range.start + 1),
                    }));
                    pending.push(TokenTree::Ident(Ident {
                        sym: Symbol::intern(&s[range.start..range.end]),
                        kind: IdentKind::Normal,
                        span: span.derive_ranged(range),
                    }))
                }
                rustc_lexer::TokenKind::Semi => pending.push(TokenTree::Punct(Punct {
                    ch: b';',
                    joint: is_joint(),
                    span: span.derive_ranged(range),
                })),
                rustc_lexer::TokenKind::Comma => pending.push(TokenTree::Punct(Punct {
                    ch: b',',
                    joint: is_joint(),
                    span: span.derive_ranged(range),
                })),
                rustc_lexer::TokenKind::Dot => pending.push(TokenTree::Punct(Punct {
                    ch: b'.',
                    joint: is_joint(),
                    span: span.derive_ranged(range),
                })),
                rustc_lexer::TokenKind::At => pending.push(TokenTree::Punct(Punct {
                    ch: b'@',
                    joint: is_joint(),
                    span: span.derive_ranged(range),
                })),
                rustc_lexer::TokenKind::Pound => pending.push(TokenTree::Punct(Punct {
                    ch: b'#',
                    joint: is_joint(),
                    span: span.derive_ranged(range),
                })),
                rustc_lexer::TokenKind::Tilde => pending.push(TokenTree::Punct(Punct {
                    ch: b'~',
                    joint: is_joint(),
                    span: span.derive_ranged(range),
                })),
                rustc_lexer::TokenKind::Question => pending.push(TokenTree::Punct(Punct {
                    ch: b'?',
                    joint: is_joint(),
                    span: span.derive_ranged(range),
                })),
                rustc_lexer::TokenKind::Colon => pending.push(TokenTree::Punct(Punct {
                    ch: b':',
                    joint: is_joint(),
                    span: span.derive_ranged(range),
                })),
                rustc_lexer::TokenKind::Dollar => pending.push(TokenTree::Punct(Punct {
                    ch: b'$',
                    joint: is_joint(),
                    span: span.derive_ranged(range),
                })),
                rustc_lexer::TokenKind::Eq => pending.push(TokenTree::Punct(Punct {
                    ch: b'=',
                    joint: is_joint(),
                    span: span.derive_ranged(range),
                })),
                rustc_lexer::TokenKind::Bang => pending.push(TokenTree::Punct(Punct {
                    ch: b'!',
                    joint: is_joint(),
                    span: span.derive_ranged(range),
                })),
                rustc_lexer::TokenKind::Lt => pending.push(TokenTree::Punct(Punct {
                    ch: b'<',
                    joint: is_joint(),
                    span: span.derive_ranged(range),
                })),
                rustc_lexer::TokenKind::Gt => pending.push(TokenTree::Punct(Punct {
                    ch: b'>',
                    joint: is_joint(),
                    span: span.derive_ranged(range),
                })),
                rustc_lexer::TokenKind::Minus => pending.push(TokenTree::Punct(Punct {
                    ch: b'-',
                    joint: is_joint(),
                    span: span.derive_ranged(range),
                })),
                rustc_lexer::TokenKind::And => pending.push(TokenTree::Punct(Punct {
                    ch: b'&',
                    joint: is_joint(),
                    span: span.derive_ranged(range),
                })),
                rustc_lexer::TokenKind::Or => pending.push(TokenTree::Punct(Punct {
                    ch: b'|',
                    joint: is_joint(),
                    span: span.derive_ranged(range),
                })),
                rustc_lexer::TokenKind::Plus => pending.push(TokenTree::Punct(Punct {
                    ch: b'+',
                    joint: is_joint(),
                    span: span.derive_ranged(range),
                })),
                rustc_lexer::TokenKind::Star => pending.push(TokenTree::Punct(Punct {
                    ch: b'*',
                    joint: is_joint(),
                    span: span.derive_ranged(range),
                })),
                rustc_lexer::TokenKind::Slash => pending.push(TokenTree::Punct(Punct {
                    ch: b'/',
                    joint: is_joint(),
                    span: span.derive_ranged(range),
                })),
                rustc_lexer::TokenKind::Caret => pending.push(TokenTree::Punct(Punct {
                    ch: b'^',
                    joint: is_joint(),
                    span: span.derive_ranged(range),
                })),
                rustc_lexer::TokenKind::Percent => pending.push(TokenTree::Punct(Punct {
                    ch: b'%',
                    joint: is_joint(),
                    span: span.derive_ranged(range),
                })),
                rustc_lexer::TokenKind::Eof => break,
            }
        }
        match groups[..] {
            [(Delimiter::None, _, 0)] => Ok(TokenStream::new(owned(&mut pending, 0))),
            _ => refuse("Mismatched token groups"),
        }
    }
}

/// `from_str`'s refusal of its input, saying why.
fn refuse<T>(why: &str) -> Result<T, String> {
    // alloc: error path, the message the macro is given.
    Err(why.to_owned())
}

/// The items of `pending` from `start` on, moved out into a `Vec` of exactly their number: a
/// group's trees, which the macro owns, out of the frame they waited in.
pub(crate) fn owned<T>(pending: &mut Vec<T, &Frame>, start: usize) -> Vec<T> {
    let mut items = Vec::with_capacity(pending.len() - start);
    items.extend(pending.drain(start..));
    items
}

/// A doc comment's `#[doc = "…"]` (`#![…]` when inner), as rustc's lexer makes of it.
fn push_doc<S: SpanLike + Copy>(
    pending: &mut Vec<TokenTree<S>, &Frame>,
    style: DocStyle,
    text: &str,
    span: S,
    range: Range<usize>,
    frame: &Frame,
) {
    pending.push(TokenTree::Punct(Punct { ch: b'#', joint: false, span }));
    if style == DocStyle::Inner {
        pending.push(TokenTree::Punct(Punct { ch: b'!', joint: false, span }));
    }
    let mut escaped: Vec<u8, &Frame> = Vec::with_capacity_in(text.len(), frame);
    for c in text.escape_debug() {
        escaped.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
    }
    let escaped = std::str::from_utf8(&escaped).expect("chars encode as UTF-8");
    // alloc: the attribute's stream, which the macro owns.
    let mut attribute = Vec::with_capacity(3);
    let doc = Ident { sym: Symbol::intern("doc"), kind: IdentKind::Normal, span };
    attribute.push(TokenTree::Ident(doc));
    attribute.push(TokenTree::Punct(Punct { ch: b'=', joint: false, span }));
    attribute.push(TokenTree::Literal(Literal {
        kind: LitKind::Str,
        symbol: Symbol::intern(escaped),
        suffix: None,
        span: span.derive_ranged(range),
    }));
    pending.push(TokenTree::Group(Group {
        delimiter: Delimiter::Bracket,
        stream: Some(TokenStream::new(attribute)),
        span: DelimSpan { open: span, close: span, entire: span },
    }));
}

impl<S> fmt::Display for TokenStream<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut emit_whitespace = false;
        for tt in self.0.iter() {
            display_token_tree(tt, &mut emit_whitespace, f)?;
        }
        Ok(())
    }
}

fn display_token_tree<S>(
    tt: &TokenTree<S>,
    emit_whitespace: &mut bool,
    f: &mut std::fmt::Formatter<'_>,
) -> std::fmt::Result {
    if mem::take(emit_whitespace) {
        write!(f, " ")?;
    }
    match tt {
        TokenTree::Group(Group { delimiter, stream, span: _ }) => {
            write!(
                f,
                "{}",
                match delimiter {
                    rustc_proc_macro::Delimiter::Parenthesis => "(",
                    rustc_proc_macro::Delimiter::Brace => "{",
                    rustc_proc_macro::Delimiter::Bracket => "[",
                    rustc_proc_macro::Delimiter::None => "",
                }
            )?;
            if let Some(stream) = stream {
                write!(f, "{stream}")?;
            }
            write!(
                f,
                "{}",
                match delimiter {
                    rustc_proc_macro::Delimiter::Parenthesis => ")",
                    rustc_proc_macro::Delimiter::Brace => "}",
                    rustc_proc_macro::Delimiter::Bracket => "]",
                    rustc_proc_macro::Delimiter::None => "",
                }
            )?;
        }
        TokenTree::Punct(Punct { ch, joint, span: _ }) => {
            *emit_whitespace = !*joint;
            write!(f, "{}", *ch as char)?;
        }
        TokenTree::Ident(Ident { sym, kind, span: _ }) => {
            if let Some(prefix) = kind.prefix() {
                write!(f, "{prefix}")?;
            }
            write!(f, "{sym}")?;
            *emit_whitespace = true;
        }
        TokenTree::Literal(lit) => {
            display_fmt_literal(lit, f)?;
            let joint = match lit.kind {
                LitKind::Str
                | LitKind::StrRaw(_)
                | LitKind::ByteStr
                | LitKind::ByteStrRaw(_)
                | LitKind::CStr
                | LitKind::CStrRaw(_) => true,
                _ => false,
            };
            *emit_whitespace = !joint;
        }
    }
    Ok(())
}

pub fn literal_to_string<S>(literal: &Literal<S>) -> String {
    // alloc: the text a caller asks for, which it owns.
    let mut buf = String::new();
    display_fmt_literal(literal, &mut buf).unwrap();
    buf
}

fn display_fmt_literal<S>(literal: &Literal<S>, f: &mut impl std::fmt::Write) -> fmt::Result {
    match literal.kind {
        LitKind::Byte => write!(f, "b'{}'", literal.symbol),
        LitKind::Char => write!(f, "'{}'", literal.symbol),
        LitKind::Integer | LitKind::Float | LitKind::ErrWithGuar => {
            write!(f, "{}", literal.symbol)
        }
        LitKind::Str => write!(f, "\"{}\"", literal.symbol),
        LitKind::ByteStr => write!(f, "b\"{}\"", literal.symbol),
        LitKind::CStr => write!(f, "c\"{}\"", literal.symbol),
        LitKind::StrRaw(num_of_hashes) => {
            let num_of_hashes = num_of_hashes as usize;
            write!(
                f,
                r#"r{0:#<num_of_hashes$}"{text}"{0:#<num_of_hashes$}"#,
                "",
                text = literal.symbol
            )
        }
        LitKind::ByteStrRaw(num_of_hashes) => {
            let num_of_hashes = num_of_hashes as usize;
            write!(
                f,
                r#"br{0:#<num_of_hashes$}"{text}"{0:#<num_of_hashes$}"#,
                "",
                text = literal.symbol
            )
        }
        LitKind::CStrRaw(num_of_hashes) => {
            let num_of_hashes = num_of_hashes as usize;
            write!(
                f,
                r#"cr{0:#<num_of_hashes$}"{text}"{0:#<num_of_hashes$}"#,
                "",
                text = literal.symbol
            )
        }
    }?;
    if let Some(suffix) = &literal.suffix {
        write!(f, "{suffix}")?;
    }
    Ok(())
}

impl<S: fmt::Debug> fmt::Debug for TokenStream<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        debug_token_stream(self, 0, f)
    }
}

fn debug_token_stream<S: fmt::Debug>(
    ts: &TokenStream<S>,
    depth: usize,
    f: &mut std::fmt::Formatter<'_>,
) -> std::fmt::Result {
    for tt in ts.0.iter() {
        debug_token_tree(tt, depth, f)?;
    }
    Ok(())
}

fn debug_token_tree<S: fmt::Debug>(
    tt: &TokenTree<S>,
    depth: usize,
    f: &mut std::fmt::Formatter<'_>,
) -> std::fmt::Result {
    write!(f, "{:indent$}", "", indent = depth * 2)?;
    match tt {
        TokenTree::Group(Group { delimiter, stream, span }) => {
            writeln!(
                f,
                "GROUP {}{} {:#?} {:#?} {:#?}",
                match delimiter {
                    rustc_proc_macro::Delimiter::Parenthesis => "(",
                    rustc_proc_macro::Delimiter::Brace => "{",
                    rustc_proc_macro::Delimiter::Bracket => "[",
                    rustc_proc_macro::Delimiter::None => "$",
                },
                match delimiter {
                    rustc_proc_macro::Delimiter::Parenthesis => ")",
                    rustc_proc_macro::Delimiter::Brace => "}",
                    rustc_proc_macro::Delimiter::Bracket => "]",
                    rustc_proc_macro::Delimiter::None => "$",
                },
                span.open,
                span.close,
                span.entire,
            )?;
            if let Some(stream) = stream {
                debug_token_stream(stream, depth + 1, f)?;
            }
            return Ok(());
        }
        TokenTree::Punct(Punct { ch, joint, span }) => write!(
            f,
            "PUNCT {span:#?} {} {}",
            *ch as char,
            if *joint { "[joint]" } else { "[alone]" }
        )?,
        TokenTree::Ident(Ident { sym, kind, span }) => {
            write!(f, "IDENT {span:#?} ")?;
            if let Some(prefix) = kind.prefix() {
                write!(f, "{prefix}")?;
            }
            write!(f, "{sym}")?;
        }
        TokenTree::Literal(Literal { kind, symbol, suffix, span }) => write!(
            f,
            "LITER {span:#?} {kind:?} {symbol}{}",
            suffix.as_ref().map_or("", Symbol::as_str)
        )?,
    }
    writeln!(f)
}

impl<S: Copy> TokenStream<S> {
    /// `tree` alone: the bridge's `ts_from_token_tree`.
    pub(crate) fn of_tree(tree: TokenTree<S>) -> Self {
        let mut trees = Vec::with_capacity(1);
        trees.push(tree);
        TokenStream::new(trees)
    }

    /// `base`'s trees and then `trees`: the bridge's `ts_concat_trees`. `base`'s are copied only
    /// when someone else holds them too.
    pub(crate) fn concat_trees(base: Option<Self>, trees: Vec<TokenTree<S>>) -> Self {
        match base {
            Some(mut base) => {
                Arc::make_mut(&mut base.0).extend(trees);
                base
            }
            None => TokenStream::new(trees),
        }
    }

    /// `base`'s trees and then each of `streams`': the bridge's `ts_concat_streams`. Without a
    /// base the first stream is it, and a stream no one else holds gives its trees up whole.
    pub(crate) fn concat_streams(base: Option<Self>, streams: Vec<Self>) -> Self {
        let mut streams = streams.into_iter();
        let Some(mut stream) = base.or_else(|| streams.next()) else {
            // alloc: the empty stream the macro asked for, which it owns.
            return TokenStream::default();
        };
        for more in streams {
            stream.push_stream(more);
        }
        stream
    }

    /// The stream's trees: the bridge's `ts_into_trees`, copied only when the stream is shared.
    pub(crate) fn into_trees(self) -> Vec<TokenTree<S>> {
        Arc::unwrap_or_clone(self.0)
    }

    /// Appends `stream`'s trees, moved when no one else holds them, copied when someone does.
    fn push_stream(&mut self, stream: TokenStream<S>) {
        let trees = Arc::make_mut(&mut self.0);
        match Arc::try_unwrap(stream.0) {
            Ok(moved) => trees.extend(moved),
            Err(shared) => trees.extend(shared.iter().cloned()),
        }
    }
}

impl<S> FromIterator<TokenTree<S>> for TokenStream<S> {
    fn from_iter<I: IntoIterator<Item = TokenTree<S>>>(iter: I) -> Self {
        // alloc: the stream a caller collects, which it owns.
        TokenStream::new(iter.into_iter().collect::<Vec<TokenTree<S>>>())
    }
}

#[derive(Clone)]
pub struct TokenStreamIter<'t, S> {
    stream: &'t TokenStream<S>,
    index: usize,
}

impl<'t, S> TokenStreamIter<'t, S> {
    fn new(stream: &'t TokenStream<S>) -> Self {
        TokenStreamIter { stream, index: 0 }
    }
}

impl<'t, S> Iterator for TokenStreamIter<'t, S> {
    type Item = &'t TokenTree<S>;

    fn next(&mut self) -> Option<&'t TokenTree<S>> {
        self.stream.0.get(self.index).map(|tree| {
            self.index += 1;
            tree
        })
    }
}

pub(super) fn literal_from_lexer<Span>(
    s: &str,
    span: Span,
    kind: rustc_lexer::LiteralKind,
    suffix_start: u32,
) -> Literal<Span> {
    let (kind, start_offset, end_offset) = match kind {
        LiteralKind::Int { .. } => (LitKind::Integer, 0, 0),
        LiteralKind::Float { .. } => (LitKind::Float, 0, 0),
        LiteralKind::Char { terminated } => (LitKind::Char, 1, terminated as usize),
        LiteralKind::Byte { terminated } => (LitKind::Byte, 2, terminated as usize),
        LiteralKind::Str { terminated } => (LitKind::Str, 1, terminated as usize),
        LiteralKind::ByteStr { terminated } => (LitKind::ByteStr, 2, terminated as usize),
        LiteralKind::CStr { terminated } => (LitKind::CStr, 2, terminated as usize),
        LiteralKind::RawStr { n_hashes } => (
            LitKind::StrRaw(n_hashes.unwrap_or_default()),
            2 + n_hashes.unwrap_or_default() as usize,
            1 + n_hashes.unwrap_or_default() as usize,
        ),
        LiteralKind::RawByteStr { n_hashes } => (
            LitKind::ByteStrRaw(n_hashes.unwrap_or_default()),
            3 + n_hashes.unwrap_or_default() as usize,
            1 + n_hashes.unwrap_or_default() as usize,
        ),
        LiteralKind::RawCStr { n_hashes } => (
            LitKind::CStrRaw(n_hashes.unwrap_or_default()),
            3 + n_hashes.unwrap_or_default() as usize,
            1 + n_hashes.unwrap_or_default() as usize,
        ),
    };

    let (lit, suffix) = s.split_at(suffix_start as usize);
    let lit = &lit[start_offset..lit.len() - end_offset];
    let suffix = match suffix {
        "" | "_" => None,
        suffix => Some(Symbol::intern(suffix)),
    };

    Literal { kind, symbol: Symbol::intern(lit), suffix, span }
}

impl SpanLike for crate::SpanId {
    fn derive_ranged(&self, _: std::ops::Range<usize>) -> Self {
        *self
    }
}

impl SpanLike for () {
    fn derive_ranged(&self, _: std::ops::Range<usize>) -> Self {
        *self
    }
}

impl SpanLike for crate::Span {
    fn derive_ranged(&self, range: std::ops::Range<usize>) -> Self {
        crate::Span {
            range: span::TextRange::new(
                span::TextSize::new(range.start as u32),
                span::TextSize::new(range.end as u32),
            ),
            anchor: self.anchor,
            ctx: self.ctx,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ts_to_string() {
        let token_stream =
            TokenStream::from_str("{} () [] <> ;/., \"gfhdgfuiofghd\" 0f32 r#\"dff\"# 'r#lt", ())
                .unwrap();
        assert_eq!(token_stream.to_string(), "{}()[]<> ;/., \"gfhdgfuiofghd\"0f32 r#\"dff\"#'r#lt");
    }

    #[test]
    fn doc_comment_from_str() {
        let token_stream = TokenStream::from_str("/// foo", ()).unwrap();
        assert_eq!(token_stream.to_string(), r#"# [doc = " foo"]"#);
    }

    #[test]
    fn a_stream_parsed_allocates_only_its_streams() {
        let source = "a (b, 1) c";
        // The first interns the symbols and reserves the thread's frame.
        let first = TokenStream::from_str(source, ()).unwrap();
        // Each stream's trees and its shared handle: the group's and the top's.
        let parsed = blaze_alloc_count::assert_at_most(4, "a stream parsed", || {
            TokenStream::from_str(source, ())
        });
        assert_eq!(parsed.unwrap().to_string(), first.to_string());
    }

    #[test]
    fn streams_no_one_else_holds_are_joined_and_given_up_in_place() {
        let streams =
            vec![TokenStream::from_str("a b", ()).unwrap(), TokenStream::from_str("c", ()).unwrap()];
        // The first stream's trees, grown to hold the second's.
        let joined = blaze_alloc_count::assert_at_most(1, "two streams joined", || {
            TokenStream::concat_streams(None, streams)
        });
        assert_eq!(joined.to_string(), TokenStream::from_str("a b c", ()).unwrap().to_string());
        let trees = blaze_alloc_count::assert_at_most(0, "a stream's trees", || joined.into_trees());
        assert_eq!(trees.len(), 3);
    }
}

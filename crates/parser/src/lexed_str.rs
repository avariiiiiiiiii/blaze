//! Lexing `&str` into a sequence of Rust tokens.
//!
//! Note that strictly speaking the parser in this crate is not required to work
//! on tokens which originated from text. Macros, eg, can synthesize tokens out
//! of thin air. So, ideally, lexer should be an orthogonal crate. It is however
//! convenient to include a text-based lexer here!
//!
//! Note that these tokens, unlike the tokens we feed into the parser, do
//! include info about comments and whitespace.

#[allow(unsafe_code)]
mod scan;

use std::{borrow::Cow, ops};

use rustc_literal_escaper::{
    EscapeError, Mode, unescape_byte, unescape_byte_str, unescape_c_str, unescape_char,
    unescape_str,
};

use crate::{
    Edition,
    SyntaxKind::{self, *},
    T,
};

/// The tokens of a text, the last an `EOF` of no length.
///
/// A token is one `u32`: its kind in the low [`KIND_BITS`] bits and its length in bytes in the
/// rest. A length of [`LONG`] (4 MiB) or more, a huge string literal or comment, is written as
/// [`LONG`] and kept in `long`, a side table sorted by token. Starts are arithmetic, as in the
/// flat syntax tree: `checkpoints` holds the start of every [`CHECKPOINT`]th token, and a start
/// is its checkpoint plus the lengths of the tokens between. So a token costs 4 bytes and a
/// sixteenth of a checkpoint's 4, 4.25 bytes, against the 6 of a kind (`u16`) and a start
/// (`u32`) each; finding a start reads at most 15 lengths, and walking the tokens in order
/// (`to_input`, `intersperse_trivia`) keeps a running offset instead.
pub struct LexedStr<'a> {
    text: &'a str,
    tokens: Vec<Token>,
    checkpoints: Vec<u32>,
    long: Vec<(u32, u32)>,
    error: Vec<LexError>,
}

/// A packed token: kind and length.
type Token = u32;

const _: () = assert!(size_of::<Token>() == 4, "a token is 4 bytes (docs/v2/plan/design/data.md)");

const KIND_BITS: u32 = 10;
const KIND_MASK: u32 = (1 << KIND_BITS) - 1;
const LONG: u32 = (1 << (32 - KIND_BITS)) - 1;
const CHECKPOINT: usize = 16;
const _: () = assert!((SyntaxKind::__LAST as u32) <= KIND_MASK, "a kind fits in KIND_BITS");

/// The messages are mostly fixed text: only an unterminated literal's, an unknown prefix's and
/// a bad raw name's are formatted, so a valid token allocates nothing.
struct LexError {
    msg: Cow<'static, str>,
    token: u32,
}

impl<'a> LexedStr<'a> {
    pub fn new(edition: Edition, text: &'a str) -> LexedStr<'a> {
        Self::lex::<true>(edition, text)
    }

    /// With `FAST`, the tokens [`scan::token`] decides from the byte classes are lexed there and
    /// the rest by `rustc_lexer`; without, all by `rustc_lexer`. Both give the same tokens
    /// (`fast_and_rustc_lexer_lex_alike`).
    fn lex<const FAST: bool>(edition: Edition, text: &'a str) -> LexedStr<'a> {
        let _p = tracing::info_span!("LexedStr::new").entered();
        assert!(text.len() <= u32::MAX as usize, "a text of 4 GiB or more");
        let mut conv = Converter::new(edition, text);
        if let Ok(script) = crate::frontmatter::ScriptSource::parse(text) {
            if let Some(shebang) = script.shebang_span() {
                conv.push(SHEBANG, shebang.end - shebang.start);
            }
            if script.frontmatter().is_some() {
                conv.push(FRONTMATTER, script.content_span().start - conv.offset);
            }
        } else if let Some(shebang_len) = rustc_lexer::strip_shebang(text) {
            // Leave error reporting to `rustc_lexer`
            conv.push(SHEBANG, shebang_len);
        }

        let bytes = text.as_bytes();
        let classes = FAST.then(|| scan::Classes::new(bytes));
        // Lex from scratch at every token because some of `rustc_lexer`'s tokens are split as
        // rustc splits them in older editions: `GuardedStrPrefix` before 2024, a C string and a
        // raw lifetime before 2021.
        while conv.offset < text.len() {
            let fast = match &classes {
                Some(classes) => scan::token(bytes, conv.offset, classes),
                None => None,
            };
            let (kind, len) = fast.unwrap_or_else(|| {
                let token =
                    rustc_lexer::tokenize(&text[conv.offset..], rustc_lexer::FrontmatterAllowed::No)
                        .next()
                        .expect("a token where text is left");
                (token.kind, token.len as usize)
            });
            let token_text = &text[conv.offset..][..len];

            conv.extend_token(&kind, token_text);
        }

        conv.finalize_with_eof()
    }

    pub fn single_token(edition: Edition, text: &'a str) -> Option<(SyntaxKind, Option<String>)> {
        if text.is_empty() {
            return None;
        }

        let token = rustc_lexer::tokenize(text, rustc_lexer::FrontmatterAllowed::No).next()?;
        if token.len as usize != text.len() {
            return None;
        }

        let mut conv = Converter::new(edition, text);
        conv.extend_token(&token.kind, text);
        match conv.res.tokens.len() {
            // An edition's split (`c"x"`, `'r#a` before 2021, `#"x"` before 2024) leaves text.
            1 if conv.offset == text.len() => Some((conv.res.raw_kind(0), conv.res.error.pop().map(|it| it.msg.into_owned()))),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &str {
        self.text
    }

    pub fn len(&self) -> usize {
        self.tokens.len() - 1
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn kind(&self, i: usize) -> SyntaxKind {
        assert!(i < self.len());
        self.raw_kind(i)
    }

    pub fn text(&self, i: usize) -> &str {
        self.range_text(i..i + 1)
    }

    pub fn range_text(&self, r: ops::Range<usize>) -> &str {
        assert!(r.start < r.end && r.end <= self.len());
        let lo = self.text_start(r.start);
        let hi = lo + (r.start..r.end).map(|i| self.raw_len(i)).sum::<usize>();
        &self.text[lo..hi]
    }

    // Naming is hard.
    pub fn text_range(&self, i: usize) -> ops::Range<usize> {
        assert!(i < self.len());
        let lo = self.text_start(i);
        lo..lo + self.raw_len(i)
    }
    pub fn text_start(&self, i: usize) -> usize {
        assert!(i <= self.len());
        let checkpoint = i / CHECKPOINT;
        let from = checkpoint * CHECKPOINT;
        self.checkpoints[checkpoint] as usize + (from..i).map(|i| self.raw_len(i)).sum::<usize>()
    }
    pub fn text_len(&self, i: usize) -> usize {
        assert!(i < self.len());
        self.raw_len(i)
    }

    /// The length of token `i`, for walks that keep their own offset.
    #[inline]
    pub(crate) fn token_len(&self, i: usize) -> usize {
        debug_assert!(i < self.len());
        self.raw_len(i)
    }

    #[inline]
    fn raw_kind(&self, i: usize) -> SyntaxKind {
        SyntaxKind::from((self.tokens[i] & KIND_MASK) as u16)
    }

    #[inline]
    fn raw_len(&self, i: usize) -> usize {
        let len = self.tokens[i] >> KIND_BITS;
        if len < LONG {
            return len as usize;
        }
        let at = self
            .long
            .binary_search_by_key(&(i as u32), |&(token, _)| token)
            .expect("a long token's length is in the side table");
        self.long[at].1 as usize
    }

    pub fn error(&self, i: usize) -> Option<&str> {
        assert!(i < self.len());
        let err = self.error.binary_search_by_key(&(i as u32), |i| i.token).ok()?;
        Some(&*self.error[err].msg)
    }

    pub fn errors(&self) -> impl Iterator<Item = (usize, &str)> + '_ {
        self.error.iter().map(|it| (it.token as usize, &*it.msg))
    }

    fn push(&mut self, kind: SyntaxKind, start: usize, len: usize) {
        let at = self.tokens.len();
        if at.is_multiple_of(CHECKPOINT) {
            self.checkpoints.push(start as u32);
        }
        let kind = kind as u16 as u32;
        match u32::try_from(len) {
            Ok(len) if len < LONG => self.tokens.push(kind | (len << KIND_BITS)),
            _ => {
                self.tokens.push(kind | (LONG << KIND_BITS));
                self.long.push((at as u32, len as u32));
            }
        }
    }
}

struct Converter<'a> {
    res: LexedStr<'a>,
    offset: usize,
    edition: Edition,
}

impl<'a> Converter<'a> {
    fn new(edition: Edition, text: &'a str) -> Self {
        Self {
            res: LexedStr {
                text,
                tokens: Vec::with_capacity(text.len() / 3),
                checkpoints: Vec::with_capacity(text.len() / 3 / CHECKPOINT + 1),
                long: Vec::new(),
                error: Vec::new(),
            },
            offset: 0,
            edition,
        }
    }

    fn finalize_with_eof(mut self) -> LexedStr<'a> {
        self.res.push(EOF, self.offset, 0);
        self.res
    }

    fn push(&mut self, kind: SyntaxKind, len: usize) {
        self.res.push(kind, self.offset, len);
        self.offset += len;
    }

    /// An error of the token pushed next.
    fn error(&mut self, msg: impl Into<Cow<'static, str>>) {
        let msg = msg.into();
        if !msg.is_empty() {
            self.res.error.push(LexError { msg, token: self.res.tokens.len() as u32 });
        }
    }

    fn comment_kind(doc_style: Option<rustc_lexer::DocStyle>) -> SyntaxKind {
        match doc_style {
            Some(rustc_lexer::DocStyle::Outer) => OUTER_DOC_COMMENT,
            Some(rustc_lexer::DocStyle::Inner) => INNER_DOC_COMMENT,
            None => COMMENT,
        }
    }

    fn extend_token(&mut self, kind: &rustc_lexer::TokenKind, mut token_text: &str) {
        // A note on an intended tradeoff:
        // We drop some useful information here (see patterns with double dots `..`)
        // Storing that info in `SyntaxKind` is not possible due to its layout requirements of
        // being `u16` that come from `syntax_tree::SyntaxKind`.
        let syntax_kind = {
            match kind {
                rustc_lexer::TokenKind::LineComment { doc_style } => Self::comment_kind(*doc_style),
                rustc_lexer::TokenKind::BlockComment { doc_style, terminated } => {
                    if !terminated {
                        self.error("Missing trailing `*/` symbols to terminate the block comment");
                    }
                    Self::comment_kind(*doc_style)
                }

                rustc_lexer::TokenKind::Frontmatter {
                    has_invalid_preceding_whitespace,
                    invalid_infostring,
                } => {
                    if *has_invalid_preceding_whitespace {
                        self.error("invalid preceding whitespace for frontmatter opening");
                    } else if *invalid_infostring {
                        self.error("invalid infostring for frontmatter");
                    }
                    FRONTMATTER
                }

                rustc_lexer::TokenKind::Whitespace => WHITESPACE,

                rustc_lexer::TokenKind::Ident if token_text == "_" => UNDERSCORE,
                rustc_lexer::TokenKind::Ident => {
                    SyntaxKind::from_keyword(token_text, self.edition).unwrap_or(IDENT)
                }
                rustc_lexer::TokenKind::InvalidIdent => {
                    self.error("Ident contains invalid characters");
                    IDENT
                }

                rustc_lexer::TokenKind::RawIdent => {
                    let name = &token_text[2..];
                    if !can_be_raw(name) {
                        self.error(format!("`{name}` cannot be a raw identifier"));
                    }
                    IDENT
                }
                #[cfg(feature = "in-rust-tree")]
                rustc_lexer::TokenKind::ForcedKeywordIdent => ERROR,

                rustc_lexer::TokenKind::GuardedStrPrefix if self.edition.at_least_2024() => {
                    // FIXME: rustc does something better for recovery.
                    self.error("Invalid string literal (reserved syntax)");
                    ERROR
                }
                rustc_lexer::TokenKind::GuardedStrPrefix => {
                    // The token is `#"` or `##`, split it into two.
                    token_text = &token_text[1..];
                    POUND
                }

                // Before 2021 `c"…"` is the identifier `c` and then the string, `cr#"…"#` the
                // identifier `cr` and then `#"…"#`.
                rustc_lexer::TokenKind::Literal {
                    kind: rustc_lexer::LiteralKind::CStr { .. },
                    ..
                } if !self.edition.at_least_2021() => {
                    token_text = &token_text[..1];
                    IDENT
                }
                rustc_lexer::TokenKind::Literal {
                    kind: rustc_lexer::LiteralKind::RawCStr { .. },
                    ..
                } if !self.edition.at_least_2021() => {
                    token_text = &token_text[..2];
                    IDENT
                }
                rustc_lexer::TokenKind::Literal { kind, .. } => {
                    self.extend_literal(token_text.len(), kind);
                    return;
                }

                rustc_lexer::TokenKind::Lifetime { starts_with_number } => {
                    if *starts_with_number {
                        self.error("Lifetime name cannot start with a number");
                    }
                    LIFETIME_IDENT
                }
                rustc_lexer::TokenKind::UnknownPrefixLifetime => {
                    if self.edition.at_least_2021() {
                        self.error(format!("prefix `{token_text}` is unknown"));
                    }
                    LIFETIME_IDENT
                }
                // Before 2021 `'r#a` is `'r`, `#` and `a`.
                rustc_lexer::TokenKind::RawLifetime if !self.edition.at_least_2021() => {
                    token_text = &token_text[..2];
                    LIFETIME_IDENT
                }
                rustc_lexer::TokenKind::RawLifetime => {
                    let end = self.offset + token_text.len();
                    if self.res.text.as_bytes().get(end) == Some(&b'\'') {
                        self.error("character literal may only contain one codepoint");
                    }
                    let name = &token_text[3..];
                    if !can_be_raw(name) {
                        self.error(format!("`{name}` cannot be a raw lifetime"));
                    }
                    LIFETIME_IDENT
                }

                rustc_lexer::TokenKind::Semi => T![;],
                rustc_lexer::TokenKind::Comma => T![,],
                rustc_lexer::TokenKind::Dot => T![.],
                rustc_lexer::TokenKind::OpenParen => T!['('],
                rustc_lexer::TokenKind::CloseParen => T![')'],
                rustc_lexer::TokenKind::OpenBrace => T!['{'],
                rustc_lexer::TokenKind::CloseBrace => T!['}'],
                rustc_lexer::TokenKind::OpenBracket => T!['['],
                rustc_lexer::TokenKind::CloseBracket => T![']'],
                rustc_lexer::TokenKind::At => T![@],
                rustc_lexer::TokenKind::Pound => T![#],
                rustc_lexer::TokenKind::Tilde => T![~],
                rustc_lexer::TokenKind::Question => T![?],
                rustc_lexer::TokenKind::Colon => T![:],
                rustc_lexer::TokenKind::Dollar => T![$],
                rustc_lexer::TokenKind::Eq => T![=],
                rustc_lexer::TokenKind::Bang => T![!],
                rustc_lexer::TokenKind::Lt => T![<],
                rustc_lexer::TokenKind::Gt => T![>],
                rustc_lexer::TokenKind::Minus => T![-],
                rustc_lexer::TokenKind::And => T![&],
                rustc_lexer::TokenKind::Or => T![|],
                rustc_lexer::TokenKind::Plus => T![+],
                rustc_lexer::TokenKind::Star => T![*],
                rustc_lexer::TokenKind::Slash => T![/],
                rustc_lexer::TokenKind::Caret => T![^],
                rustc_lexer::TokenKind::Percent => T![%],
                rustc_lexer::TokenKind::Unknown => ERROR,
                rustc_lexer::TokenKind::UnknownPrefix if token_text == "builtin" => IDENT,
                // The prefix is the identifier it spells, keyword or not, in every edition.
                rustc_lexer::TokenKind::UnknownPrefix => {
                    if self.edition.at_least_2021() {
                        self.error(format!("prefix `{token_text}` is unknown"));
                    }
                    if token_text == "_" {
                        UNDERSCORE
                    } else {
                        SyntaxKind::from_keyword(token_text, self.edition).unwrap_or(IDENT)
                    }
                }
                rustc_lexer::TokenKind::Eof => EOF,
            }
        };

        self.push(syntax_kind, token_text.len());
    }

    fn extend_literal(&mut self, len: usize, kind: &rustc_lexer::LiteralKind) {
        const INVALID_RAW: &str = "Invalid raw string literal";
        let source: &'a str = self.res.text;
        let no_end_quote = |this: &mut Self, c: char, kind: &str| {
            this.error(format!("Missing trailing `{c}` symbol to terminate the {kind} literal"));
        };

        let syntax_kind = match *kind {
            rustc_lexer::LiteralKind::Int { empty_int, base: _ } => {
                if empty_int {
                    self.error("Missing digits after the integer base prefix");
                }
                INT_NUMBER
            }
            rustc_lexer::LiteralKind::Float { empty_exponent, base: _ } => {
                if empty_exponent {
                    self.error("Missing digits after the exponent symbol");
                }
                FLOAT_NUMBER
            }
            rustc_lexer::LiteralKind::Char { terminated } => {
                if !terminated {
                    no_end_quote(self, '\'', "character");
                } else {
                    let text = &source[self.offset + 1..][..len - 1];
                    let text = &text[..text.rfind('\'').unwrap()];
                    if let Err(e) = unescape_char(text) {
                        self.error(err_to_msg(e, Mode::Char));
                    }
                }
                CHAR
            }
            rustc_lexer::LiteralKind::Byte { terminated } => {
                if !terminated {
                    no_end_quote(self, '\'', "byte");
                } else {
                    let text = &source[self.offset + 2..][..len - 2];
                    let text = &text[..text.rfind('\'').unwrap()];
                    if let Err(e) = unescape_byte(text) {
                        self.error(err_to_msg(e, Mode::Byte));
                    }
                }
                BYTE
            }
            rustc_lexer::LiteralKind::Str { terminated } => {
                if !terminated {
                    no_end_quote(self, '"', "string");
                } else {
                    let text = &source[self.offset + 1..][..len - 1];
                    let text = &text[..text.rfind('"').unwrap()];
                    unescape_str(text, |_, res| {
                        if let Err(e) = res {
                            self.error(err_to_msg(e, Mode::Str));
                        }
                    });
                }
                STRING
            }
            rustc_lexer::LiteralKind::ByteStr { terminated } => {
                if !terminated {
                    no_end_quote(self, '"', "byte string");
                } else {
                    let text = &source[self.offset + 2..][..len - 2];
                    let text = &text[..text.rfind('"').unwrap()];
                    unescape_byte_str(text, |_, res| {
                        if let Err(e) = res {
                            self.error(err_to_msg(e, Mode::ByteStr));
                        }
                    });
                }
                BYTE_STRING
            }
            rustc_lexer::LiteralKind::CStr { terminated } => {
                if !terminated {
                    no_end_quote(self, '"', "C string")
                } else {
                    let text = &source[self.offset + 2..][..len - 2];
                    let text = &text[..text.rfind('"').unwrap()];
                    unescape_c_str(text, |_, res| {
                        if let Err(e) = res {
                            self.error(err_to_msg(e, Mode::CStr));
                        }
                    });
                }
                C_STRING
            }
            rustc_lexer::LiteralKind::RawStr { n_hashes } => {
                if n_hashes.is_none() {
                    self.error(INVALID_RAW);
                }
                STRING
            }
            rustc_lexer::LiteralKind::RawByteStr { n_hashes } => {
                if n_hashes.is_none() {
                    self.error(INVALID_RAW);
                }
                BYTE_STRING
            }
            rustc_lexer::LiteralKind::RawCStr { n_hashes } => {
                if n_hashes.is_none() {
                    self.error(INVALID_RAW);
                }
                C_STRING
            }
        };

        self.push(syntax_kind, len);
    }
}

/// rustc's `Symbol::can_be_raw`: not `_` and not a path segment keyword.
fn can_be_raw(name: &str) -> bool {
    !matches!(name, "" | "_" | "crate" | "self" | "Self" | "super")
}

fn err_to_msg(error: EscapeError, mode: Mode) -> &'static str {
    match error {
        EscapeError::ZeroChars => "empty character literal",
        EscapeError::MoreThanOneChar => "character literal may only contain one codepoint",
        EscapeError::LoneSlash => "",
        EscapeError::InvalidEscape if mode == Mode::Byte || mode == Mode::ByteStr => {
            "unknown byte escape"
        }
        EscapeError::InvalidEscape => "unknown character escape",
        EscapeError::BareCarriageReturn => "",
        EscapeError::BareCarriageReturnInRawString => "",
        EscapeError::EscapeOnlyChar if mode == Mode::Byte => "byte constant must be escaped",
        EscapeError::EscapeOnlyChar => "character constant must be escaped",
        EscapeError::TooShortHexEscape => "numeric character escape is too short",
        EscapeError::InvalidCharInHexEscape => "invalid character in numeric character escape",
        EscapeError::OutOfRangeHexEscape => "out of range hex escape",
        EscapeError::NoBraceInUnicodeEscape => "incorrect unicode escape sequence",
        EscapeError::InvalidCharInUnicodeEscape => "invalid character in unicode escape",
        EscapeError::EmptyUnicodeEscape => "empty unicode escape",
        EscapeError::UnclosedUnicodeEscape => "unterminated unicode escape",
        EscapeError::LeadingUnderscoreUnicodeEscape => "invalid start of unicode escape",
        EscapeError::OverlongUnicodeEscape => "overlong unicode escape",
        EscapeError::LoneSurrogateUnicodeEscape => "invalid unicode character escape",
        EscapeError::OutOfRangeUnicodeEscape => "invalid unicode character escape",
        EscapeError::UnicodeEscapeInByte => "unicode escape in byte string",
        EscapeError::NonAsciiCharInByte if mode == Mode::Byte => {
            "non-ASCII character in byte literal"
        }
        EscapeError::NonAsciiCharInByte if mode == Mode::ByteStr => {
            "non-ASCII character in byte string literal"
        }
        EscapeError::NonAsciiCharInByte => "non-ASCII character in raw byte string literal",
        EscapeError::NulInCStr => "null character in C string literal",
        EscapeError::UnskippedWhitespaceWarning => "",
        EscapeError::MultipleSkippedLinesWarning => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every start, range and text found by arithmetic is the one found by walking the tokens,
    /// across checkpoints and for tokens longer than a packed length holds.
    #[test]
    fn packed_tokens_answer_as_a_walk() {
        let long_string = format!("\"{}\"", "x".repeat(LONG as usize + 7));
        let long_comment = format!("/*{}*/", "y".repeat(LONG as usize * 2));
        let mut text = String::new();
        for i in 0..200 {
            text.push_str(&format!("fn f{i}(a: u32) -> u32 {{ a + {i} }} // {i}\n"));
            if i == 37 {
                text.push_str(&long_string);
            }
            if i == 150 {
                text.push_str(&long_comment);
                text.push('\n');
            }
        }
        let lexed = LexedStr::new(Edition::CURRENT, &text);
        assert!(lexed.long.len() == 2, "both long tokens are in the side table");
        let mut start = 0;
        for i in 0..lexed.len() {
            let len = lexed.text_len(i);
            assert_eq!(lexed.text_start(i), start);
            assert_eq!(lexed.text_range(i), start..start + len);
            assert_eq!(lexed.text(i), &text[start..start + len]);
            start += len;
        }
        assert_eq!(start, text.len());
        assert_eq!(lexed.text_start(lexed.len()), text.len());
        assert_eq!(lexed.range_text(0..lexed.len()), text);
        let kinds: Vec<_> = (0..lexed.len()).map(|i| lexed.kind(i)).collect();
        assert!(kinds.contains(&STRING) && kinds.contains(&COMMENT) && kinds.contains(&FN_KW));
        assert_eq!(LexedStr::single_token(Edition::CURRENT, &long_string).map(|it| it.0), Some(STRING));
    }

    /// Two lexings' input, one after the other, is the input of their texts lexed as one, split
    /// at any token boundary.
    #[test]
    fn input_followed_by_is_the_joined_input() {
        let text = "fn f() -> u8 { x.0.1 + 1. ; union::default; a::<'b>>=c /* d */ }";
        let edition = Edition::CURRENT;
        let whole = LexedStr::new(edition, text);
        let input = |it: &crate::Input| {
            (0..it.len())
                .map(|i| (it.kind(i), it.contextual_kind(i), it.is_joint(i)))
                .collect::<Vec<_>>()
        };
        let expected = input(&whole.to_input(edition));
        for split in 0..=whole.len() {
            let at = whole.text_start(split);
            let (head, tail) = (LexedStr::new(edition, &text[..at]), LexedStr::new(edition, &text[at..]));
            if head.len() + tail.len() != whole.len() {
                continue;
            }
            assert_eq!(input(&head.to_input_followed_by(&tail, edition)), expected, "split at {at}");
        }
    }
}

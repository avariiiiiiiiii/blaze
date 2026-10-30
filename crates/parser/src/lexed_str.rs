//! Lexing `&str` into a sequence of Rust tokens.
//!
//! Note that strictly speaking the parser in this crate is not required to work
//! on tokens which originated from text. Macros, eg, can synthesize tokens out
//! of thin air. So, ideally, lexer should be an orthogonal crate. It is however
//! convenient to include a text-based lexer here!
//!
//! Note that these tokens, unlike the tokens we feed into the parser, do
//! include info about comments and whitespace.

use std::ops;

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

struct LexError {
    msg: String,
    token: u32,
}

impl<'a> LexedStr<'a> {
    pub fn new(edition: Edition, text: &'a str) -> LexedStr<'a> {
        let _p = tracing::info_span!("LexedStr::new").entered();
        assert!(text.len() <= u32::MAX as usize, "a text of 4 GiB or more");
        let mut conv = Converter::new(edition, text);
        if let Ok(script) = crate::frontmatter::ScriptSource::parse(text) {
            if let Some(shebang) = script.shebang_span() {
                conv.push(SHEBANG, shebang.end - shebang.start, Vec::new());
            }
            if script.frontmatter().is_some() {
                conv.push(FRONTMATTER, script.content_span().start - conv.offset, Vec::new());
            }
        } else if let Some(shebang_len) = rustc_lexer::strip_shebang(text) {
            // Leave error reporting to `rustc_lexer`
            conv.push(SHEBANG, shebang_len, Vec::new());
        }

        // Re-create the tokenizer from scratch every token because `GuardedStrPrefix` is one token in the lexer
        // but we want to split it to two in edition <2024.
        while let Some(token) =
            rustc_lexer::tokenize(&text[conv.offset..], rustc_lexer::FrontmatterAllowed::No).next()
        {
            let token_text = &text[conv.offset..][..token.len as usize];

            conv.extend_token(&token.kind, token_text);
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
            1 => Some((conv.res.raw_kind(0), conv.res.error.pop().map(|it| it.msg))),
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
        Some(self.error[err].msg.as_str())
    }

    pub fn errors(&self) -> impl Iterator<Item = (usize, &str)> + '_ {
        self.error.iter().map(|it| (it.token as usize, it.msg.as_str()))
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

    /// Check for likely unterminated string by analyzing STRING token content
    fn has_likely_unterminated_string(&self) -> bool {
        let mut end = self.offset;
        for i in (0..self.res.tokens.len()).rev().take(5) {
            let start = end - self.res.raw_len(i);
            if self.res.raw_kind(i) == STRING {
                let content = &self.res.text[start..end];

                if content.contains('(') && (content.contains("//") || content.contains(";\n")) {
                    return true;
                }
            }
            end = start;
        }
        false
    }

    fn finalize_with_eof(mut self) -> LexedStr<'a> {
        self.res.push(EOF, self.offset, 0);
        self.res
    }

    fn push(&mut self, kind: SyntaxKind, len: usize, errors: Vec<String>) {
        self.res.push(kind, self.offset, len);
        self.offset += len;

        let token = (self.res.tokens.len() - 1) as u32;
        for msg in errors {
            if !msg.is_empty() {
                self.res.error.push(LexError { msg, token });
            }
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
        let mut errors: Vec<String> = vec![];

        let syntax_kind = {
            match kind {
                rustc_lexer::TokenKind::LineComment { doc_style } => Self::comment_kind(*doc_style),
                rustc_lexer::TokenKind::BlockComment { doc_style, terminated } => {
                    if !terminated {
                        errors.push(
                            "Missing trailing `*/` symbols to terminate the block comment".into(),
                        );
                    }
                    Self::comment_kind(*doc_style)
                }

                rustc_lexer::TokenKind::Frontmatter {
                    has_invalid_preceding_whitespace,
                    invalid_infostring,
                } => {
                    if *has_invalid_preceding_whitespace {
                        errors.push("invalid preceding whitespace for frontmatter opening".into());
                    } else if *invalid_infostring {
                        errors.push("invalid infostring for frontmatter".into());
                    }
                    FRONTMATTER
                }

                rustc_lexer::TokenKind::Whitespace => WHITESPACE,

                rustc_lexer::TokenKind::Ident if token_text == "_" => UNDERSCORE,
                rustc_lexer::TokenKind::Ident => {
                    SyntaxKind::from_keyword(token_text, self.edition).unwrap_or(IDENT)
                }
                rustc_lexer::TokenKind::InvalidIdent => {
                    errors.push("Ident contains invalid characters".into());
                    IDENT
                }

                rustc_lexer::TokenKind::RawIdent => IDENT,
                #[cfg(feature = "in-rust-tree")]
                rustc_lexer::TokenKind::ForcedKeywordIdent => ERROR,

                rustc_lexer::TokenKind::GuardedStrPrefix if self.edition.at_least_2024() => {
                    // FIXME: rustc does something better for recovery.
                    errors.push("Invalid string literal (reserved syntax)".into());
                    ERROR
                }
                rustc_lexer::TokenKind::GuardedStrPrefix => {
                    // The token is `#"` or `##`, split it into two.
                    token_text = &token_text[1..];
                    POUND
                }

                rustc_lexer::TokenKind::Literal { kind, .. } => {
                    self.extend_literal(token_text.len(), kind);
                    return;
                }

                rustc_lexer::TokenKind::Lifetime { starts_with_number } => {
                    if *starts_with_number {
                        errors.push("Lifetime name cannot start with a number".into());
                    }
                    LIFETIME_IDENT
                }
                rustc_lexer::TokenKind::UnknownPrefixLifetime => {
                    errors.push("Unknown lifetime prefix".into());
                    LIFETIME_IDENT
                }
                rustc_lexer::TokenKind::RawLifetime => LIFETIME_IDENT,

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
                rustc_lexer::TokenKind::UnknownPrefix => {
                    let has_unterminated = self.has_likely_unterminated_string();

                    let error_msg = if has_unterminated {
                        format!(
                            "unknown literal prefix `{token_text}` (note: check for unterminated string literal)"
                        )
                    } else {
                        "unknown literal prefix".to_owned()
                    };
                    errors.push(error_msg);
                    IDENT
                }
                rustc_lexer::TokenKind::Eof => EOF,
            }
        };

        self.push(syntax_kind, token_text.len(), errors);
    }

    fn extend_literal(&mut self, len: usize, kind: &rustc_lexer::LiteralKind) {
        let invalid_raw_msg = String::from("Invalid raw string literal");

        let mut errors = vec![];
        let mut no_end_quote = |c: char, kind: &str| {
            errors.push(format!("Missing trailing `{c}` symbol to terminate the {kind} literal"));
        };

        let syntax_kind = match *kind {
            rustc_lexer::LiteralKind::Int { empty_int, base: _ } => {
                if empty_int {
                    errors.push("Missing digits after the integer base prefix".into());
                }
                INT_NUMBER
            }
            rustc_lexer::LiteralKind::Float { empty_exponent, base: _ } => {
                if empty_exponent {
                    errors.push("Missing digits after the exponent symbol".into());
                }
                FLOAT_NUMBER
            }
            rustc_lexer::LiteralKind::Char { terminated } => {
                if !terminated {
                    no_end_quote('\'', "character");
                } else {
                    let text = &self.res.text[self.offset + 1..][..len - 1];
                    let text = &text[..text.rfind('\'').unwrap()];
                    if let Err(e) = unescape_char(text) {
                        errors.push(err_to_msg(e, Mode::Char));
                    }
                }
                CHAR
            }
            rustc_lexer::LiteralKind::Byte { terminated } => {
                if !terminated {
                    no_end_quote('\'', "byte");
                } else {
                    let text = &self.res.text[self.offset + 2..][..len - 2];
                    let text = &text[..text.rfind('\'').unwrap()];
                    if let Err(e) = unescape_byte(text) {
                        errors.push(err_to_msg(e, Mode::Byte));
                    }
                }
                BYTE
            }
            rustc_lexer::LiteralKind::Str { terminated } => {
                if !terminated {
                    no_end_quote('"', "string");
                } else {
                    let text = &self.res.text[self.offset + 1..][..len - 1];
                    let text = &text[..text.rfind('"').unwrap()];
                    unescape_str(text, |_, res| {
                        if let Err(e) = res {
                            errors.push(err_to_msg(e, Mode::Str));
                        }
                    });
                }
                STRING
            }
            rustc_lexer::LiteralKind::ByteStr { terminated } => {
                if !terminated {
                    no_end_quote('"', "byte string");
                } else {
                    let text = &self.res.text[self.offset + 2..][..len - 2];
                    let text = &text[..text.rfind('"').unwrap()];
                    unescape_byte_str(text, |_, res| {
                        if let Err(e) = res {
                            errors.push(err_to_msg(e, Mode::ByteStr));
                        }
                    });
                }
                BYTE_STRING
            }
            rustc_lexer::LiteralKind::CStr { terminated } => {
                if !terminated {
                    no_end_quote('"', "C string")
                } else {
                    let text = &self.res.text[self.offset + 2..][..len - 2];
                    let text = &text[..text.rfind('"').unwrap()];
                    unescape_c_str(text, |_, res| {
                        if let Err(e) = res {
                            errors.push(err_to_msg(e, Mode::CStr));
                        }
                    });
                }
                C_STRING
            }
            rustc_lexer::LiteralKind::RawStr { n_hashes } => {
                if n_hashes.is_none() {
                    errors.push(invalid_raw_msg);
                }
                STRING
            }
            rustc_lexer::LiteralKind::RawByteStr { n_hashes } => {
                if n_hashes.is_none() {
                    errors.push(invalid_raw_msg);
                }
                BYTE_STRING
            }
            rustc_lexer::LiteralKind::RawCStr { n_hashes } => {
                if n_hashes.is_none() {
                    errors.push(invalid_raw_msg);
                }
                C_STRING
            }
        };

        self.push(syntax_kind, len, errors);
    }
}

fn err_to_msg(error: EscapeError, mode: Mode) -> String {
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
    .into()
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

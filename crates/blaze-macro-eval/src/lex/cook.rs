//! rustc's lexer proper (`rustc_parse::lexer::Lexer::next_token_from_cursor` and the `cook_*`
//! functions): `rustc_lexer`'s tokens made into rustc's, with every error rustc emits on the
//! way. A fatal error (an unterminated literal or block comment, a bad raw string) stops the
//! lexer as rustc's `FatalError` does.

use ra_ap_rustc_lexer::{self as rustc_lexer, Base, Cursor, DocStyle, FrontmatterAllowed, LiteralKind};
use rustc_literal_escaper::{Mode, check_for_errors};
use span::Edition;
use unicode_normalization::{IsNormalized, UnicodeNormalization, is_nfc_quick};

use intern::Symbol;

use super::unicode::{confusable, is_confusable};
use super::{Emitted, Lint, unescape};
use crate::print::Op;
use crate::stream::{Delimiter, DocKind, IdentKind, LitKind};

/// rustc's `FatalError`: the error was pushed to [`Emitted::errors`].
#[derive(Debug)]
pub(super) struct Fatal;

/// One of rustc's tokens (`rustc_ast::token::TokenKind`, the kinds its lexer makes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Op(Op),
    Open(Delimiter),
    Close(Delimiter),
    Ident(Symbol, IdentKind),
    /// With its leading `'`.
    Lifetime(Symbol, IdentKind),
    Literal { kind: LitKind, symbol: Symbol, suffix: Option<Symbol> },
    Doc { kind: DocKind, inner: bool, data: Symbol },
    Eof,
}

/// A token and its bytes `lo..hi` of the text.
#[derive(Clone, Copy, Debug)]
pub(super) struct Token {
    pub(super) kind: Kind,
    pub(super) lo: u32,
    pub(super) hi: u32,
}

impl Token {
    /// `Token::glue`: of rustc's tokens only punctuation glues (its lexer never makes the lone
    /// `'` that glues to an identifier).
    pub(super) fn glue(&self, next: &Token) -> Option<Token> {
        match (self.kind, next.kind) {
            (Kind::Op(a), Kind::Op(b)) => {
                a.glue(b).map(|op| Token { kind: Kind::Op(op), lo: self.lo, hi: next.hi })
            }
            _ => None,
        }
    }

    pub(super) fn is_punct(&self) -> bool {
        matches!(self.kind, Kind::Op(_))
    }
}

/// `nfc_normalize`.
pub(super) fn nfc_normalize(string: &str) -> Symbol {
    match is_nfc_quick(string.chars()) {
        IsNormalized::Yes => Symbol::intern(string),
        // alloc: text that is not in NFC, rare.
        _ => Symbol::intern(&string.chars().nfc().collect::<String>()),
    }
}

/// `Symbol::can_be_raw`.
pub(super) fn can_be_raw(sym: &str) -> bool {
    !matches!(sym, "" | "_" | "super" | "self" | "Self" | "crate" | "{{root}}" | "$crate")
}

/// `escaped_char`: a character as rustc's messages show it.
pub(super) fn escaped_char(c: char, out: &mut String) {
    match c {
        '\u{20}'..='\u{7e}' => out.push(c),
        _ => out.extend(c.escape_default()),
    }
}

/// `contains_text_flow_control_chars`.
fn contains_text_flow_control_chars(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut at = 0;
    while let Some(idx) = bytes[at..].iter().position(|&b| b == 0xE2) {
        let i = at + idx;
        match bytes[i..i + 3] {
            [_, 0x80, 0xAA..=0xAE] | [_, 0x81, 0xA6..=0xA9] => return true,
            _ => at = i + 3,
        }
    }
    false
}

pub(super) struct Lexer<'s, 'e> {
    /// The text, normalized as rustc's `SourceFile` is.
    src: &'s str,
    cursor: Cursor<'s>,
    /// Where `cursor` is in `src`.
    pos: u32,
    /// The edition of the tokens' span.
    edition: Edition,
    /// An unknown `\u{a0}` was reported already, so the next ones are whitespace.
    nbsp_is_whitespace: bool,
    pub(super) emitted: &'e mut Emitted,
}

impl<'s, 'e> Lexer<'s, 'e> {
    pub(super) fn new(src: &'s str, edition: Edition, emitted: &'e mut Emitted) -> Self {
        Lexer {
            src,
            cursor: Cursor::new(src, FrontmatterAllowed::No),
            pos: 0,
            edition,
            nbsp_is_whitespace: false,
            emitted,
        }
    }

    fn str_from(&self, start: u32) -> &'s str {
        &self.src[start as usize..self.pos as usize]
    }

    fn str_from_to(&self, start: u32, end: u32) -> &'s str {
        &self.src[start as usize..end as usize]
    }

    fn error(&mut self, message: impl Into<String>) {
        // alloc: an error's message, said once
        self.emitted.errors.push(message.into());
    }

    fn fatal(&mut self, message: impl Into<String>) -> Fatal {
        // alloc: as for `error`
        self.emitted.errors.push(message.into());
        Fatal
    }

    /// Lexes again from `at`, as rustc does when it takes back part of a token.
    fn reset_to(&mut self, at: u32) {
        self.pos = at;
        self.cursor = Cursor::new(&self.src[at as usize..], FrontmatterAllowed::No);
    }

    /// The next token, and whether whitespace or a comment came before it.
    pub(super) fn next_token(&mut self) -> Result<(Token, bool), Fatal> {
        let mut preceded_by_whitespace = false;
        let mut swallow_next_invalid = 0usize;
        loop {
            let token = self.cursor.advance_token();
            let start = self.pos;
            self.pos += token.len;

            let kind = match token.kind {
                rustc_lexer::TokenKind::LineComment { doc_style } => {
                    let Some(doc_style) = doc_style else {
                        self.lint_unicode_text_flow(start);
                        preceded_by_whitespace = true;
                        continue;
                    };
                    let content = self.str_from(start + 3);
                    self.lint_doc_comment_unicode_text_flow(content);
                    self.cook_doc_comment(content, DocKind::Line, doc_style)
                }
                rustc_lexer::TokenKind::BlockComment { doc_style, terminated } => {
                    if !terminated {
                        return Err(self.fatal(
                            match doc_style {
                                Some(_) => "unterminated block doc-comment",
                                None => "unterminated block comment",
                            },
                        ));
                    }
                    let Some(doc_style) = doc_style else {
                        self.lint_unicode_text_flow(start);
                        preceded_by_whitespace = true;
                        continue;
                    };
                    let content = self.str_from_to(start + 3, self.pos - 2);
                    self.lint_doc_comment_unicode_text_flow(content);
                    self.cook_doc_comment(content, DocKind::Block, doc_style)
                }
                // `FrontmatterAllowed::No` makes none; rustc skips one as whitespace.
                rustc_lexer::TokenKind::Frontmatter { .. }
                | rustc_lexer::TokenKind::Whitespace => {
                    preceded_by_whitespace = true;
                    continue;
                }
                rustc_lexer::TokenKind::Ident => self.ident(start),
                rustc_lexer::TokenKind::RawIdent => {
                    let sym = nfc_normalize(self.str_from(start + 2));
                    if !can_be_raw(sym.as_str()) {
                        self.error(format!("`{}` cannot be a raw identifier", sym.as_str())); // alloc: error path
                    }
                    Kind::Ident(sym, IdentKind::Raw)
                }
                rustc_lexer::TokenKind::UnknownPrefix => match self.forced_keyword(start) {
                    Some(kind) => kind,
                    None => {
                        self.report_unknown_prefix(start);
                        self.ident(start)
                    }
                },
                rustc_lexer::TokenKind::UnknownPrefixLifetime => {
                    self.report_unknown_prefix(start);
                    Kind::Lifetime(Symbol::intern(self.str_from(start)), IdentKind::Normal)
                }
                rustc_lexer::TokenKind::InvalidIdent if !self.is_one_confusable(start) => {
                    let sym = nfc_normalize(self.str_from(start));
                    // Reported once per identifier after expansion (`bad_unicode_identifiers`).
                    let message = match sym.as_str() {
                        "🦀" => "Ferris cannot be used as an identifier".to_owned(), // alloc: error path
                        text => format!("identifiers cannot contain emoji: `{text}`"), // alloc: error path
                    };
                    if !self.emitted.errors.contains(&message) {
                        self.error(message);
                    }
                    Kind::Ident(sym, IdentKind::Normal)
                }
                rustc_lexer::TokenKind::Literal {
                    kind: kind @ (LiteralKind::CStr { .. } | LiteralKind::RawCStr { .. }),
                    ..
                } if self.edition < Edition::Edition2021 => {
                    // Only the prefix (`c` or `cr`) is an identifier before 2021.
                    let prefix_len = if matches!(kind, LiteralKind::CStr { .. }) { 1 } else { 2 };
                    self.reset_to(start + prefix_len);
                    self.ident(start)
                }
                rustc_lexer::TokenKind::GuardedStrPrefix => self.maybe_report_guarded_str(start)?,
                rustc_lexer::TokenKind::Literal { kind, suffix_start } => {
                    let suffix_start = start + suffix_start;
                    let (kind, symbol) = self.cook_lexer_literal(start, suffix_start, kind)?;
                    let suffix = if suffix_start < self.pos {
                        let string = self.str_from(suffix_start);
                        if string == "_" {
                            self.error("underscore literal suffix is not allowed");
                            None
                        } else {
                            Some(Symbol::intern(string))
                        }
                    } else {
                        None
                    };
                    self.lint_literal_unicode_text_flow(symbol, kind);
                    Kind::Literal { kind, symbol, suffix }
                }
                rustc_lexer::TokenKind::Lifetime { starts_with_number } => {
                    let lifetime_name = nfc_normalize(self.str_from(start));
                    if starts_with_number {
                        // Stashed by rustc's lexer, and emitted unless the parser replaces it.
                        self.error("lifetimes cannot start with a number");
                    }
                    Kind::Lifetime(lifetime_name, IdentKind::Normal)
                }
                rustc_lexer::TokenKind::RawLifetime => {
                    let ident_start = start + 3;
                    if self.edition >= Edition::Edition2021 {
                        if self.cursor.as_str().starts_with('\'') {
                            self.error(
                                "character literal may only contain one codepoint",
                            );
                        }
                        let name = self.str_from(ident_start);
                        if !can_be_raw(name) {
                            self.error(format!("`{name}` cannot be a raw lifetime")); // alloc: error path
                        }
                        // alloc: the `'` put back before the name, as rustc does.
                        let sym = nfc_normalize(&format!("'{name}"));
                        Kind::Lifetime(sym, IdentKind::Raw)
                    } else {
                        // Only `'r` is a lifetime before 2021.
                        self.reset_to(start + 2);
                        Kind::Lifetime(nfc_normalize(self.str_from(start)), IdentKind::Normal)
                    }
                }
                rustc_lexer::TokenKind::Semi => Kind::Op(Op::Semi),
                rustc_lexer::TokenKind::Comma => Kind::Op(Op::Comma),
                rustc_lexer::TokenKind::Dot => Kind::Op(Op::Dot),
                rustc_lexer::TokenKind::OpenParen => Kind::Open(Delimiter::Parenthesis),
                rustc_lexer::TokenKind::CloseParen => Kind::Close(Delimiter::Parenthesis),
                rustc_lexer::TokenKind::OpenBrace => Kind::Open(Delimiter::Brace),
                rustc_lexer::TokenKind::CloseBrace => Kind::Close(Delimiter::Brace),
                rustc_lexer::TokenKind::OpenBracket => Kind::Open(Delimiter::Bracket),
                rustc_lexer::TokenKind::CloseBracket => Kind::Close(Delimiter::Bracket),
                rustc_lexer::TokenKind::At => Kind::Op(Op::At),
                rustc_lexer::TokenKind::Pound => Kind::Op(Op::Pound),
                rustc_lexer::TokenKind::Tilde => Kind::Op(Op::Tilde),
                rustc_lexer::TokenKind::Question => Kind::Op(Op::Question),
                rustc_lexer::TokenKind::Colon => Kind::Op(Op::Colon),
                rustc_lexer::TokenKind::Dollar => Kind::Op(Op::Dollar),
                rustc_lexer::TokenKind::Eq => Kind::Op(Op::Eq),
                rustc_lexer::TokenKind::Bang => Kind::Op(Op::Bang),
                rustc_lexer::TokenKind::Lt => Kind::Op(Op::Lt),
                rustc_lexer::TokenKind::Gt => Kind::Op(Op::Gt),
                rustc_lexer::TokenKind::Minus => Kind::Op(Op::Minus),
                rustc_lexer::TokenKind::And => Kind::Op(Op::And),
                rustc_lexer::TokenKind::Or => Kind::Op(Op::Or),
                rustc_lexer::TokenKind::Plus => Kind::Op(Op::Plus),
                rustc_lexer::TokenKind::Star => Kind::Op(Op::Star),
                rustc_lexer::TokenKind::Slash => Kind::Op(Op::Slash),
                rustc_lexer::TokenKind::Caret => Kind::Op(Op::Caret),
                rustc_lexer::TokenKind::Percent => Kind::Op(Op::Percent),
                rustc_lexer::TokenKind::Unknown | rustc_lexer::TokenKind::InvalidIdent => {
                    // No error for a run of the same invalid token.
                    if swallow_next_invalid > 0 {
                        swallow_next_invalid -= 1;
                        continue;
                    }
                    let mut it = self.src[start as usize..].chars();
                    let c = it.next().expect("an unknown token has a character");
                    if c == '\u{a0}' {
                        if self.nbsp_is_whitespace {
                            preceded_by_whitespace = true;
                            continue;
                        }
                        self.nbsp_is_whitespace = true;
                    }
                    let repeats = it.take_while(|c1| *c1 == c).count();
                    let token = substitution(c);
                    let mut message = String::from("unknown start of token: "); // alloc: error path
                    escaped_char(c, &mut message);
                    self.error(message);
                    if repeats > 0 {
                        swallow_next_invalid = repeats;
                    }
                    match token {
                        Some(kind) => kind,
                        None => {
                            preceded_by_whitespace = true;
                            continue;
                        }
                    }
                }
                rustc_lexer::TokenKind::Eof => Kind::Eof,
            };
            return Ok((Token { kind, lo: start, hi: self.pos }, preceded_by_whitespace));
        }
    }

    fn ident(&self, start: u32) -> Kind {
        Kind::Ident(nfc_normalize(self.str_from(start)), IdentKind::Normal)
    }

    /// The token is the one character rustc's confusables table has.
    fn is_one_confusable(&self, start: u32) -> bool {
        let mut chars = self.str_from(start).chars();
        matches!((chars.next(), chars.next()), (Some(c), None) if is_confusable(c))
    }

    /// `k#ident`: the pinned `rustc_lexer`'s `ForcedKeywordIdent`, which the lexer crate
    /// blaze links lexes as the unknown prefix `k`. From 2021 it is a forced keyword; before,
    /// rustc takes back all but the `k`, which is then an identifier as here.
    fn forced_keyword(&mut self, start: u32) -> Option<Kind> {
        if self.str_from(start) != "k" || self.edition < Edition::Edition2021 {
            return None;
        }
        let rest = self.cursor.as_str().strip_prefix('#')?;
        let mut chars = rest.chars();
        let first = chars.next().filter(|&c| rustc_lexer::is_id_start(c))?;
        let ident_len = first.len_utf8()
            + chars.take_while(|&c| rustc_lexer::is_id_continue(c)).map(char::len_utf8).sum::<usize>();
        self.reset_to(self.pos + 1 + ident_len as u32);
        self.emitted.forced_keywords += 1;
        Some(Kind::Ident(nfc_normalize(self.str_from(start + 2)), IdentKind::ForcedKeyword))
    }

    /// `report_unknown_prefix`: an error from 2021, a lint (allowed by default) before.
    fn report_unknown_prefix(&mut self, start: u32) {
        if self.edition >= Edition::Edition2021 {
            let prefix = self.str_from(start);
            self.error(format!("prefix `{prefix}` is unknown")); // alloc: error path
        }
    }

    /// `maybe_report_guarded_str`: `#"..."` or `##` is reserved from 2024; before, it is a
    /// `#` and what follows.
    fn maybe_report_guarded_str(&mut self, start: u32) -> Result<Kind, Fatal> {
        let edition2024 = self.edition >= Edition::Edition2024;
        let mut cursor =
            Cursor::new(&self.src[start as usize..], FrontmatterAllowed::No);
        let (is_string, unterminated) = match cursor.guarded_double_quoted_string() {
            Some(rustc_lexer::GuardedStr { n_hashes: _, terminated, token_len }) => {
                if edition2024 {
                    self.cursor = cursor;
                    self.pos = start + token_len;
                }
                (true, !terminated)
            }
            None => (false, false),
        };
        if edition2024 {
            if unterminated {
                return Err(self.fatal("unterminated double quote string"));
            }
            self.error(
                match is_string {
                    true => "invalid string literal",
                    false => "reserved multi-hash token is forbidden",
                },
            );
            Ok(Kind::Literal {
                kind: LitKind::Err,
                symbol: Symbol::intern(self.str_from(start)),
                suffix: None,
            })
        } else {
            self.reset_to(start + 1);
            Ok(Kind::Op(Op::Pound))
        }
    }

    fn cook_doc_comment(&mut self, content: &str, kind: DocKind, doc_style: DocStyle) -> Kind {
        for _ in content.bytes().filter(|&b| b == b'\r') {
            self.error(
                match kind {
                    DocKind::Block => "bare CR not allowed in block doc-comment",
                    DocKind::Line => "bare CR not allowed in doc-comment",
                },
            );
        }
        Kind::Doc {
            kind,
            inner: doc_style == DocStyle::Inner,
            data: Symbol::intern(content),
        }
    }

    fn cook_lexer_literal(
        &mut self,
        start: u32,
        end: u32,
        kind: LiteralKind,
    ) -> Result<(LitKind, Symbol), Fatal> {
        let unterminated = |lexer: &mut Self, terminated: bool, message: &str| match terminated {
            true => Ok(()),
            false => Err(lexer.fatal(message)),
        };
        Ok(match kind {
            LiteralKind::Char { terminated } => {
                unterminated(self, terminated, "unterminated character literal")?;
                self.cook_quoted(LitKind::Char, Mode::Char, start, end, 1, 1)
            }
            LiteralKind::Byte { terminated } => {
                unterminated(self, terminated, "unterminated byte constant")?;
                self.cook_quoted(LitKind::Byte, Mode::Byte, start, end, 2, 1)
            }
            LiteralKind::Str { terminated } => {
                unterminated(self, terminated, "unterminated double quote string")?;
                self.cook_quoted(LitKind::Str, Mode::Str, start, end, 1, 1)
            }
            LiteralKind::ByteStr { terminated } => {
                unterminated(self, terminated, "unterminated double quote byte string")?;
                self.cook_quoted(LitKind::ByteStr, Mode::ByteStr, start, end, 2, 1)
            }
            LiteralKind::CStr { terminated } => {
                unterminated(self, terminated, "unterminated C string")?;
                self.cook_quoted(LitKind::CStr, Mode::CStr, start, end, 2, 1)
            }
            LiteralKind::RawStr { n_hashes } => match n_hashes {
                Some(n) => {
                    let n32 = u32::from(n);
                    self.cook_quoted(LitKind::StrRaw(n), Mode::RawStr, start, end, 2 + n32, 1 + n32)
                }
                None => return Err(self.report_raw_str_error(start, 1)),
            },
            LiteralKind::RawByteStr { n_hashes } => match n_hashes {
                Some(n) => {
                    let n32 = u32::from(n);
                    let kind = LitKind::ByteStrRaw(n);
                    self.cook_quoted(kind, Mode::RawByteStr, start, end, 3 + n32, 1 + n32)
                }
                None => return Err(self.report_raw_str_error(start, 2)),
            },
            LiteralKind::RawCStr { n_hashes } => match n_hashes {
                Some(n) => {
                    let n32 = u32::from(n);
                    self.cook_quoted(LitKind::CStrRaw(n), Mode::RawCStr, start, end, 3 + n32, 1 + n32)
                }
                None => return Err(self.report_raw_str_error(start, 2)),
            },
            LiteralKind::Int { base, empty_int } => {
                let mut kind = LitKind::Integer;
                if empty_int {
                    self.error("no valid digits found for number");
                    kind = LitKind::Err;
                } else if matches!(base, Base::Binary | Base::Octal) {
                    let radix = base as u32;
                    let digits = self.str_from_to(start + 2, end);
                    for c in digits.chars() {
                        if c != '_' && c.to_digit(radix).is_none() {
                            self.error(format!("invalid digit for a base {radix} literal")); // alloc: error path
                            kind = LitKind::Err;
                        }
                    }
                }
                (kind, Symbol::intern(self.str_from_to(start, end)))
            }
            LiteralKind::Float { base, empty_exponent } => {
                let mut kind = LitKind::Float;
                if empty_exponent {
                    self.error("expected at least one digit in exponent");
                    kind = LitKind::Err;
                }
                let base = match base {
                    Base::Hexadecimal => Some("hexadecimal"),
                    Base::Octal => Some("octal"),
                    Base::Binary => Some("binary"),
                    Base::Decimal => None,
                };
                if let Some(base) = base {
                    self.error(format!("{base} float literal is not supported")); // alloc: error path
                    kind = LitKind::Err;
                }
                (kind, Symbol::intern(self.str_from_to(start, end)))
            }
        })
    }

    /// `report_raw_str_error`: always fatal.
    fn report_raw_str_error(&mut self, start: u32, prefix_len: u32) -> Fatal {
        let message = match rustc_lexer::validate_raw_str(&self.src[start as usize..], prefix_len) {
            Err(rustc_lexer::RawStrError::InvalidStarter { bad_char }) => {
                // alloc: error path
                let mut message = String::from(
                    "found invalid character; only `#` is allowed in raw string delimitation: ",
                );
                escaped_char(bad_char, &mut message);
                message
            }
            Err(rustc_lexer::RawStrError::NoTerminator { .. }) => {
                "unterminated raw string".to_owned() // alloc: error path
            }
            // alloc: error path
            Err(rustc_lexer::RawStrError::TooManyDelimiters { found }) => format!(
                "too many `#` symbols: raw strings may be delimited by up to 255 `#` symbols, but found {found}"
            ),
            Ok(()) => panic!("no error found for supposedly invalid raw string literal"),
        };
        self.fatal(message)
    }

    fn cook_quoted(
        &mut self,
        mut kind: LitKind,
        mode: Mode,
        start: u32,
        end: u32,
        prefix_len: u32,
        postfix_len: u32,
    ) -> (LitKind, Symbol) {
        let lit_content = self.str_from_to(start + prefix_len, end - postfix_len);
        let errors = &mut self.emitted.errors;
        check_for_errors(lit_content, mode, |range, err| {
            if let Some(message) = unescape::message(lit_content, mode, range, &err) {
                debug_assert!(err.is_fatal());
                errors.push(message);
                kind = LitKind::Err;
            }
        });
        // An error keeps the quotes in the symbol, as rustc's does.
        let sym = match kind {
            LitKind::Err => Symbol::intern(self.str_from_to(start, end)),
            _ => Symbol::intern(lit_content),
        };
        (kind, sym)
    }

    fn lint_unicode_text_flow(&mut self, start: u32) {
        if contains_text_flow_control_chars(self.str_from(start + 2)) {
            self.emitted.lints.push((
                Lint::TextDirectionCodepointInComment,
                "unicode codepoint changing visible direction of text present in comment".to_owned(), // alloc: error path
            ));
        }
    }

    fn lint_doc_comment_unicode_text_flow(&mut self, content: &str) {
        if contains_text_flow_control_chars(content) {
            self.text_direction_in("doc comment");
        }
    }

    fn lint_literal_unicode_text_flow(&mut self, text: Symbol, kind: LitKind) {
        if kind != LitKind::Err && contains_text_flow_control_chars(text.as_str()) {
            self.text_direction_in("literal");
        }
    }

    fn text_direction_in(&mut self, label: &str) {
        self.emitted.lints.push((
            Lint::TextDirectionCodepointInLiteral,
            format!("unicode codepoint changing visible direction of text present in {label}"), // alloc: error path
        ));
    }
}

/// `check_for_substitution`'s token: what rustc lexes a confusable as, with its error.
fn substitution(c: char) -> Option<Kind> {
    Some(match confusable(c)? {
        "_" => Kind::Ident(Symbol::intern("_"), IdentKind::Normal),
        "-" => Kind::Op(Op::Minus),
        "," => Kind::Op(Op::Comma),
        ";" => Kind::Op(Op::Semi),
        ":" => Kind::Op(Op::Colon),
        "!" => Kind::Op(Op::Bang),
        "?" => Kind::Op(Op::Question),
        "." => Kind::Op(Op::Dot),
        "(" => Kind::Open(Delimiter::Parenthesis),
        ")" => Kind::Close(Delimiter::Parenthesis),
        "[" => Kind::Open(Delimiter::Bracket),
        "]" => Kind::Close(Delimiter::Bracket),
        "{" => Kind::Open(Delimiter::Brace),
        "}" => Kind::Close(Delimiter::Brace),
        "*" => Kind::Op(Op::Star),
        "/" => Kind::Op(Op::Slash),
        "&" => Kind::Op(Op::And),
        "+" => Kind::Op(Op::Plus),
        "<" => Kind::Op(Op::Lt),
        "=" => Kind::Op(Op::Eq),
        "==" => Kind::Op(Op::EqEq),
        ">" => Kind::Op(Op::Gt),
        // Space, backslash and the quotes have no token.
        _ => return None,
    })
}

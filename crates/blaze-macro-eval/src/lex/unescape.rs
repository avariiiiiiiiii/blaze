//! The messages of rustc's escape errors (`rustc_parse::lexer::unescape_error_reporting::
//! emit_unescape_error` and the `UnescapeError` diagnostics): the primary message of each, as
//! the session shows it.

use std::ops::Range;

use rustc_literal_escaper::{EscapeError, Mode};

use super::cook::escaped_char;

/// The error `emit_unescape_error` emits for `error` at `range` of `lit` (the literal between
/// its quotes), or `None` for the two that are warnings.
pub(super) fn message(lit: &str, mode: Mode, range: Range<usize>, error: &EscapeError) -> Option<String> {
    let last_char = || lit[range.start..range.end].chars().next_back().expect("an escape error covers a character");
    // The message, and the character it ends with between backquotes.
    let (head, shown): (&str, Option<char>) = match error {
        EscapeError::LoneSurrogateUnicodeEscape | EscapeError::OutOfRangeUnicodeEscape => {
            ("invalid unicode character escape", None)
        }
        EscapeError::MoreThanOneChar => ("character literal may only contain one codepoint", None),
        EscapeError::EscapeOnlyChar => match mode {
            Mode::Byte => ("byte constant must be escaped: ", Some(last_char())),
            _ => ("character constant must be escaped: ", Some(last_char())),
        },
        EscapeError::BareCarriageReturn => match mode.in_double_quotes() {
            true => ("bare CR not allowed in string, use `\\r` instead", None),
            false => ("character constant must be escaped: `\\r`", None),
        },
        EscapeError::BareCarriageReturnInRawString => ("bare CR not allowed in raw string", None),
        EscapeError::InvalidEscape => match mode {
            Mode::Byte | Mode::ByteStr => ("unknown byte escape: ", Some(last_char())),
            _ => ("unknown character escape: ", Some(last_char())),
        },
        EscapeError::TooShortHexEscape => ("numeric character escape is too short", None),
        EscapeError::InvalidCharInHexEscape => {
            ("invalid character in numeric character escape: ", Some(last_char()))
        }
        EscapeError::InvalidCharInUnicodeEscape => {
            ("invalid character in unicode escape: ", Some(last_char()))
        }
        EscapeError::NonAsciiCharInByte => match mode {
            Mode::Byte => ("non-ASCII character in byte literal", None),
            Mode::ByteStr => ("non-ASCII character in byte string literal", None),
            Mode::RawByteStr => ("non-ASCII character in raw byte string literal", None),
            _ => panic!("non-is_byte literal paired with NonAsciiCharInByte"),
        },
        EscapeError::OutOfRangeHexEscape => ("out of range hex escape", None),
        EscapeError::LeadingUnderscoreUnicodeEscape => ("invalid start of unicode escape: `_`", None),
        EscapeError::OverlongUnicodeEscape => ("overlong unicode escape", None),
        EscapeError::UnclosedUnicodeEscape => ("unterminated unicode escape", None),
        EscapeError::NoBraceInUnicodeEscape => ("incorrect unicode escape sequence", None),
        EscapeError::UnicodeEscapeInByte => ("unicode escape in byte string", None),
        EscapeError::EmptyUnicodeEscape => ("empty unicode escape", None),
        EscapeError::ZeroChars => ("empty character literal", None),
        EscapeError::LoneSlash => ("invalid trailing slash in literal", None),
        EscapeError::NulInCStr => ("null characters in C string literals are not supported", None),
        EscapeError::UnskippedWhitespaceWarning | EscapeError::MultipleSkippedLinesWarning => {
            return None;
        }
    };
    // alloc: error path, once per error
    let mut message = String::from(head);
    if let Some(c) = shown {
        message.push('`');
        escaped_char(c, &mut message);
        message.push('`');
    }
    Some(message)
}

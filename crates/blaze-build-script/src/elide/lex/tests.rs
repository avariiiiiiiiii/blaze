//! The lexical forms as rustc reads them (`rustc_lexer`'s tests and the Reference's chapter on
//! tokens), and what this lexer refuses.

use super::*;

fn kinds(source: &str) -> Vec<Kind> {
    lex(source).unwrap().into_iter().map(|it| it.kind).collect()
}

fn texts(source: &str) -> Vec<&str> {
    lex(source).unwrap().into_iter().map(|it| it.text).collect()
}

fn string(value: &str) -> Kind<'_> {
    Kind::Str { value: value.into(), escaped_brace: false }
}

fn raw(value: &str) -> Kind<'_> {
    Kind::RawStr { value }
}

#[test]
fn identifiers_punctuation_and_whitespace() {
    assert_eq!(
        texts("fn main ( ) {\t\x0b\x0c\n r#type _x::y!; }"),
        ["fn", "main", "(", ")", "{", "r#type", "_x", ":", ":", "y", "!", ";", "}"]
    );
    let lexed = lex("fn r#main 'a").unwrap();
    assert_eq!(lexed[0].kind, Kind::Ident);
    assert_eq!(lexed[1].kind, Kind::RawIdent);
    assert_eq!(lexed[2].kind, Kind::Lifetime);
    assert!(lexed[0].is_ident("fn") && !lexed[1].is_ident("main"));
}

#[test]
fn comments_and_doc_comments() {
    assert_eq!(kinds("// plain\n//// plain too\n/// outer\n//! inner\n"), [
        Kind::OuterDoc,
        Kind::InnerDoc
    ]);
    assert_eq!(kinds("///"), [Kind::OuterDoc], "an empty doc comment is one");
    assert_eq!(kinds("/**/ /***/ /* a */ /** outer */ /*! inner */"), [
        Kind::OuterDoc,
        Kind::InnerDoc
    ]);
    assert_eq!(kinds("/* a /* nested */ still a comment */ x"), [Kind::Ident]);
    assert!(kinds("/* a /* b /* c */ */ */").is_empty());
    assert_eq!(kinds("/*/ still open */ x"), [Kind::Ident]);
    assert_eq!(kinds("// \"not a string\n'a'"), [Kind::Char('a')]);
    assert!(lex("/* a /* b */").is_err(), "unterminated: one level left open");
    assert!(lex("/*").is_err());
}

#[test]
fn strings_and_every_escape() {
    assert_eq!(kinds(r#""plain""#), [string("plain")]);
    assert_eq!(
        kinds(r#""\n\r\t\\\0\'\"""#),
        [string("\n\r\t\\\0'\"")]
    );
    assert_eq!(kinds(r#""\x41\x7f\u{42}\u{1F600}\u{10ffff}""#), [string(
        "A\x7fB\u{1F600}\u{10ffff}"
    )]);
    assert_eq!(kinds("\"a\\\n   \t\n  b\""), [string("ab")], "a line continuation skips");
    assert_eq!(kinds("\"a\\\n\u{a0}b\""), [string("a\u{a0}b")], "but not non-ASCII whitespace");
    assert_eq!(kinds("\"two\nlines\""), [string("two\nlines")]);
    assert_eq!(kinds("\"é ünïcode {}\""), [string("é ünïcode {}")]);
    assert_eq!(kinds(r#""\u{7b}\x7d""#), [Kind::Str {
        value: "{}".into(),
        escaped_brace: true
    }]);
    for refused in [
        r#""\x80""#,
        r#""\xg0""#,
        r#""\u{}""#,
        r#""\u{1234567}""#,
        r#""\u{d800}""#,
        r#""\u{110000}""#,
        r#""\u{1_0}""#,
        r#""\u41""#,
        r#""\q""#,
        r#""unterminated"#,
        r#""suffixed"x"#,
        r#""suffixed"_"#,
    ] {
        assert!(lex(refused).is_err(), "{refused}");
    }
}

#[test]
fn raw_strings() {
    assert_eq!(kinds(r#"r"a\n{}""#), [raw(r"a\n{}")]);
    assert_eq!(kinds(r###"r#"a "quoted" b"#"###), [raw(r#"a "quoted" b"#)]);
    assert_eq!(kinds(r###"r##"a "# b"##"###), [raw(r##"a "# b"##)]);
    assert_eq!(kinds(r###"r#"a"##"###), [raw("a"), Kind::Punct('#')]);
    assert!(lex(r###"r#"unterminated"###).is_err());
    assert!(lex(r##"r#"a""##).is_err(), "one hash short");
    assert!(lex(r#"r"a"x"#).is_err(), "suffixed");
    assert!(lex("r#1").is_err());
    assert!(lex(&format!("r{}\"a\"{}", "#".repeat(256), "#".repeat(256))).is_err());
    assert_eq!(kinds(&format!("r{0}\"a\"{0}", "#".repeat(255))), [raw("a")]);
}

#[test]
fn characters_and_lifetimes() {
    assert_eq!(kinds(r"'a' '\n' '\'' '\x41' '\u{e9}' 'é' '\\' '{'"), [
        Kind::Char('a'),
        Kind::Char('\n'),
        Kind::Char('\''),
        Kind::Char('A'),
        Kind::Char('é'),
        Kind::Char('é'),
        Kind::Char('\\'),
        Kind::Char('{'),
    ]);
    assert_eq!(kinds("'a 'static"), [Kind::Lifetime, Kind::Lifetime]);
    for refused in ["'\t'", "'\n'", "'''", "'ab'", "'\\\n'", "'a'b", "''", "'é"] {
        assert!(lex(refused).is_err(), "{refused:?}");
    }
}

#[test]
fn numbers() {
    assert_eq!(kinds("0 42 1_000 007 1_"), [
        Kind::Int(0),
        Kind::Int(42),
        Kind::Int(1000),
        Kind::Int(7),
        Kind::Int(1)
    ]);
    assert_eq!(kinds("1u8 0x1f 0o7 0b1 1e5 1_i32"), vec![Kind::Number; 6]);
    assert_eq!(kinds("1.5"), [Kind::Int(1), Kind::Punct('.'), Kind::Int(5)]);
    assert_eq!(kinds(&"9".repeat(40)), [Kind::Number], "more than a u128 holds");
}

#[test]
fn prefixes_and_what_is_refused() {
    for refused in [
        "b\"bytes\"",
        "br\"raw bytes\"",
        "c\"c string\"",
        "cr#\"raw c\"#",
        "b'x'",
        "foo\"reserved\"",
        "foo#",
        "é",
        "fn mäin",
        "`",
        "\\",
        "a\rb",
        "\"\u{202e}\"",
        "// \u{2066}",
    ] {
        assert!(lex(refused).is_err(), "{refused:?}");
    }
    assert_eq!(kinds("b c r"), vec![Kind::Ident; 3], "the prefixes alone are identifiers");
    assert_eq!(lex("x é").unwrap_err().at, 2);
}

/// The byte search for text direction code points against the characters rustc's lints name,
/// over every character, alone and after others.
#[test]
fn text_direction_is_found_as_by_its_characters() {
    let named = |c: char| matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}');
    let mut source = String::new();
    for c in (0..=u32::from(char::MAX)).filter_map(char::from_u32) {
        source.clear();
        source.push_str("a\u{2020}é");
        source.push(c);
        let expected = named(c).then_some("a\u{2020}é".len());
        assert_eq!(text_direction(&source), expected, "{c:?}");
    }
}

/// A string's value borrows the source unless an escape made it differ, and is the same
/// value either way.
#[test]
fn a_string_borrows_its_value_until_an_escape() {
    let lexed = lex(r#""plain {} é" "a\nb" "x\\y" r"raw""#).unwrap();
    let values: Vec<(&str, bool)> = lexed
        .iter()
        .map(|it| match &it.kind {
            Kind::Str { value, .. } => (&**value, matches!(value, Cow::Borrowed(_))),
            Kind::RawStr { value } => (*value, true),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(values, [("plain {} é", true), ("a\nb", false), ("x\\y", false), ("raw", true)]);
}

/// The value of a decimal integer read digit by digit against `str::parse` of its digits
/// without `_`s, over seeded runs of digits and `_`s, short and past what a `u128` holds.
#[test]
fn an_integer_s_value_is_what_parse_gives() {
    let mut state = 0xd1b5_4a32_d192_ed03u64;
    for _ in 0..20_000 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let len = 1 + (state % 45) as usize;
        let mut text = String::from(char::from(b'0' + (state >> 6) as u8 % 10));
        for at in 1..len {
            let pick = (state.rotate_left(at as u32 * 5) % 12) as u8;
            text.push(if pick >= 10 { '_' } else { char::from(b'0' + pick) });
        }
        let digits: String = text.chars().filter(|c| *c != '_').collect();
        let expected = match digits.parse::<u128>() {
            Ok(value) => Kind::Int(value),
            Err(_) => Kind::Number,
        };
        assert_eq!(kinds(&text), [expected], "{text}");
    }
}

#[test]
fn normalization() {
    assert_eq!(normalize("\u{feff}fn"), "fn");
    assert_eq!(normalize("a\r\nb\r\n"), "a\nb\n");
    assert_eq!(normalize("a\rb"), "a\rb", "a lone carriage return stays, and lex refuses it");
    assert_eq!(normalize("x\u{feff}"), "x\u{feff}", "only a leading mark goes");
    assert!(matches!(normalize("plain"), Cow::Borrowed(_)));
}

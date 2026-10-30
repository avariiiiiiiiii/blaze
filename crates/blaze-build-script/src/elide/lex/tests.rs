//! The lexical forms as rustc reads them (`rustc_lexer`'s tests and the Reference's chapter on
//! tokens), and what this lexer refuses.

use super::*;

fn kinds(source: &str) -> Vec<Kind> {
    lex(source).unwrap().into_iter().map(|it| it.kind).collect()
}

fn texts(source: &str) -> Vec<&str> {
    lex(source).unwrap().into_iter().map(|it| it.text).collect()
}

fn string(value: &str) -> Kind {
    Kind::Str { value: value.to_owned(), escaped_brace: false }
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
        value: "{}".to_owned(),
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
    let raw = |value: &str| Kind::RawStr { value: value.to_owned() };
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

#[test]
fn normalization() {
    assert_eq!(normalize("\u{feff}fn"), "fn");
    assert_eq!(normalize("a\r\nb\r\n"), "a\nb\n");
    assert_eq!(normalize("a\rb"), "a\rb", "a lone carriage return stays, and lex refuses it");
    assert_eq!(normalize("x\u{feff}"), "x\u{feff}", "only a leading mark goes");
    assert!(matches!(normalize("plain"), Cow::Borrowed(_)));
}

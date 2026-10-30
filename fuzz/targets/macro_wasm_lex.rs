//! Feeds `blaze_macro_wasm::tokens`, the host's lexer of the text a proc-macro module asks to
//! be tokens of (`str::parse::<TokenStream>`, the host's `ts_from_str`), with the input as
//! text (not UTF-8 is made UTF-8 by replacement, as the host's `read_text` does).
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - lexing never panics, whatever the text: a text that is no tokens is an `Err`, as
//!   `TokenStream::from_str` is a `LexError`;
//! - it asks for at most 1 MiB plus 256 bytes for every byte of the text at once: a token is
//!   at least a byte and its tree under a hundred bytes, in vectors that double;
//! - the tokens are at most 2 trees for every byte of the text, and the one top tree: a doc
//!   comment (`//!`, 3 bytes) is the most, 6 trees.

#![forbid(unsafe_code)]

use std::time::Duration;

use blaze_fuzzer::Target;
use span::{
    Edition, EditionedFileId, FileId, ROOT_ERASED_FILE_AST_ID, Span, SpanAnchor, SyntaxContext,
    TextRange, TextSize,
};

const TARGET: Target = Target {
    name: "macro_wasm_lex",
    check,
    seeds,
    dictionary: &[
        b"r\"", b"r#", b"br\"", b"cr#", b"b'", b"c\"", b"r#ident", b"'r#a", b"'a", b"#\"", b"##",
        b"///", b"//!", b"/**", b"/*!", b"*/", b"1u8", b"0x1f", b"1e10", b"1.5f32", b"\\u{", b"(",
        b")", b"[", b"]", b"{", b"}",
    ],
    max_len: 1 << 16,
    budget: Duration::from_secs(300),
};

fn call_site() -> Span {
    let file_id = EditionedFileId::new(FileId::from_raw(0), Edition::CURRENT);
    Span {
        range: TextRange::empty(TextSize::new(0)),
        anchor: SpanAnchor { file_id, ast_id: ROOT_ERASED_FILE_AST_ID },
        ctx: SyntaxContext::root(Edition::CURRENT),
    }
}

fn check(data: &[u8]) {
    let text = String::from_utf8_lossy(data);
    let span = call_site();
    let (largest, lexed) = blaze_fuzz::largest_allocation(|| blaze_macro_wasm::tokens(&text, span));
    let bound = (1 << 20) + 256 * text.len();
    assert!(
        largest <= bound,
        "lexing {} bytes asked for {largest} bytes at once, past {bound}",
        text.len()
    );
    if let Ok(top) = lexed {
        let trees = top.token_trees().len();
        assert!(
            trees <= 2 * text.len() + 1,
            "{} bytes of text lexed to {trees} token trees, more than they can be",
            text.len()
        );
    }
}

/// Texts of every kind of token, as macros give `parse` them.
fn seeds() -> Vec<Vec<u8>> {
    let texts: &[&str] = &[
        "",
        "fn f<'a>(x: &'a u8) -> r#u8 { x.0 + -1i32 } /// doc",
        "#[derive(Debug)] struct S<T> { a: [u8; 3], b: (T, \"s\", b'x', r#\"raw\"#, 2.5e3) }",
        "//! inner\n/** outer */ /*! inner block */ /* plain */ // plain\nx",
        "'r#raw 'static r#match c\"c\" cr#\"craw\"# br\"braw\" b\"bytes\" '\\u{1F600}' '\\n'",
        "0x1f_u8 0b101 0o17 1_000i64 1e-3 2.5E+10f64 1.0 1. 7usize",
        "a => b; c::d<'_>, e ..= f ... g -> h #!x $y @z ~w ?v %u ^t |s &r *q /p",
        "#\"guarded\"# ##\"twice\"##",
        "( [ { } ] )",
        "(]",
        "a ` b",
        "'1a",
        "\"unterminated",
        "b'",
        "/* unterminated",
    ];
    let mut seeds: Vec<Vec<u8>> = texts.iter().map(|it| it.as_bytes().to_vec()).collect();
    seeds.push("( ".repeat(2000).into_bytes());
    seeds.push("x + 1 ".repeat(2000).into_bytes());
    seeds
}

#[cfg_attr(test, allow(dead_code))]
fn main() -> std::process::ExitCode {
    blaze_fuzzer::main(&TARGET)
}

#[cfg(test)]
mod tests {
    #[test]
    fn seeds() {
        blaze_fuzzer::seeds(&super::TARGET);
    }
}

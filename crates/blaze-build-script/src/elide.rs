//! Build scripts that provably only print constant cargo directives, read rather than
//! compiled and run (#1296): what such a script prints is known from its source, on every
//! target, cfg and machine. The forms read are Relax's (`front/src/unitgraph_noop.rs` of
//! makepad/makepad at d5bed7bba) and a few more, each proved by the rules rustc applies:
//!
//! - the root file is doc comments and one `fn main() { … }`, nothing else: no attribute but
//!   doc comments (`//!` first, then `///`), no `use`, `extern`, `mod`, other item, return type,
//!   `pub`, `async`, `unsafe` or `const`, so nothing replaces `main`, shadows `println!` or
//!   reads another file;
//! - the body is `println!(…)` and `print!(…)` statements alone (any delimiters), each a
//!   string literal or raw string as the format string with literal arguments: strings,
//!   characters, decimal integers no bigger than `i32::MAX` with no suffix, `true`, `false`;
//!   placeholders `{}` and `{N}` only, every argument used, no escape that makes a brace in the
//!   format string;
//! - every line printed is a cargo directive (`cargo:…`) and the last ends in `\n`.
//!
//! Anything else is `None`, and the script is compiled and run as any other. The tokens are
//! this crate's own ([`lex`]), so elision depends on no front end. They are read one at a
//! time as the forms ask for them, so most scripts (which start with `use`) are refused at
//! their first token, and nothing is allocated per token: one buffer holds a macro call's
//! arguments and one what is printed, each reused.

pub mod lex;

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use self::lex::{Kind, Token, Tokens};
use crate::Error;

/// What the script whose root file is `root` prints, when its source proves it: the exact
/// bytes of its standard output, which then go through [`crate::output::parse`] (or
/// [`crate::run::elided`] and [`crate::run::Ran::finish`]) as a run's do. `files` are its
/// source files by path: the root and the module files it declares. A script elision accepts
/// declares none and reads no other file, so only the root is read; `None` when it is not in
/// `files`.
///
/// The verdict is the source's alone: the flags the script itself is compiled with must be
/// ones [`flags_allow`] accepts.
pub fn elide(root: &Path, files: &BTreeMap<PathBuf, Vec<u8>>) -> Option<Vec<u8>> {
    let source = std::str::from_utf8(files.get(root)?).ok()?;
    let source = lex::normalize(source);
    let printed = printed(Tokens::new(&source).ok()?)?;
    directives_only(&printed).then(|| printed.into_bytes())
}

/// Whether `flags`, the flags blaze compiles a build script itself with (not its unit's
/// `rustflags`, which a script is only told), leave what [`elide`] proves true. Flags can make
/// a warning an error (`-D warnings`, `-D missing-docs`), add crate attributes
/// (`-Z crate-attr`), take std away or change linking, any of which can fail the compile of a
/// script elision would read; only optimization and debug info levels and plain `--cfg`s,
/// which the forms elision reads don't see, are allowed.
pub fn flags_allow(flags: &[String]) -> bool {
    let mut flags = flags.iter().map(String::as_str);
    while let Some(flag) = flags.next() {
        let allowed = match flag {
            "-C" => flags.next().is_some_and(inert_codegen),
            "--cfg" => flags.next().is_some_and(plain_cfg),
            _ => match (flag.strip_prefix("-C"), flag.strip_prefix("--cfg=")) {
                (Some(option), _) => inert_codegen(option),
                (_, Some(cfg)) => plain_cfg(cfg),
                _ => false,
            },
        };
        if !allowed {
            return false;
        }
    }
    true
}

fn inert_codegen(option: &str) -> bool {
    match option.split_once('=') {
        Some(("opt-level", level)) => matches!(level, "0" | "1" | "2" | "3" | "s" | "z"),
        Some(("debuginfo", level)) => matches!(
            level,
            "0" | "1"
                | "2"
                | "none"
                | "line-directives-only"
                | "line-tables-only"
                | "limited"
                | "full"
        ),
        _ => false,
    }
}

/// `name` or `name="value"`, `name` an ASCII identifier and the value with no quote or
/// backslash.
fn plain_cfg(cfg: &str) -> bool {
    let (name, value) = cfg.split_once('=').unwrap_or((cfg, "\"\""));
    let name_ok = name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    let value = value.strip_prefix('"').and_then(|it| it.strip_suffix('"'));
    name_ok && value.is_some_and(|it| !it.contains(['"', '\\']))
}

/// Whether an elided script's output and a run's agree, for `BLAZE_ELIDE=verify`, which runs
/// both: if not, an error naming the first line that differs.
pub fn verify(elided: &[u8], ran: &[u8]) -> Result<(), Error> {
    if elided == ran {
        return Ok(());
    }
    let mut elided_lines = elided.split_inclusive(|b| *b == b'\n');
    let mut ran_lines = ran.split_inclusive(|b| *b == b'\n');
    let mut line = 1;
    loop {
        let (said, printed) = (elided_lines.next(), ran_lines.next());
        if said != printed {
            let shown = |it: Option<&[u8]>| match it {
                Some(it) => format!("{:?}", String::from_utf8_lossy(it)),
                None => "nothing".to_owned(),
            };
            return Err(Error(format!(
                "the elided build script's output differs from its run's at line {line}: \
                 elision says {}, the run printed {}",
                shown(said),
                shown(printed)
            )));
        }
        line += 1;
    }
}

/// Every line a cargo directive, the last ended.
fn directives_only(printed: &str) -> bool {
    let Some(lines) = printed.strip_suffix('\n') else { return printed.is_empty() };
    lines.split('\n').all(|line| line.starts_with("cargo:"))
}

/// The next token, or none at the end or at a refusal: either leaves the script unread.
fn next<'a>(tokens: &mut Tokens<'a>) -> Option<Token<'a>> {
    tokens.next()?.ok()
}

/// What the script of `tokens` prints, if it is of the forms read.
fn printed(mut tokens: Tokens<'_>) -> Option<String> {
    let mut token = next(&mut tokens)?;
    while token.kind == Kind::InnerDoc {
        token = next(&mut tokens)?;
    }
    while token.kind == Kind::OuterDoc {
        token = next(&mut tokens)?;
    }
    let head_ok = token.is_ident("fn")
        && next(&mut tokens)?.is_ident("main")
        && next(&mut tokens)?.is_punct('(')
        && next(&mut tokens)?.is_punct(')')
        && next(&mut tokens)?.is_punct('{');
    if !head_ok {
        return None;
    }
    let mut out = String::new();
    let mut args: Vec<Token<'_>> = Vec::new();
    let mut scratch = Scratch::default();
    let mut token = next(&mut tokens)?;
    while !token.is_punct('}') {
        let newline = match &token {
            it if it.is_ident("println") => true,
            it if it.is_ident("print") => false,
            _ => return None,
        };
        if !next(&mut tokens)?.is_punct('!') {
            return None;
        }
        let (open, close) = match next(&mut tokens)?.kind {
            Kind::Punct('(') => ('(', ')'),
            Kind::Punct('[') => ('[', ']'),
            Kind::Punct('{') => ('{', '}'),
            _ => return None,
        };
        // Arguments are literals and commas, so the first delimiter after the opening one is
        // its close, or the script is of no form read.
        args.clear();
        let end = loop {
            let token = next(&mut tokens)?;
            if matches!(token.kind, Kind::Punct('(' | ')' | '[' | ']' | '{' | '}')) {
                break token;
            }
            args.push(token);
        };
        if !end.is_punct(close) {
            return None;
        }
        format(&args, newline, &mut out, &mut scratch)?;
        token = next(&mut tokens)?;
        // A brace-delimited macro is a statement of its own; a `;` after it would be an empty
        // statement (`redundant_semicolons`). Another needs its `;` unless it ends the block.
        if open != '{' {
            if token.is_punct(';') {
                token = next(&mut tokens)?;
            } else if !token.is_punct('}') {
                return None;
            }
        }
    }
    tokens.next().is_none().then_some(out)
}

/// What [`format`] reuses from one macro call to the next.
#[derive(Default)]
struct Scratch {
    /// A format string's value, when an escape made it differ from its text.
    spec: String,
    /// Which arguments a placeholder used.
    used: Vec<bool>,
}

/// What `format_args!` (with `println!`'s newline) makes of `args`, added to `out`. The
/// format string is copied in runs between its braces, each argument written where the
/// source has it, with nothing allocated but what `out` and `scratch` grow by.
fn format(
    args: &[Token<'_>],
    newline: bool,
    out: &mut String,
    scratch: &mut Scratch,
) -> Option<()> {
    let (spec, rest) = args.split_first()?;
    let spec: &str = match spec.kind {
        Kind::Str { escaped: false, escaped_brace: false } => &spec.text[1..spec.text.len() - 1],
        Kind::Str { escaped: true, escaped_brace: false } => {
            scratch.spec.clear();
            spec.push_value(&mut scratch.spec)?;
            &scratch.spec
        }
        Kind::RawStr { value } => value,
        _ => return None,
    };
    // `, value` after the format string, a last `,` allowed: the values are every second
    // token, from the second.
    if rest.iter().step_by(2).any(|it| !it.is_punct(',')) {
        return None;
    }
    if !rest.iter().skip(1).step_by(2).all(displayable) {
        return None;
    }
    let count = rest.len() / 2;
    let used = &mut scratch.used;
    used.clear();
    used.resize(count, false);
    let mut next = 0;
    // Braces are ASCII, so every index below is on a character's boundary.
    let bytes = spec.as_bytes();
    let (mut at, mut copied) = (0, 0);
    while at < bytes.len() {
        match bytes[at] {
            brace @ (b'{' | b'}') if bytes.get(at + 1) == Some(&brace) => {
                out.push_str(&spec[copied..=at]);
                at += 2;
                copied = at;
            }
            b'{' => {
                out.push_str(&spec[copied..at]);
                let close = at + 1 + memchr::memchr(b'}', &bytes[at + 1..])?;
                let argument = &spec[at + 1..close];
                let index = if argument.is_empty() {
                    next += 1;
                    next - 1
                } else if argument.bytes().all(|b| b.is_ascii_digit())
                    && (argument == "0" || !argument.starts_with('0'))
                {
                    argument.parse().ok()?
                } else {
                    return None;
                };
                if index >= count {
                    return None;
                }
                write_arg(&rest[1 + 2 * index], out)?;
                used[index] = true;
                at = close + 1;
                copied = at;
            }
            b'}' => return None,
            _ => at += 1,
        }
    }
    out.push_str(&spec[copied..]);
    if used.contains(&false) {
        return None;
    }
    if newline {
        out.push('\n');
    }
    Some(())
}

/// Whether `token` is a literal [`write_arg`] writes.
fn displayable(token: &Token<'_>) -> bool {
    match token.kind {
        Kind::Str { .. } | Kind::RawStr { .. } | Kind::Char(_) => true,
        Kind::Int(value) => value <= i32::MAX as u128,
        Kind::Ident => token.text == "true" || token.text == "false",
        _ => false,
    }
}

/// A literal as `Display` shows it, added to `out`: a string's value, a character, a number
/// in decimal, `true` or `false`.
fn write_arg(token: &Token<'_>, out: &mut String) -> Option<()> {
    match token.kind {
        Kind::Str { .. } | Kind::RawStr { .. } => token.push_value(out),
        Kind::Char(c) => {
            out.push(c);
            Some(())
        }
        Kind::Int(value) if value <= i32::MAX as u128 => {
            let mut digits = [0u8; 10];
            let (mut value, mut at) = (value as u32, digits.len());
            loop {
                at -= 1;
                digits[at] = b'0' + (value % 10) as u8;
                value /= 10;
                if value == 0 {
                    break;
                }
            }
            out.extend(digits[at..].iter().map(|it| char::from(*it)));
            Some(())
        }
        Kind::Ident if token.text == "true" || token.text == "false" => {
            out.push_str(token.text);
            Some(())
        }
        _ => None,
    }
}

#[cfg(test)]
mod reference;
#[cfg(test)]
mod tests;

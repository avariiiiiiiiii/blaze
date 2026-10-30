//! GNU linker scripts given as shared libraries, as glibc ships `libc.so`:
//! `GROUP ( /lib/x86_64-linux-gnu/libc.so.6 /usr/lib/x86_64-linux-gnu/libc_nonshared.a
//! AS_NEEDED ( /lib64/ld-linux-x86-64.so.2 ) )`. Only the commands that name inputs are read.

use crate::model::{Names, Shared};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Token<'a> {
    Word(&'a str),
    Open,
    Close,
    Comma,
    Semicolon,
}

pub(super) fn read(name: &str, bytes: &[u8]) -> Result<Shared, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| {
        format!("{name}: neither an ELF file, an archive nor a linker script (not UTF-8 text)")
    })?;
    let tokens = tokens(name, text)?;
    let mut inputs = Vec::new();
    let mut at = 0;
    while let Some(&token) = tokens.get(at) {
        at += 1;
        let command = match token {
            Token::Semicolon => continue,
            Token::Word(command) => command,
            other => {
                return Err(format!("{name}: linker script: expected a command, found {other:?}"));
            }
        };
        if tokens.get(at) != Some(&Token::Open) {
            return Err(format!("{name}: linker script: {command} without its arguments"));
        }
        at += 1;
        match command {
            "GROUP" | "INPUT" => at = list(name, &tokens, at, false, &mut inputs)?,
            "OUTPUT_FORMAT" | "OUTPUT_ARCH" => {
                let mut depth = 1;
                while depth > 0 {
                    match tokens.get(at) {
                        Some(Token::Open) => depth += 1,
                        Some(Token::Close) => depth -= 1,
                        Some(_) => {}
                        None => {
                            return Err(format!("{name}: linker script: {command}( is not closed"));
                        }
                    }
                    at += 1;
                }
            }
            _ => {
                return Err(format!(
                    "{name}: linker script command {command} is not supported: blaze reads only \
                     GROUP, INPUT, AS_NEEDED, OUTPUT_FORMAT and OUTPUT_ARCH"
                ));
            }
        }
    }
    if inputs.is_empty() {
        return Err(format!("{name}: a linker script that names no inputs"));
    }
    Ok(Shared {
        name: super::file_name(name),
        symbols: Vec::new(),
        names: Names::default(),
        versions: Vec::new(),
        more: Vec::new(),
        inputs,
    })
}

/// Reads the file names of a `GROUP`, `INPUT` or `AS_NEEDED` from `at`, just past its `(`, into
/// `inputs`; returns where its `)` ends.
fn list(
    name: &str,
    tokens: &[Token<'_>],
    mut at: usize,
    as_needed: bool,
    inputs: &mut Vec<String>,
) -> Result<usize, String> {
    loop {
        let Some(&token) = tokens.get(at) else {
            return Err(format!("{name}: linker script: a file list is not closed"));
        };
        at += 1;
        match token {
            Token::Close => return Ok(at),
            Token::Comma => {}
            Token::Word("AS_NEEDED") if tokens.get(at) == Some(&Token::Open) => {
                if as_needed {
                    return Err(format!("{name}: linker script: AS_NEEDED inside AS_NEEDED"));
                }
                at = list(name, tokens, at + 1, true, inputs)?;
            }
            Token::Word(library) if library.starts_with("-l") => {
                return Err(format!(
                    "{name}: linker script names {library}: blaze does not search library \
                     directories from a linker script; name the file's path"
                ));
            }
            Token::Word(file) => inputs.push(file.to_owned()),
            other => {
                return Err(format!(
                    "{name}: linker script: expected a file name, found {other:?}"
                ));
            }
        }
    }
}

fn tokens<'a>(name: &str, text: &'a str) -> Result<Vec<Token<'a>>, String> {
    let mut tokens = Vec::new();
    let mut rest = text;
    loop {
        rest = rest.trim_start();
        let Some(first) = rest.chars().next() else {
            return Ok(tokens);
        };
        if let Some(comment) = rest.strip_prefix("/*") {
            let end = comment
                .find("*/")
                .ok_or_else(|| format!("{name}: linker script: a comment is not closed"))?;
            rest = &comment[end + 2..];
            continue;
        }
        let token = match first {
            '(' => Token::Open,
            ')' => Token::Close,
            ',' => Token::Comma,
            ';' => Token::Semicolon,
            '"' => {
                let quoted = &rest[1..];
                let end = quoted
                    .find('"')
                    .ok_or_else(|| format!("{name}: linker script: a quoted name is not closed"))?;
                tokens.push(Token::Word(&quoted[..end]));
                rest = &quoted[end + 1..];
                continue;
            }
            _ => {
                let end = rest
                    .find(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | ',' | ';' | '"'))
                    .unwrap_or(rest.len());
                let end = rest[..end].find("/*").unwrap_or(end);
                tokens.push(Token::Word(&rest[..end]));
                rest = &rest[end..];
                continue;
            }
        };
        tokens.push(token);
        rest = &rest[1..];
    }
}

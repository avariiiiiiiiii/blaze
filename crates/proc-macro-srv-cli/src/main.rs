//! `blaze-proc-macro-srv`, blaze's proc-macro server ([`proc_macro_srv_cli`]): `--version`, or
//! `--format <json-legacy|bidirectional-postcard-prototype>` (JSON by default) to serve on stdin
//! and stdout, or on the socket `BLAZE_WARM_SOCKET` names when blaze's pool started it warm.
#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::{
    ffi::{OsStr, OsString},
    io,
};

use blaze_usercode::pool::SOCKET;
use proc_macro_api::ProtocolFormat;
use proc_macro_srv_cli::main_loop;

fn main() -> io::Result<()> {
    if std::env::var_os("RUST_ANALYZER_INTERNALS_DO_NOT_USE").is_none() {
        eprintln!(
            "This is an IDE implementation detail, you can use this tool by exporting RUST_ANALYZER_INTERNALS_DO_NOT_USE."
        );
        eprintln!(
            "Note that this tool's API is highly unstable and may break without prior notice"
        );
        std::process::exit(122);
    }
    let format = match asked(std::env::args_os().skip(1)) {
        Ok(Asked::Version) => {
            println!("blaze-proc-macro-srv {}", version());
            return Ok(());
        }
        Ok(Asked::Serve(format)) => format,
        Err(why) => {
            eprintln!("blaze-proc-macro-srv: {why}");
            std::process::exit(2);
        }
    };
    match std::env::var_os(SOCKET) {
        Some(socket) => warm(&socket, format),
        None => {
            let mut stdin = io::BufReader::new(io::stdin());
            let mut stdout = io::stdout();
            main_loop::run(&mut stdin, &mut stdout, format)
        }
    }
}

/// The toolchain's version, whose `proc_macro` the server is built over: its `rustc --version`
/// without the `rustc `.
fn version() -> &'static str {
    let rustc = proc_macro_srv::RUSTC_VERSION_STRING;
    rustc.strip_prefix("rustc ").unwrap_or(rustc)
}

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
enum Asked {
    Version,
    Serve(ProtocolFormat),
}

fn asked(mut args: impl Iterator<Item = OsString>) -> Result<Asked, String> {
    let mut format = ProtocolFormat::JsonLegacy;
    while let Some(arg) = args.next() {
        let Some(text) = arg.to_str() else {
            // alloc: an error at start, once.
            return Err(format!("an argument that is not UTF-8: {arg:?}"));
        };
        match text {
            "--version" => return Ok(Asked::Version),
            "--format" => {
                let Some(value) = args.next() else {
                    return Err("`--format` needs a value".into());
                };
                format = format_named(&value)?;
            }
            _ => match text.strip_prefix("--format=") {
                Some(value) => format = format_named(value.as_ref())?,
                // alloc: an error at start, once.
                None => return Err(format!("unknown argument `{text}`")),
            },
        }
    }
    Ok(Asked::Serve(format))
}

fn format_named(name: &OsStr) -> Result<ProtocolFormat, String> {
    match name.to_str() {
        Some("json-legacy") => Ok(ProtocolFormat::JsonLegacy),
        Some("bidirectional-postcard-prototype") => {
            Ok(ProtocolFormat::BidirectionalPostcardPrototype)
        }
        // alloc: an error at start, once.
        _ => Err(format!(
            "unknown protocol format {name:?}: json-legacy or bidirectional-postcard-prototype"
        )),
    }
}

/// Serves blaze's pool on the socket `socket`, as a warm server in its sandbox, which has no
/// pipes: in the bidirectional protocol only, as the pool starts it.
#[cfg(unix)]
fn warm(socket: &OsStr, format: ProtocolFormat) -> io::Result<()> {
    if format != ProtocolFormat::BidirectionalPostcardPrototype {
        // alloc: an error at start, once.
        return Err(io::Error::other(format!(
            "{SOCKET} is set, and a warm server speaks bidirectional-postcard-prototype, not {format}"
        )));
    }
    let stream = main_loop::connect(std::path::Path::new(socket))?;
    // SAFETY: `remove_var` needs no other thread reading or writing the environment as it
    // changes. The server has started no thread yet: an expansion's starts once its request has
    // come on the stream, after this.
    unsafe { std::env::remove_var(SOCKET) };
    main_loop::run_warm(stream)
}

#[cfg(not(unix))]
fn warm(_: &OsStr, _: ProtocolFormat) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        // alloc: an error at start, once.
        format!("{SOCKET} is set, but a warm server talks over a Unix socket: unix only"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asked_of(args: &[&str]) -> Result<Asked, String> {
        asked(args.iter().map(OsString::from))
    }

    #[test]
    fn the_command_line_asks_for_a_version_or_a_format() {
        assert_eq!(asked_of(&[]), Ok(Asked::Serve(ProtocolFormat::JsonLegacy)));
        assert_eq!(asked_of(&["--version"]), Ok(Asked::Version));
        let bidirectional = Ok(Asked::Serve(ProtocolFormat::BidirectionalPostcardPrototype));
        assert_eq!(asked_of(&["--format", "bidirectional-postcard-prototype"]), bidirectional);
        assert_eq!(asked_of(&["--format=bidirectional-postcard-prototype"]), bidirectional);
        assert!(asked_of(&["--format"]).unwrap_err().contains("needs a value"));
        assert!(asked_of(&["--format", "x"]).unwrap_err().contains("unknown protocol format"));
        assert!(asked_of(&["--help"]).unwrap_err().contains("unknown argument"));
    }

    #[test]
    fn the_version_is_the_toolchains() {
        assert!(version().contains("nightly"), "{}", version());
        assert!(!version().starts_with("rustc"));
    }
}

//! The program `blaze` (also installed as `blaze-engine`, and run by cargo as `cargo-blaze`):
//! what runs before blaze's cargo, then blaze's cargo, the client of the daemon
//! ([`blaze_cargo::cli`]), which is also the daemon (`blaze serve`).
//!
//! Before it, in this order: the sandbox launcher (`blaze sandbox ...`, which forks, so it comes
//! before any thread); blaze started as a build script's `rustc` or `rustdoc`; the
//! release of blaze the workspace pins, run in this one's place when it is not this one
//! (`pin.rs`, `versions.rs`); and the commands of the pins themselves.

use std::{ffi::OsString, path::Path, process::ExitCode};

use crate::{as_rustc, pin, versions};

/// The program `blaze`, whose `src/main.rs` and `src/bin/blaze.rs` are only this.
pub fn main() -> ExitCode {
    if std::env::args_os().nth(1).is_some_and(|it| it == blaze_cargo::sandbox::COMMAND) {
        blaze_cargo::sandbox::command(std::env::args_os().skip(2));
    }
    blaze_cargo::release::set(blaze_cargo::release::Release {
        version: pin::OURS,
        line: env!("BLAZE_VERSION_LINE"),
    });
    if as_rustc::is_me() {
        as_rustc::command();
    }
    if as_rustc::rustdoc::is_me() {
        as_rustc::rustdoc::command();
    }
    let words = blaze_cargo::cli::words(std::env::args_os());
    if let Some(ended) = pinned_elsewhere(&words) {
        return ended;
    }
    let first = words.first().and_then(|it| it.to_str());
    let rest = || words.iter().skip(1).map(|it| it.to_string_lossy().into_owned());
    let done = match first {
        Some("pin") => versions::pin(&rest().collect::<Vec<_>>()),
        Some("version-fetch") => versions::command(rest()).map(|()| 0),
        Some("version-resolve") => versions::resolve_command(rest()).map(|()| 0),
        Some("--version" | "-V") if words.len() == 1 => {
            println!("{}", blaze_cargo::version());
            println!("{}", versions::why_this_one());
            Ok(0)
        }
        _ => return blaze_cargo::cli::main(std::env::args_os()),
    };
    match done {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(error) => failed(&format!("{error:#}")),
    }
}

/// Runs the release of blaze the workspace pins with `words`, when it pins one and that is not
/// this one, fetching it first when it is not there. `None` when this one is to run. `blaze
/// pin` always runs this one: it pins this one.
fn pinned_elsewhere(words: &[OsString]) -> Option<ExitCode> {
    let text: Vec<String> = words.iter().map(|it| it.to_string_lossy().into_owned()).collect();
    if matches!(text.first().map(String::as_str), Some("pin" | "version-fetch" | "version-resolve" | "serve")) {
        return None;
    }
    let cwd = std::env::current_dir().ok()?;
    let manifest = pin::root_manifest(&pin::start(&text, &cwd))?;
    let wanted = match pin::pinned(&manifest) {
        Ok(Some(wanted)) => wanted,
        Ok(None) => return None,
        Err(why) => return Some(failed(&why)),
    };
    let shown = manifest.display();
    let wanted = if pin::is_channel(&wanted) {
        match channel_version(&wanted) {
            Ok(version) => version,
            Err(why) => {
                return Some(failed(&format!(
                    "{shown} pins blaze's {wanted} channel, and {why}; no other blaze builds in its place"
                )));
            }
        }
    } else {
        wanted
    };
    if wanted == pin::OURS {
        return None;
    }
    if std::env::var_os(pin::PINNED_BY).is_some() {
        let this = std::env::current_exe().unwrap_or_default();
        return Some(failed(&format!(
            "{shown} pins blaze {wanted}, and {}, run for it, is blaze {}: removing {} has it fetched again",
            this.display(),
            pin::OURS,
            this.parent().unwrap_or(Path::new("")).display()
        )));
    }
    let pinned = match versions::fetch(&wanted) {
        Ok(pinned) => pinned,
        Err(why) => {
            return Some(failed(&format!(
                "{shown} pins blaze {wanted}, which could not be fetched ({why:#}); no other blaze builds in its place"
            )));
        }
    };
    // A release from before pins says only its version.
    if matches!(text.as_slice(), [it] if it == "--version" || it == "-V") {
        println!("blaze {} runs blaze {wanted} here, as {shown} pins it:", pin::OURS);
    }
    let mut command = std::process::Command::new(&pinned);
    command.args(words).env(pin::PINNED_BY, &manifest);
    Some(ended(&mut command, &pinned))
}

/// The version `channel` names: the one read for it within [`pin::CHANNEL_FRESH`], else read
/// again ([`versions::resolve`], which keeps it). When it can't be read again, the one read
/// last, with a line saying so.
fn channel_version(channel: &str) -> Result<String, String> {
    let file = pin::channel_file(channel).ok_or_else(|| {
        "blaze keeps what a channel names in .blaze in your home directory, and there is none \
         (`BLAZE_HOME` names where)"
            .to_owned()
    })?;
    let known = pin::channel_known(&file);
    if let Some((version, true)) = &known {
        return Ok(version.clone());
    }
    let why = match versions::resolve(channel) {
        Ok(version) => return Ok(version),
        Err(why) => format!("{why:#}"),
    };
    match known {
        Some((version, _)) => {
            eprintln!(
                "blaze: what the {channel} channel names could not be read ({why}); blaze \
                 {version}, which it named when last read, runs"
            );
            Ok(version)
        }
        None => Err(format!("what it names could not be read: {why}")),
    }
}

fn failed(why: &str) -> ExitCode {
    eprintln!("error: {why}");
    ExitCode::FAILURE
}

/// Becomes `command`, where a process can, else runs it and ends as it ended.
fn ended(command: &mut std::process::Command, program: &Path) -> ExitCode {
    #[cfg(unix)]
    let error = std::os::unix::process::CommandExt::exec(command);
    #[cfg(not(unix))]
    let error = match command.status() {
        Ok(status) => return ExitCode::from(u8::try_from(status.code().unwrap_or(1)).unwrap_or(1)),
        Err(error) => error,
    };
    failed(&format!("could not run {}: {error}", program.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pins_own_commands_run_this_blaze() {
        for first in ["pin", "version-fetch", "version-resolve", "serve"] {
            assert!(pinned_elsewhere(&[OsString::from(first)]).is_none(), "{first}");
        }
    }
}

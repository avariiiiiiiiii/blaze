//! `blaze why [fn|crate]`: what the last build compiled again and why, from its records
//! ([`blaze_protocol::events`]), which the command that waited for it kept beside the daemon
//! ([`keep`]).

use std::{io::Write as _, path::PathBuf};

use anyhow::Context as _;
use blaze_protocol::events::{CrateState, Event, Record};

use super::daemon;

/// The file the records of the last build are kept in.
fn kept() -> anyhow::Result<PathBuf> {
    Ok(daemon::home()?.join("last-build"))
}

/// Keeps `records` as the last build's, when they are of a build that compiled, refused or
/// failed: a check compiles nothing, and a fresh build has nothing to say why of.
pub fn keep(records: &[Record]) {
    let build = records
        .iter()
        .any(|it| matches!(&it.event, Event::Began { command, .. } if command == "build"));
    // A build that did nothing leaves the last that did something to say why of.
    let did = records.iter().any(|it| match &it.event {
        Event::Crate { state, .. } => matches!(state, CrateState::Built | CrateState::Failed),
        Event::Rebuilt { .. } | Event::Unlisted { .. } | Event::Refused { .. } => true,
        Event::Finished { ok, .. } => !ok,
        _ => false,
    });
    if !build || !did {
        return;
    }
    let Ok(path) = kept() else { return };
    let mut frames = Vec::new();
    for record in records {
        let Ok(item) = blaze_protocol::cbor::to_vec(record) else { return };
        if blaze_protocol::write_frame(&mut frames, &item).is_err() {
            return;
        }
    }
    // Whole or not at all, for a `blaze why` that reads it meanwhile.
    let written = path.with_extension(std::process::id().to_string());
    if std::fs::write(&written, frames).is_err() || std::fs::rename(&written, &path).is_err() {
        let _ = std::fs::remove_file(&written);
    }
}

fn last() -> anyhow::Result<Vec<Record>> {
    let path = kept()?;
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            anyhow::bail!("no build is kept to say why of: `blaze build` or `run` first")
        }
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let mut records = Vec::new();
    let mut rest = &bytes[..];
    while let Some(item) = blaze_protocol::read_frame(&mut rest)? {
        let record = blaze_protocol::cbor::from_slice(&item)
            .with_context(|| format!("{} is not of this blaze", path.display()))?;
        records.push(record);
    }
    Ok(records)
}

/// `blaze why`, of the last build, or of the function or crate `args` names.
pub fn command(args: &[String]) -> anyhow::Result<i32> {
    let records = last()?;
    let mut build = blaze_ui::Build::default();
    for record in &records {
        build.apply(record);
    }
    let mut out = String::new();
    let took = build.finished.as_ref().map_or(build.last_ns, |it| it.summary.took_ns);
    let command = build.command.as_deref().unwrap_or("build");
    let compiled_again = |name: &str, krate: &str, cause: &blaze_protocol::events::Cause| {
        format!("  {name} ({krate}): {}\n", cause.short())
    };
    match args.first() {
        None => {
            out.push_str(&format!(
                "the last `blaze {command}` that compiled anything took {}\n",
                blaze_ui::took(took)
            ));
            if build.rebuilt.is_empty() {
                out.push_str("  compiled nothing again\n");
            }
            for (name, krate, cause) in &build.rebuilt {
                out.push_str(&compiled_again(name, krate, cause));
            }
            if build.unlisted > 0 {
                out.push_str(&format!("  and {} more\n", build.unlisted));
            }
            for krate in &build.crates {
                let fns = krate.fns;
                out.push_str(&format!(
                    "  {}: {} fns, {} compiled, {} kept\n",
                    krate.name,
                    fns.total,
                    fns.compiled(),
                    fns.kept
                ));
            }
        }
        Some(asked) => {
            let of_crate = build.crates.iter().find(|it| it.name == *asked);
            let rebuilt: Vec<_> = build
                .rebuilt
                .iter()
                .filter(|(name, krate, _)| name == asked || krate == asked)
                .collect();
            if let Some(krate) = of_crate {
                let fns = krate.fns;
                out.push_str(&format!(
                    "{asked}: {} of {} fns compiled again, {} kept\n",
                    fns.compiled(),
                    fns.total,
                    fns.kept
                ));
            }
            for (name, krate, cause) in &rebuilt {
                out.push_str(&compiled_again(name, krate, cause));
            }
            if of_crate.is_none() && rebuilt.is_empty() {
                let named = match build.unlisted {
                    0 => String::new(),
                    more => format!(" (it names {} and not {more} more)", build.rebuilt.len()),
                };
                out.push_str(&format!(
                    "the last `blaze {command}` did not compile `{asked}` again{named}\n"
                ));
            }
        }
    }
    std::io::stdout().write_all(out.as_bytes())?;
    Ok(0)
}

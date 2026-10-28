//! `blaze build|run|test|check|stop ...`, which becomes `blaze-engine cargo ...`, the `blaze` being
//! the one installed next to it. In a workspace that pins another release of blaze
//! (`[workspace.metadata.blaze] version`), it runs that release instead, fetched once when it
//! is not there (`pin.rs`).

use std::{
    ffi::OsString,
    path::Path,
    process::{Command, ExitCode},
};

#[path = "../pin.rs"]
#[allow(dead_code)]
mod pin;

fn main() -> ExitCode {
    let exe = match std::env::current_exe() {
        Ok(this) => this.with_file_name(format!("blaze-engine{}", std::env::consts::EXE_SUFFIX)),
        Err(error) => {
            eprintln!("error: blaze does not know where it is installed: {error}");
            return ExitCode::FAILURE;
        }
    };
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if let Some(ended) = pinned_elsewhere(&exe, &args) {
        return ended;
    }
    match Command::new(&exe).arg("cargo").args(&args).status() {
        Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
        Err(error) => {
            eprintln!(
                "error: blaze could not run {}, which is installed with it ({error}); \
                 installing blaze again puts it back",
                exe.display()
            );
            ExitCode::FAILURE
        }
    }
}

/// Runs the release of blaze the workspace pins with `args`, when it pins one and that is not
/// this one, fetching it first with `engine` when it is not there. `None` when this one is
/// to run. `blaze pin` always runs this one: it pins this one.
fn pinned_elsewhere(engine: &Path, args: &[OsString]) -> Option<ExitCode> {
    let text: Vec<String> = args.iter().map(|it| it.to_string_lossy().into_owned()).collect();
    if text.first().is_some_and(|it| it == "pin") {
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
        match channel_version(engine, &wanted) {
            Ok(version) => version,
            Err(why) => {
                return Some(failed(&format!(
                    "{shown} pins blaze's {wanted} channel, and {why}; no other blaze builds in \
                     its place"
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
            "{shown} pins blaze {wanted}, and {}, run for it, is blaze {}: removing {} has it \
             fetched again",
            this.display(),
            pin::OURS,
            this.parent().unwrap_or(Path::new("")).display()
        )));
    }
    let Some(pinned) = pin::installed(&wanted) else {
        return Some(failed(&format!(
            "{shown} pins blaze {wanted}, which blaze keeps in .blaze in your home directory, \
             and there is none: `BLAZE_HOME` names where"
        )));
    };
    if !pinned.is_file() {
        let fetched = Command::new(engine).arg("version-fetch").arg(&wanted).status();
        if !fetched.as_ref().is_ok_and(|it| it.success()) {
            let why = fetched.err().map_or(String::new(), |it| format!(" ({it})"));
            return Some(failed(&format!(
                "{shown} pins blaze {wanted}, which could not be fetched{why}; no other blaze \
                 builds in its place"
            )));
        }
    }
    // A release from before pins says only its version.
    if matches!(text.as_slice(), [it] if it == "--version" || it == "-V") {
        println!("blaze {} runs blaze {wanted} here, as {shown} pins it:", pin::OURS);
    }
    let mut command = Command::new(&pinned);
    command.args(args).env(pin::PINNED_BY, &manifest);
    Some(ended(&mut command, &pinned))
}

/// The version `channel` names: the one read for it within [`pin::CHANNEL_FRESH`], else read
/// again with `engine` (`version-resolve`, which keeps it). When it can't be read again, the one
/// read last, with a line saying so.
fn channel_version(engine: &Path, channel: &str) -> Result<String, String> {
    let file = pin::channel_file(channel).ok_or_else(|| {
        "blaze keeps what a channel names in .blaze in your home directory, and there is none \
         (`BLAZE_HOME` names where)"
            .to_owned()
    })?;
    let known = pin::channel_known(&file);
    if let Some((version, true)) = &known {
        return Ok(version.clone());
    }
    let asked = Command::new(engine).arg("version-resolve").arg(channel).output();
    let answer = asked.as_ref().ok().filter(|it| it.status.success());
    let answer = answer.map(|it| String::from_utf8_lossy(&it.stdout).trim().to_owned());
    if let Some(version) = answer.filter(|it| pin::is_version(it)) {
        return Ok(version);
    }
    let why = match &asked {
        Ok(out) => String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        Err(error) => format!("{} did not run: {error}", engine.display()),
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
fn ended(command: &mut Command, program: &Path) -> ExitCode {
    #[cfg(unix)]
    let error = std::os::unix::process::CommandExt::exec(command);
    #[cfg(not(unix))]
    let error = match command.status() {
        Ok(status) => return ExitCode::from(status.code().unwrap_or(1) as u8),
        Err(error) => error,
    };
    failed(&format!("could not run {}: {error}", program.display()))
}

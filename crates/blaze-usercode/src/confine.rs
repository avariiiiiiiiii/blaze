//! The sandbox of a spawn ([`blaze_core::Confine`]): what a driver starts in place of a
//! command whose [`Sandbox`] is on. That is blaze's launcher (`blaze sandbox --v2 SPEC`, in
//! `blaze-cargo/src/sandbox`), given a [`Spec`] in the run's scratch directory; it makes the
//! platform's sandbox (namespaces and seccomp on Linux, Seatbelt on macOS, an AppContainer in
//! a job object on Windows), runs the program in it, appends what it sees to the live record
//! as it sees it ([`crate::trace::Line`]), and as it ends leaves its [`crate::trace::Report`]:
//! that it could not make the sandbox, or that the program ran and, when it traces, what it
//! read, hashed as the files are then ([`crate::inputs::Disk`]).

use std::{
    borrow::Cow,
    ffi::OsString,
    path::{Path, PathBuf},
};

use blaze_core::{Command, Confine, Confined, Sandbox, Traced};
use serde::{Deserialize, Serialize, Serializer};

use crate::trace::{self, Report};

/// The variable of a [`Command`]'s environment that names, for a warm process, where its live
/// record goes: the confine takes it out of what the program sees and gives it the launcher
/// as [`Spec::live`]. A process that serves many requests is read after each answer, by
/// scope ([`crate::pool`]).
pub const LIVE: &str = "__BLAZE_SANDBOX_LIVE";

/// The subcommand and flag of blaze's launcher.
pub const LAUNCH: [&str; 2] = ["sandbox", "--v2"];

/// The spec's and the report's names in the run's scratch directory.
pub const SPEC_FILE: &str = "spec.json";
pub const REPORT_FILE: &str = "report";
/// The live record's name in the scratch directory, of a run that names none of its own.
pub const LIVE_FILE: &str = "live";

/// What the launcher is told to run, as JSON, refusing fields it does not know.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// All of its environment: the launcher's own is not passed on.
    pub env: Vec<(OsString, OsString)>,
    pub dir: PathBuf,
    /// Seen read-only, with what is under them; the rest of the machine is seen read-only
    /// too where the platform can't hide it (macOS, Windows), and not at all in a Linux view.
    pub read: Vec<PathBuf>,
    /// Written for real, with what is under them.
    pub write: Vec<PathBuf>,
    pub network: bool,
    /// Whether to record what it read; what was refused it is recorded either way.
    pub trace: bool,
    /// Where the live record goes, appended a line at a time before the call it notes goes
    /// on.
    pub live: PathBuf,
    /// Where the report goes as it ends ([`trace::encode`]), whole or not at all.
    pub report: PathBuf,
    /// Paths that are no inputs of what runs: the program itself, whose code is its key.
    pub not_inputs: Vec<PathBuf>,
}

/// A [`Spec`] lent from the command it runs: what [`Launcher::confine`] writes, the same JSON
/// as the `Spec` the launcher reads back, with nothing of the command copied.
#[derive(Serialize, Debug)]
pub struct SpecOf<'a> {
    program: &'a Path,
    args: &'a [OsString],
    env: Passed<'a>,
    dir: &'a Path,
    read: &'a [PathBuf],
    write: &'a [PathBuf],
    network: bool,
    trace: bool,
    live: Cow<'a, Path>,
    report: PathBuf,
    not_inputs: &'a [PathBuf],
}

/// A command's environment as the program sees it: without [`LIVE`], which is the launcher's.
#[derive(Debug)]
struct Passed<'a>(&'a [(OsString, OsString)]);

impl Serialize for Passed<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.0.iter().filter(|(name, _)| name != LIVE))
    }
}

/// The sandbox blaze makes: its launcher at `program`, which is blaze itself.
#[derive(Debug)]
pub struct Launcher {
    pub program: PathBuf,
}

impl Launcher {
    /// The spec of `command`, run with `scratch` as its scratch directory.
    pub fn spec<'a>(command: &'a Command, scratch: &Path) -> Result<SpecOf<'a>, String> {
        let Sandbox::On { read, write, network, trace } = &command.sandbox else {
            return Err(format!("{}: its sandbox is off", command.program.display())); // alloc: error path
        };
        let live = command.env.iter().rev().find(|(name, _)| name == LIVE);
        Ok(SpecOf {
            program: &command.program,
            args: &command.args,
            env: Passed(&command.env),
            dir: &command.dir,
            read,
            write,
            network: *network,
            trace: *trace,
            live: live.map_or_else(|| Cow::Owned(scratch.join(LIVE_FILE)), |(_, value)| Cow::Borrowed(Path::new(value))),
            report: scratch.join(REPORT_FILE),
            not_inputs: std::slice::from_ref(&command.program),
        })
    }

    /// What the report in `scratch` says, or why there is none.
    pub fn report(scratch: &Path) -> Result<Report, String> {
        let file = scratch.join(REPORT_FILE);
        let bytes = std::fs::read(&file).map_err(|error| {
            format!("the launcher left no report at {} ({error}): it did not end as it should", file.display()) // alloc: error path
        })?;
        trace::decode(&bytes).map_err(|why| format!("{}: {why}", file.display())) // alloc: error path
    }
}

impl Confine for Launcher {
    fn confine(&self, command: &Command, scratch: &Path) -> Result<Confined, String> {
        let spec = Launcher::spec(command, scratch)?;
        let file = scratch.join(SPEC_FILE);
        blaze_mem::scratch::with_bytes(|text| {
            serde_json::to_writer(&mut *text, &spec).map_err(|error| format!("the spec: {error}"))?; // alloc: error path
            std::fs::write(&file, &*text).map_err(|error| format!("{}: {error}", file.display())) // alloc: error path
        })?;
        let mut args = Vec::with_capacity(LAUNCH.len() + 1);
        args.extend(LAUNCH.map(OsString::from));
        args.push(file.into_os_string());
        Ok(Confined {
            program: self.program.clone(), // clone: the edge: `Confined` owns the process it starts
            args,
            // The launcher reads no variable: the program's environment is in the spec.
            env: Vec::new(), // alloc: empty, allocates nothing
            dir: scratch.to_owned(), // alloc: the edge: `Confined` owns the process it starts
        })
    }

    fn traced(&self, command: &Command, scratch: &Path) -> Result<Traced, String> {
        match Launcher::report(scratch)? {
            Report::NotMade(why) => {
                Err(format!("{}: no sandbox could be made: {why}", command.program.display())) // alloc: error path
            }
            Report::Ran(Some(traced)) => Ok(traced),
            Report::Ran(None) => match &command.sandbox {
                Sandbox::On { trace: false, .. } => Ok(Traced::default()),
                // alloc: error path
                _ => Err(format!(
                    "{}: the sandbox traces, and its report has no record of what was read",
                    command.program.display()
                )),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(trace: bool) -> Command {
        Command {
            program: "/build/script".into(),
            args: vec!["a".into()],
            env: vec![("CC".into(), "cc".into()), (LIVE.into(), "/root/live/7".into())],
            dir: "/ws".into(),
            stdin: Vec::new(),
            sandbox: Sandbox::On { read: vec!["/ws".into()], write: vec!["/out".into()], network: false, trace },
            memory: None,
        }
    }

    /// The spec of `command` as the launcher reads it back.
    fn read_back(command: &Command, scratch: &Path) -> Spec {
        let text = serde_json::to_vec(&Launcher::spec(command, scratch).unwrap()).unwrap();
        serde_json::from_slice(&text).unwrap()
    }

    #[test]
    fn the_live_variable_is_the_launchers_not_the_programs() {
        let spec = read_back(&command(true), Path::new("/s"));
        assert_eq!(spec.env, vec![("CC".into(), "cc".into())]);
        assert_eq!(spec.live, PathBuf::from("/root/live/7"));
        assert_eq!(spec.report, PathBuf::from("/s/report"));
        assert_eq!(spec.not_inputs, vec![PathBuf::from("/build/script")]);
        assert_eq!(
            (spec.program, spec.args, spec.dir, spec.read, spec.write, spec.network, spec.trace),
            ("/build/script".into(), vec!["a".into()], "/ws".into(), vec!["/ws".into()], vec!["/out".into()], false, true)
        );
        let mut unnamed = command(false);
        unnamed.env.retain(|(name, _)| name != LIVE);
        let spec = read_back(&unnamed, Path::new("/s"));
        assert_eq!((spec.live, spec.trace), (PathBuf::from("/s/live"), false));
        let mut off = command(true);
        off.sandbox = Sandbox::Off;
        assert!(Launcher::spec(&off, Path::new("/s")).is_err());
    }

    #[test]
    fn the_launcher_is_given_its_spec_and_the_report_is_read_back() {
        let scratch = std::env::temp_dir().join(format!("blaze-confine-test-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch).unwrap();
        let launcher = Launcher { program: "/bin/blaze".into() };
        let confined = launcher.confine(&command(true), &scratch).unwrap();
        assert_eq!(confined.program, PathBuf::from("/bin/blaze"));
        assert_eq!(confined.args[..2], [OsString::from("sandbox"), OsString::from("--v2")]);
        let text = std::fs::read(scratch.join(SPEC_FILE)).unwrap();
        let spec: Spec = serde_json::from_slice(&text).unwrap();
        assert_eq!(spec, read_back(&command(true), &scratch));
        assert!(launcher.traced(&command(true), &scratch).is_err());
        std::fs::write(scratch.join(REPORT_FILE), trace::encode(&Report::NotMade("no".into()))).unwrap();
        assert!(launcher.traced(&command(true), &scratch).unwrap_err().contains("no sandbox"));
        std::fs::write(scratch.join(REPORT_FILE), trace::encode(&Report::Ran(None))).unwrap();
        assert!(launcher.traced(&command(true), &scratch).is_err());
        assert_eq!(launcher.traced(&command(false), &scratch), Ok(Traced::default()));
        let traced = Traced { env: vec![("CC".into(), Some("cc".into()))], ..Traced::default() };
        std::fs::write(scratch.join(REPORT_FILE), trace::encode(&Report::Ran(Some(traced.clone())))).unwrap();
        assert_eq!(launcher.traced(&command(true), &scratch), Ok(traced));
        _ = std::fs::remove_dir_all(&scratch);
    }
}

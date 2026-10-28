//! What `blaze test` writes down of the doc tests cargo would have rustdoc run, and the run that
//! replays them; running them is the engine's (`crates/blaze-engine/src/doctest.rs`).

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::Context as _;

/// The variable that tells the run of the doc tests the build `blaze test` was asked
/// for, as JSON: the libraries the examples link against are built with its features, which a
/// package shares with the others it selects, not with those of a build of the package alone.
pub const BUILD: &str = "BLAZE_DOCTEST_BUILD";

/// The argument that has blaze make the runs [`write_run`] wrote down, before a [`Replay`] as
/// JSON.
pub const REPLAY: &str = "--replay-doctests";

/// The code a test harness ends with when a test failed.
pub const FAILED: i32 = 101;

/// A command that runs where blaze was asked and with its environment as it is.
#[expect(clippy::disallowed_methods, reason = "`toolchain::command` adds to the environment")]
pub fn as_asked(program: impl AsRef<std::ffi::OsStr>) -> Command {
    Command::new(program)
}

/// `command`, run in `cwd` with `env` and nothing else of blaze's environment.
pub fn in_place(mut command: Command, cwd: &Path, env: &[(OsString, OsString)]) -> Command {
    command.current_dir(cwd).env_clear().envs(env.iter().map(|(name, value)| (name, value)));
    command
}

/// The variable of `env` called `name`, when it is text.
pub fn var_in<'a>(env: &'a [(OsString, OsString)], name: &str) -> Option<&'a str> {
    env.iter().find(|(it, _)| it == name).and_then(|(_, value)| value.to_str())
}

/// A run of rustdoc as cargo asked for it: its arguments, where it ran and its environment.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Recorded {
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub env: Vec<(OsString, OsString)>,
}

impl Recorded {
    pub fn var(&self, name: &str) -> Option<&str> {
        var_in(&self.env, name)
    }

    /// The crate the run tests the examples of, as cargo names it after `Doc-tests`.
    pub fn crate_name(&self) -> &str {
        let mut args = self.args.iter().filter_map(|it| it.to_str());
        while let Some(arg) = args.next() {
            match arg.strip_prefix("--crate-name") {
                Some("") => return args.next().unwrap_or_default(),
                Some(joined) if joined.starts_with('=') => return &joined[1..],
                _ => {}
            }
        }
        ""
    }

    /// Says why blaze does not run the examples of the run's library: they fail.
    pub fn refused(&self, why: &str) -> i32 {
        let name = self.crate_name();
        crate::cargo::say::error(&format!("blaze does not run the doc tests of `{name}`: {why}"));
        FAILED
    }

    /// Writes the run down in `dir`, under a name that sorts after those of the runs before.
    pub fn write(&self, dir: &Path) -> anyhow::Result<()> {
        let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?;
        let file = dir.join(format!("{:039}-{}.json", since.as_nanos(), std::process::id()));
        std::fs::write(file, serde_json::to_vec(self)?)?;
        Ok(())
    }

    /// The runs written down in `dir`, in the order cargo asked for them.
    pub fn all_in(dir: &Path) -> anyhow::Result<Vec<Recorded>> {
        let mut files = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            files.push(entry?.path());
        }
        files.retain(|it| it.extension().is_some_and(|it| it == "json"));
        files.sort();
        let read = |file: &PathBuf| Ok(serde_json::from_slice(&std::fs::read(file)?)?);
        files.iter().map(read).collect()
    }
}

/// Writes down in `dir`, for a [`Replay`], the run of rustdoc `cargo test --doc` would make of
/// a library's examples: blaze plans the doc tests itself, and no cargo runs.
pub fn write_run(
    dir: &Path,
    args: Vec<OsString>,
    cwd: PathBuf,
    env: Vec<(OsString, OsString)>,
) -> anyhow::Result<()> {
    Recorded { args, cwd, env }.write(dir)
}

/// The runs of rustdoc [`write_run`] wrote down in `dir`, and how `blaze test` was asked
/// to run them.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Replay {
    pub dir: PathBuf,
    /// Whether cargo was asked to go on past a library whose examples failed.
    pub no_fail_fast: bool,
    pub quiet: bool,
}

impl Replay {
    /// The run of blaze that makes the runs, where cargo made the first and with its
    /// environment. `None` when cargo made none.
    pub fn command(&self) -> anyhow::Result<Option<Command>> {
        let Some(first) = Recorded::all_in(&self.dir)?.into_iter().next() else {
            return Ok(None);
        };
        let blaze = std::env::current_exe().context("finding blaze itself")?;
        let mut blaze = in_place(as_asked(blaze), &first.cwd, &first.env);
        blaze.arg(REPLAY).arg(serde_json::to_string(self)?);
        Ok(Some(blaze))
    }
}

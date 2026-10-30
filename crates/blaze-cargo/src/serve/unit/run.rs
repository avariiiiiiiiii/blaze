//! The program of a build run ([`Front::run`](crate::serve::work::Front::run)): the binary
//! `cargo run` picks (`cargo/src/cargo/ops/cargo_run.rs`: `--bin`, then the package's
//! `default-run`, then the only one), run with its arguments in the sandbox tests run in, its
//! end reported as [`Response::Ran`].

#![forbid(unsafe_code)]

use std::{ffi::OsString, path::PathBuf, sync::Arc};

use blaze_api::{ApiError, Artifact, Response};
use blaze_cas::Store;
use blaze_core::{Command, Completion, Done, Effect, Sandbox, Tag};
use blaze_units::schema::Mode;
use rustc_hash::FxHashMap;

use super::{Job, package_of};
use crate::serve::work::{Runs, Writer};

/// The environment `artifact`'s program runs in: the job's, cargo's variables of it over it,
/// and the job's dynamic library search path after cargo's directories, as
/// `Compilation::fill_env` joins them (on macOS, with none set, `$HOME/lib`, `/usr/local/lib`
/// and `/usr/lib` after).
pub(crate) fn process_env(job: &[(String, String)], artifact: &Artifact, os: &str) -> Vec<(OsString, OsString)> {
    let var = super::artifact::dylib_var(os);
    let separator = match os {
        "windows" => ";",
        _ => ":",
    };
    let mut env: FxHashMap<String, String> = job.iter().cloned().collect();
    for (name, value) in &artifact.env {
        let joined = match env.get(name).filter(|_| name == var) {
            Some(theirs) if theirs.starts_with(value.as_str()) => theirs.clone(),
            Some(theirs) if !theirs.is_empty() => format!("{value}{separator}{theirs}"),
            _ if name == var && os == "macos" => {
                let mut dirs = vec![value.clone()];
                if let Some(home) = env.get("HOME") {
                    dirs.push(format!("{home}/lib"));
                }
                dirs.extend(["/usr/local/lib".to_owned(), "/usr/lib".to_owned()]);
                dirs.join(separator)
            }
            _ => value.clone(),
        };
        env.insert(name.clone(), joined);
    }
    let mut env: Vec<(String, String)> = env.into_iter().collect();
    env.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    env.into_iter().map(|(name, value)| (OsString::from(name), OsString::from(value))).collect()
}

/// Whether `artifact` is a program `cargo run` may run: a binary or an example built as one.
fn runnable(artifact: &Artifact) -> bool {
    !artifact.profile.test
        && artifact.executable.is_some()
        && (artifact.kind == "bin" || artifact.target.kind.iter().any(|it| it == "example"))
}

/// The program cargo runs of `built`: `bin` when given, else the `default-run` of the package
/// they are of, else the only one; cargo's error otherwise.
pub(crate) fn choose<'a>(
    built: &'a [Artifact],
    bin: Option<&str>,
    default_run: Option<&str>,
) -> Result<&'a Artifact, String> {
    let runnable: Vec<&Artifact> = built.iter().filter(|it| runnable(it)).collect();
    let names = || runnable.iter().map(|it| it.target.name.clone()).collect::<Vec<_>>();
    if let Some(bin) = bin {
        return runnable.iter().copied().find(|it| it.target.name == bin).ok_or_else(|| {
            let mut what = format!("no bin target named `{bin}`");
            if !runnable.is_empty() {
                what += "\n\nhelp: available bin targets:";
                for name in names() {
                    what += &format!("\n    {name}");
                }
            }
            what
        });
    }
    if let Some(default) = default_run
        && let Some(it) = runnable.iter().copied().find(|it| it.target.name == default)
    {
        return Ok(it);
    }
    match runnable.as_slice() {
        [] => Err("a bin target must be available for `cargo run`".to_owned()),
        [only] => Ok(only),
        _ => Err(format!(
            "`cargo run` could not determine which binary to run. Use the `--bin` option to specify a \
             binary, or the `default-run` manifest key.\navailable binaries: {}",
            names().join(", ")
        )),
    }
}

/// The program `start` runs, on the loop.
struct Program {
    command: Option<Command>,
    tag: Tag,
    running: bool,
}

pub(crate) fn start(
    job: &Arc<Job>,
    built: &[Artifact],
    bin: Option<&str>,
    args: &[String],
    tags: u64,
) -> Result<Box<dyn Runs>, ApiError> {
    let units = job.units();
    let default_run = {
        let mut packages: Vec<&str> =
            built.iter().filter(|it| runnable(it)).map(|it| it.package.as_str()).collect();
        packages.sort_unstable();
        packages.dedup();
        match packages.as_slice() {
            [one] => units
                .packages
                .binary_search_by(|it| it.id.as_str().cmp(one))
                .ok()
                .and_then(|at| units.packages[at].default_run.as_deref()),
            _ => None,
        }
    };
    let chosen = choose(built, bin, default_run).map_err(|what| ApiError::new(what, None))?;
    let program = chosen.executable.clone().unwrap_or_else(|| chosen.path.clone());
    let target = &chosen.target;
    let at = job
        .graph
        .find(&units.graph.units, &chosen.package, &target.name, &target.kind, Mode::Build)
        .ok_or_else(|| ApiError::new(format!("no unit of `{}` is in the build", target.name), None))? as usize;
    let unit = &units.graph.units[at];
    let package = package_of(units, unit)?;
    let config = &job.input.config;
    let env = process_env(&config.invocation.env, chosen, std::env::consts::OS);
    // cargo's `target.<triple>.runner`: the runner, its arguments, then the program's.
    let (program, args): (String, Vec<String>) = match chosen.runner.split_first() {
        Some((runner, before)) => {
            (runner.clone(), before.iter().cloned().chain([program]).chain(args.iter().cloned()).collect())
        }
        None => (program, args.to_vec()),
    };
    let manifest_dir = PathBuf::from(&package.manifest_path).parent().map(PathBuf::from).unwrap_or_default();
    let sandbox = match job.sandbox_off() {
        true => Sandbox::Off,
        false => Sandbox::On {
            read: vec![PathBuf::from(&config.invocation.root), PathBuf::from(&units.target_dir), manifest_dir],
            write: Vec::new(),
            network: false,
            trace: false,
        },
    };
    let command = Command {
        program: PathBuf::from(program),
        args: args.into_iter().map(OsString::from).collect(),
        env,
        dir: PathBuf::from(&config.invocation.cwd),
        stdin: Vec::new(),
        sandbox,
        memory: None,
    };
    Ok(Box::new(Program { command: Some(command), tag: Tag(tags), running: false }))
}

impl Runs for Program {
    fn start(
        &mut self,
        _store: &Store,
        _writer: Option<&mut dyn Writer>,
        effects: &mut Vec<Effect>,
        _out: &mut Vec<Response>,
    ) -> Option<bool> {
        let command = self.command.take()?;
        effects.push(Effect::Spawn { command: Box::new(command), tag: self.tag });
        self.running = true;
        None
    }

    fn owns(&self, tag: Tag) -> bool {
        tag == self.tag
    }

    fn complete(
        &mut self,
        completion: Completion,
        _store: &mut Store,
        _writer: Option<&mut dyn Writer>,
        _effects: &mut Vec<Effect>,
        out: &mut Vec<Response>,
    ) -> Option<bool> {
        self.running = false;
        match completion.done {
            Done::Exited(exit) => {
                let ok = exit.code == Some(0);
                out.push(Response::Ran { code: exit.code, stdout: exit.stdout, stderr: exit.stderr });
                Some(ok)
            }
            Done::Failed(error) => {
                out.push(Response::Ran { code: None, stdout: Vec::new(), stderr: error.0.into_bytes() });
                Some(false)
            }
            _ => Some(false),
        }
    }

    fn cancel(&mut self, effects: &mut Vec<Effect>) -> Option<bool> {
        match self.running {
            true => {
                effects.push(Effect::Kill { tag: self.tag });
                None
            }
            false => {
                self.command = None;
                Some(false)
            }
        }
    }
}

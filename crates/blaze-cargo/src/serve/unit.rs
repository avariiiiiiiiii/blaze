//! The daemon's real build ([`Real`], the [`Steps`] of `blaze serve`; docs/v2/plan/lanes/cargo/notes.md
//! section 6): a job's units lowered into the front end, each checked, compiled and linked
//! by blaze's own code generator and linker, its build scripts, tests and program run as the
//! driver's sandboxed spawns.
//!
//! - [`graph`]: the job's unit graph as flat columns, built once.
//! - [`files`]: where a unit's files are (the mounts), and the names rustc gives them.
//! - [`compile`]: [`Front::compile`] and [`Front::fresh`]: diagnostics, codegen, the link,
//!   the files cargo would write, a unit's record in the store.
//! - [`codegen`]: a unit's codegen policy and roots, and its link.
//! - [`artifact`]: cargo's `compiler-artifact` of a unit.
//! - [`script`]: a build script's run and what its directives do, as cargo applies them.
//! - [`test`]: the tests of a build, on one queue, the reached ones only.
//! - [`run`]: the program `cargo run` picks, run in the tests' sandbox.
//! - [`reach`]: what a change reaches, from the database's facts.
//! - [`say`]: cargo's own lines (`error: ...`, `warning: ...`) as diagnostics.
//!
//! Nothing here runs rustc or cargo, and nothing user code runs is outside the sandbox but
//! what `BLAZE_SANDBOX=off` and the manifest's opt-outs take out (docs/cargo.md).

mod artifact;
mod codegen;
mod compile;
mod files;
mod graph;
mod reach;
mod run;
mod say;
mod script;
mod test;

#[cfg(test)]
mod tests;

use std::{
    any::Any,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock, atomic::Ordering},
    time::Instant,
};

use blaze_api::{ApiError, Artifact, Reach, UnitOf, UnitTarget};
use blaze_units::{
    query::{Fetch, Units},
    schema::{Mode, Unit},
};
use load_cargo::v2::{Lower, Lowered};

use crate::serve::{
    Paths,
    fetch::Fetcher,
    work::{Compiled, Failed, Front, Goal, JobInput, Runs, Source, SourceFile, Steps},
};

/// The build `blaze serve` runs: one for the daemon's life, shared by every job.
pub struct Real {
    fetcher: Fetcher,
    shared: Arc<Shared>,
}

/// What every job's front shares: the daemon's paths, what was read of the machine's files,
/// the images kept linked between jobs, the code generator's memo.
pub(crate) struct Shared {
    pub(crate) state: PathBuf,
    pub(crate) launcher: PathBuf,
    /// The directory of blaze's stand-ins for the tools cargo names to a build script:
    /// `rustc` and `rustdoc`, each blaze's own executable under that name.
    pub(crate) tools: PathBuf,
    /// Every executable and shared library linked, by the path cargo writes it at, kept so the
    /// next link of it relinks only what changed. Taken out for a link and put back after, so
    /// the lock is held for the move only.
    pub(crate) images: Mutex<rustc_hash::FxHashMap<String, codegen::Linked>>,
    pub(crate) memo: Arc<blaze_codegen::memo::Memo>,
    /// What `-C target-cpu=native` is on this machine; none without LLVM.
    pub(crate) host_cpu: Option<blaze_units::stdlib::target::HostCpu>,
}

impl Real {
    pub fn new(paths: &Paths) -> Result<Real, String> {
        let fetcher = Fetcher::new()?;
        let tools = paths.state.join("tools");
        stand_ins(&tools, &paths.launcher)?;
        let shared = Shared {
            state: paths.state.clone(),
            launcher: paths.launcher.clone(),
            tools,
            images: Mutex::new(rustc_hash::FxHashMap::default()),
            memo: blaze_codegen::driver::shared_memo(),
            host_cpu: hir_ty_codegen::host_cpu().map(|(name, features)| blaze_units::stdlib::target::HostCpu {
                arch: std::env::consts::ARCH.to_owned(),
                name,
                features,
            }),
        };
        Ok(Real { fetcher, shared: Arc::new(shared) })
    }
}

/// blaze's executable linked as `rustc` and `rustdoc` in `dir`: what a build script is given as
/// `RUSTC` and `RUSTDOC`, so a probe of them (`autocfg`, `version_check`) is answered by blaze
/// and no rustc runs (docs/no-rustc.md).
fn stand_ins(dir: &std::path::Path, launcher: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    for name in ["rustc", "rustdoc"] {
        let at = dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        match std::fs::read_link(&at) {
            Ok(to) if to == launcher => continue,
            Ok(_) => std::fs::remove_file(&at).map_err(|error| format!("{}: {error}", at.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("{}: {error}", at.display())),
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(launcher, &at).map_err(|error| format!("{}: {error}", at.display()))?;
        #[cfg(windows)]
        std::fs::hard_link(launcher, &at).map_err(|error| format!("{}: {error}", at.display()))?;
    }
    Ok(())
}

impl Steps for Real {
    fn fetch(&self, fetch: &Fetch) -> Result<Source, ApiError> {
        self.fetcher.fetch(fetch)
    }

    fn library(&self) -> Result<(String, Source), ApiError> {
        let library = blaze_units::stdlib::library(&self.shared.launcher)
            .map_err(|what| ApiError::new(what, Some("the standard library's sources".to_owned())))?;
        let files = library
            .files
            .into_iter()
            .map(|it| SourceFile {
                path: it.path,
                bytes: it.bytes,
                mode: match it.executable {
                    true => blaze_vfs::Mode::Executable,
                    false => blaze_vfs::Mode::Regular,
                },
            })
            .collect();
        Ok((library.at, Source { files, under: String::new() }))
    }

    fn lower(&self, job: &JobInput) -> Result<Arc<dyn Front>, Failed> {
        if job.cancelled.load(Ordering::Relaxed) {
            return Err(Failed::Again);
        }
        let lowered = cancellable(|| {
            load_cargo::v2::lower(Lower {
                units: &job.units,
                config: &job.config,
                files: &job.files,
                root: job.root,
                host_cpu: self.shared.host_cpu.as_ref(),
            })
        })?
        .map_err(|error| Failed::Error(ApiError::new(error.what, error.at)))?;
        Ok(Arc::new(JobFront(Arc::new(Job::new(job.clone(), lowered, self.shared.clone())))))
    }
}

/// `work` on a worker, the front end's or the build graph's cancel as [`Failed::Again`]: the
/// loop posts it again whole, and nothing of it is kept. Any other panic is not caught.
pub(crate) fn cancellable<T>(work: impl FnOnce() -> T) -> Result<T, Failed> {
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work));
    match caught {
        Ok(it) => Ok(it),
        Err(payload) => {
            if payload.is::<salsa::Cancelled>() || payload.is::<blaze_graph::Cancelled>() {
                Err(Failed::Again)
            } else {
                std::panic::resume_unwind(payload)
            }
        }
    }
}

/// One job's front: its units lowered, and what its build scripts and compiles found that
/// later units read.
pub(crate) struct Job {
    pub(crate) input: JobInput,
    pub(crate) lowered: Lowered,
    pub(crate) shared: Arc<Shared>,
    /// The unit graph as columns.
    pub(crate) graph: graph::Graph,
    /// What each build script's run told cargo, by its run unit. Each slot is written once,
    /// when the run ends, and read with no lock by every unit after it.
    pub(crate) scripts: Box<[OnceLock<script::Applied>]>,
    /// What each compiled unit made that later work reads: its main file, its tests. Written
    /// once per unit.
    pub(crate) made: Box<[OnceLock<compile::Made>]>,
    /// Each unit's record key, worked out once: `fresh` on the loop and `compile` on a worker
    /// both read it.
    pub(crate) keys: Box<[OnceLock<Option<blaze_cas::Key>>]>,
    /// The view and the mounts, as paths.
    pub(crate) mounted: Arc<files::Mounted>,
    /// What each build script prints, by its compile unit, when it provably prints only
    /// constants; worked out once.
    pub(crate) inert: Box<[OnceLock<Option<blaze_usercode::run::Value>>]>,
    /// How each unit's program is run, worked out once (the key and the artifact read it).
    pub(crate) runs: Box<[OnceLock<artifact::Runs>]>,
    /// Each build script run prepared by `fresh`, taken by `script`, so it is prepared once.
    /// Only the loop touches it: the lock is never contended.
    pub(crate) starts: Box<[Mutex<Option<Box<script::Start>>>]>,
    /// The crates built for build scripts and proc macros, as the code generator takes them.
    pub(crate) build_time: OnceLock<hir_ty_codegen::v2::BuildTime>,
    /// When the job's front was made: the clock the test queue is given.
    pub(crate) began: Instant,
}

impl Job {
    fn new(input: JobInput, lowered: Lowered, shared: Arc<Shared>) -> Job {
        let graph = graph::Graph::new(&input.units);
        let config = &input.config;
        let mounted = Arc::new(files::Mounted::new(input.files.clone(), input.root, &config.mounts, &config.invocation.root));
        let slots = graph.len();
        Job {
            input,
            lowered,
            shared,
            graph,
            scripts: (0..slots).map(|_| OnceLock::new()).collect(),
            made: (0..slots).map(|_| OnceLock::new()).collect(),
            keys: (0..slots).map(|_| OnceLock::new()).collect(),
            runs: (0..slots).map(|_| OnceLock::new()).collect(),
            mounted,
            inert: (0..slots).map(|_| OnceLock::new()).collect(),
            starts: (0..slots).map(|_| Mutex::new(None)).collect(),
            build_time: OnceLock::new(),
            began: Instant::now(),
        }
    }

    pub(crate) fn units(&self) -> &Units {
        &self.input.units
    }

    pub(crate) fn unit(&self, unit: usize) -> Result<&Unit, ApiError> {
        self.units()
            .graph
            .units
            .get(unit)
            .ok_or_else(|| ApiError::new(format!("the build has no unit {unit}"), None))
    }

    pub(crate) fn plan(&self, unit: usize) -> Result<&blaze_units::query::UnitPlan, ApiError> {
        self.units()
            .plans
            .get(unit)
            .ok_or_else(|| ApiError::new(format!("the build has no plan of unit {unit}"), None))
    }

    pub(crate) fn package(&self, unit: &Unit) -> Result<&blaze_units::query::PackagePlan, ApiError> {
        package_of(self.units(), unit)
    }

    /// The unit a diagnostic of `unit` is of, as cargo's JSON names it.
    pub(crate) fn unit_of(&self, unit: &Unit) -> Result<UnitOf, ApiError> {
        let package = self.package(unit)?;
        Ok(UnitOf {
            package_id: unit.pkg_id.clone(),
            manifest_path: package.manifest_path.clone(),
            target: unit_target(unit),
        })
    }

    /// `BLAZE_SANDBOX=off` in the request's environment: every build script and test of the
    /// command runs outside the sandbox (docs/cargo.md).
    pub(crate) fn sandbox_off(&self) -> bool {
        sandbox_off(&self.input.config.invocation.env)
    }

    pub(crate) fn cancelled(&self) -> bool {
        self.input.cancelled.load(Ordering::Relaxed)
    }
}

pub(crate) fn sandbox_off(env: &[(String, String)]) -> bool {
    env.iter().any(|(name, value)| name == "BLAZE_SANDBOX" && value == "off")
}

pub(crate) fn package_of<'a>(
    units: &'a Units,
    unit: &Unit,
) -> Result<&'a blaze_units::query::PackagePlan, ApiError> {
    units
        .packages
        .binary_search_by(|it| it.id.as_str().cmp(&unit.pkg_id))
        .ok()
        .map(|at| &units.packages[at])
        .ok_or_else(|| ApiError::new(format!("the build has no package `{}`", unit.pkg_id), None))
}

/// A unit's target, as cargo's JSON messages give it.
pub(crate) fn unit_target(unit: &Unit) -> UnitTarget {
    let target = &unit.target;
    UnitTarget {
        kind: target.kind.clone(),
        crate_types: target.crate_types.clone(),
        name: target.name.clone(),
        src_path: target.src_path.to_string_lossy().into_owned(),
        edition: target.edition.clone(),
        doc: target.doc,
        doctest: target.doctest,
        test: target.test,
    }
}

/// Whether the job stops `unit` after the front end: a check, or every unit of `cargo check`.
pub(crate) fn checks_only(goal: Goal, unit: &Unit) -> bool {
    goal == Goal::Check || matches!(unit.mode, Mode::Check)
}

/// A job's front as the loop holds it.
pub(crate) struct JobFront(pub(crate) Arc<Job>);

impl Front for JobFront {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn fresh(&self, unit: usize) -> bool {
        match self.0.unit(unit).map(|it| it.mode) {
            Ok(Mode::RunCustomBuild) => script::fresh(&self.0, unit),
            Ok(_) => compile::fresh(&self.0, unit),
            Err(_) => false,
        }
    }

    fn compile(&self, unit: usize) -> Result<Compiled, Failed> {
        if self.0.cancelled() {
            return Err(Failed::Again);
        }
        cancellable(|| compile::compile(&self.0, unit))?
    }

    fn script(&self, unit: usize, tags: u64) -> Result<Box<dyn Runs>, ApiError> {
        script::start(&self.0, unit, tags)
    }

    fn tests(
        &self,
        built: &[Artifact],
        filter: &[String],
        all: bool,
        tags: u64,
    ) -> Result<Box<dyn Runs>, ApiError> {
        test::start(&self.0, built, filter, all, tags)
    }

    fn run(
        &self,
        built: &[Artifact],
        bin: Option<&str>,
        args: &[String],
        tags: u64,
    ) -> Result<Box<dyn Runs>, ApiError> {
        run::start(&self.0, built, bin, args, tags)
    }

    fn reach(&self, from: &dyn Front, changes: &[blaze_vfs::Change]) -> Result<Reach, Failed> {
        let Some(from) = from.as_any().downcast_ref::<JobFront>() else {
            return Err(Failed::Error(ApiError::new(
                "the view to reach from was not lowered by this daemon's build",
                None,
            )));
        };
        if self.0.cancelled() {
            return Err(Failed::Again);
        }
        cancellable(|| reach::reach(&from.0, &self.0, changes))?
    }
}

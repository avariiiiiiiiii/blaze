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
//! - [`test`]: the tests of a build, on one queue, the reached ones only; with `--ub`, each
//!   run in blaze's MIR evaluator under the checker of undefined behavior, nothing compiled.
//! - [`run`]: the program `cargo run` picks, run in the tests' sandbox.
//! - [`reach`]: what a change reaches, from the database's facts.
//! - [`say`]: cargo's own lines (`error: ...`, `warning: ...`) as diagnostics.
//!
//! The front end is written by the loop alone, through the job's [`UnitWriter`], and read by
//! each task through the [`UnitReader`] it was posted with, a shared reference: no worker
//! locks to read it, and a write waits for every reader before it (`work`'s docs).
//!
//! Nothing here runs rustc or cargo, and nothing user code runs is outside the sandbox but
//! what `BLAZE_SANDBOX=off` and the manifest's opt-outs take out (docs/cargo.md).

#![forbid(unsafe_code)]

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
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock, atomic::Ordering},
    time::Instant,
};

use base_db::FileId;
use blaze_api::{ApiError, Artifact, Reach, Table, TestResult, Ub, UnitOf, UnitTarget};
use blaze_ids::ContentId;
use blaze_units::{
    query::{BuildConfig, Fetch, Units},
    schema::{Mode, Unit},
};
use load_cargo::v2::{Crates, Db, Lower, Lowered, ProcMacroLoadResult, Ran};

use crate::serve::{
    Paths,
    fetch::Fetcher,
    work::{
        Compiled, Failed, Front, FrontEnd, Goal, Image, JobInput, Kept, KeptFront, Library, Reader, Runs, Selection,
        Source, SourceFile, Steps, Writer,
    },
};

/// The build `blaze serve` runs: one for the daemon's life, shared by every job.
pub struct Real {
    fetcher: Fetcher,
    shared: Arc<Shared>,
}

/// What every job's front shares: the daemon's paths, what was read of the machine's files,
/// the code generator's memo. The images kept linked between jobs are the loop's
/// ([`Image`]).
pub(crate) struct Shared {
    pub(crate) state: PathBuf,
    pub(crate) launcher: PathBuf,
    /// The directory of blaze's stand-ins for the tools cargo names to a build script:
    /// `rustc` and `rustdoc`, each blaze's own executable under that name.
    pub(crate) tools: PathBuf,
    pub(crate) memo: Arc<blaze_codegen::memo::Memo>,
    /// What `-C target-cpu=native` is on this machine; none without LLVM.
    pub(crate) host_cpu: Option<blaze_units::stdlib::target::HostCpu>,
}

impl Real {
    pub fn new(paths: &Paths) -> Result<Real, String> {
        let fetcher = Fetcher::new(paths.state.join("fetch"))?;
        let tools = paths.state.join("tools");
        stand_ins(&tools, &paths.launcher)?;
        let shared = Shared {
            state: paths.state.clone(),
            launcher: paths.launcher.clone(),
            tools,
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

    fn library(&self, kept: &[(String, String)]) -> Result<Library, ApiError> {
        let at = || Some("the standard library's sources".to_owned());
        let library = crate::cargo::toolchain::library::fetch().map_err(|what| ApiError::new(what, at()))?;
        let in_dir = |what| ApiError::new(what, Some(library.dir.display().to_string()));
        let named = blaze_units::stdlib::library_at(&library.commit);
        let mut hasher = blaze_ids::Hasher::new("blaze v2 serve library fingerprint");
        hasher.part(library.digest.as_bytes());
        library_fingerprint(&library.dir, "", &mut hasher).map_err(in_dir)?;
        let fingerprint = format!("{:x}", hasher.finish());
        if kept.iter().any(|(it, mounted)| *it == fingerprint && *mounted == named) {
            return Ok(Library { at: named, fingerprint, source: None });
        }
        let mut files = Vec::new();
        library_files(&library.dir, "", &mut files).map_err(in_dir)?;
        blaze_ids::names::sort_by_name(&mut files, |it| it.path.as_bytes());
        Ok(Library { at: named, fingerprint, source: Some(Source { files, under: String::new() }) })
    }

    fn lower(&self, input: &JobInput, kept: Option<KeptFront>) -> Result<FrontEnd, Failed> {
        if input.cancelled.load(Ordering::Relaxed) {
            return Err(Failed::Again);
        }
        let kept = kept.and_then(KeptFront::take::<KeptLowered>).filter(|it| {
            (Arc::ptr_eq(&it.units, &input.units) || it.units == input.units) && it.config == input.config
        });
        let writer = cancellable(move || -> Result<UnitWriter, Failed> {
            let lower = || Lower {
                units: &input.units,
                config: &input.config,
                files: &input.files,
                root: input.root,
                host_cpu: self.shared.host_cpu.as_ref(),
            };
            let failed = |error| Failed::Error(lower_error(error));
            let mut lowered = match kept {
                Some(KeptLowered { mut lowered, .. }) => match lowered.refresh(lower()).map_err(failed)? {
                    true => lowered,
                    false => load_cargo::v2::lower(lower()).map_err(failed)?,
                },
                None => load_cargo::v2::lower(lower()).map_err(failed)?,
            };
            let job = Arc::new(Job::new(input.clone(), lowered.crates().clone(), self.shared.clone()));
            let waits = graph::Waits::new(&job.graph);
            for unit in 0..job.graph.len() as u32 {
                if unit % 64 == 0 && input.cancelled.load(Ordering::Relaxed) {
                    return Err(Failed::Again);
                }
                if waits.ready(unit) {
                    identify(&mut lowered, &job, unit);
                }
            }
            Ok(UnitWriter { front: Arc::new(lowered), job, waits, ready: Vec::new(), runs: Vec::new(), macros: Vec::new() })
        })??;
        let front: Arc<dyn Front> = Arc::new(JobFront(writer.job.clone()));
        Ok(FrontEnd { front, writer: Box::new(writer) })
    }
}

fn lower_error(error: load_cargo::v2::LowerError) -> ApiError {
    ApiError::new(error.what, error.at)
}

/// A job's front end as the loop keeps it ([`Writer::keep`]), with what it was lowered from:
/// the next build of the head lowers into it only when it lowers the same.
struct KeptLowered {
    lowered: Lowered,
    units: Arc<Units>,
    config: Arc<BuildConfig>,
}

/// What a task reads the front end through ([`Reader`]): a shared reference to the job's
/// lowered front end, as it stood when the loop posted the task. Its database is read by
/// reference, never cloned.
pub(crate) struct UnitReader {
    front: Arc<Lowered>,
}

impl UnitReader {
    pub(crate) fn db(&self) -> &Db {
        self.front.db()
    }

    /// The job's files by path.
    pub(crate) fn paths(&self) -> &Arc<HashMap<FileId, String>> {
        self.front.paths()
    }
}

/// The job's front end as only the loop writes it: the one reference not lent to a reader,
/// the writes asked for and not yet applied, and as each build script's run is applied, the
/// identities of the units it was the last run in the closure of.
pub(crate) struct UnitWriter {
    front: Arc<Lowered>,
    job: Arc<Job>,
    waits: graph::Waits,
    /// The units a run made ready, reused from run to run.
    ready: Vec<u32>,
    /// Build scripts' runs asked for, checked, in the order asked.
    runs: Vec<(usize, Ran)>,
    /// Proc macros' loads asked for, checked, in the order asked.
    macros: Vec<(usize, ProcMacroLoadResult)>,
}

impl Writer for UnitWriter {
    fn reader(&self) -> Option<Reader> {
        (!self.waiting()).then(|| Reader::new(UnitReader { front: Arc::clone(&self.front) }))
    }

    fn ran(&mut self, run: usize, ran: Ran) -> Result<(), ApiError> {
        self.front.check_ran(run, &ran).map_err(lower_error)?;
        self.runs.push((run, ran));
        Ok(())
    }

    fn macros(&mut self, unit: usize, macros: ProcMacroLoadResult) -> Result<(), ApiError> {
        self.front.check_macros(unit).map_err(lower_error)?;
        self.macros.push((unit, macros));
        Ok(())
    }

    fn waiting(&self) -> bool {
        !self.runs.is_empty() || !self.macros.is_empty()
    }

    fn apply(&mut self) -> Result<bool, ApiError> {
        if !self.waiting() {
            return Ok(true);
        }
        let Some(lowered) = Arc::get_mut(&mut self.front) else { return Ok(false) };
        for (unit, macros) in self.macros.drain(..) {
            lowered.macros(unit, macros).map_err(lower_error)?;
        }
        for (run, ran) in self.runs.drain(..) {
            lowered.ran(run, &ran).map_err(lower_error)?;
            self.ready.clear();
            self.waits.applied(run as u32, &mut self.ready);
            for &unit in &self.ready {
                identify(lowered, &self.job, unit);
            }
        }
        Ok(true)
    }

    fn tables(&mut self) -> Result<Vec<(&'static str, u64, u64)>, Failed> {
        let Some(lowered) = Arc::get_mut(&mut self.front) else {
            let what = "the front end's tables are measured with no task reading it, and one still does";
            return Err(ApiError::new(what, None).into());
        };
        cancellable(|| lowered.tables())
    }

    fn keep(mut self: Box<Self>) -> Option<KeptFront> {
        if !matches!(self.apply(), Ok(true)) {
            return None;
        }
        let UnitWriter { front, job, .. } = *self;
        let lowered = Arc::into_inner(front)?;
        Some(KeptFront::new(KeptLowered { lowered, units: job.input.units.clone(), config: job.input.config.clone() }))
    }
}

/// Unit `unit`'s identity set from the front end, once every run of its closure is applied:
/// from then on nothing changes what its crate is made from.
fn identify(lowered: &mut Lowered, job: &Job, unit: u32) {
    if let Some(slot) = job.identities.get(unit as usize) {
        let _ = slot.set(lowered.identity(unit as usize));
    }
}

/// Every file under the library's `dir`, by its path from there. A link is its target's bytes:
/// the library's digest (`library::digest_of`) was checked when it was found or fetched.
fn library_files(dir: &std::path::Path, prefix: &str, into: &mut Vec<SourceFile>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("{}: {error}", dir.display()))?;
        let full = entry.path();
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(|| format!("{}: a name that is not UTF-8", full.display()))?;
        let path = if prefix.is_empty() { name.to_owned() } else { format!("{prefix}/{name}") };
        let meta = std::fs::metadata(&full).map_err(|error| format!("{}: {error}", full.display()))?;
        if meta.is_dir() {
            library_files(&full, &path, into)?;
            continue;
        }
        let bytes = std::fs::read(&full).map_err(|error| format!("{}: {error}", full.display()))?;
        #[cfg(unix)]
        let executable = std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o111 != 0;
        #[cfg(not(unix))]
        let executable = false;
        let mode = if executable { blaze_vfs::Mode::Executable } else { blaze_vfs::Mode::Regular };
        into.push(SourceFile { path, bytes, mode });
    }
    Ok(())
}

/// Every file under the library's `dir` as [`library_files`] reads it, but looked at only:
/// its path, size, time of its last change and whether it is executable, in name order.
fn library_fingerprint(dir: &std::path::Path, prefix: &str, into: &mut blaze_ids::Hasher) -> Result<(), String> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|error| format!("{}: {error}", dir.display()))? {
        let entry = entry.map_err(|error| format!("{}: {error}", dir.display()))?;
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(|| format!("{}: a name that is not UTF-8", entry.path().display()))?;
        entries.push((name.to_owned(), entry.path()));
    }
    blaze_ids::names::sort_by_name(&mut entries, |it| it.0.as_bytes());
    for (name, full) in entries {
        let path = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
        let meta = std::fs::metadata(&full).map_err(|error| format!("{}: {error}", full.display()))?;
        if meta.is_dir() {
            library_fingerprint(&full, &path, into)?;
            continue;
        }
        let modified = meta.modified().map_err(|error| format!("{}: {error}", full.display()))?;
        let since = modified.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        #[cfg(unix)]
        let executable = std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o111 != 0;
        #[cfg(not(unix))]
        let executable = false;
        into.part(path.as_bytes());
        into.update(&meta.len().to_le_bytes()).update(&since.as_nanos().to_le_bytes()).update(&[u8::from(executable)]);
    }
    Ok(())
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
    /// Each unit's crate in the front end.
    pub(crate) crates: Arc<Crates>,
    pub(crate) shared: Arc<Shared>,
    /// The unit graph as columns.
    pub(crate) graph: graph::Graph,
    /// Each unit's identity in the front end (what its crate is made from, deeply), set by the
    /// writer once no run left to apply changes it; unset, the unit's record is not known.
    pub(crate) identities: Box<[OnceLock<Option<ContentId>>]>,
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
    /// `blaze test --ub`'s tests, selected once on a worker, read by every evaluation.
    pub(crate) ub: OnceLock<test::UbTests>,
    /// When the job's front was made: the clock the test queue is given.
    pub(crate) began: Instant,
}

impl Job {
    fn new(input: JobInput, crates: Arc<Crates>, shared: Arc<Shared>) -> Job {
        let graph = graph::Graph::new(&input.units);
        let config = &input.config;
        let mounted = Arc::new(files::Mounted::new(input.files.clone(), input.root, &config.mounts, &config.invocation.root));
        let slots = graph.len();
        Job {
            input,
            crates,
            shared,
            graph,
            identities: (0..slots).map(|_| OnceLock::new()).collect(),
            scripts: (0..slots).map(|_| OnceLock::new()).collect(),
            made: (0..slots).map(|_| OnceLock::new()).collect(),
            keys: (0..slots).map(|_| OnceLock::new()).collect(),
            runs: (0..slots).map(|_| OnceLock::new()).collect(),
            mounted,
            inert: (0..slots).map(|_| OnceLock::new()).collect(),
            starts: (0..slots).map(|_| Mutex::new(None)).collect(),
            build_time: OnceLock::new(),
            ub: OnceLock::new(),
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

    fn compile(&self, reader: Reader, unit: usize, images: &mut Vec<Image>) -> Result<Compiled, Failed> {
        let reader: UnitReader = reader.take();
        if self.0.cancelled() {
            return Err(Failed::Again);
        }
        cancellable(|| compile::compile(&self.0, &reader, unit, images))?
    }

    fn script(&self, unit: usize, tags: u64) -> Result<Box<dyn Runs>, ApiError> {
        script::start(&self.0, unit, tags)
    }

    fn tests(&self, built: &[Artifact], selection: Selection<'_>, tags: u64) -> Result<Box<dyn Runs>, ApiError> {
        test::start(&self.0, built, selection, tags)
    }

    fn ub_tests(&self, reader: Reader, selection: Selection<'_>) -> Result<Vec<String>, Failed> {
        let reader: UnitReader = reader.take();
        cancellable(|| test::ub_tests(&self.0, &reader, selection))?
    }

    fn evaluate(&self, reader: Reader, test: &str, ub: Ub) -> Result<TestResult, Failed> {
        let reader: UnitReader = reader.take();
        if self.0.cancelled() {
            return Err(Failed::Again);
        }
        cancellable(|| test::evaluate(&self.0, reader.db(), test, ub))?
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

    fn reach(
        &self,
        reader: Reader,
        from: &dyn Front,
        from_reader: Reader,
        changes: &[blaze_vfs::Change],
    ) -> Result<Reach, Failed> {
        let Some(from) = from.as_any().downcast_ref::<JobFront>() else {
            return Err(Failed::Error(ApiError::new(
                "the view to reach from was not lowered by this daemon's build",
                None,
            )));
        };
        let after: UnitReader = reader.take();
        let before: UnitReader = from_reader.take();
        if self.0.cancelled() {
            return Err(Failed::Again);
        }
        cancellable(|| reach::reach(&from.0, before.db(), &self.0, after.db(), changes))?
    }

    fn codegen_inputs(&self, reader: Reader) -> Result<Vec<(ContentId, ContentId)>, Failed> {
        // Read from the units' records in the store: the front end is not asked.
        drop(reader.take::<UnitReader>());
        cancellable(|| compile::codegen_inputs(&self.0))?
    }

    fn tables(&self, mut writer: Box<dyn Writer>, images: &[(&'static str, u64, u64)]) -> Result<Vec<Table>, Failed> {
        let mut counted = writer.tables()?;
        counted.extend_from_slice(images);
        Ok(summed(counted))
    }
}

impl Kept for codegen::Linked {
    fn tables(&self) -> Vec<(&'static str, u64, u64)> {
        codegen::Linked::tables(self)
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any + Send> {
        self
    }
}

/// The front end's tables (tokens, syntax nodes, HIR expressions, MIR statements, memos and
/// their edges, each as its owner counts it) and the linker's (relocations and the rest),
/// summed by name and sorted by it.
fn summed(mut counted: Vec<(&'static str, u64, u64)>) -> Vec<Table> {
    counted.sort_unstable_by_key(|it| it.0);
    let mut tables: Vec<Table> = Vec::new();
    for (name, elements, bytes) in counted {
        match tables.last_mut() {
            Some(table) if table.name == name => {
                table.elements += elements;
                table.bytes += bytes;
            }
            _ => tables.push(Table { name: name.to_owned(), elements, bytes }),
        }
    }
    tables
}

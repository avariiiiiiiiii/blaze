//! A build script's run ([`Front::script`](crate::serve::work::Front::script)), as cargo runs
//! it and reads what it printed (`cargo/src/cargo/core/compiler/custom_build.rs`), in the
//! sandbox: what it is given (the job's whole environment, cargo's variables over it, the
//! `DEP_<links>_<KEY>` of the runs it depends on), its result kept in the store by what it read
//! ([`blaze_usercode::run`]), a script that provably prints only constants not run at all
//! ([`blaze_usercode::script::inert`]), and what it printed applied as cargo applies it: its
//! cfgs and env to its package's units through the front end ([`Lowered::ran`]), its link
//! libraries and search paths to what links them, its metadata to its dependents' scripts.
//!
//! A run outside the sandbox is one `BLAZE_SANDBOX=off` or the workspace's
//! `unsandboxed-build-scripts` (a member's `build-script = false`) takes out: it runs every
//! time, and nothing of it is kept.
//!
//! [`Lowered::ran`]: load_cargo::v2::Lowered::ran

use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

use blaze_api::{ApiError, BuildScriptRan, Diagnostic, Level, Response, UnitOf};
use blaze_build_script::{
    output::{Context, Output, Severity},
    unit as bs,
};
use blaze_cas::Store;
use blaze_core::{Command, Completion, Done, Effect, Sandbox, Tag};
use blaze_units::{
    query::{UnitPlan, Units},
    schema::{DebugInfo, Mode, Unit},
};
use blaze_usercode::{
    run::{Begun, Run, Value},
    script::{self, Place, Prepared},
};

use super::{Job, package_of, say};
use crate::serve::work::Runs;

/// What a build script's run told cargo, for what depends on it.
#[derive(Clone, Debug)]
pub(crate) struct Applied {
    /// The run's package.
    pub(crate) package_id: String,
    pub(crate) links: Option<String>,
    pub(crate) output: Output,
    pub(crate) out_dir: String,
}

/// The unit that compiles run `run`'s script: its dependency that builds the custom-build
/// target.
pub(crate) fn compile_of(units: &Units, run: usize) -> Option<usize> {
    let unit = units.graph.units.get(run)?;
    unit.dependencies.iter().map(|it| it.index).find(|at| {
        units.graph.units.get(*at).is_some_and(|it| it.target.is_custom_build() && !matches!(it.mode, Mode::RunCustomBuild))
    })
}

/// What a script that provably prints only constants prints, read from its source through the
/// VFS: it is then neither compiled nor run. `unit` is the script's compile or its run.
pub(crate) fn inert(job: &Job, unit: usize) -> Option<Value> {
    let units = job.units();
    let u = units.graph.units.get(unit)?;
    let compile = match u.mode {
        Mode::RunCustomBuild => compile_of(units, unit)?,
        _ => unit,
    };
    let root = units.graph.units.get(compile)?.target.src_path.to_string_lossy().into_owned();
    let mounted = super::files::Mounted::new(
        job.input.files.clone(),
        job.input.root,
        &job.input.config.mounts,
        &job.input.config.invocation.root,
    );
    let (_, _, bytes) = mounted.read(&root)?;
    let files: BTreeMap<PathBuf, Vec<u8>> = [(PathBuf::from(&root), bytes.to_vec())].into();
    script::inert(Path::new(&root), &files, &units.plans.get(compile)?.rustflags)
}

/// What a run is made from, beside the job: the build, and what the run's dependencies said.
pub(crate) struct Of<'a> {
    pub(crate) units: &'a Units,
    pub(crate) host: &'a str,
    /// Where blaze's stand-ins for cargo's tools are: `rustc`, `rustdoc`.
    pub(crate) tools: &'a Path,
    pub(crate) launcher: &'a Path,
    /// The compiled script.
    pub(crate) program: PathBuf,
    pub(crate) scripts: &'a BTreeMap<usize, Applied>,
}

fn var<'a>(plan: &'a UnitPlan, name: &str) -> Option<&'a str> {
    plan.env.iter().find(|(it, _)| it == name).map(|(_, value)| value.as_str())
}

/// The run of build script unit `run`, as `blaze_build_script` describes it.
pub(crate) fn script_unit(of: &Of<'_>, run: usize) -> Result<bs::Unit, String> {
    let units = of.units;
    let unit = units.graph.units.get(run).ok_or_else(|| format!("the build has no unit {run}"))?;
    let plan = units.plans.get(run).ok_or_else(|| format!("the build has no plan of unit {run}"))?;
    let package = package_of(units, unit).map_err(|it| it.what)?;
    let out_dir = plan
        .out_dir
        .clone()
        .ok_or_else(|| format!("the run of `{}`'s build script has no OUT_DIR", package.name))?;
    let manifest_path = PathBuf::from(&package.manifest_path);
    let manifest_dir = manifest_path.parent().map(Path::to_path_buf).unwrap_or_default();
    let targets = targets(units, &unit.pkg_id);
    let text = |name: &str| var(plan, name).filter(|it| !it.is_empty()).map(str::to_owned);
    let descr = match &package.source {
        Some(source) if super::compile::local(Some(source)) => {
            format!("{} v{} {source}", package.name, package.version)
        }
        _ => format!("{} v{}", package.name, package.version),
    };
    let target = triple_of(unit, of.host).to_owned();
    let cfg_args = blaze_units::stdlib::target::CfgArgs::from_flags(&plan.rustflags)?;
    let target_cfgs = blaze_units::stdlib::target::target_cfg(&target, &cfg_args)?;
    let profile = &unit.profile;
    let release = var(plan, "PROFILE").map_or(profile.name == "release", |it| it == "release");
    let debuginfo = !matches!(&profile.debuginfo, DebugInfo::Level(0))
        && !matches!(&profile.debuginfo, DebugInfo::Named(name) if name == "none" || name == "false");
    let dependencies = unit
        .dependencies
        .iter()
        .filter_map(|dep| of.scripts.get(&dep.index).map(|applied| (dep, applied)))
        .map(|(dep, applied)| {
            let name = units
                .graph
                .units
                .get(dep.index)
                .and_then(|it| package_of(units, it).ok())
                .map_or_else(|| dep.extern_crate_name.clone(), |it| it.name.clone());
            bs::Dependency::of(&name, applied.links.as_deref(), &applied.output)
        })
        .collect();
    // `<profile>/build/<package>-<hash>/out`, or `<profile>/build/<package>/<hash>/out`.
    let profile_dir = Path::new(&out_dir)
        .ancestors()
        .find(|it| it.file_name().is_some_and(|name| name == "build"))
        .and_then(Path::parent)
        .map(Path::to_path_buf);
    let exe = |name: &str| of.tools.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    Ok(bs::Unit {
        package: bs::Package {
            name: package.name.clone(),
            version: package.version.clone(),
            descr: Some(descr),
            manifest_dir,
            manifest_path,
            authors: var(plan, "CARGO_PKG_AUTHORS")
                .map(|it| it.split(':').filter(|it| !it.is_empty()).map(str::to_owned).collect())
                .unwrap_or_default(),
            description: text("CARGO_PKG_DESCRIPTION"),
            homepage: text("CARGO_PKG_HOMEPAGE"),
            repository: text("CARGO_PKG_REPOSITORY"),
            license: text("CARGO_PKG_LICENSE"),
            license_file: text("CARGO_PKG_LICENSE_FILE"),
            readme: text("CARGO_PKG_README"),
            rust_version: text("CARGO_PKG_RUST_VERSION"),
            links: plan.links.clone(),
            targets,
        },
        features: unit.features.clone(),
        profile: bs::Profile {
            name: profile.name.clone(),
            release,
            opt_level: profile.opt_level.clone(),
            debuginfo,
            debug_assertions: profile.debug_assertions,
        },
        target,
        host: of.host.to_owned(),
        target_cfgs,
        rustflags: plan.rustflags.clone(),
        jobs: std::thread::available_parallelism().map_or(1, |it| it.get() as u32),
        script: of.program.clone(),
        out_dir: PathBuf::from(&out_dir),
        tools: bs::Tools {
            cargo: of.launcher.to_path_buf(),
            rustc: exe("rustc"),
            rustdoc: exe("rustdoc"),
            rustc_wrapper: None,
            rustc_workspace_wrapper: None,
            linker: None,
        },
        workspace_member: package.is_member,
        dependencies,
        config_env: Vec::new(),
        dylib_search: bs::DylibSearch {
            host_root: profile_dir.clone(),
            host_deps: profile_dir.iter().map(|it| it.join("deps")).collect(),
            sysroot_libdir: PathBuf::new(),
            plugin_search: Vec::new(),
        },
        jobserver: None,
    })
}

/// The target of `unit`, as `blaze_build_script` names it.
pub(crate) fn target_of(unit: &Unit) -> bs::Target {
    bs::Target {
        name: unit.target.name.clone(),
        kind: unit.target.kind.clone(),
        crate_types: unit.target.crate_types.clone(),
    }
}

/// The targets of package `pkg_id` the build has a unit of, each once, in the graph's order.
pub(crate) fn targets(units: &Units, pkg_id: &str) -> Vec<bs::Target> {
    let mut all = Vec::new();
    for it in units.graph.units.iter().filter(|it| it.pkg_id == pkg_id).map(target_of) {
        if !all.contains(&it) {
            all.push(it);
        }
    }
    all
}

fn triple_of<'a>(unit: &'a Unit, host: &'a str) -> &'a str {
    unit.platform.as_deref().unwrap_or(host)
}

/// The environment a script is started in: the job's whole environment, what cargo sets for
/// the unit over it.
pub(crate) fn inherited(job: &[(String, String)], plan: &UnitPlan) -> Vec<(OsString, OsString)> {
    let mut env: BTreeMap<String, String> = job.iter().cloned().collect();
    for (name, value) in &plan.env {
        env.insert(name.clone(), value.clone());
    }
    env.into_iter().map(|(name, value)| (OsString::from(name), OsString::from(value))).collect()
}

/// How a run ended, judged as cargo judges it.
#[derive(Debug)]
pub(crate) struct Judged {
    pub(crate) output: Option<Output>,
    /// cargo's lines of it: its warnings, and why it failed.
    pub(crate) diagnostics: Vec<Diagnostic>,
}

/// A process's end as an `ExitStatus`, for cargo's text of it.
fn status(code: Option<i32>, signal: Option<i32>) -> std::process::ExitStatus {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        match (code, signal) {
            (_, Some(signal)) => std::process::ExitStatus::from_raw(signal & 0x7f),
            (Some(code), None) => std::process::ExitStatus::from_raw((code & 0xff) << 8),
            (None, None) => std::process::ExitStatus::from_raw(0x7f),
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(code.unwrap_or(1) as u32)
    }
}

/// What a sandboxed script may print, checked before cargo reads it
/// ([`crate::cargo::directives`]); `None` when it may print it all.
pub(crate) fn refused(unit: &bs::Unit, stdout: &[u8], link_dirs: &[PathBuf]) -> Option<String> {
    let ran = crate::cargo::directives::Ran {
        package_dir: &unit.package.manifest_dir,
        out_dir: &unit.out_dir,
        link_dirs,
    };
    let refused = crate::cargo::directives::refused(stdout, &ran);
    (!refused.is_empty()).then(|| crate::cargo::directives::refused_error(&unit.package.name, &refused))
}

/// `value`, a run of `unit` started in `env`, as cargo makes of it: what it printed, once it
/// succeeded, with its warnings; or why the build fails, with the output cargo shows.
pub(crate) fn judge(
    unit: &bs::Unit,
    env: &[(OsString, OsString)],
    value: &Value,
    began: std::time::SystemTime,
    checked: Option<&[PathBuf]>,
    of: Option<UnitOf>,
    color: bool,
) -> Judged {
    let ran = blaze_build_script::run::Ran {
        began,
        status: status(value.code, value.signal),
        stdout: value.stdout.clone(),
        stderr: value.stderr.clone(),
    };
    let cx = Context { nightly_features_allowed: true, ..Context::of(unit, env) };
    let line = |severity: &Severity, message: &str| {
        let level = match severity {
            Severity::Error => Level::Error,
            Severity::Warning => Level::Warning,
        };
        say::cargo(level, format!("{}@{}: {message}", unit.package.name, unit.package.version), of.clone(), color)
    };
    let failure = |error: String| {
        let what = match error.starts_with("failed to run custom build command") {
            true => error,
            false => format!(
                "failed to run custom build command for `{}`\n\nCaused by:\n  {}",
                unit.package.descr(),
                error.lines().collect::<Vec<_>>().join("\n  ")
            ),
        };
        say::cargo(Level::Error, what, of.clone(), color)
    };
    match ran.finish(unit, &cx) {
        Ok(output) => {
            let mut diagnostics: Vec<Diagnostic> =
                output.log_messages.iter().map(|(severity, message)| line(severity, message)).collect();
            if let Some(link_dirs) = checked
                && let Some(why) = refused(unit, &value.stdout, link_dirs)
            {
                diagnostics.push(say::refused(why, of.clone(), color));
                return Judged { output: None, diagnostics };
            }
            Judged { output: Some(output), diagnostics }
        }
        Err(failed) => {
            let mut diagnostics: Vec<Diagnostic> =
                failed.log_messages.iter().map(|(severity, message)| line(severity, message)).collect();
            diagnostics.push(failure(failed.error.0));
            Judged { output: None, diagnostics }
        }
    }
}

/// What cargo's `build-script-executed` says of a run.
pub(crate) fn executed(package_id: &str, output: &Output, out_dir: &str) -> BuildScriptRan {
    BuildScriptRan {
        package_id: package_id.to_owned(),
        linked_libs: output.library_links.clone(),
        linked_paths: output.library_paths.iter().map(|it| it.path().to_string_lossy().into_owned()).collect(),
        cfgs: output.cfgs.clone(),
        env: output.env.clone(),
        out_dir: out_dir.to_owned(),
    }
}

/// The files a run left in its `OUT_DIR`, by `/`-separated path under it: those the store's
/// value has, else as the run wrote them.
fn out_files(value: &Value, out_dir: &Path) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut files = Vec::new();
    for file in &value.files {
        let bytes = match &file.bytes {
            Some(bytes) => bytes.clone(),
            None => std::fs::read(out_dir.join(&file.path))
                .map_err(|error| format!("{}: {error}", out_dir.join(&file.path).display()))?,
        };
        files.push((file.path.clone(), bytes));
    }
    Ok(files)
}

/// Every file under `dir`, by `/`-separated path: what a run outside the sandbox left.
fn walk(dir: &Path) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut out = Vec::new();
    let mut pending = vec![PathBuf::new()];
    while let Some(rel) = pending.pop() {
        let at = dir.join(&rel);
        let entries = match std::fs::read_dir(&at) {
            Ok(it) => it,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && rel.as_os_str().is_empty() => return Ok(out),
            Err(error) => return Err(format!("{}: {error}", at.display())),
        };
        for entry in entries {
            let entry = entry.map_err(|error| format!("{}: {error}", at.display()))?;
            let rel = rel.join(entry.file_name());
            let kind = entry.file_type().map_err(|error| format!("{}: {error}", entry.path().display()))?;
            if kind.is_dir() {
                pending.push(rel);
            } else if kind.is_file() {
                let bytes = std::fs::read(entry.path()).map_err(|error| format!("{}: {error}", entry.path().display()))?;
                out.push((rel.to_string_lossy().replace('\\', "/"), bytes));
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Whether the store has run `unit`'s result for its inputs as they are now, or the script is
/// inert; false when that is not known.
pub(crate) fn fresh(job: &Job, unit: usize) -> bool {
    let Ok(Some(start)) = prepare(job, unit) else { return false };
    match &start.how {
        How::Inert(_) => true,
        How::Outside(_) => false,
        How::Sandboxed(prepared) => {
            let store = job.input.store.read().unwrap_or_else(|it| it.into_inner());
            let mut disk = job.shared.disk.lock().unwrap_or_else(|it| it.into_inner());
            disk.set_env(prepared.env.clone());
            matches!(
                blaze_usercode::index::find(&store, &prepared.spec.key, &mut *disk),
                Ok(blaze_usercode::index::Found::Hit { .. })
            )
        }
    }
}

/// A run ready to begin.
struct Start {
    unit: bs::Unit,
    /// The environment it is started in, which its output is read with.
    env: Vec<(OsString, OsString)>,
    how: How,
}

enum How {
    /// It prints only this: not run.
    Inert(Value),
    /// In the sandbox, its result kept by what it read.
    Sandboxed(Prepared),
    /// Outside the sandbox (`BLAZE_SANDBOX=off`, `unsandboxed-build-scripts`): every time,
    /// nothing kept.
    Outside(Command),
}

/// Whether run `run` is outside the sandbox: `BLAZE_SANDBOX=off`, or the workspace took its
/// package out by name.
fn outside(job: &Job, run: usize) -> bool {
    job.sandbox_off() || job.plan(run).is_ok_and(|it| it.unsandboxed)
}

/// Run `run` ready to begin; none while its script is not built.
fn prepare(job: &Job, run: usize) -> Result<Option<Start>, ApiError> {
    let units = job.units();
    let Some(compile) = compile_of(units, run) else {
        return Err(ApiError::new(format!("the run of unit {run} has no build script compiled"), None));
    };
    let inert = inert(job, run);
    let program = match &inert {
        // Never compiled: its program is named, not there.
        Some(_) => None,
        None => {
            let made = job.made.lock().unwrap_or_else(|it| it.into_inner());
            match made.get(&compile).and_then(|it| it.main.clone()) {
                Some(it) => Some(it),
                None => return Ok(None),
            }
        }
    };
    let scripts = job.scripts.lock().unwrap_or_else(|it| it.into_inner());
    let script_path = match &program {
        Some((path, _)) => PathBuf::from(path),
        None => PathBuf::from(&units.plans[compile].outputs.first().cloned().unwrap_or_default()),
    };
    let of = Of {
        units,
        host: &job.input.config.host,
        tools: &job.shared.tools,
        launcher: &job.shared.launcher,
        program: script_path,
        scripts: &scripts,
    };
    let unit = script_unit(&of, run).map_err(|what| ApiError::new(what, Some(format!("the build script of unit {run}"))))?;
    drop(scripts);
    let env = inherited(&job.input.config.invocation.env, job.plan(run)?);
    let (Some((_, code)), None) = (program, inert.clone()) else {
        let value = inert.ok_or_else(|| ApiError::new("a build script neither built nor inert", None))?;
        return Ok(Some(Start { unit, env, how: How::Inert(value) }));
    };
    let mut read = vec![
        unit.package.manifest_dir.clone(),
        PathBuf::from(&job.input.config.invocation.root),
        PathBuf::from(&units.target_dir),
        job.shared.launcher.clone(),
        job.shared.tools.clone(),
    ];
    read.dedup();
    let place = Place { code, read, network: false, memory: None, mode: blaze_usercode::run::Mode::Normal };
    let prepared = script::prepare(&unit, &env, &place)
        .map_err(|error| ApiError::new(error.0, Some(unit.package.descr())))?;
    let how = match outside(job, run) {
        true => How::Outside(Command { sandbox: Sandbox::Off, ..prepared.spec.command }),
        false => How::Sandboxed(prepared),
    };
    Ok(Some(Start { unit, env, how }))
}

/// A build script's run, on the loop.
struct ScriptRun {
    job: Arc<Job>,
    unit: usize,
    tag: Tag,
    state: State,
}

enum State {
    Ready(Box<Start>),
    Running { start: Box<Start>, run: Option<Run>, began: SystemTime, cancelled: bool },
    Done,
}

pub(crate) fn start(job: &Arc<Job>, unit: usize, tags: u64) -> Result<Box<dyn Runs>, ApiError> {
    let u = job.unit(unit)?;
    if !matches!(u.mode, Mode::RunCustomBuild) {
        return Err(ApiError::new(format!("unit {unit} (`{}`) is no build script's run", u.target.name), None));
    }
    let Some(start) = prepare(job, unit)? else {
        return Err(ApiError::new(format!("the build script of `{}` was not built before its run", u.target.name), None));
    };
    Ok(Box::new(ScriptRun { job: job.clone(), unit, tag: Tag(tags), state: State::Ready(Box::new(start)) }))
}

impl ScriptRun {
    fn of(&self) -> Option<UnitOf> {
        self.job.unit(self.unit).ok().and_then(|it| self.job.unit_of(it).ok())
    }

    fn error(&mut self, what: String, out: &mut Vec<Response>) -> Option<bool> {
        out.push(Response::Diagnostic(say::refused(what, self.of(), self.job.input.color)));
        self.state = State::Done;
        Some(false)
    }

    /// The run's end: judged, its warnings said, what it printed applied.
    fn finish(&mut self, start: &Start, value: &Value, began: SystemTime, out: &mut Vec<Response>) -> Option<bool> {
        let job = self.job.clone();
        let link_dirs: Vec<PathBuf> = job.units().link_dirs.iter().map(PathBuf::from).collect();
        // What a run outside the sandbox prints is not checked: it could do anything anyway.
        let checked = match start.how {
            How::Outside(_) => None,
            _ => Some(link_dirs.as_slice()),
        };
        let judged = judge(&start.unit, &start.env, value, began, checked, self.of(), job.input.color);
        out.extend(judged.diagnostics.into_iter().map(Response::Diagnostic));
        let Some(output) = judged.output else {
            self.state = State::Done;
            return Some(false);
        };
        let out_dir = start.unit.out_dir.to_string_lossy().into_owned();
        let files = match start.how {
            How::Inert(_) => Ok(Vec::new()),
            How::Sandboxed(_) => out_files(value, &start.unit.out_dir),
            How::Outside(_) => walk(&start.unit.out_dir),
        };
        let files = match files {
            Ok(it) => it,
            Err(what) => return self.error(what, out),
        };
        let ran = load_cargo::v2::Ran { cfgs: output.cfgs.clone(), env: output.env.clone(), out_dir: out_dir.clone(), files };
        if let Err(error) = job.lowered.ran(self.unit, &ran) {
            return self.error(error.what, out);
        }
        let package_id = job.unit(self.unit).map(|it| it.pkg_id.clone()).unwrap_or_default();
        out.push(Response::BuildScript(executed(&package_id, &output, &out_dir)));
        let links = start.unit.package.links.clone();
        let applied = Applied { package_id, links, output, out_dir };
        job.scripts.lock().unwrap_or_else(|it| it.into_inner()).insert(self.unit, applied);
        self.state = State::Done;
        Some(true)
    }
}

impl Runs for ScriptRun {
    fn start(&mut self, store: &Store, effects: &mut Vec<Effect>, out: &mut Vec<Response>) -> Option<bool> {
        let State::Ready(start) = std::mem::replace(&mut self.state, State::Done) else { return None };
        // cargo's start of a run: `OUT_DIR` made, and the time it began.
        let stdout = match &start.how {
            How::Inert(value) => value.stdout.clone(),
            _ => Vec::new(),
        };
        let began = match blaze_build_script::run::elided(&start.unit, stdout) {
            Ok(it) => it.began,
            Err(error) => return self.error(error.0, out),
        };
        match &start.how {
            How::Inert(value) => {
                let value = value.clone();
                self.finish(&start, &value, began, out)
            }
            How::Outside(command) => {
                effects.push(Effect::Spawn { command: Box::new(command.clone()), tag: self.tag });
                self.state = State::Running { start, run: None, began, cancelled: false };
                None
            }
            How::Sandboxed(prepared) => {
                let mut disk = self.job.shared.disk.lock().unwrap_or_else(|it| it.into_inner());
                disk.set_env(prepared.env.clone());
                let begun = Run::begin(prepared.spec.clone(), store, &mut *disk, self.tag, effects);
                drop(disk);
                match begun {
                    Ok(Begun::Reused(value)) => self.finish(&start, &value, began, out),
                    Ok(Begun::Running(run)) => {
                        self.state = State::Running { start, run: Some(run), began, cancelled: false };
                        None
                    }
                    Err(error) => {
                        let what = format!("the build script of `{}`: {error}", start.unit.package.descr());
                        self.error(what, out)
                    }
                }
            }
        }
    }

    fn owns(&self, tag: Tag) -> bool {
        tag.0 >= self.tag.0 && tag.0 - self.tag.0 < 1 << 32
    }

    fn complete(
        &mut self,
        completion: Completion,
        store: &mut Store,
        _effects: &mut Vec<Effect>,
        out: &mut Vec<Response>,
    ) -> Option<bool> {
        let State::Running { start, run, began, cancelled } = std::mem::replace(&mut self.state, State::Done) else {
            return None;
        };
        if cancelled {
            // A run killed is no result: nothing of it is kept or applied.
            return Some(false);
        }
        let value = match (run, &start.how) {
            (Some(run), How::Sandboxed(prepared)) => {
                let mut disk = self.job.shared.disk.lock().unwrap_or_else(|it| it.into_inner());
                disk.set_env(prepared.env.clone());
                let completed = run.complete(
                    completion,
                    store,
                    &mut *disk,
                    |exit| script::cargo_rule(&start.unit, &start.env, exit),
                    |traced| script::keyed(prepared, traced),
                );
                drop(disk);
                match completed {
                    Ok((value, _, _)) => value,
                    Err(error) => {
                        let what = format!("the build script of `{}`: {error}", start.unit.package.descr());
                        return self.error(what, out);
                    }
                }
            }
            _ => match completion.done {
                Done::Exited(exit) => {
                    Value { code: exit.code, signal: exit.signal, stdout: exit.stdout, stderr: exit.stderr, files: Vec::new() }
                }
                Done::Failed(error) => {
                    let what = format!(
                        "failed to run custom build command for `{}`\n\nCaused by:\n  could not execute process `{}` \
                         (never executed)\n\nCaused by:\n  {error}",
                        start.unit.package.descr(),
                        start.unit.script.display()
                    );
                    return self.error(what, out);
                }
                other => return self.error(format!("the build script's spawn ended as {other:?}"), out),
            },
        };
        self.finish(&start, &value, began, out)
    }

    fn cancel(&mut self, effects: &mut Vec<Effect>) -> Option<bool> {
        match &mut self.state {
            State::Running { cancelled, .. } => {
                *cancelled = true;
                effects.push(Effect::Kill { tag: self.tag });
                None
            }
            _ => {
                self.state = State::Done;
                Some(false)
            }
        }
    }
}

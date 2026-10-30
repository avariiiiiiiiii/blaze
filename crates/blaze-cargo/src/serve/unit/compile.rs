//! One unit's compile ([`Front::compile`](crate::serve::work::Front::compile)): its
//! diagnostics as rustc gives them and cargo shows them, then, unless it is a check, its code
//! compiled and linked by blaze, and every file cargo would write for it.
//!
//! A unit's result is kept in the store as its record, under what it was made from: the
//! front end's identity of the unit's inputs (its sources, cfgs, env, its dependencies' and
//! their build scripts' results, deeply), its plan, its profile and target, the toolchain and
//! blaze's own code ([`Toolchain::id`](blaze_codegen::settings::Toolchain)), the goal and the
//! colour of its diagnostics. A unit whose record and outputs are all in the store is fresh:
//! its diagnostics are replayed as cargo replays a fresh unit's, and nothing runs.

use std::collections::BTreeSet;

use blaze_api::{ApiError, Artifact, Diagnostic, Level};
use blaze_cas::{Key, Kind};
use blaze_diagnostics::{Options, lints::CommandLine};
use blaze_ids::{ContentId, Hasher};
use blaze_items::CodeHash;
use blaze_units::schema::Unit;
use hir_ty::db::HirDatabase;
use serde::{Deserialize, Serialize};

use super::{
    Job, artifact, checks_only,
    codegen::{self, Makes},
    files::{Mounted, Names},
    say, script,
};
use crate::serve::work::{Compiled, Failed, Goal, Output, Put};

/// What a compiled unit made that later work of the job reads.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
pub(crate) struct Made {
    /// Its main file and content: a build script's program, a test binary, a library.
    pub(crate) main: Option<(String, ContentId)>,
    /// A test binary's tests, in the harness's order.
    pub(crate) tests: Vec<TestOf>,
    /// What the binary does depends on beside the code it reaches: its target, toolchain,
    /// codegen settings and panic strategy ([`blaze_usercode::test::Suite::context`]).
    pub(crate) context: Option<ContentId>,
}

/// A test of a test binary, as selection sees it.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub(crate) struct TestOf {
    pub(crate) name: String,
    pub(crate) ignore: bool,
    /// `#[should_panic]`, with its `expected` message if it gives one.
    pub(crate) should_panic: Option<Option<String>>,
    pub(crate) bench: bool,
    /// It reaches the outside world, as codegen marks it: it always runs.
    pub(crate) outside: bool,
    /// The code hashes of what it reaches, in the walk's order.
    pub(crate) reached: Vec<CodeHash>,
}

/// A unit's result as the store keeps it.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
struct Record {
    ok: bool,
    diagnostics: Vec<Diagnostic>,
    outputs: Vec<(String, Key, bool)>,
    artifact: Option<Artifact>,
    made: Made,
    /// Every native input the link read off the machine, by path, with its content: a unit
    /// whose system library changed is not fresh.
    natives: Vec<(String, ContentId)>,
}

const RECORD: &str = "blaze v2 cargo unit record 1";
const OUTPUT: &str = "blaze v2 cargo output 1";
const NATIVE: &str = "blaze v2 native input 1";

/// The key of `unit`'s record: none while the front end can't say what the unit's inputs are.
fn record_key(job: &Job, unit: usize) -> Result<Option<Key>, ApiError> {
    let Some(identity) = job.lowered.identity(unit) else { return Ok(None) };
    let u = job.unit(unit)?;
    let plan = job.plan(unit)?;
    let toolchain = codegen::toolchain(job, unit).map_err(|what| ApiError::new(what, Some(u.target.name.clone())))?;
    let goal: u8 = match job.input.goal {
        Goal::Check => 0,
        Goal::Build => 1,
        Goal::Test => 2,
        Goal::Run => 3,
        Goal::Reach => 4,
    };
    let mut hasher = Hasher::new(RECORD);
    hasher
        .id(&identity)
        .part(&cbor(u, "unit", unit)?)
        .part(&cbor(plan, "plan", unit)?)
        .part(&cbor(&runs(job, unit).env, "run environment", unit)?)
        .id(&toolchain.id)
        .update(&[goal, u8::from(job.input.color)]);
    Ok(Some(Key(hasher.finish())))
}

fn cbor<T: Serialize>(value: &T, what: &str, unit: usize) -> Result<Vec<u8>, ApiError> {
    blaze_protocol::cbor::to_vec(value)
        .map_err(|why| ApiError::new(format!("encoding the {what} of unit {unit}: {why}"), None))
}

/// The record of `unit` the store has, every output and native input of it as it was.
fn kept(job: &Job, unit: usize) -> Option<Record> {
    let key = record_key(job, unit).ok()??;
    let store = job.input.store.read().unwrap_or_else(|it| it.into_inner());
    let entry = store.get(&key).ok()??;
    let record: Record = blaze_protocol::cbor::from_slice(entry.bytes).ok()?;
    if !record.outputs.iter().all(|(_, key, _)| store.has(key)) {
        return None;
    }
    let same = |(path, id): &(String, ContentId)| {
        std::fs::read(path).is_ok_and(|bytes| ContentId::of(NATIVE, &bytes) == *id)
    };
    record.natives.iter().all(same).then_some(record)
}

/// Whether nothing of `unit` will be made again: its record and all it wrote are in the store.
pub(crate) fn fresh(job: &Job, unit: usize) -> bool {
    kept(job, unit).is_some()
}

/// `unit` checked and, unless it is a check, compiled and linked.
pub(crate) fn compile(job: &Job, unit: usize) -> Result<Compiled, Failed> {
    if let Some(record) = kept(job, unit) {
        let artifact = record.artifact.map(|it| Artifact { fresh: true, ..it });
        job.made.lock().unwrap_or_else(|it| it.into_inner()).insert(unit, record.made);
        return Ok(Compiled {
            diagnostics: record.diagnostics,
            ok: record.ok,
            puts: Vec::new(),
            outputs: record
                .outputs
                .into_iter()
                .map(|(path, key, executable)| Output { path, key, executable })
                .collect(),
            artifact,
        });
    }
    let made = make(job, unit)?;
    let key = record_key(job, unit)?;
    let Made2 { compiled, made, natives } = made;
    job.made.lock().unwrap_or_else(|it| it.into_inner()).insert(unit, made.clone());
    let mut compiled = compiled;
    if let Some(key) = key {
        let record = Record {
            ok: compiled.ok,
            diagnostics: compiled.diagnostics.clone(),
            outputs: compiled.outputs.iter().map(|it| (it.path.clone(), it.key, it.executable)).collect(),
            artifact: compiled.artifact.clone(),
            made,
            natives,
        };
        let bytes = cbor(&record, "record", unit)?;
        compiled.puts.push(Put { key, kind: Kind::Query, bytes });
    }
    Ok(compiled)
}

struct Made2 {
    compiled: Compiled,
    made: Made,
    natives: Vec<(String, ContentId)>,
}

/// The lint flags of `unit`'s rustc: cargo's `[lints]`, those of its rustflags, its
/// `--check-cfg`s, and `--cap-lints allow` for a package cargo shows no warnings of (one not
/// from a path, and the standard library).
pub(crate) fn lint_flags(unit: &Unit, plan: &blaze_units::query::UnitPlan, local: bool) -> Vec<String> {
    let mut out = plan.lints.clone();
    let mut flags = plan.rustflags.iter();
    while let Some(flag) = flags.next() {
        let short = ["-A", "-W", "-D", "-F"];
        let long = ["--allow", "--warn", "--deny", "--forbid", "--force-warn", "--cap-lints"];
        let takes_next = short.contains(&flag.as_str()) || long.contains(&flag.as_str());
        if takes_next {
            out.push(flag.clone());
            if let Some(value) = flags.next() {
                out.push(value.clone());
            }
        } else if short.iter().any(|it| flag.starts_with(it)) || long.iter().any(|it| flag.starts_with(&format!("{it}="))) {
            out.push(flag.clone());
        }
    }
    for spec in &plan.check_cfg {
        out.push("--check-cfg".to_owned());
        out.push(spec.clone());
    }
    if unit.is_std || !local {
        out.push("--cap-lints".to_owned());
        out.push("allow".to_owned());
    }
    out
}

/// Whether cargo shows a package's warnings: one from a path (`(/abs/path)`).
pub(crate) fn local(source: Option<&str>) -> bool {
    source.is_some_and(|it| {
        let path = it.trim_start_matches('(');
        path.starts_with('/') || path.get(1..3) == Some(":\\") || path.starts_with("\\\\")
    })
}

/// Whether a diagnostic is one of rustc's closing lines cargo does not show: "aborting due to
/// ...", "N warnings emitted" (`cargo/src/cargo/core/compiler/mod.rs`, `on_stderr_line_inner`).
pub(crate) fn cargo_hides(message: &str) -> bool {
    message.starts_with("aborting due to")
        || message.ends_with("warning emitted")
        || message.ends_with("warnings emitted")
}

fn mounted(job: &Job) -> Mounted {
    let config = &job.input.config;
    Mounted::new(job.input.files.clone(), job.input.root, &config.mounts, &config.invocation.root)
}

fn make(job: &Job, unit: usize) -> Result<Made2, Failed> {
    let u = job.unit(unit)?;
    let plan = job.plan(unit)?;
    let package = job.package(u)?;
    let unit_of = job.unit_of(u)?;
    let color = job.input.color;
    let krate = job.lowered.krate(unit).ok_or_else(|| {
        ApiError::new(format!("unit {unit} (`{}`) has no crate in the front end", u.target.name), None)
    })?;
    let mounted = mounted(job);
    let names = Names::new(&job.lowered, &mounted);
    let lints = CommandLine::parse(&lint_flags(u, plan, local(package.source.as_deref())))
        .map_err(|why| ApiError::new(why, Some(format!("the lint flags of `{}`", u.target.name))))?;
    let options = Options { lints, ..Options::default() };
    let snapshot = job.lowered.snapshot();
    let db: &dyn HirDatabase = &snapshot;
    let diags = blaze_diagnostics::check_crate(db, hir::Crate::from(krate), &names, &options);
    let mut sources = mounted.sources();
    let style = blaze_diagnostics::emit::Style { ansi: color, ..Default::default() };
    let diagnostics: Vec<Diagnostic> = diags
        .iter()
        .filter(|it| !cargo_hides(&it.message))
        .map(|it| blaze_diagnostics::api::diagnostic(it, &mut sources, style, Some(unit_of.clone())))
        .collect();
    let failed = diagnostics.iter().any(|it| it.level == Level::Error);
    let mut made = Made::default();
    let nothing = |diagnostics: Vec<Diagnostic>, ok: bool, made: Made| Made2 {
        compiled: Compiled { diagnostics, ok, puts: Vec::new(), outputs: Vec::new(), artifact: None },
        made,
        natives: Vec::new(),
    };
    if failed {
        return Ok(nothing(diagnostics, false, made));
    }
    if job.cancelled() {
        return Err(Failed::Again);
    }
    let mut diagnostics = diagnostics;
    let mut puts = Vec::new();
    let mut outputs = Vec::new();
    let mut natives = Vec::new();
    // What each file rustc would write holds: (path, bytes, executable).
    let mut files: Vec<(String, Vec<u8>, bool)> = Vec::new();
    let identity = job.lowered.identity(unit);
    if u.target.is_custom_build() && script::inert(job, unit).is_some() {
        // A build script that does nothing is not run, so not built: what it prints is known.
        return Ok(nothing(diagnostics, true, made));
    }
    if checks_only(job.input.goal, u) {
        for path in artifact::made(plan) {
            files.push((path.to_owned(), crate_record(u, identity, &[]), false));
        }
    } else {
        let makes = match codegen::makes(u) {
            Ok(it) => it,
            Err(what) => {
                diagnostics.push(say::refused(what, Some(unit_of), color));
                return Ok(nothing(diagnostics, false, made));
            }
        };
        let generated = match codegen::generate(job, unit, &makes, &mounted)
            .map_err(|what| ApiError::new(what, Some(u.target.name.clone())))?
        {
            Ok(it) => it,
            Err(errors) => {
                let what = crate::refused_error(&u.target.name, errors.len(), errors.into_iter());
                diagnostics.push(say::refused(what, Some(unit_of), color));
                return Ok(nothing(diagnostics, false, made));
            }
        };
        if job.cancelled() {
            return Err(Failed::Again);
        }
        puts.extend(generated.built.puts.iter().map(|it| Put { key: it.key, kind: it.kind, bytes: it.bytes.clone() }));
        made.context = Some(context(&generated.policy));
        for test in &generated.roots.tests {
            let root = generated.built.roots.get(test.root).ok_or_else(|| {
                ApiError::new(format!("test `{}` has no root among the build's", test.name), Some(u.target.name.clone()))
            })?;
            let reached = generated.built.reached(root);
            made.tests.push(TestOf {
                name: test.name.clone(),
                ignore: test.ignore,
                should_panic: test.should_panic.clone(),
                bench: test.bench,
                outside: test.outside,
                reached,
            });
        }
        let roots: Vec<ContentId> = generated.built.roots.iter().map(|it| it.0).collect();
        for path in artifact::made(plan) {
            let library = path.ends_with(".rlib") || path.ends_with(".rmeta");
            if library || makes == Makes::Library {
                files.push((path.to_owned(), crate_record(u, identity, &roots), false));
                continue;
            }
            match codegen::link(job, unit, path, &makes, &generated) {
                Ok((bytes, read)) => {
                    natives.extend(read);
                    files.push((path.to_owned(), bytes, true));
                }
                Err(what) => {
                    diagnostics.push(say::refused(what, Some(unit_of), color));
                    return Ok(nothing(diagnostics, false, made));
                }
            }
        }
    }
    let sources = crate_files(job, krate);
    for path in &plan.outputs {
        let bytes = if let Some((_, bytes, executable)) = files.iter().find(|it| it.0 == *path) {
            Some((bytes.clone(), *executable))
        } else if let Some(made) = files.iter().find(|it| artifact::uplifted(&it.0, plan) == Some(path.as_str())) {
            Some((made.1.clone(), made.2))
        } else if path.ends_with(".d") {
            let main = files.first().map(|it| it.0.as_str());
            Some((dep_info(path, plan, main, &sources, &mounted), false))
        } else {
            // Debug info beside an image (`.dSYM`, `.pdb`, `.dwp`): blaze's images carry
            // their line tables, and write none beside them.
            None
        };
        let Some((bytes, executable)) = bytes else { continue };
        let key = Key(ContentId::of(OUTPUT, &bytes));
        if !puts.iter().any(|it: &Put| it.key == key) {
            puts.push(Put { key, kind: Kind::Output, bytes });
        }
        outputs.push(Output { path: path.clone(), key, executable });
    }
    let main = artifact::main(u, plan);
    let main_key = main.as_ref().and_then(|main| outputs.iter().find(|it| it.path == *main)).map(|it| it.key);
    made.main = match (&main, main_key) {
        (Some(path), Some(key)) => Some((path.clone(), key.0)),
        _ => None,
    };
    let artifact = match (&main, main_key) {
        (Some(path), Some(key)) => Some(artifact::artifact(u, plan, package, path, key.0, false, runs(job, unit))),
        _ => None,
    };
    let natives = natives.into_iter().map(|(path, bytes): (String, Vec<u8>)| (path, ContentId::of(NATIVE, &bytes))).collect();
    Ok(Made2 { compiled: Compiled { diagnostics, ok: true, puts, outputs, artifact }, made, natives })
}

/// How `unit`'s program is run: its plan's runner, and cargo's variables of it, from the build
/// scripts' runs it depends on (its package's script's `rustc-env`, every run's library search
/// directories).
fn runs(job: &Job, unit: usize) -> artifact::Runs {
    let units = job.units();
    let Some(plan) = units.plans.get(unit) else { return artifact::Runs::default() };
    let pkg_id = units.graph.units.get(unit).map(|it| it.pkg_id.as_str());
    let scripts = job.scripts.lock().unwrap_or_else(|it| it.into_inner());
    let mut own = Vec::new();
    let mut native = Vec::new();
    for at in codegen::closure(job, unit) {
        let Some(applied) = scripts.get(&at) else { continue };
        if Some(applied.package_id.as_str()) == pkg_id {
            own = applied.output.env.clone();
        }
        for path in &applied.output.library_paths {
            let path = blaze_build_script::output::search_path_of(path.path()).to_string_lossy().into_owned();
            if !native.contains(&path) {
                native.push(path);
            }
        }
    }
    let cargo = job.shared.launcher.to_string_lossy();
    artifact::Runs {
        runner: plan.runner.clone(),
        env: artifact::run_env(plan, &cargo, &own, &native, std::env::consts::OS),
    }
}

/// What a test binary's behaviour depends on beside the code it reaches.
fn context(policy: &blaze_codegen::plan::Policy) -> ContentId {
    let bytes = blaze_protocol::cbor::to_vec(policy).unwrap_or_default();
    ContentId::of("blaze v2 test context 1", &bytes)
}

/// What blaze writes for a library (`.rlib`, `.rmeta`): no object code (its functions are in
/// the store, by key, for whatever links it), but an `ar` archive, as rustc's are, with one
/// member `lib.rmeta` saying which unit it is, what it was made from, and its roots' keys.
pub(crate) fn crate_record(unit: &Unit, identity: Option<ContentId>, roots: &[ContentId]) -> Vec<u8> {
    let mut member = b"blaze v2 crate 1\n".to_vec();
    member.extend_from_slice(format!("{} {}\n", unit.pkg_id, unit.target.name).as_bytes());
    if let Some(identity) = identity {
        member.extend_from_slice(&identity.0);
    }
    member.extend_from_slice(&(roots.len() as u64).to_le_bytes());
    for root in roots {
        member.extend_from_slice(&root.0);
    }
    ar(&[("lib.rmeta", &member)])
}

/// A System V `ar` archive of `members`, deterministic (no time, no owner, mode 644).
fn ar(members: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = b"!<arch>\n".to_vec();
    for (name, bytes) in members {
        let header = format!("{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n", format!("{name}/"), 0, 0, 0, 644, bytes.len());
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(bytes);
        if bytes.len() % 2 == 1 {
            out.push(b'\n');
        }
    }
    out
}

/// The files of `krate`'s modules, by absolute path, sorted.
fn crate_files(job: &Job, krate: base_db::Crate) -> Vec<String> {
    let snapshot = job.lowered.snapshot();
    let db: &dyn HirDatabase = &snapshot;
    let paths = job.lowered.paths();
    let def_map = hir_def::nameres::crate_def_map(db, krate);
    let mut out = BTreeSet::new();
    for (_, module) in def_map.modules(db) {
        if let Some(file) = module.origin.file_id()
            && let Some(path) = paths.get(&file.file_id(db))
        {
            out.insert(path.clone());
        }
    }
    out.into_iter().collect()
}

/// A dep-info file as rustc (`deps/foo-0123.d`: its outputs, then each source by the name
/// rustc gives it) or cargo (`target/debug/foo.d`: the uplifted file and absolute sources)
/// writes it.
pub(crate) fn dep_info(path: &str, plan: &blaze_units::query::UnitPlan, main: Option<&str>, sources: &[String], mounted: &Mounted) -> String {
    let escape = |it: &str| it.replace(' ', "\\ ");
    let rustc = plan.extra_filename.is_empty() || path.contains(plan.extra_filename.as_str());
    match rustc {
        true => {
            let names: Vec<String> = sources.iter().map(|it| escape(&mounted.name(it))).collect();
            let deps = names.join(" ");
            let mut out = String::new();
            for target in main.into_iter().chain(std::iter::once(path)) {
                out += &format!("{}: {deps}\n\n", escape(target));
            }
            for name in &names {
                out += &format!("{name}:\n");
            }
            out
        }
        false => {
            let target = main.and_then(|it| artifact::uplifted(it, plan)).or(main).unwrap_or(path);
            let deps: Vec<String> = sources.iter().map(|it| escape(it)).collect();
            format!("{}: {}\n", escape(target), deps.join(" "))
        }
    }
}

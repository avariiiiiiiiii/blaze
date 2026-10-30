//! One unit's compile ([`Front::compile`](crate::serve::work::Front::compile)): its
//! diagnostics as rustc gives them and cargo shows them, then, unless it is a check, its code
//! compiled and linked by blaze, and every file cargo would write for it.
//!
//! A unit's result is kept in the store as its record, under what it was made from: the
//! front end's identity of the unit's inputs (its sources, cfgs, env, its dependencies' and
//! their build scripts' results, deeply), its plan, its profile and target, the toolchain and
//! blaze's own code ([`Toolchain::id`](blaze_codegen::settings::Toolchain)), the bytes of a
//! `-C profile-use` profile, the goal and the colour of its diagnostics. A unit whose record and outputs are all in the store is fresh:
//! its diagnostics are replayed as cargo replays a fresh unit's, and nothing runs.

#![forbid(unsafe_code)]

use blaze_api::{ApiError, Artifact, Diagnostic, Level};
use blaze_cas::{Key, Kind};
use blaze_codegen::settings::Pgo;
use blaze_diagnostics::{Options, lints::CommandLine};
use blaze_ids::{ContentId, Hasher};
use blaze_items::CodeHash;
use blaze_units::schema::Unit;
use hir_ty_codegen::v2::BodyChecks;
use blaze_lower::{ProcMacroLoadResult, ProcMacroLoadingError};
use serde::{Deserialize, Serialize};

use super::{
    Job, Real, UnitReader, artifact,
    checks::Checking,
    checks_only, graph,
    codegen::{self, Makes},
    files::Mounted,
    say, script,
};
use crate::serve::work::{Compiled, Failed, Goal, Output, Put};

type Image = crate::serve::work::Image<Real>;

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
    /// Every instance its codegen compiled or found, with the code hash it was made from, as
    /// [`blaze_api::Response::CodegenInputs`] lists them: sorted by key.
    pub(crate) inputs: Vec<(ContentId, ContentId)>,
    /// A test binary's execution table, made beside its link
    /// ([`super::test::image::table`]), which its tests are selected by: kept with its record,
    /// so a replayed binary selects by the table it was linked with.
    pub(crate) table: Option<blaze_codegen::ExecTable>,
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
    /// What it reaches, sorted and each once: every code hash, argument and static value
    /// (`Built::reached`), and a hash of each span table of its binary.
    pub(crate) reached: Vec<CodeHash>,
}

/// A unit's result as the store keeps it, after the [`header`]: what a replay gives back.
#[derive(Deserialize)]
struct Record {
    ok: bool,
    diagnostics: Vec<Diagnostic>,
    outputs: Vec<(String, Key, bool)>,
    artifact: Option<Artifact>,
    made: Made,
}

/// [`Record`] as it is written, borrowing what the compile made rather than copying it.
#[derive(Serialize)]
struct RecordRef<'a> {
    ok: bool,
    diagnostics: &'a [Diagnostic],
    outputs: Vec<(&'a str, Key, bool)>,
    artifact: Option<&'a Artifact>,
    made: &'a Made,
}

const RECORD: &str = "blaze v2 cargo unit record 3";
const OUTPUT: &str = "blaze v2 cargo output 1";
/// A span table's anchor and content, as a test's selection holds it.
const SPANS: &str = "blaze v2 cargo test spans 1";
/// A test binary's context with the native inputs its link read, in link order.
const NATIVES: &str = "blaze v2 cargo test natives 1";

/// The record's fixed header, which `fresh` reads where the store has it, allocating nothing:
/// `MAGIC`, the count of outputs and of native inputs (`u32` LE each), each output's key
/// (32 bytes), then each native input's BLAKE3 hash (32 bytes), its path's length (`u32` LE)
/// and its path. The CBOR of [`Record`] follows.
pub(crate) mod header {
    use blaze_format::cursor::{Put as _, Reader};

    pub(crate) const MAGIC: &[u8; 4] = b"BZU1";

    pub(crate) fn write(out: &mut Vec<u8>, outputs: &[blaze_cas::Key], natives: &[(String, blaze_ids::ContentId)]) {
        let paths: usize = natives.iter().map(|it| it.0.len()).sum();
        out.reserve(12 + 32 * outputs.len() + 36 * natives.len() + paths);
        out.put_bytes(MAGIC);
        out.put_u32(outputs.len() as u32);
        out.put_u32(natives.len() as u32);
        for key in outputs {
            out.put_bytes(&key.0.0);
        }
        for (path, hash) in natives {
            out.put_bytes(&hash.0);
            out.put_bytes_u32(path.as_bytes());
        }
    }

    /// Whether every output is in the store and every native input is as it was, and where the
    /// CBOR body starts; `None` for a record that is not, or a header that is cut short.
    pub(crate) fn check(
        bytes: &[u8],
        has: impl Fn(&blaze_cas::Key) -> bool,
        file: &mut impl FnMut(&std::path::Path) -> Option<[u8; 32]>,
    ) -> Option<usize> {
        let mut read = Reader::new(bytes);
        if read.array::<4>().ok()? != MAGIC {
            return None;
        }
        let outputs = read.u32().ok()?;
        let natives = read.u32().ok()?;
        for _ in 0..outputs {
            let key = blaze_cas::Key(blaze_ids::ContentId(*read.array().ok()?));
            if !has(&key) {
                return None;
            }
        }
        for _ in 0..natives {
            let hash: [u8; 32] = *read.array().ok()?;
            let path = read.str_u32().ok()?;
            if file(std::path::Path::new(path))? != hash {
                return None;
            }
        }
        Some(read.at())
    }
}

/// The key of `unit`'s record: none while the front end can't say what the unit's inputs are.
/// Worked out once per job and unit, once the writer has set the unit's identity; before
/// that, none and nothing memoized.
fn record_key(job: &Job, unit: usize) -> Result<Option<Key>, ApiError> {
    if let Some(key) = job.keys.get(unit).and_then(|it| it.get()) {
        return Ok(*key);
    }
    let Some(identity) = job.identities.get(unit).and_then(|it| it.get()) else { return Ok(None) };
    let key = match identity {
        Some(identity) => Some(work_out_key(job, unit, *identity)?),
        None => None,
    };
    if let Some(slot) = job.keys.get(unit) {
        let _ = slot.set(key);
    }
    Ok(key)
}

/// `unit`'s identity in the front end, once the writer set it.
fn identity(job: &Job, unit: usize) -> Option<ContentId> {
    job.identities.get(unit).and_then(|it| it.get()).copied().flatten()
}

fn work_out_key(job: &Job, unit: usize, identity: ContentId) -> Result<Key, ApiError> {
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
    let runs = runs(job, unit);
    // An inert script's compile makes nothing, unless `BLAZE_ELIDE=verify` builds it to run it.
    let verified = u.target.is_custom_build() && script::verifies(job)? && script::inert(job, unit).is_some();
    let reached = reached_only(job, u)?;
    let mut hasher = Hasher::new(RECORD);
    hasher.id(&identity);
    // The unit, its plan and its run environment, each encoded into one buffer kept per
    // thread and hashed from it: no buffer allocated per unit once the first has grown it.
    SCRATCH.with_borrow_mut(|scratch| -> Result<(), ApiError> {
        hash_cbor(&mut hasher, scratch, u, "unit", unit)?;
        hash_cbor(&mut hasher, scratch, plan, "plan", unit)?;
        hash_cbor(&mut hasher, scratch, &runs.env, "run environment", unit)
    })?;
    hasher.id(&toolchain.id).update(&[
        goal,
        u8::from(job.input.color),
        u8::from(job.input.short),
        u8::from(verified),
        u8::from(reached),
    ]);
    // The flags name a profile by its path; its bytes are the input.
    if let Pgo::Use(profile) = codegen::unit_pgo(job, unit).map_err(|what| ApiError::new(what, Some(u.target.name.clone())))? {
        hasher.id(&profile);
    }
    // The rendered diagnostics' columns: none is rustc's default, apart from every width.
    hasher.update(&job.input.width.map_or(0, |it| u64::from(it) + 1).to_le_bytes());
    Ok(Key(hasher.finish()))
}

/// Whether `unit` is a build script elision found inert, so neither built nor run: under
/// `BLAZE_ELIDE=verify` it is built and run as well.
fn elided_unbuilt(job: &Job, unit: usize) -> Result<bool, ApiError> {
    let custom_build = job.unit(unit)?.target.is_custom_build();
    Ok(custom_build && !script::verifies(job)? && script::inert(job, unit).is_some())
}

/// The variable that builds every dependency whole: `full`, for the differential run against a
/// reachable-only build (`harness/v2/reachable.py`); unset, only what the build's programs reach.
pub(crate) const DEPS: &str = "BLAZE_DEPS";

/// Why a crate [`reached_only`] has no check outcome or bodies of its own in the store.
pub(crate) const REACHED_ONLY: &str =
    "only what the build's programs reach of it is type-checked and compiled (`BLAZE_DEPS=full` checks it whole)";

/// Whether only what the build's programs reach of `unit`'s crate is type-checked and compiled
/// (docs/v2/plan/lanes/cold-v2/notes.md): a crate of the standard library, or of a package
/// cargo caps the lints of (a registry or git dependency), unless the job's [`DEPS`] is `full`.
/// Such a library runs no check and compiles no roots of its own: a program's walk, from roots
/// as complete as rustc's collector's, compiles what it reaches of it and type-checks those
/// bodies on the way, and a body it reaches that does not type-check stops that walk with an
/// error naming it. A workspace member and a path package are checked and compiled whole, as
/// cargo shows their diagnostics. Any other value of [`DEPS`] is an error naming it.
pub(crate) fn reached_only(job: &Job, unit: &Unit) -> Result<bool, ApiError> {
    match job.input.config.invocation.env.iter().find(|(name, _)| name == DEPS).map(|(_, value)| value.as_str()) {
        None => {}
        Some("full") => return Ok(false),
        Some(value) => {
            return Err(ApiError::new(
                // alloc: error path
                format!(
                    "{DEPS}={value} is not a value blaze takes: `full` checks and compiles every dependency whole, \
                     unset only what the build's programs reach of each"
                ),
                None,
            ));
        }
    }
    let package = job.package(unit)?;
    Ok(capped(unit.is_std, package.is_member, package.source.as_deref()))
}

/// Whether a unit's diagnostics are ones cargo never shows (lints capped, so only an error
/// could be seen): the standard library's, and any package that is neither a workspace member
/// nor at a path.
pub(crate) fn capped(is_std: bool, is_member: bool, source: Option<&str>) -> bool {
    is_std || !(is_member || local(source))
}

fn hash_cbor<T: Serialize + ?Sized>(
    hasher: &mut Hasher,
    scratch: &mut Vec<u8>,
    value: &T,
    what: &str,
    unit: usize,
) -> Result<(), ApiError> {
    scratch.clear();
    blaze_protocol::cbor::append(scratch, value)
        .map_err(|why| ApiError::new(format!("encoding the {what} of unit {unit}: {why}"), None))?;
    hasher.part(scratch);
    Ok(())
}

thread_local! {
    static SCRATCH: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// The machine's files as the process's table of them by `stat` hashes them: one `stat` for a
/// file hashed before, read only when its `stat` changed.
fn machine_file(disk: &mut blaze_usercode::inputs::Disk) -> impl FnMut(&std::path::Path) -> Option<[u8; 32]> + '_ {
    move |path| blaze_usercode::trace::Inputs::file(disk, path).ok().flatten()
}

/// Whether nothing of `unit` will be made again: its record and all it wrote are in the store,
/// and every native input it linked is as it was. Read in place: no decode, no allocation.
pub(crate) fn fresh(job: &Job, unit: usize) -> bool {
    let Ok(Some(key)) = record_key(job, unit) else { return false };
    let (store, guard) = (&job.input.store, blaze_cas::enter());
    let Ok(Some(entry)) = store.get(&guard, &key) else { return false };
    let mut disk = blaze_usercode::inputs::Disk::new(Vec::new());
    header::check(entry.bytes, |key| store.has(&guard, key), &mut machine_file(&mut disk)).is_some()
}

/// What `unit`'s last build made, as its record keeps it: none when the view's units were never
/// built, or were built from other code.
pub(crate) fn last_made(job: &Job, unit: usize) -> Option<Made> {
    kept(job, unit).map(|it| it.made)
}

/// Every instance the view's last build compiled or found, with the code hash it was made
/// from, sorted by key: each unit's from its record. A unit with no record, never built or
/// built from other code, is an error naming it.
pub(crate) fn codegen_inputs(job: &Job) -> Result<Vec<(ContentId, ContentId)>, Failed> {
    let mut inputs = Vec::new();
    for unit in 0..job.graph.len() {
        if job.graph.has(unit as u32, graph::flag::RUN) {
            continue;
        }
        if job.cancelled() {
            return Err(Failed::Again);
        }
        let Some(made) = last_made(job, unit) else {
            let name = &job.unit(unit)?.target.name;
            return Err(ApiError::new(format!("`{name}` has no build of this view to list the codegen inputs of"), None).into());
        };
        inputs.extend(made.inputs);
    }
    inputs.sort_unstable();
    inputs.dedup();
    Ok(inputs)
}

/// The record of `unit` the store has, every output and native input of it as it was.
fn kept(job: &Job, unit: usize) -> Option<Record> {
    let key = record_key(job, unit).ok()??;
    let (store, guard) = (&job.input.store, blaze_cas::enter());
    let entry = store.get(&guard, &key).ok()??;
    let mut disk = blaze_usercode::inputs::Disk::new(Vec::new());
    let body = header::check(entry.bytes, |key| store.has(&guard, key), &mut machine_file(&mut disk))?;
    blaze_protocol::cbor::from_slice(&entry.bytes[body..]).ok()
}

/// `unit` checked and, unless it is a check, compiled and linked, reading the front end through
/// `reader` and relinking the kept `images`. A proc macro's compile gives its expanders
/// ([`Compiled::macros`]).
pub(crate) fn compile(job: &Job, reader: &UnitReader, unit: usize, images: &mut Vec<Image>) -> Result<Compiled, Failed> {
    if let Some(record) = kept(job, unit) {
        let artifact = record.artifact.map(|it| Artifact { fresh: true, ..it });
        let mut compiled = Compiled {
            diagnostics: record.diagnostics,
            ok: record.ok,
            puts: Vec::new(),
            outputs: record
                .outputs
                .into_iter()
                .map(|(path, key, executable)| Output { path, key, executable })
                .collect(),
            artifact,
            macros: None,
            links: Vec::new(),
        };
        compiled.macros = macros(job, reader, unit, &compiled);
        if let Some(slot) = job.made.get(unit) {
            let _ = slot.set(record.made);
        }
        return Ok(compiled);
    }
    let made = make(job, reader, unit, images)?;
    let key = record_key(job, unit)?;
    let Made2 { mut compiled, made, natives } = made;
    compiled.macros = macros(job, reader, unit, &compiled);
    if let Some(key) = key && !script::pure::built_nothing(job, unit) {
        let record = RecordRef {
            ok: compiled.ok,
            diagnostics: &compiled.diagnostics,
            outputs: compiled.outputs.iter().map(|it| (it.path.as_str(), it.key, it.executable)).collect(),
            artifact: compiled.artifact.as_ref(),
            made: &made,
        };
        let keys: Vec<Key> = compiled.outputs.iter().map(|it| it.key).collect();
        let mut bytes = Vec::new();
        header::write(&mut bytes, &keys, &natives);
        blaze_protocol::cbor::append(&mut bytes, &record)
            .map_err(|why| ApiError::new(format!("encoding the record of unit {unit}: {why}"), None))?;
        compiled.puts.push(Put { key, kind: Kind::Query, bytes });
    }
    if let Some(slot) = job.made.get(unit) {
        let _ = slot.set(made);
    }
    Ok(compiled)
}

struct Made2 {
    compiled: Compiled,
    made: Made,
    natives: Vec<(String, ContentId)>,
}

/// The expanders of proc-macro unit `unit`, for the crates that use it
/// ([`crate::serve::work::Writer::macros`]); none for a unit that is no proc macro or did not
/// build. Each runs the macro's own code on blaze's compiler in this process
/// ([`blaze_macro_eval::load`], docs/v2/plan/design/compile-time.md change 20), isolated by
/// construction: nothing it compiled to is loaded, and no process is started. Asked again each
/// time the unit compiles or its record is reused, as the front end it reads may have changed.
fn macros(job: &Job, reader: &UnitReader, unit: usize, compiled: &Compiled) -> Option<ProcMacroLoadResult> {
    if !compiled.ok || !job.crates.is_proc_macro(unit) {
        return None;
    }
    let name = job.unit(unit).map_or("", |it| it.target.name.as_str());
    let failed = |what: String| Some(Err(ProcMacroLoadingError::Refused(what.into())));
    let Some(krate) = job.crates.krate(unit) else {
        return failed(format!("proc macro `{name}` has no crate in the front end"));
    };
    let toolchain = match codegen::toolchain(job, unit) {
        Ok(it) => it.id,
        Err(why) => return failed(format!("proc macro `{name}`: its toolchain: {why}")),
    };
    Some(blaze_macro_eval::load(reader.db(), krate, toolchain))
}

/// The lint flags of unit `unit`'s check, as `make` gives them.
pub(crate) fn unit_lint_flags(job: &Job, unit: usize) -> Result<Vec<String>, ApiError> {
    let u = job.unit(unit)?;
    let package = job.package(u)?;
    Ok(lint_flags(u, job.plan(unit)?, local(package.source.as_deref())))
}

pub(crate) use blaze_lower::{lint_flags, local};

/// Whether a diagnostic is one of rustc's closing lines cargo does not show: "aborting due to
/// ...", "N warnings emitted" (`cargo/src/cargo/core/compiler/mod.rs`, `on_stderr_line_inner`).
pub(crate) fn cargo_hides(message: &str) -> bool {
    message.starts_with("aborting due to")
        || message.ends_with("warning emitted")
        || message.ends_with("warnings emitted")
}



fn make(job: &Job, reader: &UnitReader, unit: usize, images: &mut Vec<Image>) -> Result<Made2, Failed> {
    let u = job.unit(unit)?;
    let plan = job.plan(unit)?;
    let package = job.package(u)?;
    let unit_of = job.unit_of(unit)?;
    let color = job.input.color;
    let krate = job.crates.krate(unit).ok_or_else(|| {
        ApiError::new(format!("unit {unit} (`{}`) has no crate in the front end", u.target.name), None)
    })?;
    let mounted = &job.mounted;
    let files = blaze_diagnostics::Files::new(reader.paths(), mounted.root());
    let flags = lint_flags(u, plan, local(package.source.as_deref()));
    let mut lints = CommandLine::parse(&flags)
        .map_err(|why| ApiError::new(why, Some(format!("the lint flags of `{}`", u.target.name))))?;
    // Every crate blaze builds is built as cargo builds it, with `CARGO_CRATE_NAME` set.
    lints.check_cfg.cargo = true;
    let options = Options { lints, ..Options::default() };
    let db = reader.db();
    let inert = elided_unbuilt(job, unit)? || script::pure::unbuilt(job, reader, unit)?;
    let reached = reached_only(job, u)?;
    // A library compiled only as a program reaches it has no code of its own to build.
    let library_reached = reached && codegen::makes(u).is_ok_and(|it| it == Makes::Library);
    let builds_code = !inert && !checks_only(job.input.goal, u) && !library_reached;
    let served = blaze_diagnostics::cache::read(db, reader.checks(), krate, mounted.root());
    // Declared before `generated`: the build's front end borrows it until `generated` is dropped.
    let checking;
    let (diags, generated) = match served {
        Some(Err(why)) => return Err(ApiError::new(why, Some(u.target.name.clone())).into()),
        // A dependency's check outcome from the store: no check runs, and codegen reads the
        // bodies' records served with it.
        Some(Ok(outcome)) => {
            let generated = builds_code.then(|| {
                let makes = codegen::makes(u)?;
                let generated = codegen::generate(job, reader, unit, &makes, mounted, None);
                Ok::<_, String>((makes, generated))
            });
            (outcome.diags, generated)
        }
        // No check: what a program reaches of the crate is type-checked by its walk.
        None if reached => {
            let generated = builds_code.then(|| {
                let makes = codegen::makes(u)?;
                let generated = codegen::generate(job, reader, unit, &makes, mounted, None);
                Ok::<_, String>((makes, generated))
            });
            // alloc: none: an empty `Vec` holds no buffer
            (Vec::new(), generated)
        }
        None => {
            checking = Checking::new(db, hir::Crate::from(krate));
            let (diags, generated) = match builds_code {
                false => (checking.alone(db, files, &options), None),
                true => {
                    let (diags, generated) = checking.alongside(db, files, &options, || {
                        let makes = codegen::makes(u)?;
                        // dyn: `HirFrontend::with_checks` takes it; called once a level of the walk
                        let checks: &dyn BodyChecks = &checking;
                        let generated = codegen::generate(job, reader, unit, &makes, mounted, Some(checks));
                        Ok::<_, String>((makes, generated))
                    });
                    (diags, Some(generated))
                }
            };
            if hir_def::deps::eligible(db, krate) {
                let paths = blaze_diagnostics::cache::Paths::of(db, krate);
                // alloc: the outcome encoded once more per dependency crate checked, for its report line
                let refused = blaze_diagnostics::cache::encode(&diags, &paths, mounted.root()).err().map(|it| it.to_string());
                // clone: once per dependency crate checked this build
                job.check_later(krate, flags, diags.clone(), refused);
            }
            (diags, generated)
        }
    };
    let sources = mounted.sources(&diags);
    let mut style = blaze_diagnostics::emit::Style { ansi: color, short: job.input.short, ..Default::default() };
    if let Some(width) = job.input.width {
        style.width = usize::try_from(width).unwrap_or(usize::MAX);
    }
    let diagnostics: Vec<Diagnostic> = diags
        .iter()
        .filter(|it| !cargo_hides(&it.message))
        .map(|it| blaze_diagnostics::api::diagnostic(it, &sources, style, Some(unit_of.clone())))
        .collect();
    let failed = diagnostics.iter().any(|it| it.level == Level::Error);
    let mut made = Made::default();
    let nothing = |diagnostics: Vec<Diagnostic>, ok: bool, made: Made| Made2 {
        compiled: Compiled {
            diagnostics,
            ok,
            puts: Vec::new(),
            outputs: Vec::new(),
            artifact: None,
            macros: None,
            links: Vec::new(),
        },
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
    let mut links = Vec::new(); // alloc: none until a link
    // What each file rustc would write holds: (path, bytes, executable).
    let mut files: Vec<(String, Vec<u8>, bool)> = Vec::new();
    let identity = identity(job, unit);
    if inert {
        // A build script that does nothing is not run, so not built: what it prints is known.
        return Ok(nothing(diagnostics, true, made));
    }
    match generated {
        None => {
            for path in artifact::made(plan) {
                files.push((path.to_owned(), crate_record(u, identity, &[]), false));
            }
        }
        Some(Err(what)) => {
            diagnostics.push(say::refused(what, Some(unit_of), color));
            return Ok(nothing(diagnostics, false, made));
        }
        Some(Ok((makes, generated))) => {
            let mut generated =
                match generated.map_err(|what| ApiError::new(what, Some(u.target.name.clone())))? {
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
            let built_puts = std::mem::take(&mut generated.built.puts);
            puts.reserve(built_puts.len() + plan.outputs.len());
            puts.extend(built_puts.into_iter().map(|it| Put { key: it.key, kind: it.kind, bytes: it.bytes }));
            made.context = Some(context(&generated.policy));
            made.inputs = generated.built.inputs.iter().map(|(key, code)| (key.0, code.0)).collect();
            // A test's output prints positions (a panic's `file:line:col`), which no code hash
            // has: a reformat that moves a `panic!` changes its message. Every span table of the
            // binary is in each test's selection, so such an edit runs its tests again.
            // alloc: once per test unit, one hash per span table
            let spans: Vec<CodeHash> = match generated.roots.tests.is_empty() {
                true => Vec::new(),
                false => generated
                    .side()
                    .map_err(|what| ApiError::new(what, Some(u.target.name.clone())))?
                    .spans
                    .iter()
                    .map(|(anchor, table)| {
                        let mut hasher = Hasher::new(SPANS);
                        hasher.id(anchor).part(table);
                        CodeHash(hasher.finish())
                    })
                    .collect(),
            };
            for test in &generated.roots.tests {
                let root = generated.built.roots.get(test.root).ok_or_else(|| {
                    ApiError::new(
                        format!("test `{}` has no root among the build's", test.name),
                        Some(u.target.name.clone()),
                    )
                })?;
                let mut reached = generated.built.reached(root);
                reached.extend_from_slice(&spans);
                reached.sort_unstable();
                reached.dedup();
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
                match codegen::link(job, unit, path, &makes, &generated, images) {
                    Ok(linked) => {
                        natives.extend(linked.read);
                        if linked.table.is_some() {
                            made.table = linked.table;
                        }
                        // alloc: once per image linked, a unit links one or two
                        links.push((path.to_owned(), linked.how));
                        files.push((path.to_owned(), linked.bytes, true));
                    }
                    Err(failed) => {
                        // alloc: error path
                        diagnostics.push(say::refused(failed.to_string(), Some(unit_of), color));
                        return Ok(nothing(diagnostics, false, made));
                    }
                }
            }
        }
    }
    // Each file's bytes go into the store once, moved, under their key; a copy cargo makes of
    // one (`target/debug/foo` of `deps/foo-0123`) is the same key.
    let sources = crate_files(reader, krate);
    let mut keyed: Vec<(String, Key, bool)> = Vec::with_capacity(files.len());
    let mut output_keys: Vec<Key> = Vec::with_capacity(plan.outputs.len());
    let mut put_output = |puts: &mut Vec<Put>, bytes: Vec<u8>| {
        let key = Key(ContentId::of(OUTPUT, &bytes));
        if !output_keys.contains(&key) {
            output_keys.push(key);
            puts.push(Put { key, kind: Kind::Output, bytes });
        }
        key
    };
    let main_file = files.first().map(|it| it.0.clone());
    for (path, bytes, executable) in files {
        let key = put_output(&mut puts, bytes);
        keyed.push((path, key, executable));
    }
    outputs.reserve(plan.outputs.len());
    for path in &plan.outputs {
        let found = keyed
            .iter()
            .find(|it| it.0 == *path)
            .or_else(|| keyed.iter().find(|it| artifact::uplifted(&it.0, plan) == Some(path.as_str())));
        let (key, executable) = match found {
            Some((_, key, executable)) => (*key, *executable),
            None if path.ends_with(".d") => {
                let text = dep_info(path, plan, main_file.as_deref(), &sources, mounted);
                (put_output(&mut puts, text.into_bytes()), false)
            }
            // Debug info beside an image (`.dSYM`, `.pdb`, `.dwp`): blaze's images carry
            // their line tables, and write none beside them.
            None => continue,
        };
        outputs.push(Output { path: path.clone(), key, executable });
    }
    let main = artifact::main(u, plan);
    let main_key = main.as_ref().and_then(|main| outputs.iter().find(|it| it.path == *main)).map(|it| it.key);
    made.main = match (&main, main_key) {
        (Some(path), Some(key)) => Some((path.clone(), key.0)),
        _ => None,
    };
    let artifact = match (&main, main_key) {
        (Some(path), Some(key)) => Some(artifact::artifact(u, plan, package, path, key.0, false, runs(job, unit))?),
        _ => None,
    };
    // The native code linked in is in no code hash a test reaches: a change of it runs them all.
    if let Some(context) = &mut made.context {
        *context = with_natives(*context, &natives);
    }
    Ok(Made2 { compiled: Compiled { diagnostics, ok: true, puts, outputs, artifact, macros: None, links }, made, natives })
}

/// A test binary's `context` with the content of each native input its link read, in link
/// order; `context` itself when it read none.
pub(crate) fn with_natives(context: ContentId, natives: &[(String, ContentId)]) -> ContentId {
    if natives.is_empty() {
        return context;
    }
    let mut hasher = Hasher::new(NATIVES);
    hasher.id(&context);
    for (_, native) in natives {
        hasher.id(native);
    }
    hasher.finish()
}

/// How `unit`'s program is run: its plan's runner, and cargo's variables of it, from the build
/// scripts' runs it depends on (its package's script's `rustc-env`, every run's library search
/// directories). Worked out once per job and unit: the record's key and the artifact read it.
fn runs(job: &Job, unit: usize) -> artifact::Runs {
    if let Some(slot) = job.runs.get(unit) {
        return slot.get_or_init(|| work_out_runs(job, unit)).clone();
    }
    artifact::Runs::default()
}

fn work_out_runs(job: &Job, unit: usize) -> artifact::Runs {
    let units = job.units();
    let Some(plan) = units.plans.get(unit) else { return artifact::Runs::default() };
    let package = job.graph.package(unit as u32);
    let mut own: &[(String, String)] = &[];
    let mut native: Vec<String> = Vec::new();
    let mut seen = rustc_hash::FxHashSet::default();
    for at in job.graph.closure(unit as u32) {
        if !job.graph.has(at, graph::flag::RUN) {
            continue;
        }
        let Some(applied) = job.scripts[at as usize].get() else { continue };
        if job.graph.package(at) == package {
            own = &applied.output.env;
        }
        for path in &applied.output.library_paths {
            let path = blaze_build_script::output::search_path_of(path.path());
            if seen.insert(path) {
                native.push(path.to_string_lossy().into_owned());
            }
        }
    }
    let cargo = job.shared.launcher.to_string_lossy();
    artifact::Runs {
        runner: plan.runner.clone(),
        env: artifact::run_env(plan, &cargo, own, &native, std::env::consts::OS),
    }
}

/// What a test binary's behaviour depends on beside the code it reaches.
fn context(policy: &blaze_codegen::plan::Policy) -> ContentId {
    let bytes = blaze_protocol::cbor::to_vec(policy).unwrap_or_default();
    ContentId::of("blaze v2 test context 2", &bytes)
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
fn crate_files(reader: &UnitReader, krate: base_db::Crate) -> Vec<String> {
    let db = reader.db();
    let paths = reader.paths();
    let def_map = hir_def::nameres::crate_def_map(db, krate);
    let mut out = Vec::new();
    for (_, module) in def_map.modules(db) {
        if let Some(file) = module.origin.file_id()
            && let Some(path) = paths.get(&file.file_id(db))
        {
            out.push(path);
        }
    }
    out.sort_unstable();
    out.dedup();
    out.into_iter().cloned().collect()
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

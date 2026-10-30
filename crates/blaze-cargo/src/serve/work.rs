//! What a job of the daemon asks of the build (docs/v2/cargo-notes.md section 6): the steps
//! between a plan and its outputs, behind traits so the loop runs the same in the daemon
//! ([`crate::serve::unit::Real`]) and in the simulation ([`crate::serve::tests`]).
//!
//! Where each is called is part of its contract:
//!
//! - **On an IO thread of the plane**: [`Steps::fetch`], [`Steps::library`]. They may block on
//!   the network or the disk; they touch no state of the loop.
//! - **On a worker of the plane**: [`Steps::lower`], [`Front::compile`], [`Front::reach`],
//!   [`Front::ub_tests`], [`Front::evaluate`], [`Front::codegen_inputs`], [`Front::tables`].
//!   They read the front end through the [`Reader`] the loop made as it posted the task, the
//!   VFS's reader and the store through [`JobInput`], and give back what the loop writes
//!   ([`Compiled::puts`]); a [`Failed::Again`] goes back to the loop, which posts it again
//!   with a new reader. No worker locks to read.
//! - **On the loop**: [`Front::fresh`], [`Front::script`], [`Front::tests`], [`Front::run`],
//!   every [`Writer`] and every [`Runs`] method. User code runs as
//!   [`blaze_core::Effect::Spawn`]s of the loop's driver, in the sandbox its
//!   [`blaze_core::Confine`] makes, never on a thread of its own.
//!
//! # Who reads the front end, and where the stop is
//!
//! A job's front end is one value owned by the loop: its database is shared by reference and
//! never cloned (docs/v2/plan/process/decisions.md). The tasks that read it are those of a
//! batch added with `reads` ([`crate::serve::plane::Plane::add`]): every compile, the
//! `--ub` selection and evaluations, a reach (of both its views) and the codegen inputs. Each
//! is posted with a [`Reader`], a shared reference it holds for its run and drops as it ends,
//! with no lock, no clone of the database and no cancellation. The tables are measured, which
//! salsa does only with the database exclusive: their task, the front end's last use, is
//! handed the job's [`Writer`] itself.
//!
//! A build script's output ([`Writer::ran`]) and a proc macro's load ([`Writer::macros`]) are
//! writes. Each is checked as it is asked for and applied by [`Writer::apply`], on the loop,
//! once no reader is alive: every reader is one of a task posted before the write was asked
//! for, and from then until it is applied the loop posts no reading task of that job, but
//! holds it and posts it after, with a reader of what the write made. That wait is the stop.
//! Tasks of the job that don't read (build scripts and the program's runs, store puts and
//! placements, fetches, the plan) and every other job's tasks keep going meanwhile. Each
//! write asked while readers run waits with the others, and `apply` applies them all as one,
//! so a wave of build scripts is one stop.
//!
//! Images kept linked between jobs are the loop's too ([`Image`]): each is moved into the
//! one compile that relinks it and back with what it made, so no worker shares them. So is a
//! checkout's last front end ([`Writer::keep`]), once no reader of it is left: it is moved
//! into the one lowering of the checkout's next build, which moves it to the new view.

#![forbid(unsafe_code)]

use std::{
    any::Any,
    sync::{Arc, atomic::AtomicBool},
};

use blaze_api::{ApiError, Artifact, Diagnostic, Reach, Response, Table, TestResult, Ub};
use blaze_cas::Store;
use blaze_core::{Completion, Effect, Tag};
use blaze_ids::ContentId;
use blaze_query::{Database, DbError, Query, QueryKey};
use blaze_units::query::{BuildConfig, Fetch, Plan, Planned, Units};
use blaze_vfs::{Change, Mode, ViewId};

/// A source fetched and checked, as its files: the daemon puts it into the VFS as a root of its
/// own and mounts it where its [`Fetch::at`] says.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Source {
    /// Every file, by `/`-separated path under the source's root, each once, none with an
    /// empty, `.` or `..` part.
    pub files: Vec<SourceFile>,
    /// What the fetch's `at` is under the root: "" for a directory (a `.crate`, a git tree),
    /// the file's path for one file (a sparse index's).
    pub under: String,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SourceFile {
    pub path: String,
    pub bytes: Vec<u8>,
    pub mode: Mode,
}

/// What a job's work is, as far as its units' work differs by it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Goal {
    /// Stops after the front end.
    Check,
    Build,
    Test,
    Run,
    /// Builds nothing: [`Front::reach`].
    Reach,
}

/// The tests a job runs, and how, as `blaze test` asked.
#[derive(Clone, Copy, Debug)]
pub struct Selection<'a> {
    /// Text a test's name must contain one of; none selects every test.
    pub filter: &'a [String],
    /// libtest's own selection, what followed `--`: `--exact`, `--ignored`,
    /// `--include-ignored`, `--skip NAME`.
    pub args: &'a [String],
    /// Every test, not only those the change reached.
    pub all: bool,
    /// Each test run in blaze's MIR evaluator, checked for undefined behavior. The job's
    /// units were checked only ([`Goal::Check`]): `built` is empty, nothing is linked.
    pub ub: Option<Ub>,
}

/// A job as its steps see it.
#[derive(Clone)]
pub struct JobInput {
    pub db: Arc<Database>,
    /// The job's view of `db`, opened with `config` ([`Database::open_with`]).
    pub queries: ViewId,
    /// The view's root, readable through `files`.
    pub root: ContentId,
    /// Every root of the VFS (the view's, each mount's), from any thread.
    pub files: blaze_vfs::Reader,
    /// What the view was opened with; its `invocation.env` is the request's whole
    /// environment, held in memory for the job only and never logged or kept.
    pub config: Arc<BuildConfig>,
    pub units: Arc<Units>,
    /// The store, read by tasks with no lock (a read is valid while its
    /// [`blaze_cas::enter`] guard is held); written only by the loop.
    pub store: blaze_cas::Reader,
    pub color: bool,
    pub goal: Goal,
    /// Set once the job is cancelled: long work looks at it between pieces and stops with
    /// [`Failed::Again`], which the loop drops for a cancelled job.
    pub cancelled: Arc<AtomicBool>,
}

/// Why a step gave no result.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Failed {
    /// It gave way (a cycle of waits, memory, a cancel): posted again whole, nothing of it kept.
    Again,
    /// The job can't go on: it finishes with this error.
    Error(ApiError),
}

impl From<ApiError> for Failed {
    fn from(error: ApiError) -> Failed {
        Failed::Error(error)
    }
}

/// The standard library's `library/` as [`Steps::library`] found it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Library {
    /// Where its sources are named: `/rustc/<commit>/library`.
    pub at: String,
    /// Of its digest and every file's path, size, time and mode: its files are read again
    /// only once this changes.
    pub fingerprint: String,
    /// Its files; none when it is mounted as it is already.
    pub source: Option<Source>,
}

/// A value for the store, put by the loop.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Put {
    pub key: blaze_cas::Key,
    pub kind: blaze_cas::Kind,
    pub bytes: Vec<u8>,
}

/// A file a unit writes: where cargo writes it (absolute, one of the unit's
/// `UnitPlan::outputs`) and the store key of its bytes, in the store or among the puts.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Output {
    pub path: String,
    pub key: blaze_cas::Key,
    pub executable: bool,
}

/// What came of one unit's compile.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Compiled {
    /// In rustc's order, each with its unit (`Diagnostic::unit`).
    pub diagnostics: Vec<Diagnostic>,
    /// No error among the diagnostics and nothing else failed: what depends on it goes on, as
    /// cargo's.
    pub ok: bool,
    pub puts: Vec<Put>,
    /// Placed by the loop once `puts` are in the store.
    pub outputs: Vec<Output>,
    /// cargo's `compiler-artifact` of the unit, reported in `Built` once its outputs are
    /// placed; none for a unit cargo lists none of.
    pub artifact: Option<Artifact>,
    /// A proc macro's expanders, loaded from what it compiled to (or why there are none):
    /// the loop asks the job's [`Writer::macros`] with them. None for any other unit.
    pub macros: Option<load_cargo::v2::ProcMacroLoadResult>,
}

/// The build of one job, as the daemon has it: one value for every job.
pub trait Steps: Send + Sync + 'static {
    /// On an IO thread: the source `fetch` names, fetched and checked (a `.crate`'s sha256 and
    /// its paths kept inside it, a git tree at its commit, an index file as served).
    fn fetch(&self, fetch: &Fetch) -> Result<Source, ApiError>;
    /// On an IO thread: the standard library's `library/`, put into the VFS once: where it is
    /// named (`/rustc/<commit>/library`), its fingerprint, and its files unless `kept` (a
    /// fingerprint, and where that library is mounted) has it there, read before by this
    /// daemon or one before it.
    fn library(&self, kept: &[(String, String)]) -> Result<Library, ApiError>;
    /// On a worker: the job's units lowered into the front end. `kept` is the front end of
    /// the head's last build ([`Writer::keep`]): built from the same units and config, it is
    /// moved to the job's view, its memos kept, and what changed alone read again; else it
    /// is dropped and the units lowered anew.
    fn lower(&self, job: &JobInput, kept: Option<KeptFront>) -> Result<FrontEnd, Failed>;
    /// On the loop, once at start: the queries of the daemon's database.
    fn register(&self, db: &mut Database) {
        blaze_units::query::register(db);
    }
    /// On a worker: the plan of `view` (opened with its [`BuildConfig`]).
    fn plan(&self, db: &Database, view: ViewId) -> Result<Planned, Failed> {
        match db.pin().get::<Plan>(view, &()) {
            Ok(planned) => Ok(planned.get()),
            Err(DbError::Cancelled) => Err(Failed::Again),
            Err(error) => Err(Failed::Error(ApiError::new(format!("the load of the workspace: {error}"), None))),
        }
    }
    /// The key [`Steps::plan`] asks: where a view's memos are walked from, and what the
    /// daemon's collection keeps.
    fn plan_key(&self) -> QueryKey {
        QueryKey { kind: <Plan as Query>::KIND, input: <Plan as Query>::input(&()) }
    }
}

/// One job's front end, lowered on a worker and handed to the loop in two: what every task
/// shares, and what only the loop writes.
pub struct FrontEnd {
    pub front: Arc<dyn Front>,
    pub writer: Box<dyn Writer>,
}

/// What a task reads the front end through: a shared reference to it, made by the loop as it
/// posted the task and dropped as the task ends. While one is alive the front end is not
/// written; a write waits for it ([`Writer::apply`]).
pub struct Reader(Box<dyn Any + Send>);

impl Reader {
    pub fn new(it: impl Any + Send) -> Reader {
        Reader(Box::new(it))
    }

    /// What [`Reader::new`] was given.
    ///
    /// # Panics
    ///
    /// When it was given another type: a reader of another front.
    pub fn take<R: Any>(self) -> R {
        match self.0.downcast() {
            Ok(it) => *it,
            Err(_) => panic!("a reader of another front than {}", std::any::type_name::<R>()),
        }
    }
}

/// A job's front end kept by the loop once the job is done with it, for the next job of the
/// same head to lower into ([`Steps::lower`]).
pub struct KeptFront(Box<dyn Any + Send>);

impl KeptFront {
    pub fn new(it: impl Any + Send) -> KeptFront {
        KeptFront(Box::new(it))
    }

    /// What [`KeptFront::new`] was given; none for another type, kept by another front.
    pub fn take<K: Any>(self) -> Option<K> {
        self.0.downcast().ok().map(|it| *it)
    }
}

/// The readers a task is posted with: of its job's front end, and for a reach, of the front it
/// reaches from; for a compile, the images it relinks; for the tables, the job's writer itself.
#[derive(Default)]
pub struct Readers {
    pub this: Option<Reader>,
    pub from: Option<Reader>,
    pub images: Vec<Image>,
    pub writer: Option<Box<dyn Writer>>,
}

/// An image kept linked between jobs at the path cargo writes it, so the next link of it
/// relinks only what changed. The loop owns every one; it moves it into the one compile that
/// relinks it ([`Front::compile`]) and takes it back with what the compile made. One the
/// compile does not give back (its link failed, or it gave way) is linked whole next time.
pub struct Image {
    pub path: String,
    pub kept: Box<dyn Kept>,
}

/// What a front keeps of a linked image.
pub trait Kept: Any + Send {
    /// Its tables as the linker counts them: name, elements, bytes ([`Response::Tables`]).
    fn tables(&self) -> Vec<(&'static str, u64, u64)>;
    fn into_any(self: Box<Self>) -> Box<dyn Any + Send>;
}

/// What of a job's front end only the loop writes. Its writes are asked for, then applied
/// together by [`Writer::apply`] once no [`Reader`] of it is alive (the module's docs).
pub trait Writer: Send + 'static {
    /// A reader of the front end as it stands, for one task about to be posted; none while a
    /// write is asked for and not applied, when the loop holds the task instead.
    fn reader(&self) -> Option<Reader>;
    /// Asks for the run of build script unit `run` applied to the units depending on it
    /// (`load_cargo::v2::Lowered::ran`): checked now, an error naming what is wrong with it,
    /// and applied by [`Writer::apply`].
    fn ran(&mut self, run: usize, ran: load_cargo::v2::Ran) -> Result<(), ApiError>;
    /// Asks for proc-macro unit `unit`'s expanders set (`load_cargo::v2::Lowered::macros`):
    /// checked now, applied by [`Writer::apply`].
    fn macros(&mut self, unit: usize, macros: load_cargo::v2::ProcMacroLoadResult) -> Result<(), ApiError>;
    /// Whether a write was asked for and is not applied yet.
    fn waiting(&self) -> bool;
    /// Every write asked for, applied as one, in the order asked, if no reader is alive:
    /// `Ok(true)` once applied (or none waited), `Ok(false)` while a reader is, asking again
    /// costing one load. An error ends the job.
    fn apply(&mut self) -> Result<bool, ApiError>;
    /// On a worker, the writer handed to the tables' task as the front end's last use: its
    /// tables (name, elements, bytes), measured with it exclusive, as salsa measures; an
    /// error while a reader of it is alive.
    fn tables(&mut self) -> Result<Vec<(&'static str, u64, u64)>, Failed>;
    /// On the loop, once every task of the job's build has ended: the front end with every
    /// write applied, for the head's next build to lower into. None while a reader of it is
    /// alive or a write fails to apply: the next build lowers anew.
    fn keep(self: Box<Self>) -> Option<KeptFront>;
}

/// One job's front end, its units lowered: shared by the job's tasks, read only.
pub trait Front: Send + Sync + 'static {
    fn as_any(&self) -> &dyn Any;
    /// On the loop, as unit `unit` (an index into `Units::graph.units`) is about to start:
    /// whether nothing of it will be made again (cargo's `Fresh`).
    fn fresh(&self, unit: usize) -> bool;
    /// On a worker: unit `unit` (any mode but `RunCustomBuild`) checked and, unless the goal
    /// is [`Goal::Check`] or the unit is a check, compiled and linked. Every unit it depends on
    /// is done, the runs of build scripts it depends on applied. `images` are the kept images
    /// at the unit's outputs (`UnitPlan::outputs`): it takes each it relinks and puts back
    /// what it linked.
    fn compile(&self, reader: Reader, unit: usize, images: &mut Vec<Image>) -> Result<Compiled, Failed>;
    /// On the loop: the run of build script unit `unit` (mode `RunCustomBuild`). Its run
    /// reports [`Response::BuildScript`] (and cargo's warnings as diagnostics), and before it
    /// ends applies its directives as cargo does to the units they reach (`rustc-cfg`,
    /// `rustc-env`, `rustc-link-lib`, `rustc-link-search`, `DEP_<links>_*` of dependents).
    /// It ends not ok for a script that failed, after the diagnostic cargo gives.
    fn script(&self, unit: usize, tags: u64) -> Result<Box<dyn Runs>, ApiError>;
    /// On the loop, once every unit is built: the tests of the build (their artifacts in
    /// `built`) that `selection` selects, each reported as a [`Response::TestResult`]; it ends
    /// not ok when one failed. It runs compiled binaries and reads nothing of the front end.
    fn tests(&self, built: &[Artifact], selection: Selection<'_>, tags: u64) -> Result<Box<dyn Runs>, ApiError>;
    /// On a worker, for `blaze test --ub` once every unit is checked: the tests `selection`
    /// selects (as libtest would: its filters, `--exact`, `#[ignore]`), by name in libtest's
    /// order, each run by [`Front::evaluate`].
    fn ub_tests(&self, reader: Reader, selection: Selection<'_>) -> Result<Vec<String>, Failed>;
    /// On a worker: the test named `test` of [`Front::ub_tests`] run in blaze's MIR
    /// evaluator under `ub` (`hir_ty_ub::run_test`), nothing compiled. It passes, fails as a
    /// panicking test does (honoring `#[should_panic]`), or fails with the evaluator's report
    /// then `seed: N` as its output for undefined behavior, or with what the evaluator can't
    /// run: never a pass and never a compiled run in its place.
    fn evaluate(&self, reader: Reader, test: &str, ub: Ub) -> Result<TestResult, Failed>;
    /// On the loop, once every unit is built: `bin` (cargo's choice when none) run with
    /// `args`, reported as [`Response::Ran`].
    fn run(&self, built: &[Artifact], bin: Option<&str>, args: &[String], tags: u64)
    -> Result<Box<dyn Runs>, ApiError>;
    /// On a worker: what the change from `from`'s view to this one reaches, building nothing.
    fn reach(&self, reader: Reader, from: &dyn Front, from_reader: Reader, changes: &[Change]) -> Result<Reach, Failed>;
    /// On a worker: each codegen unit's inputs and output of the view's last build, as
    /// [`Response::CodegenInputs`] lists them.
    fn codegen_inputs(&self, reader: Reader) -> Result<Vec<(ContentId, ContentId)>, Failed>;
    /// On a worker: the front end's tables, each by its owner, measured through the job's
    /// `writer` ([`Writer::tables`]), with `images`, the linker's tables of the images kept
    /// when the loop posted it ([`Response::Tables`]).
    fn tables(&self, writer: Box<dyn Writer>, images: &[(&'static str, u64, u64)]) -> Result<Vec<Table>, Failed>;
}

/// User code of a job run through the loop's driver: it asks with effects whose tags are in
/// the range it was given (`tags..tags + 2^32`), takes their completions, and reports. Each
/// method that can end it says so with `Some(ok)`; after that it is asked nothing more. A
/// build script's is given its job's [`Writer`] to apply its run with; tests and runs, none.
pub trait Runs: Send {
    /// Asks for what it runs first; it may end at once (a result the store kept).
    fn start(
        &mut self,
        store: &Store,
        writer: Option<&mut dyn Writer>,
        effects: &mut Vec<Effect>,
        out: &mut Vec<Response>,
    ) -> Option<bool>;
    fn owns(&self, tag: Tag) -> bool;
    fn complete(
        &mut self,
        completion: Completion,
        store: &mut Store,
        writer: Option<&mut dyn Writer>,
        effects: &mut Vec<Effect>,
        out: &mut Vec<Response>,
    ) -> Option<bool>;
    /// Ends it early: what it runs is killed and ends through `complete`, not ok; with nothing
    /// running it ends here.
    fn cancel(&mut self, effects: &mut Vec<Effect>) -> Option<bool>;
}

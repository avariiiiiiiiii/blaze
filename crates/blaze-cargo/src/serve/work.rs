//! What a job of the daemon asks of the build (docs/v2/cargo-notes.md section 6): the steps
//! between a plan and its outputs, behind traits so the loop runs the same in the daemon
//! ([`crate::serve::unit::Real`]) and in the simulation ([`crate::serve::tests`]).
//!
//! Where each is called is part of its contract:
//!
//! - **On an IO thread of the plane**: [`Steps::fetch`], [`Steps::library`]. They may block on
//!   the network or the disk; they touch no state of the loop.
//! - **On a worker of the plane**: [`Steps::lower`], [`Front::compile`], [`Front::reach`]. They
//!   read the database, the VFS's reader and the store through [`JobInput`], and give back what
//!   the loop writes ([`Compiled::puts`]); a [`Failed::Again`] is posted again whole.
//! - **On the loop**: [`Front::fresh`], [`Front::script`], [`Front::tests`], [`Front::run`] and
//!   every [`Runs`] method. User code runs as [`blaze_core::Effect::Spawn`]s of the loop's
//!   driver, in the sandbox its [`blaze_core::Confine`] makes, never on a thread of its own.

use std::{
    any::Any,
    sync::{Arc, RwLock, atomic::AtomicBool},
};

use blaze_api::{ApiError, Artifact, Diagnostic, Reach, Response, Table};
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
    /// The store, read by tasks; written only by the loop.
    pub store: Arc<RwLock<Store>>,
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
}

/// The build of one job, as the daemon has it: one value for every job.
pub trait Steps: Send + Sync + 'static {
    /// On an IO thread: the source `fetch` names, fetched and checked (a `.crate`'s sha256 and
    /// its paths kept inside it, a git tree at its commit, an index file as served).
    fn fetch(&self, fetch: &Fetch) -> Result<Source, ApiError>;
    /// On an IO thread: the standard library's `library/`, put into the VFS once: where it is
    /// named (`/rustc/<commit>/library`) and its files.
    fn library(&self) -> Result<(String, Source), ApiError>;
    /// On a worker: the job's units lowered into the front end.
    fn lower(&self, job: &JobInput) -> Result<Arc<dyn Front>, Failed>;
    /// On the loop, once at start: the queries of the daemon's database.
    fn register(&self, db: &mut Database) {
        blaze_units::query::register(db);
    }
    /// On a worker: the plan of `view` (opened with its [`BuildConfig`]).
    fn plan(&self, db: &Database, view: ViewId) -> Result<Planned, Failed> {
        match db.get::<Plan>(view, &()) {
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

/// One job's front end, its units lowered: shared by the job's tasks.
pub trait Front: Send + Sync + 'static {
    fn as_any(&self) -> &dyn Any;
    /// On the loop, as unit `unit` (an index into `Units::graph.units`) is about to start:
    /// whether nothing of it will be made again (cargo's `Fresh`).
    fn fresh(&self, unit: usize) -> bool;
    /// On a worker: unit `unit` (any mode but `RunCustomBuild`) checked and, unless the goal
    /// is [`Goal::Check`] or the unit is a check, compiled and linked. Every unit it depends on
    /// is done, the runs of build scripts it depends on applied.
    fn compile(&self, unit: usize) -> Result<Compiled, Failed>;
    /// On the loop: the run of build script unit `unit` (mode `RunCustomBuild`). Its run
    /// reports [`Response::BuildScript`] (and cargo's warnings as diagnostics), and before it
    /// ends applies its directives as cargo does to the units they reach (`rustc-cfg`,
    /// `rustc-env`, `rustc-link-lib`, `rustc-link-search`, `DEP_<links>_*` of dependents).
    /// It ends not ok for a script that failed, after the diagnostic cargo gives.
    fn script(&self, unit: usize, tags: u64) -> Result<Box<dyn Runs>, ApiError>;
    /// On the loop, once every unit is built: the tests of the build (their artifacts in
    /// `built`), filtered by name, the reached ones only unless `all`, each reported as a
    /// [`Response::TestResult`]; it ends not ok when one failed.
    fn tests(&self, built: &[Artifact], filter: &[String], all: bool, tags: u64)
    -> Result<Box<dyn Runs>, ApiError>;
    /// On the loop, once every unit is built: `bin` (cargo's choice when none) run with
    /// `args`, reported as [`Response::Ran`].
    fn run(&self, built: &[Artifact], bin: Option<&str>, args: &[String], tags: u64)
    -> Result<Box<dyn Runs>, ApiError>;
    /// On a worker: what the change from `from`'s view to this one reaches, building nothing.
    fn reach(&self, from: &dyn Front, changes: &[Change]) -> Result<Reach, Failed>;
    /// On a worker: each codegen unit's inputs and output of the view's last build, as
    /// [`Response::CodegenInputs`] lists them.
    fn codegen_inputs(&self) -> Result<Vec<(ContentId, ContentId)>, Failed>;
    /// On a worker: the front end's tables, each by its owner ([`Response::Tables`]).
    fn tables(&self) -> Result<Vec<Table>, Failed>;
}

/// User code of a job run through the loop's driver: it asks with effects whose tags are in
/// the range it was given (`tags..tags + 2^32`), takes their completions, and reports. Each
/// method that can end it says so with `Some(ok)`; after that it is asked nothing more.
pub trait Runs: Send {
    /// Asks for what it runs first; it may end at once (a result the store kept).
    fn start(&mut self, store: &Store, effects: &mut Vec<Effect>, out: &mut Vec<Response>) -> Option<bool>;
    fn owns(&self, tag: Tag) -> bool;
    fn complete(
        &mut self,
        completion: Completion,
        store: &mut Store,
        effects: &mut Vec<Effect>,
        out: &mut Vec<Response>,
    ) -> Option<bool>;
    /// Ends it early: what it runs is killed and ends through `complete`, not ok; with nothing
    /// running it ends here.
    fn cancel(&mut self, effects: &mut Vec<Effect>) -> Option<bool>;
}

//! The daemon's [`Builder`] (docs/v2/plan/lanes/cargo/notes.md section 6): each job of the
//! session as tasks of the plane, from its view to what it reports.
//!
//! A job that builds goes through, for each view it reads (two for a reach, else one):
//!
//! 1. The standard library's `library/`, put into the VFS once for every job and mounted
//!    where its sources are named (`/rustc/<commit>/library`).
//! 2. The view opened in the database with its [`BuildConfig`]: the invocation (cargo's, or
//!    made of an agent's [`Targets`]), the checkout's root and every source fetched so far.
//! 3. The load's [`Planned`] asked. Each source it [`Planned::Needs`] is fetched as an IO task
//!    (one fetch for every job that asks it), put into the VFS as a root and mounted durably;
//!    the view is opened again with the new mount and the plan asked again. A
//!    [`Planned::Failed`] finishes the job with its error.
//! 4. The plan's lock file written where cargo writes it, its warnings reported, and its units
//!    lowered into the front end.
//!
//! Then its work: every unit a node (compiles on workers, build scripts on the loop), each
//! reported as it starts, its outputs put into the store and placed where cargo places them,
//! then `Built`, the tests or the run, and `Work`; or the one answer a reach, a trace, a
//! verify, the codegen inputs or the tables give. A cancel skips what has not started, kills
//! what runs, and the job still finishes once, not ok.

#![forbid(unsafe_code)]

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use blaze_api::{
    ApiError, Artifact, Diagnostic, FetchVerb, Level, LinkKind, Mismatch, Response, Targets, TestOutcome,
    TestResult, Traced, Ub, UnitStarted, Verb, Worked,
    build::{Builder, JobId, Opened, Work},
};
use blaze_cas::{Placed, Store};
use blaze_core::{Completion, Done, Effect, FileName, Tag};
use blaze_graph::{
    Key, Outcome, Pool, RequestId, RequestStats,
    admission::TaskKind,
    machine::NodeId,
    plane::NodeRef,
};
use blaze_ids::{ContentId, Hasher};
use blaze_protocol::events::NodeKind;
use blaze_query::{Database, QueryKey};
use blaze_units::{
    query::{BuildConfig, Command, Fetch, FetchKind, Invocation, Mount, Origin, Planned},
    schema::Mode as UnitMode,
};
use blaze_vfs::{Ticket, ViewId, Vfs};
use rustc_hash::{FxHashMap, FxHashSet};

use super::{
    heads::Heads,
    mounts::Mounts,
    plane::{self, Did, Fresh, Happened, Plane, Posting, Task, kinds},
    work::{
        Compiled, Failed, Front, FrontEnd, Goal, Image, JobInput, Kept, Reader, Readers, Runs, Selection, Source, Steps,
        Writer,
    },
};

/// How many tags one run of user code uses ([`Runs`]).
const RUN_TAGS: u64 = 1 << 32;

/// What a thread task of a job hands the loop.
pub enum Value {
    Library(Result<(String, Source), ApiError>),
    /// The fetch at its place in its batch ([`Fetching::fetches`]).
    Fetched(Arc<[Fetch]>, u32, Result<Source, ApiError>),
    Planned(Result<Planned, ApiError>),
    Lowered(Result<FrontEnd, ApiError>),
    /// A unit's compile, and the images it relinked, given back to the loop.
    Compiled(usize, Result<Compiled, ApiError>, Vec<Image>),
    Answer(Result<Response, ApiError>),
    /// `blaze test --ub`: the tests selected, and one's result.
    Selected(Result<Vec<String>, ApiError>),
    Evaluated(Result<TestResult, ApiError>),
}

/// What jobs tell the session, in order.
#[derive(Debug, PartialEq)]
pub enum Said {
    Report(JobId, Response),
    Finish(JobId, bool, Option<ApiError>),
}

/// What the jobs work with, lent by the loop for one call.
pub struct World<'a> {
    pub now: Duration,
    pub vfs: &'a mut Vfs,
    pub db: &'a Arc<Database>,
    pub store: &'a mut Store,
    pub plane: &'a mut Plane<Value>,
    pub mounts: &'a mut Mounts,
    pub heads: &'a Heads,
    pub steps: &'a Arc<dyn Steps>,
    pub effects: &'a mut Vec<Effect>,
    pub said: &'a mut Vec<Said>,
}

/// What the jobs need of the daemon besides its parts.
pub struct Setup {
    /// Where an agent's build of a root no checkout has is placed: `<state>/views`.
    pub views: String,
    /// The triple of the machine the daemon runs on.
    pub host: String,
    /// The daemon's `PATH` and `HOME`: an agent's invocation's whole environment.
    pub env: Vec<(String, String)>,
    pub color: bool,
    /// The lock writes and modes: `tags..tags + 2^32`.
    pub tags: u64,
    /// User code's runs take ranges of `2^32` from `runs` up to `runs_end`.
    pub runs: u64,
    pub runs_end: u64,
}

/// The single-flight key of the standard library's source.
fn library_key() -> Key {
    Key(ContentId::of("blaze v2 serve library", b"library").0)
}

/// The single-flight key of a fetch: every job asking the same source shares one download.
fn fetch_key(fetch: &Fetch) -> Key {
    let mut hasher = Hasher::new("blaze v2 serve fetch");
    hasher.part(fetch.at.as_bytes()).part(fetch.url.as_bytes());
    match &fetch.kind {
        FetchKind::Crate { sha256 } => hasher.part(b"crate").part(sha256.as_bytes()),
        FetchKind::IndexFile => hasher.part(b"index"),
        FetchKind::Git { commit } => hasher.part(b"git").part(commit.as_bytes()),
    };
    Key(hasher.finish().0)
}

/// A step's work as a task: a give-way posts it again, unless the job is cancelled.
/// The work of a batch of thread tasks: `step` of a task's `arg`, and the value it is told to
/// the loop as.
fn task<R: 'static>(
    cancelled: &Arc<AtomicBool>,
    step: impl Fn(u32, Readers) -> Result<R, Failed> + Send + Sync + 'static,
    value: impl Fn(u32, Result<R, ApiError>) -> Value + Send + Sync + 'static,
) -> plane::Work<Value> {
    let cancelled = cancelled.clone();
    Arc::new(move |arg, readers| match step(arg, readers) {
        Ok(it) => Some(Did { outcome: Outcome::Built, value: value(arg, Ok(it)) }),
        Err(Failed::Error(error)) => {
            Some(Did { outcome: Outcome::Failed { why: error.what.clone() }, value: value(arg, Err(error)) })
        }
        Err(Failed::Again) if cancelled.load(Ordering::Acquire) => Some(Did {
            outcome: Outcome::Failed { why: "cancelled".to_owned() },
            value: value(arg, Err(ApiError::new("cancelled", None))),
        }),
        Err(Failed::Again) => None,
    })
}

/// The reader of a task's own front end, which a task of a batch that reads is posted with.
fn this(readers: Readers) -> Reader {
    readers.this.expect("a task that reads is posted with its reader")
}

/// One thread task of kind `kind`, the batch's only one.
fn one(kind: TaskKind) -> Task {
    Task::Thread { kind, arg: 0 }
}

/// A node's name, `a` then `b`, in one allocation of its length.
fn named(a: &str, b: &str) -> String {
    let mut name = String::with_capacity(a.len() + b.len());
    name.push_str(a);
    name.push_str(b);
    name
}

/// A node of cost 1, its memory to be predicted from its kind.
fn node<'a>(kind: NodeKind, name: &'a str, deps: &'a [NodeId], pool: Pool, key: Option<Key>) -> NodeRef<'a> {
    NodeRef { kind, name, deps, pool, cost: 1, key, memory: 0 }
}

/// A batch of one thread task of kind `task`, with no deps.
fn lone(plane: &mut Plane<Value>, kind: NodeKind, name: &str, pool: Pool, key: Option<Key>, task: TaskKind) -> Fresh {
    let mut fresh = plane.fresh(1, name.len(), 0);
    plane.push(&mut fresh, node(kind, name, &[], pool, key), one(task));
    fresh
}

/// What a job's work asks of cargo: its command.
fn command_of(work: &Work) -> Command {
    match work {
        Work::Build(_) | Work::CodegenInputs(_) => Command::Build,
        Work::Test { .. } => Command::Test,
        Work::Run { .. } => Command::Run,
        Work::Check(_) | Work::Reach { .. } | Work::Verify(_) | Work::Trace(_) | Work::Tables | Work::Snapshot => {
            Command::Check
        }
    }
}

fn goal_of(work: &Work) -> Goal {
    match work {
        Work::Build(_) | Work::CodegenInputs(_) => Goal::Build,
        Work::Test { ub: Some(_), .. } => Goal::Check,
        Work::Test { .. } => Goal::Test,
        Work::Run { .. } => Goal::Run,
        Work::Reach { .. } => Goal::Reach,
        Work::Check(_) | Work::Verify(_) | Work::Trace(_) | Work::Tables | Work::Snapshot => Goal::Check,
    }
}

fn targets_of(work: &Work) -> Option<&Targets> {
    match work {
        Work::Check(targets)
        | Work::Build(targets)
        | Work::Verify(targets)
        | Work::CodegenInputs(targets)
        | Work::Trace(targets)
        | Work::Test { targets, .. }
        | Work::Run { targets, .. }
        | Work::Reach { targets, .. } => Some(targets),
        Work::Tables | Work::Snapshot => None,
    }
}

/// cargo's command line for an agent's targets, run at `root` with only the daemon's `PATH`
/// and `HOME`.
pub fn invocation(work: &Work, targets: Option<&Targets>, root: &str, env: &[(String, String)]) -> Invocation {
    if let Some(cargo) = targets.and_then(|it| it.cargo.as_ref()) {
        let command = match cargo.command.as_str() {
            "build" | "b" => Command::Build,
            "test" | "t" => Command::Test,
            "bench" => Command::Bench,
            "run" | "r" => Command::Run,
            _ => Command::Check,
        };
        return Invocation {
            command,
            args: cargo.args.clone(),
            root: cargo.root.clone(),
            cwd: cargo.cwd.clone(),
            env: cargo.env.clone(),
            files: cargo.files.clone(),
        };
    }
    let mut args = Vec::new();
    if let Some(targets) = targets {
        for package in &targets.packages {
            args.extend(["-p".to_owned(), package.clone()]);
        }
        if !targets.profile.is_empty() {
            args.extend(["--profile".to_owned(), targets.profile.clone()]);
        }
        if let Some(target) = &targets.target {
            args.extend(["--target".to_owned(), target.clone()]);
        }
        if !targets.features.is_empty() {
            args.extend(["-F".to_owned(), targets.features.join(",")]);
        }
        if targets.all_features {
            args.push("--all-features".to_owned());
        }
        if targets.no_default_features {
            args.push("--no-default-features".to_owned());
        }
    }
    if let Work::Run { bin: Some(bin), .. } = work {
        args.extend(["--bin".to_owned(), bin.clone()]);
    }
    Invocation {
        command: command_of(work),
        args,
        root: root.to_owned(),
        cwd: root.to_owned(),
        env: env.to_vec(),
        files: Vec::new(),
    }
}

/// A view being made ready: its config, its database view, and what its load asked.
struct Prep {
    opened: Opened,
    root_dir: String,
    config: Arc<BuildConfig>,
    queries: ViewId,
    /// Every fetch's `at` this view's load asked for: asked again once mounted is a loop.
    asked: FxHashSet<String>,
}

/// A view made ready: lowered, or planned only for the work that walks the plan.
struct Ready {
    input: JobInput,
    front: Option<Arc<dyn Front>>,
    writer: Option<Box<dyn Writer>>,
}

enum Stage {
    Library(NodeId),
    Plan(NodeId, Prep),
    Fetch(Fetching),
    Lower(NodeId, JobInput),
    Units(Box<Build>),
    Answer(NodeId),
    /// The tests or the run, once every unit is built: its first tag, and it.
    Runs(u64, Box<dyn Runs>),
    /// Waits for the VFS's commit of the heads and the store's last put before this.
    Snapshot { ticket: Option<Ticket>, heads: Vec<(String, ContentId)>, store: Option<blaze_cas::Ticket> },
    Over,
}

/// What the plan needs fetched: fetch `at` is node `first + at`.
struct Fetching {
    first: NodeId,
    fetches: Arc<[Fetch]>,
    /// How many have not settled.
    left: u32,
    prep: Prep,
}

struct Build {
    input: JobInput,
    front: Arc<dyn Front>,
    /// The front end, written only here, on the loop: each task that reads it is posted with
    /// a reader made from it then, and a write waits for those readers to end.
    writer: Box<dyn Writer>,
    /// Reading tasks the plane asked to post while a write waited, posted once it is applied.
    held: Vec<(Posting, bool)>,
    /// Unit `at` is node `first + at`.
    first: NodeId,
    /// Each unit's package, by its place in the plan's packages; `u32::MAX` for none.
    package_of: Vec<u32>,
    /// The packages reported started, a bit each.
    reported: Vec<u64>,
    /// The build scripts that run: their first tag, and the run.
    scripts: FxHashMap<NodeId, (u64, Box<dyn Runs>)>,
    artifacts: Vec<Artifact>,
    /// `blaze test --ub`'s tests, run in the evaluator once every unit is checked.
    evaluation: Option<Evaluation>,
}

struct Evaluation {
    ub: Ub,
    /// The node that selects the tests, once every unit is checked.
    select: NodeId,
    /// A test failed.
    failed: bool,
}

impl Build {
    fn unit(&self, node: NodeId) -> Option<usize> {
        let at = node.0.wrapping_sub(self.first.0) as usize;
        (at < self.package_of.len()).then_some(at)
    }
}

struct Job {
    work: Work,
    views: Vec<Opened>,
    ready: Vec<Ready>,
    request: Option<RequestId>,
    closed: bool,
    finished: bool,
    cancelled: Arc<AtomicBool>,
    stage: Stage,
    values: FxHashMap<NodeId, Value>,
    /// Index files and git repositories reported as `Updating`, once each.
    updating: FxHashSet<String>,
    /// Opened by the job and closed as it finishes.
    queries: Vec<ViewId>,
    placing: usize,
    /// The first ids of its batches of nodes ([`Jobs::nodes`]).
    batches: Vec<NodeId>,
    /// Its runs of user code that have not ended.
    runs: u32,
    over: Option<(bool, Option<ApiError>)>,
    stats: RequestStats,
    /// Units started that were fresh, and that were made again.
    fresh: u64,
    compiled: u64,
}

/// The jobs of the session's [`Builder`], as the loop steps them.
pub struct Jobs {
    setup: Setup,
    started: Vec<(JobId, Option<Opened>, Work)>,
    cancels: Vec<JobId>,
    /// Walked in id order through [`Jobs::order`] wherever the walk says something.
    jobs: FxHashMap<JobId, Job>,
    /// The jobs' ids sorted, reused by each walk that must go in id order.
    order: Vec<JobId>,
    /// Whose each batch of nodes is, by its first id: (how many, the job). A node's batch is
    /// the last one starting at or below it, O(log batches) with nothing kept per node.
    // BTreeMap: range queries by node id.
    nodes: BTreeMap<u32, (u32, JobId)>,
    by_request: FxHashMap<RequestId, JobId>,
    library: Option<String>,
    library_failed: Option<ApiError>,
    fetch_failed: FxHashMap<String, ApiError>,
    /// The store's placements: the job, and whether it is executable.
    placing: FxHashMap<Tag, (JobId, bool)>,
    /// The job of each lock write and mode of the loop's own.
    own: FxHashMap<Tag, (JobId, String)>,
    next_own: u64,
    /// Whose each run of user code is (the job, and the script's node), by its number less
    /// `runs_from`: run `n` has the tags `setup.runs + n * 2^32` on. Ended runs at the front
    /// are let go, so a completion finds its run in O(1).
    runs: VecDeque<Option<(JobId, Option<NodeId>)>>,
    runs_from: u64,
    /// The store's last put: its tickets are made in order, so all before it are durable once
    /// it is.
    put: Option<blaze_cas::Ticket>,
    /// Every image kept linked, by the path cargo writes it at, for the daemon's life: each is
    /// moved into the compile that relinks it and back once it ends ([`Image`]).
    images: FxHashMap<String, Box<dyn Kept>>,
    /// A job ran a query or a unit since the image was written.
    pub cold: bool,
    /// A job finished since the last collection.
    pub ended: bool,
}

impl Builder for Jobs {
    fn start(&mut self, job: JobId, view: Option<Opened>, work: Work) {
        self.started.push((job, view, work));
    }

    fn cancel(&mut self, job: JobId) {
        self.cancels.push(job);
    }
}

impl Jobs {
    pub fn new(setup: Setup) -> Jobs {
        Jobs {
            setup,
            started: Vec::new(),
            cancels: Vec::new(),
            jobs: FxHashMap::default(),
            order: Vec::new(),
            nodes: BTreeMap::new(),
            by_request: FxHashMap::default(),
            library: None,
            library_failed: None,
            fetch_failed: FxHashMap::default(),
            placing: FxHashMap::default(),
            own: FxHashMap::default(),
            next_own: 0,
            runs: VecDeque::new(),
            runs_from: 0,
            put: None,
            images: FxHashMap::default(),
            cold: false,
            ended: false,
        }
    }

    /// Whether no job is under way.
    pub fn idle(&self) -> bool {
        self.jobs.is_empty() && self.started.is_empty()
    }

    /// The roots the jobs' views read, kept by a collection.
    pub fn roots(&self) -> Vec<ContentId> {
        let mut jobs: Vec<(&JobId, &Job)> = self.jobs.iter().collect();
        blaze_ids::radix::sort_by_key(&mut jobs, |(id, _)| id.0);
        jobs.into_iter().flat_map(|(_, it)| it.views.iter().map(|view| view.view.root)).collect()
    }

    /// The jobs' ids in order, in [`Jobs::order`]'s buffer: give it back once done.
    fn sorted(&mut self) -> Vec<JobId> {
        let mut order = std::mem::take(&mut self.order);
        order.clear();
        order.extend(self.jobs.keys().copied());
        blaze_ids::radix::sort_copy_by_key(&mut order, |it| it.0);
        order
    }

    pub fn owns(&self, tag: Tag) -> bool {
        if tag.0.wrapping_sub(self.setup.tags) < 1 << 32 {
            return true;
        }
        tag.0 >= self.setup.runs && tag.0 < self.setup.runs_end
    }

    /// Takes what the session asked since the last call.
    pub fn step(&mut self, w: &mut World<'_>) {
        if !self.started.is_empty() {
            let mut started = std::mem::take(&mut self.started);
            for (id, view, work) in started.drain(..) {
                self.begin(w, id, view, work);
            }
            if self.started.is_empty() {
                self.started = started;
            }
        }
        if !self.cancels.is_empty() {
            let mut cancels = std::mem::take(&mut self.cancels);
            for id in cancels.drain(..) {
                self.cancel_job(w, id);
            }
            if self.cancels.is_empty() {
                self.cancels = cancels;
            }
        }
        self.wind_up(w);
    }

    fn begin(&mut self, w: &mut World<'_>, id: JobId, view: Option<Opened>, work: Work) {
        let views = match (&work, view) {
            (Work::Reach { from, .. }, Some(to)) => vec![*from, to],
            (_, Some(view)) => vec![view],
            (_, None) => Vec::new(),
        };
        let keep_going = targets_of(&work)
            .and_then(|it| it.cargo.as_ref())
            .is_some_and(|cargo| cargo.args.iter().any(|arg| arg == "--keep-going"));
        let snapshot = matches!(work, Work::Snapshot);
        let mut job = Job {
            work,
            views,
            ready: Vec::new(),
            request: None,
            closed: false,
            finished: snapshot,
            cancelled: Arc::new(AtomicBool::new(false)),
            stage: Stage::Over,
            values: FxHashMap::default(),
            updating: FxHashSet::default(),
            queries: Vec::new(),
            placing: 0,
            batches: Vec::new(),
            runs: 0,
            over: None,
            stats: RequestStats::default(),
            fresh: 0,
            compiled: 0,
        };
        if snapshot {
            job.stage = self.snapshot(w);
            self.jobs.insert(id, job);
            return;
        }
        let request = w.plane.open(w.now, keep_going);
        job.request = Some(request);
        self.by_request.insert(request, id);
        self.jobs.insert(id, job);
        self.prepare(w, id);
    }

    /// Makes the next view of the job ready, the standard library first.
    fn prepare(&mut self, w: &mut World<'_>, id: JobId) {
        if self.library.is_none() {
            let job = self.jobs.get_mut(&id).expect("a job being prepared");
            let steps = w.steps.clone();
            let work = task(&job.cancelled, move |_, _| Ok(steps.library()?), |_, it| Value::Library(it));
            let name = "the standard library's source";
            let fresh = lone(w.plane, NodeKind::Std, name, Pool::Io, Some(library_key()), kinds::LIBRARY);
            job.stage = Stage::Library(fresh.first());
            self.add(w, id, fresh, Some(work), false);
            return;
        }
        let job = self.jobs.get(&id).expect("a job being prepared");
        let opened = job.views[job.ready.len()];
        let prep = self.open(w, id, opened, FxHashSet::default());
        self.plan(w, id, prep);
    }

    /// The view opened in the database with its build configuration as the mounts are now.
    fn open(&mut self, w: &mut World<'_>, id: JobId, opened: Opened, asked: FxHashSet<String>) -> Prep {
        let job = self.jobs.get_mut(&id).expect("a job opening a view");
        let targets = targets_of(&job.work);
        let root_dir = match targets.and_then(|it| it.cargo.as_ref()) {
            Some(cargo) => cargo.root.clone(),
            None => w
                .heads
                .path_of(w.vfs, opened.view.root)
                .unwrap_or_else(|| format!("{}/{:x}", self.setup.views, opened.view.root)),
        };
        let library = self.library.clone().expect("the library is mounted before a view is opened");
        let mut mounts = vec![Mount { at: root_dir.clone(), origin: Origin::View, under: String::new() }];
        mounts.extend(w.mounts.all().cloned());
        mounts.sort_by(|a, b| a.at.cmp(&b.at));
        let config = BuildConfig {
            invocation: invocation(&job.work, targets, &root_dir, &self.setup.env),
            mounts,
            host: self.setup.host.clone(),
            library,
        };
        let queries = w.db.open_with(opened.view, &config.encode());
        job.queries.push(queries);
        Prep { opened, root_dir, config: Arc::new(config), queries, asked }
    }

    fn plan(&mut self, w: &mut World<'_>, id: JobId, prep: Prep) {
        let job = self.jobs.get_mut(&id).expect("a job planning");
        let (steps, db, queries) = (w.steps.clone(), w.db.clone(), prep.queries);
        let work = task(&job.cancelled, move |_, _| steps.plan(&db, queries), |_, it| Value::Planned(it));
        let name = named("the plan of ", &prep.root_dir);
        let fresh = lone(w.plane, NodeKind::Manifest, &name, Pool::Work, None, kinds::PLAN);
        job.stage = Stage::Plan(fresh.first(), prep);
        self.add(w, id, fresh, Some(work), false);
    }

    /// A batch of the job's nodes; with `reads`, its tasks read the job's front end.
    fn add(&mut self, w: &mut World<'_>, id: JobId, fresh: Fresh, work: Option<plane::Work<Value>>, reads: bool) {
        let job = self.jobs.get_mut(&id).expect("a job adding nodes");
        let request = job.request.expect("a job that builds has a request");
        let (first, count) = (fresh.first(), fresh.count());
        if count > 0 {
            job.batches.push(first);
            self.nodes.insert(first.0, (count, id));
        }
        if let Err(error) = w.plane.add(w.now, request, fresh, work, reads) {
            self.end(w, id, false, Some(ApiError::new(format!("the plane refused the job's tasks: {error}"), None)));
        }
    }

    /// A task of a batch that reads posted with readers of its job's front ends as they stand
    /// now, after every write the loop applied before it. While a write of its job waits for
    /// the readers before it, the task is held and posted once the write is applied
    /// ([`Jobs::apply_writes`]). A compile is given the images kept at its outputs.
    fn post(&mut self, w: &mut World<'_>, task: Posting, again: bool) {
        let id = self.job_of(task.id);
        let job = id.and_then(|id| self.jobs.get_mut(&id));
        let readers = match job {
            Some(Job { stage: Stage::Units(build), .. }) => {
                let Some(this) = build.writer.reader() else {
                    build.held.push((task, again));
                    return;
                };
                let images = match task.kind == kinds::COMPILE {
                    true => take_images(&mut self.images, &build.input.units, task.arg as usize),
                    false => Vec::new(),
                };
                Some(Readers { this: Some(this), from: None, images })
            }
            Some(job) if matches!(job.stage, Stage::Answer(_)) => match job.work {
                Work::Tables => job.ready.last_mut().and_then(|it| it.writer.take()).map(|writer| Readers {
                    writer: Some(writer),
                    ..Readers::default()
                }),
                _ => (|| {
                    let this = job.ready.last()?.writer.as_ref()?.reader()?;
                    let from = match job.work {
                        Work::Reach { .. } => Some(job.ready.first()?.writer.as_ref()?.reader()?),
                        _ => None,
                    };
                    Some(Readers { this: Some(this), from, ..Readers::default() })
                })(),
            },
            _ => None,
        };
        match readers {
            Some(readers) => w.plane.post(task, again, readers),
            None => w.plane.refuse(w.now, task, "its job has no front end to read now".to_owned()),
        }
    }

    /// Each job's waiting writes applied where no reader of its front end is left, and the
    /// tasks held for them posted with readers of what they made. Asking a job whose readers
    /// still run costs one load of its reference count. Called as the loop winds up each
    /// step: a reader is dropped as its task's work returns, before the plane hears the task
    /// ended, so the step that hears the last one applies the write. The daemon calls it
    /// once a turn as well, for a reader that outlives its task.
    pub fn apply_writes(&mut self, w: &mut World<'_>) {
        let order = self.sorted();
        let mut posts = Vec::new();
        for id in &order {
            let job = self.jobs.get_mut(id).expect("a job in the order");
            let Stage::Units(build) = &mut job.stage else { continue };
            if !build.writer.waiting() && build.held.is_empty() {
                continue;
            }
            match build.writer.apply() {
                Ok(true) => posts.append(&mut build.held),
                Ok(false) => {}
                Err(error) => {
                    let held = std::mem::take(&mut build.held);
                    for (task, _) in held {
                        w.plane.refuse(w.now, task, format!("applying a build script's run or a proc macro: {}", error.what));
                    }
                    if !job.cancelled.load(Ordering::Acquire) {
                        job.over.get_or_insert((false, Some(error)));
                    }
                    self.end(w, *id, false, None);
                }
            }
        }
        self.order = order;
        for (task, again) in posts {
            self.post(w, task, again);
        }
    }

    /// The job whose node `node` is.
    fn job_of(&self, node: NodeId) -> Option<JobId> {
        let (first, (count, id)) = self.nodes.range(..=node.0).next_back()?;
        (node.0 - first < *count).then_some(*id)
    }

    /// What the plane did.
    pub fn happened(&mut self, w: &mut World<'_>, happened: &mut Vec<Happened<Value>>) {
        for it in happened.drain(..) {
            match it {
                Happened::Did { id, value } => self.did(w, id, value),
                Happened::Started { id } => self.started_unit(w, id),
                Happened::Post { task, again } => self.post(w, task, again),
                Happened::Run { id } => self.run_script(w, id),
                Happened::Settled { id, ok, why } => self.settled(w, id, ok, why),
                Happened::Finished { request, ok, stats } => self.plane_finished(w, request, ok, stats),
            }
        }
        self.wind_up(w);
    }

    fn did(&mut self, w: &mut World<'_>, node: NodeId, value: Value) {
        match value {
            Value::Library(Ok((at, source))) => match w.mounts.mount(w.vfs, &at, &source) {
                Ok(_) => self.library = Some(at),
                Err(why) => {
                    self.library_failed = Some(ApiError::new(format!("the standard library's source: {why}"), Some(at)));
                }
            },
            Value::Library(Err(error)) => self.library_failed = Some(error),
            Value::Fetched(fetches, at, Ok(source)) => {
                let fetch = &fetches[at as usize];
                if let Err(why) = w.mounts.mount(w.vfs, &fetch.at, &source) {
                    self.fetch_failed.insert(fetch.at.clone(), ApiError::new(why, Some(fetch.what.clone())));
                }
            }
            Value::Fetched(fetches, at, Err(error)) => {
                self.fetch_failed.insert(fetches[at as usize].at.clone(), error);
            }
            Value::Compiled(unit, result, images) => {
                self.images.extend(images.into_iter().map(|Image { path, kept }| (path, kept)));
                self.compiled(w, node, unit, result);
            }
            Value::Evaluated(result) => {
                let Some(id) = self.job_of(node) else { return };
                let Some(job) = self.jobs.get_mut(&id) else { return };
                match result {
                    Ok(result) => {
                        if result.outcome == TestOutcome::Failed
                            && let Stage::Units(build) = &mut job.stage
                            && let Some(evaluation) = &mut build.evaluation
                        {
                            evaluation.failed = true;
                        }
                        w.said.push(Said::Report(id, Response::TestResult(result)));
                    }
                    Err(error) => {
                        if !job.cancelled.load(Ordering::Acquire) {
                            job.over.get_or_insert((false, Some(error)));
                        }
                    }
                }
            }
            value => {
                if let Some(job) = self.job_of(node).and_then(|id| self.jobs.get_mut(&id)) {
                    job.values.insert(node, value);
                }
            }
        }
    }

    fn settled(&mut self, w: &mut World<'_>, node: NodeId, ok: bool, why: Option<String>) {
        let Some(id) = self.job_of(node) else { return };
        let Some(job) = self.jobs.get_mut(&id) else { return };
        if job.over.is_some() {
            job.values.remove(&node);
            return;
        }
        let why = why.unwrap_or_default();
        match std::mem::replace(&mut job.stage, Stage::Over) {
            Stage::Library(at) if at == node => {
                if self.library.is_some() {
                    self.prepare(w, id);
                } else {
                    let error = self
                        .library_failed
                        .clone()
                        .unwrap_or_else(|| ApiError::new(format!("the standard library's source: {why}"), None));
                    self.end(w, id, false, Some(error));
                }
            }
            Stage::Plan(at, prep) if at == node => {
                let value = job.values.remove(&node);
                match value {
                    Some(Value::Planned(Ok(planned))) => self.planned(w, id, prep, planned),
                    Some(Value::Planned(Err(error))) => self.end(w, id, false, Some(error)),
                    _ => self.end(w, id, false, Some(ApiError::new(format!("the plan of {}: {why}", prep.root_dir), None))),
                }
            }
            Stage::Fetch(mut fetching) => {
                let place = node.0.wrapping_sub(fetching.first.0) as usize;
                if place >= fetching.fetches.len() {
                    job.stage = Stage::Fetch(fetching);
                    return;
                }
                if !ok {
                    let at = &fetching.fetches[place].at;
                    let error = self
                        .fetch_failed
                        .get(at)
                        .cloned()
                        .unwrap_or_else(|| ApiError::new(format!("fetching {at}: {why}"), Some(at.clone())));
                    job.over.get_or_insert((false, Some(error)));
                }
                fetching.left -= 1;
                if fetching.left > 0 {
                    job.stage = Stage::Fetch(fetching);
                    return;
                }
                let prep = fetching.prep;
                if let Some((_, error)) = job.over.take() {
                    self.end(w, id, false, error);
                    return;
                }
                w.db.close(prep.queries);
                job.queries.retain(|it| *it != prep.queries);
                let again = self.open(w, id, prep.opened, prep.asked);
                self.plan(w, id, again);
            }
            Stage::Lower(at, input) if at == node => match job.values.remove(&node) {
                Some(Value::Lowered(Ok(FrontEnd { front, writer }))) => {
                    job.ready.push(Ready { input, front: Some(front), writer: Some(writer) });
                    self.next(w, id);
                }
                Some(Value::Lowered(Err(error))) => self.end(w, id, false, Some(error)),
                _ => self.end(w, id, false, Some(ApiError::new(format!("lowering the units: {why}"), None))),
            },
            Stage::Answer(at) if at == node => match job.values.remove(&node) {
                Some(Value::Answer(Ok(response))) => {
                    w.said.push(Said::Report(id, response));
                    self.end(w, id, true, None);
                }
                Some(Value::Answer(Err(error))) => self.end(w, id, false, Some(error)),
                _ => self.end(w, id, false, Some(ApiError::new(why, None))),
            },
            Stage::Units(build) if build.evaluation.as_ref().is_some_and(|it| it.select == node) => {
                let value = job.values.remove(&node);
                job.stage = Stage::Units(build);
                match value {
                    Some(Value::Selected(Ok(tests))) => self.evaluate(w, id, tests),
                    Some(Value::Selected(Err(error))) => self.end(w, id, false, Some(error)),
                    _ => self.end(w, id, false, Some(ApiError::new(format!("selecting the tests: {why}"), None))),
                }
            }
            stage => job.stage = stage,
        }
    }

    fn planned(&mut self, w: &mut World<'_>, id: JobId, mut prep: Prep, planned: Planned) {
        match planned {
            Planned::Failed(error) => self.end(w, id, false, Some(ApiError::new(error.what, error.at))),
            Planned::Needs(fetches) => {
                let job = self.jobs.get_mut(&id).expect("a job planning");
                let mut wanted = Vec::with_capacity(fetches.len());
                for fetch in fetches {
                    if !prep.asked.insert(fetch.at.clone()) {
                        let error = ApiError::new(
                            format!("the load asked for {} again after it was fetched and mounted at {}", fetch.what, fetch.at),
                            Some(fetch.at.clone()),
                        );
                        self.end(w, id, false, Some(error));
                        return;
                    }
                    if w.mounts.has(&fetch.at) {
                        continue;
                    }
                    let verb = match &fetch.kind {
                        FetchKind::Crate { .. } => Some((FetchVerb::Downloaded, fetch.what.clone())),
                        FetchKind::IndexFile | FetchKind::Git { .. } => {
                            job.updating.insert(fetch.what.clone()).then(|| (FetchVerb::Updating, fetch.what.clone()))
                        }
                    };
                    if let Some((verb, what)) = verb {
                        w.said.push(Said::Report(id, Response::Fetched { verb, what }));
                    }
                    wanted.push(fetch);
                }
                if wanted.is_empty() {
                    w.db.close(prep.queries);
                    job.queries.retain(|it| *it != prep.queries);
                    let again = self.open(w, id, prep.opened, prep.asked);
                    self.plan(w, id, again);
                    return;
                }
                let count = u32::try_from(wanted.len()).expect("under 2^32 fetches");
                let fetches: Arc<[Fetch]> = wanted.into();
                const FETCH: &str = "fetch ";
                let names = fetches.iter().map(|it| FETCH.len() + it.what.len()).sum();
                let mut fresh = w.plane.fresh(count, names, 0);
                let mut name = String::new();
                for (at, fetch) in (0u32..).zip(fetches.iter()) {
                    name.clear();
                    name.push_str(FETCH);
                    name.push_str(&fetch.what);
                    let task = Task::Thread { kind: kinds::FETCH, arg: at };
                    w.plane.push(&mut fresh, node(NodeKind::Fetch, &name, &[], Pool::Io, Some(fetch_key(fetch))), task);
                }
                let first = fresh.first();
                let work = {
                    let (steps, asked, told) = (w.steps.clone(), fetches.clone(), fetches.clone());
                    task(
                        &job.cancelled,
                        move |at, _| Ok(steps.fetch(&asked[at as usize])?),
                        move |at, result| Value::Fetched(told.clone(), at, result),
                    )
                };
                job.stage = Stage::Fetch(Fetching { first, fetches, left: count, prep });
                self.add(w, id, fresh, Some(work), false);
            }
            Planned::Ready(mut units) => {
                let job = self.jobs.get_mut(&id).expect("a job planning");
                if let Some(lock) = units.lock.take() {
                    match FileName::new(lock.path.trim_start_matches('/')) {
                        Some(file) => {
                            let tag = Tag(self.setup.tags + self.next_own % (1 << 32));
                            self.next_own += 1;
                            self.own.insert(tag, (id, lock.path));
                            job.placing += 1;
                            w.effects.push(Effect::Write { file, bytes: lock.text.into_bytes(), tag });
                        }
                        None => {
                            let error = ApiError::new(format!("the lock file's path {} is no file", lock.path), Some(lock.path));
                            self.end(w, id, false, Some(error));
                            return;
                        }
                    }
                }
                for warning in &units.warnings {
                    w.said.push(Said::Report(id, Response::Diagnostic(warned(warning))));
                }
                let input = JobInput {
                    db: w.db.clone(),
                    queries: prep.queries,
                    root: prep.opened.view.root,
                    files: w.vfs.reader(),
                    config: prep.config,
                    units: Arc::new(units),
                    store: w.store.reader(),
                    color: self.setup.color,
                    goal: goal_of(&job.work),
                    cancelled: job.cancelled.clone(),
                };
                if matches!(job.work, Work::Trace(_) | Work::Verify(_)) {
                    job.ready.push(Ready { input, front: None, writer: None });
                    self.next(w, id);
                    return;
                }
                let (steps, lowering) = (w.steps.clone(), input.clone());
                let work = task(&job.cancelled, move |_, _| steps.lower(&lowering), |_, it| Value::Lowered(it));
                let name = named("lowering ", &prep.root_dir);
                let fresh = lone(w.plane, NodeKind::Names, &name, Pool::Work, None, kinds::LOWER);
                job.stage = Stage::Lower(fresh.first(), input);
                self.add(w, id, fresh, Some(work), false);
            }
        }
    }

    /// The next view to make ready, or the work once all are.
    fn next(&mut self, w: &mut World<'_>, id: JobId) {
        let job = self.jobs.get(&id).expect("a job going on");
        if job.ready.len() < job.views.len() {
            self.prepare(w, id);
            return;
        }
        let job = self.jobs.get_mut(&id).expect("a job going on");
        let cancelled = job.cancelled.clone();
        let ready = job.ready.last().expect("a job has a view");
        let reads = ready.writer.is_some();
        let answer: Option<plane::Work<Value>> = match &job.work {
            Work::Check(_) | Work::Build(_) | Work::Test { .. } | Work::Run { .. } => None,
            Work::Reach { changes, .. } => {
                let from = job.ready[0].front.clone().expect("a reach lowers both views");
                let to = ready.front.clone().expect("a reach lowers both views");
                let changes = changes.clone();
                let reach = move |_: u32, readers: Readers| {
                    let from_reader = readers.from.expect("a reach is posted with a reader of the view it reaches from");
                    to.reach(this(Readers { from: None, ..readers }), from.as_ref(), from_reader, &changes)
                };
                Some(task(&cancelled, reach, |_, it| Value::Answer(it.map(Response::Reached))))
            }
            Work::CodegenInputs(_) => {
                let front = ready.front.clone().expect("lowered");
                Some(task(&cancelled, move |_, readers| front.codegen_inputs(this(readers)), |_, it| Value::Answer(it.map(Response::CodegenInputs))))
            }
            Work::Tables => {
                let front = ready.front.clone().expect("lowered");
                let images: Vec<(&'static str, u64, u64)> = self.images.values().flat_map(|it| it.tables()).collect();
                let tables = move |_: u32, readers: Readers| {
                    let writer = readers.writer.expect("the tables' task is posted with its job's writer");
                    front.tables(writer, &images)
                };
                Some(task(&cancelled, tables, |_, it| Value::Answer(it.map(Response::Tables))))
            }
            Work::Trace(_) => {
                let (db, queries, root) = (ready.input.db.clone(), ready.input.queries, w.steps.plan_key());
                Some(task(&cancelled, move |_, _| Ok(trace(&db, queries, root)), |_, it| Value::Answer(it.map(Response::Trace))))
            }
            Work::Verify(_) => {
                let (steps, input) = (w.steps.clone(), ready.input.clone());
                let view = job.views[0].view;
                Some(task(
                    &cancelled,
                    move |_, _| verify(steps.as_ref(), &input, view),
                    |_, it| Value::Answer(it.map(|mismatches| Response::Verified { mismatches })),
                ))
            }
            Work::Snapshot => unreachable!("a snapshot has no view"),
        };
        if let Some(work) = answer {
            let fresh = lone(w.plane, NodeKind::Bodies, "the job's answer", Pool::Work, None, kinds::WALK);
            job.stage = Stage::Answer(fresh.first());
            self.add(w, id, fresh, Some(work), reads);
            self.close(w, id);
            return;
        }
        let ready = job.ready.pop().expect("a job has a view");
        let front = ready.front.expect("a build lowers its view");
        let writer = ready.writer.expect("a build lowers its view");
        let input = ready.input;
        let units = &input.units;
        let count = u32::try_from(units.graph.units.len()).expect("under 2^32 units");
        let check = input.goal == Goal::Check;
        let kind = if check { NodeKind::Bodies } else { NodeKind::Codegen };
        let (names, all_deps) = units.graph.units.iter().fold((0, 0), |(names, deps), unit| {
            (names + unit.pkg_id.len() + 1 + unit.target.name.len(), deps + unit.dependencies.len())
        });
        let mut nodes = w.plane.fresh(count, names, all_deps);
        let first = nodes.first();
        let (mut name, mut deps) = (String::new(), Vec::new());
        for (at, unit) in (0u32..).zip(&units.graph.units) {
            deps.clear();
            deps.extend(unit.dependencies.iter().map(|dep| NodeId(first.0 + dep.index as u32)));
            name.clear();
            name.push_str(&unit.pkg_id);
            name.push(' ');
            name.push_str(&unit.target.name);
            let (kind, task) = match unit.mode {
                UnitMode::RunCustomBuild => (NodeKind::ScriptRun, Task::Loop),
                _ => (kind, Task::Thread { kind: kinds::COMPILE, arg: at }),
            };
            w.plane.push(&mut nodes, node(kind, &name, &deps, Pool::Work, None), task);
        }
        let work: plane::Work<Value> = {
            let (front, cancelled) = (front.clone(), cancelled.clone());
            Arc::new(move |arg, readers: Readers| {
                let at = arg as usize;
                let Readers { this: reader, mut images, .. } = readers;
                let reader = reader.expect("a task that reads is posted with its reader");
                match front.compile(reader, at, &mut images) {
                    Ok(compiled) => {
                        let outcome = match compiled.ok {
                            true => Outcome::Built,
                            false => Outcome::Failed { why: "it did not compile".to_owned() },
                        };
                        Some(Did { outcome, value: Value::Compiled(at, Ok(compiled), images) })
                    }
                    Err(Failed::Error(error)) => Some(Did {
                        outcome: Outcome::Failed { why: error.what.clone() },
                        value: Value::Compiled(at, Err(error), images),
                    }),
                    Err(Failed::Again) if cancelled.load(Ordering::Acquire) => Some(Did {
                        outcome: Outcome::Failed { why: "cancelled".to_owned() },
                        value: Value::Compiled(at, Err(ApiError::new("cancelled", None)), images),
                    }),
                    Err(Failed::Again) => None,
                }
            })
        };
        let package_of = {
            let packages: FxHashMap<&str, u32> =
                (0u32..).zip(&units.packages).map(|(at, package)| (package.id.as_str(), at)).collect();
            units.graph.units.iter().map(|unit| packages.get(unit.pkg_id.as_str()).copied().unwrap_or(u32::MAX)).collect()
        };
        let reported = vec![0; units.packages.len().div_ceil(64)];
        let select = match &job.work {
            Work::Test { filter, args, all, ub: Some(ub), .. } => {
                let (front, filter, args, all, ub) = (front.clone(), filter.clone(), args.clone(), *all, *ub);
                let selection =
                    move |_: u32, readers: Readers| front.ub_tests(this(readers), Selection { filter: &filter, args: &args, all, ub: Some(ub) });
                let work = task(&cancelled, selection, |_, it| Value::Selected(it));
                deps.clear();
                deps.extend((0..count).map(|at| NodeId(first.0 + at)));
                let name = "the tests to evaluate";
                let mut fresh = w.plane.fresh(1, name.len(), deps.len());
                let select = w.plane.push(&mut fresh, node(NodeKind::Bodies, name, &deps, Pool::Work, None), one(kinds::UB_TESTS));
                Some((Evaluation { ub, select, failed: false }, fresh, work))
            }
            _ => None,
        };
        let (evaluation, select) = match select {
            Some((evaluation, node, work)) => (Some(evaluation), Some((node, work))),
            None => (None, None),
        };
        job.stage = Stage::Units(Box::new(Build {
            input,
            front,
            writer,
            held: Vec::new(),
            first,
            package_of,
            reported,
            scripts: FxHashMap::default(),
            artifacts: Vec::new(),
            evaluation,
        }));
        self.add(w, id, nodes, Some(work), true);
        match select {
            Some((fresh, work)) => self.add(w, id, fresh, Some(work), true),
            None => self.close(w, id),
        }
    }

    /// `blaze test --ub`'s tests selected: each one evaluated on a worker, its result reported
    /// as it ends.
    fn evaluate(&mut self, w: &mut World<'_>, id: JobId, tests: Vec<String>) {
        let job = self.jobs.get_mut(&id).expect("a job evaluating");
        let Stage::Units(build) = &job.stage else { return };
        let Some(evaluation) = &build.evaluation else { return };
        let count = u32::try_from(tests.len()).expect("under 2^32 tests");
        const TEST: &str = "test ";
        let names = tests.iter().map(|it| TEST.len() + it.len()).sum();
        let mut fresh = w.plane.fresh(count, names, 0);
        let mut name = String::new();
        for (at, test) in (0u32..).zip(&tests) {
            name.clear();
            name.push_str(TEST);
            name.push_str(test);
            let task = Task::Thread { kind: kinds::EVALUATE, arg: at };
            w.plane.push(&mut fresh, node(NodeKind::Bodies, &name, &[], Pool::Work, None), task);
        }
        let (front, tests, ub): (_, Arc<[String]>, _) = (build.front.clone(), tests.into(), evaluation.ub);
        let evaluate = move |at: u32, readers: Readers| front.evaluate(this(readers), &tests[at as usize], ub);
        let work = task(&job.cancelled, evaluate, |_, it| Value::Evaluated(it));
        self.add(w, id, fresh, Some(work), true);
        self.close(w, id);
    }

    fn close(&mut self, w: &mut World<'_>, id: JobId) {
        let Some(job) = self.jobs.get_mut(&id) else { return };
        if !job.closed && !job.finished {
            job.closed = true;
            w.plane.close(w.now, job.request.expect("a job that builds has a request"));
        }
    }

    /// A unit begins: reported once for its package, as cargo says `Compiling` once.
    fn started_unit(&mut self, w: &mut World<'_>, node: NodeId) {
        let Some(id) = self.job_of(node) else { return };
        let Some(job) = self.jobs.get_mut(&id) else { return };
        let Stage::Units(build) = &mut job.stage else { return };
        let Some(at) = build.unit(node) else { return };
        let fresh = build.front.fresh(at);
        match fresh {
            true => job.fresh += 1,
            false => job.compiled += 1,
        }
        let unit = &build.input.units.graph.units[at];
        let Some(package) = build.input.units.packages.get(build.package_of[at] as usize) else { return };
        let (word, bit) = (build.package_of[at] as usize / 64, 1 << (build.package_of[at] % 64));
        if build.reported[word] & bit != 0 {
            return;
        }
        build.reported[word] |= bit;
        let verb = match (fresh, build.input.goal, unit.mode) {
            (true, ..) => Verb::Fresh,
            (false, Goal::Check, _) | (false, _, UnitMode::Check) => Verb::Checking,
            (false, ..) => Verb::Compiling,
        };
        let started = UnitStarted {
            package_id: package.id.clone(),
            name: package.name.clone(),
            version: package.version.clone(),
            source: package.source.clone(),
            verb,
        };
        w.said.push(Said::Report(id, Response::Unit(started)));
    }

    /// The first tag of a new run of the job's.
    fn run_tags(&mut self, id: JobId, node: Option<NodeId>) -> u64 {
        let tags = self.setup.runs + (self.runs_from + self.runs.len() as u64) * RUN_TAGS;
        assert!(tags < self.setup.runs_end, "the daemon has run every run of user code its tags allow");
        self.runs.push_back(Some((id, node)));
        if let Some(job) = self.jobs.get_mut(&id) {
            job.runs += 1;
        }
        tags
    }

    /// The run a tag is of: its first tag, its job, and the script's node.
    fn run_of(&self, tag: u64) -> Option<(u64, JobId, Option<NodeId>)> {
        let number = tag.checked_sub(self.setup.runs)? / RUN_TAGS;
        let (id, node) = (*self.runs.get(usize::try_from(number.checked_sub(self.runs_from)?).ok()?)?)?;
        Some((self.setup.runs + number * RUN_TAGS, id, node))
    }

    fn run_over(&mut self, tags: u64) {
        let Some(at) = ((tags - self.setup.runs) / RUN_TAGS).checked_sub(self.runs_from) else { return };
        if let Some((id, _)) = self.runs.get_mut(at as usize).and_then(Option::take)
            && let Some(job) = self.jobs.get_mut(&id)
        {
            job.runs -= 1;
        }
        while self.runs.front().is_some_and(Option::is_none) {
            self.runs.pop_front();
            self.runs_from += 1;
        }
    }

    /// A build script's run begins, on the loop.
    fn run_script(&mut self, w: &mut World<'_>, node: NodeId) {
        let Some(id) = self.job_of(node) else { return };
        self.started_unit(w, node);
        let Some(Stage::Units(build)) = self.jobs.get(&id).map(|it| &it.stage) else { return };
        let Some(at) = build.unit(node) else { return };
        let tags = self.run_tags(id, Some(node));
        let job = self.jobs.get_mut(&id).expect("a job running a script");
        let Stage::Units(build) = &mut job.stage else { unreachable!("checked above") };
        let mut runs = match build.front.script(at, tags) {
            Ok(runs) => runs,
            Err(error) => {
                w.plane.ended(w.now, node, Outcome::Failed { why: error.what.clone() });
                job.over.get_or_insert((false, Some(error)));
                self.run_over(tags);
                return;
            }
        };
        let mut out = Vec::new();
        let ended = runs.start(w.store, Some(build.writer.as_mut()), w.effects, &mut out);
        for response in out {
            w.said.push(Said::Report(id, response));
        }
        match ended {
            Some(ok) => {
                self.run_over(tags);
                script_ended(w, node, ok);
            }
            None => _ = build.scripts.insert(node, (tags, runs)),
        }
    }

    /// A unit's compile on the loop: its diagnostics said, its puts and outputs stored and
    /// placed, and a proc macro's load asked of the writer, so the units after it, posted only
    /// once it is applied, expand with it.
    fn compiled(&mut self, w: &mut World<'_>, node: NodeId, unit: usize, result: Result<Compiled, ApiError>) {
        let Some(id) = self.job_of(node) else { return };
        let Some(job) = self.jobs.get_mut(&id) else { return };
        let mut compiled = match result {
            Ok(compiled) => compiled,
            Err(error) => {
                if !job.cancelled.load(Ordering::Acquire) {
                    job.over.get_or_insert((false, Some(error)));
                }
                return;
            }
        };
        if let (Some(macros), Stage::Units(build)) = (compiled.macros.take(), &mut job.stage)
            && let Err(error) = build.writer.macros(unit, macros)
        {
            job.over.get_or_insert((false, Some(error)));
        }
        for diagnostic in compiled.diagnostics {
            w.said.push(Said::Report(id, Response::Diagnostic(diagnostic)));
        }
        for put in compiled.puts {
            let key = put.key;
            match w.store.put(put.key, put.kind, put.bytes) {
                Ok(ticket) => self.put = Some(ticket),
                Err(mismatch) => {
                    let error = ApiError::new(format!("the store holds other bytes under {key:?}: {mismatch:?}"), None);
                    job.over.get_or_insert((false, Some(error)));
                }
            }
        }
        for output in compiled.outputs {
            let Some(file) = FileName::new(output.path.trim_start_matches('/')) else {
                let error = ApiError::new(format!("{} is no path an output can be placed at", output.path), Some(output.path));
                job.over.get_or_insert((false, Some(error)));
                continue;
            };
            match w.store.place(&output.key, file, w.effects) {
                Some(tag) => {
                    self.placing.insert(tag, (id, output.executable));
                    job.placing += 1;
                }
                None => {
                    let error = ApiError::new(
                        format!("the store has nothing under {:?} to place at {}", output.key, output.path),
                        Some(output.path),
                    );
                    job.over.get_or_insert((false, Some(error)));
                }
            }
        }
        if let (Some(artifact), Stage::Units(build)) = (compiled.artifact, &mut job.stage) {
            build.artifacts.push(artifact);
        }
    }

    /// What the store placed.
    pub fn placed(&mut self, w: &mut World<'_>, placed: Vec<Placed>) {
        for Placed { tag, to, result } in placed {
            let Some((id, executable)) = self.placing.remove(&tag) else { continue };
            let Some(job) = self.jobs.get_mut(&id) else { continue };
            match result {
                Ok(()) if executable => {
                    let tag = Tag(self.setup.tags + self.next_own % (1 << 32));
                    self.next_own += 1;
                    self.own.insert(tag, (id, format!("/{to}")));
                    w.effects.push(Effect::Mode { file: to, mode: 0o755, tag });
                }
                Ok(()) => job.placing -= 1,
                Err(why) => {
                    job.placing -= 1;
                    job.over.get_or_insert((false, Some(ApiError::new(format!("placing /{to}: {why}"), Some(format!("/{to}"))))));
                }
            }
        }
        self.wind_up(w);
    }

    /// A completion of the jobs' own: a lock write, a mode, or one of user code's runs.
    pub fn complete(&mut self, w: &mut World<'_>, completion: Completion) {
        if completion.tag.0.wrapping_sub(self.setup.tags) < 1 << 32 {
            if let Some((id, path)) = self.own.remove(&completion.tag)
                && let Some(job) = self.jobs.get_mut(&id)
            {
                job.placing -= 1;
                if let Done::Failed(error) = completion.done {
                    job.over.get_or_insert((false, Some(ApiError::new(format!("writing {path}: {error}"), Some(path)))));
                }
            }
            self.wind_up(w);
            return;
        }
        let Some((tags, id, node)) = self.run_of(completion.tag.0) else { return };
        let Some(job) = self.jobs.get_mut(&id) else {
            self.run_over(tags);
            return;
        };
        let mut out = Vec::new();
        let ended = match (node, &mut job.stage) {
            (Some(node), Stage::Units(build)) => match build.scripts.get_mut(&node) {
                Some((_, runs)) => runs.complete(completion, w.store, Some(build.writer.as_mut()), w.effects, &mut out),
                None => None,
            },
            (None, Stage::Runs(_, runs)) => runs.complete(completion, w.store, None, w.effects, &mut out),
            _ => None,
        };
        for response in out {
            w.said.push(Said::Report(id, response));
        }
        if let Some(ok) = ended {
            self.run_over(tags);
            self.run_ended(w, id, node, ok);
        }
        self.wind_up(w);
    }

    fn run_ended(&mut self, w: &mut World<'_>, id: JobId, node: Option<NodeId>, ok: bool) {
        let Some(job) = self.jobs.get_mut(&id) else { return };
        match node {
            Some(node) => {
                if let Stage::Units(build) = &mut job.stage {
                    build.scripts.remove(&node);
                }
                script_ended(w, node, ok);
            }
            None => {
                job.stage = Stage::Over;
                self.worked(w, id);
                self.end(w, id, ok, None);
            }
        }
    }

    fn plane_finished(&mut self, w: &mut World<'_>, request: RequestId, ok: bool, stats: RequestStats) {
        let Some(id) = self.by_request.remove(&request) else { return };
        let Some(job) = self.jobs.get_mut(&id) else { return };
        job.finished = true;
        job.stats = stats;
        if stats.ran > 0 {
            self.cold = true;
        }
        if !matches!(job.stage, Stage::Units(_)) {
            return;
        }
        if job.cancelled.load(Ordering::Acquire) {
            return;
        }
        let ok = ok && job.over.is_none();
        let runs = ok && matches!(job.work, Work::Test { ub: None, .. } | Work::Run { .. });
        let Stage::Units(build) = std::mem::replace(&mut job.stage, Stage::Over) else { unreachable!("checked above") };
        let Build { input, front, artifacts, evaluation, .. } = *build;
        let profile = input.units.finished.clone();
        if !runs {
            let ok = ok && evaluation.is_none_or(|it| !it.failed);
            w.said.push(Said::Report(id, Response::Built { artifacts, profile }));
            self.worked(w, id);
            self.end(w, id, ok, None);
            return;
        }
        let tags = self.run_tags(id, None);
        let started = match &self.jobs[&id].work {
            Work::Test { filter, all, args, ub, .. } => {
                front.tests(&artifacts, Selection { filter, args, all: *all, ub: *ub }, tags)
            }
            Work::Run { bin, args, .. } => front.run(&artifacts, bin.as_deref(), args, tags),
            _ => unreachable!("checked above"),
        };
        w.said.push(Said::Report(id, Response::Built { artifacts, profile }));
        let mut runs = match started {
            Ok(runs) => runs,
            Err(error) => {
                self.run_over(tags);
                self.worked(w, id);
                self.end(w, id, false, Some(error));
                return;
            }
        };
        let mut out = Vec::new();
        let ended = runs.start(w.store, None, w.effects, &mut out);
        for response in out {
            w.said.push(Said::Report(id, response));
        }
        self.jobs.get_mut(&id).expect("a job whose plane finished").stage = Stage::Runs(tags, runs);
        if let Some(ok) = ended {
            self.run_over(tags);
            self.run_ended(w, id, None, ok);
        }
    }

    /// `Work` of a build, test or run: what the plane and the units did.
    fn worked(&mut self, w: &mut World<'_>, id: JobId) {
        let job = &self.jobs[&id];
        if !matches!(job.work, Work::Build(_) | Work::Test { .. } | Work::Run { .. }) {
            return;
        }
        let (stats, fresh, compiled) = (job.stats, job.fresh, job.compiled);
        let worked = Worked {
            reran: stats.ran,
            green: stats.cached + stats.joined,
            compiled,
            found: fresh,
            link: LinkKind::Placed,
            critical_path_us: stats.critical_path_ns / 1000,
        };
        w.said.push(Said::Report(id, Response::Work(worked)));
    }

    fn cancel_job(&mut self, w: &mut World<'_>, id: JobId) {
        let Some(job) = self.jobs.get_mut(&id) else { return };
        job.cancelled.store(true, Ordering::Release);
        job.over.get_or_insert((false, None));
        if matches!(job.stage, Stage::Snapshot { .. }) {
            job.stage = Stage::Over;
        }
        refuse_held(w, &mut job.stage);
        let mut ended = Vec::new();
        match &mut job.stage {
            Stage::Units(build) => {
                for (node, (tags, runs)) in &mut build.scripts {
                    if let Some(ok) = runs.cancel(w.effects) {
                        ended.push((*tags, Some(*node), ok));
                    }
                }
            }
            Stage::Runs(tags, runs) => {
                if let Some(ok) = runs.cancel(w.effects) {
                    ended.push((*tags, None, ok));
                }
            }
            _ => {}
        }
        if let (Some(request), false) = (job.request, job.finished) {
            w.plane.cancel(w.now, request);
            if !job.closed {
                job.closed = true;
                w.plane.close(w.now, request);
            }
        }
        for (tags, node, ok) in ended {
            self.run_over(tags);
            match node {
                Some(node) => {
                    if let Some(Stage::Units(build)) = self.jobs.get_mut(&id).map(|it| &mut it.stage) {
                        build.scripts.remove(&node);
                    }
                    script_ended(w, node, ok);
                }
                None => {
                    if let Some(job) = self.jobs.get_mut(&id) {
                        job.stage = Stage::Over;
                    }
                }
            }
        }
    }

    /// Ends the job, not ok with `error` when one is given: what of it runs is let go, and it
    /// finishes once nothing of it is left.
    fn end(&mut self, w: &mut World<'_>, id: JobId, ok: bool, error: Option<ApiError>) {
        let Some(job) = self.jobs.get_mut(&id) else { return };
        match &mut job.over {
            Some((was, _)) => *was = *was && ok,
            None => job.over = Some((ok, error)),
        }
        refuse_held(w, &mut job.stage);
        if !matches!(job.stage, Stage::Runs(..)) {
            job.stage = Stage::Over;
        }
        if let (Some(request), false) = (job.request, job.finished) {
            if !ok {
                w.plane.cancel(w.now, request);
            }
            if !job.closed {
                job.closed = true;
                w.plane.close(w.now, request);
            }
        }
    }

    /// Finishes every job that is over and has nothing left: its tasks, its placements and its
    /// runs; and a snapshot once all it waits for is durable.
    fn wind_up(&mut self, w: &mut World<'_>) {
        self.apply_writes(w);
        let mut over = Vec::new();
        let order = self.sorted();
        for id in &order {
            let job = self.jobs.get_mut(id).expect("a job in the order");
            if let Stage::Snapshot { ticket: None, heads, store } = &mut job.stage
                && w.db.settled()
                && store.is_none_or(|it| w.store.durable(it))
            {
                let heads = std::mem::take(heads);
                w.said.push(Said::Report(*id, Response::Snapshotted { heads }));
                job.stage = Stage::Over;
                job.over = Some((true, None));
            }
            let Some((ok, error)) = &mut job.over else { continue };
            if job.finished && job.placing == 0 && job.runs == 0 && !matches!(job.stage, Stage::Runs(..)) {
                over.push((*id, *ok, error.take()));
            }
        }
        self.order = order;
        for (id, ok, error) in over {
            let job = self.jobs.remove(&id).expect("a job over");
            for queries in job.queries {
                w.db.close(queries);
            }
            for first in job.batches {
                self.nodes.remove(&first.0);
            }
            self.ended = true;
            w.said.push(Said::Finish(id, ok, error));
        }
    }

    /// The VFS's tickets that are durable: a snapshot waits for its own.
    pub fn durable(&mut self, w: &mut World<'_>, tickets: &[Ticket]) {
        for job in self.jobs.values_mut() {
            if let Stage::Snapshot { ticket, .. } = &mut job.stage
                && ticket.is_some_and(|it| tickets.contains(&it))
            {
                *ticket = None;
            }
        }
        self.wind_up(w);
    }

    /// A snapshot: the image written, the journal and the store flushed, and every head as it
    /// stands, reported once all of it is durable.
    fn snapshot(&mut self, w: &mut World<'_>) -> Stage {
        w.db.write_image(w.effects);
        w.store.flush(w.effects);
        let heads: Vec<(String, ContentId)> = w.vfs.heads().into_iter().map(|(head, root)| (head.to_owned(), root)).collect();
        let ticket = heads.first().and_then(|(head, root)| w.vfs.set_head(head, *root).ok());
        w.vfs.flush(w.effects);
        Stage::Snapshot { ticket, heads, store: self.put }
    }
}

/// The reading tasks a job held for a write ended unposted: the job is over or cancelled.
fn refuse_held(w: &mut World<'_>, stage: &mut Stage) {
    if let Stage::Units(build) = stage {
        for (task, _) in build.held.drain(..) {
            w.plane.refuse(w.now, task, "cancelled".to_owned());
        }
    }
}

/// The images kept at unit `unit`'s outputs, moved out of `images` for its compile.
fn take_images(images: &mut FxHashMap<String, Box<dyn Kept>>, units: &blaze_units::query::Units, unit: usize) -> Vec<Image> {
    if images.is_empty() {
        return Vec::new();
    }
    let Some(plan) = units.plans.get(unit) else { return Vec::new() };
    plan.outputs.iter().filter_map(|path| images.remove_entry(path.as_str())).map(|(path, kept)| Image { path, kept }).collect()
}

fn script_ended(w: &mut World<'_>, node: NodeId, ok: bool) {
    let outcome = match ok {
        true => Outcome::Built,
        false => Outcome::Failed { why: "the build script failed".to_owned() },
    };
    w.plane.ended(w.now, node, outcome);
}

/// A warning of the load, as cargo prints it.
fn warned(warning: &str) -> Diagnostic {
    Diagnostic {
        level: Level::Warning,
        code: None,
        message: warning.to_owned(),
        spans: Vec::new(),
        children: Vec::new(),
        facts: Vec::new(),
        rendered: format!("warning: {warning}\n"),
        unit: None,
    }
}

/// Every memo reachable from `from` in `db`, each once, in the order it is reached.
fn walk(db: &Database, from: QueryKey) -> Vec<blaze_query::Memo> {
    let mut seen = FxHashSet::default();
    let mut queue = vec![from];
    let mut memos = Vec::new();
    while let Some(key) = queue.pop() {
        if !seen.insert(key) {
            continue;
        }
        let Some(memo) = db.memo(&key) else { continue };
        queue.extend(memo.edges.iter().rev().map(|edge| edge.read));
        memos.push(memo);
    }
    memos
}

/// The memos of the view's last build, from its plan down.
pub fn trace(db: &Database, view: ViewId, from: QueryKey) -> Vec<Traced> {
    walk(db, from)
        .into_iter()
        .map(|memo| Traced {
            ran: db.why(view, &memo.key).is_some(),
            key: memo.key,
            fingerprint: memo.fingerprint,
            edges: memo.edges,
        })
        .collect()
}

/// The view's plan asked again in a database of its own from nothing, and every result
/// compared with the daemon's.
fn verify(steps: &dyn Steps, input: &JobInput, view: blaze_vfs::View) -> Result<Vec<Mismatch>, Failed> {
    let config = blaze_query::Config { tree: Some(Arc::new(input.files.clone())), dir: None, tags: 0 };
    let mut scratch = Database::new(config);
    steps.register(&mut scratch);
    let queries = scratch.open_with(view, &input.config.encode());
    steps.plan(&scratch, queries)?;
    let mut mismatches = Vec::new();
    for memo in walk(&scratch, steps.plan_key()) {
        let Some(ours) = input.db.memo(&memo.key) else { continue };
        if ours.fingerprint != memo.fingerprint {
            mismatches.push(Mismatch { query: memo.key, incremental: ours.fingerprint, scratch: memo.fingerprint, fact: None });
        }
    }
    Ok(mismatches)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn targets() -> Targets {
        Targets {
            packages: vec!["a".into(), "b".into()],
            profile: "release".into(),
            target: Some("wasm32-unknown-unknown".into()),
            features: vec!["x".into(), "y".into()],
            all_features: false,
            no_default_features: true,
            cargo: None,
        }
    }

    #[test]
    fn an_agents_targets_are_cargos_flags_at_the_root() {
        let env = vec![("HOME".to_owned(), "/home/u".to_owned()), ("PATH".to_owned(), "/bin".to_owned())];
        let work = Work::Build(targets());
        let made = invocation(&work, targets_of(&work), "/w", &env);
        assert_eq!(made.command, Command::Build);
        let args: Vec<&str> = made.args.iter().map(String::as_str).collect();
        assert_eq!(
            args,
            [
                "-p", "a", "-p", "b", "--profile", "release", "--target", "wasm32-unknown-unknown", "-F", "x,y",
                "--no-default-features"
            ]
        );
        assert_eq!((made.root.as_str(), made.cwd.as_str()), ("/w", "/w"));
        assert_eq!(made.env, env);
        assert!(made.files.is_empty());
    }

    #[test]
    fn cargos_own_invocation_is_kept_whole() {
        let mut targets = targets();
        targets.cargo = Some(blaze_api::Cargo {
            command: "test".into(),
            args: vec!["--workspace".into()],
            root: "/w".into(),
            cwd: "/w/a".into(),
            env: vec![("RUSTFLAGS".into(), "-Dwarnings".into())],
            files: Vec::new(),
            color: false,
        });
        let work = Work::Test { targets: targets.clone(), filter: Vec::new(), all: false, args: Vec::new(), ub: None };
        let made = invocation(&work, Some(&targets), "/elsewhere", &[]);
        assert_eq!(made.command, Command::Test);
        assert_eq!(made.args, vec!["--workspace".to_owned()]);
        assert_eq!(made.cwd, "/w/a");
        assert_eq!(made.env, vec![("RUSTFLAGS".to_owned(), "-Dwarnings".to_owned())]);
    }

    #[test]
    fn tests_under_the_evaluator_check_the_units_and_build_none() {
        let ub = Some(blaze_api::Ub { seed: Some(7), schedules: None });
        let work = Work::Test { targets: targets(), filter: Vec::new(), all: true, args: Vec::new(), ub };
        assert_eq!(command_of(&work), Command::Test);
        assert_eq!(goal_of(&work), Goal::Check);
        let compiled = Work::Test { targets: targets(), filter: Vec::new(), all: true, args: Vec::new(), ub: None };
        assert_eq!(goal_of(&compiled), Goal::Test);
    }

    #[test]
    fn a_fetchs_key_is_its_source() {
        let fetch = |at: &str, sha: &str| Fetch {
            what: "serde v1.0.0".into(),
            url: "https://static.crates.io/crates/serde/1.0.0/download".into(),
            kind: FetchKind::Crate { sha256: sha.into() },
            at: at.into(),
        };
        assert_eq!(fetch_key(&fetch("/r/serde-1.0.0", "aa")), fetch_key(&fetch("/r/serde-1.0.0", "aa")));
        assert_ne!(fetch_key(&fetch("/r/serde-1.0.0", "aa")), fetch_key(&fetch("/r/serde-1.0.0", "bb")));
        assert_ne!(fetch_key(&fetch("/r/serde-1.0.0", "aa")), fetch_key(&fetch("/s/serde-1.0.0", "aa")));
    }

    #[test]
    fn jobs_are_walked_in_id_order_whatever_their_hashes() {
        let setup = Setup { views: "/v".into(), host: "h".into(), env: Vec::new(), color: false, tags: 0, runs: 0, runs_end: 0 };
        let mut jobs = Jobs::new(setup);
        let ids = [9u64, 1, 300, 2, 1 << 40, 0, 77];
        for id in ids {
            let job = Job {
                work: Work::Snapshot,
                views: Vec::new(),
                ready: Vec::new(),
                request: None,
                closed: false,
                finished: true,
                cancelled: Arc::new(AtomicBool::new(false)),
                stage: Stage::Over,
                values: FxHashMap::default(),
                updating: FxHashSet::default(),
                queries: Vec::new(),
                placing: 0,
                batches: Vec::new(),
                runs: 0,
                over: None,
                stats: RequestStats::default(),
                fresh: 0,
                compiled: 0,
            };
            jobs.jobs.insert(JobId(id), job);
        }
        let mut want: Vec<JobId> = ids.iter().map(|it| JobId(*it)).collect();
        want.sort();
        let order = jobs.sorted();
        assert_eq!(order, want);
        jobs.order = order;
        jobs.jobs.remove(&JobId(300));
        want.retain(|it| it.0 != 300);
        assert_eq!(jobs.sorted(), want);
    }
}

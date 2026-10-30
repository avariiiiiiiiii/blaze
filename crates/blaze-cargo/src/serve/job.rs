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

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use blaze_api::{
    ApiError, Artifact, Diagnostic, FetchVerb, Level, LinkKind, Mismatch, Response, Targets, Traced,
    UnitStarted, Verb, Worked,
    build::{Builder, JobId, Opened, Work},
};
use blaze_cas::{Placed, Store};
use blaze_core::{Completion, Done, Effect, FileName, Tag};
use blaze_graph::{
    Key, Outcome, Pool, RequestId, RequestStats, Spec,
    machine::NodeId,
};
use blaze_ids::ContentId;
use blaze_protocol::events::NodeKind;
use blaze_query::{Database, QueryKey};
use blaze_units::{
    query::{BuildConfig, Command, Fetch, FetchKind, Invocation, Mount, Origin, Planned},
    schema::Mode as UnitMode,
};
use blaze_vfs::{Ticket, ViewId, Vfs};
use rustc_hash::FxHashMap;

use super::{
    heads::Heads,
    mounts::Mounts,
    plane::{self, Did, Happened, Node, Plane, Task, kinds},
    work::{Compiled, Failed, Front, Goal, JobInput, Runs, Source, Steps},
};

/// How many tags one run of user code uses ([`Runs`]).
const RUN_TAGS: u64 = 1 << 32;

/// What a thread task of a job hands the loop.
pub enum Value {
    Library(Result<(String, Source), ApiError>),
    Fetched(Box<Fetch>, Result<Source, ApiError>),
    Planned(Result<Planned, ApiError>),
    Lowered(Result<Arc<dyn Front>, ApiError>),
    Compiled(usize, Result<Compiled, ApiError>),
    Answer(Result<Response, ApiError>),
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
    pub store: &'a Arc<RwLock<Store>>,
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
    let kind = match &fetch.kind {
        FetchKind::Crate { sha256 } => format!("crate {sha256}"),
        FetchKind::IndexFile => "index".to_owned(),
        FetchKind::Git { commit } => format!("git {commit}"),
    };
    let text = format!("{}\0{}\0{kind}", fetch.at, fetch.url);
    Key(ContentId::of("blaze v2 serve fetch", text.as_bytes()).0)
}

/// A step's work as a task: a give-way posts it again, unless the job is cancelled.
fn task<R: 'static>(
    cancelled: &Arc<AtomicBool>,
    step: impl Fn() -> Result<R, Failed> + Send + Sync + 'static,
    value: impl Fn(Result<R, ApiError>) -> Value + Send + Sync + 'static,
) -> plane::Work<Value> {
    let cancelled = cancelled.clone();
    Arc::new(move || match step() {
        Ok(it) => Some(Did { outcome: Outcome::Built, value: value(Ok(it)) }),
        Err(Failed::Error(error)) => {
            Some(Did { outcome: Outcome::Failed { why: error.what.clone() }, value: value(Err(error)) })
        }
        Err(Failed::Again) if cancelled.load(Ordering::Acquire) => Some(Did {
            outcome: Outcome::Failed { why: "cancelled".to_owned() },
            value: value(Err(ApiError::new("cancelled", None))),
        }),
        Err(Failed::Again) => None,
    })
}

fn spec(id: NodeId, kind: NodeKind, name: String, deps: Vec<NodeId>, pool: Pool, key: Option<Key>) -> Spec {
    Spec { id, kind, name, deps, pool, cost: 1, key, memory: 0 }
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
    asked: BTreeSet<String>,
}

/// A view made ready: lowered, or planned only for the work that walks the plan.
struct Ready {
    input: JobInput,
    front: Option<Arc<dyn Front>>,
}

enum Stage {
    Library(NodeId),
    Plan(NodeId, Prep),
    Fetch(BTreeSet<NodeId>, BTreeMap<NodeId, String>, Prep),
    Lower(NodeId, JobInput),
    Units(Box<Build>),
    Answer(NodeId),
    /// The tests or the run, once every unit is built.
    Runs(Box<dyn Runs>),
    Snapshot { ticket: Option<Ticket>, heads: Vec<(String, ContentId)>, store: Vec<blaze_cas::Ticket> },
    Over,
}

struct Build {
    input: JobInput,
    front: Arc<dyn Front>,
    units: FxHashMap<NodeId, usize>,
    scripts: FxHashMap<NodeId, Box<dyn Runs>>,
    reported: BTreeSet<String>,
    artifacts: Vec<Artifact>,
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
    updating: BTreeSet<String>,
    /// Opened by the job and closed as it finishes.
    queries: Vec<ViewId>,
    placing: usize,
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
    jobs: BTreeMap<JobId, Job>,
    by_node: FxHashMap<NodeId, JobId>,
    by_request: FxHashMap<RequestId, JobId>,
    library: Option<String>,
    library_failed: Option<ApiError>,
    fetch_failed: BTreeMap<String, ApiError>,
    /// The store's placements: the job, where, and whether it is executable.
    placing: FxHashMap<Tag, (JobId, FileName, bool)>,
    /// The job of each lock write and mode of the loop's own.
    own: FxHashMap<Tag, (JobId, String)>,
    next_own: u64,
    next_runs: u64,
    /// By the first tag of its range: whose run it is.
    runs: BTreeMap<u64, (JobId, Option<NodeId>)>,
    /// What the store was given and is not durable yet.
    unsaved: Vec<blaze_cas::Ticket>,
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
            next_runs: setup.runs,
            setup,
            started: Vec::new(),
            cancels: Vec::new(),
            jobs: BTreeMap::new(),
            by_node: FxHashMap::default(),
            by_request: FxHashMap::default(),
            library: None,
            library_failed: None,
            fetch_failed: BTreeMap::new(),
            placing: FxHashMap::default(),
            own: FxHashMap::default(),
            next_own: 0,
            runs: BTreeMap::new(),
            unsaved: Vec::new(),
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
        self.jobs.values().flat_map(|it| it.views.iter().map(|view| view.view.root)).collect()
    }

    pub fn owns(&self, tag: Tag) -> bool {
        if tag.0.wrapping_sub(self.setup.tags) < 1 << 32 {
            return true;
        }
        tag.0 >= self.setup.runs && tag.0 < self.setup.runs_end
    }

    /// Takes what the session asked since the last call.
    pub fn step(&mut self, w: &mut World<'_>) {
        for (id, view, work) in std::mem::take(&mut self.started) {
            self.begin(w, id, view, work);
        }
        for id in std::mem::take(&mut self.cancels) {
            self.cancel_job(w, id);
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
            updating: BTreeSet::new(),
            queries: Vec::new(),
            placing: 0,
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
            let node = w.plane.node_id();
            let steps = w.steps.clone();
            let work = task(&job.cancelled, move || Ok(steps.library()?), Value::Library);
            let spec = spec(node, NodeKind::Std, "the standard library's source".into(), Vec::new(), Pool::Io, Some(library_key()));
            job.stage = Stage::Library(node);
            self.add(w, id, vec![Node { spec, task: plane::Task::Thread { kind: kinds::LIBRARY, work } }]);
            return;
        }
        let job = self.jobs.get(&id).expect("a job being prepared");
        let opened = job.views[job.ready.len()];
        let prep = self.open(w, id, opened, BTreeSet::new());
        self.plan(w, id, prep);
    }

    /// The view opened in the database with its build configuration as the mounts are now.
    fn open(&mut self, w: &mut World<'_>, id: JobId, opened: Opened, asked: BTreeSet<String>) -> Prep {
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
        let node = w.plane.node_id();
        let (steps, db, queries) = (w.steps.clone(), w.db.clone(), prep.queries);
        let work = task(&job.cancelled, move || steps.plan(&db, queries), Value::Planned);
        let spec = spec(node, NodeKind::Manifest, format!("the plan of {}", prep.root_dir), Vec::new(), Pool::Work, None);
        job.stage = Stage::Plan(node, prep);
        self.add(w, id, vec![Node { spec, task: plane::Task::Thread { kind: kinds::PLAN, work } }]);
    }

    fn add(&mut self, w: &mut World<'_>, id: JobId, nodes: Vec<Node<Value>>) {
        let request = self.jobs[&id].request.expect("a job that builds has a request");
        for node in &nodes {
            self.by_node.insert(node.spec.id, id);
        }
        if let Err(error) = w.plane.add(w.now, request, nodes) {
            self.end(w, id, false, Some(ApiError::new(format!("the plane refused the job's tasks: {error}"), None)));
        }
    }

    /// What the plane did.
    pub fn happened(&mut self, w: &mut World<'_>, happened: Vec<Happened<Value>>) {
        for it in happened {
            match it {
                Happened::Did { id, value } => self.did(w, id, value),
                Happened::Started { id } => self.started_unit(w, id),
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
            Value::Fetched(fetch, Ok(source)) => {
                if let Err(why) = w.mounts.mount(w.vfs, &fetch.at, &source) {
                    self.fetch_failed.insert(fetch.at.clone(), ApiError::new(why, Some(fetch.what.clone())));
                }
            }
            Value::Fetched(fetch, Err(error)) => {
                self.fetch_failed.insert(fetch.at.clone(), error);
            }
            Value::Compiled(unit, result) => self.compiled(w, node, unit, result),
            value => {
                if let Some(job) = self.by_node.get(&node).and_then(|id| self.jobs.get_mut(id)) {
                    job.values.insert(node, value);
                }
            }
        }
    }

    fn settled(&mut self, w: &mut World<'_>, node: NodeId, ok: bool, why: Option<String>) {
        let Some(id) = self.by_node.remove(&node) else { return };
        let Some(job) = self.jobs.get_mut(&id) else { return };
        if job.over.is_some() {
            job.values.remove(&node);
            if let Stage::Units(build) = &mut job.stage {
                build.units.remove(&node);
            }
            return;
        }
        let why = why.unwrap_or_default();
        match std::mem::replace(&mut job.stage, Stage::Over) {
            Stage::Library(at) if at == node => match &self.library {
                Some(_) => self.prepare(w, id),
                None => {
                    let error = self
                        .library_failed
                        .clone()
                        .unwrap_or_else(|| ApiError::new(format!("the standard library's source: {why}"), None));
                    self.end(w, id, false, Some(error));
                }
            },
            Stage::Plan(at, prep) if at == node => {
                let value = job.values.remove(&node);
                match value {
                    Some(Value::Planned(Ok(planned))) => self.planned(w, id, prep, planned),
                    Some(Value::Planned(Err(error))) => self.end(w, id, false, Some(error)),
                    _ => self.end(w, id, false, Some(ApiError::new(format!("the plan of {}: {why}", prep.root_dir), None))),
                }
            }
            Stage::Fetch(mut nodes, ats, prep) => {
                nodes.remove(&node);
                if !ok {
                    let at = ats.get(&node).cloned().unwrap_or_default();
                    let error = self
                        .fetch_failed
                        .get(&at)
                        .cloned()
                        .unwrap_or_else(|| ApiError::new(format!("fetching {at}: {why}"), Some(at.clone())));
                    job.over.get_or_insert((false, Some(error)));
                }
                if !nodes.is_empty() {
                    job.stage = Stage::Fetch(nodes, ats, prep);
                    return;
                }
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
                Some(Value::Lowered(Ok(front))) => {
                    job.ready.push(Ready { input, front: Some(front) });
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
            Stage::Units(mut build) => {
                build.units.remove(&node);
                job.stage = Stage::Units(build);
            }
            stage => job.stage = stage,
        }
    }

    fn planned(&mut self, w: &mut World<'_>, id: JobId, mut prep: Prep, planned: Planned) {
        match planned {
            Planned::Failed(error) => self.end(w, id, false, Some(ApiError::new(error.what, error.at))),
            Planned::Needs(fetches) => {
                let job = self.jobs.get_mut(&id).expect("a job planning");
                let mut nodes = Vec::new();
                let (mut ids, mut ats) = (BTreeSet::new(), BTreeMap::new());
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
                    let node = w.plane.node_id();
                    let steps = w.steps.clone();
                    let key = fetch_key(&fetch);
                    let name = format!("fetch {}", fetch.what);
                    ats.insert(node, fetch.at.clone());
                    ids.insert(node);
                    let fetch = Box::new(fetch);
                    let asked = fetch.clone();
                    let work = task(
                        &job.cancelled,
                        move || Ok(steps.fetch(&asked)?),
                        move |result| Value::Fetched(fetch.clone(), result),
                    );
                    nodes.push(Node {
                        spec: spec(node, NodeKind::Fetch, name, Vec::new(), Pool::Io, Some(key)),
                        task: plane::Task::Thread { kind: kinds::FETCH, work },
                    });
                }
                if nodes.is_empty() {
                    w.db.close(prep.queries);
                    job.queries.retain(|it| *it != prep.queries);
                    let again = self.open(w, id, prep.opened, prep.asked);
                    self.plan(w, id, again);
                    return;
                }
                job.stage = Stage::Fetch(ids, ats, prep);
                self.add(w, id, nodes);
            }
            Planned::Ready(units) => {
                let job = self.jobs.get_mut(&id).expect("a job planning");
                if let Some(lock) = &units.lock {
                    match FileName::new(lock.path.trim_start_matches('/')) {
                        Some(file) => {
                            let tag = Tag(self.setup.tags + self.next_own % (1 << 32));
                            self.next_own += 1;
                            self.own.insert(tag, (id, lock.path.clone()));
                            job.placing += 1;
                            w.effects.push(Effect::Write { file, bytes: lock.text.clone().into_bytes(), tag });
                        }
                        None => {
                            let error = ApiError::new(format!("the lock file's path {} is no file", lock.path), Some(lock.path.clone()));
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
                    store: w.store.clone(),
                    color: self.setup.color,
                    goal: goal_of(&job.work),
                    cancelled: job.cancelled.clone(),
                };
                if matches!(job.work, Work::Trace(_) | Work::Verify(_)) {
                    job.ready.push(Ready { input, front: None });
                    self.next(w, id);
                    return;
                }
                let node = w.plane.node_id();
                let (steps, lowering) = (w.steps.clone(), input.clone());
                let work = task(&job.cancelled, move || steps.lower(&lowering), Value::Lowered);
                let spec = spec(node, NodeKind::Names, format!("lowering {}", prep.root_dir), Vec::new(), Pool::Work, None);
                job.stage = Stage::Lower(node, input);
                self.add(w, id, vec![Node { spec, task: plane::Task::Thread { kind: kinds::LOWER, work } }]);
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
        let answer: Option<plane::Work<Value>> = match &job.work {
            Work::Check(_) | Work::Build(_) | Work::Test { .. } | Work::Run { .. } => None,
            Work::Reach { changes, .. } => {
                let from = job.ready[0].front.clone().expect("a reach lowers both views");
                let to = ready.front.clone().expect("a reach lowers both views");
                let changes = changes.clone();
                Some(task(&cancelled, move || to.reach(from.as_ref(), &changes), |it| Value::Answer(it.map(Response::Reached))))
            }
            Work::CodegenInputs(_) => {
                let front = ready.front.clone().expect("lowered");
                Some(task(&cancelled, move || front.codegen_inputs(), |it| Value::Answer(it.map(Response::CodegenInputs))))
            }
            Work::Tables => {
                let front = ready.front.clone().expect("lowered");
                Some(task(&cancelled, move || front.tables(), |it| Value::Answer(it.map(Response::Tables))))
            }
            Work::Trace(_) => {
                let (db, queries, root) = (ready.input.db.clone(), ready.input.queries, w.steps.plan_key());
                Some(task(&cancelled, move || Ok(trace(&db, queries, root)), |it| Value::Answer(it.map(Response::Trace))))
            }
            Work::Verify(_) => {
                let (steps, input) = (w.steps.clone(), ready.input.clone());
                let view = job.views[0].view;
                Some(task(
                    &cancelled,
                    move || verify(steps.as_ref(), &input, view),
                    |it| Value::Answer(it.map(|mismatches| Response::Verified { mismatches })),
                ))
            }
            Work::Snapshot => unreachable!("a snapshot has no view"),
        };
        if let Some(work) = answer {
            let node = w.plane.node_id();
            let spec = spec(node, NodeKind::Bodies, "the job's answer".into(), Vec::new(), Pool::Work, None);
            job.stage = Stage::Answer(node);
            self.add(w, id, vec![Node { spec, task: plane::Task::Thread { kind: kinds::WALK, work } }]);
            self.close(w, id);
            return;
        }
        let ready = job.ready.pop().expect("a job has a view");
        let front = ready.front.expect("a build lowers its view");
        let input = ready.input;
        let units = input.units.clone();
        let ids: Vec<NodeId> = units.graph.units.iter().map(|_| w.plane.node_id()).collect();
        let mut nodes = Vec::with_capacity(ids.len());
        let mut by_unit = FxHashMap::default();
        let check = input.goal == Goal::Check;
        for (at, unit) in units.graph.units.iter().enumerate() {
            let deps = unit.dependencies.iter().map(|dep| ids[dep.index]).collect();
            let name = format!("{} {}", unit.pkg_id, unit.target.name);
            by_unit.insert(ids[at], at);
            let node = match unit.mode {
                UnitMode::RunCustomBuild => Node {
                    spec: spec(ids[at], NodeKind::ScriptRun, name, deps, Pool::Work, None),
                    task: Task::Loop,
                },
                _ => {
                    let front = front.clone();
                    let work: plane::Work<Value> = {
                        let cancelled = cancelled.clone();
                        Arc::new(move || match front.compile(at) {
                            Ok(compiled) => {
                                let outcome = match compiled.ok {
                                    true => Outcome::Built,
                                    false => Outcome::Failed { why: "it did not compile".to_owned() },
                                };
                                Some(Did { outcome, value: Value::Compiled(at, Ok(compiled)) })
                            }
                            Err(Failed::Error(error)) => Some(Did {
                                outcome: Outcome::Failed { why: error.what.clone() },
                                value: Value::Compiled(at, Err(error)),
                            }),
                            Err(Failed::Again) if cancelled.load(Ordering::Acquire) => Some(Did {
                                outcome: Outcome::Failed { why: "cancelled".to_owned() },
                                value: Value::Compiled(at, Err(ApiError::new("cancelled", None))),
                            }),
                            Err(Failed::Again) => None,
                        })
                    };
                    let kind = if check { NodeKind::Bodies } else { NodeKind::Codegen };
                    Node { spec: spec(ids[at], kind, name, deps, Pool::Work, None), task: Task::Thread { kind: kinds::COMPILE, work } }
                }
            };
            nodes.push(node);
        }
        job.stage = Stage::Units(Box::new(Build {
            input,
            front,
            units: by_unit,
            scripts: FxHashMap::default(),
            reported: BTreeSet::new(),
            artifacts: Vec::new(),
        }));
        self.add(w, id, nodes);
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
        let Some(&id) = self.by_node.get(&node) else { return };
        let Some(job) = self.jobs.get_mut(&id) else { return };
        let Stage::Units(build) = &mut job.stage else { return };
        let Some(&at) = build.units.get(&node) else { return };
        let fresh = build.front.fresh(at);
        match fresh {
            true => job.fresh += 1,
            false => job.compiled += 1,
        }
        let unit = &build.input.units.graph.units[at];
        if !build.reported.insert(unit.pkg_id.clone()) {
            return;
        }
        let Some(package) = build.input.units.packages.iter().find(|it| it.id == unit.pkg_id) else { return };
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

    fn run_tags(&mut self, id: JobId, node: Option<NodeId>) -> u64 {
        assert!(self.next_runs < self.setup.runs_end, "the daemon has run 2^32 runs of user code and has no tags for another");
        let tags = self.next_runs;
        self.next_runs += RUN_TAGS;
        self.runs.insert(tags, (id, node));
        tags
    }

    /// A build script's run begins, on the loop.
    fn run_script(&mut self, w: &mut World<'_>, node: NodeId) {
        let Some(&id) = self.by_node.get(&node) else { return };
        self.started_unit(w, node);
        let Some(Stage::Units(build)) = self.jobs.get(&id).map(|it| &it.stage) else { return };
        let Some(&at) = build.units.get(&node) else { return };
        let tags = self.run_tags(id, Some(node));
        let job = self.jobs.get_mut(&id).expect("a job running a script");
        let Stage::Units(build) = &mut job.stage else { unreachable!("checked above") };
        let mut runs = match build.front.script(at, tags) {
            Ok(runs) => runs,
            Err(error) => {
                w.plane.ended(w.now, node, Outcome::Failed { why: error.what.clone() });
                job.over.get_or_insert((false, Some(error)));
                return;
            }
        };
        let mut out = Vec::new();
        let ended = runs.start(&w.store.read().expect("the store's lock"), w.effects, &mut out);
        for response in out {
            w.said.push(Said::Report(id, response));
        }
        match ended {
            Some(ok) => {
                self.runs.remove(&tags);
                script_ended(w, node, ok);
            }
            None => _ = build.scripts.insert(node, runs),
        }
    }

    fn compiled(&mut self, w: &mut World<'_>, node: NodeId, _unit: usize, result: Result<Compiled, ApiError>) {
        let Some(&id) = self.by_node.get(&node) else { return };
        let Some(job) = self.jobs.get_mut(&id) else { return };
        let compiled = match result {
            Ok(compiled) => compiled,
            Err(error) => {
                if !job.cancelled.load(Ordering::Acquire) {
                    job.over.get_or_insert((false, Some(error)));
                }
                return;
            }
        };
        for diagnostic in compiled.diagnostics {
            w.said.push(Said::Report(id, Response::Diagnostic(diagnostic)));
        }
        let mut store = w.store.write().expect("the store's lock");
        for put in compiled.puts {
            let key = format!("{:?}", put.key);
            match store.put(put.key, put.kind, put.bytes) {
                Ok(ticket) => self.unsaved.push(ticket),
                Err(mismatch) => {
                    let error = ApiError::new(format!("the store holds other bytes under {key}: {mismatch:?}"), None);
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
            match store.place(&output.key, file.clone(), w.effects) {
                Some(tag) => {
                    self.placing.insert(tag, (id, file, output.executable));
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
            let Some((id, file, executable)) = self.placing.remove(&tag) else { continue };
            let Some(job) = self.jobs.get_mut(&id) else { continue };
            match result {
                Ok(()) if executable => {
                    let tag = Tag(self.setup.tags + self.next_own % (1 << 32));
                    self.next_own += 1;
                    self.own.insert(tag, (id, format!("/{file}")));
                    w.effects.push(Effect::Mode { file, mode: 0o755, tag });
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
        let Some((&tags, &(id, node))) = self.runs.range(..=completion.tag.0).next_back() else { return };
        if completion.tag.0 - tags >= RUN_TAGS {
            return;
        }
        let Some(job) = self.jobs.get_mut(&id) else {
            self.runs.remove(&tags);
            return;
        };
        let mut out = Vec::new();
        let mut store = w.store.write().expect("the store's lock");
        let ended = match (node, &mut job.stage) {
            (Some(node), Stage::Units(build)) => match build.scripts.get_mut(&node) {
                Some(runs) => runs.complete(completion, &mut store, w.effects, &mut out),
                None => None,
            },
            (None, Stage::Runs(runs)) => runs.complete(completion, &mut store, w.effects, &mut out),
            _ => None,
        };
        drop(store);
        for response in out {
            w.said.push(Said::Report(id, response));
        }
        if let Some(ok) = ended {
            self.runs.remove(&tags);
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
        let runs = match (&job.work, ok) {
            (Work::Test { filter, all, .. }, true) => Some((filter.clone(), *all, None, Vec::new())),
            (Work::Run { bin, args, .. }, true) => Some((Vec::new(), false, Some(bin.clone()), args.clone())),
            _ => None,
        };
        self.report_built(w, id);
        let Some((filter, all, bin, args)) = runs else {
            self.worked(w, id);
            self.end(w, id, ok, None);
            return;
        };
        let Some(Stage::Units(build)) = self.jobs.get(&id).map(|it| &it.stage) else {
            unreachable!("checked above")
        };
        let (front, artifacts) = (build.front.clone(), build.artifacts.clone());
        let tags = self.run_tags(id, None);
        let started = match &bin {
            None => front.tests(&artifacts, &filter, all, tags),
            Some(bin) => front.run(&artifacts, bin.as_deref(), &args, tags),
        };
        let mut runs = match started {
            Ok(runs) => runs,
            Err(error) => {
                self.runs.remove(&tags);
                self.worked(w, id);
                self.end(w, id, false, Some(error));
                return;
            }
        };
        let mut out = Vec::new();
        let ended = runs.start(&w.store.read().expect("the store's lock"), w.effects, &mut out);
        for response in out {
            w.said.push(Said::Report(id, response));
        }
        self.jobs.get_mut(&id).expect("a job whose plane finished").stage = Stage::Runs(runs);
        if let Some(ok) = ended {
            self.runs.remove(&tags);
            self.run_ended(w, id, None, ok);
        }
    }

    fn report_built(&mut self, w: &mut World<'_>, id: JobId) {
        let job = &self.jobs[&id];
        let Stage::Units(build) = &job.stage else { return };
        let profile = build.input.units.finished.clone();
        w.said.push(Said::Report(id, Response::Built { artifacts: build.artifacts.clone(), profile }));
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
        let mut ended = Vec::new();
        match &mut job.stage {
            Stage::Units(build) => {
                for (node, runs) in &mut build.scripts {
                    if let Some(ok) = runs.cancel(w.effects) {
                        ended.push((Some(*node), ok));
                    }
                }
            }
            Stage::Runs(runs) => {
                if let Some(ok) = runs.cancel(w.effects) {
                    ended.push((None, ok));
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
        for (node, ok) in ended {
            self.runs.retain(|_, it| *it != (id, node));
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
        if !matches!(job.stage, Stage::Runs(_)) {
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
        {
            let store = w.store.read().expect("the store's lock");
            self.unsaved.retain(|ticket| !store.durable(*ticket));
        }
        let mut over = Vec::new();
        for (id, job) in &mut self.jobs {
            if let Stage::Snapshot { ticket: None, heads, store } = &job.stage
                && w.db.settled()
                && store.iter().all(|it| self.unsaved.iter().all(|unsaved| unsaved != it))
            {
                let heads = heads.clone();
                w.said.push(Said::Report(*id, Response::Snapshotted { heads }));
                job.stage = Stage::Over;
                job.over = Some((true, None));
            }
            let Some((ok, error)) = &job.over else { continue };
            if job.finished && job.placing == 0 && !matches!(job.stage, Stage::Runs(_)) && self.runs.values().all(|it| it.0 != *id)
            {
                over.push((*id, *ok, error.clone()));
            }
        }
        for (id, ok, error) in over {
            let job = self.jobs.remove(&id).expect("a job over");
            for queries in job.queries {
                w.db.close(queries);
            }
            self.by_node.retain(|_, it| *it != id);
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
        w.store.write().expect("the store's lock").flush(w.effects);
        let heads: Vec<(String, ContentId)> = w.vfs.heads().into_iter().map(|(head, root)| (head.to_owned(), root)).collect();
        let ticket = heads.first().and_then(|(head, root)| w.vfs.set_head(head, *root).ok());
        w.vfs.flush(w.effects);
        Stage::Snapshot { ticket, heads, store: self.unsaved.clone() }
    }
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
    let mut seen = BTreeSet::new();
    let mut queue = vec![from];
    let mut memos = Vec::new();
    while let Some(key) = queue.pop() {
        if !seen.insert((key.kind.0, key.input)) {
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
        let work = Work::Test { targets: targets.clone(), filter: Vec::new(), all: false };
        let made = invocation(&work, Some(&targets), "/elsewhere", &[]);
        assert_eq!(made.command, Command::Test);
        assert_eq!(made.args, vec!["--workspace".to_owned()]);
        assert_eq!(made.cwd, "/w/a");
        assert_eq!(made.env, vec![("RUSTFLAGS".to_owned(), "-Dwarnings".to_owned())]);
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
}

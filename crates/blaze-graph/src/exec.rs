//! Does what the plane asks (docs/driver.md, "The executor"; docs/v2/plane.md, "The executor").
//!
//! [`drive`] runs one build of a [`Machine`] on the thread that calls it, which is the only one
//! that steps the machine: its work nodes on a worker pool made for the build, its IO nodes on
//! threads of their own (they wait, they don't compute). [`Scheduler`] is the daemon's one
//! scheduler: it owns the [`Plane`] and steps it on a thread of its own, the owner thread, whose
//! one wait is for the next input; it never waits for a worker. Its tasks run on the process's
//! [`shared`] pool, near the group the plane names, inside [`arena::measure`], and what each
//! held teaches the [`Estimates`] the next task of its kind is admitted by.

use std::{
    any::Any,
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU32, Ordering},
    },
    thread::{self, JoinHandle},
    time::Instant,
};

use blaze_protocol::events::{Event, Record};
use crossbeam_channel::{Receiver, Sender, unbounded};
use rustc_hash::FxHashMap;

use crate::{
    Cancelled,
    admission::{Estimates, TaskKind},
    arena,
    machine::{Error, Input, Machine, NodeId, Outcome, Output, Pool, Spec},
    plane::{Plane, PlaneConfig, PlaneInput, PlaneOutput, RequestId},
    pool::{self, Pool as Workers},
    pump::lock,
    topology::Topology,
};

static NEXT_ID: AtomicU32 = AtomicU32::new(0);

/// `count` node ids no other graph of this process has, the first of them: what one process
/// says of several graphs is one stream.
pub fn ids(count: usize) -> u32 {
    NEXT_ID.fetch_add(u32::try_from(count).unwrap_or(u32::MAX), Ordering::Relaxed)
}

/// What a node's work came to, and the nodes it found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Did {
    pub outcome: Outcome,
    pub added: Vec<Spec>,
}

impl Did {
    pub fn built() -> Did {
        Did { outcome: Outcome::Built, added: Vec::new() }
    }

    pub fn failed(why: impl Into<String>) -> Did {
        Did { outcome: Outcome::Failed { why: why.into() }, added: Vec::new() }
    }
}

/// The process's worker pool: a worker per CPU of [`Topology::detect`], bound to it.
///
/// # Panics
///
/// Where a worker cannot be started or bound, naming why: the pool is never made smaller.
pub fn shared() -> &'static Workers {
    static SHARED: OnceLock<Workers> = OnceLock::new();
    SHARED.get_or_init(|| {
        let bind = cfg!(any(target_os = "linux", target_os = "macos"));
        Workers::new(&Topology::detect(), bind)
            .unwrap_or_else(|why| panic!("the process's worker pool: {why}"))
    })
}

/// Runs the build of `nodes` on `machine` to its end: `work` does a node's work on a thread of
/// its pool, `said` hears every record as the machine says it, and `trace`, when given, gets
/// every input the machine took, in order ([`crate::trace`]). Whether every node was done.
///
/// The work nodes run on a pool of the machine's `work` workers made for the build (not bound
/// to CPUs: several builds may run at once), so `work` may borrow what the caller has.
///
/// A node whose work panics fails, and once the build is over the panic goes on on the calling
/// thread, as from a thread of [`std::thread::scope`]: a panic that unwinds on purpose, as
/// salsa's cancellation does, reaches whoever catches it.
pub fn drive<W>(
    machine: &mut Machine,
    nodes: Vec<Spec>,
    work: &W,
    said: &mut dyn FnMut(&Record),
    trace: Option<&mut Vec<Input>>,
) -> Result<bool, Error>
where
    W: Fn(NodeId) -> Did + Sync,
{
    let start = Instant::now();
    let now = || ns(start);
    let config = machine.config();
    let workers = Workers::new(&Topology::flat(config.work), false)
        .unwrap_or_else(|why| panic!("the build's {} worker threads: {why}", config.work));
    let (done_tx, done_rx) = unbounded::<(NodeId, Did, u64)>();
    let first_panic: Mutex<Option<Box<dyn Any + Send>>> = Mutex::new(None);
    let failed = |panic: Box<dyn Any + Send>| {
        let did = Did::failed(panicked(&*panic));
        lock(&first_panic).get_or_insert(panic);
        did
    };
    let over = thread::scope(|threads| {
        let (io_tx, io_rx) = unbounded::<NodeId>();
        for _ in 0..config.io {
            let (io_rx, done_tx, failed, now) = (io_rx.clone(), done_tx.clone(), &failed, &now);
            threads.spawn(move || {
                for id in io_rx {
                    let did = catch_unwind(AssertUnwindSafe(|| work(id))).unwrap_or_else(failed);
                    if done_tx.send((id, did, now())).is_err() {
                        return;
                    }
                }
            });
        }
        let over = workers.scope(|jobs| {
            let post = |id: NodeId, at: Pool| match at {
                Pool::Io => _ = io_tx.send(id),
                Pool::Work => {
                    let (done_tx, failed, now) = (done_tx.clone(), &failed, &now);
                    jobs.submit(None, move || work(id), move |did| {
                        _ = done_tx.send((id, did.unwrap_or_else(failed), now()));
                    });
                }
            };
            run(machine, nodes, &post, &done_rx, &now, said, trace)
        });
        // The IO threads end once nothing more can be sent them, whether the build ended or not.
        drop(io_tx);
        over
    });
    drop(workers);
    if let Some(panic) = first_panic.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner)
    {
        resume_unwind(panic);
    }
    over
}

fn run(
    machine: &mut Machine,
    nodes: Vec<Spec>,
    post: &dyn Fn(NodeId, Pool),
    done_rx: &Receiver<(NodeId, Did, u64)>,
    now: &dyn Fn() -> u64,
    said: &mut dyn FnMut(&Record),
    mut trace: Option<&mut Vec<Input>>,
) -> Result<bool, Error> {
    let mut feed = |input: Input| -> Result<Option<bool>, Error> {
        if let Some(trace) = trace.as_deref_mut() {
            trace.push(input.clone());
        }
        let mut over = None;
        for output in machine.step(input)? {
            match output {
                Output::Said(record) => said(&record),
                Output::Run { id, pool } => post(id, pool),
                Output::Finished { ok } => over = Some(ok),
            }
        }
        Ok(over)
    };
    let mut over = feed(Input::Add { at_ns: now(), nodes })?;
    if over.is_none() {
        over = feed(Input::Close { at_ns: now() })?;
    }
    while over.is_none() {
        // Every node the machine runs is on a thread that sends what came of it.
        let Ok((id, did, at_ns)) = done_rx.recv() else {
            unreachable!("`drive` keeps a sender until the build is over");
        };
        over = feed(Input::Done { at_ns, id, outcome: did.outcome, added: did.added })?;
    }
    Ok(over.unwrap_or(false))
}

fn ns(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

fn panicked(panic: &(dyn Any + Send)) -> String {
    let what = panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("a panic with no message");
    format!("panicked: {what}")
}

/// A task's work: what it came to, or [`Cancelled`] to be posted again whole.
pub type Work = Arc<dyn Fn() -> Result<Did, Cancelled> + Send + Sync>;

/// A node of a [`Scheduler`]'s request, with its work.
///
/// Nodes a task finds are added with [`Handle::add`] by the work itself, before it returns
/// (their deps may be the task's own node): a [`Did::added`] carries no work, so a task whose
/// `Did` has any fails, saying so.
#[derive(Clone)]
pub struct Task {
    pub spec: Spec,
    pub kind: TaskKind,
    pub work: Work,
}

/// What a request's opener hears of it, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Heard {
    Said(Record),
    /// The last of a request: nothing more comes.
    Finished { ok: bool },
    /// What the opener asked that the plane could not take, which changed nothing.
    Refused(Error),
}

pub struct Opened {
    pub request: RequestId,
    pub heard: Receiver<Heard>,
}

enum Msg {
    Open { request: RequestId, weight: u32, keep_going: bool, heard: Sender<Heard> },
    Add { request: RequestId, tasks: Vec<Task> },
    Close { request: RequestId },
    Cancel { request: RequestId },
    Done { at_ns: u64, id: NodeId, request: RequestId, kind: TaskKind, did: Did, peak: Option<u64>, group: Option<u32> },
    Estimates { reply: Sender<Estimates> },
    Stop,
}

/// What a running task needs to run and say it ran.
struct Job {
    id: NodeId,
    request: RequestId,
    kind: TaskKind,
    work: Work,
    tx: Sender<Msg>,
    start: Instant,
}

impl Job {
    fn done(&self, did: Did, peak: Option<u64>, group: Option<u32>) {
        let (id, request, kind) = (self.id, self.request, self.kind);
        let at_ns = ns(self.start);
        // A scheduler dropped while its tasks run takes nothing more of them.
        _ = self.tx.send(Msg::Done { at_ns, id, request, kind, did, peak, group });
    }
}

/// Asks a [`Scheduler`] for requests from any thread, the work of its tasks included.
#[derive(Clone)]
pub struct Handle {
    tx: Sender<Msg>,
    next: Arc<AtomicU32>,
}

impl Handle {
    /// A request of `weight` (its share of the threads against the others'); with
    /// `keep_going` false its first failure skips what of it has not started. Its records and
    /// its end come to [`Opened::heard`]; it is gone when the scheduler is.
    pub fn open(&self, weight: u32, keep_going: bool) -> Opened {
        let request = RequestId(self.next.fetch_add(1, Ordering::Relaxed));
        let (heard_tx, heard) = unbounded();
        _ = self.tx.send(Msg::Open { request, weight, keep_going, heard: heard_tx });
        Opened { request, heard }
    }

    /// Tasks of `request`, besides those it had; each one's predicted memory is at least what
    /// the scheduler's estimates predict for its kind.
    pub fn add(&self, request: RequestId, tasks: Vec<Task>) {
        _ = self.tx.send(Msg::Add { request, tasks });
    }

    /// No more tasks are added to `request` but by its tasks.
    pub fn close(&self, request: RequestId) {
        _ = self.tx.send(Msg::Close { request });
    }

    /// Skips what of `request` has not started; what runs is let finish, and the request
    /// finishes not ok.
    pub fn cancel(&self, request: RequestId) {
        _ = self.tx.send(Msg::Cancel { request });
    }

    /// The estimates as they are now, to be kept in the daemon's image.
    ///
    /// # Panics
    ///
    /// When the scheduler is gone.
    pub fn estimates(&self) -> Estimates {
        let (reply, estimates) = crossbeam_channel::bounded(1);
        _ = self.tx.send(Msg::Estimates { reply });
        estimates.recv().unwrap_or_else(|_| panic!("the scheduler is gone"))
    }
}

/// The daemon's one scheduler (docs/v2/plane.md): requests from any thread, through
/// [`Handle`]s; the plane stepped on its owner thread; tasks on a worker pool, IO tasks on
/// threads of their own. Dropping it stops the owner thread and waits for the IO tasks under
/// way; pool tasks under way finish on the pool, and what came of them is not taken.
///
/// A task's work runs inside [`arena::measure`], and its `Done` gives the plane the peak it
/// held and the group it ran on; after each `Done` the plane hears what the process holds
/// ([`arena::held`]). Work that returns [`Cancelled`] is posted again (the plane still sees it
/// running), behind what is ready on its group. The clock is read here, never by the plane.
pub struct Scheduler {
    handle: Handle,
    owner: Option<JoinHandle<()>>,
    io: Vec<JoinHandle<()>>,
}

impl Scheduler {
    /// A scheduler of the plane `config` says, on the process's [`shared`] pool.
    pub fn new(config: PlaneConfig, estimates: Estimates) -> Result<Scheduler, Error> {
        Scheduler::with_pool(config, estimates, shared())
    }

    /// A scheduler whose tasks run on `pool`.
    ///
    /// # Panics
    ///
    /// When a thread cannot be started, as [`std::thread::spawn`].
    pub fn with_pool(
        config: PlaneConfig,
        estimates: Estimates,
        pool: &'static Workers,
    ) -> Result<Scheduler, Error> {
        let io_threads = config.io;
        let plane = Plane::new(config)?;
        let (tx, rx) = unbounded::<Msg>();
        let (io_tx, io_rx) = unbounded::<Arc<Job>>();
        let io = (0..io_threads)
            .map(|at| {
                let io_rx = io_rx.clone();
                thread::Builder::new()
                    .name(format!("blaze-io-{at}"))
                    .spawn(move || {
                        for job in io_rx {
                            run_io(&job);
                        }
                    })
                    .unwrap_or_else(|why| panic!("an IO thread of the scheduler: {why}"))
            })
            .collect();
        let owner = Owner {
            plane,
            estimates,
            heard: FxHashMap::default(),
            works: FxHashMap::default(),
            pool,
            io: io_tx,
            tx: tx.clone(),
            start: Instant::now(),
        };
        let owner = thread::Builder::new()
            .name("blaze-plane".to_owned())
            .spawn(move || owner.serve(&rx))
            .unwrap_or_else(|why| panic!("the scheduler's owner thread: {why}"));
        Ok(Scheduler {
            handle: Handle { tx, next: Arc::new(AtomicU32::new(0)) },
            owner: Some(owner),
            io,
        })
    }

    pub fn handle(&self) -> Handle {
        self.handle.clone()
    }

    pub fn open(&self, weight: u32, keep_going: bool) -> Opened {
        self.handle.open(weight, keep_going)
    }

    pub fn add(&self, request: RequestId, tasks: Vec<Task>) {
        self.handle.add(request, tasks);
    }

    pub fn close(&self, request: RequestId) {
        self.handle.close(request);
    }

    pub fn cancel(&self, request: RequestId) {
        self.handle.cancel(request);
    }

    pub fn estimates(&self) -> Estimates {
        self.handle.estimates()
    }
}

impl Drop for Scheduler {
    fn drop(&mut self) {
        _ = self.handle.tx.send(Msg::Stop);
        if let Some(owner) = self.owner.take() {
            _ = owner.join();
        }
        for io in self.io.drain(..) {
            _ = io.join();
        }
    }
}

struct Entry {
    request: RequestId,
    kind: TaskKind,
    work: Work,
}

/// The owner thread's: the plane and everything only it changes.
struct Owner {
    plane: Plane,
    estimates: Estimates,
    heard: FxHashMap<RequestId, Sender<Heard>>,
    /// The work of each node added and not settled.
    works: FxHashMap<NodeId, Entry>,
    pool: &'static Workers,
    io: Sender<Arc<Job>>,
    tx: Sender<Msg>,
    start: Instant,
}

impl Owner {
    fn serve(mut self, rx: &Receiver<Msg>) {
        // The owner's one wait.
        while let Ok(msg) = rx.recv() {
            if !self.take(msg) {
                break;
            }
        }
    }

    /// Takes one input; false once told to stop.
    fn take(&mut self, msg: Msg) -> bool {
        let at_ns = ns(self.start);
        match msg {
            Msg::Open { request, weight, keep_going, heard } => {
                self.heard.insert(request, heard);
                let input = PlaneInput::Open { at_ns, request, weight, keep_going };
                if !self.step(request, input) {
                    self.heard.remove(&request);
                }
            }
            Msg::Add { request, tasks } => {
                let mut nodes = Vec::with_capacity(tasks.len());
                let mut new = Vec::with_capacity(tasks.len());
                for Task { mut spec, kind, work } in tasks {
                    spec.memory = spec.memory.max(self.estimates.predict(kind));
                    // A node added twice is refused by the plane: the first keeps its work.
                    if !self.works.contains_key(&spec.id) {
                        self.works.insert(spec.id, Entry { request, kind, work });
                        new.push(spec.id);
                    }
                    nodes.push(spec);
                }
                if !self.step(request, PlaneInput::Add { at_ns, request, nodes }) {
                    for id in new {
                        self.works.remove(&id);
                    }
                }
            }
            Msg::Close { request } => _ = self.step(request, PlaneInput::Close { at_ns, request }),
            Msg::Cancel { request } => {
                let outputs = self.plane.cancel(at_ns, request);
                self.outputs(request, outputs);
            }
            Msg::Done { at_ns, id, request, kind, did, peak, group } => {
                if let Some(peak) = peak {
                    self.estimates.observe(kind, peak);
                }
                let outcome = if did.added.is_empty() {
                    did.outcome
                } else {
                    Outcome::Failed {
                        why: format!(
                            "the work of node {id} found {} nodes in its `Did`, which carries no \
                             work: a scheduler's task adds what it finds with `Handle::add`",
                            did.added.len()
                        ),
                    }
                };
                let input = PlaneInput::Done {
                    at_ns,
                    id,
                    outcome,
                    added: Vec::new(),
                    group,
                    peak: peak.unwrap_or(0),
                };
                self.step(request, input);
                let held = PlaneInput::Held { at_ns: ns(self.start), bytes: arena::held() };
                self.step(request, held);
            }
            Msg::Estimates { reply } => _ = reply.send(self.estimates.clone()),
            Msg::Stop => return false,
        }
        true
    }

    /// Steps the plane; what it refuses goes to `request`'s opener. Whether it took it.
    fn step(&mut self, request: RequestId, input: PlaneInput) -> bool {
        let outputs = self.plane.step(input);
        self.outputs(request, outputs)
    }

    fn outputs(&mut self, request: RequestId, outputs: Result<Vec<PlaneOutput>, Error>) -> bool {
        match outputs {
            Ok(outputs) => {
                for output in outputs {
                    self.output(output);
                }
                true
            }
            Err(error) => {
                if let Some(heard) = self.heard.get(&request) {
                    _ = heard.send(Heard::Refused(error));
                }
                false
            }
        }
    }

    fn output(&mut self, output: PlaneOutput) {
        match output {
            PlaneOutput::Said { request, record } => {
                if let Event::Moved { id, state } = &record.event
                    && state.settled()
                {
                    self.works.remove(&NodeId(*id));
                }
                if let Some(heard) = self.heard.get(&request) {
                    _ = heard.send(Heard::Said(record));
                }
            }
            PlaneOutput::Run { id, pool, near } => self.run(id, pool, near),
            PlaneOutput::Finished { request, ok } => {
                if let Some(heard) = self.heard.remove(&request) {
                    _ = heard.send(Heard::Finished { ok });
                }
            }
        }
    }

    fn run(&mut self, id: NodeId, pool: Pool, near: Option<u32>) {
        let Some(entry) = self.works.get(&id) else {
            // The plane runs only nodes added here, each with its work: a node it runs with
            // none fails, saying so, through the owner's own queue.
            _ = self.tx.send(Msg::Done {
                at_ns: ns(self.start),
                id,
                request: RequestId(u32::MAX),
                kind: TaskKind(u32::MAX),
                did: Did::failed(format!("node {id} was run with no work")),
                peak: None,
                group: None,
            });
            return;
        };
        let job = Arc::new(Job {
            id,
            request: entry.request,
            kind: entry.kind,
            work: entry.work.clone(),
            tx: self.tx.clone(),
            start: self.start,
        });
        match pool {
            Pool::Work => post(self.pool, near, job),
            // The IO threads outlive this send: they end once the owner is gone.
            Pool::Io => _ = self.io.send(job),
        }
    }
}

/// Runs `job` on `pool` near `near`; posted again, behind what is ready on the group it ran on,
/// while its work gives way.
fn post(pool: &'static Workers, near: Option<u32>, job: Arc<Job>) {
    let again = job.clone();
    pool.submit(
        near,
        move || arena::measure(|| (job.work)()),
        move |result| match result {
            Ok((Ok(did), peak)) => again.done(did, Some(peak), pool::group()),
            Ok((Err(Cancelled), _)) => post(pool, pool::group(), again),
            Err(panic) => again.done(Did::failed(panicked(&*panic)), None, pool::group()),
        },
    );
}

/// Runs an IO task on this thread, again while its work gives way, each time after what is
/// already queued of this thread's turn: an IO task waits rather than computes.
fn run_io(job: &Job) {
    loop {
        match catch_unwind(AssertUnwindSafe(|| arena::measure(|| (job.work)()))) {
            Ok((Ok(did), peak)) => return job.done(did, Some(peak), None),
            Ok((Err(Cancelled), _)) => thread::yield_now(),
            Err(panic) => return job.done(Did::failed(panicked(&*panic)), None, None),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicU32, Ordering},
        },
        time::Duration,
    };

    use blaze_protocol::events::{Event, NodeKind, NodeState, Record};
    use crossbeam_channel::bounded;
    use rustc_hash::FxHashMap;

    use super::{Did, Heard, Opened, Scheduler, Task, Work, Workers, drive};
    use crate::{
        Cancelled, Key,
        admission::{Estimates, TaskKind},
        machine::{Config, Machine, NodeId, Pool, Spec},
        plane::PlaneConfig,
        pool::group,
        topology::Topology,
    };

    const LONG: Duration = Duration::from_secs(60);

    fn spec(id: u32, deps: &[u32], pool: Pool, key: Option<u8>) -> Spec {
        Spec {
            id: NodeId(id),
            kind: NodeKind::Names,
            name: format!("n{id}"),
            deps: deps.iter().map(|&it| NodeId(it)).collect(),
            pool,
            cost: 1,
            key: key.map(|it| Key([it; 32])),
            memory: 0,
        }
    }

    fn task(id: u32, deps: &[u32], key: Option<u8>, work: Work) -> Task {
        Task { spec: spec(id, deps, Pool::Work, key), kind: TaskKind(1), work }
    }

    fn scheduler(workers: usize) -> Scheduler {
        let pool: &'static Workers =
            Box::leak(Box::new(Workers::new(&Topology::flat(workers), false).unwrap()));
        let config = PlaneConfig { io: 1, work: workers, budget: None, starve_after: 4 };
        Scheduler::with_pool(config, Estimates::default(), pool).unwrap()
    }

    /// Everything the request said, up to its end, and whether it was ok.
    fn finish(opened: &Opened) -> (bool, Vec<Record>) {
        let mut records = Vec::new();
        loop {
            match opened.heard.recv_timeout(LONG).expect("the request ends") {
                Heard::Said(record) => records.push(record),
                Heard::Finished { ok } => return (ok, records),
                Heard::Refused(error) => panic!("refused: {error}"),
            }
        }
    }

    /// Work nodes run on the build's pool, IO nodes on threads of no pool, and the work borrows
    /// what the caller has.
    #[test]
    fn drive_runs_work_on_the_pool_and_io_beside_it() {
        let ran: Mutex<Vec<(u32, Option<u32>)>> = Mutex::new(Vec::new());
        let mut machine = Machine::new(Config { io: 2, work: 3, keep_going: false }).unwrap();
        let nodes = vec![
            spec(0, &[], Pool::Io, None),
            spec(1, &[0], Pool::Work, None),
            spec(2, &[0], Pool::Work, None),
            spec(3, &[1, 2], Pool::Io, None),
        ];
        let work = |id: NodeId| {
            ran.lock().unwrap().push((id.0, group()));
            Did::built()
        };
        let ok = drive(&mut machine, nodes, &work, &mut |_| {}, None).unwrap();
        assert!(ok);
        let mut ran = ran.into_inner().unwrap();
        ran.sort();
        assert_eq!(ran, [(0, None), (1, Some(0)), (2, Some(0)), (3, None)]);
    }

    /// Two requests of weights 1 and 3 ask for the same ten keys: each key runs once, and both
    /// finish.
    #[test]
    fn shared_keys_run_once_for_two_requests() {
        let scheduler = scheduler(4);
        let runs: Arc<Mutex<FxHashMap<u8, u32>>> = Arc::default();
        let keyed = |id: u32, key: u8| {
            let runs = runs.clone();
            let work: Work = Arc::new(move || {
                *runs.lock().unwrap().entry(key).or_default() += 1;
                std::thread::sleep(Duration::from_millis(1));
                Ok(Did::built())
            });
            task(id, &[], Some(key), work)
        };
        let light = scheduler.open(1, false);
        let heavy = scheduler.open(3, false);
        scheduler.add(light.request, (0..10).map(|key| keyed(u32::from(key), key)).collect());
        scheduler.add(heavy.request, (0..10).map(|key| keyed(100 + u32::from(key), key)).collect());
        scheduler.close(light.request);
        scheduler.close(heavy.request);
        assert!(finish(&light).0);
        assert!(finish(&heavy).0);
        let runs = runs.lock().unwrap();
        assert_eq!(runs.len(), 10);
        assert!(runs.values().all(|&it| it == 1), "{runs:?}");
    }

    /// A request cancelled while its first task runs: what waits for it never runs, the task
    /// runs to its end, and the request finishes not ok.
    #[test]
    fn a_cancelled_request_finishes_not_ok_and_its_running_work_completes() {
        let scheduler = scheduler(2);
        let (started_tx, started_rx) = bounded::<()>(1);
        let (gate_tx, gate_rx) = bounded::<()>(0);
        let (ended_tx, ended_rx) = bounded::<()>(1);
        let second_ran = Arc::new(AtomicBool::new(false));
        let first: Work = Arc::new(move || {
            _ = started_tx.send(());
            gate_rx.recv_timeout(LONG).expect("the gate opens");
            _ = ended_tx.send(());
            Ok(Did::built())
        });
        let ran = second_ran.clone();
        let second: Work = Arc::new(move || {
            ran.store(true, Ordering::SeqCst);
            Ok(Did::built())
        });
        let opened = scheduler.open(1, false);
        scheduler.add(opened.request, vec![task(0, &[], None, first), task(1, &[0], None, second)]);
        scheduler.close(opened.request);
        started_rx.recv_timeout(LONG).unwrap();
        scheduler.cancel(opened.request);
        gate_tx.send_timeout((), LONG).unwrap();
        let (ok, _) = finish(&opened);
        assert!(!ok);
        ended_rx.recv_timeout(LONG).expect("the running work completes");
        assert!(!second_ran.load(Ordering::SeqCst));
    }

    #[test]
    fn work_that_gives_way_twice_is_posted_again_and_done() {
        let scheduler = scheduler(2);
        let tries = Arc::new(AtomicU32::new(0));
        let counted = tries.clone();
        let work: Work = Arc::new(move || {
            if counted.fetch_add(1, Ordering::SeqCst) < 2 { Err(Cancelled) } else { Ok(Did::built()) }
        });
        let opened = scheduler.open(1, false);
        scheduler.add(opened.request, vec![task(7, &[], None, work)]);
        scheduler.close(opened.request);
        let (ok, records) = finish(&opened);
        assert!(ok);
        assert_eq!(tries.load(Ordering::SeqCst), 3);
        assert!(records.iter().any(|it| matches!(
            it.event,
            Event::Moved { id: 7, state: NodeState::Built }
        )));
        assert!(scheduler.estimates().by_kind.contains_key(&TaskKind(1)));
    }

    /// Nodes a task finds are added by its work, through a handle, and wait for it.
    #[test]
    fn a_task_adds_what_it_finds_through_its_handle() {
        let scheduler = scheduler(2);
        let opened = scheduler.open(2, false);
        let (request, handle) = (opened.request, scheduler.handle());
        let found_ran = Arc::new(AtomicBool::new(false));
        let ran = found_ran.clone();
        let finder: Work = Arc::new(move || {
            let ran = ran.clone();
            let found: Work = Arc::new(move || {
                ran.store(true, Ordering::SeqCst);
                Ok(Did::built())
            });
            handle.add(request, vec![task(21, &[20], None, found)]);
            Ok(Did::built())
        });
        scheduler.add(request, vec![task(20, &[], None, finder)]);
        scheduler.close(request);
        assert!(finish(&opened).0);
        assert!(found_ran.load(Ordering::SeqCst));
    }

    #[test]
    fn a_refused_input_is_heard_by_its_opener() {
        let scheduler = scheduler(1);
        let opened = scheduler.open(0, false);
        assert!(matches!(opened.heard.recv_timeout(LONG), Ok(Heard::Refused(_))));
        let opened = scheduler.open(1, false);
        let built: Work = Arc::new(|| Ok(Did::built()));
        scheduler.add(opened.request, vec![task(30, &[31], None, built)]);
        assert!(matches!(opened.heard.recv_timeout(LONG), Ok(Heard::Refused(_))));
    }
}

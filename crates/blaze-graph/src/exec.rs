//! Does what the plane asks (docs/driver.md, "The executor"; docs/v2/plan/lanes/core/plane.md, "The executor").
//!
//! [`drive`] runs one build of a [`Machine`] on the thread that calls it, which is the only one
//! that steps the machine: its work nodes on a worker pool lent to the build alone, its IO nodes
//! on a second one (they wait, they don't compute). [`Scheduler`] is the daemon's one
//! scheduler: it owns the [`Plane`] and steps it on a thread of its own, the owner thread, whose
//! one wait is for the next input; it never waits for a worker. Its tasks run on the process's
//! [`shared`] pool, near the group the plane names, inside [`arena::measure`], and what each
//! held and how long it ran teach the [`Predict`] the next task's cost and memory come from.

use std::{
    any::Any,
    cell::UnsafeCell,
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    ptr::{self, NonNull},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU32, Ordering},
    },
    thread::{self, JoinHandle},
    time::Instant,
};

use blaze_protocol::events::{Event, Record};
use crossbeam_channel::{Receiver, Sender, unbounded};
use rustc_hash::FxHashMap;

use crate::{
    Cancelled, Key,
    admission::{Predict, TaskKind},
    arena,
    machine::{Error, Input, Machine, NodeId, Outcome, Output, Pool, Spec},
    plane::{Memo, Plane, PlaneConfig, PlaneInput, PlaneOutput, RequestId},
    pool::{self, Header, Pool as Workers},
    pump::Lock,
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
/// Where the machine's topology cannot be read, or a worker cannot be started or bound,
/// naming why: the pool is never made smaller or shaped by a guess.
pub fn shared() -> &'static Workers {
    static SHARED: OnceLock<Workers> = OnceLock::new();
    SHARED.get_or_init(|| {
        let bind = cfg!(any(target_os = "linux", target_os = "macos"));
        Topology::detect()
            .and_then(|topology| Workers::new(&topology, bind))
            .unwrap_or_else(|why| panic!("the process's worker pool: {why}"))
    })
}

/// Runs the build of `nodes` on `machine` to its end: `work` does a node's work on a thread of
/// its pool, `said` hears every record as the machine says it, and `trace`, when given, gets
/// every input the machine took, in order ([`crate::trace`]). Whether every node was done.
///
/// The work nodes run on a pool of the machine's `work` workers and the IO nodes on one of its
/// `io`, both lent to this build alone and kept for the next build of their size once it's
/// over (not bound to CPUs: several builds may run at once). `work` may borrow what the caller
/// has, and runs in its thread's task arena ([`arena::task`]), reset when the node is done.
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
    let workers = Lent::take(config.work, "worker");
    let io = Lent::take(config.io, "IO");
    let (done_tx, done_rx) = unbounded::<(NodeId, Did, u64)>();
    // Taken only to keep a node's panic, once it has unwound.
    let first_panic: Lock<Option<Box<dyn Any + Send>>> = Lock::new(None);
    let failed = |panic: Box<dyn Any + Send>| {
        let did = Did::failed(panicked(&*panic));
        first_panic.lock().get_or_insert(panic);
        did
    };
    let over = workers.pool().scope(|jobs| {
        io.pool().scope(|waits| {
            let post = |id: NodeId, at: Pool| {
                let (done_tx, failed, now) = (done_tx.clone(), &failed, &now);
                let done = move |did: thread::Result<Did>| {
                    _ = done_tx.send((id, did.unwrap_or_else(failed), now()));
                };
                let run = move || arena::task(|| work(id));
                match at {
                    Pool::Io => waits.submit(None, run, done),
                    Pool::Work => jobs.submit(None, run, done),
                }
            };
            run(machine, nodes, &post, &done_rx, &now, said, trace)
        })
    });
    drop((workers, io));
    if let Some(panic) = first_panic.into_inner()
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

/// The pools no [`drive`] is using, each kept for the next build of its size, so a build
/// starts no thread once one of its size has run. Taken only to take or give back a pool.
static SPARE: Lock<Vec<Workers>> = Lock::new(Vec::new());

/// A pool of `workers` threads that only one [`drive`] uses, handed back to [`SPARE`] when
/// it's over: a build runs as many nodes at once as its machine says, whatever else runs.
struct Lent(Option<Workers>);

impl Lent {
    fn take(workers: usize, what: &str) -> Lent {
        let spare = {
            let mut spare = SPARE.lock();
            spare.iter().position(|it| it.workers() == workers).map(|at| spare.swap_remove(at))
        };
        Lent(Some(spare.unwrap_or_else(|| {
            Workers::new(&Topology::flat(workers), false)
                .unwrap_or_else(|why| panic!("the build's {workers} {what} threads: {why}"))
        })))
    }

    fn pool(&self) -> &Workers {
        self.0.as_ref().expect("a lent pool is held until it's dropped")
    }
}

impl Drop for Lent {
    fn drop(&mut self) {
        if let Some(pool) = self.0.take() {
            SPARE.lock().push(pool);
        }
    }
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
/// The work of a node with a key puts its value in the database before it returns: the plane
/// keeps no memo, and asks the scheduler's [`Memo`] whether a key is settled before it runs a
/// node of it. A value that reached the database only after the `Done` would leave a node of
/// its key that became ready in between to run again.
///
/// The nodes a task finds go in its [`Did::added`] as [`Handle::found`] gives them, which
/// gives the scheduler their work first; a found node with no work fails the task that found
/// it, saying so.
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
    Found { request: RequestId, tasks: Vec<Task> },
    Close { request: RequestId },
    Cancel { request: RequestId },
    Done {
        at_ns: u64,
        /// The run's slot in the owner's [`Table`], free again once this is taken; `None` for
        /// a node run with no work, which had none.
        slot: Option<u32>,
        id: NodeId,
        request: RequestId,
        kind: TaskKind,
        key: Option<Key>,
        did: Did,
        /// What its last run held and took; `None` when it panicked.
        measure: Option<Measure>,
        group: Option<u32>,
    },
    /// What the predictor learned, as [`Predict::encode`] gives it.
    Estimates { reply: Sender<Vec<u8>> },
    /// How many slots the owner's table has made.
    #[cfg(test)]
    Slots { reply: Sender<usize> },
    Stop,
}

/// What every run of a scheduler reaches besides its slot. Leaked as the scheduler is made: a
/// run's last act is its `Done`'s send through `tx`, which nothing can wait for the end of, so
/// the sender (and the channel it keeps) must outlive every run.
struct Ctx {
    tx: Sender<Msg>,
    /// The IO threads' queue; `None` stops one of them.
    io: Sender<Option<Ran>>,
    pool: &'static Workers,
    start: Instant,
}

/// What a task's run that ended held at most, and the time it ran (its own, not the tries
/// that gave way before it).
#[derive(Debug, Clone, Copy)]
struct Measure {
    peak: u64,
    ran_ns: u64,
}

/// A run under way: the node, and the work it runs (a clone the owner made and drops).
struct Live {
    id: NodeId,
    request: RequestId,
    kind: TaskKind,
    key: Option<Key>,
    /// On its request's critical path, as the plane said when it ran it.
    critical: bool,
    work: Work,
}

/// A slot of the owner's [`Table`]: the pool's job itself (its header first), so posting a run
/// allocates nothing and clones no `Arc` or sender. Filled by the owner, then read only by the
/// thread that runs it until it sends its `Done`; then the owner's again.
#[repr(C)]
struct Run {
    header: Header,
    ctx: &'static Ctx,
    at: u32,
    live: UnsafeCell<Option<Live>>,
}

// SAFETY: `live` has one reader or writer at a time: the owner while the slot is free (or
// filling it), then the thread holding its `Ran`, handed over by the pool's queue or the IO
// queue and back by the `Done` message, each of which orders the writes before the reads. The
// other fields are written only as the chunk is made.
unsafe impl Sync for Run {}

/// How many slots a [`Table`] grows by.
const RUNS: usize = 64;

/// The runs under way, by slot, each reused once its `Done` is in: chunks of [`RUNS`] that
/// never move, so a slot's address is its job's for as long as it runs. Only the owner thread
/// takes and frees slots; a run that ends sends its slot's index back, nothing else.
#[derive(Default)]
struct Table {
    /// Leaked boxes, freed on drop: a `Box` moved as the list grows would claim its chunk
    /// unshared while runs read it.
    chunks: Vec<&'static [Run]>,
    free: Vec<u32>,
    running: usize,
}

impl Drop for Table {
    fn drop(&mut self) {
        // An owner unwinding with runs under way leaves their slots to them.
        if self.running > 0 {
            return;
        }
        for chunk in self.chunks.drain(..) {
            // SAFETY: `grow` leaked the chunk, and no run is under way to name a slot of it.
            drop(unsafe { Box::from_raw(ptr::from_ref(chunk).cast_mut()) });
        }
    }
}

impl Table {
    fn slot(&self, at: u32) -> &Run {
        &self.chunks[at as usize / RUNS][at as usize % RUNS]
    }

    fn take(&mut self, ctx: &'static Ctx, live: Live) -> Ran {
        let at = match self.free.pop() {
            Some(at) => at,
            None => self.grow(ctx),
        };
        let run = self.slot(at);
        // SAFETY: a free slot is named by no `Ran`, so only this (the owner) thread reaches it.
        unsafe { *run.live.get() = Some(live) };
        self.running += 1;
        Ran(NonNull::from(run))
    }

    /// One chunk more: its first slot, the others free.
    fn grow(&mut self, ctx: &'static Ctx) -> u32 {
        let first = u32::try_from(self.chunks.len() * RUNS)
            .unwrap_or_else(|_| panic!("a scheduler with 2^32 runs under way"));
        let end = first + RUNS as u32;
        let chunk: Box<[Run]> = (first..end)
            .map(|at| Run { header: Header::new(run_pooled), ctx, at, live: UnsafeCell::new(None) })
            .collect();
        self.chunks.push(Box::leak(chunk));
        self.free.extend((first + 1..end).rev());
        first
    }

    fn free(&mut self, at: u32) {
        let run = self.slot(at);
        // SAFETY: its `Done` is in: the `Ran` that named it was consumed sending it, so only
        // this (the owner) thread reaches it.
        drop(unsafe { (*run.live.get()).take() });
        self.free.push(at);
        self.running -= 1;
    }
}

/// The one name of a filled slot, held by the thread that runs it: made by [`Table::take`],
/// consumed by [`Ran::done`] or by posting it again.
struct Ran(NonNull<Run>);

// SAFETY: a `Ran` moves to the thread that runs it with the pool's queue or the IO queue (both
// order the owner's writes to the slot before the reads); what the slot holds is `Send`.
unsafe impl Send for Ran {}

impl Ran {
    fn run(&self) -> &Run {
        // SAFETY: a slot stays in its table (whose chunks never move) until the owner frees it,
        // which is after this `Ran` is consumed; the owner waits for its runs before it drops
        // the table.
        unsafe { self.0.as_ref() }
    }

    fn live(&self) -> &Live {
        // SAFETY: while a `Ran` lives its slot is filled and nobody writes it (see `Run`).
        unsafe { &*self.run().live.get() }.as_ref().expect("a slot given to a run is filled")
    }

    /// Says what came of the run. Its last touch of the slot is reading it for the message.
    fn done(self, did: Did, measure: Option<Measure>, group: Option<u32>) {
        let (run, live) = (self.run(), self.live());
        let ctx = run.ctx;
        let msg = Msg::Done {
            at_ns: ns(ctx.start),
            slot: Some(run.at),
            id: live.id,
            request: live.request,
            kind: live.kind,
            key: live.key,
            did,
            measure,
            group,
        };
        _ = ctx.tx.send(msg);
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

    /// Tasks of `request`, besides those it had; each one's spec takes the scheduler's
    /// [`Prediction`](crate::admission::Prediction) for its kind and key (its memory never
    /// under the predicted peak, its cost the predicted one when there is one).
    pub fn add(&self, request: RequestId, tasks: Vec<Task>) {
        _ = self.tx.send(Msg::Add { request, tasks });
    }

    /// Gives the scheduler the work of the nodes a running task of `request` found, called by
    /// that task's work on its own thread before it returns; what it gives back goes in the
    /// task's [`Did::added`]. A request closed takes new nodes only so.
    pub fn found(&self, request: RequestId, tasks: Vec<Task>) -> Vec<Spec> {
        let specs = tasks.iter().map(|it| it.spec.clone()).collect();
        _ = self.tx.send(Msg::Found { request, tasks });
        specs
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

    /// What the scheduler's predictor has learned, as its [`Predict::encode`] gives it, to be
    /// kept in the daemon's image.
    ///
    /// # Panics
    ///
    /// When the scheduler is gone.
    pub fn estimates(&self) -> Vec<u8> {
        let (reply, estimates) = crossbeam_channel::bounded(1);
        _ = self.tx.send(Msg::Estimates { reply });
        estimates.recv().unwrap_or_else(|_| panic!("the scheduler is gone"))
    }

    #[cfg(test)]
    fn slots(&self) -> usize {
        let (reply, slots) = crossbeam_channel::bounded(1);
        _ = self.tx.send(Msg::Slots { reply });
        slots.recv().unwrap_or_else(|_| panic!("the scheduler is gone"))
    }
}

/// The daemon's one scheduler (docs/v2/plan/lanes/core/plane.md): requests from any thread, through
/// [`Handle`]s; the plane stepped on its owner thread; tasks on a worker pool, IO tasks on
/// threads of their own. Dropping it waits for the tasks under way (pool and IO, each posted
/// again until it ends; what came of them is not taken), then stops the owner thread and the
/// IO threads: so it is never dropped by one of its own tasks.
///
/// A run is a slot of the owner's table, reused once its `Done` is in, and the slot itself is
/// the pool's job: posting a task allocates nothing and clones no `Arc` or sender, and its end
/// is one message to the owner.
///
/// A task's work runs in its thread's task arena ([`arena::task`]: a pool worker's is on its
/// NUMA node, in huge pages, and reset when the task ends) inside [`arena::measure`], and its
/// `Done` gives the plane the peak it held and the group it ran on; after each `Done` the plane
/// hears what the process holds ([`arena::held`]). The scheduler's [`Predict`] gives each task
/// added or found its cost and memory before the plane sees it, and learns the peak and own run
/// time of each run that ends with a result (a run that gave way and one that panicked teach it
/// nothing), under the task's kind and its spec's key. A task the plane says is on its request's
/// critical path runs on a fast group of the pool ([`Workers::submit_critical`]). Work that
/// returns [`Cancelled`] is posted again (the plane still sees it running), behind what is
/// ready on its group. The clock is read here, never by the plane.
///
/// Every request is released as it opens: once it finishes, the plane gives its room (nodes,
/// names, deps, its id) to the next, so a daemon's plane holds only the requests under way.
pub struct Scheduler {
    handle: Handle,
    owner: Option<JoinHandle<()>>,
    io: Vec<JoinHandle<()>>,
}

impl Scheduler {
    /// A scheduler of the plane `config` says, on the process's [`shared`] pool. The plane
    /// asks `memo`, the database's, whether a key is settled before it runs a node of it;
    /// `predict` gives each task's cost and memory and learns from each run that ends.
    pub fn new(
        config: PlaneConfig,
        predict: Box<dyn Predict>,
        memo: Box<dyn Memo + Send>,
    ) -> Result<Scheduler, Error> {
        Scheduler::with_pool(config, predict, memo, shared())
    }

    /// A scheduler whose tasks run on `pool`.
    ///
    /// # Panics
    ///
    /// When a thread cannot be started, as [`std::thread::spawn`].
    pub fn with_pool(
        config: PlaneConfig,
        predict: Box<dyn Predict>,
        memo: Box<dyn Memo + Send>,
        pool: &'static Workers,
    ) -> Result<Scheduler, Error> {
        let io_threads = config.io;
        let plane = Plane::with_memo(config, memo)?;
        let (tx, rx) = unbounded::<Msg>();
        let (io_tx, io_rx) = unbounded::<Option<Ran>>();
        let io = (0..io_threads)
            .map(|at| {
                let io_rx = io_rx.clone();
                thread::Builder::new()
                    .name(format!("blaze-io-{at}"))
                    .spawn(move || {
                        // Ends when the owner, its runs over, says so.
                        while let Ok(Some(run)) = io_rx.recv() {
                            run_io(run);
                        }
                    })
                    .unwrap_or_else(|why| panic!("an IO thread of the scheduler: {why}"))
            })
            .collect();
        let ctx: &'static Ctx =
            Box::leak(Box::new(Ctx { tx: tx.clone(), io: io_tx, pool, start: Instant::now() }));
        let owner = Owner {
            plane,
            predict,
            heard: FxHashMap::default(),
            works: FxHashMap::default(),
            ctx,
            table: Table::default(),
            io_threads,
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

    pub fn estimates(&self) -> Vec<u8> {
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
    key: Option<Key>,
    work: Work,
}

/// The owner thread's: the plane and everything only it changes.
struct Owner {
    plane: Plane,
    predict: Box<dyn Predict>,
    heard: FxHashMap<RequestId, Sender<Heard>>,
    /// The work of each node added and not settled.
    works: FxHashMap<NodeId, Entry>,
    ctx: &'static Ctx,
    table: Table,
    io_threads: usize,
}

impl Owner {
    fn serve(mut self, rx: &Receiver<Msg>) {
        /// Stops the IO threads as the owner ends, unwinding too.
        struct StopIo(&'static Ctx, usize);
        impl Drop for StopIo {
            fn drop(&mut self) {
                for _ in 0..self.1 {
                    _ = self.0.io.send(None);
                }
            }
        }
        let _stop_io = StopIo(self.ctx, self.io_threads);
        // The owner's one wait.
        while let Ok(msg) = rx.recv() {
            if !self.take(msg) {
                break;
            }
        }
        // Each run under way reads a slot of the table: it ends before the table goes, and
        // nothing more comes of it.
        while self.table.running > 0 {
            match rx.recv() {
                Ok(Msg::Done { slot: Some(slot), .. }) => self.table.free(slot),
                Ok(_) => {}
                Err(_) => unreachable!("the scheduler's context keeps a sender"),
            }
        }
    }

    /// Takes one input; false once told to stop.
    fn take(&mut self, msg: Msg) -> bool {
        let at_ns = ns(self.ctx.start);
        match msg {
            Msg::Open { request, weight, keep_going, heard } => {
                self.heard.insert(request, heard);
                let input = PlaneInput::Open { at_ns, request, weight, keep_going };
                if self.step(request, input) {
                    // Its room in the plane is given back once it finishes; after that a step
                    // naming it is refused as `UnknownRequest`.
                    let released = self.plane.release(request);
                    self.outputs(request, released.map(|()| Vec::new()));
                } else {
                    self.heard.remove(&request);
                }
            }
            Msg::Add { request, tasks } => {
                let mut nodes = Vec::with_capacity(tasks.len());
                let mut new = Vec::with_capacity(tasks.len());
                for Task { mut spec, kind, work } in tasks {
                    self.predict.predict(kind, spec.key).apply(&mut spec);
                    // A node added twice is refused by the plane: the first keeps its work.
                    if !self.works.contains_key(&spec.id) {
                        self.works.insert(spec.id, Entry { request, kind, key: spec.key, work });
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
            Msg::Found { request, tasks } => {
                for Task { spec, kind, work } in tasks {
                    // Found again by a task posted again: the first keeps its work.
                    let key = spec.key;
                    self.works.entry(spec.id).or_insert(Entry { request, kind, key, work });
                }
            }
            Msg::Close { request } => _ = self.step(request, PlaneInput::Close { at_ns, request }),
            Msg::Cancel { request } => {
                let outputs = self.plane.cancel(at_ns, request);
                self.outputs(request, outputs);
            }
            Msg::Done { at_ns, slot, id, request, kind, key, did, measure, group } => {
                if let Some(slot) = slot {
                    self.table.free(slot);
                }
                if let Some(Measure { peak, ran_ns }) = measure {
                    self.predict.observe(kind, key, ran_ns, peak);
                }
                let (mut outcome, mut added) = (did.outcome, did.added);
                let workless = added.iter().map(|spec| spec.id).find(|found| {
                    !self.works.get(found).is_some_and(|it| it.request == request)
                });
                if let Some(found) = workless {
                    outcome = Outcome::Failed {
                        why: format!(
                            "node {found} found by node {id} has no work: a scheduler's task \
                             gives the work of what it finds with `Handle::found`"
                        ),
                    };
                    added.clear();
                }
                for spec in &mut added {
                    let kind = self.works[&spec.id].kind;
                    self.predict.predict(kind, spec.key).apply(spec);
                }
                let peak = measure.map_or(0, |it| it.peak);
                let input = PlaneInput::Done { at_ns, id, outcome, added, group, peak };
                if !self.step(request, input) {
                    // What it found could not be added: the node fails rather than run forever.
                    let why = format!("what node {id} found could not be added to {request}");
                    let outcome = Outcome::Failed { why };
                    let input = PlaneInput::Done { at_ns, id, outcome, added: Vec::new(), group, peak };
                    self.step(request, input);
                }
                let held = PlaneInput::Held { at_ns: ns(self.ctx.start), bytes: arena::held() };
                self.step(request, held);
            }
            Msg::Estimates { reply } => _ = reply.send(self.predict.encode()),
            #[cfg(test)]
            Msg::Slots { reply } => _ = reply.send(self.table.chunks.len() * RUNS),
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
            PlaneOutput::Run { id, pool, near, critical } => self.run(id, pool, near, critical),
            PlaneOutput::Finished { request, ok } => {
                // Work found by a task that then gave way, and never added.
                self.works.retain(|_, it| it.request != request);
                if let Some(heard) = self.heard.remove(&request) {
                    _ = heard.send(Heard::Finished { ok });
                }
            }
        }
    }

    fn run(&mut self, id: NodeId, pool: Pool, near: Option<u32>, critical: bool) {
        let Some(entry) = self.works.get(&id) else {
            // The plane runs only nodes added here, each with its work: a node it runs with
            // none fails, saying so, through the owner's own queue.
            _ = self.ctx.tx.send(Msg::Done {
                at_ns: ns(self.ctx.start),
                slot: None,
                id,
                request: RequestId(u32::MAX),
                kind: TaskKind(u32::MAX),
                key: None,
                did: Did::failed(format!("node {id} was run with no work")),
                measure: None,
                group: None,
            });
            return;
        };
        let (request, kind, key, work) = (entry.request, entry.kind, entry.key, entry.work.clone());
        let live = Live { id, request, kind, key, critical, work };
        let run = self.table.take(self.ctx, live);
        match pool {
            Pool::Work => post(self.ctx, near, critical, run),
            // The IO threads outlive this send: they end once the owner, its runs over, says so.
            Pool::Io => _ = self.ctx.io.send(Some(run)),
        }
    }
}

/// Runs `run` on the scheduler's pool near `near` (on a fast group when it is critical).
fn post(ctx: &Ctx, near: Option<u32>, critical: bool, run: Ran) {
    // SAFETY: the slot's header is its first field (`Run` is `repr(C)`) and runs `run_pooled`;
    // the slot stays in its table, unmoved, until its `Done` is taken, and `run`, moved here,
    // was its one name.
    let job = unsafe { pool::Job::from_header(run.0.cast::<Header>()) };
    ctx.pool.push_placed(near, critical, job);
}

/// A run's pool job: its work, then its `Done`; posted again, behind what is ready on the group
/// it ran on, while its work gives way.
///
/// # Safety
///
/// `header` is the header of a filled slot of a live [`Table`], given to the pool by [`post`].
unsafe fn run_pooled(header: NonNull<Header>) {
    let run = Ran(header.cast::<Run>());
    let (ctx, critical) = (run.run().ctx, run.live().critical);
    let result = catch_unwind(AssertUnwindSafe(|| measured(run.live())));
    let group = pool::group();
    match result {
        Ok((Ok(did), measure)) => run.done(did, Some(measure), group),
        Ok((Err(Cancelled), _)) => post(ctx, group, critical, run),
        Err(panic) => run.done(Did::failed(panicked(&*panic)), None, group),
    }
}

/// `live`'s work in this thread's task arena, reset when it ends, the peak it held and the
/// time it took.
fn measured(live: &Live) -> (Result<Did, Cancelled>, Measure) {
    let start = Instant::now();
    let (did, peak) = arena::measure(|| arena::task(|| (live.work)()));
    (did, Measure { peak, ran_ns: ns(start) })
}

/// Runs an IO task on this thread; posted again, behind what is queued for the IO threads,
/// while its work gives way.
fn run_io(run: Ran) {
    match catch_unwind(AssertUnwindSafe(|| measured(run.live()))) {
        Ok((Ok(did), measure)) => run.done(did, Some(measure), None),
        Ok((Err(Cancelled), _)) => {
            let ctx = run.run().ctx;
            _ = ctx.io.send(Some(run));
        }
        Err(panic) => run.done(Did::failed(panicked(&*panic)), None, None),
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_types, reason = "a test's own bookkeeping, off every hot path")]
mod tests {
    use std::{
        collections::BTreeSet,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicU32, Ordering},
        },
        thread::ThreadId,
        time::Duration,
    };

    use blaze_protocol::events::{Event, NodeKind, NodeState, Record};
    use crossbeam_channel::bounded;
    use rustc_hash::FxHashMap;

    use super::{Did, Heard, Opened, RUNS, SPARE, Scheduler, Task, Work, Workers, drive};
    use crate::{
        Cancelled, Key,
        admission::{Estimates, Predict, Prediction, TaskKind},
        arena,
        machine::{Config, Machine, NodeId, Outcome, Pool, Spec},
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

    /// The database a test's work puts each key's value in before its `Done`, as real work
    /// does; the scheduler's memo reads it.
    type Db = Arc<Mutex<FxHashMap<Key, Outcome>>>;

    fn scheduler(workers: usize) -> (Scheduler, Db) {
        scheduler_on(&Topology::flat(workers)).0
    }

    /// A scheduler on a pool of `topology`'s shape, and the pool.
    fn scheduler_on(topology: &Topology) -> ((Scheduler, Db), &'static Workers) {
        scheduler_with(topology, Box::new(Estimates::default()))
    }

    fn scheduler_with(
        topology: &Topology,
        predict: Box<dyn Predict>,
    ) -> ((Scheduler, Db), &'static Workers) {
        let pool: &'static Workers = Box::leak(Box::new(Workers::new(topology, false).unwrap()));
        let workers = pool.workers();
        let config = PlaneConfig { io: 1, work: workers, budget: None, starve_after: 4 };
        let db = Db::default();
        let memo = {
            let db = db.clone();
            move |key: Key| db.lock().expect("the test's database").get(&key).cloned()
        };
        ((Scheduler::with_pool(config, predict, Box::new(memo), pool).unwrap(), db), pool)
    }

    /// What a [`Recorder`] was asked and told, in order.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Asked {
        Predict(TaskKind, Option<Key>),
        Observe { kind: TaskKind, key: Option<Key>, ran_ns: u64 },
    }

    /// A predictor that keeps what it is asked and told, and predicts `prediction` for all.
    struct Recorder {
        asked: Arc<Mutex<Vec<Asked>>>,
        prediction: Prediction,
    }

    impl Predict for Recorder {
        fn predict(&self, kind: TaskKind, key: Option<Key>) -> Prediction {
            self.asked.lock().unwrap().push(Asked::Predict(kind, key));
            self.prediction
        }

        fn observe(&mut self, kind: TaskKind, key: Option<Key>, ran_ns: u64, _peak: u64) {
            self.asked.lock().unwrap().push(Asked::Observe { kind, key, ran_ns });
        }

        fn encode(&self) -> Vec<u8> {
            self.asked.lock().unwrap().len().to_le_bytes().to_vec()
        }
    }

    /// The scheduler asks its predictor of each task added and found, with the task's kind and
    /// key, and tells it the own run time of each run that ended: a task that gave way twice
    /// is observed once, for the run that finished, not the tries before it. Its estimates are
    /// the predictor's bytes.
    #[test]
    fn the_scheduler_asks_its_predictor_and_teaches_it_each_run() {
        let asked: Arc<Mutex<Vec<Asked>>> = Arc::default();
        let recorder = Recorder {
            asked: asked.clone(),
            prediction: Prediction { cost: Some(5), peak: 1 << 10 },
        };
        let ((scheduler, _), _) = scheduler_with(&Topology::flat(2), Box::new(recorder));
        let opened = scheduler.open(1, false);
        let (request, handle) = (opened.request, scheduler.handle());
        let tries = Arc::new(AtomicU32::new(0));
        let counted = tries.clone();
        let slow: Work = Arc::new(move || {
            if counted.fetch_add(1, Ordering::SeqCst) < 2 {
                std::thread::sleep(Duration::from_millis(200));
                return Err(Cancelled);
            }
            std::thread::sleep(Duration::from_millis(5));
            Ok(Did::built())
        });
        let finder: Work = Arc::new(move || {
            let found = Task {
                spec: spec(52, &[51], Pool::Work, Some(9)),
                kind: TaskKind(3),
                work: Arc::new(|| Ok(Did::built())),
            };
            let added = handle.found(request, vec![found]);
            Ok(Did { outcome: Outcome::Built, added })
        });
        let mut keyed = task(51, &[50], Some(8), finder);
        keyed.kind = TaskKind(2);
        scheduler.add(request, vec![task(50, &[], None, slow), keyed]);
        scheduler.close(request);
        assert!(finish(&opened).0);
        assert_eq!(tries.load(Ordering::SeqCst), 3);

        let asked = asked.lock().unwrap().clone();
        let key = |it: u8| Some(Key([it; 32]));
        let predicted: Vec<_> =
            asked.iter().filter(|it| matches!(it, Asked::Predict(..))).cloned().collect();
        assert_eq!(predicted, [
            Asked::Predict(TaskKind(1), None),
            Asked::Predict(TaskKind(2), key(8)),
            Asked::Predict(TaskKind(3), key(9)),
        ]);
        let observed: Vec<(TaskKind, Option<Key>, u64)> = asked
            .iter()
            .filter_map(|it| match *it {
                Asked::Observe { kind, key, ran_ns } => Some((kind, key, ran_ns)),
                Asked::Predict(..) => None,
            })
            .collect();
        let kinds: Vec<_> = observed.iter().map(|&(kind, key, _)| (kind, key)).collect();
        assert_eq!(kinds, [(TaskKind(1), None), (TaskKind(2), key(8)), (TaskKind(3), key(9))]);
        let slow_ns = observed[0].2;
        assert!(
            (5_000_000..200_000_000).contains(&slow_ns),
            "the run that finished took {slow_ns} ns, not the tries that gave way"
        );
        assert_eq!(scheduler.estimates(), 6usize.to_le_bytes());
    }

    /// A chain of five tasks on a machine of P and E cores: each, the only work of its request
    /// when it runs, is on its critical path, and the scheduler submits it as critical.
    #[test]
    fn a_task_on_the_critical_path_is_submitted_as_critical() {
        let ((scheduler, _), pool) = scheduler_on(&Topology::from_perflevels(&[(1, 1), (2, 2)]));
        let built: Work = Arc::new(|| Ok(Did::built()));
        let opened = scheduler.open(1, false);
        let chain = (0..5u32)
            .map(|id| {
                let deps: Vec<u32> = (id > 0).then_some(59 + id).into_iter().collect();
                task(60 + id, &deps, None, built.clone())
            })
            .collect();
        scheduler.add(opened.request, chain);
        scheduler.close(opened.request);
        assert!(finish(&opened).0);
        assert_eq!(pool.critical_submitted(), 5);
    }

    /// Every task's work runs in its thread's task arena, under `drive` and the scheduler both.
    #[test]
    fn work_runs_in_a_task_arena() {
        let in_arena = || arena::with(|bump| *bump.alloc(7u64).unwrap()) == 7;

        let mut machine = Machine::new(Config { io: 1, work: 2, keep_going: false }).unwrap();
        let nodes = vec![spec(0, &[], Pool::Io, None), spec(1, &[0], Pool::Work, None)];
        let work = |_: NodeId| if in_arena() { Did::built() } else { Did::failed("no arena") };
        assert!(drive(&mut machine, nodes, &work, &mut |_| {}, None).unwrap());

        let (scheduler, _) = scheduler(2);
        let work: Work =
            Arc::new(move || Ok(if in_arena() { Did::built() } else { Did::failed("no arena") }));
        let opened = scheduler.open(1, false);
        let io = Task { spec: spec(71, &[], Pool::Io, None), kind: TaskKind(1), work: work.clone() };
        scheduler.add(opened.request, vec![io, task(72, &[71], None, work)]);
        scheduler.close(opened.request);
        assert!(finish(&opened).0);
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
    fn drive_runs_work_and_io_on_pools_apart_and_keeps_them_for_the_next_build() {
        // Sizes no other test drives, so no other build can borrow these pools in between.
        let build = || {
            let ran: Mutex<Vec<(u32, ThreadId)>> = Mutex::new(Vec::new());
            let mut machine = Machine::new(Config { io: 3, work: 7, keep_going: false }).unwrap();
            let nodes = vec![
                spec(0, &[], Pool::Io, None),
                spec(1, &[0], Pool::Work, None),
                spec(2, &[0], Pool::Work, None),
                spec(3, &[1, 2], Pool::Io, None),
            ];
            let work = |id: NodeId| {
                assert_eq!(group(), Some(0), "node {id} runs on a pool");
                ran.lock().unwrap().push((id.0, std::thread::current().id()));
                Did::built()
            };
            assert!(drive(&mut machine, nodes, &work, &mut |_| {}, None).unwrap());
            let ran = ran.into_inner().unwrap();
            let on = |nodes: &[u32]| -> BTreeSet<ThreadId> {
                ran.iter().filter(|(id, _)| nodes.contains(id)).map(|&(_, at)| at).collect()
            };
            assert_eq!(ran.len(), 4);
            assert!(on(&[0, 3]).is_disjoint(&on(&[1, 2])), "IO and work share a thread");
        };
        let spare = || SPARE.lock().iter().filter(|it| [3, 7].contains(&it.workers())).count();
        // A build that made its pools rather than borrowing the last one's would leave more.
        for _ in 0..4 {
            build();
            assert_eq!(spare(), 2, "the build's two pools are kept for the next");
        }
    }

    /// Two requests of weights 1 and 3 ask for the same ten keys: each key runs once, joined
    /// while it runs or read from the database once done, and both finish. Finished requests
    /// are released: a third takes their node ids again, and the database settles all of it.
    #[test]
    fn shared_keys_run_once_for_two_requests() {
        let (scheduler, db) = scheduler(4);
        let runs: Arc<Mutex<FxHashMap<u8, u32>>> = Arc::default();
        let keyed = |id: u32, key: u8| {
            let (runs, db) = (runs.clone(), db.clone());
            let work: Work = Arc::new(move || {
                *runs.lock().unwrap().entry(key).or_default() += 1;
                std::thread::sleep(Duration::from_millis(1));
                db.lock().unwrap().insert(Key([key; 32]), Outcome::Built);
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
        {
            let runs = runs.lock().unwrap();
            assert_eq!(runs.len(), 10);
            assert!(runs.values().all(|&it| it == 1), "{runs:?}");
        }

        let again = scheduler.open(1, false);
        scheduler.add(again.request, (0..10).map(|key| keyed(u32::from(key), key)).collect());
        scheduler.close(again.request);
        let (ok, records) = finish(&again);
        assert!(ok);
        let cached = records
            .iter()
            .filter(|it| matches!(it.event, Event::Moved { state: NodeState::Cached { .. }, .. }))
            .count();
        assert_eq!(cached, 10);
        assert!(runs.lock().unwrap().values().all(|&it| it == 1), "the database settled it");
    }

    /// A request cancelled while its first task runs: what waits for it never runs, the task
    /// runs to its end, and the request finishes not ok.
    #[test]
    fn a_cancelled_request_finishes_not_ok_and_its_running_work_completes() {
        let (scheduler, _) = scheduler(2);
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

    /// A thousand runs, no more than the pool's two workers under way at once and an IO task
    /// beside each round, take one chunk of the run table: each slot is reused once its `Done`
    /// is in.
    #[test]
    fn runs_reuse_their_slots() {
        let (scheduler, _) = scheduler(2);
        let built: Work = Arc::new(|| Ok(Did::built()));
        for round in 0..10u32 {
            let opened = scheduler.open(1, false);
            let first = 2000 + round * 101;
            let mut tasks: Vec<Task> =
                (0..100).map(|at| task(first + at, &[], None, built.clone())).collect();
            let io = spec(first + 100, &[], Pool::Io, None);
            tasks.push(Task { spec: io, kind: TaskKind(1), work: built.clone() });
            scheduler.add(opened.request, tasks);
            scheduler.close(opened.request);
            assert!(finish(&opened).0);
        }
        assert_eq!(scheduler.handle().slots(), RUNS);
    }

    /// Dropping a scheduler waits for the task under way, which reads its slot, and the task
    /// ends.
    #[test]
    fn dropping_the_scheduler_waits_for_its_runs() {
        let (scheduler, _) = scheduler(2);
        let (started_tx, started_rx) = bounded::<()>(1);
        let (gate_tx, gate_rx) = bounded::<()>(0);
        let ended = Arc::new(AtomicBool::new(false));
        let work: Work = {
            let ended = ended.clone();
            Arc::new(move || {
                _ = started_tx.send(());
                gate_rx.recv_timeout(LONG).expect("the gate opens");
                ended.store(true, Ordering::SeqCst);
                Ok(Did::built())
            })
        };
        let opened = scheduler.open(1, false);
        scheduler.add(opened.request, vec![task(90, &[], None, work)]);
        scheduler.close(opened.request);
        started_rx.recv_timeout(LONG).unwrap();
        let dropped = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let dropper = scope.spawn(|| {
                drop(scheduler);
                dropped.store(true, Ordering::SeqCst);
            });
            std::thread::sleep(Duration::from_millis(50));
            assert!(!dropped.load(Ordering::SeqCst), "the drop waits for the run under way");
            gate_tx.send_timeout((), LONG).unwrap();
            dropper.join().unwrap();
        });
        assert!(ended.load(Ordering::SeqCst) && dropped.load(Ordering::SeqCst));
    }

    #[test]
    fn work_that_gives_way_twice_is_posted_again_and_done() {
        let (scheduler, _) = scheduler(2);
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
        // Measured once: the tries that gave way taught nothing.
        let estimates = Estimates::decode(&scheduler.estimates()).unwrap();
        assert_eq!(estimates.by_kind.get(&TaskKind(1)).map(|it| it.seen), Some(1));
    }

    /// Nodes a task finds, their work given through a handle, run after it once the request is
    /// closed; a found node with no work fails the task that found it.
    #[test]
    fn a_task_gives_the_work_of_what_it_finds() {
        let (scheduler, _) = scheduler(2);
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
            let added = handle.found(request, vec![task(21, &[20], None, found)]);
            Ok(Did { outcome: Outcome::Built, added })
        });
        scheduler.add(request, vec![task(20, &[], None, finder)]);
        scheduler.close(request);
        assert!(finish(&opened).0);
        assert!(found_ran.load(Ordering::SeqCst));

        let opened = scheduler.open(1, true);
        let workless: Work = Arc::new(|| {
            Ok(Did { outcome: Outcome::Built, added: vec![spec(41, &[40], Pool::Work, None)] })
        });
        scheduler.add(opened.request, vec![task(40, &[], None, workless)]);
        scheduler.close(opened.request);
        let (ok, records) = finish(&opened);
        assert!(!ok);
        assert!(records.iter().any(|it| matches!(
            &it.event,
            Event::Moved { id: 40, state: NodeState::Failed { why } } if why.contains("no work")
        )));
    }

    #[test]
    fn a_refused_input_is_heard_by_its_opener() {
        let (scheduler, _) = scheduler(1);
        let opened = scheduler.open(0, false);
        assert!(matches!(opened.heard.recv_timeout(LONG), Ok(Heard::Refused(_))));
        let opened = scheduler.open(1, false);
        let built: Work = Arc::new(|| Ok(Did::built()));
        scheduler.add(opened.request, vec![task(30, &[31], None, built)]);
        assert!(matches!(opened.heard.recv_timeout(LONG), Ok(Heard::Refused(_))));
    }
}

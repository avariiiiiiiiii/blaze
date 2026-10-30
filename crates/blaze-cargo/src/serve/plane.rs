//! The plane (docs/v2/plan/lanes/core/plane.md) as the daemon's loop steps it: every job's
//! tasks are nodes of one [`blaze_graph::Plane`], with single flight, fair share, critical path
//! and memory admission. The loop owns the plane and steps it with the driver's clock, so a run
//! under the simulation driver is a function of its seed.
//!
//! A node's task runs where its [`Task`] says: on a thread ([`Task::Thread`]: the process's
//! worker pool for [`Pool::Work`], an IO thread for [`Pool::Io`]) through a [`Runner`], or on
//! the loop itself ([`Task::Loop`]: user code, which runs as the driver's spawns). What each
//! thread task held at most is measured ([`arena::measure`]) and teaches the [`Estimates`] the
//! next task of its kind is admitted by.

use std::{
    collections::{BTreeMap, VecDeque},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::Arc,
    time::Duration,
};

use blaze_core::sim::Rng;
use blaze_graph::{
    Budget, Error, Plane as Graph, PlaneConfig, PlaneInput, PlaneOutput, RequestId, RequestStats,
    admission::{Estimates, TaskKind},
    arena,
    machine::{NodeId, Outcome, Pool, Spec},
};
use blaze_protocol::events::{Event, NodeState};
use crossbeam_channel::{Receiver, Sender, unbounded};

/// The kinds of the daemon's own tasks, for their estimates: past every query kind (`u16`).
pub mod kinds {
    use blaze_graph::admission::TaskKind;

    /// The load's `Plan`: its query kind.
    pub const PLAN: TaskKind = TaskKind(0x6000);
    pub const FETCH: TaskKind = TaskKind(0x1_0001);
    pub const LIBRARY: TaskKind = TaskKind(0x1_0002);
    pub const LOWER: TaskKind = TaskKind(0x1_0003);
    pub const COMPILE: TaskKind = TaskKind(0x1_0004);
    pub const REACH: TaskKind = TaskKind(0x1_0005);
    /// A view's queries walked for `Trace`, `Verify` or `CodegenInputs`.
    pub const WALK: TaskKind = TaskKind(0x1_0006);
}

/// What a thread task came to: how the plane settles its node, and its value for the loop.
pub struct Did<T> {
    pub outcome: Outcome,
    pub value: T,
}

/// The thread tasks of one batch of nodes: called with a task's `arg`, it says what the task
/// came to, or `None` when it gave way (a cycle of waits, a cancel, memory), to be posted again
/// whole. One per batch, never one per node.
pub type Work<T> = Arc<dyn Fn(u32) -> Option<Did<T>> + Send + Sync>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Task {
    /// The batch's [`Work`] called with `arg`.
    Thread { kind: TaskKind, arg: u32 },
    /// Run by the loop once the plane says so ([`Happened::Run`]); it ends with
    /// [`Plane::ended`].
    Loop,
    /// Settled, or never added.
    Gone,
}

const _: () = assert!(std::mem::size_of::<Task>() == 12);

pub struct Node {
    pub spec: Spec,
    pub task: Task,
}

/// What the plane did, for the loop to act on, in order.
pub enum Happened<T> {
    /// A thread task's value, before its node settles.
    Did { id: NodeId, value: T },
    /// A thread task was posted: it starts.
    Started { id: NodeId },
    /// A loop task to start now.
    Run { id: NodeId },
    /// A node settled: done (`why` none) or not (failed, skipped or cancelled, saying why).
    Settled { id: NodeId, ok: bool, why: Option<String> },
    /// Every node of the request settled and it was closed; what it did, taken before the
    /// plane let it go.
    Finished { request: RequestId, ok: bool, stats: RequestStats },
}

/// A thread task that ended: its value, or its panic's message.
pub struct Ran<T> {
    pub id: NodeId,
    pub kind: TaskKind,
    pub did: Result<Did<T>, String>,
    pub peak: u64,
}

/// Runs thread tasks and hands back how each ended.
pub trait Runner<T>: Send {
    /// `work` is the batch's, shared: posting costs one reference count, not a closure.
    fn post(&mut self, id: NodeId, pool: Pool, near: Option<u32>, kind: TaskKind, work: Work<T>, arg: u32);
    /// Moves the tasks that ended into `out`.
    fn take(&mut self, out: &mut Vec<Ran<T>>);
    /// Whether a task ended and was not taken yet: the loop then polls the driver without
    /// waiting.
    fn ready(&self) -> bool;
    /// How many tasks were posted and not taken.
    fn running(&self) -> usize;
}

/// The daemon's: work tasks on the process's worker pool, IO tasks on threads of their own.
pub struct Threads<T> {
    done_tx: Sender<Ran<T>>,
    done: Receiver<Ran<T>>,
    io: Sender<IoJob<T>>,
    running: usize,
}

struct IoJob<T> {
    id: NodeId,
    kind: TaskKind,
    work: Work<T>,
    arg: u32,
    done: Sender<Ran<T>>,
}

impl<T: Send + 'static> Threads<T> {
    /// `io` threads for IO tasks.
    ///
    /// # Panics
    ///
    /// When a thread cannot be started, as [`std::thread::spawn`].
    pub fn new(io: usize) -> Threads<T> {
        let (done_tx, done) = unbounded();
        let (io_tx, io_rx) = unbounded::<IoJob<T>>();
        for at in 0..io.max(1) {
            let io_rx = io_rx.clone();
            std::thread::Builder::new()
                .name(format!("blaze-serve-io-{at}"))
                .spawn(move || {
                    for job in io_rx {
                        // A task that gave way runs again on this thread: an IO task waits on
                        // the world, not on another task.
                        let (did, peak) = loop {
                            if let (Some(did), peak) = run_measured(&job.work, job.arg) {
                                break (did, peak);
                            }
                        };
                        _ = job.done.send(Ran { id: job.id, kind: job.kind, did, peak });
                    }
                })
                .unwrap_or_else(|why| panic!("an IO thread of the daemon's plane: {why}"));
        }
        Threads { done_tx, done, io: io_tx, running: 0 }
    }
}

/// Runs `work` inside a measure: `None` when it gave way, else its value or its panic.
fn run_measured<T>(work: &Work<T>, arg: u32) -> (Option<Result<Did<T>, String>>, u64) {
    let (result, peak) = arena::measure(|| catch_unwind(AssertUnwindSafe(|| work(arg))));
    match result {
        Ok(None) => (None, peak),
        Ok(Some(did)) => (Some(Ok(did)), peak),
        Err(panic) => (Some(Err(panicked(&*panic))), peak),
    }
}

fn panicked(panic: &(dyn std::any::Any + Send)) -> String {
    let what = panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("a panic with no message");
    format!("panicked: {what}")
}

fn post_work<T: Send + 'static>(
    id: NodeId,
    near: Option<u32>,
    kind: TaskKind,
    work: Work<T>,
    arg: u32,
    done: Sender<Ran<T>>,
) {
    let pool = blaze_graph::exec::shared();
    let again = work.clone();
    pool.submit(
        near,
        move || run_measured(&work, arg),
        move |result| match result {
            Ok((Some(did), peak)) => _ = done.send(Ran { id, kind, did, peak }),
            // Gave way: posted again behind what is ready on the group it ran on.
            Ok((None, _)) => post_work(id, blaze_graph::pool::group(), kind, again, arg, done),
            Err(panic) => _ = done.send(Ran { id, kind, did: Err(panicked(&*panic)), peak: 0 }),
        },
    );
}

impl<T: Send + 'static> Runner<T> for Threads<T> {
    fn post(&mut self, id: NodeId, pool: Pool, near: Option<u32>, kind: TaskKind, work: Work<T>, arg: u32) {
        self.running += 1;
        match pool {
            Pool::Work => post_work(id, near, kind, work, arg, self.done_tx.clone()),
            Pool::Io => {
                let job = IoJob { id, kind, work, arg, done: self.done_tx.clone() };
                // The IO threads hold the receiver for as long as the process lives.
                _ = self.io.send(job);
            }
        }
    }

    fn take(&mut self, out: &mut Vec<Ran<T>>) {
        while let Ok(ran) = self.done.try_recv() {
            self.running -= 1;
            out.push(ran);
        }
    }

    fn ready(&self) -> bool {
        !self.done.is_empty()
    }

    fn running(&self) -> usize {
        self.running
    }
}

/// The simulation's: tasks run on the loop's thread when it takes them, a seeded part of what
/// is posted each time in a seeded order, so a run is a function of its seed.
pub struct Inline<T> {
    rng: Rng,
    queue: VecDeque<Posted<T>>,
    /// Reused by every take.
    posted: Vec<Posted<T>>,
}

type Posted<T> = (NodeId, TaskKind, Work<T>, u32);

impl<T> Inline<T> {
    pub fn new(seed: u64) -> Inline<T> {
        Inline { rng: Rng::new(seed), queue: VecDeque::new(), posted: Vec::new() }
    }
}

impl<T: Send + 'static> Runner<T> for Inline<T> {
    fn post(&mut self, id: NodeId, _: Pool, _: Option<u32>, kind: TaskKind, work: Work<T>, arg: u32) {
        self.queue.push_back((id, kind, work, arg));
    }

    fn take(&mut self, out: &mut Vec<Ran<T>>) {
        if self.queue.is_empty() {
            return;
        }
        let mut posted = std::mem::take(&mut self.posted);
        posted.extend(self.queue.drain(..));
        self.rng.shuffle(&mut posted);
        let now = 1 + self.rng.below(posted.len() as u64) as usize;
        self.queue.extend(posted.drain(now..));
        for (id, kind, work, arg) in posted.drain(..) {
            match run_measured(&work, arg) {
                (Some(did), peak) => out.push(Ran { id, kind, did, peak }),
                (None, _) => self.queue.push_back((id, kind, work, arg)),
            }
        }
        self.posted = posted;
    }

    fn ready(&self) -> bool {
        !self.queue.is_empty()
    }

    fn running(&self) -> usize {
        self.queue.len()
    }
}

/// Nodes added together, ids `first..first + tasks.len()` (reserved at once, [`Plane::reserve`]):
/// each one's task by offset, 12 bytes a node, and the one work their thread tasks share. Let
/// go once every node of it settled.
struct Batch<T> {
    tasks: Vec<Task>,
    live: u32,
    work: Option<Work<T>>,
}

pub struct Plane<T> {
    graph: Graph,
    estimates: Estimates,
    runner: Box<dyn Runner<T>>,
    /// By first id: a node's batch is the last one starting at or below it, O(log batches).
    batches: BTreeMap<u32, Batch<T>>,
    next_node: u32,
    next_request: u32,
    happened: Vec<Happened<T>>,
    ran: Vec<Ran<T>>,
}

fn ns(at: Duration) -> u64 {
    u64::try_from(at.as_nanos()).unwrap_or(u64::MAX)
}

impl<T: Send + 'static> Plane<T> {
    pub fn new(config: PlaneConfig, estimates: Estimates, runner: Box<dyn Runner<T>>) -> Result<Plane<T>, Error> {
        Ok(Plane {
            graph: Graph::new(config)?,
            estimates,
            runner,
            batches: BTreeMap::new(),
            next_node: 0,
            next_request: 0,
            happened: Vec::new(),
            ran: Vec::new(),
        })
    }

    pub fn budget(&self) -> Option<&Budget> {
        self.graph.config().budget.as_ref()
    }

    pub fn estimates(&self) -> &Estimates {
        &self.estimates
    }

    pub fn stats(&self, request: RequestId) -> Option<RequestStats> {
        self.graph.stats(request)
    }

    /// The first of `count` consecutive node ids no node of this plane has, for one
    /// [`Plane::add`].
    ///
    /// # Panics
    ///
    /// When the ids wrapped around onto a batch that has not settled, after 2^32 nodes.
    pub fn reserve(&mut self, count: u32) -> NodeId {
        let first = match self.next_node.checked_add(count) {
            Some(_) => self.next_node,
            None => 0,
        };
        let end = first + count;
        let free = self.batches.range(..end).next_back().is_none_or(|(at, batch)| at + batch.tasks.len() as u32 <= first);
        assert!(free, "the plane's node ids wrapped onto nodes that have not settled");
        self.next_node = end;
        NodeId(first)
    }

    fn task(&self, id: NodeId) -> Option<(&Batch<T>, Task)> {
        let (first, batch) = self.batches.range(..=id.0).next_back()?;
        batch.tasks.get((id.0 - first) as usize).map(|task| (batch, *task))
    }

    fn settled(&mut self, id: NodeId) {
        let Some((&first, batch)) = self.batches.range_mut(..=id.0).next_back() else { return };
        let Some(task) = batch.tasks.get_mut((id.0 - first) as usize) else { return };
        if *task != Task::Gone {
            *task = Task::Gone;
            batch.live -= 1;
            if batch.live == 0 {
                self.batches.remove(&first);
            }
        }
    }

    /// A request, of weight 1: every job's share of the threads is the same.
    pub fn open(&mut self, at: Duration, keep_going: bool) -> RequestId {
        let request = RequestId(self.next_request);
        self.next_request = self.next_request.wrapping_add(1);
        let input = PlaneInput::Open { at_ns: ns(at), request, weight: 1, keep_going };
        self.step(input).unwrap_or_else(|error| panic!("the plane refused to open {request}: {error}"));
        request
    }

    /// Nodes of `request` with the ids [`Plane::reserve`] gave from `first`, in order, their
    /// thread tasks run by `work`; each thread task's memory raised to what its kind is
    /// predicted to hold. An error (a cycle, a dep of another request) adds none of them.
    ///
    /// # Panics
    ///
    /// When a node's id is not `first` plus its place, or a thread task has no `work`.
    pub fn add(
        &mut self,
        at: Duration,
        request: RequestId,
        first: NodeId,
        nodes: Vec<Node>,
        work: Option<Work<T>>,
    ) -> Result<(), Error> {
        let mut specs = Vec::with_capacity(nodes.len());
        let mut tasks = Vec::with_capacity(nodes.len());
        for (offset, Node { mut spec, task }) in (0u32..).zip(nodes) {
            assert_eq!(spec.id.0, first.0 + offset, "a batch's nodes are the ids reserved for it, in order");
            if let Task::Thread { kind, .. } = task {
                assert!(work.is_some(), "a batch with thread tasks is given their work");
                spec.memory = spec.memory.max(self.estimates.predict(kind));
            }
            tasks.push(task);
            specs.push(spec);
        }
        if !tasks.is_empty() {
            let live = tasks.len() as u32;
            self.batches.insert(first.0, Batch { tasks, live, work });
        }
        match self.step(PlaneInput::Add { at_ns: ns(at), request, nodes: specs }) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.batches.remove(&first.0);
                Err(error)
            }
        }
    }

    /// No more nodes are added to `request`.
    pub fn close(&mut self, at: Duration, request: RequestId) {
        self.step(PlaneInput::Close { at_ns: ns(at), request })
            .unwrap_or_else(|error| panic!("the plane refused to close {request}: {error}"));
    }

    /// Skips what of `request` has not started; it finishes not ok once what runs is done.
    pub fn cancel(&mut self, at: Duration, request: RequestId) {
        let outputs = self
            .graph
            .cancel(ns(at), request)
            .unwrap_or_else(|error| panic!("the plane refused to cancel {request}: {error}"));
        self.outputs(outputs);
    }

    /// A loop task is over.
    pub fn ended(&mut self, at: Duration, id: NodeId, outcome: Outcome) {
        let input = PlaneInput::Done { at_ns: ns(at), id, outcome, added: Vec::new(), group: None, peak: 0 };
        self.step(input).unwrap_or_else(|error| panic!("the plane refused the end of loop task {id}: {error}"));
    }

    /// Takes the thread tasks that ended into the plane.
    pub fn poll(&mut self, at: Duration) {
        let mut ran = std::mem::take(&mut self.ran);
        self.runner.take(&mut ran);
        for Ran { id, kind, did, peak } in ran.drain(..) {
            self.estimates.observe(kind, peak);
            let outcome = match did {
                Ok(Did { outcome, value }) => {
                    self.happened.push(Happened::Did { id, value });
                    outcome
                }
                Err(why) => Outcome::Failed { why },
            };
            let input = PlaneInput::Done { at_ns: ns(at), id, outcome, added: Vec::new(), group: None, peak };
            self.step(input).unwrap_or_else(|error| panic!("the plane refused the end of task {id}: {error}"));
        }
        self.ran = ran;
        let held = PlaneInput::Held { at_ns: ns(at), bytes: arena::held() };
        self.step(held).unwrap_or_else(|error| panic!("the plane refused what is held: {error}"));
    }

    /// What happened since the last call, in order, swapped into `into`, which is empty: the
    /// two buffers trade places each call and neither is allocated again.
    pub fn happened(&mut self, into: &mut Vec<Happened<T>>) {
        debug_assert!(into.is_empty(), "what happened before was taken");
        std::mem::swap(&mut self.happened, into);
    }

    /// Whether a thread task ended and waits to be taken.
    pub fn ready(&self) -> bool {
        self.runner.ready()
    }

    /// How many thread tasks run.
    pub fn running(&self) -> usize {
        self.runner.running()
    }

    fn step(&mut self, input: PlaneInput) -> Result<(), Error> {
        let outputs = self.graph.step(input)?;
        self.outputs(outputs);
        Ok(())
    }

    fn outputs(&mut self, outputs: Vec<PlaneOutput>) {
        for output in outputs {
            match output {
                PlaneOutput::Said { record, .. } => {
                    let Event::Moved { id, state } = record.event else { continue };
                    if !state.settled() {
                        continue;
                    }
                    let id = NodeId(id);
                    self.settled(id);
                    let (ok, why) = match state {
                        NodeState::Built | NodeState::Cached { .. } => (true, None),
                        NodeState::Failed { why } => (false, Some(why)),
                        NodeState::Skipped { because } => {
                            (false, Some(format!("skipped: node {because} it waited for did not end well")))
                        }
                        _ => (false, Some("cancelled".to_owned())),
                    };
                    self.happened.push(Happened::Settled { id, ok, why });
                }
                PlaneOutput::Run { id, pool, near } => match self.task(id) {
                    Some((batch, Task::Thread { kind, arg })) => {
                        let work = batch.work.clone().expect("checked by add");
                        self.happened.push(Happened::Started { id });
                        self.runner.post(id, pool, near, kind, work, arg);
                    }
                    Some((_, Task::Loop)) => self.happened.push(Happened::Run { id }),
                    Some((_, Task::Gone)) | None => panic!("the plane runs node {id}, which the daemon gave no task"),
                },
                PlaneOutput::Finished { request, ok } => {
                    let stats = self.graph.stats(request).unwrap_or_default();
                    self.graph
                        .release(request)
                        .unwrap_or_else(|error| panic!("the plane refused to release {request}: {error}"));
                    self.happened.push(Happened::Finished { request, ok, stats });
                }
            }
        }
    }
}

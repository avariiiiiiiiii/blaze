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

#![forbid(unsafe_code)]

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
    machine::{NodeId, Outcome, Pool},
    plane::{NodeRef, Nodes},
};
use blaze_protocol::events::{Event, NodeState};
use crossbeam_channel::{Receiver, Sender, unbounded};

use super::work::Readers;

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
    /// `blaze test --ub`: the tests selected, and one test evaluated.
    pub const UB_TESTS: TaskKind = TaskKind(0x1_0007);
    pub const EVALUATE: TaskKind = TaskKind(0x1_0008);
}

/// What a thread task came to: how the plane settles its node, and its value for the loop.
pub struct Did<T> {
    pub outcome: Outcome,
    pub value: T,
}

/// The thread tasks of one batch of nodes: called with a task's `arg` and the readers the loop
/// posted it with, it says what the task came to, or `None` when it gave way (a cycle of waits,
/// a cancel, memory, a write of what it read), to be posted again whole. One per batch, never
/// one per node.
pub type Work<T> = Arc<dyn Fn(u32, Readers) -> Option<Did<T>> + Send + Sync>;

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

/// A batch of nodes being made ([`Plane::fresh`]): the ids reserved for it, the plane's
/// columns of it, and each node's task, filled by [`Plane::push`] and added whole by
/// [`Plane::add`].
pub struct Fresh {
    nodes: Nodes,
    tasks: Vec<Task>,
    count: u32,
}

impl Fresh {
    pub fn first(&self) -> NodeId {
        self.nodes.first()
    }

    pub fn count(&self) -> u32 {
        self.count
    }
}

/// What the plane did, for the loop to act on, in order.
pub enum Happened<T> {
    /// A thread task's value, before its node settles.
    Did { id: NodeId, value: T },
    /// A thread task was posted: it starts.
    Started { id: NodeId },
    /// A thread task of a batch that reads to post now, `again` when it gave way before:
    /// the loop makes its readers and gives them to [`Plane::post`].
    Post { task: Posting, again: bool },
    /// A loop task to start now.
    Run { id: NodeId },
    /// A node settled: done (`why` none) or not (failed, skipped or cancelled, saying why).
    Settled { id: NodeId, ok: bool, why: Option<String> },
    /// Every node of the request settled and it was closed; what it did, taken before the
    /// plane let it go.
    Finished { request: RequestId, ok: bool, stats: RequestStats },
}

/// A thread task that ended, or gave way.
pub struct Ran<T> {
    pub task: Posting,
    pub ended: Ended<T>,
    pub peak: u64,
}

pub enum Ended<T> {
    Did(Did<T>),
    Panicked(String),
    /// A task of a batch that reads gave way, on the thread group `group`: the loop posts it
    /// again with new readers. Others are posted again where they ran.
    GaveWay { group: Option<u32> },
}

/// Runs thread tasks and hands back how each ended.
pub trait Runner<T>: Send {
    /// `work` is the batch's, shared: posting costs one reference count, not a closure. A
    /// `critical` task is on its request's critical path: it goes to the fastest cores.
    fn post(&mut self, task: Posting, work: Work<T>, readers: Readers);
    /// Moves the tasks that ended into `out`.
    fn take(&mut self, out: &mut Vec<Ran<T>>);
    /// Whether a task ended and was not taken yet: the loop then polls the driver without
    /// waiting.
    fn ready(&self) -> bool;
    /// How many tasks were posted and not taken.
    fn running(&self) -> usize;
}

/// A thread task as the plane posts it.
#[derive(Clone, Copy, Debug)]
pub struct Posting {
    pub id: NodeId,
    pub pool: Pool,
    pub near: Option<u32>,
    pub critical: bool,
    pub kind: TaskKind,
    pub arg: u32,
    /// Its batch reads the front end: it is posted with readers the loop made.
    pub reads: bool,
}

/// The daemon's: work tasks on the process's worker pool, IO tasks on threads of their own.
pub struct Threads<T> {
    done_tx: Sender<Ran<T>>,
    done: Receiver<Ran<T>>,
    io: Sender<IoJob<T>>,
    running: usize,
}

struct IoJob<T> {
    task: Posting,
    work: Work<T>,
    readers: Readers,
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
                        // the world, not on another task. One that reads goes back for new
                        // readers.
                        let mut readers = Some(job.readers);
                        let (ended, peak) = loop {
                            match run_measured(&job.work, job.task.arg, readers.take().unwrap_or_default()) {
                                (Some(ended), peak) => break (ended, peak),
                                (None, peak) if job.task.reads => break (Ended::GaveWay { group: None }, peak),
                                (None, _) => {}
                            }
                        };
                        _ = job.done.send(Ran { task: job.task, ended, peak });
                    }
                })
                .unwrap_or_else(|why| panic!("an IO thread of the daemon's plane: {why}"));
        }
        Threads { done_tx, done, io: io_tx, running: 0 }
    }
}

/// Runs `work` inside a measure: `None` when it gave way (its readers dropped with it), else
/// its value or its panic.
fn run_measured<T>(work: &Work<T>, arg: u32, readers: Readers) -> (Option<Ended<T>>, u64) {
    let (result, peak) = arena::measure(|| catch_unwind(AssertUnwindSafe(|| work(arg, readers))));
    match result {
        Ok(None) => (None, peak),
        Ok(Some(did)) => (Some(Ended::Did(did)), peak),
        Err(panic) => (Some(Ended::Panicked(panicked(&*panic))), peak),
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

fn post_work<T: Send + 'static>(task: Posting, work: Work<T>, readers: Readers, done: Sender<Ran<T>>) {
    let pool = blaze_graph::exec::shared();
    let again = work.clone();
    let run = move || run_measured(&work, task.arg, readers);
    let then = move |result: std::thread::Result<_>| match result {
        Ok((Some(ended), peak)) => _ = done.send(Ran { task, ended, peak }),
        Ok((None, peak)) if task.reads => {
            _ = done.send(Ran { task, ended: Ended::GaveWay { group: blaze_graph::pool::group() }, peak });
        }
        // Gave way: posted again behind what is ready on the group it ran on.
        Ok((None, _)) => {
            post_work(Posting { near: blaze_graph::pool::group(), ..task }, again, Readers::default(), done);
        }
        Err(panic) => _ = done.send(Ran { task, ended: Ended::Panicked(panicked(&*panic)), peak: 0 }),
    };
    match task.critical {
        true => pool.submit_critical(task.near, run, then),
        false => pool.submit(task.near, run, then),
    }
}

impl<T: Send + 'static> Runner<T> for Threads<T> {
    fn post(&mut self, task: Posting, work: Work<T>, readers: Readers) {
        self.running += 1;
        match task.pool {
            Pool::Work => post_work(task, work, readers, self.done_tx.clone()),
            Pool::Io => {
                let job = IoJob { task, work, readers, done: self.done_tx.clone() };
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
/// is posted each time in a seeded order, so a run is a function of its seed. The readers of
/// tasks it holds queued are alive while the loop writes, so it is for fronts whose writes
/// don't wait on their readers (the simulation's fakes).
pub struct Inline<T> {
    rng: Rng,
    queue: VecDeque<Posted<T>>,
    /// Reused by every take.
    posted: Vec<Posted<T>>,
}

type Posted<T> = (Posting, Work<T>, Readers);

impl<T> Inline<T> {
    pub fn new(seed: u64) -> Inline<T> {
        Inline { rng: Rng::new(seed), queue: VecDeque::new(), posted: Vec::new() }
    }
}

impl<T: Send + 'static> Runner<T> for Inline<T> {
    fn post(&mut self, task: Posting, work: Work<T>, readers: Readers) {
        self.queue.push_back((task, work, readers));
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
        for (task, work, readers) in posted.drain(..) {
            match run_measured(&work, task.arg, readers) {
                (Some(ended), peak) => out.push(Ran { task, ended, peak }),
                (None, peak) if task.reads => out.push(Ran { task, ended: Ended::GaveWay { group: None }, peak }),
                (None, _) => self.queue.push_back((task, work, Readers::default())),
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

/// Nodes added together, ids `first..first + tasks.len()` (reserved at once, [`Plane::fresh`]):
/// each one's task by offset, 12 bytes a node, and the one work their thread tasks share. Let
/// go once every node of it settled.
struct Batch<T> {
    tasks: Vec<Task>,
    live: u32,
    work: Option<Work<T>>,
    reads: bool,
}

pub struct Plane<T> {
    graph: Graph,
    estimates: Estimates,
    runner: Box<dyn Runner<T>>,
    /// By first id: a node's batch is the last one starting at or below it, O(log batches).
    // BTreeMap: range queries by node id.
    batches: BTreeMap<u32, Batch<T>>,
    next_node: u32,
    next_request: u32,
    happened: Vec<Happened<T>>,
    ran: Vec<Ran<T>>,
    said: Vec<PlaneOutput>,
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
            said: Vec::new(),
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

    /// A batch of `count` nodes with consecutive ids no node of this plane has, its columns
    /// sized for `name_bytes` of names and `deps` deps in all.
    ///
    /// # Panics
    ///
    /// When the ids wrapped around onto a batch that has not settled, after 2^32 nodes.
    pub fn fresh(&mut self, count: u32, name_bytes: usize, deps: usize) -> Fresh {
        let first = self.reserve(count);
        let nodes = Nodes::with_capacity(first, count as usize, name_bytes, deps);
        Fresh { nodes, tasks: Vec::with_capacity(count as usize), count }
    }

    /// The next node of `fresh`, its id; a thread task's memory raised to what its kind is
    /// predicted to hold.
    ///
    /// # Panics
    ///
    /// When `fresh` has all the nodes it was made for.
    pub fn push(&self, fresh: &mut Fresh, mut node: NodeRef<'_>, task: Task) -> NodeId {
        assert!((fresh.tasks.len() as u32) < fresh.count, "a batch of {} nodes given another", fresh.count);
        if let Task::Thread { kind, .. } = task {
            node.memory = node.memory.max(self.estimates.predict(kind));
        }
        fresh.tasks.push(task);
        fresh.nodes.push(node)
    }

    fn reserve(&mut self, count: u32) -> NodeId {
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

    /// The nodes of `fresh` added to `request`, in one input to the plane, their thread tasks
    /// run by `work`; with `reads`, each posted with readers the loop makes ([`Happened::Post`]).
    /// An error (a cycle, a dep of another request) adds none of them.
    ///
    /// # Panics
    ///
    /// When `fresh` has fewer nodes than it was made for, or a thread task has no `work`.
    pub fn add(
        &mut self,
        at: Duration,
        request: RequestId,
        fresh: Fresh,
        work: Option<Work<T>>,
        reads: bool,
    ) -> Result<(), Error> {
        let Fresh { nodes, tasks, count } = fresh;
        assert_eq!(tasks.len() as u32, count, "a batch is added with every node it was made for");
        assert!(
            work.is_some() || !tasks.iter().any(|it| matches!(it, Task::Thread { .. })),
            "a batch with thread tasks is given their work"
        );
        let first = nodes.first();
        if count > 0 {
            self.batches.insert(first.0, Batch { tasks, live: count, work, reads });
        }
        match self.step(PlaneInput::AddNodes { at_ns: ns(at), request, nodes }) {
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
        let mut said = std::mem::take(&mut self.said);
        self.graph
            .cancel_into(ns(at), request, &mut said)
            .unwrap_or_else(|error| panic!("the plane refused to cancel {request}: {error}"));
        self.outputs(&mut said);
        self.said = said;
    }

    /// A loop task is over.
    pub fn ended(&mut self, at: Duration, id: NodeId, outcome: Outcome) {
        let input = PlaneInput::Done { at_ns: ns(at), id, outcome, added: Vec::new(), group: None, peak: 0 };
        self.step(input).unwrap_or_else(|error| panic!("the plane refused the end of loop task {id}: {error}"));
    }

    /// A thread task of [`Happened::Post`] posted with the readers the loop made for it.
    pub fn post(&mut self, task: Posting, again: bool, readers: Readers) {
        let Some((batch, Task::Thread { .. })) = self.task(task.id) else {
            panic!("the daemon posts node {}, which is no thread task waiting", task.id)
        };
        let work = batch.work.clone().expect("checked by add");
        if !again {
            self.happened.push(Happened::Started { id: task.id });
        }
        self.runner.post(task, work, readers);
    }

    /// A thread task of [`Happened::Post`] the loop has no readers for: it ends failed.
    pub fn refuse(&mut self, at: Duration, task: Posting, why: String) {
        let input = PlaneInput::Done { at_ns: ns(at), id: task.id, outcome: Outcome::Failed { why }, added: Vec::new(), group: None, peak: 0 };
        self.step(input).unwrap_or_else(|error| panic!("the plane refused the end of task {}: {error}", task.id));
    }

    /// Takes the thread tasks that ended into the plane.
    pub fn poll(&mut self, at: Duration) {
        let mut ran = std::mem::take(&mut self.ran);
        self.runner.take(&mut ran);
        for Ran { task, ended, peak } in ran.drain(..) {
            let id = task.id;
            let outcome = match ended {
                Ended::GaveWay { group } => {
                    self.happened.push(Happened::Post { task: Posting { near: group, ..task }, again: true });
                    continue;
                }
                Ended::Did(Did { outcome, value }) => {
                    self.happened.push(Happened::Did { id, value });
                    outcome
                }
                Ended::Panicked(why) => Outcome::Failed { why },
            };
            self.estimates.observe(task.kind, peak);
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

    /// One input, its outputs taken through `said`, which is reused by every step.
    fn step(&mut self, input: PlaneInput) -> Result<(), Error> {
        let mut said = std::mem::take(&mut self.said);
        let taken = self.graph.step_into(input, &mut said);
        if taken.is_ok() {
            self.outputs(&mut said);
        }
        said.clear();
        self.said = said;
        taken
    }

    fn outputs(&mut self, outputs: &mut Vec<PlaneOutput>) {
        for output in outputs.drain(..) {
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
                PlaneOutput::Run { id, pool, near, critical } => match self.task(id) {
                    Some((batch, Task::Thread { kind, arg })) => {
                        let task = Posting { id, pool, near, critical, kind, arg, reads: batch.reads };
                        if batch.reads {
                            self.happened.push(Happened::Post { task, again: false });
                        } else {
                            let work = batch.work.clone().expect("checked by add");
                            self.happened.push(Happened::Started { id });
                            self.runner.post(task, work, Readers::default());
                        }
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

//! The plane (docs/v2/plan/lanes/core/plane.md, "The plane"): one scheduler for every request a daemon has under
//! way, as a state machine that does no IO.
//!
//! A request (a build, a test run, an edit) is opened with a weight, adds nodes, and is closed;
//! it finishes once each of its nodes is settled. A node may have a [`Key`], the hash of
//! everything its value depends on. The plane keeps no results (docs/v2/plan/lanes/core.md, "One memo, the
//! database's"): before it runs a node with a key it asks the [`Memo`], and a key the database
//! has settles the node at once, running nothing. A node whose key another node is running or
//! about to run joins it (**single flight**): one task runs, and every node of it settles when
//! it ends. A task not yet started is ready in the request of each of its nodes, at that node's
//! priority, and the first request to start it runs it.
//!
//! Of the requests with ready work, the next thread of a pool goes to the one that started the
//! fewest tasks of the pool for its weight (**fair share**, stride scheduling: each start moves
//! a request's virtual time on by the inverse of its weight, and the least virtual time goes
//! next; a request that had no ready work comes back at the plane's virtual time, not behind
//! it). Ties go to the heavier request, then to the one opened first. Within a request, the
//! ready task with the longest path of expected cost to the end of the request runs first
//! (**critical path**), ties to the node added first.
//!
//! With a [`Budget`], a task starts only when the predicted peaks of what runs (or what the
//! process is said to hold, if more), plus its own, fit in it (**memory admission**). Each time
//! a task with memory starts while another, ready in the same pool, does not fit, that one is
//! passed over; the first passed over [`PlaneConfig::starve_after`] times holds back every task
//! with memory, of every pool and request, until it starts, so a large task is never starved
//! ([`Plane::holding`]). A task whose prediction alone is more than the budget fails before it
//! starts, saying so; so does one that cannot fit while nothing runs. Nothing is retried with
//! less at once.
//!
//! A request may be cancelled ([`Plane::cancel`]): what of it has not started is skipped, what
//! runs is let finish, and it finishes failed. A plane lives as long as the daemon: a request
//! [`Plane::release`]d gives its nodes' room back once it finishes.
//!
//! **Layout** (docs/v2/plan/design/data.md). Nodes and tasks are `u32` slots into columns, a
//! hot row of what scheduling reads (64 bytes a node, 32 a task) beside colder ones, reused
//! through free lists. A request's edges are one flat array: a node's deps a range of it, the
//! nodes that wait for a node a list threaded through it. A task's members are a list threaded
//! through the node rows. A request's ready nodes of a pool are a 4-ary heap of 24-byte entries
//! that each node knows its place in, so a start, a raise and a leave are `O(log n)` with no
//! allocation. A key is its own hash: the flight table indexes by its first eight bytes. What a
//! step needs for a moment (a batch's order, the requests touched, the tasks passed over) is a
//! buffer kept on the plane, so in steady state a task costs the plane no allocation:
//! [`Plane::step_into`] appends to the caller's buffer of outputs.

#![forbid(unsafe_code)]

use std::{
    cmp::Reverse,
    collections::{BinaryHeap, HashMap, hash_map::Entry},
    fmt,
    hash::{BuildHasherDefault, Hash, Hasher},
};

use blaze_protocol::events::{Event, Level, NodeState, Record};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

use crate::machine::{Error, NodeId, Outcome, Pool, Spec};

/// A request's name, chosen by whoever opens it.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(transparent)]
pub struct RequestId(pub u32);

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "request {}", self.0)
    }
}

/// The hash of everything a node's value depends on: two nodes with one key are one piece of
/// work.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[serde(transparent)]
pub struct Key(pub [u8; 32]);

/// A key is already a content hash: its first eight bytes are its hash, for any hasher.
impl Hash for Key {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u64(self.prefix());
    }
}

impl Key {
    fn prefix(&self) -> u64 {
        let [a, b, c, d, e, f, g, h, ..] = self.0;
        u64::from_le_bytes([a, b, c, d, e, f, g, h])
    }
}

/// What the query database has of a key: the one memo of results (docs/v2/plan/lanes/core.md, "One memo,
/// the database's"). `Some` settles a node of the key at once, running nothing: `Built` as
/// cached on this machine, `Cached` as it says, `Failed` as failed. A task's work puts its
/// value in the database before its [`PlaneInput::Done`], so a node of its key that becomes
/// ready after finds it. Answers must be the same for the same database, or a trace does not
/// replay.
pub trait Memo {
    fn settled(&self, key: Key) -> Option<Outcome>;
}

impl<F: Fn(Key) -> Option<Outcome>> Memo for F {
    fn settled(&self, key: Key) -> Option<Outcome> {
        self(key)
    }
}

/// The memory the plane may grant, and what says so, for the error of a task that cannot fit.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Budget {
    pub bytes: u64,
    pub from: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PlaneConfig {
    /// How many tasks of [`Pool::Io`] run at once.
    pub io: usize,
    /// How many tasks of [`Pool::Work`] run at once: the cores.
    pub work: usize,
    pub budget: Option<Budget>,
    /// How many times a task may be passed over for memory before it holds back the tasks
    /// behind it.
    pub starve_after: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum PlaneInput {
    /// A request begins. Its `weight` is its share of the threads against the others'; with
    /// `keep_going` false its first failure skips what of it has not started.
    Open { at_ns: u64, request: RequestId, weight: u32, keep_going: bool },
    /// Nodes of the request, besides those it had. Their deps are nodes of the same request.
    Add { at_ns: u64, request: RequestId, nodes: Vec<Spec> },
    /// No more nodes are added to the request but by [`PlaneInput::Done`].
    Close { at_ns: u64, request: RequestId },
    /// The task the plane asked to run as `id` is done, on the thread group `group` where the
    /// executor has groups; its work found the nodes `added`, of `id`'s request. `peak` is the
    /// most its arena held, measured, for [`crate::admission::Estimates::observe`]; 0 when
    /// not measured.
    Done {
        at_ns: u64,
        id: NodeId,
        outcome: Outcome,
        added: Vec<Spec>,
        group: Option<u32>,
        #[serde(default)]
        peak: u64,
    },
    /// The bytes the process holds now, measured.
    Held { at_ns: u64, bytes: u64 },
}

impl PlaneInput {
    pub fn at_ns(&self) -> u64 {
        match self {
            PlaneInput::Open { at_ns, .. }
            | PlaneInput::Add { at_ns, .. }
            | PlaneInput::Close { at_ns, .. }
            | PlaneInput::Done { at_ns, .. }
            | PlaneInput::Held { at_ns, .. } => *at_ns,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum PlaneOutput {
    /// A record of a request: a node, a move, its critical path.
    Said { request: RequestId, record: Record },
    /// Run the work of node `id` on `pool`, near the thread group `near` if the executor has
    /// one, then give the plane a [`PlaneInput::Done`] of it. `critical`: its path of expected
    /// cost to the end of its request is the longest of the request's ready or running work,
    /// so a machine with fast and slow cores gives it a fast one.
    Run { id: NodeId, pool: Pool, near: Option<u32>, critical: bool },
    /// Every node of the request is settled. Nothing more comes of it.
    Finished { request: RequestId, ok: bool },
}

/// What the plane has done, for a person and for the tests.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    /// Tasks started.
    pub ran: u64,
    /// Nodes settled from the [`Memo`], running nothing.
    pub remembered: u64,
    /// Nodes that joined a task another node had.
    pub joined: u64,
    /// The most memory granted at once.
    pub granted_most: u64,
}

/// What a start moves a request's virtual time on by, over its weight: divisible by every
/// weight up to 16, so for those the shares are exact.
const SHARE: u128 = 720_720 << 32;

/// What one request did, for its proof of work ([`Plane::stats`]).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RequestStats {
    /// Its nodes that ran their task (whatever came of it, a cache hit of the work too).
    pub ran: u64,
    /// Its nodes settled by the [`Memo`], running nothing.
    pub cached: u64,
    /// Its nodes that settled with a task another node ran.
    pub joined: u64,
    /// The longest chain through its nodes, each after its deps, of measured run times: from
    /// the step that said a node's `Run` to the step of its `Done`. A cached or joined node's
    /// own time is 0 here; one skipped ends no chain.
    pub critical_path_ns: u64,
}

/// No node, task, edge or place: the end of a list, a node of no task or in no ready heap.
const NONE: u32 = u32::MAX;

/// In the index while a batch is checked: the id is of the batch, at the place in the low
/// bits. Slots are below it.
const NEW: u32 = 1 << 31;

/// [`NodeCold::flags`]: it has a key; it settled (`done_at` holds); its task started as it
/// (`started_at` holds); its task ran on a thread group (`group` holds).
const KEYED: u8 = 1;
const DONE: u8 = 2;
const STARTED: u8 = 4;
const GROUP: u8 = 8;

/// What adding, readying, starting and settling a node read: 64 bytes, one cache line, found
/// by its slot.
#[derive(Clone, Copy)]
struct NodeRow {
    /// The longest path of expected cost from it to the end of its request, its own included.
    priority: u64,
    /// Its place in the order nodes were added to the plane.
    seq: u64,
    cost: u64,
    /// The longest chain of measured run times through its request's nodes to it, its own
    /// included, once it settled ([`RequestStats::critical_path_ns`]).
    path_ns: u64,
    id: NodeId,
    request: u32,
    /// How many of its deps are not done.
    waiting: u32,
    task: u32,
    /// Its place in its request's ready heap of its task's pool.
    heap_at: u32,
    /// The member of its task that joined after it.
    next_member: u32,
    /// The first and last of its request's edges whose dep it is: the nodes that wait for it,
    /// in the order they were added.
    first_out: u32,
    last_out: u32,
}

const _: () = assert!(size_of::<NodeRow>() == 64);

/// What a node needs now and then: as it becomes ready, as its task starts, as its request
/// ends. 56 bytes, found by its slot.
#[derive(Clone, Copy)]
struct NodeCold {
    memory: u64,
    done_at: u64,
    started_at: u64,
    /// Its deps: `deps` edges of its request from `deps_at`.
    deps_at: u32,
    deps: u32,
    /// The dep that was done last when it became ready.
    blocker: u32,
    /// The thread group its task ran on.
    group: u32,
    /// Its name: a range of its request's `names`.
    name_at: u32,
    name_len: u32,
    pool: Pool,
    flags: u8,
}

const _: () = assert!(size_of::<NodeCold>() == 56);

/// A dep of a node in its request's edges: the dep, the node that waits for it (`owner`), and
/// the next edge of the same dep. A node's deps are one range of edges (CSR); the nodes that
/// wait for one are a list through `next`. 12 bytes an edge.
#[derive(Clone, Copy)]
struct Edge {
    dep: u32,
    owner: u32,
    next: u32,
}

const _: () = assert!(size_of::<Edge>() == 12);

/// A task: 32 bytes, found by its slot; its key, if `keyed`, in a column beside.
#[derive(Clone, Copy)]
struct TaskRow {
    memory: u64,
    /// The node whose id the task runs as, and whose request it counts against: the member
    /// that started it, once it runs.
    runner: u32,
    /// Every node that settles when it does, a list through [`NodeRow::next_member`] in the
    /// order they joined. While it is not running, each is ready in its own request.
    first: u32,
    last: u32,
    /// How many times a task with memory started while it did not fit.
    passed: u32,
    /// The last pass over it counted ([`Plane::pass`]), so a pass counts it once.
    stamp: u32,
    pool: Pool,
    running: bool,
    keyed: bool,
}

const _: () = assert!(size_of::<TaskRow>() == 32);

/// A ready node in its request's heap: the least `(rank, seq)` runs first, `rank` being the
/// complement of its priority, so the longest path goes first and ties to the node added
/// first. 24 bytes.
#[derive(Clone, Copy)]
struct Ready {
    rank: u64,
    seq: u64,
    node: u32,
}

const _: () = assert!(size_of::<Ready>() == 24);

impl Ready {
    fn before(&self, other: &Ready) -> bool {
        (self.rank, self.seq) < (other.rank, other.seq)
    }
}

struct Request {
    id: RequestId,
    /// Its place in the order requests were opened.
    seq: u64,
    weight: u32,
    /// What a start moves its virtual time on by: [`SHARE`] over its weight.
    stride: u128,
    keep_going: bool,
    closed: bool,
    cancelled: bool,
    finished: bool,
    /// Whether its room is given back once it finishes.
    release: bool,
    /// How many of its nodes are not settled.
    open: u32,
    /// Its first node that failed.
    failed: u32,
    /// Its ready nodes of each pool, each one of a task not running, as a 4-ary heap: the
    /// first to run first.
    ready: [Vec<Ready>; 2],
    /// Its place in the plane's `active` requests of each pool, while its heap of it is not
    /// empty.
    active_at: [u32; 2],
    /// Its virtual time in each pool, for fair share.
    vtime: [u128; 2],
    /// Its running nodes as `(priority, seq, slot)`, the most first, for
    /// [`PlaneOutput::Run`]'s `critical`. An entry goes stale when its node stops running or
    /// rises (a raise pushes a new one); a stale one is dropped when it comes to the top, and
    /// all of them once they outnumber the live ones ([`running_live`]).
    running: BinaryHeap<(u64, u64, u32)>,
    /// How many of its nodes are running.
    running_now: u32,
    nodes: Vec<u32>,
    edges: Vec<Edge>,
    names: String,
    stats: RequestStats,
}

impl Request {
    fn empty() -> Request {
        Request {
            id: RequestId(0),
            seq: 0,
            weight: 1,
            stride: SHARE,
            keep_going: false,
            closed: false,
            cancelled: false,
            finished: false,
            release: false,
            open: 0,
            failed: NONE,
            ready: [Vec::new(), Vec::new()],
            active_at: [NONE; 2],
            vtime: [0; 2],
            running: BinaryHeap::new(),
            running_now: 0,
            nodes: Vec::new(),
            edges: Vec::new(),
            names: String::new(),
            stats: RequestStats::default(),
        }
    }

    /// Opens it in its room, keeping what its buffers hold room for.
    fn open(&mut self, id: RequestId, seq: u64, weight: u32, keep_going: bool, vclock: [u128; 2]) {
        debug_assert!(self.nodes.is_empty() && self.edges.is_empty() && self.names.is_empty());
        debug_assert!(self.ready.iter().all(Vec::is_empty));
        self.id = id;
        self.seq = seq;
        self.weight = weight;
        self.stride = SHARE / u128::from(weight);
        self.keep_going = keep_going;
        self.closed = false;
        self.cancelled = false;
        self.finished = false;
        self.release = false;
        self.open = 0;
        self.failed = NONE;
        self.active_at = [NONE; 2];
        self.vtime = vclock;
        debug_assert_eq!(self.running_now, 0);
        self.running.clear();
        self.stats = RequestStats::default();
    }

    fn deps(&self, cold: &NodeCold) -> &[Edge] {
        &self.edges[cold.deps_at as usize..(cold.deps_at + cold.deps) as usize]
    }

    fn name(&self, cold: &NodeCold) -> &str {
        &self.names[cold.name_at as usize..(cold.name_at + cold.name_len) as usize]
    }
}

/// What [`Plane::check`] found of a batch of nodes, for [`Plane::add`]; its buffers are kept
/// for the next batch.
#[derive(Default)]
struct Batch {
    /// Each node's deps, by its place in the batch: a slot, or [`NEW`] and the place of a node
    /// of the batch.
    deps: Vec<u32>,
    /// Where each place's deps begin in `deps`, and the end.
    deps_at: Vec<u32>,
    /// The places in the order they are added, when that is not the order given.
    order: Vec<u32>,
    /// The slot each place took, once added.
    slots: Vec<u32>,
    /// For Kahn's order: each place's deps of the batch not yet placed; the places that wait
    /// for each, as CSR; the places that can go, least first.
    waiting: Vec<u32>,
    then_at: Vec<u32>,
    then: Vec<u32>,
    can: BinaryHeap<Reverse<u32>>,
}

impl Batch {
    /// Kahn's order over a batch with a dep on a later node of it, into `order`, the first
    /// given first among those that can go; or the ids of the nodes in a cycle, sorted.
    fn sort(&mut self, nodes: &[Spec]) -> Result<(), Vec<NodeId>> {
        let len = nodes.len();
        self.waiting.clear();
        self.waiting.resize(len, 0);
        self.then_at.clear();
        self.then_at.resize(len + 1, 0);
        for at in 0..len {
            for &dep in &self.deps[self.deps_at[at] as usize..self.deps_at[at + 1] as usize] {
                if dep & NEW != 0 {
                    self.waiting[at] += 1;
                    self.then_at[(dep & !NEW) as usize + 1] += 1;
                }
            }
        }
        for at in 0..len {
            self.then_at[at + 1] += self.then_at[at];
        }
        self.then.clear();
        self.then.resize(self.then_at[len] as usize, 0);
        for at in 0..len {
            for &dep in &self.deps[self.deps_at[at] as usize..self.deps_at[at + 1] as usize] {
                if dep & NEW != 0 {
                    let fill = &mut self.then_at[(dep & !NEW) as usize];
                    self.then[*fill as usize] = at as u32;
                    *fill += 1;
                }
            }
        }
        for at in (1..=len).rev() {
            self.then_at[at] = self.then_at[at - 1];
        }
        self.then_at[0] = 0;
        self.can.clear();
        self.can.extend((0..len as u32).filter(|&at| self.waiting[at as usize] == 0).map(Reverse));
        self.order.clear();
        while let Some(Reverse(at)) = self.can.pop() {
            self.order.push(at);
            let at = at as usize;
            for then in self.then_at[at]..self.then_at[at + 1] {
                let next = self.then[then as usize] as usize;
                self.waiting[next] -= 1;
                if self.waiting[next] == 0 {
                    self.can.push(Reverse(next as u32));
                }
            }
        }
        if self.order.len() < len {
            let mut cycle: Vec<NodeId> =
                (0..len).filter(|&at| self.waiting[at] > 0).map(|at| nodes[at].id).collect();
            cycle.sort();
            self.order.clear();
            return Err(cycle);
        }
        Ok(())
    }
}

/// Keys of tasks, by the key's own bits.
type ByKey<V> = HashMap<Key, V, BuildHasherDefault<KeyHasher>>;

/// The hash of a [`Key`] is the `u64` it hashes as: a content hash is hashed again by nothing.
#[derive(Default)]
struct KeyHasher(u64);

impl Hasher for KeyHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write_u64(&mut self, word: u64) {
        self.0 = word;
    }

    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 = (self.0.rotate_left(5) ^ u64::from(byte)).wrapping_mul(0x517c_c1b7_2722_0a95);
        }
    }
}

pub struct Plane {
    config: PlaneConfig,
    /// The node columns, by slot.
    rows: Vec<NodeRow>,
    cold: Vec<NodeCold>,
    states: Vec<NodeState>,
    keys: Vec<Key>,
    index: FxHashMap<NodeId, u32>,
    /// The task columns, by slot.
    tasks: Vec<TaskRow>,
    task_keys: Vec<Key>,
    requests: Vec<Request>,
    request_index: FxHashMap<RequestId, u32>,
    /// Slots of `rows`, `tasks` and `requests` given back, taken again first.
    free_nodes: Vec<u32>,
    free_tasks: Vec<u32>,
    free_requests: Vec<u32>,
    next_seq: u64,
    memo: Option<Box<dyn Memo + Send>>,
    /// Keys of tasks not yet settled.
    flight: ByKey<u32>,
    running: [usize; 2],
    granted: u64,
    held: u64,
    /// The virtual time of each pool: that of the request that started a task last.
    vclock: [u128; 2],
    /// The requests whose ready heap of each pool is not empty, in no order.
    active: [Vec<u32>; 2],
    /// The task passed over for memory long enough that it holds back the others.
    hold: Option<u32>,
    /// The count of passes over tasks ([`TaskRow::stamp`]).
    passes: u32,
    /// Nodes to settle once the move under way is done, as the memo or the budget says, in
    /// order: settling them where they are found would nest without bound.
    deferred: Vec<(u32, NodeState)>,
    last_ns: u64,
    counts: Counts,
    /// Buffers of one step, kept for the next.
    batch: Batch,
    touched: Vec<u32>,
    added: Vec<u32>,
    rising: Vec<(u32, u64)>,
    skipping: Vec<u32>,
    order: Vec<u32>,
    passed: Vec<u32>,
    scan: BinaryHeap<Reverse<(u64, u64, u32)>>,
}

fn pool_at(pool: Pool) -> usize {
    match pool {
        Pool::Io => 0,
        Pool::Work => 1,
    }
}

fn gb(bytes: u64) -> String {
    format!("{:.2} GB", bytes as f64 / f64::from(1 << 30))
}

impl Plane {
    /// A plane with no database: every node with a key runs, joined only to a task of its key
    /// that has not settled. For one build of a graph, as [`crate::machine::Machine`] is.
    pub fn new(config: PlaneConfig) -> Result<Plane, Error> {
        Plane::build(config, None)
    }

    /// A plane that asks `memo` before it runs a node with a key.
    pub fn with_memo(config: PlaneConfig, memo: Box<dyn Memo + Send>) -> Result<Plane, Error> {
        Plane::build(config, Some(memo))
    }

    fn build(config: PlaneConfig, memo: Option<Box<dyn Memo + Send>>) -> Result<Plane, Error> {
        for (pool, threads) in [(Pool::Io, config.io), (Pool::Work, config.work)] {
            if threads == 0 {
                return Err(Error::NoThreads(pool));
            }
        }
        Ok(Plane {
            config,
            rows: Vec::new(),
            cold: Vec::new(),
            states: Vec::new(),
            keys: Vec::new(),
            index: FxHashMap::default(),
            tasks: Vec::new(),
            task_keys: Vec::new(),
            requests: Vec::new(),
            request_index: FxHashMap::default(),
            free_nodes: Vec::new(),
            free_tasks: Vec::new(),
            free_requests: Vec::new(),
            next_seq: 0,
            memo,
            flight: ByKey::default(),
            running: [0; 2],
            granted: 0,
            held: 0,
            vclock: [0; 2],
            active: [Vec::new(), Vec::new()],
            hold: None,
            passes: 0,
            deferred: Vec::new(),
            last_ns: 0,
            counts: Counts::default(),
            batch: Batch::default(),
            touched: Vec::new(),
            added: Vec::new(),
            rising: Vec::new(),
            skipping: Vec::new(),
            order: Vec::new(),
            passed: Vec::new(),
            scan: BinaryHeap::new(),
        })
    }

    pub fn config(&self) -> &PlaneConfig {
        &self.config
    }

    pub fn counts(&self) -> Counts {
        self.counts
    }

    /// The memory granted to the tasks running now.
    pub fn granted(&self) -> u64 {
        self.granted
    }

    /// The state of `id`, if it was added.
    pub fn state(&self, id: NodeId) -> Option<&NodeState> {
        Some(&self.states[*self.index.get(&id)? as usize])
    }

    /// What `request` did so far, if it is known: open, or finished and not released.
    pub fn stats(&self, request: RequestId) -> Option<RequestStats> {
        Some(self.requests[*self.request_index.get(&request)? as usize].stats)
    }

    /// How many node, task and request slots the plane has made, free or not.
    #[cfg(test)]
    pub(crate) fn slots(&self) -> (usize, usize, usize) {
        (self.rows.len(), self.tasks.len(), self.requests.len())
    }

    /// A node of the task that holds back every other task with memory until it fits, if one
    /// does.
    pub fn holding(&self) -> Option<NodeId> {
        let task = &self.tasks[self.hold? as usize];
        if task.running || task.first == NONE {
            return None;
        }
        Some(self.rows[task.first as usize].id)
    }

    /// Gives back the room of `request` once it has finished (now, if it has): its nodes are
    /// no longer known, by [`Plane::state`] or as deps, and its id is unknown, so it may be
    /// opened again. What a long-lived plane holds is then what its open requests hold.
    pub fn release(&mut self, request: RequestId) -> Result<(), Error> {
        let &r = self.request_index.get(&request).ok_or(Error::UnknownRequest(request.0))?;
        self.requests[r as usize].release = true;
        if self.requests[r as usize].finished {
            self.free_request(r);
        }
        Ok(())
    }

    /// The room of finished request `r` and its nodes, for the next to take. Each of its
    /// nodes is settled, so no task, ready heap or node of another request points at them.
    fn free_request(&mut self, r: u32) {
        let request = &mut self.requests[r as usize];
        debug_assert!(request.finished && request.open == 0);
        self.request_index.remove(&request.id);
        for &me in &request.nodes {
            let me = me as usize;
            debug_assert!(self.states[me].settled() && self.rows[me].task == NONE);
            self.index.remove(&self.rows[me].id);
            self.states[me] = NodeState::Waiting;
        }
        self.free_nodes.extend_from_slice(&request.nodes);
        request.nodes.clear();
        request.edges.clear();
        request.names.clear();
        self.free_requests.push(r);
    }

    fn free_task(&mut self, task: u32) {
        if self.hold == Some(task) {
            self.hold = None;
        }
        self.free_tasks.push(task);
    }

    fn seq(&mut self) -> u64 {
        self.next_seq += 1;
        self.next_seq
    }

    pub fn step(&mut self, input: PlaneInput) -> Result<Vec<PlaneOutput>, Error> {
        let mut out = Vec::new();
        self.step_into(input, &mut out)?;
        Ok(out)
    }

    /// [`Plane::step`], with its outputs appended to `out`: a caller that keeps one buffer and
    /// drains it gets them with no allocation of its own per step. An input refused appends
    /// nothing and changes nothing.
    pub fn step_into(&mut self, input: PlaneInput, out: &mut Vec<PlaneOutput>) -> Result<(), Error> {
        // A time before the last is the last: completions race the clock that stamps them.
        let at = input.at_ns().max(self.last_ns);
        let mut touched = std::mem::take(&mut self.touched);
        let taken = self.take(at, input, out, &mut touched);
        if taken.is_ok() {
            self.after(at, out, &mut touched);
        }
        touched.clear();
        self.touched = touched;
        taken
    }

    fn take(
        &mut self,
        at: u64,
        input: PlaneInput,
        out: &mut Vec<PlaneOutput>,
        touched: &mut Vec<u32>,
    ) -> Result<(), Error> {
        match input {
            PlaneInput::Open { request, weight, keep_going, .. } => {
                if self.request_index.contains_key(&request) {
                    return Err(Error::RequestTwice(request.0));
                }
                if weight == 0 {
                    return Err(Error::NoWeight(request.0));
                }
                let seq = self.seq();
                let r = match self.free_requests.pop() {
                    Some(r) => r,
                    None => {
                        self.requests.push(Request::empty());
                        (self.requests.len() - 1) as u32
                    }
                };
                self.requests[r as usize].open(request, seq, weight, keep_going, self.vclock);
                self.request_index.insert(request, r);
            }
            PlaneInput::Add { request, nodes, .. } => {
                let r = self.open_request(request)?;
                if self.requests[r as usize].closed {
                    return Err(Error::Closed);
                }
                self.check(r, &nodes)?;
                self.add(at, r, nodes, out);
                touched.push(r);
            }
            PlaneInput::Close { request, .. } => {
                let r = self.open_request(request)?;
                if self.requests[r as usize].closed {
                    return Err(Error::Closed);
                }
                self.requests[r as usize].closed = true;
                touched.push(r);
            }
            PlaneInput::Done { id, outcome, added, group, .. } => {
                let task = match self.index.get(&id) {
                    Some(&node) if matches!(self.states[node as usize], NodeState::Running) => {
                        let task = self.rows[node as usize].task;
                        let t = &self.tasks[task as usize];
                        if !(t.running && t.runner == node) {
                            return Err(Error::NotRunning(id));
                        }
                        task
                    }
                    _ => return Err(Error::NotRunning(id)),
                };
                let runner = self.tasks[task as usize].runner;
                let r = self.rows[runner as usize].request;
                self.check(r, &added)?;
                self.finish_task(at, task, outcome, group, out, touched);
                self.add(at, r, added, out);
                touched.push(r);
            }
            PlaneInput::Held { bytes, .. } => self.held = bytes,
        }
        Ok(())
    }

    /// Cancels `request`: each of its nodes not started is skipped, because of the request's
    /// first node (the protocol has no state of its own for it yet), after a note that says
    /// the request is cancelled. A task it runs is let finish, and its [`PlaneInput::Done`] is
    /// taken as ever: its nodes of other requests settle with it, and what it finds is added
    /// skipped. A node of it that joined another request's running task leaves it, skipped,
    /// and that task runs on for the rest. Once what it runs is done the request finishes,
    /// not ok. It takes no more [`PlaneInput::Add`] or [`PlaneInput::Close`]; a second cancel
    /// does nothing.
    pub fn cancel(&mut self, at_ns: u64, request: RequestId) -> Result<Vec<PlaneOutput>, Error> {
        let mut out = Vec::new();
        self.cancel_into(at_ns, request, &mut out)?;
        Ok(out)
    }

    /// [`Plane::cancel`], with its outputs appended to `out`, as [`Plane::step_into`].
    pub fn cancel_into(
        &mut self,
        at_ns: u64,
        request: RequestId,
        out: &mut Vec<PlaneOutput>,
    ) -> Result<(), Error> {
        let at = at_ns.max(self.last_ns);
        let r = self.open_request(request)?;
        let ri = r as usize;
        if !self.requests[ri].cancelled {
            self.requests[ri].cancelled = true;
            self.requests[ri].closed = true;
            out.push(said(
                request,
                at,
                Event::Said { level: Level::Note, text: format!("{request} is cancelled") },
            ));
            if let Some(&first) = self.requests[ri].nodes.first() {
                let because = self.rows[first as usize].id.0;
                for at_node in 0..self.requests[ri].nodes.len() {
                    let me = self.requests[ri].nodes[at_node];
                    let skip = match self.states[me as usize] {
                        NodeState::Waiting | NodeState::Ready => true,
                        NodeState::Running => {
                            let task = self.rows[me as usize].task;
                            task != NONE && self.tasks[task as usize].runner != me
                        }
                        _ => false,
                    };
                    if skip {
                        self.leave_task(me);
                        self.moved(at, me, NodeState::Skipped { because }, out);
                    }
                }
            }
        }
        let mut touched = std::mem::take(&mut self.touched);
        touched.push(r);
        self.after(at, out, &mut touched);
        touched.clear();
        self.touched = touched;
        Ok(())
    }

    /// What follows any input: the deferred nodes settled, the threads given work, and the
    /// requests `touched` that are over finished.
    fn after(&mut self, at: u64, out: &mut Vec<PlaneOutput>, touched: &mut Vec<u32>) {
        self.settle_deferred(at, out, touched);
        self.last_ns = at;
        self.dispatch(at, out, touched);
        let requests = &self.requests;
        touched.sort_unstable_by_key(|&r| requests[r as usize].seq);
        touched.dedup();
        for &r in touched.iter() {
            let request = &self.requests[r as usize];
            if request.closed && request.open == 0 && !request.finished {
                self.requests[r as usize].finished = true;
                let path = self.critical(r);
                let request = &self.requests[r as usize];
                let id = request.id;
                let ok = request.failed == NONE && !request.cancelled;
                let release = request.release;
                out.push(said(id, at, Event::Critical { path }));
                out.push(PlaneOutput::Finished { request: id, ok });
                if release {
                    self.free_request(r);
                }
            }
        }
    }

    fn open_request(&self, request: RequestId) -> Result<u32, Error> {
        match self.request_index.get(&request) {
            Some(&r) if !self.requests[r as usize].finished => Ok(r),
            Some(_) => Err(Error::Over),
            None => Err(Error::UnknownRequest(request.0)),
        }
    }

    /// Whether `nodes` can be added to request `r`: none of their ids known or given twice,
    /// each dep one of them or a node of `r`, and no cycle among them. If so, their ids are in
    /// the index as places of the batch ([`NEW`]), and [`Plane::batch`] says each one's deps
    /// and the order to add them in: each after its deps of them, the first given first among
    /// those that can go. If not, nothing is changed.
    fn check(&mut self, r: u32, nodes: &[Spec]) -> Result<(), Error> {
        let batch = &mut self.batch;
        batch.deps.clear();
        batch.deps_at.clear();
        batch.order.clear();
        if nodes.is_empty() {
            return Ok(());
        }
        assert!(nodes.len() < NEW as usize, "{} nodes in one input", nodes.len());
        for (at, spec) in nodes.iter().enumerate() {
            let fresh = match self.index.entry(spec.id) {
                Entry::Occupied(_) => false,
                Entry::Vacant(it) => {
                    it.insert(NEW | at as u32);
                    true
                }
            };
            if !fresh {
                unindex(&mut self.index, &nodes[..at]);
                return Err(Error::Twice(spec.id));
            }
        }
        let mut in_order = true;
        for (at, spec) in nodes.iter().enumerate() {
            batch.deps_at.push(batch.deps.len() as u32);
            for dep in &spec.deps {
                match self.index.get(dep) {
                    Some(&it) if it & NEW != 0 => {
                        in_order &= (it & !NEW) < at as u32;
                        batch.deps.push(it);
                    }
                    Some(&it) if self.rows[it as usize].request == r => batch.deps.push(it),
                    _ => {
                        unindex(&mut self.index, nodes);
                        return Err(Error::UnknownDep { node: spec.id, dep: *dep });
                    }
                }
            }
        }
        batch.deps_at.push(batch.deps.len() as u32);
        // Given each after its deps of them, the order is the order given.
        if !in_order && let Err(cycle) = batch.sort(nodes) {
            unindex(&mut self.index, nodes);
            return Err(Error::Cycle(cycle));
        }
        Ok(())
    }

    /// Adds the nodes [`Plane::check`] took to request `r`.
    fn add(&mut self, at: u64, r: u32, mut nodes: Vec<Spec>, out: &mut Vec<PlaneOutput>) {
        if nodes.is_empty() {
            return;
        }
        let mut batch = std::mem::take(&mut self.batch);
        if !batch.order.is_empty() {
            permute(&mut nodes, &mut batch.order);
        }
        batch.slots.clear();
        batch.slots.resize(nodes.len(), NONE);
        let mut added = std::mem::take(&mut self.added);
        added.clear();
        for (at_node, spec) in nodes.into_iter().enumerate() {
            let place = match batch.order.get(at_node) {
                Some(&place) => (place & !NEW) as usize,
                None => at_node,
            };
            let me = self.add_one(at, r, spec, &batch, place, out);
            batch.slots[place] = me;
            added.push(me);
        }
        self.batch = batch;
        // A node's priority is its cost and the most of its dependents'. A node is added after
        // its deps, so from the last added to the first each new one's dependents, all new,
        // have theirs before it. An older node's priority can only rise, and only through a
        // dep of a new one: what rises is carried up through its deps, and no further.
        for &me in added.iter().rev() {
            let edges = &self.requests[r as usize].edges;
            let row = &self.rows[me as usize];
            let mut after = 0;
            let mut edge = row.first_out;
            while edge != NONE {
                let it = edges[edge as usize];
                after = after.max(self.rows[it.owner as usize].priority);
                edge = it.next;
            }
            let priority = row.cost.saturating_add(after);
            self.raise(me, priority);
        }
        let mut rising = std::mem::take(&mut self.rising);
        rising.clear();
        let request = &self.requests[r as usize];
        for &me in &added {
            let priority = self.rows[me as usize].priority;
            let deps = request.deps(&self.cold[me as usize]);
            rising.extend(deps.iter().map(|it| (it.dep, priority)));
        }
        while let Some((me, after)) = rising.pop() {
            let priority = self.rows[me as usize].cost.saturating_add(after);
            if priority <= self.rows[me as usize].priority {
                continue;
            }
            self.raise(me, priority);
            let deps = self.requests[r as usize].deps(&self.cold[me as usize]);
            rising.extend(deps.iter().map(|it| (it.dep, priority)));
        }
        self.rising = rising;
        self.added = added;
    }

    /// Adds the node at `place` of the batch to request `r`, each of its deps added before it:
    /// its slot.
    fn add_one(
        &mut self,
        at: u64,
        r: u32,
        spec: Spec,
        batch: &Batch,
        place: usize,
        out: &mut Vec<PlaneOutput>,
    ) -> u32 {
        let Spec { id, kind, name, deps, pool, cost, key, memory } = spec;
        let me = match self.free_nodes.pop() {
            Some(me) => me,
            None => {
                assert!(self.rows.len() < NEW as usize, "the plane holds 2^31 nodes");
                self.rows.push(NodeRow {
                    priority: 0,
                    seq: 0,
                    cost: 0,
                    path_ns: 0,
                    id,
                    request: r,
                    waiting: 0,
                    task: NONE,
                    heap_at: NONE,
                    next_member: NONE,
                    first_out: NONE,
                    last_out: NONE,
                });
                self.cold.push(NodeCold {
                    memory: 0,
                    done_at: 0,
                    started_at: 0,
                    deps_at: 0,
                    deps: 0,
                    blocker: NONE,
                    group: 0,
                    name_at: 0,
                    name_len: 0,
                    pool,
                    flags: 0,
                });
                self.states.push(NodeState::Waiting);
                self.keys.push(Key([0; 32]));
                (self.rows.len() - 1) as u32
            }
        };
        let request = &mut self.requests[r as usize];
        let deps_at = request.edges.len() as u32;
        let mut waiting = 0;
        // The id of the node it is skipped because of.
        let mut skipped: Option<u32> = None;
        let mut blocker = NONE;
        let later = |rows: &[NodeRow], cold: &[NodeCold], it: u32| {
            (cold[it as usize].done_at, rows[it as usize].seq)
        };
        for &dep in &batch.deps[batch.deps_at[place] as usize..batch.deps_at[place + 1] as usize] {
            let dep = if dep & NEW != 0 { batch.slots[(dep & !NEW) as usize] } else { dep };
            match &self.states[dep as usize] {
                NodeState::Built | NodeState::Cached { .. } => {
                    if blocker == NONE
                        || later(&self.rows, &self.cold, dep) > later(&self.rows, &self.cold, blocker)
                    {
                        blocker = dep;
                    }
                }
                NodeState::Failed { .. } => _ = skipped.get_or_insert(self.rows[dep as usize].id.0),
                NodeState::Skipped { because } => _ = skipped.get_or_insert(*because),
                _ => waiting += 1,
            }
            let edge = request.edges.len() as u32;
            request.edges.push(Edge { dep, owner: me, next: NONE });
            let row = &mut self.rows[dep as usize];
            if row.last_out == NONE {
                row.first_out = edge;
            } else {
                request.edges[row.last_out as usize].next = edge;
            }
            row.last_out = edge;
        }
        let deps_len = request.edges.len() as u32 - deps_at;
        if !request.keep_going && skipped.is_none() && request.failed != NONE {
            skipped = Some(self.rows[request.failed as usize].id.0);
        }
        if request.cancelled
            && skipped.is_none()
            && let Some(&first) = request.nodes.first()
        {
            skipped = Some(self.rows[first as usize].id.0);
        }
        let name_at = u32::try_from(request.names.len()).expect("a request's names under 4 GiB");
        request.names.push_str(&name);
        request.nodes.push(me);
        request.open += 1;
        let request_id = request.id;
        out.push(said(
            request_id,
            at,
            Event::Node { id: id.0, kind, name, deps: deps.into_iter().map(|it| it.0).collect() },
        ));
        *self.index.get_mut(&id).expect("a checked node in the index") = me;
        let seq = self.seq();
        let m = me as usize;
        self.rows[m] = NodeRow {
            priority: cost,
            seq,
            cost,
            path_ns: 0,
            id,
            request: r,
            waiting,
            task: NONE,
            heap_at: NONE,
            next_member: NONE,
            first_out: NONE,
            last_out: NONE,
        };
        self.cold[m] = NodeCold {
            memory,
            done_at: 0,
            started_at: 0,
            deps_at,
            deps: deps_len,
            blocker,
            group: 0,
            name_at,
            name_len: self.requests[r as usize].names.len() as u32 - name_at,
            pool,
            flags: if key.is_some() { KEYED } else { 0 },
        };
        self.keys[m] = key.unwrap_or(Key([0; 32]));
        self.states[m] = NodeState::Waiting;
        match skipped {
            Some(because) => self.moved(at, me, NodeState::Skipped { because }, out),
            None if waiting == 0 => self.became_ready(at, me, out),
            None => self.moved(at, me, NodeState::Waiting, out),
        }
        me
    }

    /// Sets `me`'s priority, higher than it was, keeping its place among its request's ready
    /// nodes.
    fn raise(&mut self, me: u32, priority: u64) {
        let row = &mut self.rows[me as usize];
        if row.priority == priority {
            return;
        }
        debug_assert!(row.priority < priority, "a priority only rises");
        row.priority = priority;
        let (at, r, task) = (row.heap_at, row.request, row.task);
        if at == NONE {
            if self.states[me as usize] == NodeState::Running {
                self.track_running(me);
            }
            return;
        }
        let p = pool_at(self.tasks[task as usize].pool);
        let heap = &mut self.requests[r as usize].ready[p];
        heap[at as usize].rank = !priority;
        sift_up(heap, &mut self.rows, at as usize);
    }

    /// Says `me` is now `state`, and keeps what follows from it for its request.
    fn moved(&mut self, at: u64, me: u32, state: NodeState, out: &mut Vec<PlaneOutput>) {
        let row = &self.rows[me as usize];
        let request = &mut self.requests[row.request as usize];
        if state.settled() {
            request.open -= 1;
        }
        let was = self.states[me as usize] == NodeState::Running;
        let now = state == NodeState::Running;
        if was && !now {
            request.running_now -= 1;
        }
        out.push(said(request.id, at, Event::Moved { id: row.id.0, state: state.clone() }));
        self.states[me as usize] = state;
        if now && !was {
            self.requests[self.rows[me as usize].request as usize].running_now += 1;
            self.track_running(me);
        }
    }

    /// Puts running `me` at its priority among its request's running nodes.
    fn track_running(&mut self, me: u32) {
        let row = self.rows[me as usize];
        let request = &mut self.requests[row.request as usize];
        request.running.push((row.priority, row.seq, me));
        if request.running.len() > 2 * request.running_now as usize + 16 {
            let (rows, states) = (&self.rows, &self.states);
            request.running.retain(|&it| running_live(rows, states, it));
        }
    }

    /// Whether the task `me` starts is on its request's critical path: its priority is the
    /// most of its request's ready and running nodes. Called once `me` runs.
    fn on_critical_path(&mut self, me: u32) -> bool {
        let row = self.rows[me as usize];
        let request = &mut self.requests[row.request as usize];
        let (rows, states) = (&self.rows, &self.states);
        let running = loop {
            match request.running.peek().copied() {
                Some(top) if !running_live(rows, states, top) => _ = request.running.pop(),
                top => break top.map_or(0, |it| it.0),
            }
        };
        let ready = request.ready.iter().filter_map(|it| it.first()).map(|it| !it.rank).max();
        row.priority >= running.max(ready.unwrap_or(0))
    }

    /// Puts `me`, of a task not running, among its request's ready nodes of pool `p`. A
    /// request that had none comes back at the pool's virtual time: what it did not use while
    /// it had nothing to run is not owed to it.
    fn make_ready(&mut self, me: u32, p: usize) {
        let row = self.rows[me as usize];
        let request = &mut self.requests[row.request as usize];
        if request.ready[p].is_empty() {
            request.vtime[p] = request.vtime[p].max(self.vclock[p]);
            request.active_at[p] = self.active[p].len() as u32;
            self.active[p].push(row.request);
        }
        let entry = Ready { rank: !row.priority, seq: row.seq, node: me };
        let heap = &mut request.ready[p];
        heap.push(entry);
        let last = heap.len() - 1;
        sift_up(heap, &mut self.rows, last);
    }

    /// Takes `me` from its request's ready nodes of pool `p`.
    fn unready(&mut self, me: u32, p: usize) {
        let row = &self.rows[me as usize];
        let (r, at) = (row.request, row.heap_at);
        debug_assert!(at != NONE, "a node not ready taken from its ready nodes");
        let request = &mut self.requests[r as usize];
        heap_remove(&mut request.ready[p], &mut self.rows, at as usize);
        if request.ready[p].is_empty() {
            let place = request.active_at[p];
            request.active_at[p] = NONE;
            self.active[p].swap_remove(place as usize);
            if let Some(&moved) = self.active[p].get(place as usize) {
                self.requests[moved as usize].active_at[p] = place;
            }
        }
    }

    /// `me`'s deps are done: it joins a task of its key, is settled by what the database has
    /// of the key, or is a task of its own, ready to run.
    fn became_ready(&mut self, at: u64, me: u32, out: &mut Vec<PlaneOutput>) {
        let m = me as usize;
        let cold = self.cold[m];
        let r = self.rows[m].request as usize;
        let task = if cold.flags & KEYED == 0 {
            if let Some(why) = too_big(&self.config, &self.requests[r], &cold) {
                self.deferred.push((me, NodeState::Failed { why }));
                return;
            }
            new_task(&mut self.tasks, &mut self.task_keys, &mut self.free_tasks, me, &cold, None)
        } else {
            let key = self.keys[m];
            // A key in flight is not yet in the database: its task puts it there before its
            // `Done`, so asking first would only ask in vain.
            let vacant = match self.flight.entry(key) {
                Entry::Occupied(it) => {
                    let task = *it.get();
                    self.join(at, me, task, out);
                    return;
                }
                Entry::Vacant(it) => it,
            };
            if let Some(outcome) = self.memo.as_ref().and_then(|memo| memo.settled(key)) {
                self.counts.remembered += 1;
                self.requests[r].stats.cached += 1;
                let state = match outcome {
                    Outcome::Built => NodeState::Cached { remote: false },
                    Outcome::Cached { remote } => NodeState::Cached { remote },
                    Outcome::Failed { why } => NodeState::Failed { why },
                };
                self.deferred.push((me, state));
                return;
            }
            if let Some(why) = too_big(&self.config, &self.requests[r], &cold) {
                self.deferred.push((me, NodeState::Failed { why }));
                return;
            }
            let task = new_task(
                &mut self.tasks,
                &mut self.task_keys,
                &mut self.free_tasks,
                me,
                &cold,
                Some(key),
            );
            vacant.insert(task);
            task
        };
        self.rows[m].task = task;
        self.make_ready(me, pool_at(cold.pool));
        self.moved(at, me, NodeState::Ready, out);
    }

    /// `me` joins `task`, of its key, as its last member.
    fn join(&mut self, at: u64, me: u32, task: u32, out: &mut Vec<PlaneOutput>) {
        self.counts.joined += 1;
        let t = &mut self.tasks[task as usize];
        debug_assert!(t.last != NONE, "a task in flight has a member");
        self.rows[t.last as usize].next_member = me;
        t.last = me;
        let (running, pool) = (t.running, t.pool);
        let row = &mut self.rows[me as usize];
        row.next_member = NONE;
        row.task = task;
        if running {
            self.moved(at, me, NodeState::Running, out);
        } else {
            self.make_ready(me, pool_at(pool));
            self.moved(at, me, NodeState::Ready, out);
        }
    }

    fn settle_deferred(&mut self, at: u64, out: &mut Vec<PlaneOutput>, touched: &mut Vec<u32>) {
        let mut next = 0;
        while next < self.deferred.len() {
            let me = self.deferred[next].0;
            let state = std::mem::replace(&mut self.deferred[next].1, NodeState::Waiting);
            next += 1;
            // Skipped by a failure of the same step before it was settled.
            if self.states[me as usize].settled() {
                continue;
            }
            touched.push(self.rows[me as usize].request);
            self.settle(at, me, state, out);
        }
        self.deferred.clear();
    }

    fn settle(&mut self, at: u64, me: u32, state: NodeState, out: &mut Vec<PlaneOutput>) {
        let failed = matches!(state, NodeState::Failed { .. });
        let m = me as usize;
        let (row, cold) = (self.rows[m], self.cold[m]);
        let r = row.request as usize;
        // Each dep settled before `me` was ready, so its path is known.
        let own = if cold.flags & STARTED != 0 { at.saturating_sub(cold.started_at) } else { 0 };
        let request = &mut self.requests[r];
        let before = request.deps(&cold).iter().map(|it| self.rows[it.dep as usize].path_ns).max();
        let path = own.saturating_add(before.unwrap_or(0));
        request.stats.critical_path_ns = request.stats.critical_path_ns.max(path);
        self.rows[m].path_ns = path;
        self.cold[m].done_at = at;
        self.cold[m].flags |= DONE;
        self.moved(at, me, state, out);
        if !failed {
            let mut edge = row.first_out;
            while edge != NONE {
                let it = self.requests[r].edges[edge as usize];
                edge = it.next;
                let dependent = &mut self.rows[it.owner as usize];
                dependent.waiting -= 1;
                if dependent.waiting == 0 && matches!(self.states[it.owner as usize], NodeState::Waiting) {
                    self.cold[it.owner as usize].blocker = me;
                    self.became_ready(at, it.owner, out);
                }
            }
            return;
        }
        let because = row.id.0;
        let request = &mut self.requests[r];
        if request.failed == NONE {
            request.failed = me;
        }
        let mut skipping = std::mem::take(&mut self.skipping);
        skipping.clear();
        push_dependents(&mut skipping, &self.requests[r].edges, row.first_out);
        if !self.requests[r].keep_going {
            skipping.extend_from_slice(&self.requests[r].nodes);
        }
        let mut at_skipping = 0;
        while let Some(&it) = skipping.get(at_skipping) {
            at_skipping += 1;
            if matches!(self.states[it as usize], NodeState::Waiting | NodeState::Ready) {
                self.leave_task(it);
                self.moved(at, it, NodeState::Skipped { because }, out);
                let first_out = self.rows[it as usize].first_out;
                push_dependents(&mut skipping, &self.requests[r].edges, first_out);
            }
        }
        self.skipping = skipping;
    }

    /// `me` no longer waits for its task: a node of a task not running is no longer ready, and
    /// a task with no node left is dropped. A running task runs on without it; its runner
    /// never leaves it.
    fn leave_task(&mut self, me: u32) {
        let m = me as usize;
        let task = self.rows[m].task;
        if task == NONE {
            return;
        }
        self.rows[m].task = NONE;
        let t = self.tasks[task as usize];
        debug_assert!(!t.running || t.runner != me, "the runner of a running task left it");
        let next = self.rows[m].next_member;
        self.rows[m].next_member = NONE;
        let mut before = NONE;
        let mut member = t.first;
        while member != me {
            before = member;
            member = self.rows[member as usize].next_member;
        }
        if before == NONE {
            self.tasks[task as usize].first = next;
        } else {
            self.rows[before as usize].next_member = next;
        }
        if t.last == me {
            self.tasks[task as usize].last = before;
        }
        if t.running {
            return;
        }
        self.unready(me, pool_at(t.pool));
        if self.tasks[task as usize].first == NONE {
            if t.keyed {
                let key = self.task_keys[task as usize];
                self.flight.remove(&key);
            }
            self.free_task(task);
        }
    }

    fn finish_task(
        &mut self,
        at: u64,
        task: u32,
        outcome: Outcome,
        group: Option<u32>,
        out: &mut Vec<PlaneOutput>,
        touched: &mut Vec<u32>,
    ) {
        let t = self.tasks[task as usize];
        self.tasks[task as usize].running = false;
        let p = pool_at(t.pool);
        self.running[p] -= 1;
        self.granted -= t.memory;
        if t.keyed {
            let key = self.task_keys[task as usize];
            self.flight.remove(&key);
        }
        self.free_task(task);
        let mut state = match outcome {
            Outcome::Built => NodeState::Built,
            Outcome::Cached { remote } => NodeState::Cached { remote },
            Outcome::Failed { why } => NodeState::Failed { why },
        };
        let mut member = t.first;
        while member != NONE {
            let m = member as usize;
            let next = self.rows[m].next_member;
            let r = self.rows[m].request;
            if member != t.runner {
                self.requests[r as usize].stats.joined += 1;
            }
            let cold = &mut self.cold[m];
            match group {
                Some(group) => {
                    cold.group = group;
                    cold.flags |= GROUP;
                }
                None => cold.flags &= !GROUP,
            }
            self.rows[m].task = NONE;
            self.rows[m].next_member = NONE;
            touched.push(r);
            // Every member is running: a node that leaves a running task is taken out of it.
            debug_assert_eq!(self.states[m], NodeState::Running);
            let state =
                if next == NONE { std::mem::replace(&mut state, NodeState::Waiting) } else { state.clone() };
            self.settle(at, member, state, out);
            member = next;
        }
    }

    fn memory_free(&self) -> Option<u64> {
        let budget = self.config.budget.as_ref()?;
        Some(budget.bytes.saturating_sub(self.granted.max(self.held)))
    }

    /// The task that holds back the others, while it is still to start.
    fn hold(&mut self) -> Option<u32> {
        if let Some(task) = self.hold
            && (self.tasks[task as usize].running || self.tasks[task as usize].first == NONE)
        {
            self.hold = None;
        }
        self.hold
    }

    /// The request of `active` whose turn it is in pool `p`: the least virtual time, then the
    /// heavier, then the first opened.
    fn turn(&self, r: u32, p: usize) -> (u128, Reverse<u32>, u64) {
        let request = &self.requests[r as usize];
        (request.vtime[p], Reverse(request.weight), request.seq)
    }

    fn dispatch(&mut self, at: u64, out: &mut Vec<PlaneOutput>, touched: &mut Vec<u32>) {
        for pool in [Pool::Io, Pool::Work] {
            let (p, limit) = (pool_at(pool), self.threads(pool));
            if self.config.budget.is_none() {
                // With no budget the first ready node of the request whose turn it is starts.
                while self.running[p] < limit {
                    let Some(r) = self.active[p].iter().copied().min_by_key(|&r| self.turn(r, p))
                    else {
                        break;
                    };
                    let me = self.requests[r as usize].ready[p][0].node;
                    self.start(at, me, out);
                }
                continue;
            }
            while self.running[p] < limit {
                let mut order = std::mem::take(&mut self.order);
                order.clear();
                order.extend_from_slice(&self.active[p]);
                order.sort_unstable_by_key(|&r| self.turn(r, p));
                let mut passed = std::mem::take(&mut self.passed);
                passed.clear();
                let mut picked = Pick::None;
                for &r in &order {
                    picked = self.pick(r, p, &mut passed);
                    if !matches!(picked, Pick::None) {
                        break;
                    }
                }
                self.order = order;
                match picked {
                    Pick::Start(me) => {
                        let task = self.rows[me as usize].task;
                        if self.tasks[task as usize].memory > 0 {
                            self.pass(&passed);
                        }
                        self.passed = passed;
                        self.start(at, me, out);
                    }
                    Pick::Fail(me, why) => {
                        self.passed = passed;
                        self.leave_task(me);
                        touched.push(self.rows[me as usize].request);
                        self.deferred.push((me, NodeState::Failed { why }));
                        self.settle_deferred(at, out, touched);
                    }
                    Pick::None => {
                        self.passed = passed;
                        break;
                    }
                }
            }
        }
    }

    /// Each of the tasks `passed` over was so once more; the first of them to have been so
    /// [`PlaneConfig::starve_after`] times holds back the rest, if none does.
    fn pass(&mut self, passed: &[u32]) {
        if passed.is_empty() {
            return;
        }
        self.passes = self.passes.wrapping_add(1);
        if self.passes == 0 {
            for task in &mut self.tasks {
                task.stamp = 0;
            }
            self.passes = 1;
        }
        for &task in passed {
            let t = &mut self.tasks[task as usize];
            if t.stamp == self.passes {
                continue;
            }
            t.stamp = self.passes;
            t.passed = t.passed.saturating_add(1);
            let starved = t.passed >= self.config.starve_after;
            if starved && self.hold().is_none() {
                self.hold = Some(task);
            }
        }
    }

    fn threads(&self, pool: Pool) -> usize {
        match pool {
            Pool::Io => self.config.io,
            Pool::Work => self.config.work,
        }
    }

    /// The node of request `r` whose task starts next on pool `p`, as memory allows; the tasks
    /// of it that did not fit, before that one, are added to `passed`. Only with a budget.
    fn pick(&mut self, r: u32, p: usize, passed: &mut Vec<u32>) -> Pick {
        let free = self.memory_free().expect("a budget");
        let nothing_runs = self.running == [0, 0];
        let hold = self.hold();
        // The heap in order, best first: from its root, the least of the places seen whose
        // parent was taken.
        let mut scan = std::mem::take(&mut self.scan);
        scan.clear();
        let heap = &self.requests[r as usize].ready[p];
        if let Some(first) = heap.first() {
            scan.push(Reverse((first.rank, first.seq, 0)));
        }
        let mut unfit = NONE;
        let mut picked = NONE;
        while let Some(Reverse((_, _, at))) = scan.pop() {
            let at = at as usize;
            let me = heap[at].node;
            let task = self.rows[me as usize].task;
            let memory = self.tasks[task as usize].memory;
            match hold {
                // Only it, and what takes no memory, may start until it does.
                Some(held) if held != task => {
                    if memory == 0 {
                        picked = me;
                        break;
                    }
                }
                _ if memory <= free => {
                    picked = me;
                    break;
                }
                Some(_) => unfit = me,
                None => {
                    if unfit == NONE {
                        unfit = me;
                    }
                    passed.push(task);
                }
            }
            for child in 4 * at + 1..(4 * at + 5).min(heap.len()) {
                let it = &heap[child];
                scan.push(Reverse((it.rank, it.seq, child as u32)));
            }
        }
        self.scan = scan;
        if picked != NONE {
            return Pick::Start(picked);
        }
        if unfit == NONE || !nothing_runs {
            return Pick::None;
        }
        let budget = self.config.budget.as_ref().expect("a budget");
        let cold = &self.cold[unfit as usize];
        let request = &self.requests[r as usize];
        Pick::Fail(
            unfit,
            format!(
                "{} needs {} of memory as predicted, and with nothing of blaze's running \
                 the process holds {} of the {} it may use ({})",
                request.name(cold),
                gb(cold.memory),
                gb(self.held),
                gb(budget.bytes),
                budget.from
            ),
        )
    }

    fn start(&mut self, at: u64, me: u32, out: &mut Vec<PlaneOutput>) {
        let m = me as usize;
        let task = self.rows[m].task;
        let t = self.tasks[task as usize];
        let p = pool_at(t.pool);
        let mut member = t.first;
        while member != NONE {
            self.unready(member, p);
            member = self.rows[member as usize].next_member;
        }
        self.cold[m].started_at = at;
        self.cold[m].flags |= STARTED;
        let request = &mut self.requests[self.rows[m].request as usize];
        request.stats.ran += 1;
        self.vclock[p] = self.vclock[p].max(request.vtime[p]);
        request.vtime[p] += request.stride;
        self.tasks[task as usize].runner = me;
        self.tasks[task as usize].running = true;
        self.running[p] += 1;
        self.granted += t.memory;
        self.counts.granted_most = self.counts.granted_most.max(self.granted);
        self.counts.ran += 1;
        let mut member = t.first;
        while member != NONE {
            self.moved(at, member, NodeState::Running, out);
            member = self.rows[member as usize].next_member;
        }
        let blocker = self.cold[m].blocker;
        let near = (blocker != NONE && self.cold[blocker as usize].flags & GROUP != 0)
            .then(|| self.cold[blocker as usize].group);
        let critical = self.on_critical_path(me);
        out.push(PlaneOutput::Run { id: self.rows[m].id, pool: t.pool, near, critical });
    }

    /// From request `r`'s node that settled last, back through what each one waited for last:
    /// their ids.
    fn critical(&self, r: u32) -> Vec<u32> {
        let last = self.requests[r as usize]
            .nodes
            .iter()
            .copied()
            .filter(|&it| self.cold[it as usize].flags & DONE != 0)
            .max_by_key(|&it| (self.cold[it as usize].done_at, Reverse(self.rows[it as usize].seq)));
        let blocker = |it: &u32| Some(self.cold[*it as usize].blocker).filter(|&it| it != NONE);
        let mut path: Vec<u32> =
            std::iter::successors(last, blocker).map(|it| self.rows[it as usize].id.0).collect();
        path.reverse();
        path
    }
}

/// Whether an entry of a request's running nodes is still what it says: the same node (a slot
/// reused has another `seq`), running, at that priority.
fn running_live(rows: &[NodeRow], states: &[NodeState], (priority, seq, node): (u64, u64, u32)) -> bool {
    let row = &rows[node as usize];
    row.seq == seq && row.priority == priority && states[node as usize] == NodeState::Running
}

enum Pick {
    Start(u32),
    /// It cannot fit even with nothing running.
    Fail(u32, String),
    None,
}

fn said(request: RequestId, at_ns: u64, event: Event) -> PlaneOutput {
    PlaneOutput::Said { request, record: Record { at_ns, event } }
}

/// Why a node of `request` cannot run at all as the budget stands: its prediction alone is
/// more.
fn too_big(config: &PlaneConfig, request: &Request, cold: &NodeCold) -> Option<String> {
    let budget = config.budget.as_ref()?;
    (cold.memory > budget.bytes).then(|| {
        format!(
            "{} needs {} of memory as predicted, more than blaze may use here: {} ({})",
            request.name(cold),
            gb(cold.memory),
            gb(budget.bytes),
            budget.from
        )
    })
}

/// A task of `me` alone, not running, in a free slot: its slot.
fn new_task(
    tasks: &mut Vec<TaskRow>,
    task_keys: &mut Vec<Key>,
    free: &mut Vec<u32>,
    me: u32,
    cold: &NodeCold,
    key: Option<Key>,
) -> u32 {
    let row = TaskRow {
        memory: cold.memory,
        runner: me,
        first: me,
        last: me,
        passed: 0,
        stamp: 0,
        pool: cold.pool,
        running: false,
        keyed: key.is_some(),
    };
    let key = key.unwrap_or(Key([0; 32]));
    match free.pop() {
        Some(task) => {
            tasks[task as usize] = row;
            task_keys[task as usize] = key;
            task
        }
        None => {
            tasks.push(row);
            task_keys.push(key);
            (tasks.len() - 1) as u32
        }
    }
}

/// Takes the ids of a batch refused back out of the index.
fn unindex(index: &mut FxHashMap<NodeId, u32>, nodes: &[Spec]) {
    for spec in nodes {
        index.remove(&spec.id);
    }
}

/// Puts `nodes` in `order`: the node at `i` is then the one that was at `order[i]`. Each place
/// of `order` is marked [`NEW`] as it is filled.
fn permute(nodes: &mut [Spec], order: &mut [u32]) {
    for start in 0..order.len() {
        if order[start] & NEW != 0 {
            continue;
        }
        let mut at = start;
        loop {
            let from = order[at] as usize;
            order[at] |= NEW;
            if from == start {
                break;
            }
            nodes.swap(at, from);
            at = from;
        }
    }
}

/// The nodes that wait for a node, from its first edge on, after `into`'s.
fn push_dependents(into: &mut Vec<u32>, edges: &[Edge], first: u32) {
    let mut edge = first;
    while edge != NONE {
        let it = edges[edge as usize];
        into.push(it.owner);
        edge = it.next;
    }
}

fn sift_up(heap: &mut [Ready], rows: &mut [NodeRow], mut at: usize) {
    let entry = heap[at];
    while at > 0 {
        let parent = (at - 1) / 4;
        if !entry.before(&heap[parent]) {
            break;
        }
        heap[at] = heap[parent];
        rows[heap[at].node as usize].heap_at = at as u32;
        at = parent;
    }
    heap[at] = entry;
    rows[entry.node as usize].heap_at = at as u32;
}

fn sift_down(heap: &mut [Ready], rows: &mut [NodeRow], mut at: usize) {
    let entry = heap[at];
    loop {
        let first = 4 * at + 1;
        if first >= heap.len() {
            break;
        }
        let mut least = first;
        for child in first + 1..(first + 4).min(heap.len()) {
            if heap[child].before(&heap[least]) {
                least = child;
            }
        }
        if !heap[least].before(&entry) {
            break;
        }
        heap[at] = heap[least];
        rows[heap[at].node as usize].heap_at = at as u32;
        at = least;
    }
    heap[at] = entry;
    rows[entry.node as usize].heap_at = at as u32;
}

fn heap_remove(heap: &mut Vec<Ready>, rows: &mut [NodeRow], at: usize) {
    rows[heap[at].node as usize].heap_at = NONE;
    let last = heap.pop().expect("a ready node in its heap");
    if at == heap.len() {
        return;
    }
    heap[at] = last;
    if at > 0 && last.before(&heap[(at - 1) / 4]) {
        sift_up(heap, rows, at);
    } else {
        sift_down(heap, rows, at);
    }
}

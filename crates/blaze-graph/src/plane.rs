//! The plane (docs/v2/plane.md, "The plane"): one scheduler for every request a daemon has under
//! way, as a state machine that does no IO.
//!
//! A request (a build, a test run, an edit) is opened with a weight, adds nodes, and is closed;
//! it finishes once each of its nodes is settled. A node may have a [`Key`], the hash of
//! everything its value depends on. The plane keeps what came of each key that was done
//! (**memo**): a node whose key was done before, by any request, settles at once as cached and
//! runs nothing. A node whose key another node is running or about to run joins it (**single
//! flight**): one task runs, and every node of it settles when it ends.
//!
//! Of the requests with ready work, the next thread of a pool goes to the one using the least
//! of its share (its running tasks over its weight, **fair share**); ties go to the request
//! opened first. Within a request, the ready task with the longest path of expected cost to
//! the end of the request runs first (**critical path**), ties to the node added first.
//!
//! With a [`Budget`], a task starts only when the predicted peaks of what runs (or what the
//! process is said to hold, if more), plus its own, fit in it (**memory admission**). A task
//! passed over for memory more than [`PlaneConfig::starve_after`] times holds back every task
//! behind it until it fits, so a large task is never starved. A task whose prediction alone is
//! more than the budget fails before it starts, saying so; so does one that cannot fit while
//! nothing runs. Nothing is retried with less at once.

use std::{cmp::Reverse, collections::BTreeSet, fmt};

use blaze_protocol::events::{Event, NodeState, Record};
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
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(transparent)]
pub struct Key(pub [u8; 32]);

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
    /// one, then give the plane a [`PlaneInput::Done`] of it.
    Run { id: NodeId, pool: Pool, near: Option<u32> },
    /// Every node of the request is settled. Nothing more comes of it.
    Finished { request: RequestId, ok: bool },
}

/// What the plane has done, for a person and for the tests.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    /// Tasks started.
    pub ran: u64,
    /// Nodes settled from the memo, running nothing.
    pub remembered: u64,
    /// Nodes that joined a task another node had.
    pub joined: u64,
    /// The most memory granted at once.
    pub granted_most: u64,
}

struct Node {
    request: usize,
    spec: Spec,
    state: NodeState,
    /// How many of its deps are not done.
    waiting: usize,
    dependents: Vec<usize>,
    /// The longest path of expected cost from it to the end of its request, its own included.
    priority: u64,
    done_at: Option<u64>,
    /// The dep that was done last when it became ready.
    blocker: Option<usize>,
    task: Option<usize>,
    /// The thread group its task ran on.
    group: Option<u32>,
}

struct Task {
    key: Option<Key>,
    pool: Pool,
    memory: u64,
    /// The node whose id the task runs as, and whose request it counts against.
    runner: usize,
    /// Every node that settles when it does, the runner first.
    members: Vec<usize>,
    running: bool,
    passed: u32,
}

struct Request {
    id: RequestId,
    weight: u32,
    keep_going: bool,
    closed: bool,
    finished: bool,
    /// How many of its nodes are not settled.
    open: usize,
    failed: Option<usize>,
    /// Its ready tasks of each pool, by their runner: the first to run first.
    ready: [BTreeSet<(Reverse<u64>, usize)>; 2],
    running: [usize; 2],
    nodes: Vec<usize>,
}

pub struct Plane {
    config: PlaneConfig,
    nodes: Vec<Node>,
    index: FxHashMap<NodeId, usize>,
    tasks: Vec<Task>,
    requests: Vec<Request>,
    request_index: FxHashMap<RequestId, usize>,
    /// Keys done well: a node of one settles as cached.
    memo: FxHashMap<Key, ()>,
    /// Keys of tasks not yet settled.
    flight: FxHashMap<Key, usize>,
    running: [usize; 2],
    granted: u64,
    held: u64,
    /// Nodes to settle once the move under way is done, as the memo or the budget says, in
    /// order: settling them where they are found would nest without bound.
    deferred: Vec<(usize, NodeState)>,
    last_ns: u64,
    counts: Counts,
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
    pub fn new(config: PlaneConfig) -> Result<Plane, Error> {
        for (pool, threads) in [(Pool::Io, config.io), (Pool::Work, config.work)] {
            if threads == 0 {
                return Err(Error::NoThreads(pool));
            }
        }
        Ok(Plane {
            config,
            nodes: Vec::new(),
            index: FxHashMap::default(),
            tasks: Vec::new(),
            requests: Vec::new(),
            request_index: FxHashMap::default(),
            memo: FxHashMap::default(),
            flight: FxHashMap::default(),
            running: [0; 2],
            granted: 0,
            held: 0,
            deferred: Vec::new(),
            last_ns: 0,
            counts: Counts::default(),
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
        Some(&self.nodes[*self.index.get(&id)?].state)
    }

    /// Whether a value of `key` is remembered.
    pub fn remembers(&self, key: &Key) -> bool {
        self.memo.contains_key(key)
    }

    /// Forgets the value of `key`, as when the store no longer has it: the next node of it
    /// runs.
    pub fn forget(&mut self, key: &Key) {
        self.memo.remove(key);
    }

    pub fn step(&mut self, input: PlaneInput) -> Result<Vec<PlaneOutput>, Error> {
        // A time before the last is the last: completions race the clock that stamps them.
        let at = input.at_ns().max(self.last_ns);
        let mut out = Vec::new();
        let mut touched: Vec<usize> = Vec::new();
        match input {
            PlaneInput::Open { request, weight, keep_going, .. } => {
                if self.request_index.contains_key(&request) {
                    return Err(Error::RequestTwice(request.0));
                }
                if weight == 0 {
                    return Err(Error::NoWeight(request.0));
                }
                self.request_index.insert(request, self.requests.len());
                self.requests.push(Request {
                    id: request,
                    weight,
                    keep_going,
                    closed: false,
                    finished: false,
                    open: 0,
                    failed: None,
                    ready: [BTreeSet::new(), BTreeSet::new()],
                    running: [0; 2],
                    nodes: Vec::new(),
                });
            }
            PlaneInput::Add { request, nodes, .. } => {
                let r = self.open_request(request)?;
                if self.requests[r].closed {
                    return Err(Error::Closed);
                }
                let nodes = self.check(r, nodes)?;
                self.add(at, r, nodes, &mut out);
                touched.push(r);
            }
            PlaneInput::Close { request, .. } => {
                let r = self.open_request(request)?;
                if self.requests[r].closed {
                    return Err(Error::Closed);
                }
                self.requests[r].closed = true;
                touched.push(r);
            }
            PlaneInput::Done { id, outcome, added, group, .. } => {
                let task = match self.index.get(&id) {
                    Some(&node)
                        if self.nodes[node].state == NodeState::Running
                            && self.nodes[node].task.is_some_and(|it| {
                                self.tasks[it].running && self.tasks[it].runner == node
                            }) =>
                    {
                        self.nodes[node].task.expect("a running node's task")
                    }
                    _ => return Err(Error::NotRunning(id)),
                };
                let runner = self.tasks[task].runner;
                let r = self.nodes[runner].request;
                let added = self.check(r, added)?;
                self.finish_task(at, task, outcome, group, &mut out, &mut touched);
                self.add(at, r, added, &mut out);
                touched.push(r);
            }
            PlaneInput::Held { bytes, .. } => self.held = bytes,
        }
        self.settle_deferred(at, &mut out, &mut touched);
        self.last_ns = at;
        self.dispatch(at, &mut out, &mut touched);
        touched.sort_unstable();
        touched.dedup();
        for r in touched {
            let request = &self.requests[r];
            if request.closed && request.open == 0 && !request.finished {
                self.requests[r].finished = true;
                let path =
                    self.critical(r).into_iter().map(|it| self.nodes[it].spec.id.0).collect();
                let id = self.requests[r].id;
                out.push(said(id, at, Event::Critical { path }));
                let ok = self.requests[r].failed.is_none();
                out.push(PlaneOutput::Finished { request: id, ok });
            }
        }
        Ok(out)
    }

    fn open_request(&self, request: RequestId) -> Result<usize, Error> {
        match self.request_index.get(&request) {
            Some(&r) if !self.requests[r].finished => Ok(r),
            Some(_) => Err(Error::Over),
            None => Err(Error::UnknownRequest(request.0)),
        }
    }

    /// `nodes`, each after its deps of them, or why they cannot be added to request `r`.
    /// Nothing of them is added yet.
    fn check(&self, r: usize, nodes: Vec<Spec>) -> Result<Vec<Spec>, Error> {
        let mut new: FxHashMap<NodeId, usize> = FxHashMap::default();
        for (at, spec) in nodes.iter().enumerate() {
            if self.index.contains_key(&spec.id) || new.insert(spec.id, at).is_some() {
                return Err(Error::Twice(spec.id));
            }
        }
        // Kahn's order over the new nodes, the first given first among those that can go.
        let mut waiting = vec![0usize; nodes.len()];
        let mut then: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
        for (at, spec) in nodes.iter().enumerate() {
            for dep in &spec.deps {
                match new.get(dep) {
                    Some(&it) => {
                        waiting[at] += 1;
                        then[it].push(at);
                    }
                    None if self.index.get(dep).is_some_and(|&it| self.nodes[it].request == r) => {}
                    None => return Err(Error::UnknownDep { node: spec.id, dep: *dep }),
                }
            }
        }
        let mut can: BTreeSet<usize> = (0..nodes.len()).filter(|&it| waiting[it] == 0).collect();
        let mut order = Vec::with_capacity(nodes.len());
        while let Some(at) = can.pop_first() {
            order.push(at);
            for &next in &then[at] {
                waiting[next] -= 1;
                if waiting[next] == 0 {
                    can.insert(next);
                }
            }
        }
        if order.len() < nodes.len() {
            let mut cycle: Vec<NodeId> =
                (0..nodes.len()).filter(|&it| waiting[it] > 0).map(|it| nodes[it].id).collect();
            cycle.sort();
            return Err(Error::Cycle(cycle));
        }
        let mut nodes: Vec<Option<Spec>> = nodes.into_iter().map(Some).collect();
        Ok(order.into_iter().filter_map(|at| nodes[at].take()).collect())
    }

    fn add(&mut self, at: u64, r: usize, nodes: Vec<Spec>, out: &mut Vec<PlaneOutput>) {
        if nodes.is_empty() {
            return;
        }
        for spec in nodes {
            let me = self.nodes.len();
            let deps: Vec<usize> = spec.deps.iter().map(|it| self.index[it]).collect();
            let mut waiting = 0;
            let mut skipped = None;
            let mut blocker: Option<usize> = None;
            for &dep in &deps {
                match &self.nodes[dep].state {
                    NodeState::Built | NodeState::Cached { .. } => {
                        let later = |it: usize| (self.nodes[it].done_at, it);
                        if blocker.is_none_or(|it| later(dep) > later(it)) {
                            blocker = Some(dep);
                        }
                    }
                    NodeState::Failed { .. } => _ = skipped.get_or_insert(dep),
                    NodeState::Skipped { because } => {
                        _ = skipped.get_or_insert(self.index[&NodeId(*because)])
                    }
                    _ => waiting += 1,
                }
            }
            for &dep in &deps {
                self.nodes[dep].dependents.push(me);
            }
            if !self.requests[r].keep_going {
                skipped = skipped.or(self.requests[r].failed);
            }
            let id = self.requests[r].id;
            out.push(said(
                id,
                at,
                Event::Node {
                    id: spec.id.0,
                    kind: spec.kind,
                    name: spec.name.clone(),
                    deps: spec.deps.iter().map(|it| it.0).collect(),
                },
            ));
            self.index.insert(spec.id, me);
            let priority = spec.cost;
            self.nodes.push(Node {
                request: r,
                spec,
                state: NodeState::Waiting,
                waiting,
                dependents: Vec::new(),
                priority,
                done_at: None,
                blocker,
                task: None,
                group: None,
            });
            self.requests[r].nodes.push(me);
            self.requests[r].open += 1;
            match skipped {
                Some(because) => {
                    let because = self.nodes[because].spec.id.0;
                    self.moved(at, me, NodeState::Skipped { because }, out);
                }
                None if waiting == 0 => self.became_ready(at, me, out),
                None => self.moved(at, me, NodeState::Waiting, out),
            }
        }
        // A node is added after its deps, so from the last to the first each one's dependents
        // have their priority before it. Only the request's nodes can change.
        let request_nodes = std::mem::take(&mut self.requests[r].nodes);
        for &me in request_nodes.iter().rev() {
            let node = &self.nodes[me];
            let after = node.dependents.iter().map(|&it| self.nodes[it].priority).max();
            let priority = node.spec.cost.saturating_add(after.unwrap_or(0));
            self.nodes[me].priority = priority;
        }
        self.requests[r].nodes = request_nodes;
        for ready in &mut self.requests[r].ready {
            *ready = ready.iter().map(|&(_, it)| (Reverse(self.nodes[it].priority), it)).collect();
        }
    }

    /// Says `me` is now `state`, and keeps what follows from it for its request.
    fn moved(&mut self, at: u64, me: usize, state: NodeState, out: &mut Vec<PlaneOutput>) {
        let node = &mut self.nodes[me];
        if state.settled() {
            self.requests[node.request].open -= 1;
        }
        node.state = state.clone();
        let id = self.requests[node.request].id;
        out.push(said(id, at, Event::Moved { id: node.spec.id.0, state }));
    }

    /// `me`'s deps are done: it is remembered, joins a task of its key, or is a task of its
    /// own, ready to run.
    fn became_ready(&mut self, at: u64, me: usize, out: &mut Vec<PlaneOutput>) {
        let key = self.nodes[me].spec.key;
        if let Some(key) = key {
            if self.memo.contains_key(&key) {
                self.counts.remembered += 1;
                self.deferred.push((me, NodeState::Cached { remote: false }));
                return;
            }
            if let Some(&task) = self.flight.get(&key) {
                self.counts.joined += 1;
                self.tasks[task].members.push(me);
                self.nodes[me].task = Some(task);
                let state =
                    if self.tasks[task].running { NodeState::Running } else { NodeState::Ready };
                self.moved(at, me, state, out);
                return;
            }
        }
        let spec = &self.nodes[me].spec;
        if let Some(budget) = &self.config.budget
            && spec.memory > budget.bytes
        {
            let why = format!(
                "{} needs {} of memory as predicted, more than blaze may use here: {} ({})",
                spec.name,
                gb(spec.memory),
                gb(budget.bytes),
                budget.from
            );
            self.deferred.push((me, NodeState::Failed { why }));
            return;
        }
        let task = self.tasks.len();
        self.tasks.push(Task {
            key,
            pool: spec.pool,
            memory: spec.memory,
            runner: me,
            members: vec![me],
            running: false,
            passed: 0,
        });
        if let Some(key) = key {
            self.flight.insert(key, task);
        }
        self.nodes[me].task = Some(task);
        let (r, pool, priority) =
            (self.nodes[me].request, pool_at(self.tasks[task].pool), self.nodes[me].priority);
        self.requests[r].ready[pool].insert((Reverse(priority), me));
        self.moved(at, me, NodeState::Ready, out);
    }

    fn settle_deferred(&mut self, at: u64, out: &mut Vec<PlaneOutput>, touched: &mut Vec<usize>) {
        let mut next = 0;
        while next < self.deferred.len() {
            let (me, state) = self.deferred[next].clone();
            next += 1;
            // Skipped by a failure of the same step before it was settled.
            if self.nodes[me].state.settled() {
                continue;
            }
            touched.push(self.nodes[me].request);
            self.settle(at, me, state, out);
        }
        self.deferred.clear();
    }

    fn settle(&mut self, at: u64, me: usize, state: NodeState, out: &mut Vec<PlaneOutput>) {
        let failed = matches!(state, NodeState::Failed { .. });
        self.nodes[me].done_at = Some(at);
        self.moved(at, me, state, out);
        let because = self.nodes[me].spec.id.0;
        if !failed {
            for at_dependent in 0..self.nodes[me].dependents.len() {
                let dependent = self.nodes[me].dependents[at_dependent];
                let node = &mut self.nodes[dependent];
                node.waiting -= 1;
                if node.waiting == 0 && node.state == NodeState::Waiting {
                    node.blocker = Some(me);
                    self.became_ready(at, dependent, out);
                }
            }
            return;
        }
        let r = self.nodes[me].request;
        self.requests[r].failed.get_or_insert(me);
        let mut skipping: Vec<usize> = self.nodes[me].dependents.clone();
        if !self.requests[r].keep_going {
            skipping.extend(self.requests[r].nodes.iter().copied());
        }
        let mut at_skipping = 0;
        while let Some(&it) = skipping.get(at_skipping) {
            at_skipping += 1;
            if matches!(self.nodes[it].state, NodeState::Waiting | NodeState::Ready) {
                self.leave_task(it);
                self.moved(at, it, NodeState::Skipped { because }, out);
                skipping.extend(self.nodes[it].dependents.iter().copied());
            }
        }
    }

    /// `me`, which is not running, no longer waits for its task: a task nobody waits for is
    /// dropped, and one whose runner it was runs as another of its nodes.
    fn leave_task(&mut self, me: usize) {
        let Some(task) = self.nodes[me].task.take() else { return };
        let t = &mut self.tasks[task];
        t.members.retain(|&it| it != me);
        if t.running || t.runner != me {
            return;
        }
        let (pool, r) = (pool_at(t.pool), self.nodes[me].request);
        self.requests[r].ready[pool].remove(&(Reverse(self.nodes[me].priority), me));
        match self.tasks[task].members.first().copied() {
            Some(next) => {
                self.tasks[task].runner = next;
                let r = self.nodes[next].request;
                self.requests[r].ready[pool].insert((Reverse(self.nodes[next].priority), next));
            }
            None => {
                if let Some(key) = self.tasks[task].key {
                    self.flight.remove(&key);
                }
            }
        }
    }

    fn finish_task(
        &mut self,
        at: u64,
        task: usize,
        outcome: Outcome,
        group: Option<u32>,
        out: &mut Vec<PlaneOutput>,
        touched: &mut Vec<usize>,
    ) {
        let t = &mut self.tasks[task];
        t.running = false;
        let (pool, memory, runner, key) = (pool_at(t.pool), t.memory, t.runner, t.key);
        let members = std::mem::take(&mut t.members);
        self.running[pool] -= 1;
        self.requests[self.nodes[runner].request].running[pool] -= 1;
        self.granted -= memory;
        if let Some(key) = key {
            self.flight.remove(&key);
            if !matches!(outcome, Outcome::Failed { .. }) {
                self.memo.insert(key, ());
            }
        }
        let state = match outcome {
            Outcome::Built => NodeState::Built,
            Outcome::Cached { remote } => NodeState::Cached { remote },
            Outcome::Failed { why } => NodeState::Failed { why },
        };
        for member in members {
            self.nodes[member].group = group;
            self.nodes[member].task = None;
            touched.push(self.nodes[member].request);
            // A member's request may have settled it as skipped while it ran: not for a node
            // that runs, which settles when its task does.
            if !self.nodes[member].state.settled() {
                self.settle(at, member, state.clone(), out);
            }
        }
    }

    fn memory_free(&self) -> Option<u64> {
        let budget = self.config.budget.as_ref()?;
        Some(budget.bytes.saturating_sub(self.granted.max(self.held)))
    }

    fn dispatch(&mut self, at: u64, out: &mut Vec<PlaneOutput>, touched: &mut Vec<usize>) {
        for pool in [Pool::Io, Pool::Work] {
            let (p, limit) = (pool_at(pool), self.threads(pool));
            'next: while self.running[p] < limit {
                let mut order: Vec<usize> =
                    (0..self.requests.len()).filter(|&r| !self.requests[r].ready[p].is_empty()).collect();
                // The least used of its share first: running / weight, compared without division.
                order.sort_by(|&a, &b| {
                    let (a_, b_) = (&self.requests[a], &self.requests[b]);
                    let used_a = a_.running[p] as u128 * u128::from(b_.weight);
                    let used_b = b_.running[p] as u128 * u128::from(a_.weight);
                    used_a.cmp(&used_b).then(a.cmp(&b))
                });
                for r in order {
                    match self.pick(r, p) {
                        Pick::Start(me) => {
                            self.start(at, me, out);
                            continue 'next;
                        }
                        Pick::Fail(me, why) => {
                            self.leave_task(me);
                            touched.push(r);
                            self.deferred.push((me, NodeState::Failed { why }));
                            self.settle_deferred(at, out, touched);
                            continue 'next;
                        }
                        Pick::Held => break 'next,
                        Pick::None => {}
                    }
                }
                break;
            }
        }
    }

    fn threads(&self, pool: Pool) -> usize {
        match pool {
            Pool::Io => self.config.io,
            Pool::Work => self.config.work,
        }
    }

    /// The task of request `r` to start next on pool `p`, as memory allows.
    fn pick(&mut self, r: usize, p: usize) -> Pick {
        let Some(free) = self.memory_free() else {
            return match self.requests[r].ready[p].first() {
                Some(&(_, me)) => Pick::Start(me),
                None => Pick::None,
            };
        };
        let mut passed = Vec::new();
        let mut found = None;
        for &(_, me) in &self.requests[r].ready[p] {
            let task = &self.tasks[self.nodes[me].task.expect("a ready node's task")];
            if task.memory <= free {
                found = Some(me);
                break;
            }
            if task.passed >= self.config.starve_after {
                // It waited long enough: nothing behind it starts until it fits.
                found = None;
                passed.clear();
                passed.push(me);
                break;
            }
            passed.push(me);
        }
        let nothing_runs = self.running == [0, 0];
        match found {
            Some(me) => {
                for it in passed {
                    let task = self.nodes[it].task.expect("a ready node's task");
                    self.tasks[task].passed += 1;
                }
                Pick::Start(me)
            }
            None if nothing_runs && !passed.is_empty() => {
                let me = passed[0];
                let budget = self.config.budget.as_ref().expect("a budget");
                let spec = &self.nodes[me].spec;
                Pick::Fail(
                    me,
                    format!(
                        "{} needs {} of memory as predicted, and with nothing of blaze's running \
                         the process holds {} of the {} it may use ({})",
                        spec.name,
                        gb(spec.memory),
                        gb(self.held),
                        gb(budget.bytes),
                        budget.from
                    ),
                )
            }
            None if passed.iter().any(|&it| {
                self.tasks[self.nodes[it].task.expect("a ready node's task")].passed
                    >= self.config.starve_after
            }) =>
            {
                Pick::Held
            }
            None => Pick::None,
        }
    }

    fn start(&mut self, at: u64, me: usize, out: &mut Vec<PlaneOutput>) {
        let task = self.nodes[me].task.expect("a ready node's task");
        let (p, r) = (pool_at(self.tasks[task].pool), self.nodes[me].request);
        self.requests[r].ready[p].remove(&(Reverse(self.nodes[me].priority), me));
        self.tasks[task].running = true;
        self.running[p] += 1;
        self.requests[r].running[p] += 1;
        self.granted += self.tasks[task].memory;
        self.counts.granted_most = self.counts.granted_most.max(self.granted);
        self.counts.ran += 1;
        for member in self.tasks[task].members.clone() {
            self.moved(at, member, NodeState::Running, out);
        }
        let near = self.nodes[me].blocker.and_then(|it| self.nodes[it].group);
        out.push(PlaneOutput::Run { id: self.nodes[me].spec.id, pool: self.tasks[task].pool, near });
    }

    /// From request `r`'s node that settled last, back through what each one waited for last.
    fn critical(&self, r: usize) -> Vec<usize> {
        let nodes = &self.requests[r].nodes;
        let last = nodes
            .iter()
            .copied()
            .filter(|&it| self.nodes[it].done_at.is_some())
            .max_by_key(|&it| (self.nodes[it].done_at, Reverse(it)));
        let mut path: Vec<usize> =
            std::iter::successors(last, |&it| self.nodes[it].blocker).collect();
        path.reverse();
        path
    }
}

enum Pick {
    Start(usize),
    /// It cannot fit even with nothing running.
    Fail(usize, String),
    /// A task held for memory: nothing of this pool starts until it fits.
    Held,
    None,
}

fn said(request: RequestId, at_ns: u64, event: Event) -> PlaneOutput {
    PlaneOutput::Said { request, record: Record { at_ns, event } }
}

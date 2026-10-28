//! The build's graph as a state machine (docs/driver.md, "The step function").
//!
//! [`Machine::step`] takes an [`Input`]: nodes added, no more to be added, or what came of a
//! node it asked to run. It gives back [`Output`]s: the nodes to run now, the records of
//! [`blaze_protocol::events`] that say every node and every move, and the end of the build. It
//! does nothing else: the time is in the input, and the same inputs give the same outputs.
//!
//! Of the nodes that are ready, the one with the longest path of expected cost from it to the
//! end of the build runs first, so what the build waits for longest is never behind what it
//! does not wait for; ties go to the node added first.

use std::{cmp::Reverse, collections::BTreeSet, fmt};

use blaze_protocol::events::{Event, NodeKind, NodeState, Record};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

/// A node's name in the graph, chosen by whoever adds it.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(transparent)]
pub struct NodeId(pub u32);

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// Where a node's work is done: [`Pool::Io`] waits on files, the network or a process;
/// [`Pool::Work`] uses a core.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Pool {
    Io,
    Work,
}

impl Pool {
    fn at(self) -> usize {
        match self {
            Pool::Io => 0,
            Pool::Work => 1,
        }
    }
}

/// A node to add. Its `deps` are added with it, in the same input in any order, or before.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    pub id: NodeId,
    pub kind: NodeKind,
    pub name: String,
    pub deps: Vec<NodeId>,
    pub pool: Pool,
    /// What it is expected to cost, in a unit that is the same for every node of the build:
    /// it orders the ready nodes and nothing else.
    pub cost: u64,
}

/// What came of a node's work.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Built,
    Cached { remote: bool },
    Failed { why: String },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// Nodes the build has besides those it had.
    Add { at_ns: u64, nodes: Vec<Spec> },
    /// No more nodes are added but by [`Input::Done`]: the build ends once every node is
    /// settled.
    Close { at_ns: u64 },
    /// A node the machine asked to run is done, and its work found the nodes `added`.
    Done { at_ns: u64, id: NodeId, outcome: Outcome, added: Vec<Spec> },
}

impl Input {
    pub fn at_ns(&self) -> u64 {
        match self {
            Input::Add { at_ns, .. } | Input::Close { at_ns } | Input::Done { at_ns, .. } => {
                *at_ns
            }
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum Output {
    /// A record of the build: a node, a move, the critical path.
    Said(Record),
    /// Run `id`'s work on `pool`, then give the machine an [`Input::Done`] of it.
    Run { id: NodeId, pool: Pool },
    /// Every node is settled. Nothing more comes.
    Finished { ok: bool },
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// How many nodes of [`Pool::Io`] run at once.
    pub io: usize,
    /// How many nodes of [`Pool::Work`] run at once: the cores.
    pub work: usize,
    /// Whether nodes that do not wait for a failed one still run after it failed. When not,
    /// the build ends once what runs is done, as cargo's does.
    pub keep_going: bool,
}

/// An input the machine cannot take. The machine is as it was before it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    NoThreads(Pool),
    Twice(NodeId),
    UnknownDep { node: NodeId, dep: NodeId },
    /// Nodes of one input that wait for one another.
    Cycle(Vec<NodeId>),
    NotRunning(NodeId),
    Closed,
    Over,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NoThreads(pool) => write!(f, "the {pool:?} pool runs no node at once"),
            Error::Twice(id) => write!(f, "node {id} is added twice"),
            Error::UnknownDep { node, dep } => {
                write!(f, "node {node} waits for {dep}, which is not added")
            }
            Error::Cycle(nodes) => {
                let nodes: Vec<String> = nodes.iter().map(ToString::to_string).collect();
                write!(f, "nodes {} wait for one another", nodes.join(", "))
            }
            Error::NotRunning(id) => write!(f, "node {id} is said done, but it is not running"),
            Error::Closed => write!(f, "nodes added or the build closed after it was closed"),
            Error::Over => write!(f, "an input after the build finished"),
        }
    }
}

impl std::error::Error for Error {}

struct Node {
    spec: Spec,
    state: NodeState,
    /// How many of its deps are not done.
    waiting: usize,
    dependents: Vec<usize>,
    /// The longest path of expected cost from it to the end of the build, its own included.
    priority: u64,
    /// When it settled, of a node that ran.
    done_at: Option<u64>,
    /// The dep that was done last when it became ready.
    blocker: Option<usize>,
}

pub struct Machine {
    config: Config,
    nodes: Vec<Node>,
    index: FxHashMap<NodeId, usize>,
    /// The ready nodes of each pool, first the one to run first.
    ready: [BTreeSet<(Reverse<u64>, usize)>; 2],
    running: [usize; 2],
    /// How many nodes are not settled.
    open: usize,
    closed: bool,
    failed: Option<usize>,
    over: bool,
    last_ns: u64,
}

impl Machine {
    pub fn new(config: Config) -> Result<Machine, Error> {
        for (pool, threads) in [(Pool::Io, config.io), (Pool::Work, config.work)] {
            if threads == 0 {
                return Err(Error::NoThreads(pool));
            }
        }
        Ok(Machine {
            config,
            nodes: Vec::new(),
            index: FxHashMap::default(),
            ready: [BTreeSet::new(), BTreeSet::new()],
            running: [0; 2],
            open: 0,
            closed: false,
            failed: None,
            over: false,
            last_ns: 0,
        })
    }

    pub fn config(&self) -> Config {
        self.config
    }

    /// The state of `id`, if it was added.
    pub fn state(&self, id: NodeId) -> Option<&NodeState> {
        Some(&self.nodes[*self.index.get(&id)?].state)
    }

    pub fn is_over(&self) -> bool {
        self.over
    }

    pub fn step(&mut self, input: Input) -> Result<Vec<Output>, Error> {
        if self.over {
            return Err(Error::Over);
        }
        // A time before the last is the last: completions race the clock that stamps them.
        let at = input.at_ns().max(self.last_ns);
        let mut out = Vec::new();
        match input {
            Input::Add { nodes, .. } => {
                if self.closed {
                    return Err(Error::Closed);
                }
                let nodes = self.check(nodes)?;
                self.add(at, nodes, &mut out);
            }
            Input::Close { .. } => {
                if self.closed {
                    return Err(Error::Closed);
                }
                self.closed = true;
            }
            Input::Done { id, outcome, added, .. } => {
                let at_node = match self.index.get(&id) {
                    Some(&it) if self.nodes[it].state == NodeState::Running => it,
                    _ => return Err(Error::NotRunning(id)),
                };
                let added = self.check(added)?;
                self.running[self.nodes[at_node].spec.pool.at()] -= 1;
                let state = match outcome {
                    Outcome::Built => NodeState::Built,
                    Outcome::Cached { remote } => NodeState::Cached { remote },
                    Outcome::Failed { why } => NodeState::Failed { why },
                };
                self.settle(at, at_node, state, &mut out);
                self.add(at, added, &mut out);
            }
        }
        self.last_ns = at;
        self.dispatch(at, &mut out);
        if self.closed && self.open == 0 {
            self.over = true;
            let path = self.critical().into_iter().map(|it| self.nodes[it].spec.id.0).collect();
            out.push(said(at, Event::Critical { path }));
            out.push(Output::Finished { ok: self.failed.is_none() });
        }
        Ok(out)
    }

    /// `nodes`, each after its deps of them, or why they cannot be added. Nothing of them is
    /// added yet.
    fn check(&self, nodes: Vec<Spec>) -> Result<Vec<Spec>, Error> {
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
                    None if self.index.contains_key(dep) => {}
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

    fn add(&mut self, at: u64, nodes: Vec<Spec>, out: &mut Vec<Output>) {
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
            if !self.config.keep_going {
                skipped = skipped.or(self.failed);
            }
            out.push(said(
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
                spec,
                state: NodeState::Waiting,
                waiting,
                dependents: Vec::new(),
                priority,
                done_at: None,
                blocker,
            });
            self.open += 1;
            match skipped {
                Some(because) => {
                    let because = self.nodes[because].spec.id.0;
                    self.moved(at, me, NodeState::Skipped { because }, out);
                }
                None if waiting == 0 => self.moved(at, me, NodeState::Ready, out),
                None => self.moved(at, me, NodeState::Waiting, out),
            }
        }
        // A node is added after its deps, so from the last to the first each one's dependents
        // have their priority before it.
        for me in (0..self.nodes.len()).rev() {
            let node = &self.nodes[me];
            let after = node.dependents.iter().map(|&it| self.nodes[it].priority).max();
            let priority = node.spec.cost.saturating_add(after.unwrap_or(0));
            self.nodes[me].priority = priority;
        }
        for ready in &mut self.ready {
            *ready = ready.iter().map(|&(_, it)| (Reverse(self.nodes[it].priority), it)).collect();
        }
    }

    /// Says `me` is now `state`, and keeps what follows from it.
    fn moved(&mut self, at: u64, me: usize, state: NodeState, out: &mut Vec<Output>) {
        let node = &mut self.nodes[me];
        if state == NodeState::Ready {
            self.ready[node.spec.pool.at()].insert((Reverse(node.priority), me));
        }
        if state.settled() {
            self.open -= 1;
        }
        node.state = state.clone();
        out.push(said(at, Event::Moved { id: node.spec.id.0, state }));
    }

    fn settle(&mut self, at: u64, me: usize, state: NodeState, out: &mut Vec<Output>) {
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
                    self.moved(at, dependent, NodeState::Ready, out);
                }
            }
            return;
        }
        self.failed.get_or_insert(me);
        let mut skipping: Vec<usize> = self.nodes[me].dependents.clone();
        if !self.config.keep_going {
            skipping.extend(0..self.nodes.len());
        }
        let mut at_skipping = 0;
        while let Some(&it) = skipping.get(at_skipping) {
            at_skipping += 1;
            if matches!(self.nodes[it].state, NodeState::Waiting | NodeState::Ready) {
                let pool = self.nodes[it].spec.pool.at();
                self.ready[pool].remove(&(Reverse(self.nodes[it].priority), it));
                self.moved(at, it, NodeState::Skipped { because }, out);
                skipping.extend(self.nodes[it].dependents.iter().copied());
            }
        }
    }

    fn dispatch(&mut self, at: u64, out: &mut Vec<Output>) {
        for (pool, limit) in [(Pool::Io, self.config.io), (Pool::Work, self.config.work)] {
            while self.running[pool.at()] < limit
                && let Some((_, me)) = self.ready[pool.at()].pop_first()
            {
                self.running[pool.at()] += 1;
                self.moved(at, me, NodeState::Running, out);
                out.push(Output::Run { id: self.nodes[me].spec.id, pool });
            }
        }
    }

    /// From the node that ran and settled last, back through what each one waited for last.
    fn critical(&self) -> Vec<usize> {
        let last = (0..self.nodes.len())
            .filter(|&it| self.nodes[it].done_at.is_some())
            .max_by_key(|&it| (self.nodes[it].done_at, Reverse(it)));
        let mut path: Vec<usize> =
            std::iter::successors(last, |&it| self.nodes[it].blocker).collect();
        path.reverse();
        path
    }
}

fn said(at_ns: u64, event: Event) -> Output {
    Output::Said(Record { at_ns, event })
}

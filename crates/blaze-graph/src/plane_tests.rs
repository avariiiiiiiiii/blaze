//! The plane's tests (docs/v2/plane.md): what each rule does alone, the plane as the machine
//! with one request, and random requests under a seeded generator, checked after every step.

use blaze_protocol::events::{Event, NodeKind, NodeState, Record};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
    machine::{Config, Error, Input, Machine, NodeId, Outcome, Output, Pool, Spec},
    plane::{Budget, Key, Plane, PlaneConfig, PlaneInput, PlaneOutput, RequestId},
    trace::{Trace, render},
};

/// SplitMix64, so the simulation needs no dependency and every seed is the same run.
struct SplitMix(u64);

impl SplitMix {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n.max(1)
    }

    fn one_in(&mut self, n: u64) -> bool {
        self.below(n) == 0
    }
}

fn key(k: u8) -> Key {
    Key([k; 32])
}

fn name(id: u32) -> String {
    format!("<{id}>")
}

fn node(id: u32, deps: &[u32], pool: Pool, cost: u64, key: Option<Key>, memory: u64) -> Spec {
    Spec {
        id: NodeId(id),
        kind: NodeKind::Codegen,
        name: name(id),
        deps: deps.iter().map(|&it| NodeId(it)).collect(),
        pool,
        cost,
        key,
        memory,
    }
}

fn work(id: u32, deps: &[u32], cost: u64) -> Spec {
    node(id, deps, Pool::Work, cost, None, 0)
}

fn keyed(id: u32, deps: &[u32], cost: u64, k: u8) -> Spec {
    node(id, deps, Pool::Work, cost, Some(key(k)), 0)
}

fn config(io: usize, work: usize) -> PlaneConfig {
    PlaneConfig { io, work, budget: None, starve_after: 0 }
}

fn budget(bytes: u64) -> Option<Budget> {
    Some(Budget { bytes, from: "the test's budget".to_owned() })
}

fn open(plane: &mut Plane, request: u32, weight: u32, keep_going: bool) {
    let input = PlaneInput::Open { at_ns: 0, request: RequestId(request), weight, keep_going };
    assert_eq!(plane.step(input).unwrap(), []);
}

fn add(plane: &mut Plane, request: u32, nodes: Vec<Spec>) -> Vec<PlaneOutput> {
    plane.step(PlaneInput::Add { at_ns: 0, request: RequestId(request), nodes }).unwrap()
}

fn close(plane: &mut Plane, request: u32) -> Vec<PlaneOutput> {
    plane.step(PlaneInput::Close { at_ns: 0, request: RequestId(request) }).unwrap()
}

fn done(id: u32, outcome: Outcome, added: Vec<Spec>) -> PlaneInput {
    PlaneInput::Done { at_ns: 0, id: NodeId(id), outcome, added, group: None, peak: 0 }
}

fn built(plane: &mut Plane, id: u32) -> Vec<PlaneOutput> {
    plane.step(done(id, Outcome::Built, Vec::new())).unwrap()
}

fn runs(outputs: &[PlaneOutput]) -> Vec<u32> {
    outputs
        .iter()
        .filter_map(|it| match it {
            PlaneOutput::Run { id, .. } => Some(id.0),
            _ => None,
        })
        .collect()
}

fn finished(outputs: &[PlaneOutput]) -> Vec<(u32, bool)> {
    outputs
        .iter()
        .filter_map(|it| match it {
            PlaneOutput::Finished { request, ok } => Some((request.0, *ok)),
            _ => None,
        })
        .collect()
}

fn moves(outputs: &[PlaneOutput]) -> Vec<(u32, NodeState)> {
    outputs
        .iter()
        .filter_map(|it| match it {
            PlaneOutput::Said { record: Record { event: Event::Moved { id, state }, .. }, .. } => {
                Some((*id, state.clone()))
            }
            _ => None,
        })
        .collect()
}

/// The plane does no IO and reads no clock: what it knows comes in its inputs.
#[test]
fn the_plane_does_no_io() {
    let source = include_str!("plane.rs");
    for word in [
        "std::fs",
        "std::net",
        "std::process",
        "std::thread",
        "std::time",
        "std::env",
        "Instant",
        "SystemTime",
        "tokio",
        "crossbeam",
        "File::",
    ] {
        assert!(!source.contains(word), "plane.rs names `{word}`");
    }
}

/// The plane's outputs as the machine says them.
fn as_machine(outputs: Vec<PlaneOutput>) -> Vec<Output> {
    outputs
        .into_iter()
        .map(|it| match it {
            PlaneOutput::Said { request, record } => {
                assert_eq!(request, RequestId(0));
                Output::Said(record)
            }
            PlaneOutput::Run { id, pool, .. } => Output::Run { id, pool },
            PlaneOutput::Finished { request, ok } => {
                assert_eq!(request, RequestId(0));
                Output::Finished { ok }
            }
        })
        .collect()
}

/// The machine's input as the plane's, of request 0.
fn as_plane(input: Input) -> PlaneInput {
    let request = RequestId(0);
    match input {
        Input::Add { at_ns, nodes } => PlaneInput::Add { at_ns, request, nodes },
        Input::Close { at_ns } => PlaneInput::Close { at_ns, request },
        Input::Done { at_ns, id, outcome, added } => {
            PlaneInput::Done { at_ns, id, outcome, added, group: None, peak: 0 }
        }
    }
}

fn one_request(config: Config) -> Plane {
    let mut plane = Plane::new(PlaneConfig {
        io: config.io,
        work: config.work,
        budget: None,
        starve_after: 0,
    })
    .unwrap();
    let input = PlaneInput::Open {
        at_ns: 0,
        request: RequestId(0),
        weight: 1,
        keep_going: config.keep_going,
    };
    assert_eq!(plane.step(input).unwrap(), []);
    plane
}

/// With one request, no keys and no budget, the plane says what the machine says, step by
/// step: the traces of `traces/` and random builds with failures and nodes found as they go.
#[test]
fn one_request_without_keys_or_budget_says_what_the_machine_says() {
    let trace = Trace::read(include_str!("../traces/cold-small.jsonl")).unwrap();
    let mut plane = one_request(trace.config);
    let mut outputs = Vec::new();
    for input in &trace.inputs {
        outputs.extend(as_machine(plane.step(as_plane(input.clone())).unwrap()));
    }
    assert_eq!(render(&outputs), include_str!("../traces/cold-small.out"));

    for seed in 0..300u64 {
        let mut random = SplitMix(seed);
        let config = Config {
            io: 1 + random.below(3) as usize,
            work: 1 + random.below(4) as usize,
            keep_going: random.one_in(2),
        };
        let mut machine = Machine::new(config).unwrap();
        let mut plane = one_request(config);
        let mut next = 0u32;
        let mut ids: Vec<u32> = Vec::new();
        let mut make = |random: &mut SplitMix, ids: &mut Vec<u32>, count: u64| {
            let mut nodes = Vec::new();
            for _ in 0..count {
                let mut deps: Vec<u32> = (0..random.below(4))
                    .filter(|_| !ids.is_empty())
                    .map(|_| ids[random.below(ids.len() as u64) as usize])
                    .collect();
                deps.sort_unstable();
                deps.dedup();
                let pool = if random.one_in(3) { Pool::Io } else { Pool::Work };
                nodes.push(node(next, &deps, pool, random.below(10), None, 0));
                ids.push(next);
                next += 1;
            }
            nodes
        };
        let mut running: Vec<NodeId> = Vec::new();
        let mut same = |input: Input, running: &mut Vec<NodeId>| -> bool {
            let want = machine.step(input.clone()).unwrap();
            let got = as_machine(plane.step(as_plane(input)).unwrap());
            assert_eq!(render(&got), render(&want), "seed {seed}");
            assert_eq!(got, want, "seed {seed}");
            for output in &want {
                if let Output::Run { id, .. } = output {
                    running.push(*id);
                }
            }
            want.iter().any(|it| matches!(it, Output::Finished { .. }))
        };
        let count = 1 + random.below(30);
        let first = make(&mut random, &mut ids, count);
        let mut over = same(Input::Add { at_ns: 0, nodes: first }, &mut running);
        over |= same(Input::Close { at_ns: 0 }, &mut running);
        let mut at = 0;
        while !over {
            assert!(!running.is_empty(), "seed {seed}: nothing runs and the build is not over");
            let id = running.swap_remove(random.below(running.len() as u64) as usize);
            at += 1 + random.below(5);
            let outcome = match random.below(12) {
                0 => Outcome::Failed { why: "no".to_owned() },
                1 => Outcome::Cached { remote: false },
                _ => Outcome::Built,
            };
            let count = if random.one_in(4) { 1 + random.below(3) } else { 0 };
            let added = make(&mut random, &mut ids, count);
            over = same(Input::Done { at_ns: at, id, outcome, added }, &mut running);
        }
    }
}

#[test]
fn a_key_done_before_runs_nothing_in_another_request() {
    let mut plane = Plane::new(config(1, 2)).unwrap();
    open(&mut plane, 1, 1, false);
    assert_eq!(runs(&add(&mut plane, 1, vec![keyed(1, &[], 1, 7)])), [1]);
    close(&mut plane, 1);
    assert_eq!(finished(&built(&mut plane, 1)), [(1, true)]);
    assert!(plane.remembers(&key(7)));
    open(&mut plane, 2, 1, false);
    let out = add(&mut plane, 2, vec![keyed(2, &[], 1, 7), work(3, &[2], 1)]);
    assert_eq!(runs(&out), [3]);
    assert_eq!(plane.state(NodeId(2)), Some(&NodeState::Cached { remote: false }));
    assert_eq!(plane.counts().remembered, 1);
    // Forgotten, the key runs again.
    plane.forget(&key(7));
    open(&mut plane, 3, 1, false);
    assert_eq!(runs(&add(&mut plane, 3, vec![keyed(4, &[], 1, 7)])), [4]);
}

#[test]
fn one_key_runs_once_for_two_requests() {
    let mut plane = Plane::new(config(1, 4)).unwrap();
    open(&mut plane, 1, 1, false);
    open(&mut plane, 2, 1, false);
    assert_eq!(runs(&add(&mut plane, 1, vec![keyed(1, &[], 1, 3)])), [1]);
    let out = add(&mut plane, 2, vec![keyed(2, &[], 1, 3), work(3, &[2], 1)]);
    assert_eq!(runs(&out), Vec::<u32>::new());
    assert_eq!(moves(&out), [(2, NodeState::Running), (3, NodeState::Waiting)]);
    close(&mut plane, 1);
    close(&mut plane, 2);
    // The joined node is not the task's: only its runner is said done.
    assert_eq!(plane.step(done(2, Outcome::Built, Vec::new())), Err(Error::NotRunning(NodeId(2))));
    let out = built(&mut plane, 1);
    assert_eq!(runs(&out), [3]);
    assert_eq!(finished(&out), [(1, true)]);
    assert_eq!(plane.state(NodeId(2)), Some(&NodeState::Built));
    assert_eq!(finished(&built(&mut plane, 3)), [(2, true)]);
    assert_eq!((plane.counts().ran, plane.counts().joined), (2, 1));
}

/// A task not yet started is ready in each request that waits for it, at that request's
/// priority: the first request whose turn it is starts it.
#[test]
fn a_joined_node_of_another_request_can_start_the_task() {
    let mut plane = Plane::new(config(1, 1)).unwrap();
    open(&mut plane, 1, 1, true);
    let out = add(&mut plane, 1, vec![work(1, &[], 100), work(2, &[], 50), keyed(3, &[], 1, 9)]);
    assert_eq!(runs(&out), [1]);
    open(&mut plane, 2, 1, true);
    assert_eq!(runs(&add(&mut plane, 2, vec![keyed(4, &[], 1, 9)])), Vec::<u32>::new());
    // Request 1 started a task, request 2 none: request 2's turn, and its one task is the key's.
    let out = built(&mut plane, 1);
    assert_eq!(runs(&out), [4]);
    assert_eq!(plane.state(NodeId(3)), Some(&NodeState::Running));
    let out = built(&mut plane, 4);
    assert_eq!(runs(&out), [2]);
    assert_eq!(plane.state(NodeId(3)), Some(&NodeState::Built));
}

#[test]
fn a_cancelled_request_skips_what_has_not_started_and_lets_what_runs_finish() {
    let mut plane = Plane::new(config(1, 1)).unwrap();
    open(&mut plane, 1, 1, true);
    let out = add(&mut plane, 1, vec![work(1, &[], 10), work(2, &[1], 1), work(3, &[], 1)]);
    assert_eq!(runs(&out), [1]);
    let out = plane.cancel(5, RequestId(1)).unwrap();
    assert!(matches!(
        &out[0],
        PlaneOutput::Said { record: Record { event: Event::Said { text, .. }, .. }, .. }
            if text == "request 1 is cancelled"
    ));
    assert_eq!(
        moves(&out),
        [(2, NodeState::Skipped { because: 1 }), (3, NodeState::Skipped { because: 1 })]
    );
    assert_eq!(finished(&out), []);
    assert_eq!(plane.cancel(6, RequestId(1)).unwrap(), []);
    assert_eq!(
        plane.step(PlaneInput::Add { at_ns: 6, request: RequestId(1), nodes: vec![work(9, &[], 1)] }),
        Err(Error::Closed)
    );
    assert_eq!(plane.step(PlaneInput::Close { at_ns: 6, request: RequestId(1) }), Err(Error::Closed));
    // What the running task finds is added skipped; the request then finishes, not ok.
    let out = plane.step(done(1, Outcome::Built, vec![work(4, &[1], 1)])).unwrap();
    assert_eq!(
        moves(&out),
        [(1, NodeState::Built), (4, NodeState::Skipped { because: 1 })]
    );
    assert_eq!(runs(&out), Vec::<u32>::new());
    assert_eq!(finished(&out), [(1, false)]);
    assert_eq!(plane.cancel(7, RequestId(1)), Err(Error::Over));
    assert_eq!(plane.cancel(7, RequestId(8)), Err(Error::UnknownRequest(8)));
}

/// A node of another request that joined a task the cancelled request runs stays on it.
#[test]
fn a_cancel_leaves_another_request_on_the_task_it_ran() {
    let mut plane = Plane::new(config(1, 2)).unwrap();
    open(&mut plane, 1, 1, true);
    open(&mut plane, 2, 1, true);
    assert_eq!(runs(&add(&mut plane, 1, vec![keyed(1, &[], 1, 4)])), [1]);
    add(&mut plane, 2, vec![keyed(2, &[], 1, 4)]);
    close(&mut plane, 2);
    let out = plane.cancel(1, RequestId(1)).unwrap();
    assert_eq!(moves(&out), []);
    assert_eq!(plane.state(NodeId(2)), Some(&NodeState::Running));
    let out = built(&mut plane, 1);
    assert_eq!(moves(&out), [(1, NodeState::Built), (2, NodeState::Built)]);
    assert_eq!(finished(&out), [(1, false), (2, true)]);
}

/// A node of the cancelled request that joined another's running task leaves it at once; the
/// task runs on for the other.
#[test]
fn a_cancelled_request_does_not_wait_for_another_requests_task() {
    let mut plane = Plane::new(config(1, 2)).unwrap();
    open(&mut plane, 1, 1, true);
    open(&mut plane, 2, 1, true);
    assert_eq!(runs(&add(&mut plane, 1, vec![keyed(1, &[], 1, 4)])), [1]);
    add(&mut plane, 2, vec![keyed(2, &[], 1, 4), work(3, &[2], 1)]);
    let out = plane.cancel(1, RequestId(2)).unwrap();
    assert_eq!(
        moves(&out),
        [(2, NodeState::Skipped { because: 2 }), (3, NodeState::Skipped { because: 2 })]
    );
    assert_eq!(finished(&out), [(2, false)]);
    close(&mut plane, 1);
    let out = built(&mut plane, 1);
    assert_eq!(moves(&out), [(1, NodeState::Built)]);
    assert_eq!(finished(&out), [(1, true)]);
}

#[test]
fn a_task_bigger_than_the_budget_fails_naming_itself() {
    let mut plane = Plane::new(PlaneConfig { budget: budget(10), ..config(1, 2) }).unwrap();
    open(&mut plane, 1, 1, true);
    let out = add(
        &mut plane,
        1,
        vec![node(1, &[], Pool::Work, 1, None, 11), node(2, &[], Pool::Work, 1, None, 10)],
    );
    assert_eq!(runs(&out), [2]);
    let Some(NodeState::Failed { why }) = plane.state(NodeId(1)) else {
        panic!("{:?}", plane.state(NodeId(1)))
    };
    assert!(why.starts_with("<1> needs"), "{why}");
    assert!(why.contains("more than blaze may use here"), "{why}");
    assert!(why.contains("the test's budget"), "{why}");
    // One that fits the budget but not what the process holds, with nothing running, fails too.
    built(&mut plane, 2);
    plane.step(PlaneInput::Held { at_ns: 0, bytes: 8 }).unwrap();
    add(&mut plane, 1, vec![node(3, &[], Pool::Work, 1, None, 3)]);
    let Some(NodeState::Failed { why }) = plane.state(NodeId(3)) else {
        panic!("{:?}", plane.state(NodeId(3)))
    };
    assert!(why.starts_with("<3> needs"), "{why}");
    assert!(why.contains("with nothing of blaze's running"), "{why}");
}

/// A task passed over for memory `starve_after` times holds back every task with memory, of
/// any pool, until it fits, and then starts before the smaller ones behind it.
#[test]
fn a_task_passed_over_starts_before_smaller_ones_behind_it() {
    let mut plane =
        Plane::new(PlaneConfig { io: 2, work: 4, budget: budget(10), starve_after: 2 }).unwrap();
    open(&mut plane, 1, 1, true);
    let mut nodes = vec![
        node(1, &[], Pool::Work, 1000, None, 5),
        node(2, &[], Pool::Work, 100, None, 8),
    ];
    nodes.extend((3..9).map(|id| node(id, &[], Pool::Work, 10 - u64::from(id), None, 2)));
    let out = add(&mut plane, 1, nodes);
    assert_eq!(runs(&out), [1, 3, 4]);
    assert_eq!(plane.holding(), Some(NodeId(2)));
    assert_eq!(plane.granted(), 9);
    // Held back too: another pool's task with memory; not one with none.
    let out = add(
        &mut plane,
        1,
        vec![node(9, &[], Pool::Io, 1, None, 1), node(10, &[], Pool::Io, 1, None, 0)],
    );
    assert_eq!(runs(&out), [10]);
    assert_eq!(runs(&built(&mut plane, 3)), Vec::<u32>::new());
    assert_eq!(runs(&built(&mut plane, 1)), [2]);
    assert_eq!(plane.holding(), None);
    assert_eq!(plane.granted(), 10);
    let out = built(&mut plane, 2);
    assert_eq!(runs(&out), [9, 5, 6, 7]);
}

/// With one thread and every request ready, each request's starts keep to its weight.
#[test]
fn one_thread_is_shared_by_weight() {
    let mut plane = Plane::new(config(1, 1)).unwrap();
    open(&mut plane, 1, 1, true);
    open(&mut plane, 2, 3, true);
    let mut running = runs(&add(&mut plane, 1, (0..40).map(|id| work(id, &[], 1)).collect()));
    running.extend(runs(&add(&mut plane, 2, (100..140).map(|id| work(id, &[], 1)).collect())));
    let mut order = String::new();
    for _ in 0..16 {
        let id = running.pop().unwrap();
        order.push(if id < 100 { 'a' } else { 'b' });
        running.extend(runs(&built(&mut plane, id)));
    }
    // Request 2 comes in at the plane's virtual time, a task behind request 1, and ties go to
    // the heavier.
    assert_eq!(order, "abbbbabbbabbbabb");
}

/// Requests always ready, on any number of threads, finishing in any order: no request's
/// starts run ahead of its share by more than one task (stride scheduling's bound: for any
/// two, `(started_i - 1) / weight_i <= started_j / weight_j`).
#[test]
fn fair_share_keeps_each_request_to_its_weight_within_one_task() {
    for seed in 0..200u64 {
        let mut random = SplitMix(seed);
        let threads = 1 + random.below(6) as usize;
        let mut plane = Plane::new(config(1, threads)).unwrap();
        // A request that takes every thread first, so the others all come in at once.
        open(&mut plane, 0, 1, true);
        let first: Vec<Spec> = (0..threads as u32).map(|id| work(id, &[], 1)).collect();
        let mut running: Vec<u32> = runs(&add(&mut plane, 0, first));
        assert_eq!(running.len(), threads);
        let requests = 2 + random.below(4) as usize;
        let weights: Vec<u64> = (0..requests).map(|_| 1 + random.below(6)).collect();
        const TASKS: u32 = 300;
        for (at, &weight) in weights.iter().enumerate() {
            let r = at as u32 + 1;
            open(&mut plane, r, weight as u32, true);
            let nodes = (0..TASKS).map(|it| work(r * 1000 + it, &[], random.below(5))).collect();
            assert_eq!(runs(&add(&mut plane, r, nodes)), Vec::<u32>::new());
        }
        let mut started = vec![0u64; requests];
        'steps: loop {
            let id = running.swap_remove(random.below(running.len() as u64) as usize);
            for ran in runs(&built(&mut plane, id)) {
                running.push(ran);
                let at = (ran / 1000) as usize - 1;
                started[at] += 1;
                if started.iter().any(|&it| it + 1 >= u64::from(TASKS)) {
                    break 'steps;
                }
                for i in 0..requests {
                    for j in 0..requests {
                        assert!(
                            started[i].saturating_sub(1) * weights[j] <= started[j] * weights[i],
                            "seed {seed}: started {started:?} for weights {weights:?}"
                        );
                    }
                }
            }
        }
    }
}

/// A request of the simulation, as its driver sees it.
struct Planned {
    weight: u32,
    keep_going: bool,
    opened: bool,
    closed: bool,
    batches: u32,
}

/// What the simulation knows of the plane, from what it said, to check it after every step.
#[derive(Default)]
struct Model {
    seed: u64,
    budget: Option<u64>,
    specs: FxHashMap<u32, Spec>,
    request_of: FxHashMap<u32, u32>,
    nodes_of: FxHashMap<u32, Vec<u32>>,
    dependents: FxHashMap<u32, Vec<u32>>,
    states: FxHashMap<u32, NodeState>,
    /// The tasks running, by the node they run as.
    running: Vec<u32>,
    running_keys: FxHashSet<Key>,
    granted: u64,
    /// Keys whose task ended well, and not forgotten since.
    remembered: FxHashSet<Key>,
    finished: FxHashMap<u32, bool>,
    cancelled: FxHashSet<u32>,
    outputs: Vec<PlaneOutput>,
}

impl Model {
    fn register(&mut self, request: u32, nodes: &[Spec]) {
        for spec in nodes {
            self.request_of.insert(spec.id.0, request);
            self.nodes_of.entry(request).or_default().push(spec.id.0);
            for dep in &spec.deps {
                self.dependents.entry(dep.0).or_default().push(spec.id.0);
            }
            self.specs.insert(spec.id.0, spec.clone());
        }
    }

    /// The longest path of expected cost from `id` to the end of its request.
    fn priority(&self, id: u32, memo: &mut FxHashMap<u32, u64>) -> u64 {
        if let Some(&it) = memo.get(&id) {
            return it;
        }
        let after = self
            .dependents
            .get(&id)
            .map(|it| it.iter().map(|&it| self.priority(it, memo)).max().unwrap_or(0))
            .unwrap_or(0);
        let priority = self.specs[&id].cost.saturating_add(after);
        memo.insert(id, priority);
        priority
    }

    fn done_well(&self, id: u32) -> bool {
        matches!(self.states.get(&id), Some(NodeState::Built | NodeState::Cached { .. }))
    }

    /// Takes the outputs of one step, checking each; `holding` is the node the plane said held
    /// back the others before it.
    fn take(&mut self, plane: &Plane, outputs: Vec<PlaneOutput>, holding: Option<NodeId>) {
        let seed = self.seed;
        let mut first_with_memory = true;
        let mut held_settled = false;
        for output in &outputs {
            match output {
                PlaneOutput::Said { request, record } => match &record.event {
                    Event::Node { id, .. } => {
                        assert_eq!(self.request_of[id], request.0, "seed {seed}: #{id}");
                    }
                    Event::Moved { id, state } => {
                        assert_eq!(self.request_of[id], request.0, "seed {seed}: #{id}");
                        if let Some(was) = self.states.get(id) {
                            assert!(!was.settled(), "seed {seed}: #{id} moved after it settled");
                        }
                        if matches!(state, NodeState::Running | NodeState::Ready) {
                            for dep in &self.specs[id].deps {
                                assert!(
                                    self.done_well(dep.0),
                                    "seed {seed}: #{id} {state:?} before #{dep} is done"
                                );
                            }
                        }
                        if let NodeState::Failed { why } = state
                            && why != "boom"
                        {
                            let spec = &self.specs[id];
                            assert!(why.starts_with(&spec.name), "seed {seed}: {why}");
                            let budget = self.budget.expect("a failure of memory has a budget");
                            if why.contains("more than blaze may use here") {
                                assert!(spec.memory > budget, "seed {seed}: {why}");
                            } else {
                                assert!(why.contains("with nothing of blaze's running"));
                                assert!(self.running.is_empty(), "seed {seed}: {why}");
                            }
                        }
                        if state.settled() && holding == Some(NodeId(*id)) {
                            held_settled = true;
                        }
                        self.states.insert(*id, state.clone());
                    }
                    Event::Critical { path } => {
                        for pair in path.windows(2) {
                            assert!(
                                self.specs[&pair[1]].deps.contains(&NodeId(pair[0])),
                                "seed {seed}: critical {path:?}"
                            );
                        }
                    }
                    Event::Said { .. } => {
                        assert!(self.cancelled.contains(&request.0), "seed {seed}");
                    }
                    other => panic!("seed {seed}: the plane said {other:?}"),
                },
                PlaneOutput::Run { id, pool, .. } => {
                    let spec = &self.specs[&id.0];
                    assert_eq!(*pool, spec.pool, "seed {seed}");
                    assert_eq!(self.states[&id.0], NodeState::Running, "seed {seed}: #{id}");
                    for dep in &spec.deps {
                        assert!(self.done_well(dep.0), "seed {seed}: #{id} runs before #{dep}");
                    }
                    if let Some(key) = spec.key {
                        assert!(self.running_keys.insert(key), "seed {seed}: {key:?} runs twice");
                        assert!(
                            !self.remembered.contains(&key),
                            "seed {seed}: #{id} runs a remembered key"
                        );
                    }
                    self.granted += spec.memory;
                    if let Some(budget) = self.budget {
                        assert!(self.granted <= budget, "seed {seed}: {} granted", self.granted);
                    }
                    if spec.memory > 0 && first_with_memory {
                        first_with_memory = false;
                        if let Some(held) = holding
                            && !held_settled
                        {
                            let held = &self.specs[&held.0];
                            let same = held.id == spec.id
                                || (held.key.is_some() && held.key == spec.key);
                            assert!(same, "seed {seed}: #{id} starts while {} is held", held.id);
                        }
                    }
                    if plane.config().budget.is_none() {
                        let mut memo = FxHashMap::default();
                        let request = self.request_of[&id.0];
                        let mine = self.priority(id.0, &mut memo);
                        for other in &self.nodes_of[&request] {
                            if self.states.get(other) == Some(&NodeState::Ready)
                                && self.specs[other].pool == spec.pool
                            {
                                let theirs = self.priority(*other, &mut memo);
                                assert!(
                                    theirs <= mine,
                                    "seed {seed}: #{id} at {mine} before #{other} at {theirs}"
                                );
                            }
                        }
                    }
                    self.running.push(id.0);
                }
                PlaneOutput::Finished { request, ok } => {
                    let r = request.0;
                    assert!(self.finished.insert(r, *ok).is_none(), "seed {seed}: twice");
                    let nodes = self.nodes_of.get(&r).map_or(&[][..], Vec::as_slice);
                    assert!(nodes.iter().all(|it| self.states[it].settled()), "seed {seed}");
                    let failed = nodes
                        .iter()
                        .any(|it| matches!(self.states[it], NodeState::Failed { .. }));
                    assert_eq!(*ok, !failed && !self.cancelled.contains(&r), "seed {seed}");
                }
            }
        }
        assert_eq!(plane.granted(), self.granted, "seed {seed}");
        // Nothing waits for a thread while nothing runs.
        if self.running.is_empty() {
            for (id, state) in &self.states {
                assert_ne!(*state, NodeState::Ready, "seed {seed}: #{id} ready, nothing runs");
            }
        }
        if let Some(held) = plane.holding() {
            assert!(self.budget.is_some(), "seed {seed}");
            assert_eq!(self.states[&held.0], NodeState::Ready, "seed {seed}");
        }
        self.outputs.extend(outputs);
    }

    /// A running task is done: what the plane is told, and what the model keeps of it.
    fn finish(&mut self, id: u32, outcome: &Outcome) {
        self.running.retain(|&it| it != id);
        let spec = &self.specs[&id];
        self.granted -= spec.memory;
        if let Some(key) = spec.key {
            self.running_keys.remove(&key);
            if !matches!(outcome, Outcome::Failed { .. }) {
                self.remembered.insert(key);
            }
        }
    }
}

/// One random run of many requests: the outputs the plane gave, and how many steps it took.
fn simulate(seed: u64) -> (Vec<PlaneOutput>, u64) {
    let mut random = SplitMix(seed);
    let budget = (!random.one_in(3)).then(|| 100 + random.below(900));
    let starve_after = random.below(5) as u32;
    let config = PlaneConfig {
        io: 1 + random.below(3) as usize,
        work: 1 + random.below(4) as usize,
        budget: budget.and_then(self::budget),
        starve_after,
    };
    let mut plane = Plane::new(config).unwrap();
    let keys = 1 + random.below(12) as u8;
    let memory = |random: &mut SplitMix| match budget {
        None => random.below(1000),
        Some(budget) if random.one_in(15) => budget + 1 + random.below(budget),
        Some(budget) if random.one_in(4) => 0,
        Some(budget) => random.below(budget / 2 + 1),
    };
    let by_key: Vec<(u64, Pool)> = (0..keys)
        .map(|_| (memory(&mut random), if random.one_in(3) { Pool::Io } else { Pool::Work }))
        .collect();
    let mut model = Model { seed, budget, ..Model::default() };
    let mut planned: Vec<Planned> = (0..1 + random.below(5))
        .map(|_| Planned {
            weight: 1 + random.below(4) as u32,
            keep_going: !random.one_in(3),
            opened: false,
            closed: false,
            batches: 1 + random.below(4) as u32,
        })
        .collect();
    let mut next_id = 0u32;
    let mut new_nodes = |random: &mut SplitMix, model: &Model, request: u32, count: u64| {
        let mut nodes: Vec<Spec> = Vec::new();
        let mut known: Vec<u32> = model.nodes_of.get(&request).cloned().unwrap_or_default();
        for _ in 0..count {
            let mut deps: Vec<u32> = (0..random.below(4))
                .filter(|_| !known.is_empty())
                .map(|_| known[random.below(known.len() as u64) as usize])
                .collect();
            deps.sort_unstable();
            deps.dedup();
            let id = next_id;
            next_id += 1;
            let (key, memory, pool) = if random.one_in(2) {
                let k = random.below(u64::from(keys)) as u8;
                (Some(key(k)), by_key[usize::from(k)].0, by_key[usize::from(k)].1)
            } else {
                let pool = if random.one_in(3) { Pool::Io } else { Pool::Work };
                (None, memory(random), pool)
            };
            nodes.push(node(id, &deps, pool, random.below(10), key, memory));
            known.push(id);
        }
        nodes
    };
    let mut at = 0u64;
    let mut steps = 0u64;
    loop {
        let all_over = planned.iter().enumerate().all(|(r, it)| {
            it.opened && model.finished.contains_key(&(r as u32))
        });
        if all_over {
            break;
        }
        steps += 1;
        assert!(steps < 100_000, "seed {seed}: no end");
        at += random.below(3);
        let holding = plane.holding();
        let roll = random.below(100);
        let live: Vec<u32> = (0..planned.len() as u32)
            .filter(|r| planned[*r as usize].opened && !model.finished.contains_key(r))
            .collect();
        let outputs = if roll < 3 && !live.is_empty() {
            let r = live[random.below(live.len() as u64) as usize];
            model.cancelled.insert(r);
            planned[r as usize].closed = true;
            plane.cancel(at, RequestId(r)).unwrap()
        } else if roll < 8 && budget.is_some() {
            let budget = budget.unwrap_or(0);
            let bytes = if random.one_in(10) { random.below(budget * 2) } else { random.below(budget / 3) };
            plane.step(PlaneInput::Held { at_ns: at, bytes }).unwrap()
        } else if roll < 10 && !model.remembered.is_empty() {
            let mut remembered: Vec<Key> = model.remembered.iter().copied().collect();
            remembered.sort_unstable();
            let key = remembered[random.below(remembered.len() as u64) as usize];
            plane.forget(&key);
            model.remembered.remove(&key);
            Vec::new()
        } else if roll < 45 || model.running.is_empty() {
            let unopened = planned.iter().position(|it| !it.opened);
            let unclosed: Vec<usize> =
                (0..planned.len()).filter(|&r| planned[r].opened && !planned[r].closed).collect();
            if let Some(r) = unopened
                && (unclosed.is_empty() || random.one_in(2))
            {
                let it = &mut planned[r];
                it.opened = true;
                plane
                    .step(PlaneInput::Open {
                        at_ns: at,
                        request: RequestId(r as u32),
                        weight: it.weight,
                        keep_going: it.keep_going,
                    })
                    .unwrap()
            } else if !unclosed.is_empty() {
                let r = unclosed[random.below(unclosed.len() as u64) as usize];
                if planned[r].batches > 0 {
                    planned[r].batches -= 1;
                    let count = 1 + random.below(8);
                    let nodes = new_nodes(&mut random, &model, r as u32, count);
                    model.register(r as u32, &nodes);
                    plane
                        .step(PlaneInput::Add { at_ns: at, request: RequestId(r as u32), nodes })
                        .unwrap()
                } else {
                    planned[r].closed = true;
                    plane.step(PlaneInput::Close { at_ns: at, request: RequestId(r as u32) }).unwrap()
                }
            } else {
                assert!(
                    !model.running.is_empty(),
                    "seed {seed}: nothing runs, every request is closed, and not all finished"
                );
                continue;
            }
        } else {
            let id = model.running[random.below(model.running.len() as u64) as usize];
            let outcome = match random.below(10) {
                0 => Outcome::Failed { why: "boom".to_owned() },
                1 => Outcome::Cached { remote: random.one_in(2) },
                _ => Outcome::Built,
            };
            let r = model.request_of[&id];
            let added = if random.one_in(4) {
                let count = 1 + random.below(3);
                new_nodes(&mut random, &model, r, count)
            } else {
                Vec::new()
            };
            model.register(r, &added);
            model.finish(id, &outcome);
            let group = (!random.one_in(3)).then(|| random.below(4) as u32);
            let input = PlaneInput::Done {
                at_ns: at,
                id: NodeId(id),
                outcome,
                added,
                group,
                peak: random.below(1 << 20),
            };
            plane.step(input).unwrap()
        };
        model.take(&plane, outputs, holding);
    }
    assert!(model.running.is_empty(), "seed {seed}");
    for (id, state) in &model.states {
        assert!(state.settled(), "seed {seed}: #{id} {state:?} after every request finished");
    }
    assert_eq!(model.states.len(), model.specs.len(), "seed {seed}");
    (model.outputs, steps)
}

/// Random requests under many seeds, checked after every step: nothing runs before its deps;
/// no key runs twice at once; a remembered key never runs; granted memory never exceeds the
/// budget; a held task starts before any other with memory; a failure for memory names its
/// node; the critical path goes first within a request; every request ends, ok exactly when
/// nothing of it failed and it was not cancelled.
#[test]
fn random_requests_keep_the_rules() {
    let mut steps = 0;
    for seed in 0..600u64 {
        steps += simulate(seed).1;
    }
    assert!(steps > 5_000, "{steps} steps");
}

#[test]
fn the_same_seed_gives_the_same_outputs() {
    for seed in 0..60u64 {
        let (a, steps) = simulate(seed);
        let (b, _) = simulate(seed);
        assert_eq!(a, b, "seed {seed} after {steps} steps");
    }
}

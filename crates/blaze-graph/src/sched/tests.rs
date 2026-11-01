use hegel::TestCase;
use hegel::generators as gs;
use rustc_hash::{FxHashMap, FxHashSet};

use super::*;

fn key(n: u32) -> Key {
    let mut bytes = [0u8; 32];
    bytes[..4].copy_from_slice(&n.to_le_bytes());
    Key(bytes)
}

fn below(tc: &TestCase, n: usize) -> usize {
    debug_assert!(n > 0);
    tc.draw(gs::integers::<u32>().max_value(n as u32 - 1)) as usize
}

/// Nodes of L3 groups of physical cores of SMT threads, each count drawn from 1.
#[hegel::composite]
fn shape(tc: &TestCase) -> Vec<CoreInfo> {
    let nodes = tc.draw(gs::integers::<u32>().min_value(1).max_value(3));
    let l3s = tc.draw(gs::integers::<u32>().min_value(1).max_value(2));
    let phys = tc.draw(gs::integers::<u32>().min_value(1).max_value(3));
    let smt = tc.draw(gs::integers::<u32>().min_value(1).max_value(2));
    let mut info = Vec::new();
    for node in 0..nodes {
        for l3 in 0..l3s {
            for p in 0..phys {
                for _ in 0..smt {
                    info.push(CoreInfo { smt: (node * l3s + l3) * phys + p, l3: node * l3s + l3, node });
                }
            }
        }
    }
    info
}

/// What a job waits for, as the model keeps it: independent of the scheduler's own lists.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ModelWait {
    Job(u32),
    Key(u32),
    Until(u64),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    /// Every wait is on a job of higher rank: the waits are a DAG.
    Dag,
    /// Any wait; the scheduler refuses those that close a cycle.
    Any,
}

/// A driver that does what the scheduler decides, and a model of what it was told.
struct Sim {
    sched: Sched,
    mode: Mode,
    jobs: u32,
    rank: Vec<u64>,
    /// A parallel map's items its job still joins, in order.
    joins: FxHashMap<u32, Vec<u32>>,
    waits: FxHashMap<u32, ModelWait>,
    /// Key number to claimer (`None` for the driver's).
    flights: FxHashMap<u32, Option<u32>>,
    next_key: u32,
    woken: Vec<u32>,
    ran: FxHashSet<u32>,
    done: FxHashSet<u32>,
    cycles: u32,
}

const MAX_SIM_JOBS: u32 = 120;

impl Sim {
    fn new(info: Vec<CoreInfo>, mode: Mode) -> Sim {
        Sim {
            sched: Sched::new(Cores::new(info).expect("a nested shape"), 16),
            mode,
            jobs: 0,
            rank: Vec::new(),
            joins: FxHashMap::default(),
            waits: FxHashMap::default(),
            flights: FxHashMap::default(),
            next_key: 0,
            woken: Vec::new(),
            ran: FxHashSet::default(),
            done: FxHashSet::default(),
            cycles: 0,
        }
    }

    fn cores(&self) -> u32 {
        self.sched.cores().len() as u32
    }

    fn running(&self) -> Vec<u32> {
        (0..self.cores()).filter_map(|core| self.sched.running(CoreId(core))).map(|job| job.0).collect()
    }

    /// Takes the decisions, checks them and the scheduler's structures.
    fn settle(&mut self) {
        let decisions: Vec<Decision> = self.sched.take_decisions().collect();
        for decision in decisions {
            match decision {
                Decision::Run { core, job } => {
                    assert_eq!(self.sched.running(core), Some(job), "{decision:?}");
                    self.ran.insert(job.0);
                }
                Decision::Wake { core, job } => {
                    assert_ne!(self.sched.job_state(job), JobState::Done, "{decision:?}");
                    if self.sched.core_state(core) == CoreState::Woken && !self.woken.contains(&core.0) {
                        self.woken.push(core.0);
                    }
                }
            }
        }
        self.sched.audit();
        for steal in &self.sched.steals {
            if self.sched.cores().level(steal.thief, steal.victim) == Level::Remote {
                assert_eq!(
                    steal.on_thief_node, 0,
                    "core {} stole across nodes from core {} while its node had work",
                    steal.thief, steal.victim
                );
            }
        }
        self.sched.steals.clear();
    }

    fn hint(&self, tc: &TestCase) -> Affinity {
        let info = self.sched.cores().info(below(tc, self.cores() as usize) as u32);
        match below(tc, 5) {
            0 => Affinity::Spawner,
            1 => Affinity::Core(CoreId(below(tc, self.cores() as usize) as u32)),
            2 => Affinity::L3(info.l3),
            3 => Affinity::Node(info.node),
            _ => Affinity::Any,
        }
    }

    fn spawn(&mut self, parent: Option<u32>, hint: Affinity, rank: u64) -> u32 {
        let job = self.jobs;
        self.jobs += 1;
        self.rank.push(rank);
        self.sched.spawn(JobId(job), parent.map(JobId), hint);
        self.settle();
        job
    }

    /// The jobs a wait resolves to, by the model.
    fn resolve(&self, wait: ModelWait) -> Option<u32> {
        match wait {
            ModelWait::Job(job) => Some(job),
            ModelWait::Key(k) => self.flights.get(&k).copied().flatten(),
            ModelWait::Until(_) => None,
        }
    }

    fn wait(&mut self, job: u32, wait: ModelWait) {
        let on = match wait {
            ModelWait::Job(other) => Wait::Job(JobId(other)),
            ModelWait::Key(k) => Wait::Key(key(k)),
            ModelWait::Until(at) => Wait::Until(at),
        };
        let satisfied = match wait {
            ModelWait::Job(other) => self.done.contains(&other),
            ModelWait::Key(k) => !self.flights.contains_key(&k),
            ModelWait::Until(at) => at <= self.sched.now(),
        };
        // The model's own chain from the wait's target: does it reach `job`?
        let mut chain = vec![job];
        let mut closes = false;
        let mut at = if satisfied { None } else { self.resolve(wait) };
        while let Some(next) = at {
            if next == job {
                closes = true;
                break;
            }
            if chain.contains(&next) {
                break;
            }
            chain.push(next);
            at = self.waits.get(&next).and_then(|&it| self.resolve(it));
        }
        match self.sched.suspended(JobId(job), on) {
            Ok(Suspend::Ready) => assert!(satisfied, "job {job}'s wait {wait:?} was not done"),
            Ok(Suspend::Waiting) => {
                assert!(!satisfied, "job {job} waits for {wait:?}, done already");
                assert!(!closes, "job {job}'s wait {wait:?} closes the cycle {chain:?}");
                self.waits.insert(job, wait);
            }
            Err(cycle) => {
                assert!(closes, "job {job}'s wait {wait:?} was refused as {cycle:?}, closing no cycle");
                assert_eq!(self.mode, Mode::Any, "a DAG of waits has no cycle: {cycle}");
                let jobs: Vec<u32> = cycle.jobs.iter().map(|it| it.0).collect();
                assert_eq!(jobs, chain, "the cycle named");
                self.cycles += 1;
            }
        }
        self.settle();
    }

    fn clear_waits(&mut self, gone: impl Fn(ModelWait) -> bool) {
        self.waits.retain(|_, wait| !gone(*wait));
    }

    fn done(&mut self, job: u32) {
        self.sched.done(JobId(job));
        self.done.insert(job);
        let claimed: Vec<u32> = self.flights.iter().filter(|it| *it.1 == Some(job)).map(|it| *it.0).collect();
        for k in &claimed {
            self.flights.remove(k);
        }
        self.clear_waits(|wait| wait == ModelWait::Job(job) || matches!(wait, ModelWait::Key(k) if claimed.contains(&k)));
        self.settle();
    }

    fn key_done(&mut self, k: u32) {
        self.sched.key_done(key(k));
        self.flights.remove(&k);
        self.clear_waits(|wait| wait == ModelWait::Key(k));
        self.settle();
    }

    fn advance(&mut self, to: u64) {
        self.sched.advance_clock(to);
        let now = self.sched.now();
        self.clear_waits(|wait| matches!(wait, ModelWait::Until(at) if at <= now));
        self.settle();
    }

    fn claim(&mut self, by: Option<u32>) {
        let k = self.next_key;
        self.next_key += 1;
        assert!(self.sched.claim(key(k), by.map(JobId)));
        assert!(!self.sched.claim(key(k), None), "a second claim of one key");
        self.flights.insert(k, by);
    }

    /// A job that resumes inside a parallel map joins its next item, as the map's join does.
    fn join_next(&mut self, job: u32) -> bool {
        let Some(items) = self.joins.get_mut(&job) else { return false };
        let Some(item) = items.pop() else {
            self.joins.remove(&job);
            return false;
        };
        self.wait(job, ModelWait::Job(item));
        true
    }

    /// A wait target for `job`: in a DAG, only on jobs and flights of higher rank.
    fn target(&self, tc: &TestCase, job: u32) -> Option<ModelWait> {
        match below(tc, 3) {
            0 => {
                let other = below(tc, self.jobs as usize) as u32;
                (self.mode == Mode::Any || self.rank[other as usize] > self.rank[job as usize])
                    .then_some(ModelWait::Job(other))
            }
            1 => {
                let flights: Vec<(u32, Option<u32>)> = self.flights.iter().map(|it| (*it.0, *it.1)).collect();
                if flights.is_empty() {
                    return None;
                }
                let (k, by) = flights[below(tc, flights.len())];
                let ok = match by {
                    None => true,
                    Some(by) => self.mode == Mode::Any || self.rank[by as usize] > self.rank[job as usize],
                };
                ok.then_some(ModelWait::Key(k))
            }
            _ => Some(ModelWait::Until(self.sched.now() + below(tc, 200) as u64)),
        }
    }

    fn step(&mut self, tc: &TestCase) {
        let running = self.running();
        match below(tc, 12) {
            0 if self.jobs < MAX_SIM_JOBS => {
                let hint = self.hint(tc);
                let rank = tc.draw(gs::integers::<u64>().max_value(1 << 40));
                self.spawn(None, hint, rank);
            }
            1 if !running.is_empty() && self.jobs < MAX_SIM_JOBS => {
                let parent = running[below(tc, running.len())];
                let hint = self.hint(tc);
                let rank = self.rank[parent as usize] + 1 + tc.draw(gs::integers::<u64>().max_value(1 << 20));
                self.spawn(Some(parent), hint, rank);
            }
            // A parallel map: the job spawns its items and joins them in turn; each item may
            // wait for other jobs when it runs.
            2 if !running.is_empty() && self.jobs + 8 < MAX_SIM_JOBS => {
                let parent = running[below(tc, running.len())];
                if self.joins.contains_key(&parent) {
                    return;
                }
                let count = 1 + below(tc, 6);
                let mut items = Vec::new();
                for _ in 0..count {
                    let rank = self.rank[parent as usize] + 1 + tc.draw(gs::integers::<u64>().max_value(1 << 20));
                    items.push(self.spawn(Some(parent), Affinity::Spawner, rank));
                }
                items.reverse();
                self.joins.insert(parent, items);
                self.join_next(parent);
            }
            3 | 4 if !running.is_empty() => {
                let job = running[below(tc, running.len())];
                if self.joins.contains_key(&job) {
                    self.join_next(job);
                } else if let Some(wait) = self.target(tc, job) {
                    self.wait(job, wait);
                }
            }
            5 | 6 if !running.is_empty() => {
                let job = running[below(tc, running.len())];
                if !self.join_next(job) {
                    self.done(job);
                }
            }
            7 if !running.is_empty() => {
                let job = running[below(tc, running.len())];
                self.claim(Some(job));
            }
            8 => {
                let mine: Vec<u32> = self
                    .flights
                    .iter()
                    .filter(|it| it.1.is_none_or(|by| running.contains(&by)))
                    .map(|it| *it.0)
                    .collect();
                if mine.is_empty() {
                    self.claim(None);
                } else {
                    let k = mine[below(tc, mine.len())];
                    self.key_done(k);
                }
            }
            9 | 10 if !self.woken.is_empty() => {
                let core = self.woken.swap_remove(below(tc, self.woken.len()));
                self.sched.core_idle(CoreId(core));
                self.settle();
            }
            11 => {
                let core = below(tc, self.cores() as usize) as u32;
                if self.sched.core_state(CoreId(core)) == CoreState::Parked {
                    self.sched.core_idle(CoreId(core));
                    self.settle();
                } else {
                    let to = self.sched.now() + below(tc, 300) as u64;
                    self.advance(to);
                }
            }
            _ => {}
        }
    }

    /// No new work and no new waits but joins: the driver does what is asked and ends what
    /// runs until nothing is left. Every job must end.
    fn drain(&mut self) {
        for _ in 0..1_000_000 {
            if let Some(core) = self.woken.pop() {
                self.sched.core_idle(CoreId(core));
                self.settle();
                continue;
            }
            let running = self.running().first().copied();
            if let Some(job) = running {
                if !self.join_next(job) {
                    self.done(job);
                }
                continue;
            }
            let external = self.flights.iter().find(|it| it.1.is_none()).map(|it| *it.0);
            if let Some(k) = external {
                self.key_done(k);
                continue;
            }
            if let Some(at) = self.sched.next_deadline() {
                self.advance(at);
                continue;
            }
            break;
        }
        // With cycles refused, the waits that stand are a DAG in either mode.
        for job in 0..self.jobs {
            let state = self.sched.job_state(JobId(job));
            assert_eq!(state, JobState::Done, "job {job} never ended (waits: {:?})", self.waits.get(&job));
            assert!(self.ran.contains(&job), "job {job} ended without a run");
        }
        for core in 0..self.cores() {
            assert_eq!(self.sched.core_state(CoreId(core)), CoreState::Parked, "core {core} at the end");
        }
    }
}

/// Every job that becomes runnable runs, whatever the order of spawns, waits, ends and idle
/// reports: once nothing new arrives, every job ends and every core parks.
#[hegel::test]
fn every_runnable_job_runs(tc: TestCase) {
    let mut sim = Sim::new(tc.draw(shape()), Mode::Dag);
    let steps = 1 + below(&tc, 300);
    for _ in 0..steps {
        sim.step(&tc);
    }
    sim.drain();
}

/// No deadlock when the waits form a DAG, including parallel maps whose items wait for other
/// jobs (the case that hung two pools waiting on each other): no thread is held by a wait, so
/// the jobs end on as few as one core.
#[hegel::test]
fn no_deadlock_when_waits_form_a_dag(tc: TestCase) {
    let cores = 1 + below(&tc, 3) as u32;
    let mut sim = Sim::new((0..cores).map(|core| CoreInfo { smt: core, l3: 0, node: 0 }).collect(), Mode::Dag);
    let roots = 1 + below(&tc, 4);
    for _ in 0..roots {
        let rank = tc.draw(gs::integers::<u64>().max_value(1 << 40));
        sim.spawn(None, Affinity::Any, rank);
    }
    let steps = 1 + below(&tc, 400);
    for _ in 0..steps {
        sim.step(&tc);
    }
    sim.drain();
}

/// A wait that closes a cycle is refused with the jobs of that cycle, in wait order, and
/// one that closes none is never refused.
#[hegel::test]
fn a_cycle_of_waits_is_reported_naming_its_jobs(tc: TestCase) {
    let mut sim = Sim::new(tc.draw(shape()), Mode::Any);
    let steps = 1 + below(&tc, 300);
    for _ in 0..steps {
        sim.step(&tc);
    }
    sim.drain();
}

/// On a machine of several nodes, a steal crosses a node only when the thief's own node has no
/// runnable job.
#[hegel::test]
fn a_steal_never_crosses_a_node_while_its_own_has_work(tc: TestCase) {
    let mut info = tc.draw(shape());
    if info.iter().all(|it| it.node == 0) {
        let half = info.len().div_ceil(2);
        let base = info.len() as u32;
        for (at, core) in info.iter_mut().enumerate().skip(half) {
            *core = CoreInfo { smt: base + at as u32, l3: base + 1, node: 1 };
        }
    }
    let mut sim = Sim::new(info, Mode::Dag);
    let steps = 1 + below(&tc, 300);
    for _ in 0..steps {
        sim.step(&tc);
    }
    sim.drain();
}

/// Cores 0 to 7: node 0 has L3 group 0 of physical cores {0, 1} and {2, 3}; node 1 has L3
/// group 1 of {4, 5} and {6, 7}.
fn two_nodes() -> Cores {
    let mut info = Vec::new();
    for core in 0..8u32 {
        info.push(CoreInfo { smt: core / 2, l3: core / 4, node: core / 4 });
    }
    Cores::new(info).unwrap()
}

fn decisions(sched: &mut Sched) -> Vec<Decision> {
    sched.take_decisions().collect()
}

/// Every core busy with a root job `core`, each hinted to its core.
fn all_busy(sched: &mut Sched) {
    let n = sched.cores().len() as u32;
    for core in 0..n {
        sched.spawn(JobId(core), None, Affinity::Core(CoreId(core)));
        assert_eq!(decisions(sched), [Decision::Wake { core: CoreId(core), job: JobId(core) }]);
        sched.core_idle(CoreId(core));
        assert_eq!(decisions(sched), [Decision::Run { core: CoreId(core), job: JobId(core) }]);
    }
    sched.audit();
}

#[test]
fn a_job_runs_where_it_was_spawned() {
    let mut sched = Sched::new(Cores::flat(2).unwrap(), 8);
    all_busy(&mut sched);
    sched.spawn(JobId(2), Some(JobId(0)), Affinity::Spawner);
    sched.spawn(JobId(3), Some(JobId(1)), Affinity::Spawner);
    assert!(decisions(&mut sched).is_empty());
    sched.done(JobId(1));
    assert_eq!(decisions(&mut sched), [Decision::Run { core: CoreId(1), job: JobId(3) }]);
    sched.done(JobId(0));
    assert_eq!(decisions(&mut sched), [Decision::Run { core: CoreId(0), job: JobId(2) }]);
    sched.audit();
}

#[test]
fn a_waiter_wakes_on_the_finishing_core_ahead_of_its_queue() {
    let mut sched = Sched::new(Cores::flat(2).unwrap(), 8);
    all_busy(&mut sched);
    for job in [2, 3] {
        sched.spawn(JobId(job), Some(JobId(0)), Affinity::Spawner);
    }
    // Job 1 waits for job 0; its core steals job 0's oldest child.
    assert_eq!(sched.suspended(JobId(1), Wait::Job(JobId(0))), Ok(Suspend::Waiting));
    assert_eq!(decisions(&mut sched), [Decision::Run { core: CoreId(1), job: JobId(2) }]);
    // Job 0 ends on core 0: job 1 runs there next, ahead of job 3 in core 0's queue.
    sched.done(JobId(0));
    assert_eq!(decisions(&mut sched), [Decision::Run { core: CoreId(0), job: JobId(1) }]);
    sched.done(JobId(1));
    assert_eq!(decisions(&mut sched), [Decision::Run { core: CoreId(0), job: JobId(3) }]);
    sched.audit();
}

#[test]
fn a_waiting_job_frees_its_core() {
    let mut sched = Sched::new(Cores::flat(1).unwrap(), 8);
    all_busy(&mut sched);
    // On one core, a job waits for its child: the child runs on the core the wait freed.
    sched.spawn(JobId(1), Some(JobId(0)), Affinity::Spawner);
    assert_eq!(sched.suspended(JobId(0), Wait::Job(JobId(1))), Ok(Suspend::Waiting));
    assert_eq!(decisions(&mut sched), [Decision::Run { core: CoreId(0), job: JobId(1) }]);
    sched.done(JobId(1));
    assert_eq!(decisions(&mut sched), [Decision::Run { core: CoreId(0), job: JobId(0) }]);
    assert_eq!(sched.suspended(JobId(0), Wait::Job(JobId(1))), Ok(Suspend::Ready));
    sched.done(JobId(0));
    assert_eq!(sched.core_state(CoreId(0)), CoreState::Parked);
    sched.audit();
}

#[test]
fn an_idle_core_steals_its_sibling_then_its_l3_then_its_node_then_anywhere() {
    let mut sched = Sched::new(two_nodes(), 32);
    all_busy(&mut sched);
    // Work on a core of each distance from core 0: 6 (remote), 2 (its L3 group), 1 (its
    // sibling). Node 1's own cores are all busy, so none of it moves yet.
    sched.spawn(JobId(10), Some(JobId(6)), Affinity::Spawner);
    sched.spawn(JobId(11), Some(JobId(2)), Affinity::Spawner);
    sched.spawn(JobId(12), Some(JobId(1)), Affinity::Spawner);
    assert!(decisions(&mut sched).is_empty());
    sched.done(JobId(0));
    assert_eq!(decisions(&mut sched), [Decision::Run { core: CoreId(0), job: JobId(12) }]);
    sched.done(JobId(12));
    assert_eq!(decisions(&mut sched), [Decision::Run { core: CoreId(0), job: JobId(11) }]);
    sched.done(JobId(11));
    assert_eq!(decisions(&mut sched), [Decision::Run { core: CoreId(0), job: JobId(10) }]);
    assert_eq!(sched.stats().steals, [1, 1, 0, 1]);
    sched.audit();
}

#[test]
fn a_job_made_runnable_goes_to_the_nearest_parked_core() {
    let mut sched = Sched::new(two_nodes(), 32);
    all_busy(&mut sched);
    // Cores 5 and 3 park; core 4's new child goes to its sibling 5, not to 3 on node 0.
    sched.done(JobId(3));
    sched.done(JobId(5));
    assert!(decisions(&mut sched).is_empty());
    sched.spawn(JobId(20), Some(JobId(4)), Affinity::Spawner);
    assert_eq!(decisions(&mut sched), [Decision::Wake { core: CoreId(5), job: JobId(20) }]);
    sched.core_idle(CoreId(5));
    assert_eq!(decisions(&mut sched), [Decision::Run { core: CoreId(5), job: JobId(20) }]);
    // A second child: no core of node 1 is parked, so core 3 on node 0 takes it.
    sched.spawn(JobId(21), Some(JobId(4)), Affinity::Spawner);
    assert_eq!(decisions(&mut sched), [Decision::Wake { core: CoreId(3), job: JobId(21) }]);
    sched.audit();
}

#[test]
fn a_cross_core_wake_is_a_message_to_its_inbox() {
    let mut sched = Sched::new(Cores::flat(2).unwrap(), 8);
    all_busy(&mut sched);
    assert!(sched.claim(key(7), None));
    sched.spawn(JobId(2), Some(JobId(0)), Affinity::Spawner);
    assert_eq!(sched.suspended(JobId(0), Wait::Key(key(7))), Ok(Suspend::Waiting));
    assert_eq!(decisions(&mut sched), [Decision::Run { core: CoreId(0), job: JobId(2) }]);
    // The key's flight is the driver's: job 0 wakes on core 0, busy, by a message.
    sched.key_done(key(7));
    assert_eq!(decisions(&mut sched), [Decision::Wake { core: CoreId(0), job: JobId(0) }]);
    sched.done(JobId(2));
    assert_eq!(decisions(&mut sched), [Decision::Run { core: CoreId(0), job: JobId(0) }]);
    // A key with no flight is no wait.
    assert_eq!(sched.suspended(JobId(0), Wait::Key(key(7))), Ok(Suspend::Ready));
    sched.audit();
}

#[test]
fn a_key_flight_ends_with_its_job() {
    let mut sched = Sched::new(Cores::flat(2).unwrap(), 8);
    all_busy(&mut sched);
    assert!(sched.claim(key(1), Some(JobId(0))));
    assert!(!sched.claim(key(1), Some(JobId(1))));
    assert_eq!(sched.suspended(JobId(1), Wait::Key(key(1))), Ok(Suspend::Waiting));
    sched.done(JobId(0));
    assert_eq!(decisions(&mut sched), [Decision::Run { core: CoreId(0), job: JobId(1) }]);
    assert!(sched.claim(key(1), Some(JobId(1))));
    sched.audit();
}

#[test]
fn a_cycle_names_its_jobs_in_wait_order() {
    let mut sched = Sched::new(Cores::flat(3).unwrap(), 8);
    all_busy(&mut sched);
    assert!(sched.claim(key(9), Some(JobId(1))));
    assert_eq!(sched.suspended(JobId(0), Wait::Key(key(9))), Ok(Suspend::Waiting));
    assert_eq!(sched.suspended(JobId(1), Wait::Job(JobId(2))), Ok(Suspend::Waiting));
    let cycle = sched.suspended(JobId(2), Wait::Job(JobId(0))).unwrap_err();
    assert_eq!(cycle.jobs, [JobId(2), JobId(0), JobId(1)]);
    assert_eq!(cycle.to_string(), "a wait would close a cycle of waits: job 2 -> job 0 -> job 1 -> job 2");
    assert_eq!(sched.suspended(JobId(2), Wait::Job(JobId(2))).unwrap_err().jobs, [JobId(2)]);
    // Refused, job 2 runs on, and nothing changed.
    assert_eq!(sched.job_state(JobId(2)), JobState::Running);
    assert_eq!(sched.stats().cycles, 2);
    sched.audit();
}

#[test]
fn a_timed_wait_wakes_at_its_deadline() {
    let mut sched = Sched::new(Cores::flat(1).unwrap(), 8);
    all_busy(&mut sched);
    assert_eq!(sched.suspended(JobId(0), Wait::Until(100)), Ok(Suspend::Waiting));
    assert_eq!(sched.next_deadline(), Some(100));
    sched.advance_clock(99);
    assert!(decisions(&mut sched).is_empty());
    sched.advance_clock(100);
    assert_eq!(decisions(&mut sched), [Decision::Wake { core: CoreId(0), job: JobId(0) }]);
    assert_eq!(sched.next_deadline(), None);
    sched.core_idle(CoreId(0));
    assert_eq!(decisions(&mut sched), [Decision::Run { core: CoreId(0), job: JobId(0) }]);
    assert_eq!(sched.suspended(JobId(0), Wait::Until(50)), Ok(Suspend::Ready));
    sched.audit();
}

#[test]
fn the_slot_gives_way_to_the_queue_after_its_runs() {
    let mut sched = Sched::new(Cores::flat(1).unwrap(), 16);
    all_busy(&mut sched);
    let run = |job: u32| [Decision::Run { core: CoreId(0), job: JobId(job) }];
    // Jobs 1 to 4 each wait for the one before, job 1 for job 0, which sleeps meanwhile.
    for job in [4, 3, 2, 1] {
        sched.spawn(JobId(job), Some(JobId(0)), Affinity::Spawner);
    }
    assert_eq!(sched.suspended(JobId(0), Wait::Until(10)), Ok(Suspend::Waiting));
    assert_eq!(decisions(&mut sched), run(1));
    for job in 1..4 {
        assert_eq!(sched.suspended(JobId(job), Wait::Job(JobId(job - 1))), Ok(Suspend::Waiting));
        assert_eq!(decisions(&mut sched), run(job + 1));
    }
    assert_eq!(sched.suspended(JobId(4), Wait::Job(JobId(3))), Ok(Suspend::Waiting));
    assert_eq!(sched.core_state(CoreId(0)), CoreState::Parked);
    sched.advance_clock(10);
    assert_eq!(decisions(&mut sched), [Decision::Wake { core: CoreId(0), job: JobId(0) }]);
    sched.core_idle(CoreId(0));
    assert_eq!(decisions(&mut sched), run(0));
    // Job 5 queued; each end puts the next waiter in the slot.
    sched.spawn(JobId(5), Some(JobId(0)), Affinity::Spawner);
    for job in 0..u32::from(LIFO_RUNS) {
        sched.done(JobId(job));
        assert_eq!(decisions(&mut sched), run(job + 1), "slot run {}", job + 1);
    }
    // The slot has run its turns: the queue's job goes first, then the slot's.
    sched.done(JobId(u32::from(LIFO_RUNS)));
    assert_eq!(decisions(&mut sched), run(5));
    sched.done(JobId(5));
    assert_eq!(decisions(&mut sched), run(u32::from(LIFO_RUNS) + 1));
    sched.audit();
}

#[test]
fn a_shape_that_is_not_nested_is_refused() {
    let split = vec![CoreInfo { smt: 0, l3: 0, node: 0 }, CoreInfo { smt: 0, l3: 1, node: 0 }];
    assert_eq!(Cores::new(split).err(), Some(ShapeError::SiblingsSplit { core: 0, other: 1 }));
    let split = vec![CoreInfo { smt: 0, l3: 0, node: 0 }, CoreInfo { smt: 1, l3: 0, node: 1 }];
    assert_eq!(Cores::new(split).err(), Some(ShapeError::L3Split { core: 0, other: 1 }));
    assert_eq!(Cores::new(Vec::new()).err(), Some(ShapeError::NoCores));
}

#[test]
fn first_from_wraps_and_respects_the_mask() {
    let bits = [0b1010u64, 1 << 3];
    let mask = [u64::MAX, u64::MAX];
    assert_eq!(cores::first_from(&bits, &mask, 0), Some(1));
    assert_eq!(cores::first_from(&bits, &mask, 2), Some(3));
    assert_eq!(cores::first_from(&bits, &mask, 4), Some(67));
    assert_eq!(cores::first_from(&bits, &mask, 68), Some(1));
    assert_eq!(cores::first_from(&bits, &[0, u64::MAX], 0), Some(67));
    assert_eq!(cores::first_from(&bits, &[0, 0], 0), None);
}

/// Once warm, spawns, runs, steals, waits, key flights and timers make no call to the global
/// allocator.
#[test]
fn a_warm_scheduler_allocates_nothing() {
    /// Eight root jobs, one a core; a child stolen, a key flight with a waiter handed to a
    /// parked core, a timed wait woken by a message; then every job ended.
    fn round(sched: &mut Sched, now: u64) {
        for core in 0..8u32 {
            sched.spawn(JobId(core), None, Affinity::Core(CoreId(core)));
        }
        for core in 0..8u32 {
            sched.core_idle(CoreId(core));
        }
        let _ = sched.take_decisions().count();
        sched.spawn(JobId(8), Some(JobId(0)), Affinity::Spawner);
        assert!(sched.claim(key(0), Some(JobId(0))));
        assert_eq!(sched.suspended(JobId(1), Wait::Key(key(0))), Ok(Suspend::Waiting));
        assert_eq!(sched.suspended(JobId(2), Wait::Until(now + 10)), Ok(Suspend::Waiting));
        sched.key_done(key(0));
        sched.advance_clock(now + 10);
        for _ in 0..64 {
            let mut any = sched.take_decisions().count() > 0;
            for core in 0..8u32 {
                match sched.core_state(CoreId(core)) {
                    CoreState::Woken => {
                        sched.core_idle(CoreId(core));
                        any = true;
                    }
                    CoreState::Busy => {
                        let job = sched.running(CoreId(core)).unwrap();
                        sched.done(job);
                        any = true;
                    }
                    CoreState::Parked => {}
                }
            }
            if !any {
                break;
            }
        }
        assert!((0..9).all(|job| sched.job_state(JobId(job)) == JobState::Done));
        sched.steals.clear();
    }
    let mut sched = Sched::new(two_nodes(), 64);
    round(&mut sched, 0);
    round(&mut sched, 100);
    blaze_alloc_count::assert_at_most(0, "a warm scheduler's spawns, runs, waits and timers", || {
        for at in 0..200u64 {
            round(&mut sched, 200 + at * 20);
        }
    });
    sched.audit();
}

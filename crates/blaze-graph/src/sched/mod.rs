//! The scheduler as a state machine that does no IO (docs/v2/plan/lanes/sched.md): it reads no
//! clock, starts no thread and never sleeps. A driver tells it what happened ([`Sched::spawn`],
//! [`Sched::suspended`], [`Sched::done`], [`Sched::key_done`], [`Sched::yielded`],
//! [`Sched::core_idle`], [`Sched::advance_clock`]) and does what it decides
//! ([`Sched::take_decisions`]: [`Decision::Run`], [`Decision::Wake`], [`Decision::Yield`]),
//! waking itself at [`Sched::next_deadline`]. The machine's shape is an input ([`Cores`]), never
//! read here.
//!
//! A waiting job holds no core: [`Sched::suspended`] frees its core, which runs the next job at
//! once, and the job is runnable again when what it waits for is done: a job, a key's flight,
//! a counter (every job of a parallel map: one suspend for all of them, as Naughty Dog's fiber
//! jobs wait) or a time. A wait that would close a cycle is refused with the [`Cycle`] it would
//! close.
//!
//! Policy:
//! - One queue per core, and one for its critical-path jobs, taken first. A job is queued on
//!   the core that spawned it (the owner pops newest, as Chase-Lev and Rayon), unless its
//!   [`Affinity`] names another place.
//! - A waiter wakes on the core that finished what it waited on, in that core's LIFO slot ahead
//!   of its queues, hot in the cache that just wrote what it reads. After [`LIFO_RUNS`] slot
//!   runs in a row the core takes its inbox and queues first, so the slot never starves them.
//! - A job handed to a core from elsewhere (a hint, a timer, an external key) is a message to
//!   that core's inbox: [`Decision::Wake`].
//! - Work-conserving, one sleeper woken per job: while any core is parked no job sits
//!   stealable. A job made runnable on a busy core goes at once to the nearest parked core, by
//!   the tiers of [`Cores`]: a critical-path job to the nearest parked core of the highest
//!   capacity first, any other job to a lower-capacity core first within each tier. A free core
//!   with nothing of its own steals half a victim's queue from the nearest core with work, tier
//!   by tier (critical jobs first for a big core, the others first for a small one). So a steal
//!   crosses a node only when the thief's node has nothing runnable.
//! - A big core never sits idle while a small core runs a critical job: the scheduler asks the
//!   job to move ([`Decision::Yield`]), the job looks at its core's flag at its next query miss
//!   and calls [`Sched::yielded`], and moves to the big core. Nothing is preempted.

mod cores;
#[cfg(test)]
mod tests;

use std::{collections::VecDeque, fmt};

use blaze_core::wheel::Wheel;
use rustc_hash::FxHashMap;

pub use cores::{CoreInfo, Cores, Distances, LEVELS, Level, MAX_CORES, ShapeError};

use crate::Key;
use cores::{any_in, first_from, get, set};

/// How many slot jobs a core runs in a row before it looks at its inbox and queues (Tokio's
/// `MAX_LIFO_POLLS_PER_TICK`).
pub const LIFO_RUNS: u8 = 3;

/// The most job ids: a job id indexes the scheduler's columns.
pub const MAX_JOBS: u32 = 1 << 30;

const NONE: u32 = u32::MAX;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct JobId(pub u32);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct CoreId(pub u32);

/// A count of jobs still to end ([`Sched::counter`]): a wait on it is a wait for all of them.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct CounterId(pub u32);

/// Where a spawned job should run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Affinity {
    /// The spawner's core; with no spawner, the least loaded core.
    Spawner,
    Core(CoreId),
    /// The spawner's core if it is in this L3 group, else the group's least loaded core.
    L3(u32),
    /// The spawner's core if it is on this node, else the node's least loaded core.
    Node(u32),
    /// The least loaded core.
    Any,
}

/// How a job is spawned.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Spawn {
    /// The running job that spawns it; `None` for the driver.
    pub parent: Option<JobId>,
    pub hint: Affinity,
    /// On the critical path, by the profiler's data: it runs on the highest-capacity cores.
    pub critical: bool,
    /// The counter it counts towards until it ends.
    pub counter: Option<CounterId>,
}

impl Spawn {
    /// A job of the driver's, on the least loaded core.
    pub const fn root() -> Spawn {
        Spawn { parent: None, hint: Affinity::Any, critical: false, counter: None }
    }

    /// A job of `parent`'s, on its core.
    pub const fn child(parent: JobId) -> Spawn {
        Spawn { parent: Some(parent), hint: Affinity::Spawner, critical: false, counter: None }
    }

    pub const fn hint(mut self, hint: Affinity) -> Spawn {
        self.hint = hint;
        self
    }

    pub const fn critical(mut self, critical: bool) -> Spawn {
        self.critical = critical;
        self
    }

    pub const fn counted(mut self, counter: CounterId) -> Spawn {
        self.counter = Some(counter);
        self
    }
}

/// What a suspended job waits for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Wait {
    /// The job's end.
    Job(JobId),
    /// The key's current flight ([`Sched::claim`]); with none, nothing.
    Key(Key),
    /// Every job counted towards it ending.
    Counter(CounterId),
    /// The clock reaching this many nanoseconds.
    Until(u64),
}

/// What the scheduler asks of its driver.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Decision {
    /// The core, free now (it reported a job's end, wait or move, or was idle), runs or resumes
    /// the job. The driver clears the core's move flag.
    Run { core: CoreId, job: JobId },
    /// The job is handed to the core: a message to its inbox. A parked core is unparked and
    /// gets [`Decision::Run`] for it at its [`Sched::core_idle`]; a busy core takes it at its
    /// next pick, unless an idle core steals it first.
    Wake { core: CoreId, job: JobId },
    /// Set the core's move flag: the job running there checks it (one relaxed load) at its
    /// next query miss, never on a hit, and calls [`Sched::yielded`] when it is set.
    Yield { core: CoreId, job: JobId },
}

/// What [`Sched::suspended`] did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Suspend {
    /// The job waits and its core runs something else.
    Waiting,
    /// What it waits for is done already: the job goes on, on its core.
    Ready,
}

/// What [`Sched::yielded`] did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Yielded {
    /// The job is handed to a bigger core; its own runs something else.
    Moved,
    /// No bigger core is idle any more: the job goes on where it is.
    Stay,
}

/// A wait refused because it closes a cycle of waits: `jobs[0]` asked to wait, each job waits
/// for the next (its job, its key's claimer, one of its counter's jobs) and the last for the
/// first. The asking job goes on running and must give way (unwind, release, be posted again);
/// nothing in the scheduler changed.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error, off every hot path
pub struct Cycle {
    pub jobs: Vec<JobId>,
}

impl fmt::Display for Cycle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a wait would close a cycle of waits: ")?;
        for (at, job) in self.jobs.iter().enumerate() {
            if at > 0 {
                f.write_str(" -> ")?;
            }
            write!(f, "job {}", job.0)?;
        }
        if let Some(first) = self.jobs.first() {
            write!(f, " -> job {}", first.0)?;
        }
        Ok(())
    }
}

impl std::error::Error for Cycle {}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum JobState {
    /// Never spawned.
    Vacant,
    /// Runnable: in a core's slot, inbox or queues, or handed to a woken core.
    Queued,
    Running,
    Suspended,
    Done,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum CoreState {
    Busy,
    /// Nothing to run anywhere it may look; waits for a [`Decision::Wake`].
    Parked,
    /// Handed a job by a [`Decision::Wake`]; runs it at its [`Sched::core_idle`].
    Woken,
}

/// What a suspended job's wait is on, as stored.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Target {
    None,
    Job(u32),
    /// A key entry's index.
    Key(u32),
    Counter(u32),
    Until,
}

const _: () = assert!(size_of::<Target>() == 8);

/// A job's hot fields: every one a spawn, a pick, a wait or an end reads or writes, in one
/// record, so each touches one cache line of it.
#[derive(Clone, Copy)]
struct Job {
    state: JobState,
    critical: bool,
    /// Queued: the core that holds it; running: its core; suspended or done: where it last ran.
    core: u32,
    wait: Target,
    /// Head of the list of jobs waiting for this one, linked through `next`.
    waiters: u32,
    next: u32,
    /// Head of the list of key entries this job claimed, linked through `key_next_claim`.
    claims: u32,
}

const _: () = assert!(size_of::<Job>() <= 32);

const VACANT: Job = Job {
    state: JobState::Vacant,
    critical: false,
    core: NONE,
    wait: Target::None,
    waiters: NONE,
    next: NONE,
    claims: NONE,
};

/// A job's cold fields: only counted jobs and the cycle check touch them.
#[derive(Clone, Copy)]
struct Cold {
    counter: u32,
    /// Its links in its counter's list of jobs.
    item_prev: u32,
    item_next: u32,
    /// The cycle check's visit mark, and the job whose wait reached it.
    mark: u32,
    via: u32,
}

const COLD: Cold = Cold { counter: NONE, item_prev: NONE, item_next: NONE, mark: 0, via: NONE };

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CounterState {
    Free,
    Live,
    /// Its owner let it go while jobs still count towards it; freed when the last ends.
    Dropped,
}

/// Counts of what the scheduler decided, for a driver's report.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Stats {
    pub runs: u64,
    pub wakes: u64,
    /// By [`Level`] from thief to victim.
    pub steals: [u64; LEVELS],
    pub cycles: u64,
    pub yields: u64,
    pub moves: u64,
}

/// A steal as the tests see it: who took from whom, and how many jobs were stealable on the
/// thief's node (other than the victim's) at that moment, counted by a scan of every core.
#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct StealRecord {
    thief: u32,
    victim: u32,
    on_thief_node: usize,
}

pub struct Sched {
    cores: Cores,
    now: u64,
    decisions: Vec<Decision>,
    stats: Stats,

    // Per core.
    core_state: Vec<CoreState>,
    running: Vec<u32>,
    reserved: Vec<u32>,
    slot: Vec<u32>,
    lifo_runs: Vec<u8>,
    inbox: Vec<VecDeque<u32>>,
    queue: Vec<VecDeque<u32>>,
    /// Critical-path jobs, taken before `queue`.
    urgent: Vec<VecDeque<u32>>,
    /// Bit per parked core.
    parked: Vec<u64>,
    parked_count: u32,
    /// Bit per core with a job in its slot, inbox or queues.
    stealable: Vec<u64>,
    /// Bit per core below the top capacity running a critical job.
    critical_small: Vec<u64>,
    /// Bit per core asked to move its job and not answered yet.
    yield_sent: Vec<u64>,

    /// By job id.
    jobs: Vec<Job>,
    cold: Vec<Cold>,
    epoch: u32,
    /// The cycle check's stack of suspended jobs to follow.
    dfs: Vec<u32>,

    // Per key flight.
    keys: FxHashMap<Key, u32>,
    key_name: Vec<Key>,
    key_claimer: Vec<u32>,
    key_waiters: Vec<u32>,
    key_next_claim: Vec<u32>,
    key_free: Vec<u32>,

    // Per counter.
    counter_state: Vec<CounterState>,
    counter_left: Vec<u32>,
    counter_waiters: Vec<u32>,
    counter_head: Vec<u32>,
    counter_free: Vec<u32>,

    timers: Wheel<u32>,
    /// What a step collects before it acts on it: waiters woken, timers fired.
    scratch: Vec<u32>,

    #[cfg(test)]
    pub(crate) steals: Vec<StealRecord>,
}

impl Sched {
    /// Every core parked, room for `jobs` job ids before a column grows.
    pub fn new(cores: Cores, jobs: usize) -> Sched {
        let n = cores.len();
        let words = cores.words();
        let mut parked = Vec::with_capacity(words);
        parked.extend_from_slice(cores.all());
        let zeros = |words: usize| {
            let mut bits = Vec::with_capacity(words);
            bits.resize(words, 0u64);
            bits
        };
        let mut inbox = Vec::with_capacity(n);
        let mut queue = Vec::with_capacity(n);
        let mut urgent = Vec::with_capacity(n);
        for _ in 0..n {
            inbox.push(VecDeque::with_capacity(64));
            queue.push(VecDeque::with_capacity(256));
            urgent.push(VecDeque::with_capacity(64));
        }
        let mut sched = Sched {
            now: 0,
            decisions: Vec::with_capacity(64),
            stats: Stats::default(),
            core_state: Vec::with_capacity(n),
            running: Vec::with_capacity(n),
            reserved: Vec::with_capacity(n),
            slot: Vec::with_capacity(n),
            lifo_runs: Vec::with_capacity(n),
            inbox,
            queue,
            urgent,
            parked,
            parked_count: n as u32,
            stealable: zeros(words),
            critical_small: zeros(words),
            yield_sent: zeros(words),
            jobs: Vec::with_capacity(jobs),
            cold: Vec::with_capacity(jobs),
            epoch: 0,
            dfs: Vec::with_capacity(64),
            keys: FxHashMap::with_capacity_and_hasher(jobs, Default::default()),
            key_name: Vec::with_capacity(jobs),
            key_claimer: Vec::with_capacity(jobs),
            key_waiters: Vec::with_capacity(jobs),
            key_next_claim: Vec::with_capacity(jobs),
            key_free: Vec::with_capacity(jobs),
            counter_state: Vec::with_capacity(64),
            counter_left: Vec::with_capacity(64),
            counter_waiters: Vec::with_capacity(64),
            counter_head: Vec::with_capacity(64),
            counter_free: Vec::with_capacity(64),
            timers: Wheel::new(),
            scratch: Vec::with_capacity(64),
            cores,
            #[cfg(test)]
            steals: Vec::new(), // alloc: test bookkeeping
        };
        sched.core_state.resize(n, CoreState::Parked);
        sched.running.resize(n, NONE);
        sched.reserved.resize(n, NONE);
        sched.slot.resize(n, NONE);
        sched.lifo_runs.resize(n, 0);
        sched
    }

    pub fn cores(&self) -> &Cores {
        &self.cores
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    pub fn now(&self) -> u64 {
        self.now
    }

    pub fn job_state(&self, job: JobId) -> JobState {
        self.state_of(job.0)
    }

    /// Where the job is held, runs, or last ran.
    pub fn job_core(&self, job: JobId) -> Option<CoreId> {
        let core = self.jobs.get(job.0 as usize).map_or(NONE, |it| it.core);
        (core != NONE).then_some(CoreId(core))
    }

    pub fn is_critical(&self, job: JobId) -> bool {
        self.jobs.get(job.0 as usize).is_some_and(|it| it.critical)
    }

    fn state_of(&self, job: u32) -> JobState {
        self.jobs.get(job as usize).map_or(JobState::Vacant, |it| it.state)
    }

    pub fn core_state(&self, core: CoreId) -> CoreState {
        self.core_state[core.0 as usize]
    }

    pub fn running(&self, core: CoreId) -> Option<JobId> {
        let job = self.running[core.0 as usize];
        (job != NONE).then_some(JobId(job))
    }

    /// The decisions made since the last call, in the order made.
    pub fn take_decisions(&mut self) -> std::vec::Drain<'_, Decision> {
        self.decisions.drain(..)
    }

    /// When the next timed wait is due: the driver calls [`Sched::advance_clock`] by then.
    pub fn next_deadline(&mut self) -> Option<u64> {
        self.timers.next_deadline()
    }

    /// `job` is new, to run where `how` says.
    ///
    /// # Panics
    ///
    /// Where `job` is live (spawned and not done), its parent is not running, its counter is
    /// not live, or its hint names a core, group or node the shape does not have.
    pub fn spawn(&mut self, job: JobId, how: Spawn) {
        assert!(job.0 < MAX_JOBS, "job {} is past the scheduler's {MAX_JOBS} ids", job.0);
        self.ensure(job.0);
        let j = job.0 as usize;
        assert!(
            matches!(self.jobs[j].state, JobState::Vacant | JobState::Done),
            "job {} spawned while it is live ({:?})",
            job.0,
            self.jobs[j].state
        );
        let spawner = how.parent.map(|parent| {
            assert!(
                self.state_of(parent.0) == JobState::Running,
                "job {}'s parent {} is not running",
                job.0,
                parent.0
            );
            self.jobs[parent.0 as usize].core
        });
        let home = match how.hint {
            Affinity::Spawner => match spawner {
                Some(core) => core,
                None => self.least_loaded(|_, _| true),
            },
            Affinity::Core(core) => {
                assert!((core.0 as usize) < self.cores.len(), "job {} hinted to core {}, past the shape", job.0, core.0);
                core.0
            }
            Affinity::L3(l3) => match spawner.filter(|&core| self.cores.in_l3(core, l3)) {
                Some(core) => core,
                None => self.least_loaded(|cores, core| cores.in_l3(core, l3)),
            },
            Affinity::Node(node) => match spawner.filter(|&core| self.cores.in_node(core, node)) {
                Some(core) => core,
                None => self.least_loaded(|cores, core| cores.in_node(core, node)),
            },
            Affinity::Any => self.least_loaded(|_, _| true),
        };
        self.jobs[j] = Job { state: JobState::Queued, critical: how.critical, core: home, ..VACANT };
        self.cold[j] = COLD;
        if let Some(counter) = how.counter {
            self.attach(job.0, counter.0);
        }
        if spawner == Some(home) {
            self.enqueue(home, job.0);
            self.refresh(home);
            self.balance(home);
        } else {
            self.place_remote(job.0, home);
        }
    }

    /// A counter at zero, for a parallel map's jobs to count towards ([`Spawn::counted`]).
    pub fn counter(&mut self) -> CounterId {
        let counter = match self.counter_free.pop() {
            Some(counter) => counter,
            None => {
                self.counter_state.push(CounterState::Free);
                self.counter_left.push(0);
                self.counter_waiters.push(NONE);
                self.counter_head.push(NONE);
                (self.counter_state.len() - 1) as u32
            }
        };
        let c = counter as usize;
        self.counter_state[c] = CounterState::Live;
        self.counter_left[c] = 0;
        self.counter_waiters[c] = NONE;
        self.counter_head[c] = NONE;
        CounterId(counter)
    }

    /// The counter's owner is done with it; it is freed once no job counts towards it.
    ///
    /// # Panics
    ///
    /// Where the counter is not live or a job still waits on it.
    pub fn drop_counter(&mut self, counter: CounterId) {
        let c = counter.0 as usize;
        assert!(self.counter_state.get(c) == Some(&CounterState::Live), "counter {} dropped while not live", counter.0);
        assert!(self.counter_waiters[c] == NONE, "counter {} dropped while a job waits on it", counter.0);
        if self.counter_left[c] == 0 {
            self.free_counter(counter.0);
        } else {
            self.counter_state[c] = CounterState::Dropped;
        }
    }

    /// `by` (running, or the driver for `None`: IO, a child process) makes `key`'s value.
    /// `false` when the key has a flight already: wait for it instead.
    ///
    /// # Panics
    ///
    /// Where `by` is not running.
    pub fn claim(&mut self, key: Key, by: Option<JobId>) -> bool {
        if let Some(job) = by {
            assert!(self.state_of(job.0) == JobState::Running, "job {} claimed a key while not running", job.0);
        }
        if self.keys.contains_key(&key) {
            return false;
        }
        let entry = match self.key_free.pop() {
            Some(entry) => {
                self.key_name[entry as usize] = key;
                entry
            }
            None => {
                self.key_name.push(key);
                self.key_claimer.push(NONE);
                self.key_waiters.push(NONE);
                self.key_next_claim.push(NONE);
                (self.key_name.len() - 1) as u32
            }
        };
        let e = entry as usize;
        self.key_claimer[e] = by.map_or(NONE, |job| job.0);
        self.key_waiters[e] = NONE;
        self.key_next_claim[e] = NONE;
        if let Some(job) = by {
            self.key_next_claim[e] = self.jobs[job.0 as usize].claims;
            self.jobs[job.0 as usize].claims = entry;
        }
        self.keys.insert(key, entry);
        true
    }

    /// `job`, running, waits for `on`; its core runs something else.
    ///
    /// # Errors
    ///
    /// The [`Cycle`] the wait would close; the job goes on running and nothing changed.
    ///
    /// # Panics
    ///
    /// Where `job` is not running, or waits for a job never spawned or a counter not live.
    pub fn suspended(&mut self, job: JobId, on: Wait) -> Result<Suspend, Cycle> {
        let j = job.0 as usize;
        assert!(self.state_of(job.0) == JobState::Running, "job {} suspended while not running", job.0);
        let target = match on {
            Wait::Job(other) => match self.state_of(other.0) {
                JobState::Done => return Ok(Suspend::Ready),
                JobState::Vacant => panic!("job {} waits for job {}, never spawned", job.0, other.0),
                _ => Target::Job(other.0),
            },
            Wait::Key(key) => match self.keys.get(&key) {
                None => return Ok(Suspend::Ready),
                Some(&entry) => Target::Key(entry),
            },
            Wait::Counter(counter) => {
                let c = counter.0 as usize;
                assert!(
                    self.counter_state.get(c) == Some(&CounterState::Live),
                    "job {} waits on counter {}, not live",
                    job.0,
                    counter.0
                );
                if self.counter_left[c] == 0 {
                    return Ok(Suspend::Ready);
                }
                Target::Counter(counter.0)
            }
            Wait::Until(at) => {
                if at <= self.now {
                    return Ok(Suspend::Ready);
                }
                Target::Until
            }
        };
        if let Some(from) = self.closes_cycle(job.0, target) {
            self.stats.cycles += 1;
            return Err(self.cycle(job.0, from));
        }
        match target {
            Target::Job(other) => {
                self.jobs[j].next = self.jobs[other as usize].waiters;
                self.jobs[other as usize].waiters = job.0;
            }
            Target::Key(entry) => {
                self.jobs[j].next = self.key_waiters[entry as usize];
                self.key_waiters[entry as usize] = job.0;
            }
            Target::Counter(counter) => {
                self.jobs[j].next = self.counter_waiters[counter as usize];
                self.counter_waiters[counter as usize] = job.0;
            }
            Target::Until => {
                let Wait::Until(at) = on else { unreachable!("an `Until` target is an `Until` wait") };
                self.timers.insert(at, job.0);
            }
            Target::None => unreachable!("a wait has a target"),
        }
        self.jobs[j].wait = target;
        self.jobs[j].state = JobState::Suspended;
        let core = self.jobs[j].core;
        self.vacate(core);
        self.pick(core);
        Ok(Suspend::Waiting)
    }

    /// `job`, running, ended, and with it every key it claimed. Its waiters and theirs, and
    /// those of its counter if it was the last, wake on its core, the first in its slot; the
    /// core runs its next job.
    ///
    /// # Panics
    ///
    /// Where `job` is not running.
    pub fn done(&mut self, job: JobId) {
        let j = job.0 as usize;
        assert!(self.state_of(job.0) == JobState::Running, "job {} done while not running", job.0);
        let core = self.jobs[j].core;
        self.jobs[j].state = JobState::Done;
        self.vacate(core);
        let mut woken = std::mem::take(&mut self.scratch);
        woken.clear();
        let head = std::mem::replace(&mut self.jobs[j].waiters, NONE);
        Sched::drain_list(head, &mut self.jobs, &mut woken);
        let mut claim = std::mem::replace(&mut self.jobs[j].claims, NONE);
        while claim != NONE {
            let next = self.key_next_claim[claim as usize];
            let head = std::mem::replace(&mut self.key_waiters[claim as usize], NONE);
            Sched::drain_list(head, &mut self.jobs, &mut woken);
            self.free_key(claim);
            claim = next;
        }
        let counter = self.cold[j].counter;
        if counter != NONE {
            self.detach(job.0);
            let c = counter as usize;
            self.counter_left[c] -= 1;
            if self.counter_left[c] == 0 {
                let head = std::mem::replace(&mut self.counter_waiters[c], NONE);
                Sched::drain_list(head, &mut self.jobs, &mut woken);
                if self.counter_state[c] == CounterState::Dropped {
                    self.free_counter(counter);
                }
            }
        }
        self.wake_local(core, &woken);
        self.scratch = woken;
        self.pick(core);
        self.balance(core);
    }

    /// `key`'s flight ended. Its waiters wake on its claimer's core, the first in its slot, or
    /// for a flight of the driver's, each on the core it last ran on.
    ///
    /// # Panics
    ///
    /// Where the key has no flight, or its claimer is not running.
    pub fn key_done(&mut self, key: Key) {
        let entry = self.keys.get(&key).copied().unwrap_or_else(|| panic!("key {key:?} done with no flight"));
        let e = entry as usize;
        let claimer = self.key_claimer[e];
        let mut woken = std::mem::take(&mut self.scratch);
        woken.clear();
        let head = std::mem::replace(&mut self.key_waiters[e], NONE);
        Sched::drain_list(head, &mut self.jobs, &mut woken);
        if claimer != NONE {
            let c = claimer as usize;
            assert!(self.jobs[c].state == JobState::Running, "key {key:?} done while its job {claimer} is not running");
            let mut at = self.jobs[c].claims;
            if at == entry {
                self.jobs[c].claims = self.key_next_claim[e];
            } else {
                while at != NONE {
                    let next = self.key_next_claim[at as usize];
                    if next == entry {
                        self.key_next_claim[at as usize] = self.key_next_claim[e];
                        break;
                    }
                    at = next;
                }
            }
            self.free_key(entry);
            let core = self.jobs[c].core;
            self.wake_local(core, &woken);
            self.balance(core);
        } else {
            self.free_key(entry);
            for &waiter in &woken {
                self.ready(waiter);
                let home = self.jobs[waiter as usize].core;
                self.place_remote(waiter, home);
            }
        }
        self.scratch = woken;
    }

    /// `job`, running, saw its core's move flag at a query miss. It moves to the nearest idle
    /// core of the highest capacity, if one is still idle.
    ///
    /// # Panics
    ///
    /// Where `job` is not running.
    pub fn yielded(&mut self, job: JobId) -> Yielded {
        let j = job.0 as usize;
        assert!(self.state_of(job.0) == JobState::Running, "job {} yielded while not running", job.0);
        let core = self.jobs[j].core;
        set(&mut self.yield_sent, core, false);
        let Some(big) = self.search_parked(core, Some(self.cores.big())) else { return Yielded::Stay };
        self.vacate(core);
        self.jobs[j].state = JobState::Queued;
        self.hand(big, job.0);
        self.stats.moves += 1;
        self.pick(core);
        Yielded::Moved
    }

    /// The profiler says whether `job` is on the critical path. A running job moves at its
    /// next miss if it should; a queued one keeps its queue until it next moves.
    pub fn set_critical(&mut self, job: JobId, critical: bool) {
        let j = job.0 as usize;
        assert!(j < self.jobs.len(), "job {} was never spawned", job.0);
        self.jobs[j].critical = critical;
        if self.jobs[j].state == JobState::Running {
            let core = self.jobs[j].core;
            set(&mut self.critical_small, core, critical && !self.cores.is_big(core));
            if !critical {
                set(&mut self.yield_sent, core, false);
            }
            self.maybe_yield();
        }
    }

    /// `core`'s thread is free: it started, was woken, or woke for no reason. It runs what it
    /// was handed, or its own next job, or steals; with nothing, it parks.
    ///
    /// # Panics
    ///
    /// Where `core` is running a job.
    pub fn core_idle(&mut self, core: CoreId) {
        let c = core.0 as usize;
        assert!(self.running[c] == NONE, "core {} idle while it runs job {}", core.0, self.running[c]);
        if self.core_state[c] == CoreState::Parked {
            self.unpark(core.0);
        }
        self.core_state[c] = CoreState::Busy;
        self.pick(core.0);
    }

    /// The clock reads `now` nanoseconds: every wait due by then is runnable, on the core its
    /// job last ran on.
    pub fn advance_clock(&mut self, now: u64) {
        self.now = self.now.max(now);
        let mut fired = std::mem::take(&mut self.scratch);
        fired.clear();
        self.timers.advance(self.now, |_, job| fired.push(job));
        for &job in &fired {
            self.ready(job);
            let home = self.jobs[job as usize].core;
            self.place_remote(job, home);
        }
        self.scratch = fired;
    }

    fn ensure(&mut self, job: u32) {
        let need = job as usize + 1;
        if self.jobs.len() < need {
            self.jobs.resize(need, VACANT);
            self.cold.resize(need, COLD);
        }
    }

    fn free_key(&mut self, entry: u32) {
        let e = entry as usize;
        self.keys.remove(&self.key_name[e]);
        self.key_claimer[e] = NONE;
        self.key_waiters[e] = NONE;
        self.key_next_claim[e] = NONE;
        self.key_free.push(entry);
    }

    fn free_counter(&mut self, counter: u32) {
        let c = counter as usize;
        self.counter_state[c] = CounterState::Free;
        self.counter_head[c] = NONE;
        self.counter_free.push(counter);
    }

    /// Counts `job` towards `counter`, at the head of its list.
    fn attach(&mut self, job: u32, counter: u32) {
        let c = counter as usize;
        assert!(
            self.counter_state.get(c) == Some(&CounterState::Live),
            "job {job} counted towards counter {counter}, not live"
        );
        self.counter_left[c] += 1;
        let head = self.counter_head[c];
        self.cold[job as usize] = Cold { counter, item_prev: NONE, item_next: head, ..COLD };
        if head != NONE {
            self.cold[head as usize].item_prev = job;
        }
        self.counter_head[c] = job;
    }

    fn detach(&mut self, job: u32) {
        let Cold { counter, item_prev, item_next, .. } = self.cold[job as usize];
        match item_prev {
            NONE => self.counter_head[counter as usize] = item_next,
            prev => self.cold[prev as usize].item_next = item_next,
        }
        if item_next != NONE {
            self.cold[item_next as usize].item_prev = item_prev;
        }
        let cold = &mut self.cold[job as usize];
        cold.counter = NONE;
        cold.item_prev = NONE;
        cold.item_next = NONE;
    }

    /// The jobs of a waiter list from `head` into `into`, unlinked.
    fn drain_list(head: u32, jobs: &mut [Job], into: &mut Vec<u32>) {
        let mut at = head;
        while at != NONE {
            into.push(at);
            at = std::mem::replace(&mut jobs[at as usize].next, NONE);
        }
    }

    /// A suspended job is runnable again.
    fn ready(&mut self, job: u32) {
        let j = job as usize;
        debug_assert_eq!(self.jobs[j].state, JobState::Suspended);
        self.jobs[j].state = JobState::Queued;
        self.jobs[j].wait = Target::None;
    }

    /// `core` no longer runs a job.
    fn vacate(&mut self, core: u32) {
        self.running[core as usize] = NONE;
        set(&mut self.critical_small, core, false);
        set(&mut self.yield_sent, core, false);
    }

    /// On `core`'s critical queue or its other one, by the job's class.
    fn enqueue(&mut self, core: u32, job: u32) {
        self.jobs[job as usize].core = core;
        if self.jobs[job as usize].critical {
            self.urgent[core as usize].push_back(job);
        } else {
            self.queue[core as usize].push_back(job);
        }
    }

    /// Waiters woken by what `core` finished: the first in its slot (the one there before goes
    /// to its queue), the rest on its queues.
    fn wake_local(&mut self, core: u32, woken: &[u32]) {
        let c = core as usize;
        for (at, &waiter) in woken.iter().enumerate() {
            self.ready(waiter);
            if at == 0 {
                self.jobs[waiter as usize].core = core;
                let before = std::mem::replace(&mut self.slot[c], waiter);
                if before != NONE {
                    self.enqueue(core, before);
                }
            } else {
                self.enqueue(core, waiter);
            }
        }
        self.refresh(core);
    }

    /// `job` handed to `home` from elsewhere: to the nearest parked core, else a message to
    /// `home`'s inbox.
    fn place_remote(&mut self, job: u32, home: u32) {
        let critical = self.jobs[job as usize].critical;
        match self.nearest_parked(home, critical) {
            Some(parked) => self.hand(parked, job),
            None => {
                self.inbox[home as usize].push_back(job);
                self.jobs[job as usize].core = home;
                self.refresh(home);
                self.decisions.push(Decision::Wake { core: CoreId(home), job: JobId(job) });
                self.stats.wakes += 1;
            }
        }
    }

    /// Hands `job` to `core`, parked.
    fn hand(&mut self, core: u32, job: u32) {
        self.unpark(core);
        self.core_state[core as usize] = CoreState::Woken;
        self.reserved[core as usize] = job;
        self.jobs[job as usize].core = core;
        self.decisions.push(Decision::Wake { core: CoreId(core), job: JobId(job) });
        self.stats.wakes += 1;
    }

    fn unpark(&mut self, core: u32) {
        debug_assert_eq!(self.core_state[core as usize], CoreState::Parked);
        set(&mut self.parked, core, false);
        self.parked_count -= 1;
    }

    fn park(&mut self, core: u32) {
        let c = core as usize;
        self.core_state[c] = CoreState::Parked;
        self.lifo_runs[c] = 0;
        set(&mut self.parked, core, true);
        self.parked_count += 1;
        if self.cores.is_big(core) {
            self.maybe_yield();
        }
    }

    /// While a core of the top capacity is parked, every small core running a critical job
    /// not asked yet is asked to move it.
    fn maybe_yield(&mut self) {
        if self.parked_count == 0 || !any_in(&self.parked, self.cores.big()) {
            return;
        }
        for w in 0..self.critical_small.len() {
            let mut ask = self.critical_small[w] & !self.yield_sent[w];
            while ask != 0 {
                let core = (w * 64) as u32 + ask.trailing_zeros();
                ask &= ask - 1;
                set(&mut self.yield_sent, core, true);
                let job = self.running[core as usize];
                self.decisions.push(Decision::Yield { core: CoreId(core), job: JobId(job) });
                self.stats.yields += 1;
            }
        }
    }

    fn refresh(&mut self, core: u32) {
        let c = core as usize;
        let has = self.slot[c] != NONE
            || !self.inbox[c].is_empty()
            || !self.queue[c].is_empty()
            || !self.urgent[c].is_empty();
        set(&mut self.stealable, core, has);
    }

    /// The nearest parked core to `core` for a job of this class, itself first: for a critical
    /// job the nearest of the top capacity, then any; for another, within each tier a smaller
    /// core first.
    fn nearest_parked(&self, core: u32, critical: bool) -> Option<u32> {
        if self.parked_count == 0 {
            return None;
        }
        let here = get(&self.parked, core);
        if critical {
            if here && self.cores.is_big(core) {
                return Some(core);
            }
            if let Some(big) = self.search_parked(core, Some(self.cores.big())) {
                return Some(big);
            }
            if here {
                return Some(core);
            }
            return self.search_parked(core, None);
        }
        if here {
            return Some(core);
        }
        let start = core as usize + 1;
        self.cores.tiers(core).find_map(|(_, mask)| {
            first_from(&self.parked, mask, Some(self.cores.small()), start)
                .or_else(|| first_from(&self.parked, mask, None, start))
        })
    }

    /// The nearest parked core other than `core` that `filter` allows, tier by tier.
    fn search_parked(&self, core: u32, filter: Option<&[u64]>) -> Option<u32> {
        if self.parked_count == 0 {
            return None;
        }
        let start = core as usize + 1;
        self.cores.tiers(core).find_map(|(_, mask)| first_from(&self.parked, mask, filter, start))
    }

    /// A parked core of those `keep` accepts, else the one with the fewest jobs (running,
    /// slot, inbox and queues), the lowest id on a tie.
    fn least_loaded(&self, keep: impl Fn(&Cores, u32) -> bool) -> u32 {
        let mut best: Option<(usize, u32)> = None;
        for core in 0..self.cores.len() as u32 {
            if !keep(&self.cores, core) {
                continue;
            }
            let c = core as usize;
            let load = match self.core_state[c] {
                CoreState::Parked => 0,
                CoreState::Woken => 1,
                CoreState::Busy => {
                    usize::from(self.running[c] != NONE)
                        + usize::from(self.slot[c] != NONE)
                        + self.inbox[c].len()
                        + self.queue[c].len()
                        + self.urgent[c].len()
                }
            };
            if best.is_none_or(|(least, _)| load < least) {
                best = Some((load, core));
                if load == 0 {
                    break;
                }
            }
        }
        best.map(|(_, core)| core).expect("a placement hint names a group or node with no core")
    }

    /// `core` is free: it runs its next job, or steals, or parks.
    fn pick(&mut self, core: u32) {
        let c = core as usize;
        debug_assert_eq!(self.running[c], NONE);
        let job = match self.take_own(core) {
            Some(job) => Some(job),
            None => self.steal_for(core),
        };
        match job {
            Some(job) => {
                self.core_state[c] = CoreState::Busy;
                self.running[c] = job;
                self.jobs[job as usize].state = JobState::Running;
                self.jobs[job as usize].core = core;
                self.decisions.push(Decision::Run { core: CoreId(core), job: JobId(job) });
                self.stats.runs += 1;
                if self.jobs[job as usize].critical && !self.cores.is_big(core) {
                    set(&mut self.critical_small, core, true);
                    self.maybe_yield();
                }
            }
            None => self.park(core),
        }
    }

    fn take_own(&mut self, core: u32) -> Option<u32> {
        let c = core as usize;
        let reserved = std::mem::replace(&mut self.reserved[c], NONE);
        if reserved != NONE {
            return Some(reserved);
        }
        if self.slot[c] != NONE && self.lifo_runs[c] < LIFO_RUNS {
            self.lifo_runs[c] += 1;
            let job = std::mem::replace(&mut self.slot[c], NONE);
            self.refresh(core);
            return Some(job);
        }
        self.lifo_runs[c] = 0;
        let job = self.inbox[c]
            .pop_front()
            .or_else(|| self.urgent[c].pop_back())
            .or_else(|| self.queue[c].pop_back())
            .or_else(|| (self.slot[c] != NONE).then(|| std::mem::replace(&mut self.slot[c], NONE)));
        self.refresh(core);
        job
    }

    /// The nearest core with work, tier by tier; the thief runs the oldest of half its queue
    /// and keeps the rest.
    fn steal_for(&mut self, thief: u32) -> Option<u32> {
        let start = thief as usize + 1;
        let (level, victim) = self
            .cores
            .tiers(thief)
            .find_map(|(level, mask)| first_from(&self.stealable, mask, None, start).map(|victim| (level, victim)))?;
        self.stats.steals[level as usize] += 1;
        self.record_steal(thief, victim);
        Some(self.steal_half(thief, victim))
    }

    fn steal_half(&mut self, thief: u32, victim: u32) -> u32 {
        let (t, v) = (thief as usize, victim as usize);
        let big = self.cores.is_big(thief);
        let from_urgent = if big { !self.urgent[v].is_empty() } else { self.queue[v].is_empty() && !self.urgent[v].is_empty() };
        let job = if from_urgent || !self.queue[v].is_empty() {
            let queues = if from_urgent { &mut self.urgent } else { &mut self.queue };
            let take = queues[v].len().div_ceil(2);
            let first = queues[v].pop_front().expect("a victim's queue has a job");
            for _ in 1..take {
                let job = queues[v].pop_front().expect("half a queue is in it");
                queues[t].push_back(job);
                self.jobs[job as usize].core = thief;
            }
            first
        } else if let Some(job) = self.inbox[v].pop_front() {
            job
        } else {
            let job = std::mem::replace(&mut self.slot[v], NONE);
            assert!(job != NONE, "a stealable core has a job");
            job
        };
        self.refresh(victim);
        self.refresh(thief);
        job
    }

    /// The oldest job `core` holds, critical ones first: what a parked core is handed.
    fn take_oldest(&mut self, core: u32) -> u32 {
        let c = core as usize;
        let job = self.urgent[c]
            .pop_front()
            .or_else(|| self.queue[c].pop_front())
            .or_else(|| self.inbox[c].pop_front())
            .or_else(|| (self.slot[c] != NONE).then(|| std::mem::replace(&mut self.slot[c], NONE)))
            .expect("a stealable core has a job");
        self.refresh(core);
        job
    }

    /// While a core is parked, `core`'s stealable jobs go one each to the nearest parked cores.
    fn balance(&mut self, core: u32) {
        while self.parked_count > 0 && get(&self.stealable, core) {
            let job = self.take_oldest(core);
            let thief = self
                .nearest_parked(core, self.jobs[job as usize].critical)
                .expect("a core is parked");
            let level = self.cores.level(thief, core);
            self.stats.steals[level as usize] += 1;
            self.record_steal(thief, core);
            self.hand(thief, job);
        }
    }

    /// Whether the jobs `target` reaches, followed through every suspended job's wait, reach
    /// `job`: the job whose wait closes the cycle, if so.
    fn closes_cycle(&mut self, job: u32, target: Target) -> Option<u32> {
        self.epoch = self.epoch.wrapping_add(1);
        if self.epoch == 0 {
            for cold in &mut self.cold {
                cold.mark = 0;
            }
            self.epoch = 1;
        }
        self.cold[job as usize].mark = self.epoch;
        let mut stack = std::mem::take(&mut self.dfs);
        stack.clear();
        let mut found = self.expand(job, job, target, &mut stack);
        while found.is_none() {
            let Some(from) = stack.pop() else { break };
            let wait = self.jobs[from as usize].wait;
            found = self.expand(job, from, wait, &mut stack);
        }
        self.dfs = stack;
        found
    }

    fn expand(&mut self, asker: u32, from: u32, target: Target, stack: &mut Vec<u32>) -> Option<u32> {
        match target {
            Target::Job(other) => self.visit(asker, from, other, stack),
            Target::Key(entry) => match self.key_claimer[entry as usize] {
                NONE => None,
                claimer => self.visit(asker, from, claimer, stack),
            },
            Target::Counter(counter) => {
                let mut at = self.counter_head[counter as usize];
                while at != NONE {
                    let next = self.cold[at as usize].item_next;
                    if let Some(found) = self.visit(asker, from, at, stack) {
                        return Some(found);
                    }
                    at = next;
                }
                None
            }
            Target::Until | Target::None => None,
        }
    }

    fn visit(&mut self, asker: u32, from: u32, job: u32, stack: &mut Vec<u32>) -> Option<u32> {
        if job == asker {
            return Some(from);
        }
        let cold = &mut self.cold[job as usize];
        if cold.mark == self.epoch {
            return None;
        }
        cold.mark = self.epoch;
        cold.via = from;
        if self.jobs[job as usize].state == JobState::Suspended {
            stack.push(job);
        }
        None
    }

    /// The cycle `closes_cycle` found, from the asker: back from `from` along each job's `via`.
    fn cycle(&self, asker: u32, from: u32) -> Cycle {
        let mut jobs = Vec::with_capacity(4); // alloc: an error, off every hot path
        let mut at = from;
        while at != asker {
            jobs.push(JobId(at));
            at = self.cold[at as usize].via;
        }
        jobs.push(JobId(asker));
        jobs.reverse();
        Cycle { jobs }
    }

    #[cfg(not(test))]
    fn record_steal(&mut self, _thief: u32, _victim: u32) {}

    #[cfg(test)]
    fn record_steal(&mut self, thief: u32, victim: u32) {
        let node = self.cores.info(thief).node;
        let mut on_thief_node = 0;
        for core in 0..self.cores.len() as u32 {
            if core == victim || core == thief || self.cores.info(core).node != node {
                continue;
            }
            let c = core as usize;
            on_thief_node += usize::from(self.slot[c] != NONE)
                + self.inbox[c].len()
                + self.queue[c].len()
                + self.urgent[c].len();
        }
        self.steals.push(StealRecord { thief, victim, on_thief_node });
    }

    /// Every structure agrees with every other, and no job is stealable while a core is
    /// parked.
    #[cfg(test)]
    pub(crate) fn audit(&self) {
        let n = self.cores.len();
        let mut seen = vec![0u32; self.jobs.len()]; // alloc: test check
        let mut parked = 0;
        for c in 0..n {
            let core = c as u32;
            let held = self.slot[c] != NONE
                || !self.inbox[c].is_empty()
                || !self.queue[c].is_empty()
                || !self.urgent[c].is_empty();
            assert_eq!(get(&self.stealable, core), held, "core {c}'s stealable bit");
            assert_eq!(get(&self.parked, core), self.core_state[c] == CoreState::Parked, "core {c}'s parked bit");
            match self.core_state[c] {
                CoreState::Parked => {
                    parked += 1;
                    assert!(!held && self.running[c] == NONE && self.reserved[c] == NONE, "parked core {c} holds work");
                }
                CoreState::Woken => assert!(self.running[c] == NONE && self.reserved[c] != NONE, "woken core {c}"),
                CoreState::Busy => assert!(self.running[c] != NONE && self.reserved[c] == NONE, "busy core {c}"),
            }
            let critical_small = self.running[c] != NONE
                && self.jobs[self.running[c] as usize].critical
                && !self.cores.is_big(core);
            assert_eq!(get(&self.critical_small, core), critical_small, "core {c}'s critical bit");
            assert!(!get(&self.yield_sent, core) || critical_small, "core {c} asked to move no critical job");
            if self.running[c] != NONE {
                let job = self.running[c] as usize;
                assert_eq!(self.jobs[job].state, JobState::Running, "core {c} runs job {job}");
                assert_eq!(self.jobs[job].core, core, "job {job}'s core");
            }
            for &job in &self.urgent[c] {
                assert!(self.jobs[job as usize].critical, "job {job} on core {c}'s critical queue");
            }
            let queued = self.inbox[c].iter().chain(&self.queue[c]).chain(&self.urgent[c]).copied();
            let queued = queued.chain([self.slot[c], self.reserved[c]].into_iter().filter(|&it| it != NONE));
            for job in queued {
                seen[job as usize] += 1;
                assert_eq!(self.jobs[job as usize].state, JobState::Queued, "job {job} held by core {c}");
                assert_eq!(self.jobs[job as usize].core, core, "job {job} held by core {c}");
            }
        }
        assert_eq!(parked, self.parked_count, "parked count");
        if parked > 0 {
            assert!(self.stealable.iter().all(|&it| it == 0), "a job is stealable while a core is parked");
        }
        for (job, record) in self.jobs.iter().enumerate() {
            let state = record.state;
            let held = seen[job];
            assert_eq!(held, u32::from(state == JobState::Queued), "job {job} ({state:?}) is held {held} times");
            assert_eq!(record.wait != Target::None, state == JobState::Suspended, "job {job}'s wait");
            let counter = self.cold[job].counter;
            if counter != NONE {
                assert!(matches!(state, JobState::Queued | JobState::Running | JobState::Suspended), "job {job} counted");
            }
        }
        for (c, &state) in self.counter_state.iter().enumerate() {
            let mut items = 0;
            let mut at = self.counter_head[c];
            while at != NONE {
                assert_eq!(self.cold[at as usize].counter, c as u32, "job {at} in counter {c}'s list");
                items += 1;
                at = self.cold[at as usize].item_next;
            }
            match state {
                CounterState::Free => assert_eq!(items, 0, "free counter {c}"),
                CounterState::Live | CounterState::Dropped => {
                    assert_eq!(items, self.counter_left[c], "counter {c}'s count");
                }
            }
            if state == CounterState::Dropped {
                assert!(items > 0 && self.counter_waiters[c] == NONE, "dropped counter {c}");
            }
        }
    }
}

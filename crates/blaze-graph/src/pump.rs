//! The rule for a wait inside a task (#1301, docs/v2/core.md "Issues this lane takes in"): a
//! thread that waits for what another task makes runs other ready jobs of its pool while it
//! waits (it **pumps**), and a wait that would close a cycle gives way at once with
//! [`Cancelled`], so the task that asked unwinds, releases what it holds and is posted again.
//! No wait can deadlock: every wait is checked against every other before it begins.
//!
//! The task a thread runs now is the top of a stack the pool keeps for the thread: a job the
//! thread pumps while it waits is pushed above the job that waits, and that one cannot go on
//! before it returns. Who waits for whom is one graph for the process, of two kinds of edge: a
//! task waits for the task that runs the key it asked ([`Flights::get_or_run`]), and a task
//! that pumped waits for the job above it on its thread. A wait whose edge would close a cycle
//! is refused. The graph is behind one lock, taken only when a wait begins or ends and when a
//! pumped job starts or ends; nothing of a run that waits for nobody takes it.

use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    thread::{self, Thread},
};

use rustc_hash::FxHashMap;

use crate::{Cancelled, Key};

/// How many jobs a thread runs above the job at the bottom of its stack, at most. A wait at
/// this depth parks without pumping; that cannot deadlock, since the cycle check still runs.
pub const PUMP_DEPTH: usize = 16;

/// How many times a waiter looks again before it parks.
pub(crate) const SPINS: u32 = 64;

const SHARDS: usize = 64;

static NEXT_TASK: AtomicU64 = AtomicU64::new(1);

thread_local! {
    /// The tasks this thread runs, the one running now last.
    static STACK: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
    /// What a thread with no task (a client's) is called in the wait graph.
    static CLIENT: Cell<u64> = const { Cell::new(0) };
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Runs `f` as a new task of this thread, above the one it runs (if any), which waits for it.
pub(crate) fn run_task<R>(f: impl FnOnce() -> R) -> R {
    struct Leave {
        below: Option<u64>,
    }
    impl Drop for Leave {
        fn drop(&mut self) {
            STACK.with_borrow_mut(Vec::pop);
            if let Some(below) = self.below {
                lock(&WAITS).pumped(below, None);
            }
        }
    }
    let me = NEXT_TASK.fetch_add(1, Ordering::Relaxed);
    let below = STACK.with_borrow_mut(|stack| {
        let below = stack.last().copied();
        stack.push(me);
        below
    });
    if let Some(below) = below {
        lock(&WAITS).pumped(below, Some(me));
    }
    let _leave = Leave { below };
    f()
}

/// How many jobs this thread runs above the one at the bottom of its stack.
pub fn depth() -> usize {
    STACK.with_borrow(|stack| stack.len().saturating_sub(1))
}

fn current() -> u64 {
    STACK.with_borrow(|stack| stack.last().copied()).unwrap_or_else(|| {
        CLIENT.with(|client| {
            if client.get() == 0 {
                client.set(NEXT_TASK.fetch_add(1, Ordering::Relaxed));
            }
            client.get()
        })
    })
}

#[derive(Default, Clone, Copy)]
struct Edges {
    /// The runner of the key it waits for.
    on: Option<u64>,
    /// The job it pumped, above it on its thread.
    above: Option<u64>,
}

struct Waits {
    edges: BTreeMap<u64, Edges>,
}

static WAITS: Mutex<Waits> = Mutex::new(Waits { edges: BTreeMap::new() });

impl Waits {
    fn set(&mut self, task: u64, change: impl FnOnce(&mut Edges)) {
        let edges = self.edges.entry(task).or_default();
        change(edges);
        if edges.on.is_none() && edges.above.is_none() {
            self.edges.remove(&task);
        }
    }

    fn pumped(&mut self, below: u64, above: Option<u64>) {
        self.set(below, |it| it.above = above);
    }

    fn wait_on(&mut self, waiter: u64, runner: u64) {
        self.set(waiter, |it| it.on = Some(runner));
    }

    fn stop_waiting(&mut self, waiter: u64, runner: u64) {
        self.set(waiter, |it| {
            if it.on == Some(runner) {
                it.on = None;
            }
        });
    }

    /// Whether `from` waits, through any chain, for `to`.
    fn reaches(&self, from: u64, to: u64) -> bool {
        let mut next = vec![from];
        let mut seen = BTreeSet::new();
        while let Some(task) = next.pop() {
            if task == to {
                return true;
            }
            if !seen.insert(task) {
                continue;
            }
            if let Some(edges) = self.edges.get(&task) {
                next.extend(edges.on);
                next.extend(edges.above);
            }
        }
        false
    }
}

enum State<V> {
    Running { waiters: Vec<(u64, Thread)> },
    Done(V),
    Cancelled,
}

struct Flight<V> {
    runner: u64,
    state: Mutex<State<V>>,
}

impl<V: Clone> Flight<V> {
    /// `None` while it runs; then its value, or `Some(None)` when it was cancelled.
    fn outcome(&self) -> Option<Option<V>> {
        match &*lock(&self.state) {
            State::Running { .. } => None,
            State::Done(value) => Some(Some(value.clone())),
            State::Cancelled => Some(None),
        }
    }

    fn ended(&self) -> bool {
        !matches!(&*lock(&self.state), State::Running { .. })
    }
}

enum Joined<V> {
    Run(Arc<Flight<V>>),
    Wait(Arc<Flight<V>>),
}

/// In-flight work by key: one run of a key at a time; whoever else asks for it waits
/// (pumping) and gets its value. Not a memo: once a run ends its entry goes; the caller
/// keeps values.
///
/// A wait on a pool's worker pumps that pool's jobs (up to [`PUMP_DEPTH`] deep). Outside a
/// pool's threads (a client's thread) a wait blocks plainly; a client is a task of its own for
/// the cycle check, so a client that runs a key a pool task waits for, and then asks for a key
/// that task runs, gives way as a task would.
///
/// Who gives way: the asker whose wait would close the cycle, at once, and nobody else. When a
/// run ends cancelled (its task gave way, or it panicked) no partial value is given: each of
/// its waiters asks again, the first to ask runs the key itself and the others wait for it,
/// each wait checked again. A waiter gives way only for a cycle its own new wait would close,
/// so of a cycle exactly one task gives way, and the others go on.
///
/// A task that asks for a key it runs itself (the run of a key asks, through its own calls,
/// for the key) is a cycle of the keys themselves, not of the schedule: reposting cannot end
/// it, so it panics, naming the key, and the task fails.
pub struct Flights<V> {
    shards: Box<[Mutex<FxHashMap<Key, Arc<Flight<V>>>>]>,
}

impl<V: Clone + Send + Sync + 'static> Default for Flights<V> {
    fn default() -> Flights<V> {
        Flights::new()
    }
}

impl<V: Clone + Send + Sync + 'static> Flights<V> {
    pub fn new() -> Flights<V> {
        Flights { shards: (0..SHARDS).map(|_| Mutex::new(FxHashMap::default())).collect() }
    }

    /// The value of `key`: run here by `run` when no one is running it, or waited for.
    /// `Err(Cancelled)` when waiting would close a cycle, or when `run` was cancelled; a run
    /// this caller waited for that was cancelled is asked again (see [`Flights`]).
    pub fn get_or_run(
        &self,
        key: Key,
        run: impl FnOnce() -> Result<V, Cancelled>,
    ) -> Result<V, Cancelled> {
        let me = current();
        let flight = loop {
            match self.join(key, me)? {
                Joined::Run(flight) => break flight,
                Joined::Wait(flight) => {
                    if let Some(value) = wait(&flight) {
                        return Ok(value);
                    }
                }
            }
        };
        let mut ending = Ending { flights: self, key, flight: &flight, ended: false };
        let result = run();
        match &result {
            Ok(value) => ending.end(State::Done(value.clone())),
            Err(Cancelled) => ending.end(State::Cancelled),
        }
        result
    }

    /// How many keys are running now.
    pub fn running(&self) -> usize {
        self.shards.iter().map(|it| lock(it).len()).sum()
    }

    fn shard(&self, key: &Key) -> &Mutex<FxHashMap<Key, Arc<Flight<V>>>> {
        &self.shards[usize::from(key.0[0]) % SHARDS]
    }

    fn join(&self, key: Key, me: u64) -> Result<Joined<V>, Cancelled> {
        let mut shard = lock(self.shard(&key));
        if let Some(flight) = shard.get(&key) {
            let mut state = lock(&flight.state);
            // An ended flight leaves the map as it ends, under this lock: one found runs.
            if let State::Running { waiters } = &mut *state {
                if flight.runner == me {
                    drop(state);
                    drop(shard);
                    panic!(
                        "key {} is asked by the task that runs it: its value depends on itself",
                        hex(&key)
                    );
                }
                let mut waits = lock(&WAITS);
                if waits.reaches(flight.runner, me) {
                    return Err(Cancelled);
                }
                waits.wait_on(me, flight.runner);
                waiters.push((me, thread::current()));
                drop(waits);
                drop(state);
                return Ok(Joined::Wait(flight.clone()));
            }
        }
        let flight = Arc::new(Flight {
            runner: me,
            state: Mutex::new(State::Running { waiters: Vec::new() }),
        });
        shard.insert(key, flight.clone());
        Ok(Joined::Run(flight))
    }
}

/// Ends a run: on its value, its cancel, or (dropped unended) its panic, which is a cancel to
/// its waiters.
struct Ending<'a, V: Clone + Send + Sync + 'static> {
    flights: &'a Flights<V>,
    key: Key,
    flight: &'a Arc<Flight<V>>,
    ended: bool,
}

impl<V: Clone + Send + Sync + 'static> Ending<'_, V> {
    fn end(&mut self, end: State<V>) {
        self.ended = true;
        let waiters = {
            let mut shard = lock(self.flights.shard(&self.key));
            if shard.get(&self.key).is_some_and(|it| Arc::ptr_eq(it, self.flight)) {
                shard.remove(&self.key);
            }
            let mut state = lock(&self.flight.state);
            let waiters = match std::mem::replace(&mut *state, end) {
                State::Running { waiters } => waiters,
                State::Done(_) | State::Cancelled => Vec::new(),
            };
            let mut waits = lock(&WAITS);
            for (waiter, _) in &waiters {
                waits.stop_waiting(*waiter, self.flight.runner);
            }
            waiters
        };
        for (_, thread) in waiters {
            thread.unpark();
        }
    }
}

impl<V: Clone + Send + Sync + 'static> Drop for Ending<'_, V> {
    fn drop(&mut self) {
        if !self.ended {
            self.end(State::Cancelled);
        }
    }
}

/// Waits for `flight` to end, pumping while this thread may: its value, or `None` when it was
/// cancelled.
fn wait<V: Clone>(flight: &Flight<V>) -> Option<V> {
    let mut idle = 0;
    loop {
        if let Some(outcome) = flight.outcome() {
            return outcome;
        }
        let pump = depth() < PUMP_DEPTH;
        if pump && crate::pool::run_one() {
            idle = 0;
            continue;
        }
        if idle < SPINS {
            idle += 1;
            if idle % 16 == 0 {
                thread::yield_now();
            } else {
                std::hint::spin_loop();
            }
            continue;
        }
        crate::pool::park(&|| flight.ended(), pump);
    }
}

fn hex(key: &Key) -> String {
    key.0.iter().take(8).map(|it| format!("{it:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc, Barrier,
            atomic::{AtomicU32, AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };

    use crossbeam_channel::unbounded;

    use super::{Flights, PUMP_DEPTH, depth};
    use crate::{
        Cancelled, Key,
        pool::Pool,
        topology::{Cpu, Topology},
    };

    const LONG: Duration = Duration::from_secs(60);

    fn key(n: u32) -> Key {
        let mut key = [0u8; 32];
        key[..4].copy_from_slice(&n.to_le_bytes());
        Key(key)
    }

    /// SplitMix64, as `blaze-core`'s `sim::Rng`.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        fn below(&mut self, n: u64) -> u64 {
            if n == 0 { 0 } else { self.next() % n }
        }
    }

    /// A asks, while it runs `ka`, for `kb`, which B runs and which asks for `ka`: whichever
    /// asks second would close the cycle and gives way; the other runs the key given up itself
    /// and is done; the one that gave way, posted again, is done too.
    #[test]
    fn two_tasks_that_ask_for_each_others_keys_end_with_one_cancelled() {
        let pool = Pool::new(&Topology::flat(2), false).unwrap();
        let flights = Arc::new(Flights::<u32>::new());
        let (tx, rx) = unbounded::<(char, Result<Result<u32, Cancelled>, &'static str>)>();
        let post = |name: char, barrier: Option<Arc<Barrier>>| {
            let (mine, theirs, add) = if name == 'a' { (1, 2, 2) } else { (2, 1, 1) };
            let (flights, tx) = (flights.clone(), tx.clone());
            pool.submit(
                None,
                move || {
                    flights.get_or_run(key(mine), || {
                        if let Some(barrier) = &barrier {
                            barrier.wait();
                        }
                        flights.get_or_run(key(theirs), || Ok(add)).map(|it| it + 10)
                    })
                },
                move |result| _ = tx.send((name, result.map_err(|_| "panicked"))),
            );
        };
        let barrier = Arc::new(Barrier::new(2));
        post('a', Some(barrier.clone()));
        post('b', Some(barrier));
        let heard = || {
            let (name, result) = rx.recv_timeout(LONG).unwrap();
            (name, result.unwrap())
        };
        let first = heard();
        let second = heard();
        let (cancelled, done) = match (first.1, second.1) {
            (Err(Cancelled), Ok(value)) => (first.0, (second.0, value)),
            (Ok(value), Err(Cancelled)) => (second.0, (first.0, value)),
            other => panic!("not one cancelled and one done: {other:?}"),
        };
        // Each one's value is its own key's run over the other's key run inline: 2 + 10 for
        // A, 1 + 10 for B.
        let value_of = |name: char| if name == 'a' { 12 } else { 11 };
        assert_eq!(done.1, value_of(done.0));
        post(cancelled, None);
        let (name, again) = heard();
        assert_eq!(name, cancelled);
        assert_eq!(again, Ok(value_of(cancelled)));
        assert_eq!(flights.running(), 0);
    }

    /// One worker, a hundred jobs that each wait for a key a client runs: each pumps the next
    /// until the bound, where the last waits without pumping; once the key is done, every one
    /// ends.
    #[test]
    fn a_chain_of_waits_through_pumping_stops_at_the_bound() {
        let pool = Pool::new(&Topology::flat(1), false).unwrap();
        let flights = Arc::new(Flights::<u32>::new());
        let started = Arc::new(AtomicUsize::new(0));
        let deepest = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = unbounded::<Result<Result<u32, Cancelled>, &'static str>>();
        let value = flights.get_or_run(key(0), || {
            for _ in 0..100 {
                let (flights, started, deepest, tx) =
                    (flights.clone(), started.clone(), deepest.clone(), tx.clone());
                pool.submit(
                    None,
                    move || {
                        started.fetch_add(1, Ordering::SeqCst);
                        deepest.fetch_max(depth(), Ordering::SeqCst);
                        flights.get_or_run(key(0), || Ok(0))
                    },
                    move |result| _ = tx.send(result.map_err(|_| "panicked")),
                );
            }
            let until = Instant::now() + LONG;
            while started.load(Ordering::SeqCst) < PUMP_DEPTH + 1 {
                assert!(Instant::now() < until, "the chain did not reach the bound");
                std::thread::sleep(Duration::from_millis(1));
            }
            std::thread::sleep(Duration::from_millis(50));
            assert_eq!(started.load(Ordering::SeqCst), PUMP_DEPTH + 1);
            Ok(7)
        });
        assert_eq!(value, Ok(7));
        let values: Vec<u32> =
            (0..100).map(|_| rx.recv_timeout(LONG).unwrap().unwrap().unwrap()).collect();
        assert_eq!(values.iter().filter(|&&it| it == 7).count(), PUMP_DEPTH + 1);
        assert_eq!(values.iter().filter(|&&it| it == 0).count(), 100 - PUMP_DEPTH - 1);
        assert_eq!(deepest.load(Ordering::SeqCst), PUMP_DEPTH);
    }

    #[test]
    #[should_panic(expected = "its value depends on itself")]
    fn a_key_that_asks_for_itself_panics() {
        let flights = Flights::<u32>::new();
        _ = flights.get_or_run(key(5), || flights.get_or_run(key(5), || Ok(1)));
    }

    /// Random tasks over random keys whose runs ask for keys after them, on two groups of two
    /// workers: no key runs twice at once, every value is the key's, every task ends (those
    /// that gave way posted again until done), and nothing is left running.
    #[test]
    fn random_waits_end_and_run_each_key_once_at_a_time() {
        const KEYS: usize = 24;
        let topology = Topology {
            cpus: (0..4)
                .map(|id| Cpu { id, group: id / 2, socket: 0, fast: true })
                .collect(),
        };
        for seed in 0..12u64 {
            let mut rng = Rng(seed);
            let deps: Vec<Vec<usize>> = (0..KEYS)
                .map(|at| {
                    let after = (KEYS - at - 1) as u64;
                    (0..rng.below(4).min(after))
                        .map(|_| at + 1 + rng.below(after) as usize)
                        .collect()
                })
                .collect();
            let mut expected = vec![0u64; KEYS];
            for at in (0..KEYS).rev() {
                expected[at] = at as u64 * 1000 + deps[at].iter().map(|&it| expected[it]).sum::<u64>();
            }
            let tasks: Vec<Vec<usize>> = (0..64)
                .map(|_| (0..1 + rng.below(4)).map(|_| rng.below(KEYS as u64) as usize).collect())
                .collect();
            let spins: Vec<u32> = (0..KEYS).map(|_| rng.below(200) as u32).collect();
            let world = Arc::new(World {
                flights: Flights::new(),
                deps,
                expected,
                spins,
                running: (0..KEYS).map(|_| AtomicU32::new(0)).collect(),
            });
            let pool = Pool::new(&topology, false).unwrap();
            let (tx, rx) = unbounded::<(usize, Result<Result<(), Cancelled>, &'static str>)>();
            let post = |task: usize| {
                let (world, asks, tx) = (world.clone(), tasks[task].clone(), tx.clone());
                pool.submit(
                    None,
                    move || -> Result<(), Cancelled> {
                        for key in asks {
                            let value = world.get(key)?;
                            assert_eq!(value, world.expected[key]);
                        }
                        Ok(())
                    },
                    move |result| _ = tx.send((task, result.map_err(|_| "panicked"))),
                );
            };
            for task in 0..tasks.len() {
                post(task);
            }
            let mut done = 0;
            while done < tasks.len() {
                let heard = rx
                    .recv_timeout(LONG)
                    .unwrap_or_else(|_| panic!("seed {seed}: a task did not end"));
                match heard {
                    (_, Ok(Ok(()))) => done += 1,
                    (task, Ok(Err(Cancelled))) => post(task),
                    (task, Err(why)) => panic!("seed {seed}: task {task} {why}"),
                }
            }
            assert_eq!(world.flights.running(), 0, "seed {seed}");
            assert!(world.running.iter().all(|it| it.load(Ordering::SeqCst) == 0));
        }
    }

    struct World {
        flights: Flights<u64>,
        deps: Vec<Vec<usize>>,
        expected: Vec<u64>,
        spins: Vec<u32>,
        running: Vec<AtomicU32>,
    }

    impl World {
        fn get(&self, at: usize) -> Result<u64, Cancelled> {
            self.flights.get_or_run(key(at as u32), || {
                struct Out<'a>(&'a AtomicU32);
                impl Drop for Out<'_> {
                    fn drop(&mut self) {
                        self.0.fetch_sub(1, Ordering::SeqCst);
                    }
                }
                assert_eq!(self.running[at].fetch_add(1, Ordering::SeqCst), 0, "key {at} twice");
                let _out = Out(&self.running[at]);
                for _ in 0..self.spins[at] {
                    std::hint::spin_loop();
                }
                let mut value = at as u64 * 1000;
                for &dep in &self.deps[at] {
                    value += self.get(dep)?;
                }
                Ok(value)
            })
        }
    }
}

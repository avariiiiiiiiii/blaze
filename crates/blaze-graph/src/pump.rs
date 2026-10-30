//! The rule for a wait inside a task (#1301, docs/v2/plan/lanes/core.md "Issues this lane takes in"): a
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
//! is refused.
//!
//! The graph costs nothing until a task waits. Each thread publishes its stack in a [`Stack`]
//! of its own (384 bytes, 128-byte aligned, written by its thread alone): starting a job is
//! three stores to it and ending one a fourth, and the edge to the job above is read from it.
//! A wait's own edge, to the runner it waits for, sits beside the task in the same slot. Only
//! a wait takes the one process-wide lock ([`WAITS`]), to check and add its edge as one step; a
//! run nobody waits for takes its key's shard twice and nothing else.

use std::{
    cell::Cell,
    collections::hash_map::Entry,
    hash::{BuildHasherDefault, Hasher},
    num::NonZero,
    ptr,
    sync::{
        Arc, MutexGuard, OnceLock, PoisonError,
        atomic::{AtomicPtr, AtomicU8, AtomicU32, AtomicU64, Ordering},
    },
    thread::{self, Thread},
};

use rustc_hash::{FxBuildHasher, FxHashSet};

use crate::{Cancelled, Key};

/// How many jobs a thread runs above the job at the bottom of its stack, at most. A wait at
/// this depth parks without pumping; that cannot deadlock, since the cycle check still runs.
pub const PUMP_DEPTH: usize = 16;

/// How many times a waiter looks again before it parks.
pub(crate) const SPINS: u32 = 64;

/// The executor's one kind of lock, held only to claim or hand over a slot (a key's flight, a
/// wait's edge, a spare pool, a panic to raise later, a cap's bytes), never across a call into
/// work or the database: no guard lives across a query, so none can deadlock or invert a
/// priority (docs/v2/plan/design/concurrency.md, "No guard lives across a query call").
#[allow(clippy::disallowed_types)]
pub(crate) struct Lock<T>(std::sync::Mutex<T>);

#[allow(clippy::disallowed_types)]
impl<T> Lock<T> {
    pub(crate) const fn new(value: T) -> Lock<T> {
        Lock(std::sync::Mutex::new(value))
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, T> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn into_inner(self) -> T {
        self.0.into_inner().unwrap_or_else(PoisonError::into_inner)
    }
}

// A task's id says where it is published: its thread's slot (24 bits, from 1), its depth in
// that thread's stack (5 bits; `CLIENT` for a thread of no pool) and a count of the slot's
// tasks (35 bits), so an id is never 0 and no two live tasks share one.
const THREAD_SHIFT: u32 = 40;
const DEPTH_SHIFT: u32 = 35;
const SEQ_MASK: u64 = (1 << DEPTH_SHIFT) - 1;
const DEPTH_MASK: u64 = (1 << (THREAD_SHIFT - DEPTH_SHIFT)) - 1;
const CLIENT: u64 = DEPTH_MASK;
const _: () = assert!(PUMP_DEPTH < CLIENT as usize);

/// A task as the cycle check sees it: its id (0 for none) and the runner of the key it waits
/// for (0 for none; written under [`WAITS`]). 16 bytes.
struct Published {
    task: AtomicU64,
    on: AtomicU64,
}

impl Published {
    const fn new() -> Published {
        Published { task: AtomicU64::new(0), on: AtomicU64::new(0) }
    }
}

/// A thread's tasks, bottom first. Written by its thread but for `on`; read by any cycle check.
#[repr(align(128))]
struct Stack {
    len: AtomicU32,
    /// The slot's task count, kept for the next thread given it.
    seq: AtomicU64,
    /// The thread itself, for a wait outside any job.
    client: Published,
    tasks: [Published; PUMP_DEPTH + 1],
}

const _: () = assert!(size_of::<Stack>() == 384);

impl Stack {
    fn new() -> Stack {
        Stack {
            len: AtomicU32::new(0),
            seq: AtomicU64::new(0),
            client: Published::new(),
            tasks: [const { Published::new() }; PUMP_DEPTH + 1],
        }
    }
}

/// Stacks come in chunks, published once and never freed or moved, so a check reads any
/// thread's without a lock: slot `n` is chunk `n / CHUNK`, at `n % CHUNK`.
const CHUNK: usize = 64;
const CHUNKS: usize = 1 << 12;
static DIR: [AtomicPtr<Stack>; CHUNKS] = [const { AtomicPtr::new(ptr::null_mut()) }; CHUNKS];

struct Free {
    next: u32,
    free: Vec<u32>,
}

/// The slots of threads that ended, for the next; taken once by each thread.
static FREE: Lock<Free> = Lock::new(Free { next: 1, free: Vec::new() });

fn stack(index: u64) -> Option<&'static Stack> {
    let index = usize::try_from(index).ok()?;
    let chunk = DIR.get(index / CHUNK)?.load(Ordering::Acquire);
    (!chunk.is_null()).then(|| {
        // SAFETY: `index % CHUNK < CHUNK`, and a published chunk is `CHUNK` stacks.
        let at = unsafe { chunk.add(index % CHUNK) };
        // SAFETY: a published chunk is leaked, never freed or moved, and its stacks were made
        // before the release store that published it, which the acquire load above pairs with.
        unsafe { &*at }
    })
}

fn claim() -> (u32, &'static Stack) {
    let index = {
        let mut free = FREE.lock();
        let index = free.free.pop().unwrap_or_else(|| {
            free.next += 1;
            free.next - 1
        });
        let at = index as usize / CHUNK;
        assert!(at < CHUNKS, "more than {} threads have a task stack at once", CHUNKS * CHUNK);
        if DIR[at].load(Ordering::Relaxed).is_null() {
            let chunk: Box<[Stack]> = (0..CHUNK).map(|_| Stack::new()).collect();
            DIR[at].store(Box::leak(chunk).as_mut_ptr(), Ordering::Release);
        }
        index
    };
    (index, stack(u64::from(index)).expect("the chunk was just published"))
}

fn next_id(index: u32, depth: u64, seq: &Cell<u64>) -> u64 {
    let next = seq.get().wrapping_add(1) & SEQ_MASK;
    seq.set(next);
    (u64::from(index) << THREAD_SHIFT) | (depth << DEPTH_SHIFT) | next
}

/// This thread's slot.
struct Me {
    index: u32,
    stack: &'static Stack,
    client: u64,
    seq: Cell<u64>,
}

impl Me {
    fn new() -> Me {
        let (index, stack) = claim();
        let seq = Cell::new(stack.seq.load(Ordering::Relaxed));
        let client = next_id(index, CLIENT, &seq);
        stack.client.on.store(0, Ordering::Relaxed);
        stack.client.task.store(client, Ordering::Release);
        Me { index, stack, client, seq }
    }
}

impl Drop for Me {
    fn drop(&mut self) {
        self.stack.len.store(0, Ordering::Release);
        self.stack.client.task.store(0, Ordering::Release);
        self.stack.seq.store(self.seq.get(), Ordering::Relaxed);
        FREE.lock().free.push(self.index);
    }
}

thread_local! {
    static ME: Me = Me::new();
}

/// Runs `f` as a new task of this thread, above the one it runs (if any), which waits for it.
pub(crate) fn run_task<R>(f: impl FnOnce() -> R) -> R {
    struct Leave {
        stack: &'static Stack,
        len: u32,
    }
    impl Drop for Leave {
        fn drop(&mut self) {
            self.stack.len.store(self.len, Ordering::Release);
        }
    }
    let _leave = ME.with(|me| {
        let len = me.stack.len.load(Ordering::Relaxed);
        let at = len as usize;
        assert!(at <= PUMP_DEPTH, "a thread runs a task {at} deep, past the pump bound of {PUMP_DEPTH}");
        let id = next_id(me.index, u64::from(len), &me.seq);
        let entry = &me.stack.tasks[at];
        entry.on.store(0, Ordering::Relaxed);
        entry.task.store(id, Ordering::Release);
        me.stack.len.store(len + 1, Ordering::Release);
        Leave { stack: me.stack, len }
    });
    f()
}

/// How many jobs this thread runs above the one at the bottom of its stack.
pub fn depth() -> usize {
    ME.with(|me| me.stack.len.load(Ordering::Relaxed) as usize).saturating_sub(1)
}

fn current() -> u64 {
    ME.with(|me| match me.stack.len.load(Ordering::Relaxed) as usize {
        0 => me.client,
        len => me.stack.tasks[len - 1].task.load(Ordering::Relaxed),
    })
}

/// Where `task` is published, while it lives.
fn published(task: u64) -> Option<&'static Published> {
    let stack = stack(task >> THREAD_SHIFT)?;
    let depth = (task >> DEPTH_SHIFT) & DEPTH_MASK;
    let entry = if depth == CLIENT { &stack.client } else { stack.tasks.get(depth as usize)? };
    (entry.task.load(Ordering::Acquire) == task).then_some(entry)
}

/// The edges out of `task`: the runner it waits for and the job above it on its thread, 0
/// for none. A task that has ended has none, so a stale edge to it leads nowhere.
fn edges(task: u64) -> (u64, u64) {
    let Some(stack) = stack(task >> THREAD_SHIFT) else { return (0, 0) };
    let depth = (task >> DEPTH_SHIFT) & DEPTH_MASK;
    if depth == CLIENT {
        let live = stack.client.task.load(Ordering::Acquire) == task;
        return (if live { stack.client.on.load(Ordering::Relaxed) } else { 0 }, 0);
    }
    let (len, depth) = (stack.len.load(Ordering::Acquire) as usize, depth as usize);
    if depth >= len {
        return (0, 0);
    }
    let entry = &stack.tasks[depth];
    if entry.task.load(Ordering::Acquire) != task {
        return (0, 0);
    }
    let above = if depth + 1 < len { stack.tasks[depth + 1].task.load(Ordering::Acquire) } else { 0 };
    (entry.on.load(Ordering::Relaxed), above)
}

/// The cycle check's scratch, kept between checks.
struct Walk {
    next: Vec<u64>,
    seen: FxHashSet<u64>,
}

impl Walk {
    /// Whether `from` waits, through any chain, for `to`. The graph without the edge being
    /// checked has no cycle (each edge was checked as it was added, and an edge to a job just
    /// pumped cannot close one), so the walk ends.
    fn reaches(&mut self, from: u64, to: u64) -> bool {
        self.next.clear();
        self.seen.clear();
        self.next.push(from);
        while let Some(task) = self.next.pop() {
            if task == to {
                return true;
            }
            if task == 0 || !self.seen.insert(task) {
                continue;
            }
            let (on, above) = edges(task);
            self.next.push(on);
            self.next.push(above);
        }
        false
    }
}

/// Orders the waits: a check and the edge it adds are one step against every other wait's.
static WAITS: Lock<Walk> =
    Lock::new(Walk { next: Vec::new(), seen: FxHashSet::with_hasher(FxBuildHasher) });

#[cfg(test)]
thread_local! {
    static WAITS_TAKEN: Cell<usize> = const { Cell::new(0) };
}

fn waits() -> MutexGuard<'static, Walk> {
    #[cfg(test)]
    WAITS_TAKEN.with(|it| it.set(it.get() + 1));
    WAITS.lock()
}

/// Hashes a key by its first eight bytes: a key is a content hash already.
#[derive(Default)]
struct KeyBits(u64);

impl Hasher for KeyBits {
    fn write(&mut self, bytes: &[u8]) {
        if let Some(first) = bytes.first_chunk::<8>() {
            self.0 = u64::from_le_bytes(*first);
        }
    }

    // The length an array is hashed with, which says nothing of a key.
    fn write_usize(&mut self, _: usize) {}

    fn finish(&self) -> u64 {
        self.0
    }
}

#[allow(clippy::disallowed_types)]
type ByKey<V> = std::collections::HashMap<Key, V, BuildHasherDefault<KeyBits>>;

const RUNNING: u8 = 0;
const DONE: u8 = 1;
const GAVE_WAY: u8 = 2;

/// What the waiters of a run read: made by the first of them, so a run nobody waits for
/// allocates nothing. Polled with plain loads.
struct Flight<V> {
    state: AtomicU8,
    value: OnceLock<V>,
}

/// A run under way, in its key's shard: 56 bytes beside its key.
struct InFlight<V> {
    runner: u64,
    waiters: Vec<(u64, Thread)>,
    flight: Option<Arc<Flight<V>>>,
}

/// One shard of the runs under way, by key. 128-byte aligned, so no two shards' locks share
/// a line pair.
#[repr(align(128))]
struct Shard<V>(Lock<ByKey<InFlight<V>>>);

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
///
/// The runs are sharded by key, at least four shards a core (a power of two), each claimed
/// only to add or remove a run, never while it runs: a run nobody waits for takes its shard
/// twice, allocates nothing once the shard's table has grown, and takes no lock of the
/// process.
pub struct Flights<V> {
    shards: Box<[Shard<V>]>,
    mask: usize,
}

impl<V: Clone + Send + Sync + 'static> Default for Flights<V> {
    fn default() -> Flights<V> {
        Flights::new()
    }
}

impl<V: Clone + Send + Sync + 'static> Flights<V> {
    pub fn new() -> Flights<V> {
        let count = shard_count();
        Flights {
            shards: (0..count).map(|_| Shard(Lock::new(ByKey::default()))).collect(),
            mask: count - 1,
        }
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
        while let Some(flight) = self.join(key, me)? {
            if let Some(value) = wait(&flight) {
                return Ok(value);
            }
        }
        let mut ending = Ending { flights: self, key, runner: me, ended: false };
        let result = run();
        ending.end(result.as_ref().ok());
        result
    }

    /// How many keys are running now.
    pub fn running(&self) -> usize {
        self.shards.iter().map(|it| it.0.lock().len()).sum()
    }

    fn shard(&self, key: &Key) -> &Lock<ByKey<InFlight<V>>> {
        // Bytes the shard's table does not hash by, so a shard's keys spread over its table.
        &self.shards[word(key, 8) as usize & self.mask].0
    }

    /// `None` when this caller is to run `key`; else the run to wait for.
    fn join(&self, key: Key, me: u64) -> Result<Option<Arc<Flight<V>>>, Cancelled> {
        let mut shard = self.shard(&key).lock();
        let entry = match shard.entry(key) {
            Entry::Vacant(slot) => {
                slot.insert(InFlight { runner: me, waiters: Vec::new(), flight: None });
                return Ok(None);
            }
            Entry::Occupied(slot) => slot.into_mut(),
        };
        let runner = entry.runner;
        if runner == me {
            panic!("key {} is asked by the task that runs it: its value depends on itself", hex(&key));
        }
        {
            let mut waits = waits();
            if waits.reaches(runner, me) {
                return Err(Cancelled);
            }
            if let Some(mine) = published(me) {
                mine.on.store(runner, Ordering::Relaxed);
            }
        }
        entry.waiters.push((me, thread::current()));
        let flight = entry.flight.get_or_insert_with(|| {
            Arc::new(Flight { state: AtomicU8::new(RUNNING), value: OnceLock::new() })
        });
        Ok(Some(flight.clone()))
    }
}

/// Ends a run: on its value, its cancel, or (dropped unended) its panic, which is a cancel to
/// its waiters.
struct Ending<'a, V: Clone + Send + Sync + 'static> {
    flights: &'a Flights<V>,
    key: Key,
    runner: u64,
    ended: bool,
}

impl<V: Clone + Send + Sync + 'static> Ending<'_, V> {
    fn end(&mut self, value: Option<&V>) {
        self.ended = true;
        let Some(InFlight { runner, waiters, flight }) =
            self.flights.shard(&self.key).lock().remove(&self.key)
        else {
            return;
        };
        debug_assert_eq!(runner, self.runner, "a run's entry is its own until it ends");
        if waiters.is_empty() {
            return;
        }
        // Each waiter's edge goes before it can see the end and wait again, and while this
        // runner is in `end` it waits for nobody, so an edge read in between leads nowhere.
        for &(waiter, _) in &waiters {
            if let Some(entry) = published(waiter) {
                _ = entry.on.compare_exchange(runner, 0, Ordering::Relaxed, Ordering::Relaxed);
            }
        }
        if let Some(flight) = flight {
            let state = match value {
                Some(value) => {
                    _ = flight.value.set(value.clone());
                    DONE
                }
                None => GAVE_WAY,
            };
            flight.state.store(state, Ordering::Release);
        }
        for (_, thread) in waiters {
            thread.unpark();
        }
    }
}

impl<V: Clone + Send + Sync + 'static> Drop for Ending<'_, V> {
    fn drop(&mut self) {
        if !self.ended {
            self.end(None);
        }
    }
}

/// Waits for `flight` to end, pumping while this thread may: its value, or `None` when it was
/// cancelled.
fn wait<V: Clone>(flight: &Flight<V>) -> Option<V> {
    let mut idle = 0u32;
    loop {
        match flight.state.load(Ordering::Acquire) {
            RUNNING => {}
            DONE => return flight.value.get().cloned(),
            _ => return None,
        }
        let pump = depth() < PUMP_DEPTH;
        if pump && idle % 16 == 0 && crate::pool::run_one() {
            idle = 0;
            continue;
        }
        if idle < SPINS {
            idle += 1;
            std::hint::spin_loop();
            continue;
        }
        crate::pool::park(&|| flight.state.load(Ordering::Acquire) != RUNNING, pump);
    }
}

/// At least four shards a core, a power of two (docs/v2/plan/design/concurrency.md, "Writers
/// shard by hash"), counted once for the process.
fn shard_count() -> usize {
    static COUNT: OnceLock<usize> = OnceLock::new();
    *COUNT.get_or_init(|| {
        let cores = thread::available_parallelism().map_or(1, NonZero::get);
        (4 * cores).next_power_of_two().max(64)
    })
}

fn word(key: &Key, at: usize) -> u64 {
    let mut word = [0; 8];
    word.copy_from_slice(&key.0[at..at + 8]);
    u64::from_le_bytes(word)
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

    use crossbeam_channel::{bounded, unbounded};

    use super::{
        CLIENT, DEPTH_MASK, DEPTH_SHIFT, Flights, PUMP_DEPTH, THREAD_SHIFT, WAITS_TAKEN, current,
        depth, edges, run_task, shard_count,
    };
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

    fn until(what: &str, done: impl Fn() -> bool) {
        let until = Instant::now() + LONG;
        while !done() {
            assert!(Instant::now() < until, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(1));
        }
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

    /// A task's id names its thread's slot and its depth; the job above it is its edge while
    /// both run, and an ended task has no edges.
    #[test]
    fn a_task_is_published_where_its_id_says() {
        let client = current();
        assert_eq!((client >> DEPTH_SHIFT) & DEPTH_MASK, CLIENT);
        assert_eq!(edges(client), (0, 0));
        let (outer, inner) = run_task(|| {
            let outer = current();
            assert_eq!(depth(), 0);
            let inner = run_task(|| {
                let inner = current();
                assert_eq!(depth(), 1);
                assert_eq!(edges(outer), (0, inner));
                assert_eq!(edges(inner), (0, 0));
                assert_eq!(inner >> THREAD_SHIFT, outer >> THREAD_SHIFT);
                assert_eq!((inner >> DEPTH_SHIFT) & DEPTH_MASK, 1);
                inner
            });
            assert_eq!(edges(outer), (0, 0), "the job above has ended");
            (outer, inner)
        });
        assert_ne!(outer, inner);
        assert_eq!((edges(outer), edges(inner)), ((0, 0), (0, 0)));
        assert_eq!(current(), client);
        let other = std::thread::spawn(current).join().unwrap();
        assert_ne!(other >> THREAD_SHIFT, client >> THREAD_SHIFT, "a thread's slot is its own");
    }

    #[test]
    fn flights_have_four_shards_a_core_or_more() {
        let flights = Flights::<u32>::new();
        let cores = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
        assert!(flights.shards.len().is_power_of_two());
        assert!(flights.shards.len() >= 4 * cores && flights.shards.len() >= 64);
        assert_eq!(flights.shards.len(), shard_count());
        assert_eq!(align_of_val(&flights.shards[0]), 128);
    }

    /// Runs nobody waits for, nested and one after another, never take the wait graph's lock.
    #[test]
    fn a_run_nobody_waits_for_takes_no_lock_of_the_process() {
        let flights = Flights::<u32>::new();
        let before = WAITS_TAKEN.with(std::cell::Cell::get);
        for n in 0..1000u32 {
            let value = flights.get_or_run(key(n), || {
                flights.get_or_run(key(n + 1_000_000), || Ok(n)).map(|it| it + 1)
            });
            assert_eq!(value, Ok(n + 1));
        }
        assert_eq!(WAITS_TAKEN.with(std::cell::Cell::get), before);
        assert_eq!(flights.running(), 0);
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

    /// One worker: its job runs key 2 and waits for key 1, which a client holds. A job it pumps
    /// meanwhile asks for key 2: its runner waits for the pumped job (it is above it on the
    /// thread), so that wait would close a cycle and gives way at once. The edge to the job
    /// above is read from the thread's published stack.
    #[test]
    fn a_job_pumped_above_the_runner_of_its_key_gives_way() {
        let pool = Pool::new(&Topology::flat(1), false).unwrap();
        let flights = Arc::new(Flights::<u32>::new());
        let (release_tx, release_rx) = bounded::<()>(0);
        let (tx, rx) = unbounded::<(&'static str, Result<u32, Cancelled>)>();
        let waiting = Arc::new(AtomicUsize::new(0));
        std::thread::scope(|scope| {
            let holder = {
                let flights = flights.clone();
                scope.spawn(move || {
                    flights.get_or_run(key(1), || {
                        release_rx.recv_timeout(LONG).unwrap();
                        Ok(5)
                    })
                })
            };
            until("the client to run key 1", || flights.running() == 1);
            {
                let (flights, tx, waiting) = (flights.clone(), tx.clone(), waiting.clone());
                pool.submit(
                    None,
                    move || {
                        flights.get_or_run(key(2), || {
                            waiting.store(1, Ordering::SeqCst);
                            flights.get_or_run(key(1), || Ok(0)).map(|it| it + 1)
                        })
                    },
                    move |it| _ = tx.send(("outer", it.unwrap())),
                );
            }
            until("the job to wait", || waiting.load(Ordering::SeqCst) == 1 && flights.running() == 2);
            std::thread::sleep(Duration::from_millis(50));
            {
                let (flights, tx) = (flights.clone(), tx.clone());
                pool.submit(
                    None,
                    move || flights.get_or_run(key(2), || Ok(99)),
                    move |it| _ = tx.send(("pumped", it.unwrap())),
                );
            }
            assert_eq!(rx.recv_timeout(LONG).unwrap(), ("pumped", Err(Cancelled)));
            release_tx.send_timeout((), LONG).unwrap();
            assert_eq!(holder.join().unwrap(), Ok(5));
            assert_eq!(rx.recv_timeout(LONG).unwrap(), ("outer", Ok(6)));
        });
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
            until("the chain to reach the bound", || {
                started.load(Ordering::SeqCst) >= PUMP_DEPTH + 1
            });
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
    /// workers: no key runs twice at once, every value is the model's, every task ends (those
    /// that gave way posted again until done), and nothing is left running.
    #[test]
    fn random_waits_end_and_run_each_key_once_at_a_time() {
        const KEYS: usize = 24;
        let topology = Topology {
            cpus: (0..4).map(|id| Cpu { id, group: id / 2, socket: 0, node: 0, fast: true }).collect(),
        };
        for seed in 0..12u64 {
            let mut rng = Rng(seed);
            let deps: Vec<Vec<usize>> = (0..KEYS)
                .map(|at| {
                    let after = (KEYS - at - 1) as u64;
                    (0..rng.below(4).min(after)).map(|_| at + 1 + rng.below(after) as usize).collect()
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

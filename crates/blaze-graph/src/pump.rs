//! The rule for a wait inside a task (#1301, docs/v2/plan/lanes/core.md "Issues this lane takes in"): a
//! task that waits for what another task makes holds no thread while it waits, and a wait that
//! would close a cycle gives way at once with [`Cancelled`], so the task that asked unwinds,
//! releases what it holds and is posted again. No wait can deadlock: every wait is checked
//! against every other before it begins.
//!
//! Every task is a job of the scheduler ([`crate::sched`]): a pool's job on its fiber, or a
//! thread on no fiber, which is one job for its life. Who waits for whom is the scheduler's one
//! graph for the process: a task waits for the task that runs the key it asked
//! ([`Flights::get_or_run`]), and a scope's owner ([`crate::pool::Scope`], so every loop of
//! [`crate::par`]) for each of the scope's jobs that waits. A wait suspends the job (a fiber
//! switches out, a thread parks) on the scheduler's flight of the run it waits for, and resumes
//! once the runner has written its end.
//!
//! The graph costs nothing until a task waits. A run nobody waits for takes its key's shard
//! twice and nothing else; its flight in the scheduler is claimed by its first waiter, for it,
//! under the shard's lock, and ended by the runner only if one was.
//!
//! Nothing here allocates once warm (docs/v2/plan/lanes/mem/core/plane.md) but a value's
//! clones: a waiter is a node on its own stack, linked into its run's entry, and the runner
//! writes each waiter its clone.

use std::{
    cell::{Cell, UnsafeCell},
    collections::hash_map::Entry,
    hash::{BuildHasherDefault, Hasher},
    num::NonZero,
    ptr,
    sync::PoisonError,
};

use crate::{
    Cancelled, Key,
    sched::{self, CounterId, JobId, Wait},
    sync::{
        MutexGuard, OnceLock,
        atomic::{AtomicU8, AtomicU64, Ordering},
        hint, thread, thread_local,
    },
};

/// How many times a waiter looks again before it parks.
pub(crate) const SPINS: u32 = 64;

/// The executor's one kind of lock, held only to claim or hand over a slot (a key's flight, a
/// wait's edge, a spare pool, a panic to raise later, a cap's bytes), never across a call into
/// work or the database: no guard lives across a query, so none can deadlock or invert a
/// priority (docs/v2/plan/design/concurrency.md, "No guard lives across a query call").
#[allow(clippy::disallowed_types)]
pub(crate) struct Lock<T>(crate::sync::Mutex<T>);

#[allow(clippy::disallowed_types)]
impl<T> Lock<T> {
    pub(crate) const fn new(value: T) -> Lock<T> {
        Lock(crate::sync::Mutex::new(value))
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, T> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn into_inner(self) -> T {
        self.0.into_inner().unwrap_or_else(PoisonError::into_inner)
    }
}

thread_local! {
    static GAVE_WAYS: Cell<u64> = const { Cell::new(0) };
}

/// How many waits on this thread have given way ([`Cancelled`] for a cycle): a loop tells an
/// item that unwound because it gave way, which it may run again where the cycle is its own,
/// from one that failed.
pub(crate) fn gave_way() -> u64 {
    GAVE_WAYS.with(Cell::get)
}

/// The switching job's give-ways, which move with it (a loop compares them before and after an
/// item, which may resume on another thread).
pub(crate) fn take_gave_ways() -> usize {
    GAVE_WAYS.with(|it| it.replace(0)) as usize
}

pub(crate) fn put_gave_ways(taken: usize) {
    GAVE_WAYS.with(|it| it.set(taken as u64));
}

fn give_way() -> Cancelled {
    GAVE_WAYS.with(|it| it.set(it.get() + 1));
    Cancelled
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

#[allow(clippy::disallowed_types)] // alloc: not SipHash: hashed by `KeyBits`, a key's first word
type ByKey<V> = std::collections::HashMap<Key, V, BuildHasherDefault<KeyBits>>;

const RUNNING: u8 = 0;
const DONE: u8 = 1;
const GAVE_WAY: u8 = 2;

/// A wait for a run, on the waiter's own stack while it waits, linked into the run's
/// [`InFlight`] list. The runner reads it once it has taken the list out of its shard, writes
/// it its value and its state, and touches it no more: `state` is the last write, and the
/// waiter leaves only once it reads that ([`Linked`] holds it there, unwinding too).
struct Waiter<V> {
    /// The waiter linked before this one: written under the run's shard lock, read under it
    /// or by the runner once the list is its own.
    next: UnsafeCell<*const Waiter<V>>,
    /// The run's value, written by the runner before `state` says `DONE`.
    value: UnsafeCell<Option<V>>,
    state: AtomicU8,
}

impl<V> Waiter<V> {
    fn new() -> Waiter<V> {
        Waiter { next: UnsafeCell::new(ptr::null()), value: UnsafeCell::new(None), state: AtomicU8::new(RUNNING) }
    }
}

/// A run's waiters, the last linked first; null for none.
struct Waiters<V>(*const Waiter<V>);

// SAFETY: each waiter is reached through the list only under its shard's lock, or by the
// runner once it took the list out, and the runner sends it a `V` (hence `V: Send`).
unsafe impl<V: Send> Send for Waiters<V> {}

/// A run under way, in its key's shard.
struct InFlight<V> {
    /// The task that runs it ([`task`]).
    runner: u64,
    /// The run's flight in the scheduler ([`flight`]), claimed for the runner by its first
    /// waiter; 0 until then.
    flight: u64,
    /// The runner's innermost scope as the run began, which the run ends inside: a wait for the
    /// run waits on the scopes the runner opened since, not on that one or those around it.
    scope: Option<CounterId>,
    waiters: Waiters<V>,
}

impl<V> InFlight<V> {
    fn new(runner: u64) -> InFlight<V> {
        InFlight { runner, flight: 0, scope: sched::innermost_scope(), waiters: Waiters(ptr::null()) }
    }
}

/// One shard of the runs under way, by key. 128-byte aligned, so no two shards' locks share
/// a line pair.
#[repr(align(128))]
struct Shard<V>(Lock<ByKey<InFlight<V>>>);

/// In-flight work by key: one run of a key at a time; whoever else asks for it waits, holding
/// no thread, and gets its value. Not a memo: once a run ends its entry goes; the caller keeps
/// values.
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
            // alloc: the shards, made once with the flights
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
        self.get_or_run_named(key, |it| it.push_str(&hex(&key)), run)
    }

    /// [`Flights::get_or_run`], a wait's span (see [`WaitSpans`]) naming what it waits for by
    /// what `named` writes, called only while a profiler spans waits.
    pub fn get_or_run_named(
        &self,
        key: Key,
        mut named: impl FnMut(&mut String),
        run: impl FnOnce() -> Result<V, Cancelled>,
    ) -> Result<V, Cancelled> {
        sched::miss();
        let me = task(sched::job_here());
        loop {
            let waiter = Waiter::new();
            let Some(flight) = self.join(key, me, &waiter) else { break };
            let linked = Linked { flights: self, key, waiter: &waiter };
            let waiting = Waiting::begin(&mut named);
            let got = wait(&waiter, flight);
            drop(waiting);
            drop(linked);
            if let Some(value) = got? {
                return Ok(value);
            }
        }
        let mut ending = Ending { flights: self, key, runner: me, ended: false };
        let result = run();
        ending.end(result.as_ref().ok());
        result
    }

    /// As [`Flights::get_or_run`], but never waits: `None`, running nothing, when another task
    /// runs `key` now.
    pub fn run_if_idle(
        &self,
        key: Key,
        run: impl FnOnce() -> Result<V, Cancelled>,
    ) -> Option<Result<V, Cancelled>> {
        let me = task(sched::job_here());
        match self.shard(&key).lock().entry(key) {
            Entry::Vacant(slot) => _ = slot.insert(InFlight::new(me)),
            Entry::Occupied(slot) if slot.get().runner == me => {
                panic!("key {} is asked by the task that runs it: its value depends on itself", hex(&key))
            }
            Entry::Occupied(_) => return None,
        }
        let mut ending = Ending { flights: self, key, runner: me, ended: false };
        let result = run();
        ending.end(result.as_ref().ok());
        Some(result)
    }

    /// How many keys are running now.
    pub fn running(&self) -> usize {
        self.shards.iter().map(|it| it.0.lock().len()).sum()
    }

    fn shard(&self, key: &Key) -> &Lock<ByKey<InFlight<V>>> {
        // Bytes the shard's table does not hash by, so a shard's keys spread over its table.
        &self.shards[word(key, 8) as usize & self.mask].0
    }

    /// `None` when this caller is to run `key`; else the flight of the run `waiter` is linked
    /// to wait for, claimed for its runner if no waiter did yet. The cycle check is the
    /// scheduler's, at the wait.
    fn join(&self, key: Key, me: u64, waiter: &Waiter<V>) -> Option<Key> {
        let mut shard = self.shard(&key).lock();
        let entry = match shard.entry(key) {
            Entry::Vacant(slot) => {
                slot.insert(InFlight::new(me));
                return None;
            }
            Entry::Occupied(slot) => slot.into_mut(),
        };
        let runner = entry.runner;
        if runner == me {
            panic!("key {} is asked by the task that runs it: its value depends on itself", hex(&key));
        }
        if entry.flight == 0 {
            entry.flight = FLIGHTS.fetch_add(1, Ordering::Relaxed);
            let claimed = sched::claim_for(flight(entry.flight), job(runner), entry.scope);
            assert!(claimed, "flight {} of key {}'s run by task {runner:#x} is new", entry.flight, hex(&key));
        }
        // SAFETY: `next` is the waiter's own until it is linked, here, under the shard's lock;
        // the caller keeps the waiter where it is until its run ends ([`Linked`]).
        unsafe { *waiter.next.get() = entry.waiters.0 };
        entry.waiters.0 = ptr::from_ref(waiter);
        Some(flight(entry.flight))
    }
}

/// The next run's flight number, from 1: each run waited for has its own, so a run that
/// starts while the last one's waiters are being woken never finds its flight taken.
static FLIGHTS: AtomicU64 = AtomicU64::new(1);

/// The scheduler's key of flight `number`: no content hash's.
fn flight(number: u64) -> Key {
    let mut key = [0u8; 32];
    key[..8].copy_from_slice(&number.to_le_bytes());
    key[8..].copy_from_slice(b"a run's flight, of pump.");
    Key(key)
}

/// Keeps a linked [`Waiter`] in place until its run is done with it: dropped before the run
/// ended (an unwind out of the wait), it unlinks the waiter if the run's entry still holds
/// it, or else waits for the runner, which has taken the list, to write it its end.
struct Linked<'a, V: Clone + Send + Sync + 'static> {
    flights: &'a Flights<V>,
    key: Key,
    waiter: &'a Waiter<V>,
}

impl<V: Clone + Send + Sync + 'static> Drop for Linked<'_, V> {
    fn drop(&mut self) {
        if self.waiter.state.load(Ordering::Acquire) != RUNNING {
            return;
        }
        let me = ptr::from_ref(self.waiter);
        {
            let mut shard = self.flights.shard(&self.key).lock();
            if let Some(entry) = shard.get_mut(&self.key) {
                let mut link: *mut *const Waiter<V> = &raw mut entry.waiters.0;
                loop {
                    // SAFETY: `link` is the entry's head or the `next` of a waiter of its list;
                    // under the shard's lock, held here, every waiter of the list is linked and in
                    // place, and its `next` is read and written only under the lock. A waiter
                    // that left its place while linked would make this a use after free.
                    let at = unsafe { *link };
                    if at.is_null() {
                        break;
                    }
                    if at == me {
                        // SAFETY: `me` is this waiter, in place for as long as `self` lives, and
                        // its `next` is read only under the lock.
                        let next = unsafe { *self.waiter.next.get() };
                        // SAFETY: as for the read of `link` above: it is written only under the
                        // lock.
                        unsafe { *link = next };
                        return;
                    }
                    // SAFETY: `at` is a waiter of the list, in place under the lock (as above).
                    link = unsafe { &*at }.next.get();
                }
            }
        }
        // The run ended and took the list: its runner writes this waiter its end and then
        // touches it no more.
        while self.waiter.state.load(Ordering::Acquire) == RUNNING {
            hint::spin_loop();
        }
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
        let Some(InFlight { runner, flight: number, waiters, .. }) = self.flights.shard(&self.key).lock().remove(&self.key)
        else {
            return;
        };
        debug_assert_eq!(runner, self.runner, "a run's entry is its own until it ends");
        let state = if value.is_some() { DONE } else { GAVE_WAY };
        let mut at = waiters.0;
        // SAFETY: the list was taken out of its shard under the lock, so no waiter of it is
        // unlinked or leaves before this writes its state, and no other thread writes a `next`.
        // The waiter is in place until its state is stored, and everything read of it is read
        // before that.
        while let Some(waiter) = unsafe { at.as_ref() } {
            // SAFETY: as above; no other thread writes a `next` of a list taken out.
            let next = unsafe { *waiter.next.get() };
            if let Some(value) = value {
                // clone: each waiter's own value
                let value = Some(value.clone());
                // SAFETY: the waiter reads `value` only once it loaded `DONE`, stored below, and
                // nobody else writes it.
                unsafe { *waiter.value.get() = value };
            }
            waiter.state.store(state, Ordering::Release);
            at = next;
        }
        if number != 0 {
            sched::key_done(flight(number));
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

/// A task's id in a run's entry: its job's, from 1, so 0 is none.
fn task(job: JobId) -> u64 {
    u64::from(job.0) + 1
}

/// The job of a task's id.
fn job(task: u64) -> JobId {
    JobId(u32::try_from(task - 1).expect("a task id is a job's"))
}

/// What a profiler is told of a wait for a key another task runs, so that the time reads as a
/// span of its own naming what it waits for, not as the asker's: a convoy behind one slow
/// query shows as many waits on it.
#[derive(Clone, Copy)]
pub struct WaitSpans {
    /// Opens the wait's span on this thread, `awaited` naming what it waits for: a number for
    /// `end`.
    pub begin: fn(awaited: &str) -> u64,
    /// Closes the span `begin` gave, on whatever thread the wait ended on (a job on a fiber
    /// may resume on another).
    pub end: fn(u64),
}

static WAIT_SPANS: OnceLock<WaitSpans> = OnceLock::new();

/// Spans every wait for a key from now on with `spans`; `Err` with them when spans were
/// given before.
pub fn span_waits(spans: WaitSpans) -> Result<(), WaitSpans> {
    WAIT_SPANS.set(spans)
}

/// A wait's span, open while this lives; nothing while no profiler spans waits.
struct Waiting(Option<(fn(u64), u64)>);

impl Waiting {
    fn begin(named: &mut impl FnMut(&mut String)) -> Waiting {
        Waiting(WAIT_SPANS.get().map(|spans| {
            // alloc: only while a profiler spans waits, once a wait, which suspends
            let mut awaited = String::new();
            named(&mut awaited);
            (spans.end, (spans.begin)(&awaited))
        }))
    }
}

impl Drop for Waiting {
    fn drop(&mut self) {
        if let Some((end, span)) = self.0 {
            end(span);
        }
    }
}

/// Waits for the run `waiter` is linked into, suspending this task on the run's `flight`: its
/// value, `None` when it was cancelled, or `Cancelled` when the wait would close a cycle. The
/// runner writes every waiter's end before it ends the flight, so once the wait is over the
/// end is there.
fn wait<V>(waiter: &Waiter<V>, flight: Key) -> Result<Option<V>, Cancelled> {
    if waiter.state.load(Ordering::Acquire) == RUNNING && sched::wait(Wait::Key(flight)).is_err() {
        return Err(give_way());
    }
    Ok(match waiter.state.load(Ordering::Acquire) {
        RUNNING => unreachable!("a wait on a run's flight ended before the run wrote its waiters"),
        // SAFETY: the runner wrote the value before it stored `DONE`, which this load read,
        // and touches the waiter no more.
        DONE => unsafe { (*waiter.value.get()).take() },
        _ => None,
    })
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
    // alloc: for the panic of a key that asks for itself, once, or a wait's span while profiling
    key.0.iter().take(8).map(|it| format!("{it:02x}")).collect()
}

#[cfg(all(test, not(blaze_check)))]
mod tests {
    use std::{
        sync::{
            Arc, Barrier,
            atomic::{AtomicU32, AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };

    use blaze_alloc_count::{Used, measure};
    use blaze_ids::rng::SplitMix64 as Rng;
    use crossbeam_channel::unbounded;

    use super::{Flights, shard_count};
    use crate::{
        Cancelled, Key,
        pool::Pool,
        sched::fiber::GLOBAL_TAKEN,
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

    #[test]
    fn flights_have_four_shards_a_core_or_more() {
        let flights = Flights::<u32>::new();
        let cores = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
        assert!(flights.shards.len().is_power_of_two());
        assert!(flights.shards.len() >= 4 * cores && flights.shards.len() >= 64);
        assert_eq!(flights.shards.len(), shard_count());
        assert_eq!(align_of_val(&flights.shards[0]), 128);
    }

    /// Runs nobody waits for, nested and one after another, never take the scheduler's lock
    /// once the thread is a job of it.
    #[test]
    fn a_run_nobody_waits_for_takes_no_lock_of_the_process() {
        let flights = Flights::<u32>::new();
        assert_eq!(flights.get_or_run(key(u32::MAX), || Ok(0)), Ok(0));
        let before = GLOBAL_TAKEN.with(std::cell::Cell::get);
        for n in 0..1000u32 {
            let value = flights.get_or_run(key(n), || {
                flights.get_or_run(key(n + 1_000_000), || Ok(n)).map(|it| it + 1)
            });
            assert_eq!(value, Ok(n + 1));
        }
        assert_eq!(GLOBAL_TAKEN.with(std::cell::Cell::get), before);
        assert_eq!(flights.running(), 0);
    }

    /// A run ahead runs a key nobody runs, and gives way at once, running nothing, to a key
    /// another task runs; the key is free again after.
    #[test]
    fn a_run_if_idle_never_waits() {
        let flights = Arc::new(Flights::<u32>::new());
        assert_eq!(flights.run_if_idle(key(1), || Ok(7)), Some(Ok(7)));
        let (started, finish) = (Arc::new(Barrier::new(2)), Arc::new(Barrier::new(2)));
        let runner = std::thread::spawn({
            let (flights, started, finish) = (flights.clone(), started.clone(), finish.clone());
            move || {
                flights.get_or_run(key(2), || {
                    started.wait();
                    finish.wait();
                    Ok(2)
                })
            }
        });
        started.wait();
        let before = GLOBAL_TAKEN.with(std::cell::Cell::get);
        assert_eq!(flights.run_if_idle(key(2), || panic!("ran a key another task runs")), None);
        assert_eq!(GLOBAL_TAKEN.with(std::cell::Cell::get), before, "giving way takes no lock");
        finish.wait();
        assert_eq!(runner.join().unwrap(), Ok(2));
        assert_eq!(flights.run_if_idle(key(2), || Ok(3)), Some(Ok(3)));
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

    /// Key 1 run on one thread while another waits for it: what each allocates, and what the
    /// waiter got. Each thread is a job of the scheduler before it is measured.
    fn miss_then_hit<V: Clone + Send + Sync + 'static>(flights: &Flights<V>, value: V) -> (Used, Used, V) {
        let at = key(1);
        let linked = || flights.shard(&at).lock().get(&at).is_some_and(|it| !it.waiters.0.is_null());
        std::thread::scope(|scope| {
            let runner = scope.spawn(move || {
                _ = crate::sched::job_here();
                measure(move || {
                    flights.get_or_run(at, move || {
                        until("the waiter to link itself", linked);
                        Ok(value)
                    })
                })
            });
            let waiter = scope.spawn(move || {
                _ = crate::sched::job_here();
                until("the run to begin", || flights.running() == 1);
                measure(|| flights.get_or_run(at, || panic!("the waiter ran the key")))
            });
            let (ran, by_runner) = runner.join().unwrap();
            let (got, by_waiter) = waiter.join().unwrap();
            let got = got.unwrap_or_else(|_| panic!("the wait was cancelled"));
            assert!(ran.is_ok());
            (by_runner, by_waiter, got)
        })
    }

    /// A run with a waiter, warm: neither thread allocates but for the value's clone the
    /// runner makes its waiter (none for a `u64`, one for a `Vec`).
    #[test]
    fn a_miss_then_a_hit_allocate_only_the_values_clone() {
        let numbers = Flights::<u64>::new();
        for round in 0..4 {
            miss_then_hit(&numbers, round);
        }
        let (runner, waiter, got) = miss_then_hit(&numbers, 99);
        assert_eq!((runner.allocations, waiter.allocations, got), (0, 0, 99), "{runner:?} {waiter:?}");

        let vectors = Flights::<Vec<u64>>::new();
        for round in 0..4 {
            miss_then_hit(&vectors, vec![round; 8]);
        }
        let (runner, waiter, got) = miss_then_hit(&vectors, vec![7; 8]);
        assert_eq!((runner.allocations, waiter.allocations), (1, 0), "the runner clones for its one waiter");
        assert_eq!(got, vec![7; 8]);
        assert_eq!(vectors.running(), 0);
    }

    /// Many waiters of one run, linked and unlinked as they come: each gets the run's value.
    #[test]
    fn every_waiter_of_a_run_gets_its_value() {
        let flights = Flights::<Vec<u32>>::new();
        let at = key(3);
        let waiting = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            let runner = scope.spawn(|| {
                flights.get_or_run(at, || {
                    until("eight waiters", || {
                        let mut count = 0;
                        let shard = flights.shard(&at).lock();
                        let mut link = shard.get(&at).map_or(std::ptr::null(), |it| it.waiters.0);
                        // SAFETY: under the shard's lock each linked waiter is in place.
                        while let Some(waiter) = unsafe { link.as_ref() } {
                            count += 1;
                            // SAFETY: as above.
                            link = unsafe { *waiter.next.get() };
                        }
                        count == 8
                    });
                    Ok(vec![5, 6])
                })
            });
            let waiters: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        until("the run to begin", || flights.running() == 1);
                        waiting.fetch_add(1, Ordering::SeqCst);
                        flights.get_or_run(at, || Ok(Vec::new()))
                    })
                })
                .collect();
            assert_eq!(runner.join().unwrap(), Ok(vec![5, 6]));
            for waiter in waiters {
                assert_eq!(waiter.join().unwrap(), Ok(vec![5, 6]));
            }
        });
        assert_eq!(waiting.load(Ordering::SeqCst), 8);
        assert_eq!(flights.running(), 0);
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
            let mut rng = Rng::new(seed);
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

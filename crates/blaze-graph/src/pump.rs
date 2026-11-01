//! The rule for a wait inside a task (#1301, docs/v2/plan/lanes/core.md "Issues this lane takes in"): a
//! thread that waits for what another task makes runs other ready jobs of its pool while it
//! waits (it **pumps**), and a wait that would close a cycle gives way at once with
//! [`Cancelled`], so the task that asked unwinds, releases what it holds and is posted again.
//! No wait can deadlock: every wait is checked against every other before it begins.
//!
//! The task a thread runs now is the top of a stack the pool keeps for the thread: a job the
//! thread pumps while it waits is pushed above the job that waits, and that one cannot go on
//! before it returns. Who waits for whom is one graph for the process, of three kinds of edge:
//! a task waits for the task that runs the key it asked ([`Flights::get_or_run`]), a task that
//! pumped waits for the job above it on its thread, and a task in a scope
//! ([`crate::pool::Scope`], so every loop of [`crate::par`]) waits for each job of it that has
//! started, wherever it runs. A scope's owner holds across that wait only the runs it began
//! before the scope, so a wait reaches its jobs only through such a run; a run it began inside
//! the scope (in an item it runs itself) ends before it waits. A wait whose edge would close a
//! cycle is refused.
//!
//! The graph costs nothing until a task waits. Each thread publishes its stack in a [`Stack`]
//! of its own (768 bytes, 128-byte aligned, written by its thread alone): starting a job is
//! four stores to it and ending one a fifth, and the edge to the job above is read from it.
//! A wait's own edge, to the runner it waits for, sits beside the task in the same slot. Only
//! a wait takes the one process-wide lock ([`WAITS`]), to check and add its edge as one step; a
//! run nobody waits for takes its key's shard twice and nothing else.
//!
//! Nothing here allocates once warm (docs/v2/plan/lanes/mem/core/plane.md) but a value's
//! clones: a waiter is a node on its own stack, linked into its run's entry, and the runner
//! writes each waiter its clone; a thread's slot, once freed, is linked through its own stack.
//!
//! A task on a fiber of [`crate::sched::fiber`] pumps nothing: it is its job, each run it
//! starts is a flight in the scheduler, and a wait suspends the job on the flight it waits for
//! and resumes when the runner has written its end; a wait the scheduler refuses as a cycle is
//! [`Cancelled`].

use std::{
    cell::{Cell, UnsafeCell},
    collections::hash_map::Entry,
    hash::{BuildHasherDefault, Hasher},
    num::NonZero,
    ptr,
    sync::PoisonError,
};

use blaze_mem::{Column, Tag as MemTag};
use rustc_hash::{FxBuildHasher, FxHashMap};

use crate::{
    Cancelled, Key,
    sched::{JobId, Wait, fiber},
    sync::{
        MutexGuard, OnceLock,
        atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering},
        hint,
        thread::{self, Thread},
        thread_local,
    },
};

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

// A task's id says where it is published: its thread's slot (24 bits, from 1), its depth in
// that thread's stack (5 bits; `CLIENT` for a thread of no pool) and a count of the slot's
// tasks (35 bits), so an id is never 0 and no two live tasks share one.
const THREAD_SHIFT: u32 = 40;
const DEPTH_SHIFT: u32 = 35;
const SEQ_MASK: u64 = (1 << DEPTH_SHIFT) - 1;
const DEPTH_MASK: u64 = (1 << (THREAD_SHIFT - DEPTH_SHIFT)) - 1;
const CLIENT: u64 = DEPTH_MASK;
const _: () = assert!(PUMP_DEPTH < CLIENT as usize);

/// A task as the cycle check sees it: its id (0 for none); the runner of the key it waits for
/// and when that run began ([`next_seq`] on the runner's thread), 0 for none, both written under
/// [`WAITS`]; and, for a scope's job, the scope's owner, which waits for it to end, and when
/// the scope began (0 for none; see [`waited_by`]). 40 bytes.
struct Published {
    task: AtomicU64,
    on: AtomicU64,
    on_since: AtomicU64,
    parent: AtomicU64,
    parent_since: AtomicU64,
}

impl Published {
    const fn new() -> Published {
        Published {
            task: AtomicU64::new(0),
            on: AtomicU64::new(0),
            on_since: AtomicU64::new(0),
            parent: AtomicU64::new(0),
            parent_since: AtomicU64::new(0),
        }
    }
}

/// A thread's tasks, bottom first. Written by its thread but for `on`; read by any cycle check.
#[repr(align(128))]
struct Stack {
    len: AtomicU32,
    /// The slot freed before this one, while this one is free (0 for none): written and read
    /// under [`FREE`]. `std`'s even under `blaze_check`: [`FREE`]'s list outlives an execution,
    /// as its head does, where a model atomic would start the next one from 0 and lose it.
    next_free: std::sync::atomic::AtomicU32,
    /// The slot's task count, kept for the next thread given it.
    seq: AtomicU64,
    /// The thread itself, for a wait outside any job.
    client: Published,
    tasks: [Published; PUMP_DEPTH + 1],
}

#[cfg(not(blaze_check))]
const _: () = assert!(size_of::<Stack>() == 768);

impl Stack {
    fn new() -> Stack {
        Stack {
            len: AtomicU32::new(0),
            next_free: std::sync::atomic::AtomicU32::new(0),
            seq: AtomicU64::new(0),
            client: Published::new(),
            tasks: [const { Published::new() }; PUMP_DEPTH + 1],
        }
    }
}

/// The most threads with a task stack at once.
const SLOTS: usize = 1 << 18;

/// Every thread's stack by slot, in a blaze-mem column: published once and never freed or
/// moved, so a check reads any thread's without a lock. Slot 0 is no thread's: it is "none" in
/// [`FREE`]'s list. A once cell: under `blaze_check` a set `static` cell stays set across
/// executions, so the column is made once per process there too.
static STACKS: OnceLock<Column<Stack>> = OnceLock::new();

fn stacks() -> &'static Column<Stack> {
    STACKS.get_or_init(|| {
        let stacks = Column::new(SLOTS, MemTag::Plane)
            .unwrap_or_else(|why| panic!("the pump's task stacks: {why}"));
        stacks.push(Stack::new()).unwrap_or_else(|why| panic!("the pump's slot 0: {why}"));
        stacks
    })
}

struct Free {
    /// The slot freed last (0 for none), the others through [`Stack::next_free`].
    head: u32,
}

/// The slots of threads that ended, for the next, last freed first; taken once by each
/// thread.
static FREE: Lock<Free> = Lock::new(Free { head: 0 });

fn stack(index: u64) -> Option<&'static Stack> {
    stacks().get(u32::try_from(index).ok()?)
}

fn claim() -> (u32, &'static Stack) {
    let mut free = FREE.lock();
    match free.head {
        0 => {
            let index = stacks().push(Stack::new()).unwrap_or_else(|why| {
                panic!("more than {SLOTS} threads have a task stack at once: {why}")
            });
            (index, stack(u64::from(index)).expect("a stack just pushed is published"))
        }
        head => {
            let freed = stack(u64::from(head)).expect("a freed slot's stack is published");
            free.head = freed.next_free.load(Ordering::Relaxed);
            (head, freed)
        }
    }
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
        let mut free = FREE.lock();
        self.stack.next_free.store(free.head, Ordering::Relaxed);
        free.head = self.index;
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
        entry.parent.store(0, Ordering::Relaxed);
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

/// The task this thread runs now: the top of its stack, or the thread itself outside any job.
pub(crate) fn current() -> u64 {
    ME.with(|me| match me.stack.len.load(Ordering::Relaxed) as usize {
        0 => me.client,
        len => me.stack.tasks[len - 1].task.load(Ordering::Relaxed),
    })
}

thread_local! {
    static SEQ: Cell<u64> = const { Cell::new(0) };
    static GAVE_WAYS: Cell<u64> = const { Cell::new(0) };
}

/// How many waits on this thread have given way ([`Cancelled`] for a cycle): a loop tells an
/// item that unwound because it gave way, which it may run again where the cycle is its own,
/// from one that failed.
pub(crate) fn gave_way() -> u64 {
    GAVE_WAYS.with(Cell::get)
}

fn give_way() -> Cancelled {
    GAVE_WAYS.with(|it| it.set(it.get() + 1));
    Cancelled
}

/// A number larger than every one this thread gave before (from 1): orders the start of a
/// task's runs against the start of its scopes, all on the task's own thread.
pub(crate) fn next_seq() -> u64 {
    SEQ.with(|seq| {
        let next = seq.get() + 1;
        seq.set(next);
        next
    })
}

/// Says the task this thread runs now (a scope's job, at its start) is waited for by `parent`,
/// the scope's owner, which waits in the scope for it to end, and that the scope began at
/// `since` ([`next_seq`] on the owner's thread). The owner holds across that wait only the
/// runs it began before `since`; a run it began after (in an item it runs itself) ends before
/// it waits. The edge needs no check: a task just started waits for nobody yet, so no cycle
/// runs through it.
pub(crate) fn waited_by(parent: u64, since: u64) {
    ME.with(|me| {
        if let Some(len) = (me.stack.len.load(Ordering::Relaxed) as usize).checked_sub(1) {
            let entry = &me.stack.tasks[len];
            entry.parent_since.store(since, Ordering::Relaxed);
            entry.parent.store(parent, Ordering::Release);
        }
    });
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

/// When the run `task` waits for began, 0 when it waits for none or has ended.
fn on_since(task: u64) -> u64 {
    published(task).map_or(0, |entry| entry.on_since.load(Ordering::Relaxed))
}

/// The cycle check's scratch, kept between checks.
struct Walk {
    /// Tasks to visit, each with when the run it was reached for began (0 when reached as a
    /// job some task waits for whole: the job above a pump, a scope's job).
    next: Vec<(u64, u64)>,
    /// Each task visited, with the earliest run it was reached for.
    seen: FxHashMap<u64, u64>,
    /// Each live scope job as (its scope's owner, when the scope began, the job), read at the
    /// check's start.
    children: Vec<(u64, u64, u64)>,
}

impl Walk {
    /// Whether `from`, the runner of a run begun at `since`, waits through any chain for `to`.
    /// A scope's owner reached for a run waits for the scope's jobs only when the run began
    /// before the scope: one begun after ends before the owner waits. The graph without the
    /// edge being checked has no cycle (each edge was checked as it was added, and an edge to a
    /// job just pumped or just started for a scope cannot close one), so the walk ends.
    fn reaches(&mut self, from: u64, since: u64, to: u64) -> bool {
        self.next.clear();
        self.seen.clear();
        self.read_children();
        self.next.push((from, since));
        while let Some((task, came)) = self.next.pop() {
            if task == to {
                return true;
            }
            if task == 0 {
                continue;
            }
            match self.seen.entry(task) {
                Entry::Occupied(mut was) if came < *was.get() => _ = was.insert(came),
                Entry::Occupied(_) => continue,
                Entry::Vacant(slot) => _ = slot.insert(came),
            }
            let (on, above) = edges(task);
            self.next.push((on, on_since(task)));
            self.next.push((above, 0));
            for &(parent, scope, child) in &self.children {
                if parent == task && came < scope {
                    self.next.push((child, 0));
                }
            }
        }
        false
    }

    /// Fills `children` from every thread's published stack. A job that starts after this is
    /// read waits for nobody yet; its own first wait is checked after this one, under the
    /// same lock, and sees this check's edge.
    fn read_children(&mut self) {
        self.children.clear();
        for (_, stack) in stacks().iter() {
            let len = (stack.len.load(Ordering::Acquire) as usize).min(PUMP_DEPTH + 1);
            for entry in &stack.tasks[..len] {
                let task = entry.task.load(Ordering::Acquire);
                let parent = entry.parent.load(Ordering::Acquire);
                let scope = entry.parent_since.load(Ordering::Relaxed);
                if task != 0 && parent != 0 && entry.task.load(Ordering::Acquire) == task {
                    self.children.push((parent, scope, task));
                }
            }
        }
    }
}

/// Orders the waits: a check and the edge it adds are one step against every other wait's.
static WAITS: Lock<Walk> = Lock::new(Walk {
    // alloc: none, empty: the check's scratch, kept for the process
    next: Vec::new(),
    seen: FxHashMap::with_hasher(FxBuildHasher),
    // alloc: none, empty: as `next`
    children: Vec::new(),
});

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
    task: u64,
    thread: Thread,
    /// The waiter linked before this one: written under the run's shard lock, read under it
    /// or by the runner once the list is its own.
    next: UnsafeCell<*const Waiter<V>>,
    /// The run's value, written by the runner before `state` says `DONE`.
    value: UnsafeCell<Option<V>>,
    state: AtomicU8,
}

impl<V> Waiter<V> {
    fn new(task: u64) -> Waiter<V> {
        Waiter {
            task,
            thread: thread::current(),
            next: UnsafeCell::new(ptr::null()),
            value: UnsafeCell::new(None),
            state: AtomicU8::new(RUNNING),
        }
    }
}

/// A run's waiters, the last linked first; null for none.
struct Waiters<V>(*const Waiter<V>);

// SAFETY: each waiter is reached through the list only under its shard's lock, or by the
// runner once it took the list out, and the runner sends it a `V` (hence `V: Send`).
unsafe impl<V: Send> Send for Waiters<V> {}

/// A run under way, in its key's shard: 24 bytes beside its key.
struct InFlight<V> {
    runner: u64,
    /// When the run began, on its runner's thread ([`next_seq`]).
    since: u64,
    waiters: Waiters<V>,
}

impl<V> InFlight<V> {
    fn new(runner: u64) -> InFlight<V> {
        InFlight { runner, since: next_seq(), waiters: Waiters(ptr::null()) }
    }
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
        if let Some(job) = fiber::current_job() {
            return self.get_or_run_on_fiber(fiber_task(job), key, run);
        }
        let me = current();
        loop {
            let waiter = Waiter::new(me);
            if !self.join(key, me, &waiter)? {
                break;
            }
            let linked = Linked { flights: self, key, waiter: &waiter };
            let got = wait(&waiter);
            drop(linked);
            if let Some(value) = got {
                return Ok(value);
            }
        }
        let mut ending = Ending { flights: self, key, runner: me, flight: None, ended: false };
        let result = run();
        ending.end(result.as_ref().ok());
        result
    }

    /// [`Flights::get_or_run`] for a task on a fiber (docs/v2/plan/lanes/sched.md, phase 2):
    /// the task is its job, a wait suspends the job on its run's flight in the scheduler
    /// instead of pumping, and a wait the scheduler refuses as a cycle gives way.
    #[inline(never)]
    fn get_or_run_on_fiber(
        &self,
        me: u64,
        key: Key,
        run: impl FnOnce() -> Result<V, Cancelled>,
    ) -> Result<V, Cancelled> {
        fiber::miss();
        loop {
            let waiter = Waiter::new(me);
            let Some(runner) = self.join_fiber(key, me, &waiter) else { break };
            let linked = Linked { flights: self, key, waiter: &waiter };
            let got = wait_on_fiber(&waiter, self.flight(key, runner));
            drop(linked);
            if let Some(value) = got? {
                return Ok(value);
            }
        }
        let mut ending = Ending { flights: self, key, runner: me, flight: Some(self.flight(key, me)), ended: false };
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
        let on_fiber = fiber::current_job();
        let me = on_fiber.map_or_else(current, fiber_task);
        match self.shard(&key).lock().entry(key) {
            Entry::Vacant(slot) => {
                slot.insert(InFlight::new(me));
                if on_fiber.is_some() {
                    self.claim_flight(key, me);
                }
            }
            Entry::Occupied(slot) if slot.get().runner == me => {
                panic!("key {} is asked by the task that runs it: its value depends on itself", hex(&key))
            }
            Entry::Occupied(_) => return None,
        }
        let flight = on_fiber.map(|_| self.flight(key, me));
        let mut ending = Ending { flights: self, key, runner: me, flight, ended: false };
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

    /// The scheduler's flight of one run of `key`: by `runner`, in these flights. Each run has
    /// its own, so a run that starts while the last one's waiters are being woken never finds
    /// the key's flight taken.
    fn flight(&self, key: Key, runner: u64) -> Key {
        let mut bytes = key.0;
        let mix = |bytes: &mut [u8; 32], at: usize, word: u64| {
            for (byte, add) in bytes[at..at + 8].iter_mut().zip(word.to_le_bytes()) {
                *byte ^= add;
            }
        };
        mix(&mut bytes, 0, runner);
        mix(&mut bytes, 8, ptr::from_ref(self).addr() as u64);
        Key(bytes)
    }

    /// Claims the scheduler's flight of `me`'s run of `key`, under the key's shard lock, so a
    /// waiter that finds the run finds its flight (or none, once it ended).
    fn claim_flight(&self, key: Key, me: u64) {
        let claimed = fiber::claim(self.flight(key, me));
        assert_eq!(claimed, Some(true), "the flight of key {}'s run by task {me:#x} is new and on a fiber", hex(&key));
    }

    /// As [`Flights::join`], for a task on a fiber: `None` when this caller is to run `key`
    /// (its flight claimed), else the runner `waiter` is linked to wait for. The cycle check
    /// is the scheduler's, at the wait.
    fn join_fiber(&self, key: Key, me: u64, waiter: &Waiter<V>) -> Option<u64> {
        let mut shard = self.shard(&key).lock();
        let entry = match shard.entry(key) {
            Entry::Vacant(slot) => {
                slot.insert(InFlight::new(me));
                self.claim_flight(key, me);
                return None;
            }
            Entry::Occupied(slot) => slot.into_mut(),
        };
        let runner = entry.runner;
        if runner == me {
            panic!("key {} is asked by the task that runs it: its value depends on itself", hex(&key));
        }
        // SAFETY: as in `join`: `next` is the waiter's own until it is linked, here, under the
        // shard's lock, and the caller keeps it in place until its run ends ([`Linked`]).
        unsafe { *waiter.next.get() = entry.waiters.0 };
        entry.waiters.0 = ptr::from_ref(waiter);
        Some(runner)
    }

    /// `false` when this caller is to run `key`; `true` when `waiter` is linked into the run
    /// to wait for.
    fn join(&self, key: Key, me: u64, waiter: &Waiter<V>) -> Result<bool, Cancelled> {
        let mut shard = self.shard(&key).lock();
        let entry = match shard.entry(key) {
            Entry::Vacant(slot) => {
                slot.insert(InFlight::new(me));
                return Ok(false);
            }
            Entry::Occupied(slot) => slot.into_mut(),
        };
        let (runner, since) = (entry.runner, entry.since);
        if runner == me {
            panic!("key {} is asked by the task that runs it: its value depends on itself", hex(&key));
        }
        {
            let mut waits = waits();
            if waits.reaches(runner, since, me) {
                return Err(give_way());
            }
            if let Some(mine) = published(me) {
                mine.on_since.store(since, Ordering::Relaxed);
                mine.on.store(runner, Ordering::Relaxed);
            }
        }
        // SAFETY: `next` is the waiter's own until it is linked, here, under the shard's lock;
        // the caller keeps the waiter where it is until its run ends ([`Linked`]).
        unsafe { *waiter.next.get() = entry.waiters.0 };
        entry.waiters.0 = ptr::from_ref(waiter);
        Ok(true)
    }
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
                let runner = entry.runner;
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
                        if let Some(mine) = published(self.waiter.task) {
                            _ = mine.on.compare_exchange(runner, 0, Ordering::Relaxed, Ordering::Relaxed);
                        }
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
    /// The scheduler's flight of a run on a fiber, ended once every waiter has its end.
    flight: Option<Key>,
    ended: bool,
}

impl<V: Clone + Send + Sync + 'static> Ending<'_, V> {
    fn end(&mut self, value: Option<&V>) {
        self.ended = true;
        let Some(InFlight { runner, waiters, .. }) = self.flights.shard(&self.key).lock().remove(&self.key)
        else {
            return;
        };
        debug_assert_eq!(runner, self.runner, "a run's entry is its own until it ends");
        // Each waiter's edge goes before it can see the end and wait again, and while this
        // runner is in `end` it waits for nobody, so an edge read in between leads nowhere.
        let mut at = waiters.0;
        // SAFETY: the list was taken out of its shard under the lock, so no waiter of it is
        // unlinked or leaves before this writes its state, and no other thread writes a `next`.
        while let Some(waiter) = unsafe { at.as_ref() } {
            if let Some(entry) = published(waiter.task) {
                _ = entry.on.compare_exchange(runner, 0, Ordering::Relaxed, Ordering::Relaxed);
            }
            // SAFETY: as above.
            at = unsafe { *waiter.next.get() };
        }
        let state = if value.is_some() { DONE } else { GAVE_WAY };
        let mut at = waiters.0;
        // SAFETY: as above: the waiter is in place until its state is stored, and everything read
        // of it is read before that. `waiter` is not used after the store, whose `&self` outlives
        // the store itself only as `std`'s own wake-ups' do (rust-lang/rust#55005).
        while let Some(waiter) = unsafe { at.as_ref() } {
            // SAFETY: as above; no other thread writes a `next` of a list taken out.
            let next = unsafe { *waiter.next.get() };
            // clone: a reference count, so the waiter is woken after it may have left
            let thread = waiter.thread.clone();
            if let Some(value) = value {
                // clone: each waiter's own value
                let value = Some(value.clone());
                // SAFETY: the waiter reads `value` only once it loaded `DONE`, stored below, and
                // nobody else writes it.
                unsafe { *waiter.value.get() = value };
            }
            waiter.state.store(state, Ordering::Release);
            at = next;
            thread.unpark();
        }
        if let Some(flight) = self.flight {
            fiber::key_done(flight).expect("a run on a fiber ends on its fiber");
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

/// Waits for the run `waiter` is linked into to end, pumping while this thread may: its value,
/// or `None` when it was cancelled.
fn wait<V>(waiter: &Waiter<V>) -> Option<V> {
    wait_until(&|| waiter.state.load(Ordering::Acquire) != RUNNING);
    match waiter.state.load(Ordering::Acquire) {
        // SAFETY: the runner wrote the value before it stored `DONE`, which this load read,
        // and touches the waiter no more.
        DONE => unsafe { (*waiter.value.get()).take() },
        _ => None,
    }
}

/// A fiber task's id: its job, in slot 0, which is no thread's, so it is never a thread's
/// task and the thread-stack cycle check finds no edge of it (the scheduler checks its waits).
fn fiber_task(job: JobId) -> u64 {
    u64::from(job.0) + 1
}

/// Waits on a fiber for the run `waiter` is linked into, suspending the job on the run's
/// `flight`: its value, `None` when it was cancelled, or `Cancelled` when the wait would close
/// a cycle. A run by a thread, not a fiber, has no flight to suspend on: the wait comes back
/// at once and the fiber spins on the waiter until that run ends.
fn wait_on_fiber<V>(waiter: &Waiter<V>, flight: Key) -> Result<Option<V>, Cancelled> {
    while waiter.state.load(Ordering::Acquire) == RUNNING {
        match fiber::wait(Wait::Key(flight)) {
            Some(Ok(())) => {}
            Some(Err(_)) => return Err(give_way()),
            None => unreachable!("a fiber task waits on its fiber"),
        }
        if waiter.state.load(Ordering::Acquire) == RUNNING {
            hint::spin_loop();
        }
    }
    Ok(match waiter.state.load(Ordering::Acquire) {
        // SAFETY: as in `wait`: the runner wrote the value before it stored `DONE`.
        DONE => unsafe { (*waiter.value.get()).take() },
        _ => None,
    })
}

/// Waits until `ended` is true, as a wait for a key does: on a pool's worker it runs that
/// pool's jobs (below [`PUMP_DEPTH`]), spins a little, then parks; elsewhere it spins, then
/// parks. Whoever makes `ended` true unparks this thread (`Thread::unpark`); `ended` is read
/// again after each wake, so a spurious one only loops.
pub fn wait_until(ended: &impl Fn() -> bool) {
    let mut idle = 0u32;
    loop {
        if ended() {
            return;
        }
        let pump = depth() < PUMP_DEPTH;
        if pump && idle % 16 == 0 && crate::pool::run_one() {
            idle = 0;
            continue;
        }
        if idle < SPINS {
            idle += 1;
            hint::spin_loop();
            continue;
        }
        crate::pool::park(ended, pump);
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
    // alloc: for the panic of a key that asks for itself, once
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
        let before = WAITS_TAKEN.with(std::cell::Cell::get);
        assert_eq!(flights.run_if_idle(key(2), || panic!("ran a key another task runs")), None);
        assert_eq!(WAITS_TAKEN.with(std::cell::Cell::get), before, "giving way takes no lock");
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

    /// On a worker, `wait_until` runs the pool's jobs while it waits: with one worker, the job
    /// that ends the wait can only run pumped. On a client's thread it parks until unparked.
    #[test]
    fn wait_until_pumps_on_a_worker_and_parks_elsewhere() {
        let pool = Pool::new(&Topology::flat(1), false).unwrap();
        let ended = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = unbounded::<&'static str>();
        {
            let (ended, tx) = (ended.clone(), tx.clone());
            pool.submit(
                None,
                move || {
                    ended.store(1, Ordering::SeqCst);
                    super::wait_until(&|| ended.load(Ordering::SeqCst) == 2);
                },
                move |it| {
                    it.unwrap();
                    _ = tx.send("waiter");
                },
            );
        }
        until("the job to wait", || ended.load(Ordering::SeqCst) == 1);
        {
            let (ended, tx) = (ended.clone(), tx.clone());
            pool.submit(None, move || ended.store(2, Ordering::SeqCst), move |it| {
                it.unwrap();
                _ = tx.send("ender");
            });
        }
        let mut seen = [rx.recv_timeout(LONG).unwrap(), rx.recv_timeout(LONG).unwrap()];
        seen.sort_unstable();
        assert_eq!(seen, ["ender", "waiter"]);

        let flag = Arc::new(AtomicUsize::new(0));
        std::thread::scope(|scope| {
            let waiter = {
                let flag = flag.clone();
                scope.spawn(move || super::wait_until(&|| flag.load(Ordering::SeqCst) == 1))
            };
            std::thread::sleep(Duration::from_millis(20));
            flag.store(1, Ordering::SeqCst);
            waiter.thread().unpark();
            waiter.join().unwrap();
        });
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

    /// Key 1 run on one thread while another waits for it: what each allocates, and what the
    /// waiter got. Each thread's slot is claimed before it is measured.
    fn miss_then_hit<V: Clone + Send + Sync + 'static>(flights: &Flights<V>, value: V) -> (Used, Used, V) {
        let at = key(1);
        let linked = || flights.shard(&at).lock().get(&at).is_some_and(|it| !it.waiters.0.is_null());
        std::thread::scope(|scope| {
            let runner = scope.spawn(move || {
                _ = depth();
                measure(move || {
                    flights.get_or_run(at, move || {
                        until("the waiter to link itself", linked);
                        Ok(value)
                    })
                })
            });
            let waiter = scope.spawn(move || {
                _ = depth();
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

/// Under the model checker (`RUSTFLAGS="--cfg blaze_check" blaze test -p blaze-graph --release
/// model_`), on threads of no pool (a pool's worker would pump; `pool`'s models check that):
/// a key asked by two threads against the sequential runs of one key, two runs that ask for each
/// other's keys, and a thread's published stack read while it changes.
#[cfg(all(test, blaze_check))]
mod model {
    use std::sync::Arc;

    use blaze_check::{
        linear::{History, Spec},
        model,
    };

    use super::{DEPTH_SHIFT, Flights, SEQ_MASK, THREAD_SHIFT, current, edges, run_task};
    use crate::{
        Cancelled, Key,
        sync::{
            atomic::{AtomicU64, Ordering},
            thread,
        },
    };

    fn key(n: u8) -> Key {
        let mut key = [0u8; 32];
        key[0] = n;
        Key(key)
    }

    #[derive(Debug, Clone, Copy)]
    enum Op {
        /// A `get_or_run` whose run, if it runs, gives `mine`, or is cancelled.
        Get { mine: u32 },
        /// That run begins.
        Begin(u32),
        /// That run ends: with its value when `ok`.
        End { mine: u32, ok: bool },
    }

    #[derive(Debug, Clone, Copy, PartialEq)]
    enum Ret {
        Done,
        /// A run began while another was under way, or a run ended that was not.
        Overlap,
        Got(u32),
        Cancelled,
        /// A result no ended run gave this asker.
        Nothing,
    }

    /// The sequential key: the run under way, and the last one's run and whether it gave its
    /// value.
    #[derive(Clone, PartialEq, Eq, Hash, Default)]
    struct Runs {
        running: Option<u32>,
        last: Option<(u32, bool)>,
    }

    impl Spec for Runs {
        type Op = Op;
        type Ret = Ret;

        fn apply(&mut self, op: &Op) -> Ret {
            match *op {
                Op::Begin(_) if self.running.is_some() => Ret::Overlap,
                Op::Begin(mine) => {
                    self.running = Some(mine);
                    Ret::Done
                }
                Op::End { mine, ok } if self.running == Some(mine) => {
                    self.running = None;
                    self.last = Some((mine, ok));
                    Ret::Done
                }
                Op::End { .. } => Ret::Overlap,
                // A cancel reaches only the asker whose run it was: the others ask again.
                Op::Get { mine } => match self.last {
                    Some((value, true)) => Ret::Got(value),
                    Some((run, false)) if run == mine => Ret::Cancelled,
                    _ => Ret::Nothing,
                },
            }
        }
    }

    /// Asks for `key` as one operation of `history`; its run, if it runs, lets the other thread
    /// go in its middle.
    fn ask(
        history: &History<Runs>,
        flights: &Flights<u32>,
        key: Key,
        mine: u32,
        ok: bool,
    ) -> Result<u32, Cancelled> {
        let mut out = Err(Cancelled);
        history.record(Op::Get { mine }, |_| {
            out = flights.get_or_run(key, || {
                history.record(Op::Begin(mine), |_| Ret::Done);
                thread::yield_now();
                history.record(Op::End { mine, ok }, |_| Ret::Done);
                if ok { Ok(mine) } else { Err(Cancelled) }
            });
            match out {
                Ok(value) => Ret::Got(value),
                Err(Cancelled) => Ret::Cancelled,
            }
        });
        out
    }

    /// Two threads ask for one key: one run at a time, and a thread that asked while the other
    /// ran gets that run's value.
    #[test]
    fn model_flights_run_a_key_once_at_a_time() {
        model(|| {
            let flights = Arc::new(Flights::<u32>::new());
            let history = Arc::new(History::<Runs>::new());
            let other = {
                let (flights, history) = (flights.clone(), history.clone());
                thread::spawn(move || ask(&history, &flights, key(1), 2, true))
            };
            let mine = ask(&history, &flights, key(1), 1, true);
            let theirs = other.join().unwrap();
            history.check(Runs::default());
            assert!(matches!((mine, theirs), (Ok(1 | 2), Ok(1 | 2))), "{mine:?} {theirs:?}");
            assert_eq!(flights.running(), 0);
        });
    }

    /// This thread's run of the key is cancelled: it says so, and the other, if it waited for
    /// that run, asks again and runs the key itself.
    #[test]
    fn model_a_cancelled_run_is_asked_again() {
        model(|| {
            let flights = Arc::new(Flights::<u32>::new());
            let history = Arc::new(History::<Runs>::new());
            let other = {
                let (flights, history) = (flights.clone(), history.clone());
                thread::spawn(move || ask(&history, &flights, key(1), 2, true))
            };
            let mine = ask(&history, &flights, key(1), 1, false);
            let theirs = other.join().unwrap();
            history.check(Runs::default());
            assert!(matches!(mine, Err(Cancelled) | Ok(2)), "{mine:?}");
            assert_eq!(theirs, Ok(2), "a cancelled run's waiter runs the key itself");
            assert_eq!(flights.running(), 0);
        });
    }

    /// Each of two threads runs its key and asks, inside the run, for the other's. When both
    /// runs are under way, the second of the two waits would close a cycle: that one gives way,
    /// its run ends cancelled, the other runs the key given up itself and ends with a value, and
    /// nothing deadlocks.
    #[test]
    fn model_waits_that_would_close_a_cycle_give_way() {
        /// The run of `mine` is the value of `theirs` plus 10; a run of `theirs` asked here is
        /// `theirs`.
        fn cross(flights: &Flights<u32>, mine: u8, theirs: u8) -> Result<u32, Cancelled> {
            flights.get_or_run(key(mine), || {
                flights.get_or_run(key(theirs), || Ok(u32::from(theirs))).map(|it| it + 10)
            })
        }
        model(|| {
            let flights = Arc::new(Flights::<u32>::new());
            let other = {
                let flights = flights.clone();
                thread::spawn(move || cross(&flights, 2, 1))
            };
            let mine = cross(&flights, 1, 2);
            let theirs = other.join().unwrap();
            // Each value is its own run over its own inner run (12, 11), its own run over the
            // other's run that ran its inner key alone (21, 22), or the other's inner run (1, 2).
            assert!(matches!(mine, Ok(12 | 21 | 1) | Err(Cancelled)), "{mine:?}");
            assert!(matches!(theirs, Ok(11 | 22 | 2) | Err(Cancelled)), "{theirs:?}");
            if mine.is_err() {
                assert_eq!(theirs, Ok(11), "the one that went on ran the key given up");
            }
            if theirs.is_err() {
                assert_eq!(mine, Ok(12), "the one that went on ran the key given up");
            }
            assert_eq!(flights.running(), 0);
        });
    }

    /// A thread runs a task with two jobs above it, one after the other, while another reads
    /// the task's edges: it waits for nobody, its job above is none or one of the two, and
    /// once it ended it has no edge at all.
    #[test]
    fn model_a_published_stack_is_read_while_it_changes() {
        /// The id of the `nth` task after `outer` on its thread, one above it.
        fn above(outer: u64, nth: u64) -> u64 {
            let slot = outer >> THREAD_SHIFT << THREAD_SHIFT;
            slot | (1 << DEPTH_SHIFT) | ((outer + nth) & SEQ_MASK)
        }
        model(|| {
            let published = Arc::new(AtomicU64::new(0));
            let reader = {
                let published = published.clone();
                thread::spawn(move || {
                    let outer = published.load(Ordering::Acquire);
                    (outer != 0).then(|| (outer, edges(outer)))
                })
            };
            let outer = run_task(|| {
                let outer = current();
                published.store(outer, Ordering::Release);
                for nth in 1..=2 {
                    run_task(|| assert_eq!(current(), above(outer, nth)));
                }
                outer
            });
            if let Some((seen, (on, up))) = reader.join().unwrap() {
                assert_eq!(seen, outer);
                assert_eq!(on, 0, "the task waits for nobody");
                assert!(up == 0 || up == above(outer, 1) || up == above(outer, 2), "{up:#x}");
            }
            assert_eq!(edges(outer), (0, 0), "an ended task has no edges");
        });
    }
}

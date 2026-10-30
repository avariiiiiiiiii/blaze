//! The threads the plane's tasks run on (docs/v2/plan/lanes/core.md section 4, docs/v2/plan/lanes/core/plane.md "The
//! executor"): one worker per CPU of a [`Topology`], stealing work nearest first.
//!
//! Each worker has a deque of its own (crossbeam's Chase-Lev deque, as Rayon's) and a LIFO
//! slot, and each group (the CPUs sharing a last-level cache or a cluster) an injector. A job
//! submitted near a group goes to that group's injector; one near no group goes to the least
//! loaded group, the fast ones first on a tie (Apple's performance cores, where the plane's
//! first ready nodes, its critical path, go). A job a worker gives itself ([`Pool::submit_next`],
//! or a [`Scope`]'s job submitted on one of the pool's workers) goes to its LIFO slot and runs
//! next, hot in its cache, as in Tokio; after three such jobs in a row the slot's job goes
//! behind the deque's, so the slot never starves it.
//!
//! An idle worker takes from its slot and its deque, then searches: its group's injector and
//! workers (from a random one, half a deque at a time), then each group of its
//! [`Topology::steal_order`] the same way, four times over, then parks. At most half the
//! workers search at once, and a job wakes a parked worker (of its group first) only when none
//! searches; a searcher that finds work and was the last wakes another for the rest (Go's and
//! Tokio's rules), so a burst of work wakes no stampede and none is left unseen. A parked worker
//! sleeps on its own thread's futex (`thread::park`), not a shared condvar.
//!
//! On a machine of fast and slow groups, a job on its request's critical path
//! ([`Pool::submit_critical`]) goes to its `near` group if that is fast, else to the least loaded
//! fast group; stealing is unchanged, so a slow worker with nothing else still takes it. Where
//! every group is fast a critical job is placed as any other. [`node`] names the NUMA node of
//! the calling worker, for memory it should keep local.
//!
//! Bound to its CPU, a worker is pinned to it on Linux (`sched_setaffinity`); on macOS, which
//! pins nothing, it takes the QoS class of its kind of core: user-interactive on fast cores,
//! utility on the others.
//!
//! Every job runs as a task of [`crate::pump`]'s stack, so a wait inside it pumps this pool.

use std::{
    any::Any,
    cell::Cell,
    io,
    marker::PhantomData,
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    ptr::{self, NonNull},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering, fence},
    },
    thread::{self, JoinHandle, Thread},
};

use crossbeam_deque::{Injector, Steal, Stealer, Worker};

use crate::{
    pump::{Lock, PUMP_DEPTH, SPINS, depth, run_task},
    topology::{Cpu, Topology},
};

/// A worker's stack: room for the jobs it pumps above the one that waits.
const STACK: usize = 64 << 20;
/// How many jobs a worker takes from an injector at once: few, so the plane's order (its
/// critical path first) holds across workers.
const BATCH: usize = 4;
/// How many jobs in a row a worker runs from its LIFO slot before its deque's oldest (Tokio's
/// `MAX_LIFO_POLLS_PER_TICK`).
const LIFO_RUNS: u8 = 3;
/// How many times a searching worker looks at every queue before it parks.
const ROUNDS: u32 = 4;

/// What the queues hold: a pointer to a job's [`Header`], which says how to run it. 8 bytes,
/// so a line of a deque's buffer holds eight jobs.
pub(crate) struct Job(NonNull<Header>);

const _: () = assert!(size_of::<Job>() == 8);

// SAFETY: a job is made only of work that may move to another thread (`Job::erase`'s bound, or
// a scheduler's run, whose fields are `Send`), and is run once, by the thread that takes it.
unsafe impl Send for Job {}

/// The first field of every job: the function that runs it, given the job.
#[repr(C)]
pub(crate) struct Header {
    run: unsafe fn(NonNull<Header>),
}

impl Header {
    pub(crate) const fn new(run: unsafe fn(NonNull<Header>)) -> Header {
        Header { run }
    }
}

#[repr(C)]
struct Boxed<F> {
    header: Header,
    f: F,
}

impl Job {
    /// `f` on the heap, freed as it runs.
    fn boxed<F: FnOnce() + Send + 'static>(f: F) -> Job {
        // SAFETY: `f` borrows nothing.
        unsafe { Job::erase(f) }
    }

    /// # Safety
    ///
    /// What `f` borrows outlives its run.
    unsafe fn erase<F: FnOnce() + Send>(f: F) -> Job {
        unsafe fn run<F: FnOnce()>(header: NonNull<Header>) {
            // SAFETY: `header` is the first field of the `Boxed<F>` that `erase` leaked, run once.
            let boxed = unsafe { Box::from_raw(header.cast::<Boxed<F>>().as_ptr()) };
            let Boxed { f, .. } = *boxed;
            f();
        }
        let boxed = Box::leak(Box::new(Boxed { header: Header::new(run::<F>), f }));
        Job(NonNull::from(boxed).cast())
    }

    /// # Safety
    ///
    /// `header` is the first field of a job that lives until it has run, and that no other
    /// [`Job`] names until then.
    pub(crate) unsafe fn from_header(header: NonNull<Header>) -> Job {
        Job(header)
    }

    fn run(self) {
        // SAFETY: a job is run once (it is moved here), and its header says how.
        let run = unsafe { self.0.as_ref().run };
        // SAFETY: as above.
        unsafe { run(self.0) }
    }
}

/// A group's own: 128-byte aligned, as its injector's ends are.
#[repr(align(128))]
struct Group {
    injector: Injector<Job>,
    workers: Box<[u32]>,
    order: Box<[u32]>,
    fast: bool,
    /// Its workers parked or about to park, as their [`Slot::sleeping`] says.
    sleepers: AtomicU32,
}

/// A worker as the others see it: 128-byte aligned, since thieves read it while it writes.
#[repr(align(128))]
struct Slot {
    stealer: Stealer<Job>,
    /// The LIFO slot: the job to run next, which thieves take only when there is nothing else.
    next: AtomicPtr<Header>,
    sleeping: AtomicBool,
    group: u32,
    thread: OnceLock<Thread>,
}

/// How many workers search and how many sleep, in one word so a submit reads both at once.
#[repr(align(128))]
struct Idle(AtomicU64);

const SEARCHING: u64 = 1;
const SLEEPING: u64 = 1 << 32;

fn searching(word: u64) -> u64 {
    word & (SLEEPING - 1)
}

fn sleeping(word: u64) -> u64 {
    word >> 32
}

struct Shared {
    groups: Box<[Group]>,
    slots: Box<[Slot]>,
    idle: Idle,
    stop: AtomicBool,
    /// Of fast and slow groups both: only then does a job's being critical place it.
    mixed: bool,
    /// The first panic of a job's `done`, raised when the pool is dropped.
    stray: Lock<Option<Box<dyn Any + Send>>>,
    #[cfg(test)]
    critical: AtomicUsize,
}

pub struct Pool {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
    topology: Topology,
}

/// A worker's own: on its thread's stack for as long as the thread works.
struct Local {
    shared: Arc<Shared>,
    index: usize,
    group: usize,
    node: u32,
    deque: Worker<Job>,
    /// How many jobs in a row came from the LIFO slot.
    lifo: Cell<u8>,
    /// Where a search of a group's workers begins (xorshift).
    seed: Cell<u32>,
}

thread_local! {
    static LOCAL: Cell<*const Local> = const { Cell::new(ptr::null()) };
}

fn with_local<R>(f: impl FnOnce(&Local) -> R) -> Option<R> {
    let at = LOCAL.get();
    // SAFETY: `work` points it at its own `Local`, which lives until `work` clears it on its
    // way out, on this thread.
    (!at.is_null()).then(|| f(unsafe { &*at }))
}

/// The group of the worker this thread is, or `None` for a thread of no pool.
pub fn group() -> Option<u32> {
    with_local(|local| u32::try_from(local.group).ok()).flatten()
}

/// The NUMA node of the worker this thread is (its CPU's [`Cpu::node`]), or `None` for a
/// thread of no pool.
pub fn node() -> Option<u32> {
    with_local(|local| local.node)
}

impl Pool {
    /// A worker per CPU of `topology`, bound to it when `bind` (pinned on Linux, a QoS class on
    /// macOS; an error elsewhere). An error when a thread cannot be started or bound: the pool
    /// is not made smaller.
    pub fn new(topology: &Topology, bind: bool) -> io::Result<Pool> {
        if topology.cpus.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "a worker pool of no CPU"));
        }
        let count = topology.groups() as usize;
        let mut members: Vec<Vec<u32>> = vec![Vec::new(); count];
        for (index, cpu) in topology.cpus.iter().enumerate() {
            let index = u32::try_from(index).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "a worker pool of more than 2^32 CPUs")
            })?;
            members[cpu.group as usize].push(index);
        }
        if let Some(empty) = members.iter().position(Vec::is_empty) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("group {empty} of the topology has no CPU: groups are numbered densely"),
            ));
        }
        let deques: Vec<Worker<Job>> = topology.cpus.iter().map(|_| Worker::new_fifo()).collect();
        let slots = deques
            .iter()
            .zip(&topology.cpus)
            .map(|(deque, cpu)| Slot {
                stealer: deque.stealer(),
                next: AtomicPtr::new(ptr::null_mut()),
                sleeping: AtomicBool::new(false),
                group: cpu.group,
                thread: OnceLock::new(),
            })
            .collect();
        let groups = members
            .into_iter()
            .enumerate()
            .map(|(at, workers)| Group {
                injector: Injector::new(),
                fast: workers.iter().any(|&it| topology.cpus[it as usize].fast),
                order: topology.steal_order(at as u32).into_boxed_slice(),
                workers: workers.into_boxed_slice(),
                sleepers: AtomicU32::new(0),
            })
            .collect::<Box<[Group]>>();
        let mixed = groups.iter().any(|it| it.fast) && groups.iter().any(|it| !it.fast);
        let shared = Arc::new(Shared {
            groups,
            slots,
            idle: Idle(AtomicU64::new(0)),
            stop: AtomicBool::new(false),
            mixed,
            stray: Lock::new(None),
            #[cfg(test)]
            critical: AtomicUsize::new(0),
        });
        let (ready_tx, ready_rx) = crossbeam_channel::unbounded::<io::Result<()>>();
        let mut threads = Vec::with_capacity(deques.len());
        let mut failed = None;
        for (index, deque) in deques.into_iter().enumerate() {
            let (cpu, shared, ready) = (topology.cpus[index], shared.clone(), ready_tx.clone());
            let spawned = thread::Builder::new()
                .name(format!("blaze-work-{index}"))
                .stack_size(STACK)
                .spawn(move || {
                    let bound = if bind { bind_to(&cpu) } else { Ok(()) };
                    _ = shared.slots[index].thread.set(thread::current());
                    let ok = bound.is_ok();
                    _ = ready.send(bound);
                    if ok {
                        let group = shared.slots[index].group as usize;
                        let seed = (index as u32).wrapping_mul(0x9E37_79B9) | 1;
                        work(&Local {
                            shared,
                            index,
                            group,
                            node: cpu.node,
                            deque,
                            lifo: Cell::new(0),
                            seed: Cell::new(seed),
                        });
                    }
                });
            match spawned {
                Ok(handle) => threads.push(handle),
                Err(why) => {
                    failed = Some(why);
                    break;
                }
            }
        }
        drop(ready_tx);
        for _ in 0..threads.len() {
            match ready_rx.recv() {
                Ok(Ok(())) => {}
                Ok(Err(why)) => _ = failed.get_or_insert(why),
                Err(_) => break,
            }
        }
        let pool = Pool { shared, threads, topology: topology.clone() };
        match failed {
            Some(why) => {
                drop(pool);
                Err(why)
            }
            None => Ok(pool),
        }
    }

    pub fn topology(&self) -> &Topology {
        &self.topology
    }

    pub fn workers(&self) -> usize {
        self.shared.slots.len()
    }

    /// This thread's index among this pool's workers, if it is one of them: for tables kept
    /// by worker, read and written without a lock. A job pumped while another waits runs on
    /// the same worker, so such a table's entry must not be held across a wait.
    pub fn worker(&self) -> Option<u32> {
        with_local(|local| {
            Arc::ptr_eq(&local.shared, &self.shared).then(|| u32::try_from(local.index).ok()).flatten()
        })
        .flatten()
    }

    /// Runs `run` on a worker near `group` (a group this pool does not have is no group), then
    /// `done` on the same thread with what came of it: its value, or its panic, caught. The
    /// pool goes on after a panic; a panic of `done` is kept and raised when the pool is
    /// dropped. It goes behind what is queued for its group.
    pub fn submit<R: 'static>(
        &self,
        near: Option<u32>,
        run: impl FnOnce() -> R + Send + 'static,
        done: impl FnOnce(thread::Result<R>) + Send + 'static,
    ) {
        self.shared.push(near, Job::boxed(move || done(catch_unwind(AssertUnwindSafe(run)))));
    }

    /// As [`Pool::submit`], but called on one of this pool's workers the job runs next on it
    /// (its LIFO slot), hot in its cache; elsewhere it is [`Pool::submit`] near no group.
    pub fn submit_next<R: 'static>(
        &self,
        run: impl FnOnce() -> R + Send + 'static,
        done: impl FnOnce(thread::Result<R>) + Send + 'static,
    ) {
        let job = Job::boxed(move || done(catch_unwind(AssertUnwindSafe(run))));
        if let Some(job) = push_next(&self.shared, None, job) {
            self.shared.push(None, job);
        }
    }

    /// As [`Pool::submit`], for a job on its request's critical path: on a pool of fast and
    /// slow groups it goes to `near` if that group is fast, else to the least loaded fast
    /// group. Where every group is fast it is placed as [`Pool::submit`] places it.
    pub fn submit_critical<R: 'static>(
        &self,
        near: Option<u32>,
        run: impl FnOnce() -> R + Send + 'static,
        done: impl FnOnce(thread::Result<R>) + Send + 'static,
    ) {
        #[cfg(test)]
        self.shared.critical.fetch_add(1, Ordering::Relaxed);
        let job = Job::boxed(move || done(catch_unwind(AssertUnwindSafe(run))));
        let group = self.shared.place(near, true);
        self.shared.push_to(group, job);
    }

    /// How many jobs were given by [`Pool::submit_critical`].
    #[cfg(test)]
    pub(crate) fn critical_submitted(&self) -> usize {
        self.shared.critical.load(Ordering::Relaxed)
    }

    /// `job` behind what is queued near `near`, as [`Pool::submit`] puts a job.
    pub(crate) fn push(&self, near: Option<u32>, job: Job) {
        self.shared.push(near, job);
    }

    /// Runs `f`, which may submit jobs that borrow what outlives this call, and returns once
    /// every job it submitted has run. On a worker of a pool the wait pumps (as [`crate::pump`]
    /// says); elsewhere it parks.
    pub fn scope<'env, R>(&self, f: impl FnOnce(&Scope<'env>) -> R) -> R {
        let pending = Pending {
            count: AtomicUsize::new(1),
            finished: AtomicBool::new(false),
            owner: thread::current(),
        };
        let scope = Scope { shared: Arc::as_ptr(&self.shared), pending: &pending, _env: PhantomData };
        let result = catch_unwind(AssertUnwindSafe(|| f(&scope)));
        if pending.count.fetch_sub(1, Ordering::AcqRel) == 1 {
            pending.finished.store(true, Ordering::Release);
        }
        let mut idle = 0;
        while !pending.finished.load(Ordering::Acquire) {
            let pump = depth() < PUMP_DEPTH;
            if pump && idle % 16 == 0 && run_one() {
                idle = 0;
                continue;
            }
            if idle < SPINS {
                idle += 1;
                std::hint::spin_loop();
                continue;
            }
            park(&|| pending.finished.load(Ordering::Acquire), pump);
        }
        result.unwrap_or_else(|panic| resume_unwind(panic))
    }
}

impl Drop for Pool {
    /// Stops the workers once they find nothing more to run, and joins them.
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        for slot in &self.shared.slots {
            if let Some(thread) = slot.thread.get() {
                thread.unpark();
            }
        }
        let me = thread::current().id();
        for handle in self.threads.drain(..) {
            // A pool dropped by a job of its own cannot wait for the worker that runs the job.
            if handle.thread().id() != me {
                _ = handle.join();
            }
        }
        if let Some(panic) = self.shared.stray.lock().take()
            && !thread::panicking()
        {
            resume_unwind(panic);
        }
    }
}

/// Jobs that borrow what lives for `'env`, of [`Pool::scope`].
pub struct Scope<'env> {
    shared: *const Shared,
    pending: *const Pending,
    _env: PhantomData<&'env mut &'env ()>,
}

// SAFETY: a scope only pushes jobs (lock-free, from any thread) and counts them in `Pending`'s
// atomics; the pool and `Pending` outlive it, as `Pool::scope` keeps them.
unsafe impl Send for Scope<'_> {}
// SAFETY: as above.
unsafe impl Sync for Scope<'_> {}

/// A scope's count, on the stack of its call: 128-byte aligned, since every job of it writes
/// it once.
#[repr(align(128))]
struct Pending {
    /// Jobs submitted and not done, and one for the call while it runs `f`.
    count: AtomicUsize,
    /// Set by whoever brought `count` to 0, as the last thing it does with `Pending`: the call
    /// returns only once it is.
    finished: AtomicBool,
    owner: Thread,
}

/// Counts a job of a scope as run, once everything it borrowed is no longer used.
struct Left(*const Pending);

// SAFETY: `Pending` is atomics and a `Thread`, shared by design.
unsafe impl Send for Left {}

impl Drop for Left {
    fn drop(&mut self) {
        // SAFETY: the scope's call waits for `finished`, which only the last of its jobs sets,
        // here, and does nothing with `Pending` after.
        let pending = unsafe { &*self.0 };
        if pending.count.fetch_sub(1, Ordering::AcqRel) == 1 {
            let owner = pending.owner.clone();
            pending.finished.store(true, Ordering::Release);
            owner.unpark();
        }
    }
}

impl<'env> Scope<'env> {
    /// As [`Pool::submit`], for a job that borrows what lives for `'env`. Submitted on one of
    /// the pool's workers near its own group (or none), it runs next on that worker (its LIFO
    /// slot), unless the worker's other workers take it first.
    pub fn submit<R: 'env>(
        &self,
        near: Option<u32>,
        run: impl FnOnce() -> R + Send + 'env,
        done: impl FnOnce(thread::Result<R>) + Send + 'env,
    ) {
        // SAFETY: `Pool::scope` keeps `Pending` until every job counted in it is done.
        unsafe { &*self.pending }.count.fetch_add(1, Ordering::Relaxed);
        let left = Left(self.pending);
        let job = move || {
            let _left = left;
            done(catch_unwind(AssertUnwindSafe(run)));
        };
        // SAFETY: the job borrows only what lives for `'env`, which outlives the call of
        // `Pool::scope` this scope is of. That call returns (or unwinds) only once `finished`
        // is set, which `Left` does after `run` and `done` were consumed, so nothing of `'env`
        // is used after it. The pool runs every job it was given before its workers stop, and
        // it outlives the call, which borrows it; a job never run is never counted out, and
        // the call waits rather than return.
        let job = unsafe { Job::erase(job) };
        // SAFETY: the pool outlives the call of `Pool::scope`, which borrows it.
        let shared = unsafe { &*self.shared };
        if let Some(job) = push_next(shared, near, job) {
            shared.push(near, job);
        }
    }
}

/// Gives `job` to this thread's LIFO slot when it is a worker of `shared`'s pool and `near`
/// is its group or none; otherwise gives it back.
fn push_next(shared: &Shared, near: Option<u32>, job: Job) -> Option<Job> {
    let at = LOCAL.get();
    // SAFETY: as in `with_local`.
    match unsafe { at.as_ref() } {
        Some(local)
            if ptr::eq(Arc::as_ptr(&local.shared), shared)
                && near.is_none_or(|it| it as usize == local.group) =>
        {
            local.push_next(job);
            None
        }
        _ => Some(job),
    }
}

/// The job in a LIFO slot, taken; nothing written when it is empty.
fn take(next: &AtomicPtr<Header>) -> Option<Job> {
    if next.load(Ordering::Relaxed).is_null() {
        return None;
    }
    NonNull::new(next.swap(ptr::null_mut(), Ordering::Acquire)).map(Job)
}

impl Shared {
    fn push(&self, near: Option<u32>, job: Job) {
        self.push_to(self.place(near, false), job);
    }

    fn push_to(&self, group: usize, job: Job) {
        self.groups[group].injector.push(job);
        self.announce(group);
    }

    /// The group a job near `near` (a group this pool does not have is none) is queued on: a
    /// critical one, on a pool of fast and slow groups, only on a fast group.
    fn place(&self, near: Option<u32>, critical: bool) -> usize {
        let only_fast = critical && self.mixed;
        near.map(|it| it as usize)
            .filter(|&it| it < self.groups.len() && (!only_fast || self.groups[it].fast))
            .unwrap_or_else(|| self.least_loaded(only_fast))
    }

    /// After a job is queued: wakes a parked worker, of `group` first, when none searches. A
    /// worker about to park says so, then looks for work once more; one of the two sees the
    /// other.
    fn announce(&self, group: usize) {
        fence(Ordering::SeqCst);
        let word = self.idle.0.load(Ordering::SeqCst);
        if searching(word) == 0 && sleeping(word) > 0 {
            self.wake(group);
        }
    }

    /// The group with the least queued and awake per worker (of the fast ones alone when
    /// `only_fast`), the fast ones first on a tie: read from what idling and queueing already
    /// keep, so a job adds no count of its own.
    fn least_loaded(&self, only_fast: bool) -> usize {
        if self.groups.len() == 1 {
            return 0;
        }
        least(self.groups.len(), only_fast, |at| {
            let group = &self.groups[at];
            let workers = group.workers.len() as u64;
            let asleep = u64::from(group.sleepers.load(Ordering::Relaxed)).min(workers);
            (group.injector.len() as u64 + workers - asleep, workers, group.fast)
        })
    }

    /// Wakes one parked worker, of `group` first, then of the groups that steal from it first,
    /// as a searcher: the next job queued wakes no other while it looks.
    fn wake(&self, group: usize) {
        for &at in self.groups[group].order.iter() {
            let group = &self.groups[at as usize];
            if group.sleepers.load(Ordering::SeqCst) == 0 {
                continue;
            }
            for &worker in group.workers.iter() {
                let slot = &self.slots[worker as usize];
                if slot.sleeping.load(Ordering::Relaxed)
                    && slot
                        .sleeping
                        .compare_exchange(true, false, Ordering::SeqCst, Ordering::Relaxed)
                        .is_ok()
                {
                    group.sleepers.fetch_sub(1, Ordering::SeqCst);
                    self.idle.0.fetch_add(SEARCHING.wrapping_sub(SLEEPING), Ordering::SeqCst);
                    if let Some(thread) = slot.thread.get() {
                        thread.unpark();
                    }
                    return;
                }
            }
        }
    }

    fn any_work(&self) -> bool {
        self.groups.iter().any(|it| !it.injector.is_empty())
            || self
                .slots
                .iter()
                .any(|it| !it.stealer.is_empty() || !it.next.load(Ordering::SeqCst).is_null())
    }
}

/// Of `count` groups, where `of(at)` is a group's load, its workers and whether it is fast:
/// the one of least load per worker, the fast ones first on a tie, then the lower index; of
/// the fast ones alone when `only_fast` and there is one. Compared without division.
fn least(count: usize, only_fast: bool, of: impl Fn(usize) -> (u64, u64, bool)) -> usize {
    let mut best: Option<(usize, (u64, u64, bool))> = None;
    for at in 0..count {
        let this = of(at);
        if only_fast && !this.2 {
            continue;
        }
        let better = best.is_none_or(|(_, (load, workers, fast))| {
            let (mine, theirs) =
                (u128::from(this.0) * u128::from(workers), u128::from(load) * u128::from(this.1));
            mine < theirs || (mine == theirs && this.2 && !fast)
        });
        if better {
            best = Some((at, this));
        }
    }
    best.map_or(0, |(at, _)| at)
}

impl Local {
    fn push_next(&self, job: Job) {
        let slot = &self.shared.slots[self.index];
        if let Some(old) = NonNull::new(slot.next.swap(job.0.as_ptr(), Ordering::AcqRel)) {
            self.deque.push(Job(old));
        }
        self.shared.announce(self.group);
    }

    /// The next job of this worker's own: its LIFO slot's, but not more than [`LIFO_RUNS`] in a
    /// row, then its deque's oldest.
    fn next(&self) -> Option<Job> {
        let slot = &self.shared.slots[self.index].next;
        if self.lifo.get() < LIFO_RUNS {
            if let Some(job) = take(slot) {
                self.lifo.set(self.lifo.get() + 1);
                return Some(job);
            }
        } else if let Some(job) = take(slot) {
            self.deque.push(job);
        }
        self.lifo.set(0);
        self.deque.pop()
    }

    fn random(&self) -> u32 {
        let mut x = self.seed.get();
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.seed.set(x);
        x
    }

    /// One look at every queue but this worker's own, nearest first: each group's injector (a
    /// few jobs), its workers' deques (half of one), then their LIFO slots.
    fn steal(&self) -> Option<Job> {
        let shared = &*self.shared;
        loop {
            let mut retry = false;
            for &at in shared.groups[self.group].order.iter() {
                let group = &shared.groups[at as usize];
                match group.injector.steal_batch_with_limit_and_pop(&self.deque, BATCH) {
                    Steal::Success(job) => return Some(job),
                    Steal::Retry => retry = true,
                    Steal::Empty => {}
                }
                let count = group.workers.len();
                let start = self.random() as usize % count;
                for step in 0..count {
                    let worker = group.workers[(start + step) % count] as usize;
                    if worker == self.index {
                        continue;
                    }
                    match shared.slots[worker].stealer.steal_batch_and_pop(&self.deque) {
                        Steal::Success(job) => return Some(job),
                        Steal::Retry => retry = true,
                        Steal::Empty => {}
                    }
                }
                for &worker in group.workers.iter() {
                    if worker as usize != self.index
                        && let Some(job) = take(&shared.slots[worker as usize].next)
                    {
                        return Some(job);
                    }
                }
            }
            if !retry {
                return None;
            }
        }
    }

    /// Whether this worker may search: while fewer than half the workers do.
    fn begin_search(&self) -> bool {
        let idle = &self.shared.idle.0;
        if 2 * searching(idle.load(Ordering::Relaxed)) >= self.shared.slots.len() as u64 {
            return false;
        }
        idle.fetch_add(SEARCHING, Ordering::SeqCst);
        true
    }

    /// Stops searching; the last searcher, having found work, wakes another for the rest.
    fn end_search(&self, found: bool) {
        let before = self.shared.idle.0.fetch_sub(SEARCHING, Ordering::SeqCst);
        if found && searching(before) == 1 {
            self.shared.announce(self.group);
        }
    }

    /// Searches, as a searcher, up to `rounds` times; the search ends either way.
    fn search(&self, rounds: u32) -> Option<Job> {
        for round in 0..rounds {
            if let Some(job) = self.steal() {
                self.end_search(true);
                return Some(job);
            }
            if round + 1 < rounds {
                for _ in 0..32 {
                    std::hint::spin_loop();
                }
            }
        }
        self.end_search(false);
        None
    }

    fn run(&self, job: Job) {
        // `job` catches its own panic and gives it to `done`; only `done`'s comes here.
        if let Err(panic) = catch_unwind(AssertUnwindSafe(|| run_task(|| job.run()))) {
            self.shared.stray.lock().get_or_insert(panic);
        }
    }

    /// Parks until woken, unless `ended` or there is work: a job queued after this says it
    /// sleeps wakes it. Whether it was woken to search, as its waker counted it.
    fn sleep(&self, ended: &dyn Fn() -> bool) -> bool {
        let shared = &*self.shared;
        let (slot, group) = (&shared.slots[self.index], &shared.groups[self.group]);
        slot.sleeping.store(true, Ordering::SeqCst);
        group.sleepers.fetch_add(1, Ordering::SeqCst);
        shared.idle.0.fetch_add(SLEEPING, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        if !ended() && !shared.any_work() {
            thread::park();
        }
        if slot.sleeping.compare_exchange(true, false, Ordering::SeqCst, Ordering::Relaxed).is_ok() {
            group.sleepers.fetch_sub(1, Ordering::SeqCst);
            shared.idle.0.fetch_sub(SLEEPING, Ordering::SeqCst);
            false
        } else {
            true
        }
    }
}

fn work(local: &Local) {
    LOCAL.set(local);
    let shared = &*local.shared;
    let mut searching = false;
    loop {
        if let Some(job) = local.next() {
            if searching {
                local.end_search(true);
                searching = false;
            }
            local.run(job);
            continue;
        }
        if searching || local.begin_search() {
            searching = false;
            if let Some(job) = local.search(ROUNDS) {
                local.run(job);
                continue;
            }
        }
        if shared.stop.load(Ordering::SeqCst) {
            // Every job given before the stop is run: what is left was found by none.
            if !shared.any_work() {
                break;
            }
            std::hint::spin_loop();
            continue;
        }
        searching = local.sleep(&|| shared.stop.load(Ordering::SeqCst));
    }
    LOCAL.set(ptr::null());
}

/// Runs one job of this thread's pool, if this thread is a worker and one is there to run:
/// its own, or one found by a single search when fewer than half the workers search.
pub(crate) fn run_one() -> bool {
    with_local(|local| {
        let job = local.next().or_else(|| if local.begin_search() { local.search(1) } else { None });
        job.map(|job| local.run(job)).is_some()
    })
    .unwrap_or(false)
}

/// Parks this thread until it is unparked, unless `ended` says not to. A worker that
/// `take_jobs` is woken by a job as a parked worker is, and runs what it finds.
pub(crate) fn park(ended: &dyn Fn() -> bool, take_jobs: bool) {
    let slept = take_jobs
        && with_local(|local| {
            if local.sleep(ended)
                && let Some(job) = local.search(1)
            {
                local.run(job);
            }
        })
        .is_some();
    if !slept && !ended() {
        thread::park();
    }
}

#[cfg(target_os = "linux")]
fn bind_to(cpu: &Cpu) -> io::Result<()> {
    crate::topology::linux::bind(cpu.id)
}

#[cfg(target_os = "macos")]
fn bind_to(cpu: &Cpu) -> io::Result<()> {
    let class = if cpu.fast {
        libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE
    } else {
        libc::qos_class_t::QOS_CLASS_UTILITY
    };
    // SAFETY: sets the calling thread's own QoS class; no memory is passed.
    let got = unsafe { libc::pthread_set_qos_class_self_np(class, 0) };
    if got == 0 {
        Ok(())
    } else {
        let why = io::Error::from_raw_os_error(got);
        Err(io::Error::new(why.kind(), format!("the QoS class of the worker of CPU {}: {why}", cpu.id)))
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn bind_to(cpu: &Cpu) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        format!("binding the worker of CPU {} to it: not on this OS", cpu.id),
    ))
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc, Mutex,
            atomic::{AtomicU32, AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };

    use crossbeam_channel::{bounded, unbounded};

    use super::{Group, Job, Pending, Pool, Slot, group, searching, sleeping};
    use crate::{
        pump::PUMP_DEPTH,
        topology::{Cpu, Topology},
    };

    const LONG: Duration = Duration::from_secs(60);

    fn groups_of(count: u32, each: u32) -> Topology {
        Topology {
            cpus: (0..count * each)
                .map(|id| Cpu { id, group: id / each, socket: 0, node: 0, fast: true })
                .collect(),
        }
    }

    fn until(what: &str, done: impl Fn() -> bool) {
        let until = Instant::now() + LONG;
        while !done() {
            assert!(Instant::now() < until, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn per_worker_state_is_padded_and_a_job_is_a_pointer() {
        assert_eq!(align_of::<Slot>(), 128);
        assert_eq!(align_of::<Group>(), 128);
        assert_eq!(align_of::<Pending>(), 128);
        assert_eq!(size_of::<Job>(), 8);
    }

    #[test]
    fn every_job_runs_once_and_says_its_group() {
        let pool = Pool::new(&groups_of(3, 2), false).unwrap();
        assert_eq!(pool.workers(), 6);
        assert_eq!(group(), None);
        let (tx, rx) = unbounded::<(usize, Option<u32>)>();
        for job in 0..1000usize {
            let tx = tx.clone();
            let near = (job % 4 != 3).then_some((job % 4) as u32);
            pool.submit(near, group, move |it| _ = tx.send((job, it.unwrap())));
        }
        let mut seen = vec![0u32; 1000];
        for _ in 0..1000 {
            let (job, group) = rx.recv_timeout(LONG).unwrap();
            seen[job] += 1;
            assert!(group.is_some_and(|it| it < 3), "{group:?}");
        }
        assert!(seen.iter().all(|&it| it == 1));
    }

    /// Group 0's one worker is held; a job queued near it is stolen by group 1's.
    #[test]
    fn an_idle_group_steals_from_a_busy_one() {
        let pool = Pool::new(&groups_of(2, 1), false).unwrap();
        let (hold_tx, hold_rx) = bounded::<()>(0);
        let (held_tx, held_rx) = bounded::<Option<u32>>(1);
        pool.submit(
            Some(0),
            move || {
                _ = held_tx.send(group());
                hold_rx.recv_timeout(LONG).unwrap();
            },
            |it| it.unwrap(),
        );
        let first = held_rx.recv_timeout(LONG).unwrap();
        let (tx, rx) = bounded::<Option<u32>>(1);
        let busy = first.unwrap();
        pool.submit(Some(busy), group, move |it| _ = tx.send(it.unwrap()));
        assert_eq!(rx.recv_timeout(LONG).unwrap(), Some(1 - busy));
        hold_tx.send(()).unwrap();
    }

    #[test]
    fn a_panic_is_the_jobs_failure_and_the_pool_goes_on() {
        let pool = Pool::new(&Topology::flat(2), false).unwrap();
        let (tx, rx) = unbounded::<Result<u32, String>>();
        for job in 0..10u32 {
            let tx = tx.clone();
            pool.submit(
                None,
                move || {
                    assert!(job % 3 != 0, "job {job}");
                    job
                },
                move |it| {
                    let it = it.map_err(|panic| {
                        panic.downcast_ref::<String>().cloned().unwrap_or_default()
                    });
                    _ = tx.send(it);
                },
            );
        }
        let mut failed: Vec<String> =
            (0..10).filter_map(|_| rx.recv_timeout(LONG).unwrap().err()).collect();
        failed.sort();
        assert_eq!(failed, ["job 0", "job 3", "job 6", "job 9"]);
    }

    #[test]
    fn dropping_the_pool_runs_what_it_was_given_and_joins() {
        let ran = Arc::new(AtomicUsize::new(0));
        let pool = Pool::new(&Topology::flat(3), false).unwrap();
        for _ in 0..200 {
            let ran = ran.clone();
            pool.submit(
                None,
                move || {
                    std::thread::sleep(Duration::from_micros(50));
                    ran.fetch_add(1, Ordering::SeqCst);
                },
                |it| it.unwrap(),
            );
        }
        drop(pool);
        assert_eq!(ran.load(Ordering::SeqCst), 200);
    }

    #[test]
    fn a_scope_borrows_and_waits_for_its_jobs() {
        let pool = Pool::new(&groups_of(2, 2), false).unwrap();
        let sums = Mutex::new(Vec::new());
        let numbers: Vec<u64> = (0..100).collect();
        let total = pool.scope(|scope| {
            for chunk in numbers.chunks(10) {
                scope.submit(None, move || chunk.iter().sum::<u64>(), |it| {
                    sums.lock().unwrap().push(it.unwrap());
                });
            }
            7
        });
        assert_eq!(total, 7);
        let sums = sums.into_inner().unwrap();
        assert_eq!(sums.len(), 10);
        assert_eq!(sums.iter().sum::<u64>(), 4950);
        assert_eq!(pool.scope(|_| 3), 3, "a scope of no job returns at once");
    }

    /// A scope waited on inside a job of a one-worker pool pumps its own jobs, which went to
    /// that worker's LIFO slot and deque.
    #[test]
    fn a_scope_inside_a_job_pumps() {
        let pool = Arc::new(Pool::new(&Topology::flat(1), false).unwrap());
        let (tx, rx) = bounded::<u64>(1);
        let inner = pool.clone();
        pool.submit(
            None,
            move || {
                let count = AtomicUsize::new(0);
                inner.scope(|scope| {
                    for _ in 0..5 {
                        scope.submit(None, || count.fetch_add(1, Ordering::SeqCst), |it| {
                            it.unwrap();
                        });
                    }
                });
                count.load(Ordering::SeqCst) as u64
            },
            move |it| _ = tx.send(it.unwrap()),
        );
        assert_eq!(rx.recv_timeout(LONG).unwrap(), 5);
    }

    /// A scope's job given to a worker's LIFO slot is taken by another worker while the first
    /// blocks without pumping: the slot is stealable.
    #[test]
    fn a_job_in_a_blocked_workers_slot_is_stolen() {
        let pool = Arc::new(Pool::new(&Topology::flat(2), false).unwrap());
        let (tx, rx) = bounded::<Option<u32>>(1);
        let inner = pool.clone();
        pool.submit(
            None,
            move || {
                let mine = inner.worker();
                let (ran_tx, ran_rx) = bounded::<Option<u32>>(1);
                let asked = inner.clone();
                inner.scope(|scope| {
                    scope.submit(None, move || asked.worker(), move |it| _ = ran_tx.send(it.unwrap()));
                    // Blocks this worker without pumping: only the other can run the job.
                    let ran = ran_rx.recv_timeout(LONG).expect("the other worker took it");
                    assert_ne!(ran, mine);
                    ran
                })
            },
            move |it| _ = tx.send(it.unwrap()),
        );
        assert!(rx.recv_timeout(LONG).unwrap().is_some());
    }

    /// One worker: a job gives itself `a` then `b` to run next, so `a` goes to the deque; `b`
    /// starts a chain of jobs each giving itself the next. The chain runs from the LIFO slot,
    /// but `a` runs after at most three of it.
    #[test]
    fn the_lifo_slot_runs_the_job_just_given_but_never_starves_the_deque() {
        fn chain(pool: Arc<Pool>, log: Arc<Mutex<Vec<String>>>, n: u32) {
            log.lock().unwrap().push(format!("b{n}"));
            if n < 20 {
                let next = pool.clone();
                pool.submit_next(move || chain(next, log, n + 1), |it| it.unwrap());
            }
        }
        let pool = Arc::new(Pool::new(&Topology::flat(1), false).unwrap());
        let log = Arc::new(Mutex::new(Vec::new()));
        {
            let (inner, log) = (pool.clone(), log.clone());
            pool.submit(
                None,
                move || {
                    let a = log.clone();
                    inner.submit_next(move || a.lock().unwrap().push("a".to_owned()), |it| it.unwrap());
                    let next = inner.clone();
                    inner.submit_next(move || chain(next, log, 0), |it| it.unwrap());
                },
                |it| it.unwrap(),
            );
        }
        {
            let log = log.clone();
            until("the chain to end", || log.lock().unwrap().len() == 22);
        }
        let log = log.lock().unwrap().clone();
        assert_eq!(&log[..3], ["b0", "b1", "b2"], "the job just given runs next: {log:?}");
        assert_eq!(log[3], "a", "after three from the slot, the deque's job: {log:?}");
        assert_eq!(log.iter().filter(|it| it.starts_with('b')).count(), 21);
    }

    /// Off the pool, `submit_next` is `submit`; and each worker says its own index.
    #[test]
    fn workers_say_their_index_and_submit_next_works_off_the_pool() {
        let pool = Arc::new(Pool::new(&groups_of(2, 2), false).unwrap());
        assert_eq!(pool.worker(), None);
        let other = Arc::new(Pool::new(&Topology::flat(1), false).unwrap());
        let (tx, rx) = unbounded::<(Option<u32>, Option<u32>)>();
        for _ in 0..64 {
            let (tx, mine, theirs) = (tx.clone(), pool.clone(), other.clone());
            pool.submit_next(move || (mine.worker(), theirs.worker()), move |it| _ = tx.send(it.unwrap()));
        }
        for _ in 0..64 {
            let (mine, theirs) = rx.recv_timeout(LONG).unwrap();
            assert!(mine.is_some_and(|it| it < 4), "{mine:?}");
            assert_eq!(theirs, None, "a worker of one pool is none of another's");
        }
    }

    /// Once the work is done, every worker parks and none is left counted as searching.
    #[test]
    fn idle_workers_park_and_leave_no_searcher() {
        let pool = Pool::new(&groups_of(2, 3), false).unwrap();
        let (tx, rx) = unbounded::<()>();
        for job in 0..500u32 {
            let tx = tx.clone();
            pool.submit((job % 3 != 0).then_some(job % 2), || std::hint::spin_loop(), move |_| {
                _ = tx.send(());
            });
        }
        for _ in 0..500 {
            rx.recv_timeout(LONG).unwrap();
        }
        until("every worker to park", || {
            let word = pool.shared.idle.0.load(Ordering::SeqCst);
            searching(word) == 0 && sleeping(word) == 6
        });
        let asleep: u32 = pool.shared.groups.iter().map(|it| it.sleepers.load(Ordering::SeqCst)).sum();
        assert_eq!(asleep, 6);
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

    #[derive(Clone, Copy)]
    enum How {
        Submit(Option<u32>),
        Next,
        Scope,
    }

    struct Tree {
        children: Vec<Vec<(How, usize)>>,
        ran: Vec<AtomicU32>,
        ended: AtomicUsize,
    }

    fn run_node(pool: &Arc<Pool>, tree: &Arc<Tree>, node: usize) {
        tree.ran[node].fetch_add(1, Ordering::SeqCst);
        let mut scoped = Vec::new();
        for &(how, child) in &tree.children[node] {
            let (pool2, tree2) = (pool.clone(), tree.clone());
            let job = move || run_node(&pool2, &tree2, child);
            match how {
                How::Submit(near) => pool.submit(near, job, |it| it.unwrap()),
                How::Next => pool.submit_next(job, |it| it.unwrap()),
                How::Scope => scoped.push(child),
            }
        }
        if !scoped.is_empty() {
            pool.scope(|scope| {
                for &child in &scoped {
                    scope.submit(None, move || run_node(pool, tree, child), |it| it.unwrap());
                }
            });
        }
        tree.ended.fetch_add(1, Ordering::SeqCst);
    }

    /// Random trees of jobs that give their children by `submit` (near a random group or
    /// none), `submit_next` and a scope, on random topologies: every node of the model's tree
    /// runs exactly once, and then every worker parks with no searcher left. Fewer than
    /// [`PUMP_DEPTH`] nodes wait on a scope, so no stack reaches the bound with work only it
    /// could run (a one-worker pool's scope at the bound waits for its own queue).
    #[test]
    fn a_seeded_stress_runs_every_job_of_the_model_once() {
        for seed in 0..10u64 {
            let mut rng = Rng(seed);
            let (count, each) = (1 + rng.below(3) as u32, 1 + rng.below(3) as u32);
            let mut children: Vec<Vec<(How, usize)>> = vec![Vec::new()];
            let mut scoping = 0;
            let mut at = 0;
            while at < children.len() && children.len() < 600 {
                let mut scopes = false;
                for _ in 0..rng.below(4) {
                    let mut how = match rng.below(4) {
                        0 => How::Submit(None),
                        1 => How::Submit(Some(rng.below(u64::from(count) + 1) as u32)),
                        2 => How::Next,
                        _ => How::Scope,
                    };
                    if matches!(how, How::Scope) && !scopes {
                        if scoping + 1 < PUMP_DEPTH {
                            (scoping, scopes) = (scoping + 1, true);
                        } else {
                            how = How::Next;
                        }
                    }
                    let child = children.len();
                    children.push(Vec::new());
                    children[at].push((how, child));
                }
                at += 1;
            }
            let nodes = children.len();
            let tree = Arc::new(Tree {
                children,
                ran: (0..nodes).map(|_| AtomicU32::new(0)).collect(),
                ended: AtomicUsize::new(0),
            });
            let pool = Arc::new(Pool::new(&groups_of(count, each), false).unwrap());
            {
                let (inner, tree) = (pool.clone(), tree.clone());
                pool.submit(None, move || run_node(&inner, &tree, 0), |it| it.unwrap());
            }
            until("the tree to run", || tree.ended.load(Ordering::SeqCst) == nodes);
            assert!(
                tree.ran.iter().all(|it| it.load(Ordering::SeqCst) == 1),
                "seed {seed}: a job ran other than once"
            );
            let workers = u64::from(count * each);
            until("every worker to park", || {
                let word = pool.shared.idle.0.load(Ordering::SeqCst);
                searching(word) == 0 && sleeping(word) == workers
            });
        }
    }

    /// `least` against the model: the group first in the order (load per worker, slow after
    /// fast, index), over seeded loads, of every group or the fast ones alone.
    #[test]
    fn the_least_loaded_group_is_the_models() {
        let mut rng = Rng(7);
        for _ in 0..2000 {
            let count = 1 + rng.below(6) as usize;
            let groups: Vec<(u64, u64, bool)> = (0..count)
                .map(|_| (rng.below(9), 1 + rng.below(4), rng.below(2) == 0))
                .collect();
            for only_fast in [false, true] {
                let model = (0..count)
                    .filter(|&at| !only_fast || groups[at].2)
                    .min_by(|&a, &b| {
                        let ((la, wa, fa), (lb, wb, fb)) = (groups[a], groups[b]);
                        (u128::from(la) * u128::from(wb))
                            .cmp(&(u128::from(lb) * u128::from(wa)))
                            .then(fb.cmp(&fa))
                            .then(a.cmp(&b))
                    })
                    .unwrap_or(0);
                assert_eq!(super::least(count, only_fast, |at| groups[at]), model, "{groups:?}");
            }
        }
    }

    /// Two fast groups of two and a slow group of four: a critical job goes to its `near`
    /// group only if that is fast, else to a fast group; any other job goes to `near`, and one
    /// near no group to a least loaded one (fast first on a tie, so group 0 when idle).
    #[test]
    fn a_critical_job_is_placed_on_a_fast_group() {
        let topology = Topology::from_perflevels(&[(4, 2), (4, 4)]);
        let pool = Pool::new(&topology, false).unwrap();
        let shared = &*pool.shared;
        assert!(shared.mixed);
        assert_eq!(shared.groups.iter().map(|it| it.fast).collect::<Vec<_>>(), [true, true, false]);
        until("every worker to park", || {
            sleeping(shared.idle.0.load(Ordering::SeqCst)) == 8
        });
        for near in [None, Some(0), Some(1), Some(2), Some(9)] {
            let at = shared.place(near, true);
            assert!(shared.groups[at].fast, "critical near {near:?} went to slow group {at}");
            if let Some(near @ (0 | 1)) = near {
                assert_eq!(at, near as usize);
            }
        }
        assert_eq!(shared.place(Some(2), false), 2);
        assert_eq!(shared.place(Some(1), false), 1);
        assert_eq!(shared.place(None, false), 0);
        assert_eq!(shared.place(Some(9), false), 0, "a group the pool lacks is none");
        let (tx, rx) = unbounded::<Option<u32>>();
        for _ in 0..50 {
            let tx = tx.clone();
            pool.submit_critical(Some(2), group, move |it| _ = tx.send(it.unwrap()));
        }
        for _ in 0..50 {
            assert!(rx.recv_timeout(LONG).unwrap().is_some_and(|it| it < 3));
        }
        assert_eq!(pool.critical_submitted(), 50);
    }

    /// Where every group is fast, a critical job is placed as any other.
    #[test]
    fn on_an_all_fast_machine_critical_changes_nothing() {
        let pool = Pool::new(&groups_of(3, 2), false).unwrap();
        let shared = &*pool.shared;
        assert!(!shared.mixed);
        until("every worker to park", || {
            sleeping(shared.idle.0.load(Ordering::SeqCst)) == 6
        });
        for near in [None, Some(0), Some(1), Some(2), Some(5)] {
            assert_eq!(shared.place(near, true), shared.place(near, false), "near {near:?}");
        }
    }

    /// Each worker names its CPU's node: on two nodes of a group each, a job's node is that of
    /// the group it ran on, wherever it was stolen to; off the pool there is none.
    #[test]
    fn a_worker_says_its_numa_node() {
        let cpus = (0..4).map(|id| Cpu { id, group: id / 2, socket: 0, node: id / 2, fast: true });
        let pool = Pool::new(&Topology { cpus: cpus.collect() }, false).unwrap();
        assert_eq!(super::node(), None);
        let (tx, rx) = unbounded::<(Option<u32>, Option<u32>)>();
        for job in 0..200u32 {
            let tx = tx.clone();
            let ran = || (group(), super::node());
            pool.submit(Some(job % 2), ran, move |it| _ = tx.send(it.unwrap()));
        }
        for _ in 0..200 {
            let (group, node) = rx.recv_timeout(LONG).unwrap();
            assert!(group.is_some());
            assert_eq!(node, group);
        }
    }

    #[test]
    fn no_cpu_is_refused() {
        assert!(Pool::new(&Topology { cpus: Vec::new() }, false).is_err());
        let gap = Topology { cpus: vec![Cpu { id: 0, group: 1, socket: 0, node: 0, fast: true }] };
        assert!(Pool::new(&gap, false).is_err());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn workers_bound_to_their_cpus_run_jobs() {
        let topology = Topology::detect();
        let pool = Pool::new(&topology, true).unwrap();
        let (tx, rx) = unbounded::<Option<u32>>();
        for _ in 0..pool.workers() {
            let tx = tx.clone();
            pool.submit(None, group, move |it| _ = tx.send(it.unwrap()));
        }
        for _ in 0..pool.workers() {
            assert!(rx.recv_timeout(LONG).unwrap().is_some());
        }
    }
}

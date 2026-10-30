//! The threads the plane's tasks run on (docs/v2/core.md section 4, docs/v2/plane.md "The
//! executor"): one worker per CPU of a [`Topology`], stealing work nearest first.
//!
//! Each worker has a deque of its own and each group (the CPUs sharing a last-level cache or a
//! cluster) an injector. A job submitted near a group goes to that group's injector; one near
//! no group goes to the least loaded group, the fast ones first on a tie (Apple's performance
//! cores, where the plane's first ready nodes, its critical path, go). An idle worker takes from
//! its own deque, then its group's injector, then its group's workers, then each group of its
//! [`Topology::steal_order`] the same way; after a short bound of looking again it parks, and
//! each job wakes one parked worker, of its group first. Pushing, popping and stealing take no
//! lock (crossbeam's deques).
//!
//! Bound to its CPU, a worker is pinned to it on Linux (`sched_setaffinity`); on macOS, which
//! pins nothing, it takes the QoS class of its kind of core: user-interactive on fast cores,
//! utility on the others.
//!
//! Every job runs as a task of [`crate::pump`]'s stack, so a wait inside it pumps this pool.

use std::{
    any::Any,
    cell::RefCell,
    io,
    marker::PhantomData,
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    rc::Rc,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicUsize, Ordering, fence},
    },
    thread::{self, JoinHandle, Thread},
};

use crossbeam_deque::{Injector, Steal, Stealer, Worker};

use crate::{
    pump::{PUMP_DEPTH, SPINS, depth, lock, run_task},
    topology::{Cpu, Topology},
};

/// A worker's stack: room for the jobs it pumps above the one that waits.
const STACK: usize = 64 << 20;
/// How many jobs a worker takes from an injector at once.
const BATCH: usize = 4;

struct Job {
    /// The group it was submitted to.
    group: usize,
    run: Box<dyn FnOnce() + Send>,
}

struct Group {
    injector: Injector<Job>,
    workers: Vec<usize>,
    order: Vec<usize>,
    fast: bool,
    /// Its workers parked or about to park.
    sleepers: AtomicUsize,
    /// Jobs submitted to it not yet started.
    queued: AtomicUsize,
    /// Its workers running a job.
    busy: AtomicUsize,
}

struct Slot {
    stealer: Stealer<Job>,
    group: usize,
    sleeping: AtomicBool,
    thread: OnceLock<Thread>,
}

struct Shared {
    groups: Vec<Group>,
    slots: Vec<Slot>,
    stop: AtomicBool,
    /// The first panic of a job's `done`, raised when the pool is dropped.
    stray: Mutex<Option<Box<dyn Any + Send>>>,
}

pub struct Pool {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
    topology: Topology,
}

/// A worker's own: kept by its thread.
struct Local {
    shared: Arc<Shared>,
    index: usize,
    group: usize,
    deque: Worker<Job>,
}

thread_local! {
    static LOCAL: RefCell<Option<Rc<Local>>> = const { RefCell::new(None) };
}

/// The group of the worker this thread is, or `None` for a thread of no pool.
pub fn group() -> Option<u32> {
    LOCAL.with_borrow(|local| local.as_ref().and_then(|it| u32::try_from(it.group).ok()))
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
        let mut members: Vec<Vec<usize>> = vec![Vec::new(); count];
        for (index, cpu) in topology.cpus.iter().enumerate() {
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
                group: cpu.group as usize,
                sleeping: AtomicBool::new(false),
                thread: OnceLock::new(),
            })
            .collect();
        let groups = members
            .into_iter()
            .enumerate()
            .map(|(at, workers)| Group {
                injector: Injector::new(),
                fast: workers.iter().any(|&it| topology.cpus[it].fast),
                order: topology.steal_order(at as u32).into_iter().map(|it| it as usize).collect(),
                workers,
                sleepers: AtomicUsize::new(0),
                queued: AtomicUsize::new(0),
                busy: AtomicUsize::new(0),
            })
            .collect();
        let shared = Arc::new(Shared {
            groups,
            slots,
            stop: AtomicBool::new(false),
            stray: Mutex::new(None),
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
                        let group = shared.slots[index].group;
                        work(Rc::new(Local { shared, index, group, deque }));
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

    /// Runs `run` on a worker near `group` (a group this pool does not have is no group), then
    /// `done` on the same thread with what came of it: its value, or its panic, caught. The
    /// pool goes on after a panic; a panic of `done` is kept and raised when the pool is
    /// dropped.
    pub fn submit<R: 'static>(
        &self,
        near: Option<u32>,
        run: impl FnOnce() -> R + Send + 'static,
        done: impl FnOnce(thread::Result<R>) + Send + 'static,
    ) {
        self.shared.push(near, Box::new(move || done(catch_unwind(AssertUnwindSafe(run)))));
    }

    /// Runs `f`, which may submit jobs that borrow what outlives this call, and returns once
    /// every job it submitted has run. On a worker of a pool the wait pumps (as [`crate::pump`]
    /// says); elsewhere it parks.
    pub fn scope<'env, R>(&self, f: impl FnOnce(&Scope<'env>) -> R) -> R {
        let scope = Scope {
            shared: self.shared.clone(),
            pending: Arc::new(Pending { count: AtomicUsize::new(0), owner: thread::current() }),
            _env: PhantomData,
        };
        let result = catch_unwind(AssertUnwindSafe(|| f(&scope)));
        let pending = &scope.pending;
        let mut idle = 0;
        while pending.count.load(Ordering::SeqCst) != 0 {
            let pump = depth() < PUMP_DEPTH;
            if pump && run_one() {
                idle = 0;
                continue;
            }
            if idle < SPINS {
                idle += 1;
                std::hint::spin_loop();
                continue;
            }
            park(&|| pending.count.load(Ordering::SeqCst) == 0, pump);
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
        if let Some(panic) = lock(&self.shared.stray).take()
            && !thread::panicking()
        {
            resume_unwind(panic);
        }
    }
}

/// Jobs that borrow what lives for `'env`, of [`Pool::scope`].
pub struct Scope<'env> {
    shared: Arc<Shared>,
    pending: Arc<Pending>,
    _env: PhantomData<&'env mut &'env ()>,
}

struct Pending {
    count: AtomicUsize,
    owner: Thread,
}

/// Counts a job of a scope as run, once everything it borrowed is no longer used.
struct Left(Arc<Pending>);

impl Drop for Left {
    fn drop(&mut self) {
        if self.0.count.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.0.owner.unpark();
        }
    }
}

impl<'env> Scope<'env> {
    /// As [`Pool::submit`], for a job that borrows what lives for `'env`.
    pub fn submit<R: 'env>(
        &self,
        near: Option<u32>,
        run: impl FnOnce() -> R + Send + 'env,
        done: impl FnOnce(thread::Result<R>) + Send + 'env,
    ) {
        self.pending.count.fetch_add(1, Ordering::SeqCst);
        let left = Left(self.pending.clone());
        let job: Box<dyn FnOnce() + Send + 'env> = Box::new(move || {
            let _left = left;
            done(catch_unwind(AssertUnwindSafe(run)));
        });
        // SAFETY: the job borrows only what lives for `'env`, which outlives the call of
        // `Pool::scope` this scope is of. That call returns (or unwinds) only once `pending` is
        // 0, and `Left` counts the job out after `run` and `done` were consumed, so nothing of
        // `'env` is used after it. The pool runs every job it was given before its workers stop,
        // and it outlives the call, which borrows it; a job never run is never counted out, and
        // the call waits rather than return.
        let job: Box<dyn FnOnce() + Send + 'static> = unsafe {
            std::mem::transmute::<Box<dyn FnOnce() + Send + 'env>, Box<dyn FnOnce() + Send + 'static>>(
                job,
            )
        };
        self.shared.push(near, job);
    }
}

impl Shared {
    fn push(&self, near: Option<u32>, run: Box<dyn FnOnce() + Send>) {
        let group = near
            .map(|it| it as usize)
            .filter(|&it| it < self.groups.len())
            .unwrap_or_else(|| self.least_loaded());
        self.groups[group].queued.fetch_add(1, Ordering::Relaxed);
        self.groups[group].injector.push(Job { group, run });
        // Against a worker about to park: it says so, then looks for work once more.
        fence(Ordering::SeqCst);
        self.wake(group);
    }

    fn least_loaded(&self) -> usize {
        let load = |group: &Group| {
            group.queued.load(Ordering::Relaxed) + group.busy.load(Ordering::Relaxed)
        };
        let mut best = 0;
        for at in 1..self.groups.len() {
            let (a, b) = (&self.groups[at], &self.groups[best]);
            // Load per worker, compared without division.
            let (load_a, load_b) =
                (load(a) as u128 * b.workers.len() as u128, load(b) as u128 * a.workers.len() as u128);
            if load_a < load_b || (load_a == load_b && a.fast && !b.fast) {
                best = at;
            }
        }
        best
    }

    /// Wakes one parked worker, of `group` first, then of the groups that steal from it first.
    fn wake(&self, group: usize) {
        for &at in &self.groups[group].order {
            let group = &self.groups[at];
            if group.sleepers.load(Ordering::SeqCst) == 0 {
                continue;
            }
            for &worker in &group.workers {
                let slot = &self.slots[worker];
                if slot.sleeping.compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst).is_ok()
                {
                    group.sleepers.fetch_sub(1, Ordering::SeqCst);
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
            || self.slots.iter().any(|it| !it.stealer.is_empty())
    }
}

impl Local {
    fn find(&self) -> Option<Job> {
        if let Some(job) = self.deque.pop() {
            return Some(job);
        }
        let shared = &*self.shared;
        loop {
            let mut retry = false;
            for &at in &shared.groups[self.group].order {
                let group = &shared.groups[at];
                match group.injector.steal_batch_with_limit_and_pop(&self.deque, BATCH) {
                    Steal::Success(job) => return Some(job),
                    Steal::Retry => retry = true,
                    Steal::Empty => {}
                }
                for &worker in &group.workers {
                    if worker == self.index {
                        continue;
                    }
                    match shared.slots[worker].stealer.steal() {
                        Steal::Success(job) => return Some(job),
                        Steal::Retry => retry = true,
                        Steal::Empty => {}
                    }
                }
            }
            if !retry {
                return None;
            }
        }
    }

    fn run(&self, job: Job) {
        let shared = &*self.shared;
        shared.groups[job.group].queued.fetch_sub(1, Ordering::Relaxed);
        shared.groups[self.group].busy.fetch_add(1, Ordering::Relaxed);
        // `run` catches the job's own panic; only its `done`'s comes here.
        let ran = catch_unwind(AssertUnwindSafe(|| run_task(job.run)));
        shared.groups[self.group].busy.fetch_sub(1, Ordering::Relaxed);
        if let Err(panic) = ran {
            lock(&shared.stray).get_or_insert(panic);
        }
    }

    /// Parks until unparked, unless `ended` or there is work: a job pushed after this says it
    /// sleeps wakes it.
    fn sleep(&self, ended: &dyn Fn() -> bool) {
        let (slot, group) =
            (&self.shared.slots[self.index], &self.shared.groups[self.group]);
        slot.sleeping.store(true, Ordering::SeqCst);
        group.sleepers.fetch_add(1, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        if !ended() && !self.shared.any_work() && self.deque.is_empty() {
            thread::park();
        }
        if slot.sleeping.compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
            group.sleepers.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

fn work(local: Rc<Local>) {
    LOCAL.with_borrow_mut(|it| *it = Some(local.clone()));
    'next: loop {
        if let Some(job) = local.find() {
            local.run(job);
            continue;
        }
        for spin in 0..SPINS {
            if spin % 16 == 15 {
                thread::yield_now();
            } else {
                std::hint::spin_loop();
            }
            if let Some(job) = local.find() {
                local.run(job);
                continue 'next;
            }
        }
        if local.shared.stop.load(Ordering::SeqCst) {
            // Every job given before the stop is run: what is left was found by none.
            if !local.shared.any_work() && local.deque.is_empty() {
                break;
            }
            continue;
        }
        local.sleep(&|| local.shared.stop.load(Ordering::SeqCst));
    }
    LOCAL.with_borrow_mut(|it| *it = None);
}

/// Runs one job of this thread's pool, if this thread is a worker and one is there to run.
pub(crate) fn run_one() -> bool {
    let Some(local) = LOCAL.with_borrow(Clone::clone) else { return false };
    match local.find() {
        Some(job) => {
            local.run(job);
            true
        }
        None => false,
    }
}

/// Parks this thread until it is unparked, unless `ended` says not to. A worker that
/// `take_jobs` is woken by a job as a parked worker is.
pub(crate) fn park(ended: &dyn Fn() -> bool, take_jobs: bool) {
    match LOCAL.with_borrow(Clone::clone) {
        Some(local) if take_jobs => local.sleep(ended),
        _ => {
            if !ended() {
                thread::park();
            }
        }
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
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use crossbeam_channel::{bounded, unbounded};

    use super::{Pool, group};
    use crate::topology::{Cpu, Topology};

    const LONG: Duration = Duration::from_secs(60);

    fn groups_of(count: u32, each: u32) -> Topology {
        Topology {
            cpus: (0..count * each)
                .map(|id| Cpu { id, group: id / each, socket: 0, fast: true })
                .collect(),
        }
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
                scope.submit(None, || chunk.iter().sum::<u64>(), |it| {
                    sums.lock().unwrap().push(it.unwrap());
                });
            }
            7
        });
        assert_eq!(total, 7);
        let sums = sums.into_inner().unwrap();
        assert_eq!(sums.len(), 10);
        assert_eq!(sums.iter().sum::<u64>(), 4950);
    }

    /// A scope waited on inside a job of a one-worker pool pumps its own jobs.
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

    #[test]
    fn no_cpu_is_refused() {
        assert!(Pool::new(&Topology { cpus: Vec::new() }, false).is_err());
        let gap = Topology { cpus: vec![Cpu { id: 0, group: 1, socket: 0, fast: true }] };
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

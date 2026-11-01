//! The fibers driver of the scheduler (docs/v2/plan/lanes/sched.md, phase 2): one worker thread per
//! core of the shape, each running jobs on stackful fibers (corosensei). A job that waits switches
//! back to its worker, which tells the scheduler and runs whatever it decides next; the waiting job
//! holds no thread, and resumes, on whichever core the scheduler says, when what it waits for is
//! done.
//!
//! Every call into the scheduler is made on a worker's own stack, after the fiber has switched
//! out: a job is never made runnable (and so never resumed by another core) while it is still
//! on its old core's stack.
//!
//! Every driver of the process is a member of one [`World`] behind one lock: one wait graph
//! for all their jobs, so a cycle of waits through two drivers' jobs is refused as one of a
//! single driver is, and each driver's own placement. A job's [`JobId`] here (its [`Cx::job`],
//! [`Handle`], a [`Cycle`]'s) is its world id, unique among the live jobs of every driver. Each
//! transition is a few dozen instructions and allocates nothing warm.
//!
//! Code that has no [`Cx`] to hand (the query engine) reaches the running job through
//! [`wait`], [`miss`] and [`current_job`], which read this thread's fiber.
//!
//! A fiber moves between threads only while suspended. What its stack holds across a wait must
//! be `Send`: no thread-local borrow, no lock guard, no `Rc` (phase 2's audit). A [`NoWait`]
//! guard marks a region that must not wait; a wait inside one panics, naming it. Code that
//! reads a `thread_local!` must not span a wait inside one function: the compiler may keep the
//! thread-local's address across the call, and after the wait the job can be on another
//! thread. This module's own thread-local reads sit in `#[inline(never)]` functions for that
//! reason. A thread-local whose value a job keeps across a wait is [`Carried`] with the job:
//! taken off the thread when the job parks and put on the thread that resumes it.

use std::{
    any::Any,
    cell::Cell,
    fmt, io, mem,
    ops::{Deref, DerefMut},
    panic::{self, AssertUnwindSafe},
    ptr,
    sync::{
        Arc, LazyLock, Mutex, MutexGuard, Once, OnceLock,
        atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle, Thread},
    time::{Duration, Instant},
};

use corosensei::{
    Coroutine, CoroutineResult, Yielder,
    stack::{DefaultStack, Stack},
};

use super::{CoreId, CounterId, Cores, Cycle, JobId, Spawn, Stats, Suspend, Wait, World, Yielded};
use crate::Key;

/// What the driver runs: each job's payload, and how to run one.
pub trait Work: Sized + Send + Sync + 'static {
    type Job: Send + 'static;

    /// Runs `job` on a fiber; `cx` spawns, claims and waits for it.
    fn run(&self, cx: &Cx<'_, Self>, job: Self::Job);
}

/// The driver's sizes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Config {
    /// Fibers made at start; more are made (and kept, recycled) while more jobs are started and
    /// not done at once. Admission bounds that number.
    pub fibers: usize,
    /// Bytes of each fiber's stack, reserved (committed as touched), with a guard page below.
    pub stack: usize,
    /// Spins an idle worker makes before it parks.
    pub spin: u32,
}

impl Config {
    /// Four fibers per core, 16 MiB stacks (the front end's deepest recursion runs on them),
    /// and a few microseconds of spinning.
    pub fn for_cores(cores: usize) -> Config {
        Config { fibers: cores * 4, stack: 16 << 20, spin: 1 << 10 }
    }
}

/// A spawned job, for [`Cx::wait_job`]: its id is recycled once it ends, the generation tells
/// the old job from the new one.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Handle {
    pub job: JobId,
    generation: u32,
}

/// The driver could not start or go on.
#[derive(Debug)]
pub enum DriverError {
    /// A fiber's stack could not be mapped.
    Stack { bytes: usize, why: io::Error },
    /// A worker's setup (binding it to its core, its QoS class) failed.
    Setup { core: CoreId, why: io::Error },
    /// A worker thread could not be started.
    Thread { core: CoreId, why: io::Error },
}

impl fmt::Display for DriverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DriverError::Stack { bytes, why } => write!(f, "mapping a fiber stack of {bytes} bytes: {why}"),
            DriverError::Setup { core, why } => write!(f, "setting up the worker of core {}: {why}", core.0),
            DriverError::Thread { core, why } => write!(f, "starting the worker of core {}: {why}", core.0),
        }
    }
}

impl std::error::Error for DriverError {}

// dyn: a job's panic payload, carried to `Driver::join` and raised there; cold.
type Panic = Box<dyn Any + Send>;

/// What a worker passes into a fiber.
enum In<J> {
    Start(JobId, J),
    Go(Answer),
    Exit,
}

/// What a wait or move came to.
enum Answer {
    Ready,
    Cycle(Cycle),
}

/// What a fiber passes back to its worker.
enum Out {
    Wait(Want),
    Move,
    Done(Option<Panic>),
}

enum Want {
    On(Wait),
    Job(Handle),
}

/// A fiber: a coroutine that runs job after job, never ending until told to, and its number
/// ([`Switch::fiber`]).
struct Fiber<J>(Coroutine<In<J>, Out, (), DefaultStack>, u32);

/// Fibers made in the process: the next one's number.
static FIBERS: AtomicU32 = AtomicU32::new(0);

// SAFETY: a fiber is moved to another thread only while suspended (in the idle pool, or parked
// in its job's slot), never while it runs. Its stack then holds the driver's frames (an
// `Arc<Shared>`, the `Yielder` and `Cx`, which only the fiber itself touches, and a payload
// `J: Send`) and the job's frames below a wait, which phase 2's audit
// (docs/v2/plan/lanes/sched.md) holds to `Send`: no thread-local borrow, lock guard or `Rc`
// across a wait; `NoWait` guards are checked at every switch (`Cx::switch`).
#[allow(unsafe_code)]
unsafe impl<J: Send> Send for Fiber<J> {}

enum Slot<J> {
    Free,
    /// Spawned, not started: its payload.
    Fresh(J),
    /// Started; on a core now.
    Running,
    /// Started; suspended, or moved and not resumed yet, with the thread-locals it carries.
    Parked(Fiber<J>, Box<Taken>),
}

/// A driver's own: its jobs' slots, by local id, and its fibers.
struct State<W: Work> {
    slots: Vec<Slot<W::Job>>,
    free: Vec<u32>,
    /// Spawned and not done.
    live: u32,
    idle: Vec<Fiber<W::Job>>,
    /// Boxes for parked jobs' carried thread-locals, kept for the next job to park.
    spare_taken: Vec<Box<Taken>>,
    closing: bool,
    joiner: Option<Thread>,
    panic: Option<Panic>,
    failed: Option<DriverError>,
}

const NONE: u32 = u32::MAX;

/// One core's word: its move flag and its wakeup.
struct CoreSlot {
    /// Set by `Decision::Yield`, cleared at each `Run`: the job there reads it at a miss.
    moving: AtomicBool,
    /// A `Wake` (or the stop) since the worker last looked.
    woken: AtomicBool,
    thread: OnceLock<Thread>,
}

impl CoreSlot {
    fn wake(&self) {
        self.woken.store(true, Ordering::Release);
        if let Some(thread) = self.thread.get() {
            thread.unpark();
        }
    }
}

/// Every driver's scheduling, under one lock for the process: the [`World`] (one wait graph,
/// each driver's placement as a member) and each member's wakers. A job of one driver woken by
/// another's is woken through them. The per-core change (docs/v2/plan/lanes/sched.md) takes
/// spawns and ends off this lock.
struct Global {
    world: World,
    /// By member, as the world numbers them; `None` once the driver stopped.
    io: Vec<Option<Io>>,
    dirty: Vec<u32>,
}

struct Io {
    cores: Arc<[CoreSlot]>,
    /// A `Run` for a core (a local job id) made by another core's worker; read by its worker.
    pending: Vec<u32>,
}

static GLOBAL: LazyLock<Mutex<Global>> = LazyLock::new(|| {
    // alloc: the process's scheduler, once
    Mutex::new(Global { world: World::new(1024), io: Vec::with_capacity(8), dirty: Vec::with_capacity(8) })
});

/// The process's [`Global`] and a driver's [`State`], locked in that order, always, so no two
/// locks are ever taken the other way round.
struct Locked<'a, W: Work> {
    global: MutexGuard<'static, Global>,
    state: MutexGuard<'a, State<W>>,
    member: u32,
}

impl<W: Work> Locked<'_, W> {
    fn world(&mut self) -> &mut World {
        &mut self.global.world
    }

    /// This member's pending `Run` of `core`, taken.
    fn take_pending(&mut self, core: u32) -> Option<u32> {
        let io = self.global.io[self.member as usize].as_mut().expect("a running driver's wakers");
        let job = mem::replace(&mut io.pending[core as usize], NONE);
        (job != NONE).then_some(job)
    }
}

impl<W: Work> Deref for Locked<'_, W> {
    type Target = State<W>;

    fn deref(&self) -> &State<W> {
        &self.state
    }
}

impl<W: Work> DerefMut for Locked<'_, W> {
    fn deref_mut(&mut self) -> &mut State<W> {
        &mut self.state
    }
}

struct Shared<W: Work> {
    work: W,
    /// This driver's number in the [`World`].
    member: u32,
    state: Mutex<State<W>>,
    cores: Arc<[CoreSlot]>,
    config: Config,
    start: Instant,
    go: AtomicBool,
    stop: AtomicBool,
}

impl<W: Work> Shared<W> {
    fn lock(&self) -> Locked<'_, W> {
        let global = GLOBAL.lock().expect("the scheduler's lock: a call into the scheduler panicked");
        let state = self.state.lock().expect("a driver's lock: a call into the scheduler panicked");
        Locked { global, state, member: self.member }
    }

    fn now(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }

    /// Does what the scheduler decided, for every driver whose placement it changed; the
    /// `Run` of this driver's core `me`, if any, is returned (a local job id).
    fn dispatch(&self, st: &mut Locked<'_, W>, me: Option<u32>) -> Option<u32> {
        let mut mine = None;
        let Global { world, io, dirty } = &mut *st.global;
        world.take_dirty(dirty);
        for &member in dirty.iter() {
            let Some(Some(io)) = io.get_mut(member as usize) else { continue };
            for decision in world.take_decisions(member) {
                match decision {
                    super::Decision::Run { core, job } => {
                        if member == self.member && Some(core.0) == me {
                            mine = Some(job.0);
                        } else {
                            io.pending[core.0 as usize] = job.0;
                            io.cores[core.0 as usize].wake();
                        }
                    }
                    super::Decision::Wake { core, .. } => io.cores[core.0 as usize].wake(),
                    super::Decision::Yield { core, .. } => {
                        io.cores[core.0 as usize].moving.store(true, Ordering::Relaxed);
                    }
                }
            }
        }
        mine
    }

    /// Stops every worker and the joiner: the driver cannot go on.
    fn fail(&self, st: &mut State<W>, error: DriverError) {
        st.failed.get_or_insert(error);
        self.stop.store(true, Ordering::Release);
        for core in &self.cores[..] {
            core.wake();
        }
        if let Some(joiner) = &st.joiner {
            joiner.unpark();
        }
    }

    fn spawn_locked(&self, st: &mut Locked<'_, W>, job: W::Job, how: Spawn) -> Handle {
        let local = match st.free.pop() {
            Some(local) => local,
            None => {
                st.slots.push(Slot::Free);
                (st.slots.len() - 1) as u32
            }
        };
        st.slots[local as usize] = Slot::Fresh(job);
        st.live += 1;
        let world = st.world();
        let global = world.alloc();
        world.spawn(self.member, local, global, how);
        let generation = world.generation(global);
        let _ = self.dispatch(st, None);
        Handle { job: global, generation }
    }
}

fn new_fiber<W: Work>(shared: &Arc<Shared<W>>) -> Result<Fiber<W::Job>, DriverError> {
    let bytes = shared.config.stack;
    let stack = DefaultStack::new(bytes).map_err(|why| DriverError::Stack { bytes, why })?;
    // The stack has at least `bytes` above its guard page, which `Stack::limit` counts in.
    let limit = stack.base().get() - bytes;
    let shared = Arc::clone(shared); // clone: one per fiber made, held for its life
    let number = FIBERS.fetch_add(1, Ordering::Relaxed);
    Ok(Fiber(Coroutine::with_stack(stack, move |yielder, first| body(&shared, limit, yielder, first)), number))
}

/// A fiber's life: run each job it is started with, report its end, and wait for the next.
/// `limit` is the lowest usable address of its stack, for [`stdx::ensure_sufficient_stack`].
fn body<W: Work>(
    shared: &Shared<W>,
    limit: usize,
    yielder: &Yielder<In<W::Job>, Out>,
    first: In<W::Job>,
) {
    let mut next = first;
    loop {
        let (job, payload) = match next {
            In::Start(job, payload) => (job, payload),
            In::Exit => return,
            In::Go(_) => unreachable!("an idle fiber is only started or ended"),
        };
        let cx = Cx { shared, job, yielder };
        enter(&cx);
        stdx::set_fiber_stack_limit(limit);
        let ran = panic::catch_unwind(AssertUnwindSafe(|| shared.work.run(&cx, payload)));
        stdx::set_fiber_stack_limit(0);
        next = yielder.suspend(Out::Done(ran.err()));
    }
}

/// A running job's handle on the driver.
pub struct Cx<'a, W: Work> {
    shared: &'a Shared<W>,
    job: JobId,
    yielder: &'a Yielder<In<W::Job>, Out>,
}

impl<W: Work> Cx<'_, W> {
    pub fn job(&self) -> JobId {
        self.job
    }

    pub fn work(&self) -> &W {
        &self.shared.work
    }

    /// The driver's clock, in nanoseconds since it started: what [`Wait::Until`] counts in.
    pub fn now(&self) -> u64 {
        self.shared.now()
    }

    /// A new job of this one's, placed as `how` says (its parent is this job, whatever `how`
    /// names).
    pub fn spawn(&self, job: W::Job, mut how: Spawn) -> Handle {
        how.parent = Some(self.job);
        let mut st = self.shared.lock();
        self.shared.spawn_locked(&mut st, job, how)
    }

    /// A counter for a parallel map's jobs ([`Spawn::counted`]).
    pub fn counter(&self) -> CounterId {
        self.shared.lock().world().counter()
    }

    pub fn drop_counter(&self, counter: CounterId) {
        self.shared.lock().world().drop_counter(counter);
    }

    /// Runs every job of `jobs` as a child counted towards one counter, and waits for them all
    /// with one suspend.
    ///
    /// # Errors
    ///
    /// The cycle the wait would close; the children run on regardless.
    pub fn map(&self, jobs: impl IntoIterator<Item = W::Job>, how: Spawn) -> Result<(), Cycle> {
        let counter = self.counter();
        for job in jobs {
            self.spawn(job, how.counted(counter));
        }
        let waited = self.wait(Wait::Counter(counter));
        self.drop_counter(counter);
        waited
    }

    /// This job makes `key`: `false` when another flight has it already (wait for it).
    pub fn claim(&self, key: Key) -> bool {
        self.shared.lock().world().claim(key, Some(self.job))
    }

    /// This job's flight of `key` ended: its waiters wake on this core.
    pub fn key_done(&self, key: Key) {
        let mut st = self.shared.lock();
        st.world().key_done(key);
        let _ = self.shared.dispatch(&mut st, None);
    }

    /// Whether this job is on the critical path, by the profiler.
    pub fn set_critical(&self, critical: bool) {
        let mut st = self.shared.lock();
        st.world().set_critical(self.job, critical);
        let _ = self.shared.dispatch(&mut st, None);
    }

    /// Waits for `on`, holding no thread: the fiber switches out and resumes when it is done.
    ///
    /// # Errors
    ///
    /// The cycle the wait would close; nothing waited.
    ///
    /// # Panics
    ///
    /// For `Wait::Job` (use [`Cx::wait_job`]: a bare id may be recycled), or inside a
    /// [`NoWait`].
    pub fn wait(&self, on: Wait) -> Result<(), Cycle> {
        assert!(!matches!(on, Wait::Job(_)), "job {} waits for a bare job id: wait for its `Handle`", self.job.0);
        self.switch(Out::Wait(Want::On(on)))
    }

    /// Waits for a job spawned before, holding no thread.
    ///
    /// # Errors
    ///
    /// The cycle the wait would close; nothing waited.
    pub fn wait_job(&self, job: Handle) -> Result<(), Cycle> {
        self.switch(Out::Wait(Want::Job(job)))
    }

    /// A query miss: if the scheduler asked this core to give up its job, the job moves to a
    /// faster idle core now. One relaxed load when it did not.
    pub fn miss(&self) {
        miss();
    }

    fn relocate(&self) {
        let _ = self.switch(Out::Move);
    }

    #[inline(never)]
    fn switch(&self, out: Out) -> Result<(), Cycle> {
        check_no_wait(self.job);
        let here = HERE.get();
        assert!(
            here.is_some_and(|here| ptr::eq(here.cx, ptr::from_ref(self).cast())),
            "job {}'s `Cx` waited off its own fiber (on job {:?}'s)",
            self.job.0,
            here.map(|here| here.job.0)
        );
        let back = self.yielder.suspend(out);
        enter(self);
        match back {
            In::Go(Answer::Ready) => Ok(()),
            In::Go(Answer::Cycle(cycle)) => Err(cycle),
            In::Start(..) | In::Exit => unreachable!("a suspended job is only resumed"),
        }
    }
}

// SAFETY: the one part of a `Cx` that is not `Sync` is its `Yielder`, used only by `switch`,
// which first asserts that this thread runs the fiber of this very `Cx` (`HERE` holds its
// address, set by `enter` on that fiber and cleared as it switches out): another thread
// holding a `&Cx` panics there instead of touching the yielder. The rest goes through
// `Shared`, which is `Sync`.
#[allow(unsafe_code)]
unsafe impl<W: Work> Sync for Cx<'_, W> {}

/// This thread's running job, as the code without a [`Cx`] reaches it.
#[derive(Clone, Copy)]
struct Here {
    cx: *const (),
    job: JobId,
    wait: unsafe fn(*const (), Wait) -> Result<(), Cycle>,
    relocate: unsafe fn(*const ()),
    claim: unsafe fn(*const (), Key) -> bool,
    key_done: unsafe fn(*const (), Key),
}

thread_local! {
    static HERE: Cell<Option<Here>> = const { Cell::new(None) };
    /// This worker's core's move flag; null off a worker.
    static FLAG: Cell<*const AtomicBool> = const { Cell::new(ptr::null()) };
    /// Open [`NoWait`] guards on this thread.
    static HELD: Cell<u32> = const { Cell::new(0) };
}

#[allow(unsafe_code)]
unsafe fn wait_thunk<W: Work>(cx: *const (), on: Wait) -> Result<(), Cycle> {
    // SAFETY: `cx` is the `Cx<W>` `enter` stored, on the running fiber's stack; it is read
    // only by code running on that fiber, while it is on this thread (`clear` runs as soon as
    // the fiber switches out).
    let cx = unsafe { &*cx.cast::<Cx<'_, W>>() };
    cx.wait(on)
}

#[allow(unsafe_code)]
unsafe fn relocate_thunk<W: Work>(cx: *const ()) {
    // SAFETY: as in `wait_thunk`.
    let cx = unsafe { &*cx.cast::<Cx<'_, W>>() };
    cx.relocate();
}

#[allow(unsafe_code)]
unsafe fn claim_thunk<W: Work>(cx: *const (), key: Key) -> bool {
    // SAFETY: as in `wait_thunk`.
    let cx = unsafe { &*cx.cast::<Cx<'_, W>>() };
    cx.claim(key)
}

#[allow(unsafe_code)]
unsafe fn key_done_thunk<W: Work>(cx: *const (), key: Key) {
    // SAFETY: as in `wait_thunk`.
    let cx = unsafe { &*cx.cast::<Cx<'_, W>>() };
    cx.key_done(key);
}

/// The fiber `cx` runs is on this thread now.
#[inline(never)]
fn enter<W: Work>(cx: &Cx<'_, W>) {
    let here = Here {
        cx: ptr::from_ref(cx).cast(),
        job: cx.job,
        wait: wait_thunk::<W>,
        relocate: relocate_thunk::<W>,
        claim: claim_thunk::<W>,
        key_done: key_done_thunk::<W>,
    };
    HERE.set(Some(here));
}

/// No fiber is on this thread now.
#[inline(never)]
fn clear() {
    HERE.set(None);
}

#[inline(never)]
fn check_no_wait(job: JobId) {
    let held = HELD.get();
    assert!(held == 0, "job {} waited inside {held} `NoWait` region(s): a lock or thread-local borrow would cross threads", job.0);
}

/// The job running on this thread's fiber; `None` off a fiber.
#[inline(never)]
pub fn current_job() -> Option<JobId> {
    HERE.get().map(|here| here.job)
}

/// Waits for `on` on this thread's fiber, holding no thread. `None` off a fiber: the caller
/// waits its own way.
///
/// # Errors
///
/// Inside the `Some`: the cycle the wait would close.
#[inline(never)]
#[allow(unsafe_code)]
pub fn wait(on: Wait) -> Option<Result<(), Cycle>> {
    let here = HERE.get()?;
    // SAFETY: `here` was stored by `enter` for the fiber running this code, on this thread.
    Some(unsafe { (here.wait)(here.cx, on) })
}

/// The job on this thread's fiber makes `key` ([`Cx::claim`]): `false` when the key has a
/// flight already. `None` off a fiber.
#[inline(never)]
#[allow(unsafe_code)]
pub fn claim(key: Key) -> Option<bool> {
    let here = HERE.get()?;
    // SAFETY: as in `wait`.
    Some(unsafe { (here.claim)(here.cx, key) })
}

/// The job on this thread's fiber ended its flight of `key` ([`Cx::key_done`]). `None` off a
/// fiber.
#[inline(never)]
#[allow(unsafe_code)]
pub fn key_done(key: Key) -> Option<()> {
    let here = HERE.get()?;
    // SAFETY: as in `wait`.
    unsafe { (here.key_done)(here.cx, key) };
    Some(())
}

/// A query miss on this thread's fiber: when the scheduler asked its core to give up the job,
/// the job moves now. One relaxed load otherwise; nothing off a fiber.
#[inline(never)]
#[allow(unsafe_code)]
pub fn miss() {
    let flag = FLAG.get();
    if flag.is_null() {
        return;
    }
    // SAFETY: set once by this thread's worker to its core's flag in `Shared`, which the worker
    // holds an `Arc` of for as long as it runs; this thread is that worker.
    if !unsafe { &*flag }.load(Ordering::Relaxed) {
        return;
    }
    if let Some(here) = HERE.get() {
        // SAFETY: as in `wait`.
        unsafe { (here.relocate)(here.cx) };
    }
}

/// A thread-local that a job's code keeps across a wait (the query engine's active frames):
/// when the job parks (it waits, or moves, and its core runs something else), `take` moves its
/// value off the thread, leaving the thread's empty, and `put` moves it onto whichever thread
/// resumes the job. A wait that comes back at once moves nothing. Both run on the worker's
/// stack under the driver's lock: they must not call into the driver. Registered once, with
/// [`carry`], before any driver runs.
#[derive(Clone, Copy)]
pub struct Carried {
    pub take: fn() -> usize,
    pub put: fn(usize),
}

/// The most thread-locals carried with a job.
pub const MAX_CARRIED: usize = 16;

static CARRIED: [OnceLock<Carried>; MAX_CARRIED] = [const { OnceLock::new() }; MAX_CARRIED];
static CARRIED_LEN: AtomicUsize = AtomicUsize::new(0);

/// The fiber stack limit `stdx::ensure_sufficient_stack` checks against, carried with the job
/// (each job's limit is set at its start, in `body`).
fn carry_stack_limit() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        carry(Carried {
            take: || {
                let limit = stdx::fiber_stack_limit();
                stdx::set_fiber_stack_limit(0);
                limit
            },
            put: stdx::set_fiber_stack_limit,
        });
    });
}

/// Carries a thread-local with each job across its waits.
///
/// # Panics
///
/// Past [`MAX_CARRIED`].
pub fn carry(carried: Carried) {
    let at = CARRIED_LEN.fetch_add(1, Ordering::AcqRel);
    assert!(at < MAX_CARRIED, "more than {MAX_CARRIED} thread-locals carried with jobs");
    let _ = CARRIED[at].set(carried);
}

/// What a switching job takes off its thread: each carried thread-local's value, `None` for
/// one not registered yet when it switched out (so it is not put back either).
type Taken = [Option<usize>; MAX_CARRIED];

#[inline(never)]
fn take_carried() -> Taken {
    let mut taken = [None; MAX_CARRIED];
    let len = CARRIED_LEN.load(Ordering::Acquire).min(MAX_CARRIED);
    for (slot, carried) in taken.iter_mut().zip(&CARRIED[..len]) {
        *slot = carried.get().map(|it| (it.take)());
    }
    taken
}

#[inline(never)]
fn put_carried(taken: &Taken) {
    for (value, carried) in taken.iter().zip(&CARRIED) {
        if let (Some(value), Some(carried)) = (value, carried.get()) {
            (carried.put)(*value);
        }
    }
}

/// A job's switch on its fiber, as the observer ([`observe`]) is told it: on the thread it
/// happens on, under the driver's lock, so the observer must not call into the driver.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Switch {
    pub event: SwitchEvent,
    pub job: JobId,
    /// The job's generation: with `job`, one job of the process's life ([`Handle`]).
    pub generation: u32,
    pub fiber: u32,
    pub core: CoreId,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SwitchEvent {
    /// The job's first run, on a fiber from the idle pool.
    Started,
    /// The job parked: waiting for `Some(on)`, or `None` moving to another core. Told before
    /// its carried thread-locals are taken, so they are still the job's.
    Parked(Option<Wait>),
    /// A parked job runs again, on this thread; told after its thread-locals are put back.
    Resumed,
    /// The job ended; its fiber goes back to the idle pool.
    Ended,
}

static OBSERVER: OnceLock<fn(Switch)> = OnceLock::new();

/// Has `observer` told of every job's switches (the profiler's). Once a process; a second is
/// refused.
pub fn observe(observer: fn(Switch)) -> Result<(), fn(Switch)> {
    OBSERVER.set(observer)
}

#[inline]
fn switched(world: &World, event: SwitchEvent, job: JobId, fiber: u32, me: u32) {
    if let Some(observer) = OBSERVER.get() {
        let generation = world.generation(job);
        observer(Switch { event, job, generation, fiber, core: CoreId(me) });
    }
}

/// A region that must not wait (a lock held, a thread-local borrowed): a wait inside it
/// panics, naming the job, instead of moving the region to another thread.
pub struct NoWait(());

impl NoWait {
    #[inline(never)]
    pub fn new() -> NoWait {
        HELD.set(HELD.get() + 1);
        NoWait(())
    }
}

impl Default for NoWait {
    fn default() -> NoWait {
        NoWait::new()
    }
}

impl Drop for NoWait {
    #[inline(never)]
    fn drop(&mut self) {
        HELD.set(HELD.get() - 1);
    }
}

/// The worker threads and the scheduler they drive.
pub struct Driver<W: Work> {
    shared: Arc<Shared<W>>,
    threads: Vec<JoinHandle<()>>,
}

impl<W: Work> Driver<W> {
    /// One worker per core of `cores`, each set up by `setup` (bound to its core, given its
    /// QoS class) before any job runs.
    ///
    /// # Errors
    ///
    /// A fiber stack that could not be mapped, a thread that could not start, or a failed
    /// setup; every worker started is stopped first.
    pub fn start(
        cores: Cores,
        work: W,
        config: Config,
        setup: impl Fn(CoreId) -> io::Result<()> + Send + Sync + 'static,
    ) -> Result<Driver<W>, DriverError> {
        carry_stack_limit();
        let n = cores.len();
        let slots: Arc<[CoreSlot]> = (0..n)
            .map(|_| CoreSlot { moving: AtomicBool::new(false), woken: AtomicBool::new(false), thread: OnceLock::new() })
            .collect(); // alloc: setup, once per driver
        let mut pending = Vec::with_capacity(n);
        pending.resize(n, NONE);
        let jobs = config.fibers.max(64);
        let member = {
            let mut global = GLOBAL.lock().expect("the scheduler's lock: a call into the scheduler panicked");
            let member = global.world.join(cores, jobs);
            assert_eq!(member as usize, global.io.len(), "the world's members and their wakers");
            global.io.push(Some(Io { cores: Arc::clone(&slots), pending }));
            member
        };
        let state = State {
            slots: Vec::with_capacity(jobs),
            free: Vec::with_capacity(jobs),
            live: 0,
            idle: Vec::with_capacity(config.fibers),
            spare_taken: Vec::with_capacity(config.fibers),
            closing: false,
            joiner: None,
            panic: None,
            failed: None,
        };
        // alloc: the driver's shared state, once per driver
        let shared = Arc::new(Shared {
            work,
            member,
            state: Mutex::new(state),
            cores: slots,
            config,
            start: Instant::now(),
            go: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        });
        let mut fibers = Vec::with_capacity(config.fibers);
        for _ in 0..config.fibers {
            match new_fiber(&shared) {
                Ok(fiber) => fibers.push(fiber),
                Err(error) => {
                    let mut st = shared.lock();
                    st.world().leave(member);
                    st.global.io[member as usize] = None;
                    return Err(error);
                }
            }
        }
        shared.lock().idle = fibers;
        let setup = Arc::new(setup); // alloc: once per driver
        let (sent, ready) = crossbeam_channel::bounded(n);
        let mut threads = Vec::with_capacity(n);
        let mut failed = None;
        for core in 0..n as u32 {
            let shared = Arc::clone(&shared); // clone: one per worker, held for its life
            let setup = Arc::clone(&setup); // clone: one per worker, at start
            let sent = sent.clone();
            // alloc: a worker's name, once per worker
            let started = thread::Builder::new().name(format!("blaze-fiber-{core}")).spawn(move || {
                let set = (*setup)(CoreId(core)).map_err(|why| DriverError::Setup { core: CoreId(core), why });
                let ok = set.is_ok();
                let _ = sent.send(set);
                drop(sent);
                if ok {
                    worker(&shared, core);
                }
            });
            match started {
                Ok(handle) => threads.push(handle),
                Err(why) => {
                    failed = Some(DriverError::Thread { core: CoreId(core), why });
                    break;
                }
            }
        }
        drop(sent);
        for _ in 0..threads.len() {
            match ready.recv() {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    failed.get_or_insert(error);
                }
                // A setup panicked; `shutdown` raises its panic.
                Err(_) => {
                    failed.get_or_insert_with(|| DriverError::Setup {
                        core: CoreId(NONE),
                        why: io::Error::other("a worker's setup panicked"),
                    });
                    break;
                }
            }
        }
        let driver = Driver { shared, threads };
        if let Some(error) = failed {
            driver.shared.stop.store(true, Ordering::Release);
            driver.release();
            let _ = driver.shutdown();
            return Err(error);
        }
        driver.release();
        Ok(driver)
    }

    /// Lets every worker past its setup, each core's thread known first so no wake is lost.
    fn release(&self) {
        for (core, handle) in self.threads.iter().enumerate() {
            let _ = self.shared.cores[core].thread.set(handle.thread().clone()); // clone: a thread handle, once per worker
        }
        self.shared.go.store(true, Ordering::Release);
        for handle in &self.threads {
            handle.thread().unpark();
        }
    }

    /// A job of the driver's (no parent).
    ///
    /// # Panics
    ///
    /// Where `how` names a parent: a job's children are spawned through its [`Cx`].
    pub fn spawn(&self, job: W::Job, how: Spawn) -> Handle {
        assert!(how.parent.is_none(), "the driver spawns jobs with no parent");
        let mut st = self.shared.lock();
        self.shared.spawn_locked(&mut st, job, how)
    }

    /// A flight of the driver's (IO, a child process): `false` when the key has one already.
    pub fn claim(&self, key: Key) -> bool {
        self.shared.lock().world().claim(key, None)
    }

    /// The driver's flight of `key` ended: its waiters wake, each on the core it last ran on.
    pub fn key_done(&self, key: Key) {
        let mut st = self.shared.lock();
        st.world().key_done(key);
        let _ = self.shared.dispatch(&mut st, None);
    }

    /// Waits for every job to end, then stops the workers.
    ///
    /// # Errors
    ///
    /// The driver stopped early: a fiber stack could not be mapped.
    ///
    /// # Panics
    ///
    /// Raises the first panic of any job, once every job has ended.
    pub fn join(self) -> Result<Stats, DriverError> {
        {
            let mut st = self.shared.lock();
            st.closing = true;
            while st.live > 0 && st.failed.is_none() {
                st.joiner = Some(thread::current());
                drop(st);
                thread::park();
                st = self.shared.lock();
            }
            st.joiner = None;
        }
        self.shared.stop.store(true, Ordering::Release);
        let (stats, failed, panicked) = self.shutdown();
        if let Some(panicked) = panicked {
            panic::resume_unwind(panicked);
        }
        match failed {
            Some(error) => Err(error),
            None => Ok(stats),
        }
    }

    /// Stops and joins every worker and ends every idle fiber.
    fn shutdown(mut self) -> (Stats, Option<DriverError>, Option<Panic>) {
        for core in &self.shared.cores[..] {
            core.wake();
        }
        for handle in self.threads.drain(..) {
            if let Err(panicked) = handle.join() {
                panic::resume_unwind(panicked);
            }
        }
        let mut st = self.shared.lock();
        for mut fiber in st.idle.drain(..) {
            match fiber.0.resume(In::Exit) {
                CoroutineResult::Return(()) => {}
                CoroutineResult::Yield(_) => unreachable!("an idle fiber told to exit ends"),
            }
        }
        // Fibers of jobs that never ended (the driver failed): their stacks hold frames that
        // may hold locks, so they are not unwound; they stay mapped until the process ends.
        for slot in &mut st.slots {
            if let Slot::Parked(fiber, _) = mem::replace(slot, Slot::Free) {
                mem::forget(fiber);
            }
        }
        let member = self.shared.member;
        let stats = st.world().leave(member).map(|place| place.stats()).unwrap_or_default();
        st.global.io[member as usize] = None;
        (stats, st.failed.take(), st.panic.take())
    }
}

/// A worker: runs what the scheduler gives its core, parks when it gives nothing.
fn worker<W: Work>(shared: &Arc<Shared<W>>, me: u32) {
    while !shared.go.load(Ordering::Acquire) {
        thread::park();
    }
    if shared.stop.load(Ordering::Acquire) {
        return;
    }
    let slot = &shared.cores[me as usize];
    FLAG.set(ptr::from_ref(&slot.moving));
    let member = shared.member;
    let mut st = shared.lock();
    st.world().core_idle(member, CoreId(me));
    let mut next = shared.dispatch(&mut st, Some(me));
    loop {
        if shared.stop.load(Ordering::Acquire) {
            break;
        }
        let job = match next.take() {
            Some(job) => Some(job),
            None => st.take_pending(me),
        };
        if let Some(job) = job {
            let (back, then) = run(shared, st, me, job);
            st = back;
            next = then;
            continue;
        }
        let deadline = st.world().next_deadline(member);
        drop(st);
        idle(shared, slot, deadline);
        st = shared.lock();
        if deadline.is_some() {
            st.world().advance_clock(member, shared.now());
        }
        if st.world().place(member).running(CoreId(me)).is_none() {
            st.world().core_idle(member, CoreId(me));
        }
        next = shared.dispatch(&mut st, Some(me));
    }
    drop(st);
    FLAG.set(ptr::null());
}

/// Spins, then parks, until the core is woken, the stop is set or `deadline` passes.
fn idle<W: Work>(shared: &Shared<W>, slot: &CoreSlot, deadline: Option<u64>) {
    let mut spins = 0;
    while !slot.woken.load(Ordering::Acquire) && !shared.stop.load(Ordering::Acquire) {
        if spins < shared.config.spin {
            spins += 1;
            std::hint::spin_loop();
            continue;
        }
        match deadline {
            Some(at) => {
                let now = shared.now();
                if now >= at {
                    break;
                }
                thread::park_timeout(Duration::from_nanos(at - now));
            }
            None => thread::park(),
        }
    }
    slot.woken.store(false, Ordering::Release);
}

/// Takes the carried thread-locals of the job that just switched out on this thread, as it
/// parks: before it is visible as parked, so no core resumes it first. Only a job that really
/// waits or moves pays for it; a wait that comes back at once leaves them where they are.
fn park_carried<W: Work>(st: &mut State<W>) -> Box<Taken> {
    // alloc: a box once per job parked at once, then kept in `spare_taken`.
    let mut taken = st.spare_taken.pop().unwrap_or_else(|| Box::new([None; MAX_CARRIED]));
    *taken = take_carried();
    taken
}

/// Runs (or resumes) the job of local id `local` on core `me` until it waits, moves or ends;
/// returns the lock and the core's next `Run`.
fn run<'a, W: Work>(
    shared: &'a Arc<Shared<W>>,
    mut st: Locked<'a, W>,
    me: u32,
    local: u32,
) -> (Locked<'a, W>, Option<u32>) {
    let member = shared.member;
    let j = local as usize;
    let job = st.world().place(member).global(local);
    let (mut fiber, mut input) = match mem::replace(&mut st.slots[j], Slot::Running) {
        Slot::Fresh(payload) => {
            let fiber = match st.idle.pop() {
                Some(fiber) => fiber,
                None => match new_fiber(shared) {
                    Ok(fiber) => fiber,
                    Err(error) => {
                        st.slots[j] = Slot::Free;
                        shared.fail(&mut st, error);
                        return (st, None);
                    }
                },
            };
            switched(&st.global.world, SwitchEvent::Started, job, fiber.1, me);
            (fiber, In::Start(job, payload))
        }
        Slot::Parked(fiber, taken) => {
            put_carried(&taken);
            st.spare_taken.push(taken);
            switched(&st.global.world, SwitchEvent::Resumed, job, fiber.1, me);
            (fiber, In::Go(Answer::Ready))
        }
        Slot::Running => panic!("job {} run on core {me} while it runs", job.0),
        Slot::Free => panic!("job {} run on core {me}, never spawned", job.0),
    };
    drop(st);
    shared.cores[me as usize].moving.store(false, Ordering::Relaxed);
    loop {
        let out = match fiber.0.resume(input) {
            CoroutineResult::Yield(out) => out,
            CoroutineResult::Return(()) => unreachable!("a fiber ends only when told to exit"),
        };
        clear();
        let mut st = shared.lock();
        input = match out {
            Out::Wait(want) => {
                let on = match want {
                    Want::On(on) => Some(on),
                    Want::Job(handle) => {
                        (st.world().generation(handle.job) == handle.generation).then_some(Wait::Job(handle.job))
                    }
                };
                if let Some(Wait::Until(_)) = on {
                    st.world().advance_clock(member, shared.now());
                }
                match on.map(|on| st.world().suspended(member, job, on)) {
                    None | Some(Ok(Suspend::Ready)) => In::Go(Answer::Ready),
                    Some(Err(cycle)) => In::Go(Answer::Cycle(cycle)),
                    Some(Ok(Suspend::Waiting)) => {
                        switched(&st.global.world, SwitchEvent::Parked(on), job, fiber.1, me);
                        let taken = park_carried(&mut st);
                        st.slots[j] = Slot::Parked(fiber, taken);
                        let next = shared.dispatch(&mut st, Some(me));
                        return (st, next);
                    }
                }
            }
            Out::Move => match st.world().yielded(member, job) {
                Yielded::Stay => In::Go(Answer::Ready),
                Yielded::Moved => {
                    switched(&st.global.world, SwitchEvent::Parked(None), job, fiber.1, me);
                    let taken = park_carried(&mut st);
                    st.slots[j] = Slot::Parked(fiber, taken);
                    let next = shared.dispatch(&mut st, Some(me));
                    return (st, next);
                }
            },
            Out::Done(panicked) => {
                switched(&st.global.world, SwitchEvent::Ended, job, fiber.1, me);
                st.world().done(member, job);
                st.slots[j] = Slot::Free;
                st.free.push(local);
                st.live -= 1;
                st.idle.push(fiber);
                if let Some(panicked) = panicked {
                    st.panic.get_or_insert(panicked);
                }
                if st.live == 0
                    && st.closing
                    && let Some(joiner) = &st.joiner
                {
                    joiner.unpark();
                }
                let next = shared.dispatch(&mut st, Some(me));
                return (st, next);
            }
        };
        drop(st);
    }
}

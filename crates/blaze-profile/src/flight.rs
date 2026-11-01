//! The flight recorder: the one buffer of blaze's profiler, always on in every blaze process.
//! Each thread keeps its recent rows of the schema's `spans` and `counters` tables
//! ([`crate::schema`]) in a fixed ring of its own, made once when the thread first records, so
//! recording allocates nothing and does no I/O. Every reader reads these rings:
//! - [`read`], from a [`Cursor`]: the Parquet history and the live feed, each taking only what
//!   is new since it last read, and skipping ahead (counted) past what was written over;
//! - [`window`]: the last stretch of time, for `blaze debug profile` and a stall;
//! - [`threads_now`] and [`progress`]: what the stall watchdog reads.
//!
//! A ring is written by its thread alone and read by any. The writer publishes each row by
//! advancing `head` with release, after a release fence that orders the previous `head` before
//! the row's words. A reader that copied a row the writer was overwriting sees, after its own
//! acquire fence, a `head` that tells it so, and drops the row.
//!
//! A span's open state (when it began, its parent, depth, fields and allocation counts) is kept
//! from its start to its close in a slot of the ring of the thread that made it. Its id names
//! that ring and slot, so a span closed on another thread finds it. The row is written to the
//! ring of the thread that closes it.

use std::{
    cell::{Cell, RefCell},
    fmt::Write as _,
    sync::{
        Mutex, MutexGuard, OnceLock,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering, fence},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use rustc_hash::{FxHashMap, FxHashSet, FxHasher};

use crate::{Counter, Event, Part, clock};

/// Rows a thread keeps: [`WORDS`] words, 64 bytes, each, so 1 MiB a thread, committed as rows
/// reach it.
pub const CAP: usize = 1 << 14;
const WORDS: usize = 8;
/// Spans a thread is inside that are named (the enter stack); deeper ones are counted.
const OPEN_MAX: usize = 32;
/// Spans a thread may have made and not yet closed. A span past them is not recorded, and
/// counted as lost ([`Read::lost`]).
const SLOTS_OPEN: usize = 64;

// A span id: the ring of the thread that made it, its slot there, and the ring's count of
// spans made, so no two spans of a process alive at once share an id.
const RING_BITS: u32 = 12;
const SLOT_BITS: u32 = 6;
const SEQ_BITS: u32 = 64 - RING_BITS - SLOT_BITS;
const SEQ_MASK: u64 = (1 << SEQ_BITS) - 1;
/// Rings a process may have: one per thread alive at once. Numbers run from 1; the highest
/// number the bits hold is left out, so no span's id is [`UNRECORDED`].
const RINGS_MAX: usize = (1 << RING_BITS) - 2;
const RING_MASK: u64 = (1 << RING_BITS) - 1;
/// The id of a span that is not recorded: past the callsite table, or past [`SLOTS_OPEN`].
pub(crate) const UNRECORDED: u64 = u64::MAX;

const KIND_SPAN: u64 = 1;
const KIND_PLOT: u64 = 2;
const KIND_MARK: u64 = 3;

// Word 4 of a span's row: kind, callsite, depth, the ring of the thread that made it, and the
// allocations it made (saturating).
const SITE_BITS: u32 = 12;
const DEPTH_BITS: u32 = 12;
const ALLOCS_BITS: u32 = 64 - 2 - SITE_BITS - DEPTH_BITS - RING_BITS;
const ALLOCS_MAX: u64 = (1 << ALLOCS_BITS) - 1;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|it| it.into_inner())
}

// --- Callsites --------------------------------------------------------------------------

/// Slots of the callsite table; a site is its slot plus one, so 0 is no site.
const SLOTS: usize = (1 << SITE_BITS) - 1;
/// The table is kept at most half full, so a lookup probes one or two slots.
const SITES_MAX: usize = SLOTS / 2;

type Meta = tracing::Metadata<'static>;

/// A callsite recorded, and which of the schema's fields its spans carry.
struct Site {
    meta: &'static Meta,
    fields: u8,
}

const FIELD_CRATE: u8 = 1;
const FIELD_FUNCTION: u8 = 2;
const FIELD_UNIT: u8 = 4;

fn field_of(name: &str) -> u8 {
    match name {
        "crate" | "krate" => FIELD_CRATE,
        "function" | "subject" => FIELD_FUNCTION,
        "unit" | "cgu" => FIELD_UNIT,
        _ => 0,
    }
}

/// The callsites recorded, open-addressed by the address of their metadata: written under
/// [`TAKEN`] as a callsite registers, read with no lock by every span.
static SITES: [OnceLock<Site>; SLOTS] = [const { OnceLock::new() }; SLOTS];
static TAKEN: Mutex<usize> = Mutex::new(0);

fn home(meta: &tracing::Metadata<'_>) -> usize {
    let address = std::ptr::from_ref(meta).addr() as u64;
    (address.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32) as usize % SLOTS
}

/// The site of a callsite [`register`]ed.
#[inline]
pub(crate) fn site(meta: &tracing::Metadata<'_>) -> Option<u16> {
    let mut at = home(meta);
    for _ in 0..SLOTS {
        match SITES[at].get() {
            Some(it) if std::ptr::addr_eq(it.meta, meta) => return Some(at as u16 + 1),
            Some(_) => at = (at + 1) % SLOTS,
            None => return None,
        }
    }
    None
}

/// Gives the callsite a site, once; `None` past [`SITES_MAX`] callsites.
pub(crate) fn register(meta: &'static Meta) -> Option<u16> {
    let mut taken = lock(&TAKEN);
    if let Some(site) = site(meta) {
        return Some(site);
    }
    if *taken >= SITES_MAX {
        return None;
    }
    let mut at = home(meta);
    while SITES[at].get().is_some() {
        at = (at + 1) % SLOTS;
    }
    let fields = meta.fields().iter().fold(0, |all, it| all | field_of(it.name()));
    _ = SITES[at].set(Site { meta, fields });
    *taken += 1;
    Some(at as u16 + 1)
}

/// The callsite of `site`.
pub fn meta(site: u16) -> Option<&'static Meta> {
    Some(SITES.get(usize::from(site).checked_sub(1)?)?.get()?.meta)
}

// --- Names ------------------------------------------------------------------------------

/// A string a row names (a field's value, a thread's name), as a number: 0 is none.
pub type Sym = u32;

const SHARDS: usize = 16;
/// Strings kept a process, across its shards: past them a field is left out (null), counted.
const SYMS_MAX: usize = 1 << 18;

#[derive(Default)]
struct Shard {
    ids: FxHashMap<&'static str, Sym>,
    names: Vec<&'static str>,
}

static NAMES: [Mutex<Option<Shard>>; SHARDS] = [const { Mutex::new(None) }; SHARDS];
static SYMS: AtomicU64 = AtomicU64::new(0);
static SYMS_LEFT_OUT: AtomicU64 = AtomicU64::new(0);

/// The number of `name`, given it the first time: a hash and one lock of a shard of 16.
pub fn intern(name: &str) -> Sym {
    use std::hash::{Hash, Hasher};
    let mut hasher = FxHasher::default();
    name.hash(&mut hasher);
    let shard = (hasher.finish() >> 60) as usize % SHARDS;
    let mut names = lock(&NAMES[shard]);
    let names = names.get_or_insert_with(Shard::default);
    if let Some(&id) = names.ids.get(name) {
        return id;
    }
    if SYMS.fetch_add(1, Ordering::Relaxed) >= SYMS_MAX as u64 {
        SYMS.fetch_sub(1, Ordering::Relaxed);
        SYMS_LEFT_OUT.fetch_add(1, Ordering::Relaxed);
        return 0;
    }
    // alloc: once per distinct name a process, kept for its life (bounded by SYMS_MAX)
    let kept: &'static str = Box::leak(name.into());
    names.names.push(kept);
    let id = (names.names.len() as u32) << 4 | shard as u32;
    names.ids.insert(kept, id);
    id
}

/// The string of `sym`.
pub fn name(sym: Sym) -> Option<&'static str> {
    let index = (sym >> 4).checked_sub(1)? as usize;
    lock(&NAMES[(sym & 0xf) as usize]).as_ref()?.names.get(index).copied()
}

// --- Plots ------------------------------------------------------------------------------

/// The values worth watching over time, as counters rather than spans: a span on a warm memo
/// hit (3-4 ns) would cost ten times the hit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Plot {
    /// The nodes of the last build the memo settled, running nothing.
    MemoHits,
    /// The nodes of the last build that ran their task.
    MemoMisses,
    /// The nodes of the last build that took a task another node ran.
    Joined,
    /// Tasks of the plane ready and waiting for a worker.
    Queued,
    /// Tasks of the plane running on a worker.
    Running,
    /// Bytes the workers' frames hold (`blaze_mem::tag::Tag::Frames`).
    FrameBytes,
    /// Bytes every tag of `blaze_mem` holds.
    LiveBytes,
    /// Bytes a fiber's stack has reserved, at a job's end ([`fiber_stack`]).
    FiberStackReserved,
    /// Bytes below its base the job ran on its fiber's stack ([`FiberStack::deepest`]).
    FiberStackDeepest,
    /// Bytes of the fiber's stack committed so far ([`FiberStack::touched`]).
    FiberStackTouched,
    /// Bytes of segments the job grew past its fiber's stack ([`FiberStack::grown`]).
    FiberStackGrown,
}

impl Plot {
    pub const ALL: [Plot; 11] = [
        Plot::MemoHits,
        Plot::MemoMisses,
        Plot::Joined,
        Plot::Queued,
        Plot::Running,
        Plot::FrameBytes,
        Plot::LiveBytes,
        Plot::FiberStackReserved,
        Plot::FiberStackDeepest,
        Plot::FiberStackTouched,
        Plot::FiberStackGrown,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Plot::MemoHits => "memo hits",
            Plot::MemoMisses => "memo misses",
            Plot::Joined => "joined",
            Plot::Queued => "pool queued",
            Plot::Running => "pool running",
            Plot::FrameBytes => "frame bytes",
            Plot::LiveBytes => "live bytes",
            Plot::FiberStackReserved => "fiber stack reserved",
            Plot::FiberStackDeepest => "fiber stack deepest",
            Plot::FiberStackTouched => "fiber stack touched",
            Plot::FiberStackGrown => "fiber stack grown",
        }
    }

    pub fn of(at: u64) -> Option<Plot> {
        Plot::ALL.get(usize::try_from(at).ok()?).copied()
    }
}

/// Notes `value` of `which` now, in this thread's ring.
#[inline]
pub fn plot(which: Plot, value: u64) {
    if let Some(ring) = ring() {
        let build = u64::from(BUILD.with(Cell::get));
        ring.put([clock::ticks(), value, 0, 0, KIND_PLOT << 62 | which as u64, 0, build, 0]);
    }
}

/// What a job left on its fiber's stack, read by the fiber driver when the job ends. Every
/// size is in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FiberStack {
    /// The fiber, numbered by the driver: the same number for the fiber's whole life.
    pub fiber: u32,
    /// The job that ran on it.
    pub job: u64,
    /// What the fiber's stack reserved, guard page left out.
    pub reserved: u64,
    /// How far below its base the job's stack pointer went, as far as the driver saw it.
    pub deepest: u64,
    /// How much of the fiber's stack is committed: the deepest any job on it went, to the page.
    pub touched: u64,
    /// Segments the job grew past the fiber's stack.
    pub grown: u64,
}

/// The plot byte of a [`fiber_stack`] row: one row a job, read as four counters.
const FIBER_STACK_ROW: u64 = 0xff;

/// Notes `stack` now, in this thread's ring: one row, read as the counters
/// [`Plot::FiberStackReserved`], [`Plot::FiberStackDeepest`], [`Plot::FiberStackTouched`] and
/// [`Plot::FiberStackGrown`] of its fiber and job.
#[inline]
pub fn fiber_stack(stack: FiberStack) {
    if let Some(ring) = ring() {
        let build = u64::from(BUILD.with(Cell::get));
        let what = KIND_PLOT << 62 | u64::from(stack.fiber) << 8 | FIBER_STACK_ROW;
        ring.put([clock::ticks(), stack.reserved, stack.deepest, stack.touched, what, stack.grown, build, stack.job]);
    }
}

/// Notes the end of the build `serial`: a mark on the timeline.
pub(crate) fn mark(serial: u64) {
    if let Some(ring) = ring() {
        ring.put([clock::ticks(), serial, 0, 0, KIND_MARK << 62, 0, 0, 0]);
    }
}

// --- The build a thread works for ---------------------------------------------------------

thread_local! {
    /// The build this thread's work is for now: 0 for none.
    static BUILD: Cell<u32> = const { Cell::new(0) };
}

/// While the result lives, what this thread records is of build `build`: the plane sets it
/// around each task it runs for a request.
#[must_use]
pub fn building(build: u32) -> Building {
    Building(BUILD.with(|it| it.replace(build)))
}

/// See [`building`]: gives back the build the thread was working for before.
pub struct Building(u32);

impl Drop for Building {
    fn drop(&mut self) {
        BUILD.with(|it| it.set(self.0));
    }
}

// --- Allocations --------------------------------------------------------------------------

/// What a thread has allocated so far: bytes and allocations, never decreasing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Allocated {
    pub bytes: u64,
    pub count: u64,
}

/// The reader of the calling thread's [`Allocated`], from the global allocator's own
/// thread-local counts: given by the binary that installs that allocator, once.
static ALLOCATED: OnceLock<fn() -> Allocated> = OnceLock::new();

/// Has every span note what its thread allocated while it was open, read by `read` (the
/// global allocator's counts for the calling thread). Without it `alloc_bytes` and `allocs`
/// are null. Once a process; a second call is refused.
pub fn count_allocations(read: fn() -> Allocated) -> Result<(), fn() -> Allocated> {
    ALLOCATED.set(read)
}

#[inline]
fn allocated() -> Option<Allocated> {
    ALLOCATED.get().map(|read| read())
}

// --- Rings ------------------------------------------------------------------------------

/// Who wrote a ring from row `from` on: rings outlive threads and go to the next thread.
struct Owner {
    from: u64,
    tid: u32,
    name: Sym,
}

/// A span made and not closed: written by the thread that made it, read and freed by the one
/// that closes it. `id` is 0 while the slot is free, and published last with release.
struct Slot {
    id: AtomicU64,
    /// Handles of the span alive (`tracing` clones and closes each): it closes with the last.
    refs: AtomicU64,
    start: AtomicU64,
    parent: AtomicU64,
    /// Its site, depth and build.
    what: AtomicU64,
    /// Its crate and function.
    of: AtomicU64,
    unit: AtomicU64,
    alloc_bytes: AtomicU64,
    allocs: AtomicU64,
}

impl Slot {
    const FREE: Slot = Slot {
        id: AtomicU64::new(0),
        refs: AtomicU64::new(0),
        start: AtomicU64::new(0),
        parent: AtomicU64::new(0),
        what: AtomicU64::new(0),
        of: AtomicU64::new(0),
        unit: AtomicU64::new(0),
        alloc_bytes: AtomicU64::new(0),
        allocs: AtomicU64::new(0),
    };
}

/// What a slot held, read whole.
#[derive(Clone, Copy)]
struct Opened {
    start: u64,
    parent: u64,
    site: u16,
    depth: u16,
    build: u32,
    of: u64,
    unit: u64,
    alloc: Allocated,
}

pub(crate) struct Ring {
    /// Its number, in span ids and in [`BY_NUMBER`].
    number: u16,
    /// Rows ever written.
    head: AtomicU64,
    /// Spans ever closed and work noted ([`moved`]): what [`progress`] sums.
    spans: AtomicU64,
    /// Spans its thread made and could not record: past [`SLOTS_OPEN`] open at once.
    lost: AtomicU64,
    /// Spans its threads made, for their ids.
    made: AtomicU64,
    /// Spans the thread is inside, the first [`OPEN_MAX`] of them in `entered`.
    depth: AtomicU32,
    entered: [AtomicU64; OPEN_MAX],
    open: [Slot; SLOTS_OPEN],
    /// Whether a live thread holds it.
    live: AtomicBool,
    /// The thread that holds it now or held it last: its tid and name, as [`Owner`] has them,
    /// read with no lock by a reader holding another ring's `owners`.
    current: AtomicU64,
    /// [`WORDS`] words a row.
    words: Box<[AtomicU64]>,
    owners: Mutex<Vec<Owner>>,
    /// What the sampler needs of the thread, under the lock the thread takes to leave.
    pub(crate) sampled: Mutex<crate::sampler::Thread>,
}

impl Ring {
    fn new(number: u16) -> Ring {
        // Zeroed by the allocator, not written: a page of the ring is committed only once
        // rows reach it, so a thread that records little holds little.
        // alloc: once per thread that records, its ring for the process's life
        let words = Box::<[AtomicU64]>::new_zeroed_slice(WORDS * CAP);
        // SAFETY: all-zero bytes are a valid `AtomicU64`.
        let words = unsafe { words.assume_init() };
        Ring {
            number,
            head: AtomicU64::new(0),
            spans: AtomicU64::new(0),
            lost: AtomicU64::new(0),
            made: AtomicU64::new(0),
            depth: AtomicU32::new(0),
            entered: [const { AtomicU64::new(0) }; OPEN_MAX],
            open: [Slot::FREE; SLOTS_OPEN],
            live: AtomicBool::new(true),
            current: AtomicU64::new(0),
            words,
            // alloc: once per thread, as it first records
            owners: Mutex::new(Vec::new()),
            sampled: Mutex::new(crate::sampler::Thread::default()),
        }
    }

    #[inline]
    fn put(&self, row: [u64; WORDS]) {
        let i = self.head.load(Ordering::Relaxed);
        let at = (i as usize & (CAP - 1)) * WORDS;
        fence(Ordering::Release);
        for (word, value) in self.words[at..at + WORDS].iter().zip(row) {
            word.store(value, Ordering::Relaxed);
        }
        self.head.store(i + 1, Ordering::Release);
    }

    /// The rows from index `from` that are whole now, each with its index, into `into`; the
    /// index to read from next.
    fn read(&self, from: u64, into: &mut Vec<(u64, [u64; WORDS])>) -> u64 {
        let head = self.head.load(Ordering::Acquire);
        let first = into.len();
        for i in from.max(head.saturating_sub(CAP as u64))..head {
            let at = (i as usize & (CAP - 1)) * WORDS;
            let mut row = [0; WORDS];
            for (value, word) in row.iter_mut().zip(&self.words[at..at + WORDS]) {
                *value = word.load(Ordering::Relaxed);
            }
            into.push((i, row));
        }
        fence(Ordering::Acquire);
        let whole_from = (self.head.load(Ordering::Relaxed) + 1).saturating_sub(CAP as u64);
        let mut kept = first;
        for i in first..into.len() {
            if into[i].0 >= whole_from {
                into.swap(kept, i);
                kept += 1;
            }
        }
        into.truncate(kept);
        head
    }

    /// A free slot for a span its thread makes: the one at `depth` when free, else the next
    /// free one. Only the ring's thread takes slots; any thread frees them.
    #[inline]
    fn free_slot(&self, depth: u32) -> Option<usize> {
        let first = depth as usize % SLOTS_OPEN;
        (0..SLOTS_OPEN)
            .map(|it| (first + it) % SLOTS_OPEN)
            .find(|&it| self.open[it].id.load(Ordering::Acquire) == 0)
    }

    /// What the slot of span `id` holds, while `id` is in it.
    fn opened(&self, id: u64) -> Option<Opened> {
        let slot = &self.open[slot_of(id)];
        if slot.id.load(Ordering::Acquire) != id {
            return None;
        }
        let what = slot.what.load(Ordering::Relaxed);
        let opened = Opened {
            start: slot.start.load(Ordering::Relaxed),
            parent: slot.parent.load(Ordering::Relaxed),
            site: (what & ((1 << SITE_BITS) - 1)) as u16,
            depth: (what >> 16 & 0xffff) as u16,
            build: (what >> 32) as u32,
            of: slot.of.load(Ordering::Relaxed),
            unit: slot.unit.load(Ordering::Relaxed),
            alloc: Allocated {
                bytes: slot.alloc_bytes.load(Ordering::Relaxed),
                count: slot.allocs.load(Ordering::Relaxed),
            },
        };
        fence(Ordering::Acquire);
        (slot.id.load(Ordering::Relaxed) == id).then_some(opened)
    }

    /// The span ids the thread is inside, outermost first, and how many there are.
    fn entered_now(&self) -> (Vec<u64>, u32) {
        let depth = self.depth.load(Ordering::Acquire);
        let named = (depth as usize).min(OPEN_MAX);
        // alloc: a dump, on demand
        (self.entered[..named].iter().map(|it| it.load(Ordering::Relaxed)).collect(), depth)
    }

    /// The tid and name of [`Ring::current`].
    fn current(&self) -> (u32, Sym) {
        let current = self.current.load(Ordering::Acquire);
        ((current >> 32) as u32, current as u32)
    }

    /// The thread that wrote row `i`, of those in `owners`.
    fn owner(owners: &[Owner], i: u64) -> Option<&Owner> {
        let at = owners.partition_point(|it| it.from <= i);
        owners.get(at.checked_sub(1)?)
    }
}

fn ring_of(id: u64) -> usize {
    (id >> (SLOT_BITS + SEQ_BITS)) as usize
}

fn slot_of(id: u64) -> usize {
    (id >> SEQ_BITS) as usize & ((1 << SLOT_BITS) - 1)
}

/// Every ring, in the order made: a ring's number is its place here plus one.
// alloc: once a process
static RINGS: Mutex<Vec<&'static Ring>> = Mutex::new(Vec::new());
/// Every ring by its number, read with no lock by a span closed on another thread.
static BY_NUMBER: [OnceLock<&'static Ring>; RINGS_MAX] = [const { OnceLock::new() }; RINGS_MAX];
/// Spans made on threads past [`RINGS_MAX`] alive at once, so with no ring: not recorded.
static RINGLESS: AtomicU64 = AtomicU64::new(0);

fn by_number(number: usize) -> Option<&'static Ring> {
    BY_NUMBER.get(number.checked_sub(1)?)?.get().copied()
}

/// Gives the ring back when its thread ends.
struct Held(Cell<Option<&'static Ring>>);

impl Drop for Held {
    fn drop(&mut self) {
        _ = MINE.try_with(|it| it.set(None));
        if let Some(ring) = self.0.take() {
            let mut sampled = lock(&ring.sampled);
            sampled.leave();
            ring.depth.store(0, Ordering::Relaxed);
            ring.live.store(false, Ordering::Release);
            drop(sampled);
        }
    }
}

thread_local! {
    /// This thread's ring, read by every span: no destructor, so it is there to the end.
    static MINE: Cell<Option<&'static Ring>> = const { Cell::new(None) };
    static HELD: Held = const { Held(Cell::new(None)) };
    /// Where a field's value that is not a string is written before it is interned.
    // alloc: once per thread, grown to its longest field
    static TEXT: RefCell<String> = const { RefCell::new(String::new()) };
}

#[inline]
pub(crate) fn ring() -> Option<&'static Ring> {
    match MINE.with(Cell::get) {
        Some(ring) => Some(ring),
        None => claim(),
    }
}

/// Takes a ring a thread left, or makes one, for this thread. `None` once the thread is
/// ending past the point its ring was given back, or past [`RINGS_MAX`] threads at once.
#[cold]
fn claim() -> Option<&'static Ring> {
    HELD.try_with(|held| {
        let tid = crate::sampler::os_tid() as u32;
        let current = std::thread::current();
        let name = match current.name() {
            Some(name) => intern(name),
            None => {
                // alloc: a thread's name, once as it first records
                let mut text = String::new();
                _ = write!(text, "thread {tid}");
                intern(&text)
            }
        };
        let mut rings = lock(&RINGS);
        let left = rings.iter().copied().find(|it| {
            it.live.compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed).is_ok()
        });
        let ring: &'static Ring = match left {
            Some(ring) => ring,
            None if rings.len() < RINGS_MAX => {
                let number = rings.len() as u16 + 1;
                // alloc: once per thread past those that ended, kept for the process's life
                let ring: &'static Ring = Box::leak(Box::new(Ring::new(number)));
                rings.push(ring);
                _ = BY_NUMBER[usize::from(number) - 1].set(ring);
                ring
            }
            None => return None,
        };
        drop(rings);
        let head = ring.head.load(Ordering::Relaxed);
        let mut owners = lock(&ring.owners);
        owners.push(Owner { from: head, tid, name });
        // An owner whose rows were all written over names none.
        let oldest = head.saturating_sub(CAP as u64);
        while owners.len() > 1 && owners[1].from <= oldest {
            owners.remove(0);
        }
        drop(owners);
        ring.current.store(u64::from(tid) << 32 | u64::from(name), Ordering::Release);
        ring.depth.store(0, Ordering::Relaxed);
        lock(&ring.sampled).enter(u64::from(tid));
        held.0.set(Some(ring));
        MINE.with(|it| it.set(Some(ring)));
        Some(ring)
    })
    .ok()
    .flatten()
}

// --- What spans do ----------------------------------------------------------------------

/// The schema's fields of a span, as it is made.
struct Fields {
    wanted: u8,
    krate: Sym,
    function: Sym,
    unit: Sym,
}

impl Fields {
    fn set(&mut self, field: u8, value: &str) {
        let sym = intern(value);
        match field {
            FIELD_CRATE => self.krate = sym,
            FIELD_FUNCTION => self.function = sym,
            FIELD_UNIT => self.unit = sym,
            _ => {}
        }
    }
}

impl tracing::field::Visit for Fields {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        let field = field_of(field.name()) & self.wanted;
        if field != 0 {
            self.set(field, value);
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let field = field_of(field.name()) & self.wanted;
        if field == 0 {
            return;
        }
        _ = TEXT.try_with(|text| {
            let mut text = text.borrow_mut();
            text.clear();
            _ = write!(text, "{value:?}");
            self.set(field, &text);
        });
    }
}

/// Makes the span `span` of `site` beginning now on this thread: its id, or [`UNRECORDED`].
#[inline]
pub(crate) fn opened(span: &tracing::span::Attributes<'_>, site: u16) -> u64 {
    let Some(ring) = ring() else {
        RINGLESS.fetch_add(1, Ordering::Relaxed);
        return UNRECORDED;
    };
    let depth = ring.depth.load(Ordering::Relaxed);
    let Some(at) = ring.free_slot(depth) else {
        ring.lost.store(ring.lost.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
        return UNRECORDED;
    };
    let parent = if span.is_root() {
        0
    } else if let Some(parent) = span.parent() {
        parent.into_u64()
    } else {
        depth
            .checked_sub(1)
            .and_then(|top| ring.entered.get(top as usize))
            .map_or(0, |it| it.load(Ordering::Relaxed))
    };
    let wanted = SITES[usize::from(site) - 1].get().map_or(0, |it| it.fields);
    let mut fields = Fields { wanted, krate: 0, function: 0, unit: 0 };
    if wanted != 0 {
        span.record(&mut fields);
    }
    let made = ring.made.load(Ordering::Relaxed) + 1;
    ring.made.store(made, Ordering::Relaxed);
    let id = u64::from(ring.number) << (SLOT_BITS + SEQ_BITS) | (at as u64) << SEQ_BITS | made & SEQ_MASK;
    let slot = &ring.open[at];
    let build = BUILD.with(Cell::get);
    let alloc = allocated().unwrap_or_default();
    slot.start.store(clock::ticks(), Ordering::Relaxed);
    slot.parent.store(parent, Ordering::Relaxed);
    slot.what.store(u64::from(build) << 32 | u64::from(depth.min(0xffff)) << 16 | u64::from(site), Ordering::Relaxed);
    slot.of.store(u64::from(fields.krate) << 32 | u64::from(fields.function), Ordering::Relaxed);
    slot.unit.store(u64::from(fields.unit), Ordering::Relaxed);
    slot.alloc_bytes.store(alloc.bytes, Ordering::Relaxed);
    slot.allocs.store(alloc.count, Ordering::Relaxed);
    slot.refs.store(1, Ordering::Relaxed);
    slot.id.store(id, Ordering::Release);
    id
}

/// Another handle of the open span `id` was made.
pub(crate) fn cloned(id: u64) {
    if id == UNRECORDED {
        return;
    }
    if let Some(ring) = by_number(ring_of(id)) {
        let slot = &ring.open[slot_of(id)];
        if slot.id.load(Ordering::Acquire) == id {
            slot.refs.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// The site of the open span `id`.
pub(crate) fn site_of(id: u64) -> Option<u16> {
    if id == UNRECORDED {
        return None;
    }
    Some(by_number(ring_of(id))?.opened(id)?.site)
}

#[inline]
pub(crate) fn entered(id: u64) {
    if id == UNRECORDED {
        return;
    }
    let Some(ring) = ring() else { return };
    let depth = ring.depth.load(Ordering::Relaxed);
    if let Some(slot) = ring.entered.get(depth as usize) {
        slot.store(id, Ordering::Relaxed);
    }
    ring.depth.store(depth + 1, Ordering::Release);
}

#[inline]
pub(crate) fn exited(id: u64) {
    if id == UNRECORDED {
        return;
    }
    let Some(ring) = ring() else { return };
    let depth = ring.depth.load(Ordering::Relaxed);
    let Some(top) = depth.checked_sub(1) else { return };
    let named = (depth as usize).min(OPEN_MAX);
    // Spans leave in the order they entered but for a guard dropped out of turn: then the
    // ones above it move down.
    if (top as usize) < OPEN_MAX && ring.entered[top as usize].load(Ordering::Relaxed) != id {
        let Some(at) = ring.entered[..named].iter().rposition(|it| it.load(Ordering::Relaxed) == id) else {
            return;
        };
        for i in at..named - 1 {
            ring.entered[i].store(ring.entered[i + 1].load(Ordering::Relaxed), Ordering::Relaxed);
        }
    }
    ring.depth.store(top, Ordering::Release);
}

/// A handle of the span `id` was dropped, on this thread. With the last, the span closed now:
/// its row is written to this thread's ring, and `Some` the ticks it lasted, which is what
/// `BLAZE_PROFILE` keeps of it.
#[inline]
pub(crate) fn closed(id: u64) -> Option<u64> {
    if id == UNRECORDED {
        moved();
        return None;
    }
    let made_by = by_number(ring_of(id))?;
    let slot = &made_by.open[slot_of(id)];
    if slot.id.load(Ordering::Acquire) != id || slot.refs.fetch_sub(1, Ordering::AcqRel) > 1 {
        return None;
    }
    let opened = made_by.opened(id)?;
    slot.id.store(0, Ordering::Release);
    let now = clock::ticks();
    // The allocations as their count plus one, so 0 is "not counted".
    let (allocs, bytes) = allocated().map_or((0, 0), |now| {
        let count = now.count.saturating_sub(opened.alloc.count).min(ALLOCS_MAX - 1);
        (count + 1, now.bytes.saturating_sub(opened.alloc.bytes))
    });
    let what = KIND_SPAN << 62
        | u64::from(opened.site) << (62 - SITE_BITS)
        | u64::from(opened.depth.min((1 << DEPTH_BITS) - 1)) << (62 - SITE_BITS - DEPTH_BITS)
        | (ring_of(id) as u64) << ALLOCS_BITS
        | allocs;
    let build_unit = u64::from(opened.build) << 32 | opened.unit;
    let ring = ring()?;
    ring.put([now, opened.start, id, opened.parent, what, opened.of, build_unit, bytes]);
    ring.spans.store(ring.spans.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
    Some(now.wrapping_sub(opened.start))
}

/// Notes that work moved on this thread with no span closing: what a long piece of work
/// with no span of its own (a query's run) says to the stall watchdog. One store.
#[inline]
pub fn moved() {
    if let Some(ring) = ring() {
        ring.spans.store(ring.spans.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
    }
}

/// Every ring now, into `into`.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub(crate) fn rings(into: &mut Vec<&'static Ring>) {
    into.clear();
    into.extend(lock(&RINGS).iter().copied());
}

/// Spans closed in this process so far, on every thread: it grows while any work moves.
pub fn progress() -> u64 {
    lock(&RINGS).iter().map(|it| it.spans.load(Ordering::Relaxed)).sum()
}

// --- Reading rows -----------------------------------------------------------------------

/// A row of the schema's `spans` table, as the ring holds it: names as sites and [`Sym`]s,
/// resolved by the writer of a table ([`meta`], [`name`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpanRow {
    pub build: u32,
    pub thread: u32,
    pub thread_name: Sym,
    pub span_id: u64,
    pub parent: u64,
    pub depth: u16,
    pub site: u16,
    pub start_ns: i64,
    pub dur_ns: i64,
    /// `None` when allocations are not counted ([`count_allocations`]).
    pub alloc: Option<Allocated>,
    pub krate: Sym,
    pub function: Sym,
    pub unit: Sym,
}

/// A row of the schema's `counters` table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CounterRow {
    pub build: u32,
    pub plot: Plot,
    pub t_ns: i64,
    pub value: u64,
    /// The fiber and job a [`fiber_stack`] counter is of; `None` for every other plot.
    pub fiber: Option<(u32, u64)>,
}

/// A build's end, as [`crate::built`] noted it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuildEnd {
    pub build: u64,
    pub t_ns: i64,
    pub thread: u32,
}

/// What a read took from the rings.
#[derive(Default, Debug)]
pub struct Read {
    pub spans: Vec<SpanRow>,
    pub counters: Vec<CounterRow>,
    pub ends: Vec<BuildEnd>,
    /// Rows written over before this reader read them: it fell behind by that many.
    pub skipped: u64,
    /// Spans made and not recorded since the process began: past the callsite table, the
    /// open slots of a thread, or the rings of a process.
    pub lost: u64,
    /// Field values left out since the process began: past the names a process keeps.
    pub names_left_out: u64,
    /// When the read was, on the schema's clock.
    pub at_ns: i64,
    scratch: Vec<(u64, [u64; WORDS])>,
}

impl Read {
    pub fn clear(&mut self) {
        self.spans.clear();
        self.counters.clear();
        self.ends.clear();
        self.skipped = 0;
    }
}

/// Where a reader is in each ring: the next row it takes. A new cursor takes everything the
/// rings hold.
#[derive(Clone, Debug, Default)]
pub struct Cursor {
    next: Vec<u64>,
}

/// Takes into `into` every row written since `cursor`, and moves `cursor` past them. A reader
/// slower than the writers loses what was written over before it read, counted in
/// [`Read::skipped`]; its memory stays what one read holds.
pub fn read(cursor: &mut Cursor, into: &mut Read) {
    take(into, |ring, scratch| {
        let at = usize::from(ring.number) - 1;
        if cursor.next.len() <= at {
            cursor.next.resize(at + 1, 0);
        }
        let from = cursor.next[at];
        let head = ring.read(from, scratch);
        cursor.next[at] = head;
        head.saturating_sub(CAP as u64).saturating_sub(from)
    });
}

/// Takes into `into` every row of the last `window` the rings still hold.
pub fn window(window: Duration, into: &mut Read) {
    let since = clock::mono_ns() - window.as_nanos() as i64;
    take(into, |ring, scratch| {
        ring.read(0, scratch);
        0
    });
    into.spans.retain(|it| it.start_ns + it.dur_ns >= since);
    into.counters.retain(|it| it.t_ns >= since);
    into.ends.retain(|it| it.t_ns >= since);
}

fn take(into: &mut Read, mut rows: impl FnMut(&Ring, &mut Vec<(u64, [u64; WORDS])>) -> u64) {
    let scale = clock::Scale::now();
    into.at_ns = scale.mono(clock::ticks());
    // clone: the list of rings, so none is read under the lock threads take to start
    let rings = lock(&RINGS).clone();
    let mut scratch = std::mem::take(&mut into.scratch);
    let mut lost = RINGLESS.load(Ordering::Relaxed);
    for ring in rings {
        lost += ring.lost.load(Ordering::Relaxed);
        scratch.clear();
        into.skipped += rows(ring, &mut scratch);
        let owners = lock(&ring.owners);
        for &(i, row) in &scratch {
            let Some(by) = Ring::owner(&owners, i) else { continue };
            match row[4] >> 62 {
                KIND_SPAN => {
                    let made_on = (row[4] >> ALLOCS_BITS & RING_MASK) as usize;
                    let (thread, thread_name) = if made_on == usize::from(ring.number) {
                        (by.tid, by.name)
                    } else {
                        // Closed on another thread than the one that made it: that thread is
                        // the one its ring holds now, or held last.
                        by_number(made_on).map_or((by.tid, by.name), Ring::current)
                    };
                    let allocs = row[4] & ALLOCS_MAX;
                    into.spans.push(SpanRow {
                        build: (row[6] >> 32) as u32,
                        thread,
                        thread_name,
                        span_id: row[2],
                        parent: row[3],
                        depth: (row[4] >> (62 - SITE_BITS - DEPTH_BITS) & ((1 << DEPTH_BITS) - 1)) as u16,
                        site: (row[4] >> (62 - SITE_BITS) & ((1 << SITE_BITS) - 1)) as u16,
                        start_ns: scale.mono(row[1]),
                        dur_ns: scale.ns(row[0].wrapping_sub(row[1])) as i64,
                        alloc: (allocs != 0).then(|| Allocated { bytes: row[7], count: allocs - 1 }),
                        krate: (row[5] >> 32) as u32,
                        function: row[5] as u32,
                        unit: row[6] as u32,
                    });
                }
                KIND_PLOT if row[4] & 0xff == FIBER_STACK_ROW => {
                    let fiber = Some(((row[4] >> 8) as u32, row[7]));
                    let (build, t_ns) = (row[6] as u32, scale.mono(row[0]));
                    into.counters.extend(
                        [
                            (Plot::FiberStackReserved, row[1]),
                            (Plot::FiberStackDeepest, row[2]),
                            (Plot::FiberStackTouched, row[3]),
                            (Plot::FiberStackGrown, row[5]),
                        ]
                        .map(|(plot, value)| CounterRow { build, plot, t_ns, value, fiber }),
                    );
                }
                KIND_PLOT => {
                    let Some(plot) = Plot::of(row[4] & 0xff) else { continue };
                    into.counters.push(CounterRow {
                        build: row[6] as u32,
                        plot,
                        t_ns: scale.mono(row[0]),
                        value: row[1],
                        fiber: None,
                    });
                }
                KIND_MARK => into.ends.push(BuildEnd { build: row[1], t_ns: scale.mono(row[0]), thread: by.tid }),
                _ => {}
            }
        }
    }
    into.lost = lost;
    into.names_left_out = SYMS_LEFT_OUT.load(Ordering::Relaxed);
    into.scratch = scratch;
}

// --- Threads now ------------------------------------------------------------------------

/// A span a thread is inside now, and for how long it has been.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenSpan {
    pub name: &'static str,
    pub target: &'static str,
    pub for_ns: u64,
}

/// A live thread and the spans it is inside now, outermost first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadNow {
    pub tid: u64,
    pub name: String,
    pub open: Vec<OpenSpan>,
    /// Spans it is inside past the [`OPEN_MAX`] named.
    pub deeper: u32,
}

/// Every live thread that is inside a span, and those spans. A thread moving while it is read
/// may show a span it just left; the stall watchdog reads threads that do not move.
pub fn threads_now() -> Vec<ThreadNow> {
    let scale = clock::Scale::now();
    let now = clock::ticks();
    // clone: the list of rings, so none is read under the lock threads take to start
    let rings = lock(&RINGS).clone();
    // alloc: a dump, on demand
    let mut threads = Vec::new();
    for ring in rings {
        if !ring.live.load(Ordering::Acquire) {
            continue;
        }
        let (ids, depth) = ring.entered_now();
        if depth == 0 {
            continue;
        }
        let open = ids
            .iter()
            .filter_map(|&id| {
                let opened = by_number(ring_of(id))?.opened(id)?;
                let meta = meta(opened.site)?;
                Some(OpenSpan {
                    name: meta.name(),
                    target: meta.target(),
                    for_ns: scale.ns(now.wrapping_sub(opened.start)),
                })
            })
            // alloc: a dump, on demand
            .collect();
        let (tid, name) = ring.current();
        // alloc: a dump, on demand
        let name = self::name(name).unwrap_or_default().to_owned();
        threads.push(ThreadNow { tid: u64::from(tid), name, open, deeper: depth.saturating_sub(OPEN_MAX as u32) });
    }
    threads
}

// --- The Chrome trace of a window ---------------------------------------------------------

/// Microseconds since the Unix epoch of instants, from one reading of both clocks.
pub(crate) struct Epoch {
    now: Instant,
    now_us: u64,
    mono_ns: i64,
}

impl Epoch {
    pub(crate) fn now() -> Epoch {
        let now_us = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_micros() as u64;
        Epoch { now: Instant::now(), now_us, mono_ns: clock::mono_ns() }
    }

    pub(crate) fn us(&self, at: Instant) -> u64 {
        self.now_us.saturating_sub(self.now.saturating_duration_since(at).as_micros() as u64)
    }

    fn us_of_mono(&self, ns: i64) -> u64 {
        self.now_us.saturating_sub((self.mono_ns - ns).max(0) as u64 / 1000)
    }
}

/// What every ring holds of the last `window`, as one process's part of a profile.
pub(crate) fn part(window: Duration) -> Part {
    let epoch = Epoch::now();
    let since = epoch.now.checked_sub(window).unwrap_or(epoch.now);
    let mut read = Read::default();
    self::window(window, &mut read);
    let mut part = Part { pid: std::process::id(), command: crate::command(), ..Part::default() };
    let mut named: FxHashSet<u32> = FxHashSet::default();
    for span in &read.spans {
        if named.insert(span.thread) {
            // alloc: a dump, on demand
            let thread = name(span.thread_name).unwrap_or_default().to_owned();
            part.threads.push((u64::from(span.thread), thread));
        }
        let Some(meta) = meta(span.site) else { continue };
        part.events.push(Event {
            // alloc: a dump, on demand
            name: meta.name().to_owned(),
            // alloc: a dump, on demand
            subject: name(span.function).map(str::to_owned),
            tid: u64::from(span.thread),
            start_us: epoch.us_of_mono(span.start_ns),
            dur_us: span.dur_ns.max(0) as u64 / 1000,
            cpu_us: None,
        });
    }
    for counter in &read.counters {
        part.counters.push(Counter {
            // alloc: a dump, on demand
            name: counter.plot.name().to_owned(),
            tid: 0,
            at_us: epoch.us_of_mono(counter.t_ns),
            value: counter.value,
        });
    }
    for end in &read.ends {
        part.events.push(Event {
            // alloc: a dump, on demand
            name: format!("build {} ended", end.build),
            subject: None,
            tid: u64::from(end.thread),
            start_us: epoch.us_of_mono(end.t_ns),
            dur_us: 0,
            cpu_us: None,
        });
    }
    let (samples, sampler) = crate::sampler::taken(since, &epoch);
    part.samples = samples;
    part.sampler = sampler;
    part
}

/// The last `window` of every thread's ring and of the CPU samples, as the Chrome trace
/// (ui.perfetto.dev) `BLAZE_PROFILE` writes: what a stall writes beside its error.
pub fn dump(window: Duration) -> Vec<u8> {
    let part = part(window);
    let end = Epoch::now().now_us;
    let start = end.saturating_sub(window.as_micros() as u64);
    // alloc: a dump, on demand
    let mut out = Vec::new();
    match crate::chrome(&mut out, &[], std::slice::from_ref(&part), start, end) {
        Ok(()) => out,
        Err(why) => unreachable!("a trace written to memory fails only when out of memory: {why}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plot_row(i: u64) -> [u64; WORDS] {
        [i, i, i, i, KIND_PLOT << 62, i, i, i]
    }

    #[test]
    fn a_ring_keeps_the_last_rows_whole_and_in_order() {
        let ring = Ring::new(1);
        for i in 0..(CAP as u64 + 100) {
            ring.put(plot_row(i));
        }
        let mut rows = Vec::new();
        let next = ring.read(0, &mut rows);
        assert_eq!(next, CAP as u64 + 100);
        // The row after the head may be half written by the time a reader looks: dropped.
        assert_eq!(rows.len(), CAP - 1);
        assert!(rows.windows(2).all(|it| it[1].0 == it[0].0 + 1));
        assert!(rows.iter().all(|(i, row)| row[0] == *i && row[7] == *i));
        rows.clear();
        assert_eq!(ring.read(next, &mut rows), next);
        assert!(rows.is_empty(), "a cursor at the head takes nothing");
    }

    #[test]
    fn a_ring_read_while_written_has_no_torn_row() {
        let ring: &'static Ring = Box::leak(Box::new(Ring::new(1)));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let writer = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut i = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    ring.put(plot_row(i));
                    i += 1;
                }
            })
        };
        let mut rows = Vec::new();
        for _ in 0..200 {
            rows.clear();
            ring.read(0, &mut rows);
            for (_, row) in &rows {
                assert!(row.iter().all(|it| *it == row[0] || *it == KIND_PLOT << 62), "a row's words are of one write");
            }
        }
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
    }

    #[test]
    fn spans_entered_and_left_out_of_turn_leave_the_right_ones_open() {
        let (a, b, c) = (11, 22, 33);
        std::thread::spawn(move || {
            entered(a);
            entered(b);
            entered(c);
            exited(b);
            let ring = ring().unwrap();
            assert_eq!(ring.entered_now(), (vec![a, c], 2));
            exited(c);
            exited(a);
            assert_eq!(ring.entered_now(), (vec![], 0));
        })
        .join()
        .unwrap();
    }

    #[test]
    fn a_thread_that_ended_gives_its_ring_to_the_next() {
        let first = std::thread::spawn(|| ring().unwrap().number).join().unwrap();
        let rings = lock(&RINGS).len();
        let second = std::thread::spawn(|| ring().unwrap().number).join().unwrap();
        // Another test's thread may have taken the ring in between; then a new one was made.
        assert!(second == first || lock(&RINGS).len() > rings);
    }

    #[test]
    fn a_name_is_one_number_and_reads_back() {
        let a = intern("blaze_profile::flight::tests");
        assert_ne!(a, 0);
        assert_eq!(intern("blaze_profile::flight::tests"), a);
        assert_eq!(name(a), Some("blaze_profile::flight::tests"));
        assert_ne!(intern("another name"), a);
        assert_eq!(name(0), None);
    }

    #[test]
    fn a_span_is_a_row_with_its_parent_depth_fields_and_build() {
        crate::install();
        let thread = std::thread::spawn(|| {
            let _build = building(7);
            let outer = tracing::info_span!(target: "blaze", "flight outer", krate = "app").entered();
            let outer_id = outer.id().unwrap().into_u64();
            {
                let _inner =
                    tracing::info_span!(target: "blaze", "flight inner", function = "app::main", unit = 3).entered();
            }
            drop(outer);
            (crate::sampler::os_tid() as u32, outer_id)
        });
        let mut cursor = Cursor::default();
        let mut read = Read::default();
        let (tid, outer_id) = thread.join().unwrap();
        self::read(&mut cursor, &mut read);
        let ours: Vec<&SpanRow> = read.spans.iter().filter(|it| it.thread == tid).collect();
        let name_of = |row: &SpanRow| meta(row.site).map(|it| it.name());
        let inner = ours.iter().find(|it| name_of(it) == Some("flight inner")).expect("the inner span is a row");
        let outer = ours.iter().find(|it| name_of(it) == Some("flight outer")).expect("the outer span is a row");
        assert_eq!(outer.span_id, outer_id);
        assert_eq!((inner.parent, inner.depth, outer.depth), (outer_id, 1, 0));
        assert_eq!((inner.build, outer.build), (7, 7));
        assert_eq!(name(outer.krate), Some("app"));
        assert_eq!((name(inner.function), name(inner.unit)), (Some("app::main"), Some("3")));
        assert!(inner.start_ns >= outer.start_ns && inner.start_ns + inner.dur_ns <= outer.start_ns + outer.dur_ns);
        read.clear();
        self::read(&mut cursor, &mut read);
        assert!(read.spans.iter().all(|it| it.thread != tid), "a cursor takes a row once");
    }

    #[test]
    fn a_span_closed_on_another_thread_keeps_where_it_began() {
        crate::install();
        let (span, tid) = std::thread::spawn(|| {
            (tracing::info_span!(target: "blaze", "flight moved"), crate::sampler::os_tid() as u32)
        })
        .join()
        .unwrap();
        let id = span.id().unwrap().into_u64();
        drop(span);
        let mut read = Read::default();
        window(Duration::from_secs(60), &mut read);
        let row = read.spans.iter().find(|it| it.span_id == id).expect("the span is a row");
        assert_eq!(meta(row.site).map(|it| it.name()), Some("flight moved"));
        assert!(row.dur_ns >= 0);
        // Its thread ended, so the ring that made it names that thread or the next to take it.
        assert!(row.thread == tid || row.thread != 0);
    }

    #[test]
    fn a_fiber_stack_row_reads_as_four_counters_of_its_fiber_and_job() {
        crate::install();
        let job = u64::MAX - 5;
        std::thread::spawn(move || {
            let _build = building(9);
            fiber_stack(FiberStack {
                fiber: u32::MAX - 1,
                job,
                reserved: 16 << 20,
                deepest: 300 << 10,
                touched: 320 << 10,
                grown: 1 << 20,
            });
            plot(Plot::Queued, 1);
        })
        .join()
        .unwrap();
        let mut read = Read::default();
        window(Duration::from_secs(60), &mut read);
        let ours: Vec<(Plot, u64)> = read
            .counters
            .iter()
            .filter(|it| it.fiber == Some((u32::MAX - 1, job)))
            .inspect(|it| assert_eq!(it.build, 9))
            .map(|it| (it.plot, it.value))
            .collect();
        assert_eq!(
            ours,
            [
                (Plot::FiberStackReserved, 16 << 20),
                (Plot::FiberStackDeepest, 300 << 10),
                (Plot::FiberStackTouched, 320 << 10),
                (Plot::FiberStackGrown, 1 << 20),
            ]
        );
        assert!(
            read.counters.iter().filter(|it| it.plot == Plot::Queued && it.build == 9).all(|it| it.fiber.is_none()),
            "a plot is of no fiber"
        );
    }
}

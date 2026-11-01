//! The flight recorder: always on, in every blaze process, at a cost of a few nanoseconds a
//! span. Each thread keeps the last [`CAP`] info-level spans it closed and the plot points it
//! gave ([`plot`]) in a ring of its own, made once when the thread first records (so no
//! allocation per span and no I/O), plus the spans it is inside now. [`dump`] writes what the
//! rings hold as the Chrome trace `BLAZE_PROFILE` writes, with the CPU samples of
//! [`crate::sampler`]; [`progress`] and [`threads_now`] are what the stall watchdog reads.
//!
//! A ring is written by its thread alone and read by any: the writer publishes each entry by
//! advancing `head` with release, after a release fence that orders the previous `head` before
//! the entry's words, so a reader that copied an entry the writer was overwriting sees, after
//! its own acquire fence, a `head` that tells it so, and drops that entry.

use std::{
    cell::Cell,
    sync::{
        Mutex, MutexGuard, OnceLock,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering, fence},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use rustc_hash::FxHashSet;

use crate::{Counter, Event, Part, clock};

/// Entries a thread keeps: 16 bytes each, so 512 KiB a thread.
pub const CAP: usize = 1 << 15;
/// Spans a thread is inside that a stall names; deeper ones are counted, not named.
const OPEN_MAX: usize = 32;

/// A span id holds its callsite in the low [`SITE_BITS`] and the low 52 bits of the tick it
/// began at above them: nothing is kept per open span.
pub(crate) const SITE_BITS: u32 = 12;
const TICK_MASK: u64 = (1 << (64 - SITE_BITS)) - 1;

const KIND_SPAN: u64 = 1;
const KIND_PLOT: u64 = 2;
const KIND_MARK: u64 = 3;
const DUR_MASK: u64 = (1 << 48) - 1;
const VALUE_MASK: u64 = (1 << 56) - 1;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|it| it.into_inner())
}

// --- Callsites --------------------------------------------------------------------------

/// Slots of the callsite table; a site is its slot plus one, so 0 is no site.
const SLOTS: usize = (1 << SITE_BITS) - 1;
/// The table is kept at most half full, so a lookup probes one or two slots.
const SITES_MAX: usize = SLOTS / 2;

type Meta = tracing::Metadata<'static>;

/// The callsites recorded, open-addressed by the address of their metadata: written under
/// [`TAKEN`] as a callsite registers, read with no lock by every span.
static SITES: [OnceLock<&'static Meta>; SLOTS] = [const { OnceLock::new() }; SLOTS];
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
            Some(it) if std::ptr::addr_eq(*it, meta) => return Some(at as u16 + 1),
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
    _ = SITES[at].set(meta);
    *taken += 1;
    Some(at as u16 + 1)
}

pub(crate) fn meta(site: u16) -> Option<&'static Meta> {
    SITES.get(usize::from(site).checked_sub(1)?)?.get().copied()
}

/// The id of a span of `site` beginning now.
#[inline]
pub(crate) fn id(site: u16) -> u64 {
    ((clock::ticks() & TICK_MASK) << SITE_BITS | u64::from(site)).max(1 << SITE_BITS)
}

pub(crate) fn site_of(id: u64) -> u16 {
    (id & ((1 << SITE_BITS) - 1)) as u16
}

/// The ticks from the beginning of the span `id` to `now`.
pub(crate) fn ticks_since(id: u64, now: u64) -> u64 {
    (now & TICK_MASK).wrapping_sub(id >> SITE_BITS) & TICK_MASK
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
}

impl Plot {
    pub const ALL: [Plot; 7] = [
        Plot::MemoHits,
        Plot::MemoMisses,
        Plot::Joined,
        Plot::Queued,
        Plot::Running,
        Plot::FrameBytes,
        Plot::LiveBytes,
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
        }
    }

    fn of(at: u64) -> Option<Plot> {
        Plot::ALL.get(usize::try_from(at).ok()?).copied()
    }
}

/// Notes `value` of `which` now, in this thread's ring.
#[inline]
pub fn plot(which: Plot, value: u64) {
    if let Some(ring) = ring() {
        ring.put(clock::ticks(), KIND_PLOT << 62 | (which as u64) << 56 | value.min(VALUE_MASK));
    }
}

/// Notes the end of the build `serial`: a mark on the timeline.
pub(crate) fn mark(serial: u64) {
    if let Some(ring) = ring() {
        ring.put(clock::ticks(), KIND_MARK << 62 | serial & ((1 << 62) - 1));
    }
}

// --- Rings ------------------------------------------------------------------------------

/// Who wrote a ring from entry `from` on: rings outlive threads and go to the next thread.
struct Owner {
    from: u64,
    tid: u64,
    name: String,
}

pub(crate) struct Ring {
    /// Entries ever written.
    head: AtomicU64,
    /// Spans ever closed: what [`progress`] sums.
    spans: AtomicU64,
    /// Spans the thread is inside, the first [`OPEN_MAX`] of them in `open`.
    depth: AtomicU32,
    open: [AtomicU64; OPEN_MAX],
    /// Whether a live thread holds it.
    live: AtomicBool,
    /// Two words an entry: the tick it ended (a span) or was given at, and its kind and value.
    words: Box<[AtomicU64]>,
    owners: Mutex<Vec<Owner>>,
    /// What the sampler needs of the thread, under the lock the thread takes to leave.
    pub(crate) sampled: Mutex<crate::sampler::Thread>,
}

impl Ring {
    fn new() -> Ring {
        // Zeroed by the allocator, not written: a page of the ring is committed only once
        // entries reach it, so a thread that records little holds little.
        // alloc: once per thread that records, its ring for the process's life
        let words = Box::<[AtomicU64]>::new_zeroed_slice(2 * CAP);
        // SAFETY: all-zero bytes are a valid `AtomicU64`.
        let words = unsafe { words.assume_init() };
        Ring {
            head: AtomicU64::new(0),
            spans: AtomicU64::new(0),
            depth: AtomicU32::new(0),
            open: [const { AtomicU64::new(0) }; OPEN_MAX],
            live: AtomicBool::new(true),
            words,
            // alloc: once per thread, as it first records
            owners: Mutex::new(Vec::new()),
            sampled: Mutex::new(crate::sampler::Thread::default()),
        }
    }

    #[inline]
    fn put(&self, at: u64, word: u64) {
        let i = self.head.load(Ordering::Relaxed);
        let slot = (i as usize & (CAP - 1)) * 2;
        fence(Ordering::Release);
        self.words[slot].store(at, Ordering::Relaxed);
        self.words[slot + 1].store(word, Ordering::Relaxed);
        self.head.store(i + 1, Ordering::Release);
    }

    /// The entries that are whole now, each with its index.
    fn read(&self) -> Vec<(u64, u64, u64)> {
        let head = self.head.load(Ordering::Acquire);
        let from = head.saturating_sub(CAP as u64);
        // alloc: a dump's copy of a ring, on demand
        let mut entries = Vec::with_capacity((head - from) as usize);
        for i in from..head {
            let slot = (i as usize & (CAP - 1)) * 2;
            let (at, word) = (
                self.words[slot].load(Ordering::Relaxed),
                self.words[slot + 1].load(Ordering::Relaxed),
            );
            entries.push((i, at, word));
        }
        fence(Ordering::Acquire);
        let whole_from = (self.head.load(Ordering::Relaxed) + 1).saturating_sub(CAP as u64);
        entries.retain(|it| it.0 >= whole_from);
        entries
    }

    /// The span ids the thread is inside, outermost first, and how many there are.
    fn open_now(&self) -> (Vec<u64>, u32) {
        let depth = self.depth.load(Ordering::Acquire);
        let named = (depth as usize).min(OPEN_MAX);
        // alloc: a dump, on demand
        (self.open[..named].iter().map(|it| it.load(Ordering::Relaxed)).collect(), depth)
    }
}

/// Every ring, in the order made.
// alloc: once a process
static RINGS: Mutex<Vec<&'static Ring>> = Mutex::new(Vec::new());

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
}

#[inline]
pub(crate) fn ring() -> Option<&'static Ring> {
    match MINE.with(Cell::get) {
        Some(ring) => Some(ring),
        None => claim(),
    }
}

/// Takes a ring a thread left, or makes one, for this thread. `None` once the thread is
/// ending past the point its ring was given back.
#[cold]
fn claim() -> Option<&'static Ring> {
    HELD.try_with(|held| {
        let tid = crate::sampler::os_tid();
        let current = std::thread::current();
        // alloc: once per thread, its name
        let name = current.name().map_or_else(|| format!("thread {tid}"), str::to_owned);
        let mut rings = lock(&RINGS);
        let left = rings.iter().copied().find(|it| {
            it.live.compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed).is_ok()
        });
        let ring: &'static Ring = match left {
            Some(ring) => ring,
            None => {
                // alloc: once per thread past those that ended, kept for the process's life
                let ring = Box::leak(Box::new(Ring::new()));
                rings.push(ring);
                ring
            }
        };
        drop(rings);
        let head = ring.head.load(Ordering::Relaxed);
        let mut owners = lock(&ring.owners);
        owners.push(Owner { from: head, tid, name });
        // An owner whose entries were all written over names none.
        let oldest = head.saturating_sub(CAP as u64);
        while owners.len() > 1 && owners[1].from <= oldest {
            owners.remove(0);
        }
        drop(owners);
        ring.depth.store(0, Ordering::Relaxed);
        lock(&ring.sampled).enter(tid);
        held.0.set(Some(ring));
        MINE.with(|it| it.set(Some(ring)));
        ring
    })
    .ok()
}

// --- What spans do ----------------------------------------------------------------------

#[inline]
pub(crate) fn entered(id: u64) {
    let Some(ring) = ring() else { return };
    let depth = ring.depth.load(Ordering::Relaxed);
    if let Some(slot) = ring.open.get(depth as usize) {
        slot.store(id, Ordering::Relaxed);
    }
    ring.depth.store(depth + 1, Ordering::Release);
}

#[inline]
pub(crate) fn exited(id: u64) {
    let Some(ring) = ring() else { return };
    let depth = ring.depth.load(Ordering::Relaxed);
    let Some(top) = depth.checked_sub(1) else { return };
    let named = (depth as usize).min(OPEN_MAX);
    // Spans leave in the order they entered but for a guard dropped out of turn: then the
    // ones above it move down.
    if (top as usize) < OPEN_MAX && ring.open[top as usize].load(Ordering::Relaxed) != id {
        if let Some(at) = ring.open[..named].iter().rposition(|it| it.load(Ordering::Relaxed) == id)
        {
            for i in at..named - 1 {
                ring.open[i].store(ring.open[i + 1].load(Ordering::Relaxed), Ordering::Relaxed);
            }
        } else {
            return;
        }
    }
    ring.depth.store(top, Ordering::Release);
}

/// The span `id` closed now, on this thread.
#[inline]
pub(crate) fn closed(id: u64) {
    let Some(ring) = ring() else { return };
    let now = clock::ticks();
    let dur = ticks_since(id, now).min(DUR_MASK);
    ring.put(now, KIND_SPAN << 62 | u64::from(site_of(id)) << 48 | dur);
    ring.spans.store(ring.spans.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
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

// --- Reading ----------------------------------------------------------------------------

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
        let (ids, depth) = ring.open_now();
        if depth == 0 {
            continue;
        }
        let open = ids
            .iter()
            .filter_map(|&id| {
                let meta = meta(site_of(id))?;
                Some(OpenSpan {
                    name: meta.name(),
                    target: meta.target(),
                    for_ns: scale.ns(ticks_since(id, now)),
                })
            })
            // alloc: a dump, on demand
            .collect();
        let owners = lock(&ring.owners);
        // clone: a dump, on demand
        let (tid, name) = owners.last().map_or((0, String::new()), |it| (it.tid, it.name.clone()));
        drop(owners);
        threads.push(ThreadNow { tid, name, open, deeper: depth.saturating_sub(OPEN_MAX as u32) });
    }
    threads
}

/// Microseconds since the Unix epoch of instants, from one reading of both clocks.
pub(crate) struct Epoch {
    now: Instant,
    now_us: u64,
}

impl Epoch {
    pub(crate) fn now() -> Epoch {
        let now_us =
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_micros() as u64;
        Epoch { now: Instant::now(), now_us }
    }

    pub(crate) fn us(&self, at: Instant) -> u64 {
        self.now_us.saturating_sub(self.now.saturating_duration_since(at).as_micros() as u64)
    }
}

/// What every ring holds of the last `window`, as one process's part of a profile.
pub(crate) fn part(window: Duration) -> Part {
    let scale = clock::Scale::now();
    let epoch = Epoch::now();
    let since = epoch.now.checked_sub(window).unwrap_or(epoch.now);
    // clone: the list of rings, so none is read under the lock threads take to start
    let rings = lock(&RINGS).clone();
    let mut part = Part { pid: std::process::id(), command: crate::command(), ..Part::default() };
    let mut named: FxHashSet<u64> = FxHashSet::default();
    for ring in rings {
        let entries = ring.read();
        let owners = lock(&ring.owners);
        let mut owner = 0;
        for (i, at, word) in entries {
            while owners.get(owner + 1).is_some_and(|it| it.from <= i) {
                owner += 1;
            }
            let Some(by) = owners.get(owner) else { continue };
            let end = scale.instant(at);
            if end < since {
                continue;
            }
            if named.insert(by.tid) {
                // clone: a dump, on demand
                part.threads.push((by.tid, by.name.clone()));
            }
            match word >> 62 {
                KIND_SPAN => {
                    let site = ((word >> 48) & ((1 << SITE_BITS) - 1)) as u16;
                    let Some(meta) = meta(site) else { continue };
                    let dur_ns = scale.ns(word & DUR_MASK);
                    let start = end.checked_sub(Duration::from_nanos(dur_ns)).unwrap_or(end);
                    part.events.push(Event {
                        // alloc: a dump, on demand
                        name: meta.name().to_owned(),
                        subject: None,
                        tid: by.tid,
                        start_us: epoch.us(start),
                        dur_us: dur_ns / 1000,
                        cpu_us: None,
                    });
                }
                KIND_PLOT => {
                    let Some(plot) = Plot::of((word >> 56) & 0x3f) else { continue };
                    part.counters.push(Counter {
                        // alloc: a dump, on demand
                        name: plot.name().to_owned(),
                        tid: by.tid,
                        at_us: epoch.us(end),
                        value: word & VALUE_MASK,
                    });
                }
                KIND_MARK => part.events.push(Event {
                    // alloc: a dump, on demand
                    name: format!("build {} ended", word & ((1 << 62) - 1)),
                    subject: None,
                    tid: by.tid,
                    start_us: epoch.us(end),
                    dur_us: 0,
                    cpu_us: None,
                }),
                _ => {}
            }
        }
    }
    let (samples, sampler) = crate::sampler::taken(since, &epoch);
    part.samples = samples;
    part.sampler = sampler;
    part
}

/// The last `window` of every thread's ring and of the CPU samples, as the Chrome trace
/// (ui.perfetto.dev) `BLAZE_PROFILE` writes: what `blaze debug profile` and a stall write.
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

    #[test]
    fn a_ring_keeps_the_last_entries_whole_and_in_order() {
        let ring = Ring::new();
        for i in 0..(CAP as u64 + 100) {
            ring.put(i, KIND_PLOT << 62 | i);
        }
        let entries = ring.read();
        // The entry after the head may be half written by the time a reader looks: dropped.
        assert_eq!(entries.len(), CAP - 1);
        assert!(entries.windows(2).all(|it| it[1].0 == it[0].0 + 1));
        assert!(entries.iter().all(|&(i, at, word)| at == i && word & VALUE_MASK == i));
        assert_eq!(entries.last().map(|it| it.0), Some(CAP as u64 + 99));
    }

    #[test]
    fn a_ring_read_while_written_has_no_torn_entry() {
        let ring: &'static Ring = Box::leak(Box::new(Ring::new()));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let writer = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut i = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    ring.put(i, KIND_PLOT << 62 | (i & VALUE_MASK));
                    i += 1;
                }
            })
        };
        for _ in 0..200 {
            for (_, at, word) in ring.read() {
                assert_eq!(
                    at & VALUE_MASK,
                    word & VALUE_MASK,
                    "an entry's two words are of one write"
                );
            }
        }
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
    }

    #[test]
    fn spans_entered_and_left_out_of_turn_leave_the_right_ones_open() {
        let (a, b, c) = (11 << SITE_BITS, 22 << SITE_BITS, 33 << SITE_BITS);
        std::thread::spawn(move || {
            entered(a);
            entered(b);
            entered(c);
            exited(b);
            let ring = ring().unwrap();
            assert_eq!(ring.open_now(), (vec![a, c], 2));
            exited(c);
            exited(a);
            assert_eq!(ring.open_now(), (vec![], 0));
        })
        .join()
        .unwrap();
    }

    #[test]
    fn a_thread_that_ended_gives_its_ring_to_the_next() {
        let first =
            std::thread::spawn(|| std::ptr::from_ref(ring().unwrap()).addr()).join().unwrap();
        let rings = lock(&RINGS).len();
        let second =
            std::thread::spawn(|| std::ptr::from_ref(ring().unwrap()).addr()).join().unwrap();
        // Another test's thread may have taken the ring in between; then a new one was made.
        assert!(second == first || lock(&RINGS).len() > rings);
    }
}

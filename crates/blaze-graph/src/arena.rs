//! Where a task's memory is measured and the daemon's is capped (docs/v2/plan/lanes/core.md section 4,
//! "Memory-budget admission" and "Hard backstop").
//!
//! [`Counting`] is the daemon's global allocator: it counts the bytes the process holds
//! ([`held`]) and, per thread, the bytes the task the thread runs now holds, so [`measure`]
//! says what a task held at most. [`Bump`] is an arena for results built before they are
//! written into the zero-copy format. [`Cap`] is the daemon's limit on its own arenas: past
//! `spill_at` it asks for cold results to go to disk, and a reserve that would pass the limit
//! waits. [`machine_budget`] is the memory the plane may grant: the machine's limit, less what
//! others use in blaze's own cgroup and a margin.

use std::{
    alloc::{GlobalAlloc, Layout},
    cell::{Cell, RefCell},
    fmt, io,
    path::Path,
    ptr::NonNull,
    sync::{
        Arc, Condvar, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicI64, AtomicU32, Ordering},
    },
    thread::ThreadId,
};

use rustc_hash::FxHashMap;

use crate::{Budget, Cancelled, admission::Arena};

/// How many counters the process's bytes are spread over, so threads seldom share one.
const SHARDS: usize = 64;

#[repr(align(128))]
struct Shard(AtomicI64);

/// The bytes allocated less those freed through [`Counting`], spread over shards; their sum
/// is what the process holds.
static HELD: [Shard; SHARDS] = [const { Shard(AtomicI64::new(0)) }; SHARDS];
static NEXT_SHARD: AtomicU32 = AtomicU32::new(0);

// `const` and without destructors: reading them allocates nothing and works while a thread
// ends, as an allocator must.
thread_local! {
    static SHARD: Cell<u32> = const { Cell::new(u32::MAX) };
    /// The bytes this thread allocated less those it freed, through [`Counting`].
    static NET: Cell<i64> = const { Cell::new(0) };
    /// The most [`NET`] was since the innermost [`measure`] on this thread began.
    static PEAK: Cell<i64> = const { Cell::new(0) };
}

fn count(delta: i64) {
    _ = NET.try_with(|net| {
        let now = net.get().wrapping_add(delta);
        net.set(now);
        _ = PEAK.try_with(|peak| {
            if now > peak.get() {
                peak.set(now);
            }
        });
    });
    let shard = SHARD
        .try_with(|shard| {
            if shard.get() == u32::MAX {
                shard.set(NEXT_SHARD.fetch_add(1, Ordering::Relaxed) % SHARDS as u32);
            }
            shard.get()
        })
        .unwrap_or(0);
    HELD[shard as usize].0.fetch_add(delta, Ordering::Relaxed);
}

/// A global allocator that counts what goes through `A`: the daemon's binary installs it
/// (`#[global_allocator] static ALLOC: Counting<System> = Counting(System);`). It takes no lock
/// and allocates nothing of its own.
///
/// A byte freed on another thread than the one that allocated it counts against the thread
/// that frees it: [`measure`] sees what a task's thread allocated and freed, [`held`] is exact.
pub struct Counting<A>(pub A);

fn signed(bytes: usize) -> i64 {
    // A layout's size is at most `isize::MAX`.
    bytes as i64
}

// SAFETY: every call goes to `A` with the caller's arguments, so `A`'s guarantees hold; the
// counting beside it touches only atomics and `const` thread-locals with no destructor.
unsafe impl<A: GlobalAlloc> GlobalAlloc for Counting<A> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller upholds `alloc`'s contract for `layout`.
        let ptr = unsafe { self.0.alloc(layout) };
        if !ptr.is_null() {
            count(signed(layout.size()));
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller upholds `alloc_zeroed`'s contract for `layout`.
        let ptr = unsafe { self.0.alloc_zeroed(layout) };
        if !ptr.is_null() {
            count(signed(layout.size()));
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller says `ptr` was allocated by this allocator with `layout`.
        unsafe { self.0.dealloc(ptr, layout) };
        count(-signed(layout.size()));
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller upholds `realloc`'s contract for `ptr`, `layout` and `new_size`.
        let new = unsafe { self.0.realloc(ptr, layout, new_size) };
        if !new.is_null() {
            count(signed(new_size) - signed(layout.size()));
        }
        new
    }
}

/// Runs `f` and says the most bytes it held at once above what its thread held when it began,
/// what it freed counted against what it allocated. Measures nest: an inner one's peak is part
/// of the outer one's. 0 when [`Counting`] is not the global allocator.
pub fn measure<R>(f: impl FnOnce() -> R) -> (R, u64) {
    struct Restore {
        outer: i64,
    }
    impl Drop for Restore {
        // On a panic too: the outer measure still sees what `f` held.
        fn drop(&mut self) {
            let inner = PEAK.get();
            PEAK.set(self.outer.max(inner));
        }
    }
    let base = NET.get();
    let restore = Restore { outer: PEAK.replace(base) };
    let result = f();
    let peak = PEAK.get();
    drop(restore);
    (result, u64::try_from(peak.saturating_sub(base)).unwrap_or(0))
}

/// The bytes the process holds now through [`Counting`]; 0 when it is not the global
/// allocator.
pub fn held() -> u64 {
    let sum: i64 = HELD.iter().map(|it| it.0.load(Ordering::Relaxed)).sum();
    u64::try_from(sum).unwrap_or(0)
}

/// The first chunk of a [`Bump`], and the most a chunk grows to but for one value bigger.
const FIRST_CHUNK: usize = 64 << 10;
const LAST_CHUNK: usize = 16 << 20;
const CHUNK_ALIGN: usize = 16;

struct Chunk {
    ptr: NonNull<u8>,
    layout: Layout,
    /// The [`Cap`]'s bytes it holds, given back when it is freed.
    _grant: Option<Grant>,
}

/// A bump arena: values are put one after another in chunks from the global allocator, and
/// all are freed together when it is dropped or [`Bump::reset`]. No value's destructor runs,
/// so it takes only `Copy` values. Under a [`Cap`], each chunk is reserved from it first.
pub struct Bump {
    chunks: RefCell<Vec<Chunk>>,
    /// Where the next value may go in the last chunk, and that chunk's end, as addresses.
    next: Cell<usize>,
    end: Cell<usize>,
    held: Cell<u64>,
    peak: Cell<u64>,
    cap: Option<Cap>,
}

// SAFETY: a `Bump` owns its chunks, and every value in them is borrowed from it, so moving it
// to another thread moves nothing another thread can reach.
unsafe impl Send for Bump {}

impl Default for Bump {
    fn default() -> Bump {
        Bump::new()
    }
}

impl Bump {
    pub fn new() -> Bump {
        Bump::with(None)
    }

    /// An arena whose chunks are reserved from `cap`: taking a chunk waits while the cap is
    /// full, and is [`Cancelled`] when every holder of the cap waits.
    pub fn under(cap: Cap) -> Bump {
        Bump::with(Some(cap))
    }

    fn with(cap: Option<Cap>) -> Bump {
        Bump {
            chunks: RefCell::new(Vec::new()),
            next: Cell::new(0),
            end: Cell::new(0),
            held: Cell::new(0),
            peak: Cell::new(0),
            cap,
        }
    }

    /// Room for a value of `layout`, valid until the arena is reset or dropped.
    pub fn alloc_layout(&self, layout: Layout) -> Result<NonNull<u8>, Cancelled> {
        if let Some(at) = self.fit(layout) {
            return Ok(at);
        }
        self.grow(layout)?;
        Ok(self.fit(layout).expect("a new chunk has room for the value it was made for"))
    }

    #[allow(clippy::mut_from_ref)]
    pub fn alloc<T: Copy>(&self, value: T) -> Result<&mut T, Cancelled> {
        let ptr = self.alloc_layout(Layout::new::<T>())?.cast::<T>();
        // SAFETY: the room is fresh, aligned and big enough for a `T`, and no other reference
        // to it is given out while `self` is borrowed.
        unsafe {
            ptr.as_ptr().write(value);
            Ok(&mut *ptr.as_ptr())
        }
    }

    #[allow(clippy::mut_from_ref)]
    pub fn alloc_slice<T: Copy>(&self, values: &[T]) -> Result<&mut [T], Cancelled> {
        let layout = Layout::array::<T>(values.len()).expect("a slice's layout fits");
        let ptr = self.alloc_layout(layout)?.cast::<T>();
        // SAFETY: the room is fresh, aligned and big enough for `values.len()` of `T`, which
        // cannot overlap `values`, and no other reference to it is given out.
        unsafe {
            std::ptr::copy_nonoverlapping(values.as_ptr(), ptr.as_ptr(), values.len());
            Ok(std::slice::from_raw_parts_mut(ptr.as_ptr(), values.len()))
        }
    }

    #[allow(clippy::mut_from_ref)]
    pub fn alloc_str(&self, text: &str) -> Result<&mut str, Cancelled> {
        let bytes = self.alloc_slice(text.as_bytes())?;
        // SAFETY: the bytes are a copy of a `str`'s.
        Ok(unsafe { std::str::from_utf8_unchecked_mut(bytes) })
    }

    /// Frees every chunk, and every value with them; what it held at most is kept.
    pub fn reset(&mut self) {
        self.free();
        self.next.set(0);
        self.end.set(0);
        self.held.set(0);
    }

    fn fit(&self, layout: Layout) -> Option<NonNull<u8>> {
        let (next, end) = (self.next.get(), self.end.get());
        if end == 0 {
            return None;
        }
        let at = next.checked_next_multiple_of(layout.align())?;
        let after = at.checked_add(layout.size())?;
        if after > end {
            return None;
        }
        self.next.set(after);
        let chunks = self.chunks.borrow();
        let chunk = chunks.last().expect("a chunk when `end` is set");
        // SAFETY: `at` is within the last chunk (`next <= at <= after <= end`), so the offset
        // stays in its allocation.
        let ptr = unsafe { chunk.ptr.as_ptr().add(at - chunk.ptr.as_ptr() as usize) };
        NonNull::new(ptr)
    }

    fn grow(&self, layout: Layout) -> Result<(), Cancelled> {
        let last = self.chunks.borrow().last().map_or(0, |it| it.layout.size());
        let wanted = layout.size().checked_add(layout.align()).expect("a value's size fits");
        let size = (last * 2).clamp(FIRST_CHUNK, LAST_CHUNK).max(wanted);
        let chunk_layout = Layout::from_size_align(size, CHUNK_ALIGN.max(layout.align()))
            .expect("a chunk's layout fits");
        let grant = match &self.cap {
            Some(cap) => Some(cap.reserve(size as u64)?),
            None => None,
        };
        // SAFETY: the layout's size is not zero.
        let ptr = unsafe { std::alloc::alloc(chunk_layout) };
        let Some(ptr) = NonNull::new(ptr) else { std::alloc::handle_alloc_error(chunk_layout) };
        self.chunks.borrow_mut().push(Chunk { ptr, layout: chunk_layout, _grant: grant });
        self.next.set(ptr.as_ptr() as usize);
        self.end.set(ptr.as_ptr() as usize + size);
        let held = self.held.get() + size as u64;
        self.held.set(held);
        self.peak.set(self.peak.get().max(held));
        Ok(())
    }

    fn free(&mut self) {
        for chunk in self.chunks.get_mut().drain(..) {
            // SAFETY: the chunk was allocated by the global allocator with this layout, and
            // the values in it are borrowed from `self`, which is borrowed mutably here.
            unsafe { std::alloc::dealloc(chunk.ptr.as_ptr(), chunk.layout) };
        }
    }
}

impl Drop for Bump {
    fn drop(&mut self) {
        self.free();
    }
}

impl Arena for Bump {
    /// The most bytes of chunks it held at once.
    fn peak(&self) -> u64 {
        self.peak.get()
    }

    /// The bytes of chunks it holds: what it took from the allocator, used or not.
    fn held(&self) -> u64 {
        self.held.get()
    }
}

/// The daemon's cap on its own arenas (docs/v2/plan/lanes/core.md, "Hard backstop"). Bytes are reserved
/// before they are allocated and given back when the [`Grant`] is dropped. Past `spill_at`,
/// `spill` is called with the bytes over it, once each time it is passed, so the query
/// database moves cold values to the store; a reserve that would pass `limit` waits until
/// enough is given back. A holder is the thread that reserved; when every thread holding
/// bytes is itself waiting in [`Cap::reserve`], nothing can be given back, and the newest of
/// those waits ends in [`Cancelled`], so its task unwinds and gives its bytes back.
///
/// A waiter is let in as soon as its bytes fit, whoever waited longer.
#[derive(Clone)]
pub struct Cap(Arc<Shared>);

struct Shared {
    limit: u64,
    spill_at: u64,
    spill: Box<dyn Fn(u64) + Send + Sync>,
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Default)]
struct State {
    held: u64,
    /// Whether `held` is over `spill_at` since `spill` was last called.
    above: bool,
    /// The bytes of each thread's grants that are not given back; none of 0.
    by_holder: FxHashMap<ThreadId, u64>,
    waiters: Vec<Waiter>,
    next_wait: u64,
}

struct Waiter {
    /// Its order of coming: the newest is the greatest.
    order: u64,
    thread: ThreadId,
    bytes: u64,
    cancelled: bool,
}

impl State {
    /// When every holder waits and no waiter fits, cancels the newest waiter that holds bytes.
    /// Whether it cancelled one.
    fn unstick(&mut self, limit: u64) -> bool {
        let live = |thread: &ThreadId, waiters: &[Waiter]| {
            waiters.iter().any(|it| it.thread == *thread && !it.cancelled)
        };
        if self.by_holder.is_empty()
            || self.waiters.iter().any(|it| !it.cancelled && self.held + it.bytes <= limit)
            || !self.by_holder.keys().all(|it| live(it, self.waiters.as_slice()))
        {
            return false;
        }
        let by_holder = &self.by_holder;
        let victim = self
            .waiters
            .iter_mut()
            .filter(|it| !it.cancelled && by_holder.contains_key(&it.thread))
            .max_by_key(|it| it.order);
        match victim {
            Some(victim) => {
                victim.cancelled = true;
                true
            }
            None => false,
        }
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Cap {
    pub fn new(limit: u64, spill_at: u64, spill: Box<dyn Fn(u64) + Send + Sync>) -> Cap {
        Cap(Arc::new(Shared {
            limit,
            spill_at,
            spill,
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
        }))
    }

    pub fn limit(&self) -> u64 {
        self.0.limit
    }

    /// The bytes of grants not given back.
    pub fn held(&self) -> u64 {
        self.0.lock().held
    }

    /// How many reserves wait now.
    pub fn waiting(&self) -> usize {
        self.0.lock().waiters.iter().filter(|it| !it.cancelled).count()
    }

    /// `bytes` of the cap, waiting while they would pass its limit, or [`Cancelled`] when
    /// every holder waits (see [`Cap`]).
    ///
    /// # Panics
    ///
    /// When `bytes` alone are more than the limit: no wait would end.
    pub fn reserve(&self, bytes: u64) -> Result<Grant, Cancelled> {
        let shared = &*self.0;
        assert!(
            bytes <= shared.limit,
            "a reserve of {bytes} bytes, more than the daemon's cap of {} on its arenas",
            shared.limit
        );
        let me = std::thread::current().id();
        let mut state = shared.lock();
        let mut waiting: Option<u64> = None;
        loop {
            if let Some(order) = waiting
                && state.waiters.iter().any(|it| it.order == order && it.cancelled)
            {
                state.waiters.retain(|it| it.order != order);
                drop(state);
                shared.changed.notify_all();
                return Err(Cancelled);
            }
            if state.held + bytes <= shared.limit {
                if let Some(order) = waiting {
                    state.waiters.retain(|it| it.order != order);
                }
                state.held += bytes;
                if bytes > 0 {
                    *state.by_holder.entry(me).or_default() += bytes;
                }
                let over = (!state.above && state.held > shared.spill_at).then(|| {
                    state.above = true;
                    state.held - shared.spill_at
                });
                drop(state);
                if let Some(over) = over {
                    (shared.spill)(over);
                }
                return Ok(Grant { cap: Arc::clone(&self.0), bytes, holder: me });
            }
            if waiting.is_none() {
                let order = state.next_wait;
                state.next_wait += 1;
                state.waiters.push(Waiter { order, thread: me, bytes, cancelled: false });
                waiting = Some(order);
            }
            if state.unstick(shared.limit) {
                shared.changed.notify_all();
                continue;
            }
            state = shared.changed.wait(state).unwrap_or_else(PoisonError::into_inner);
        }
    }
}

impl fmt::Debug for Cap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cap")
            .field("limit", &self.0.limit)
            .field("spill_at", &self.0.spill_at)
            .field("held", &self.held())
            .finish()
    }
}

/// Bytes of a [`Cap`], given back when dropped.
pub struct Grant {
    cap: Arc<Shared>,
    bytes: u64,
    holder: ThreadId,
}

impl Grant {
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

impl fmt::Debug for Grant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Grant").field("bytes", &self.bytes).finish()
    }
}

impl Drop for Grant {
    fn drop(&mut self) {
        let shared = &*self.cap;
        let mut state = shared.lock();
        state.held -= self.bytes;
        if let Some(held) = state.by_holder.get_mut(&self.holder) {
            *held -= self.bytes;
            if *held == 0 {
                state.by_holder.remove(&self.holder);
            }
        }
        if state.held <= shared.spill_at {
            state.above = false;
        }
        state.unstick(shared.limit);
        drop(state);
        shared.changed.notify_all();
    }
}

/// The memory the plane may grant: the machine's limit, less what others use within it, less
/// `margin`.
///
/// The limit is the least of what the machine has and what the process may use: on Linux
/// `/proc/meminfo`'s `MemTotal`, the `memory.max` of the process's cgroup and of each one above
/// it (cgroup v2; v1's `memory.limit_in_bytes`), and `RLIMIT_AS`; on macOS `sysctl hw.memsize`
/// and `RLIMIT_AS`.
///
/// What others use is what is in use in blaze's own cgroup (v2's `memory.current` of the group
/// `/proc/self/cgroup` names; v1's `memory.usage_in_bytes` where the memory controller is
/// v1's) less what blaze itself holds ([`held`]; 0 when [`Counting`] is not the global
/// allocator, so the budget is then the smaller). It is not the machine's `MemTotal` less
/// `MemAvailable`: that counts every process outside the cgroup against a limit they do not
/// share, and a small cgroup on a busy machine would refuse builds that fit. The root cgroup
/// keeps no count, and nothing is taken off there; nor on macOS, which has no cgroups.
///
/// The budget's `from` names the limit and what was taken off. An error where one cannot be
/// read or nothing is left, never a guess.
pub fn machine_budget(margin: u64) -> io::Result<Budget> {
    let mut limits = machine_limits()?;
    #[cfg(unix)]
    limits.extend(rlimit_as()?.map(|bytes| (bytes, "the process's RLIMIT_AS".to_owned())));
    let in_use = machine_in_use(held())?;
    least(limits, in_use, margin)
}

#[cfg(target_os = "linux")]
fn machine_in_use(own: u64) -> io::Result<Option<(u64, String)>> {
    linux_in_use(Path::new("/"), own)
}

#[cfg(not(target_os = "linux"))]
fn machine_in_use(_own: u64) -> io::Result<Option<(u64, String)>> {
    Ok(None)
}

#[cfg(target_os = "linux")]
fn machine_limits() -> io::Result<Vec<(u64, String)>> {
    linux_limits(Path::new("/"))
}

#[cfg(target_os = "macos")]
fn machine_limits() -> io::Result<Vec<(u64, String)>> {
    let mut bytes: u64 = 0;
    let mut len = std::mem::size_of::<u64>();
    // SAFETY: the name is a C string, and `bytes` and `len` say a buffer of `len` bytes.
    let status = unsafe {
        libc::sysctlbyname(
            c"hw.memsize".as_ptr(),
            (&raw mut bytes).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if status != 0 {
        let why = io::Error::last_os_error();
        return Err(io::Error::new(why.kind(), format!("sysctl hw.memsize: {why}")));
    }
    if len != std::mem::size_of::<u64>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("sysctl hw.memsize gave {len} bytes, not 8"),
        ));
    }
    Ok(vec![(bytes, "sysctl hw.memsize".to_owned())])
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn machine_limits() -> io::Result<Vec<(u64, String)>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        format!(
            "blaze reads the machine's memory only on Linux and macOS, not {}",
            std::env::consts::OS
        ),
    ))
}

#[cfg(unix)]
fn rlimit_as() -> io::Result<Option<u64>> {
    let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: `limit` is a `rlimit` to write.
    if unsafe { libc::getrlimit(libc::RLIMIT_AS, &mut limit) } != 0 {
        let why = io::Error::last_os_error();
        return Err(io::Error::new(why.kind(), format!("getrlimit(RLIMIT_AS): {why}")));
    }
    let current: u64 = limit.rlim_cur;
    Ok((limit.rlim_cur != libc::RLIM_INFINITY).then_some(current))
}

fn invalid(path: &Path, why: impl fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("{}: {why}", path.display()))
}

fn read(path: &Path) -> io::Result<String> {
    std::fs::read_to_string(path)
        .map_err(|why| io::Error::new(why.kind(), format!("{}: {why}", path.display())))
}

/// The memory limits of a Linux machine whose file system is at `root`: `MemTotal`, and each
/// cgroup memory limit over the process.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn linux_limits(root: &Path) -> io::Result<Vec<(u64, String)>> {
    let meminfo = root.join("proc/meminfo");
    let text = read(&meminfo)?;
    let line = text
        .lines()
        .find_map(|it| it.strip_prefix("MemTotal:"))
        .ok_or_else(|| invalid(&meminfo, "no MemTotal line"))?;
    let kib = line
        .trim()
        .strip_suffix("kB")
        .and_then(|it| it.trim().parse::<u64>().ok())
        .ok_or_else(|| invalid(&meminfo, format!("MemTotal of `{}`", line.trim())))?;
    let total = kib.checked_mul(1024).ok_or_else(|| invalid(&meminfo, "MemTotal past 2^64"))?;
    let mut limits = vec![(total, "MemTotal of /proc/meminfo".to_owned())];
    let cgroup = root.join("proc/self/cgroup");
    for line in read(&cgroup)?.lines().filter(|it| !it.is_empty()) {
        let mut parts = line.splitn(3, ':');
        let (Some(hierarchy), Some(controllers), Some(path)) =
            (parts.next(), parts.next(), parts.next())
        else {
            return Err(invalid(&cgroup, format!("a line `{line}`")));
        };
        let (mount, file) = if hierarchy == "0" && controllers.is_empty() {
            ("sys/fs/cgroup", "memory.max")
        } else if controllers.split(',').any(|it| it == "memory") {
            ("sys/fs/cgroup/memory", "memory.limit_in_bytes")
        } else {
            continue;
        };
        let mut at = Some(path);
        while let Some(group) = at {
            let limit = root.join(mount).join(group.trim_start_matches('/')).join(file);
            match std::fs::read_to_string(&limit) {
                Ok(text) if text.trim() == "max" => {}
                Ok(text) => {
                    let bytes = text
                        .trim()
                        .parse::<u64>()
                        .map_err(|_| invalid(&limit, format!("a limit of `{}`", text.trim())))?;
                    limits.push((bytes, format!("{file} of cgroup {group}")));
                }
                // A cgroup has no limit file at the root of its hierarchy.
                Err(why) if why.kind() == io::ErrorKind::NotFound => {}
                Err(why) => {
                    return Err(io::Error::new(why.kind(), format!("{}: {why}", limit.display())));
                }
            }
            at = match group.rfind('/') {
                _ if group == "/" || group.is_empty() => None,
                Some(0) => Some("/"),
                Some(slash) => Some(&group[..slash]),
                None => None,
            };
        }
    }
    Ok(limits)
}

/// What others use in the cgroup of the process, on a Linux machine whose file system is at
/// `root`, when blaze itself holds `own`: the bytes and what says so. `None` in the root
/// cgroup, which keeps no count.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn linux_in_use(root: &Path, own: u64) -> io::Result<Option<(u64, String)>> {
    let cgroup = root.join("proc/self/cgroup");
    let (mut v1, mut v2) = (None, None);
    let text = read(&cgroup)?;
    for line in text.lines().filter(|it| !it.is_empty()) {
        let mut parts = line.splitn(3, ':');
        let (Some(hierarchy), Some(controllers), Some(path)) =
            (parts.next(), parts.next(), parts.next())
        else {
            return Err(invalid(&cgroup, format!("a line `{line}`")));
        };
        if hierarchy == "0" && controllers.is_empty() {
            v2 = Some(path);
        } else if controllers.split(',').any(|it| it == "memory") {
            v1 = Some(path);
        }
    }
    // Where the memory controller is v1's, the v2 hierarchy counts no memory.
    let (mount, file, group) = match (v1, v2) {
        (Some(group), _) => ("sys/fs/cgroup/memory", "memory.usage_in_bytes", group),
        (None, Some(group)) => ("sys/fs/cgroup", "memory.current", group),
        (None, None) => return Err(invalid(&cgroup, "no cgroup that counts memory")),
    };
    let count = root.join(mount).join(group.trim_start_matches('/')).join(file);
    let current = match std::fs::read_to_string(&count) {
        Ok(text) => text
            .trim()
            .parse::<u64>()
            .map_err(|_| invalid(&count, format!("a count of `{}`", text.trim())))?,
        Err(why) if why.kind() == io::ErrorKind::NotFound && group == "/" => return Ok(None),
        Err(why) => {
            return Err(io::Error::new(why.kind(), format!("{}: {why}", count.display())));
        }
    };
    let others = current.saturating_sub(own);
    let from = format!(
        "{} others use in cgroup {group} ({file} {} less blaze's own {})",
        gb(others),
        gb(current),
        gb(own)
    );
    Ok(Some((others, from)))
}

fn gb(bytes: u64) -> String {
    format!("{:.2} GB", bytes as f64 / f64::from(1 << 30))
}

/// The least of `limits`, less what others use (`in_use`) and `margin`, named by what each was.
fn least(
    limits: Vec<(u64, String)>,
    in_use: Option<(u64, String)>,
    margin: u64,
) -> io::Result<Budget> {
    let (bytes, from) = limits
        .into_iter()
        .reduce(|least, it| if it.0 < least.0 { it } else { least })
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no memory limit was read"))?;
    let others = in_use.as_ref().map_or(0, |it| it.0);
    let taken = match &in_use {
        Some((_, used)) => format!("{used} and a margin of {}", gb(margin)),
        None => format!("a margin of {}", gb(margin)),
    };
    match bytes.checked_sub(others).and_then(|it| it.checked_sub(margin)) {
        Some(left) if left > 0 => {
            Ok(Budget { bytes: left, from: format!("{from}, {}, less {taken}", gb(bytes)) })
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("no memory is left of the {} of {from} less {taken}", gb(bytes)),
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        alloc::System,
        sync::{Barrier, atomic::AtomicUsize},
    };

    use super::*;

    #[global_allocator]
    static ALLOC: Counting<System> = Counting(System);

    const MB: usize = 1 << 20;

    #[test]
    fn a_measure_says_what_its_work_held_at_most() {
        let (held, peak) = measure(|| {
            let big = std::hint::black_box(vec![1u8; 4 * MB]);
            drop(big);
            std::hint::black_box(vec![2u8; MB])
        });
        assert!(peak >= 4 * MB as u64, "{peak}");
        assert!(peak < 5 * MB as u64, "{peak}");
        assert_eq!(held.len(), MB);
        // What was held before it and freed in it lowers what it holds.
        let before = std::hint::black_box(vec![3u8; 4 * MB]);
        let ((), peak) = measure(|| {
            drop(before);
            let again = vec![4u8; MB];
            std::hint::black_box(&again);
        });
        assert!(peak < MB as u64 / 2, "{peak}");
    }

    #[test]
    fn measures_nest() {
        let ((inner, after), outer) = measure(|| {
            let (_, inner) = measure(|| std::hint::black_box(vec![1u8; 8 * MB]).len());
            let (_, after) = measure(|| std::hint::black_box(vec![1u8; MB]).len());
            (inner, after)
        });
        assert!(inner >= 8 * MB as u64, "{inner}");
        assert!(after >= MB as u64 && after < 2 * MB as u64, "{after}");
        assert!(outer >= inner, "{outer} < {inner}");
        // A panic inside a measure leaves the outer one whole.
        let ((), outer) = measure(|| {
            let caught = std::panic::catch_unwind(|| {
                measure(|| {
                    let big = std::hint::black_box(vec![1u8; 6 * MB]);
                    if !big.is_empty() {
                        panic!("on purpose");
                    }
                })
            });
            assert!(caught.is_err());
        });
        assert!(outer >= 6 * MB as u64, "{outer}");
    }

    #[test]
    fn held_counts_what_the_process_holds() {
        let big = vec![1u8; 32 * MB];
        assert!(held() >= 32 * MB as u64, "{}", held());
        std::hint::black_box(&big);
    }

    #[test]
    fn a_bump_puts_values_in_chunks_and_frees_them_together() {
        let mut bump = Bump::new();
        assert_eq!((bump.held(), bump.peak()), (0, 0));
        let mut addresses = Vec::new();
        for at in 0..10_000u64 {
            let value = bump.alloc(at).unwrap();
            assert_eq!(*value, at);
            let address = std::ptr::from_mut(value) as usize;
            assert_eq!(address % 8, 0);
            addresses.push(address);
            let byte = bump.alloc(at as u8).unwrap();
            assert_eq!(*byte, at as u8);
        }
        addresses.sort_unstable();
        addresses.dedup();
        assert_eq!(addresses.len(), 10_000);
        let text = bump.alloc_str("a result").unwrap();
        assert_eq!(text, "a result");
        let wide = bump.alloc_slice(&[7u128; 3]).unwrap();
        assert_eq!(wide, &[7u128; 3]);
        assert_eq!(wide.as_ptr() as usize % std::mem::align_of::<u128>(), 0);
        let big = bump.alloc_slice(&vec![9u8; 40 * MB]).unwrap();
        assert!(big.iter().all(|&it| it == 9));
        let held = bump.held();
        assert!(held >= 40 * MB as u64 + 10_000 * 9, "{held}");
        assert_eq!(bump.peak(), held);
        bump.reset();
        assert_eq!((bump.held(), bump.peak()), (0, held));
        assert_eq!(*bump.alloc(5u32).unwrap(), 5);
        let empty: &mut [u64] = bump.alloc_slice(&[]).unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn a_bump_under_a_cap_reserves_its_chunks() {
        let cap = Cap::new(1 << 30, 1 << 30, Box::new(|_| {}));
        let bump = Bump::under(cap.clone());
        bump.alloc_slice(&vec![0u8; 3 * MB]).unwrap();
        bump.alloc(1u64).unwrap();
        assert_eq!(cap.held(), bump.held());
        drop(bump);
        assert_eq!(cap.held(), 0);
    }

    fn until(what: impl Fn() -> bool) {
        while !what() {
            std::thread::yield_now();
        }
    }

    #[test]
    fn a_reserve_past_the_limit_waits_until_grants_are_dropped() {
        let cap = Cap::new(100, 100, Box::new(|_| {}));
        let first = cap.reserve(80).unwrap();
        let got = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            let waiter = scope.spawn(|| {
                let grant = cap.reserve(50);
                got.store(1, Ordering::SeqCst);
                grant
            });
            until(|| cap.waiting() == 1);
            assert_eq!(got.load(Ordering::SeqCst), 0);
            assert_eq!(cap.held(), 80);
            drop(first);
            let grant = waiter.join().unwrap().expect("the wait ends, not cancelled");
            assert_eq!((grant.bytes(), cap.held()), (50, 50));
        });
        assert_eq!((cap.held(), cap.waiting()), (0, 0));
    }

    /// Three threads each hold some and wait for more than is left: the newest waiter is
    /// cancelled, gives its bytes back, and the other two go on.
    #[test]
    fn when_every_holder_waits_the_newest_is_cancelled_and_the_rest_go_on() {
        for _ in 0..20 {
            let cap = Cap::new(100, 100, Box::new(|_| {}));
            let holding = Barrier::new(3);
            let order = AtomicUsize::new(0);
            let (cap, holding, order) = (&cap, &holding, &order);
            let results: Vec<(usize, Result<(), Cancelled>)> = std::thread::scope(|scope| {
                let threads: Vec<_> = (0..3)
                    .map(|_| {
                        scope.spawn(move || {
                            let first = cap.reserve(30).unwrap();
                            holding.wait();
                            // One at a time, so which waits last is known.
                            let mine = order.fetch_add(1, Ordering::SeqCst);
                            until(|| cap.waiting() == mine);
                            let more = cap.reserve(20);
                            let result = more.map(drop);
                            drop(first);
                            (mine, result)
                        })
                    })
                    .collect();
                threads.into_iter().map(|it| it.join().unwrap()).collect()
            });
            let cancelled: Vec<usize> =
                results.iter().filter(|it| it.1.is_err()).map(|it| it.0).collect();
            assert_eq!(cancelled, [2], "{results:?}");
            assert_eq!((cap.held(), cap.waiting()), (0, 0));
        }
    }

    #[test]
    fn spill_is_called_once_each_time_spill_at_is_passed() {
        let spilled = Arc::new(Mutex::new(Vec::new()));
        let cap = {
            let spilled = Arc::clone(&spilled);
            Cap::new(1000, 500, Box::new(move |over| spilled.lock().unwrap().push(over)))
        };
        let a = cap.reserve(300).unwrap();
        assert!(spilled.lock().unwrap().is_empty());
        let b = cap.reserve(300).unwrap();
        assert_eq!(*spilled.lock().unwrap(), [100]);
        let c = cap.reserve(100).unwrap();
        assert_eq!(*spilled.lock().unwrap(), [100]);
        drop((a, b, c));
        let d = cap.reserve(650).unwrap();
        assert_eq!(*spilled.lock().unwrap(), [100, 150]);
        drop(d);
    }

    /// The spill runs with the cap unlocked, so it may give grants back.
    #[test]
    fn a_spill_may_give_grants_back() {
        let cold: Arc<Mutex<Vec<Grant>>> = Arc::new(Mutex::new(Vec::new()));
        let cap = {
            let cold = Arc::clone(&cold);
            Cap::new(100, 50, Box::new(move |_| cold.lock().unwrap().clear()))
        };
        let grant = cap.reserve(40).unwrap();
        cold.lock().unwrap().push(grant);
        let warm = cap.reserve(20).unwrap();
        assert_eq!(cap.held(), 20);
        drop(warm);
    }

    #[test]
    #[should_panic(expected = "more than the daemon's cap")]
    fn a_reserve_bigger_than_the_cap_is_refused() {
        _ = Cap::new(10, 10, Box::new(|_| {})).reserve(11);
    }

    struct Tree(std::path::PathBuf);

    impl Tree {
        fn new(name: &str, files: &[(&str, &str)]) -> Tree {
            let root = std::env::temp_dir()
                .join(format!("blaze-graph-arena-{}-{name}", std::process::id()));
            _ = std::fs::remove_dir_all(&root);
            for (path, text) in files {
                let path = root.join(path);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, text).unwrap();
            }
            Tree(root)
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const MEMINFO: &str = "MemTotal:       16384000 kB\nMemFree:         1000 kB\n";

    #[test]
    fn linux_limits_are_the_machine_and_every_cgroup_above_the_process() {
        let tree = Tree::new(
            "v2",
            &[
                ("proc/meminfo", MEMINFO),
                ("proc/self/cgroup", "0::/user.slice/lane.scope\n"),
                ("sys/fs/cgroup/user.slice/memory.max", "8589934592\n"),
                ("sys/fs/cgroup/user.slice/lane.scope/memory.max", "max\n"),
            ],
        );
        let limits = linux_limits(&tree.0).unwrap();
        assert_eq!(
            limits,
            [
                (16384000 * 1024, "MemTotal of /proc/meminfo".to_owned()),
                (8589934592, "memory.max of cgroup /user.slice".to_owned()),
            ]
        );
        let budget = least(limits, None, 1 << 30).unwrap();
        assert_eq!(budget.bytes, 8589934592 - (1 << 30));
        assert_eq!(
            budget.from,
            "memory.max of cgroup /user.slice, 8.00 GB, less a margin of 1.00 GB"
        );
    }

    /// What others use is what blaze's own cgroup counts less what blaze holds, not what the
    /// machine has in use: a busy machine does not shrink a small cgroup's budget.
    #[test]
    fn others_are_what_blazes_own_cgroup_uses_less_blaze() {
        const GB: u64 = 1 << 30;
        let busy = "MemTotal:       16384000 kB\nMemAvailable:     100000 kB\n";
        let tree = Tree::new(
            "current",
            &[
                ("proc/meminfo", busy),
                ("proc/self/cgroup", "0::/lane.scope\n"),
                ("sys/fs/cgroup/lane.scope/memory.max", "4294967296\n"),
                ("sys/fs/cgroup/lane.scope/memory.current", "3221225472\n"),
            ],
        );
        let in_use = linux_in_use(&tree.0, 2 * GB).unwrap();
        assert_eq!(
            in_use,
            Some((
                GB,
                "1.00 GB others use in cgroup /lane.scope (memory.current 3.00 GB less blaze's \
                 own 2.00 GB)"
                    .to_owned()
            ))
        );
        let budget = least(linux_limits(&tree.0).unwrap(), in_use, GB / 2).unwrap();
        assert_eq!(budget.bytes, 4 * GB - GB - GB / 2);
        assert_eq!(
            budget.from,
            "memory.max of cgroup /lane.scope, 4.00 GB, less 1.00 GB others use in cgroup \
             /lane.scope (memory.current 3.00 GB less blaze's own 2.00 GB) and a margin of 0.50 \
             GB"
        );
        // Blaze holding more than the count says (it counts late) leaves others none.
        assert_eq!(linux_in_use(&tree.0, 4 * GB).unwrap().map(|it| it.0), Some(0));
        // Others that leave nothing are an error naming them.
        let full = least(vec![(4 * GB, "x".to_owned())], Some((4 * GB, "y".to_owned())), 0);
        let why = full.unwrap_err();
        assert_eq!(why.kind(), io::ErrorKind::InvalidInput);
        assert!(why.to_string().contains("less y and a margin"), "{why}");
    }

    #[test]
    fn others_are_counted_by_v1_where_memory_is_v1s_and_not_in_the_root_cgroup() {
        let tree = Tree::new(
            "current-v1",
            &[
                ("proc/self/cgroup", "4:memory:/a/b\n0::/\n"),
                ("sys/fs/cgroup/memory/a/b/memory.usage_in_bytes", "1073741824\n"),
            ],
        );
        assert_eq!(linux_in_use(&tree.0, 0).unwrap().map(|it| it.0), Some(1 << 30));
        let tree = Tree::new("current-root", &[("proc/self/cgroup", "0::/\n")]);
        assert_eq!(linux_in_use(&tree.0, 0).unwrap(), None);
        // A group below the root has a count; one missing is an error, never a guess.
        let tree = Tree::new("current-none", &[("proc/self/cgroup", "0::/x\n")]);
        assert!(linux_in_use(&tree.0, 0).unwrap_err().to_string().contains("memory.current"));
        let tree = Tree::new(
            "current-bad",
            &[("proc/self/cgroup", "0::/x\n"), ("sys/fs/cgroup/x/memory.current", "lots\n")],
        );
        assert!(linux_in_use(&tree.0, 0).unwrap_err().to_string().contains("a count of `lots`"));
        let tree = Tree::new("current-nogroup", &[("proc/self/cgroup", "3:cpu:/a\n")]);
        assert!(linux_in_use(&tree.0, 0).unwrap_err().to_string().contains("counts memory"));
    }

    #[test]
    fn linux_limits_read_cgroup_v1_and_a_namespace_root() {
        let tree = Tree::new(
            "v1",
            &[
                ("proc/meminfo", MEMINFO),
                ("proc/self/cgroup", "12:cpu,cpuacct:/a\n4:memory:/a/b\n0::/\n"),
                ("sys/fs/cgroup/memory/a/b/memory.limit_in_bytes", "2147483648\n"),
                ("sys/fs/cgroup/memory/a/memory.limit_in_bytes", "9223372036854771712\n"),
                ("sys/fs/cgroup/memory.max", "4294967296\n"),
            ],
        );
        let limits = linux_limits(&tree.0).unwrap();
        let mut bytes: Vec<u64> = limits.iter().map(|it| it.0).collect();
        bytes.sort_unstable();
        assert_eq!(bytes, [2147483648, 4294967296, 16384000 * 1024, 9223372036854771712]);
        assert_eq!(least(limits, None, 0).unwrap().bytes, 2147483648);
    }

    #[test]
    fn what_cannot_be_read_is_an_error_never_a_guess() {
        let tree = Tree::new("none", &[("proc/meminfo", MEMINFO)]);
        assert!(linux_limits(&tree.0).unwrap_err().to_string().contains("proc/self/cgroup"));
        let tree = Tree::new(
            "bad",
            &[
                ("proc/meminfo", MEMINFO),
                ("proc/self/cgroup", "0::/x\n"),
                ("sys/fs/cgroup/x/memory.max", "lots\n"),
            ],
        );
        assert!(linux_limits(&tree.0).unwrap_err().to_string().contains("a limit of `lots`"));
        let tree = Tree::new("nototal", &[("proc/meminfo", "MemFree: 1 kB\n")]);
        assert!(linux_limits(&tree.0).unwrap_err().to_string().contains("no MemTotal"));
        assert_eq!(
            least(vec![(1 << 30, "x".to_owned())], None, 1 << 30).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn the_machine_budget_is_read() {
        let budget = machine_budget(1 << 20).unwrap();
        assert!(budget.bytes > 0);
        assert!(budget.from.contains("a margin of 0.00 GB"), "{}", budget.from);
        #[cfg(target_os = "linux")]
        assert!(budget.from.contains("others use in cgroup") || budget.from.contains("less a"));
    }
}

//! Where a task's memory is measured and the daemon's is capped (docs/v2/plan/lanes/core.md section 4,
//! "Memory-budget admission" and "Hard backstop").
//!
//! [`Counting`] is the daemon's global allocator: it counts the bytes the process holds
//! ([`held`]) and, per thread, the bytes the task the thread runs now holds, so [`measure`]
//! says what a task held at most. [`Bump`] is an arena for results built before they are
//! written into the zero-copy format; every task runs with one of its thread's ([`task`],
//! [`with`]), reset when it ends, its huge chunks on the worker's NUMA node. [`Cap`] is the
//! daemon's limit on its own arenas: past
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
        Arc, PoisonError,
        atomic::{AtomicI64, Ordering},
    },
};

use crossbeam_channel::{Receiver, Sender, unbounded};
use rustc_hash::FxHashMap;

use crate::{
    Budget, Cancelled,
    admission::Arena,
    pump::Lock,
    sync::{
        Condvar, MutexGuard,
        thread::{self, ThreadId},
    },
};

/// How far a thread's count may move before it is added to [`HELD`] (Linux's
/// `percpu_counter`): an allocation then costs a few thread-local adds and no atomic, and
/// [`HELD`] is written once per this many bytes of a thread's drift.
const BATCH: i64 = 256 << 10;

#[repr(align(128))]
struct Total(AtomicI64);

/// What every thread added of its count: the bytes the process holds through [`Counting`],
/// but for less than [`BATCH`] per thread not yet added.
static HELD: Total = Total(AtomicI64::new(0));

/// A thread's counts, in one block so an allocation reaches its thread-local storage once.
struct Local {
    /// The bytes this thread allocated less those it freed, through [`Counting`].
    net: Cell<i64>,
    /// The most `net` was since the innermost [`measure`] on this thread began.
    peak: Cell<i64>,
    /// What `net` moved since it was last added to [`HELD`].
    unflushed: Cell<i64>,
    /// How far `unflushed` may go before it is added: 0 until the flush at the thread's exit is
    /// armed, and again once it ran, so no count is lost with the thread.
    batch: Cell<i64>,
    armed: Cell<bool>,
    #[cfg(test)]
    flushed: Cell<i64>,
}

// `LOCAL` is `const` and without a destructor: reading it allocates nothing and works while a
// thread ends, as an allocator must. `EXIT` has one, registered from the cold `flush` only.
thread_local! {
    static LOCAL: Local = const {
        Local {
            net: Cell::new(0),
            peak: Cell::new(0),
            unflushed: Cell::new(0),
            batch: Cell::new(0),
            armed: Cell::new(false),
            #[cfg(test)]
            flushed: Cell::new(0),
        }
    };
    static EXIT: Exit = const { Exit };
}

struct Exit;

impl Drop for Exit {
    fn drop(&mut self) {
        LOCAL.with(exit_flush);
    }
}

/// Adds what is left of the thread's count to [`HELD`], and from then on every change at
/// once: what the thread's other destructors free still counts. What it added.
fn exit_flush(local: &Local) -> i64 {
    local.batch.set(0);
    let left = local.unflushed.replace(0);
    add(local, left);
    left
}

#[cfg_attr(not(test), allow(unused_variables))]
fn add(local: &Local, bytes: i64) {
    HELD.0.fetch_add(bytes, Ordering::Relaxed);
    #[cfg(test)]
    local.flushed.set(local.flushed.get().wrapping_add(bytes));
}

/// Per allocation: one thread-local block, three adds and two well-predicted branches; the
/// atomic add is in [`flush`], once per [`BATCH`] bytes of drift.
#[inline(always)]
fn count(delta: i64) {
    LOCAL.with(|local| {
        let net = local.net.get().wrapping_add(delta);
        local.net.set(net);
        if net > local.peak.get() {
            local.peak.set(net);
        }
        let unflushed = local.unflushed.get().wrapping_add(delta);
        if unflushed.unsigned_abs() < local.batch.get().unsigned_abs() {
            local.unflushed.set(unflushed);
        } else {
            flush(local, unflushed);
        }
    });
}

#[cold]
#[inline(never)]
fn flush(local: &Local, unflushed: i64) {
    local.unflushed.set(0);
    add(local, unflushed);
    // Registering `EXIT`'s destructor may allocate, which comes back here with `armed` set
    // and `batch` still 0: it adds its own bytes and registers nothing.
    if !local.armed.replace(true) && EXIT.try_with(|_| {}).is_ok() {
        local.batch.set(BATCH);
    }
}

/// A global allocator that counts what goes through `A`: the daemon's binary installs it over
/// the allocator it uses (`#[global_allocator] static ALLOC: Counting<Jemalloc> =
/// Counting(Jemalloc);`). It takes no lock, allocates nothing of its own and does no atomic
/// write on an allocation but once per [`BATCH`] bytes a thread's count moves.
///
/// A byte freed on another thread than the one that allocated it counts against the thread
/// that frees it: [`measure`] sees what a task's thread allocated and freed, exactly; [`held`]
/// is within [`BATCH`] per other thread of what the process holds.
pub struct Counting<A>(pub A);

fn signed(bytes: usize) -> i64 {
    // A layout's size is at most `isize::MAX`.
    bytes as i64
}

// SAFETY: every call goes to `A` with the caller's arguments, so `A`'s guarantees hold; the
// counting beside it touches an atomic and `LOCAL`, a `const` thread-local with no destructor,
// and registers `EXIT`'s destructor once per thread, whose own allocations `flush` takes
// without registering again.
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
            LOCAL.with(|local| local.peak.set(self.outer.max(local.peak.get())));
        }
    }
    let (base, outer) = LOCAL.with(|local| {
        let base = local.net.get();
        (base, local.peak.replace(base))
    });
    let restore = Restore { outer };
    let result = f();
    let peak = LOCAL.with(|local| local.peak.get());
    drop(restore);
    (result, u64::try_from(peak.saturating_sub(base)).unwrap_or(0))
}

/// The bytes the process holds now through [`Counting`]; 0 when it is not the global
/// allocator. One load: exact for what the calling thread did, within [`BATCH`] for each other
/// thread.
pub fn held() -> u64 {
    let own = LOCAL.with(|local| local.unflushed.get());
    u64::try_from(HELD.0.load(Ordering::Relaxed).wrapping_add(own)).unwrap_or(0)
}

/// Everything blaze itself holds: [`held`] (what goes through the global allocator) and the
/// pages `blaze_mem` committed for its regions, frames, chunks and pools, which it maps itself
/// and [`Counting`] never sees ([`blaze_mem::tag::Snapshot::committed`]).
pub fn own() -> u64 {
    held().saturating_add(blaze_mem::tag::snapshot().committed)
}

/// The first chunk of a [`Bump`], and the most a chunk grows to but for one value bigger.
const FIRST_CHUNK: usize = 64 << 10;
const LAST_CHUNK: usize = 16 << 20;
const CHUNK_ALIGN: usize = 16;
/// Chunks this big or bigger are huge pages (docs/v2/plan/design/hardware.md, "memory"): on
/// Linux the arena maps them itself, a multiple of this, aligned to it, with `MADV_HUGEPAGE`,
/// and binds them to the NUMA node of the worker that takes them; elsewhere they come from the
/// global allocator, aligned to it.
const HUGE_PAGE: usize = 2 << 20;

struct Chunk {
    ptr: NonNull<u8>,
    layout: Layout,
    /// Mapped by the arena ([`map_chunk`]) rather than taken from the global allocator.
    mapped: bool,
    /// The [`Cap`]'s bytes it holds, given back when it is freed.
    _grant: Option<Grant>,
}

thread_local! {
    /// The arena of the task this thread runs now ([`task`]), null off a task.
    static TASK: Cell<*const Bump> = const { Cell::new(std::ptr::null()) };
    /// Arenas of tasks that ended, reset, each keeping its last chunk: the next task on the
    /// thread takes one, so a worker's tasks reuse their memory without the allocator.
    static SPARE: RefCell<Vec<Bump>> = const { RefCell::new(Vec::new()) };
}

/// Runs `f` with an arena of its own installed for [`with`], and resets it after (on a panic
/// too): every value put in it is freed when `f` returns, and its last chunk goes to the next
/// task of the thread. A task run inside another (one a waiting worker pumps) gets its own
/// arena, not its waiter's, and the waiter's is installed again once it ends. On a pool
/// worker, the arena's huge chunks are on the worker's NUMA node.
pub fn task<R>(f: impl FnOnce() -> R) -> R {
    struct Installed {
        arena: Bump,
        outer: *const Bump,
    }
    impl Drop for Installed {
        fn drop(&mut self) {
            TASK.set(self.outer);
            let mut arena = std::mem::take(&mut self.arena);
            arena.reset();
            // At the thread's exit `SPARE` may be gone: the arena is then freed here.
            _ = SPARE.try_with(|spare| spare.borrow_mut().push(arena));
        }
    }
    let arena = SPARE.with(|spare| spare.borrow_mut().pop()).unwrap_or_default();
    let installed = Installed { arena, outer: TASK.get() };
    TASK.set(&raw const installed.arena);
    let result = f();
    drop(installed);
    result
}

/// Gives `f` the arena of the task this thread runs ([`task`]). What `f` puts in it lives
/// until the task ends, and no borrow of it outlives `f`.
///
/// # Panics
///
/// Off a task: work that wants an arena runs inside [`task`].
pub fn with<R>(f: impl FnOnce(&Bump) -> R) -> R {
    let at = TASK.get();
    assert!(!at.is_null(), "arena::with outside a task: the work runs inside arena::task");
    // SAFETY: `TASK` is not null only while a `task` on this thread runs its `f`, and then
    // points at that call's `Installed::arena`, a local it does not move, which lives and is
    // not reset until `f` has returned (its `Drop` puts back the outer pointer first, on an
    // unwind too). This call is within that `f`, and the borrow ends with `f` here, so it ends
    // before the reset; a nested `task` installs its own arena and never touches this one.
    f(unsafe { &*at })
}

/// A bump arena: values are put in chunks from the global allocator (or, for huge ones on
/// Linux, mapped by the arena: [`HUGE_PAGE`]), from each chunk's end down (the fastest bump: a
/// subtraction, a mask and one compare per value, as bumpalo), and all are freed together when
/// it is dropped or [`Bump::reset`]. No value's destructor runs, so it takes only `Copy`
/// values. Under a [`Cap`], each chunk is reserved from it first. Mapped chunks count in
/// [`held`] and [`measure`] as the allocator's do.
pub struct Bump {
    chunks: RefCell<Vec<Chunk>>,
    /// The lowest byte in use of the last chunk: the next value goes below it, down to `start`.
    /// Both are the address 1 before the first chunk, so only a zero-sized value fits.
    ptr: Cell<*mut u8>,
    start: Cell<*mut u8>,
    held: Cell<u64>,
    peak: Cell<u64>,
    cap: Option<Cap>,
}

const NO_CHUNK: *mut u8 = std::ptr::without_provenance_mut(1);

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
            ptr: Cell::new(NO_CHUNK),
            start: Cell::new(NO_CHUNK),
            held: Cell::new(0),
            peak: Cell::new(0),
            cap,
        }
    }

    /// Room for a value of `layout`, valid until the arena is reset or dropped.
    #[inline]
    pub fn alloc_layout(&self, layout: Layout) -> Result<NonNull<u8>, Cancelled> {
        match self.fit(layout) {
            Some(at) => Ok(at),
            None => self.alloc_in_new_chunk(layout),
        }
    }

    #[cold]
    #[inline(never)]
    fn alloc_in_new_chunk(&self, layout: Layout) -> Result<NonNull<u8>, Cancelled> {
        self.grow(layout)?;
        Ok(self.fit(layout).expect("a new chunk has room for the value it was made for"))
    }

    #[inline]
    #[allow(clippy::mut_from_ref)]
    pub fn alloc<T: Copy>(&self, value: T) -> Result<&mut T, Cancelled> {
        let ptr = self.alloc_layout(Layout::new::<T>())?.cast::<T>();
        // SAFETY: `alloc_layout` gave fresh room, aligned and big enough for a `T`, that nothing
        // else points at.
        unsafe { ptr.as_ptr().write(value) };
        // SAFETY: the `T` was just written there, and no other reference to the room is given
        // out while `self` is borrowed; the `Bump` frees it only on `reset`, which takes
        // `&mut self`.
        Ok(unsafe { &mut *ptr.as_ptr() })
    }

    #[inline]
    #[allow(clippy::mut_from_ref)]
    pub fn alloc_slice<T: Copy>(&self, values: &[T]) -> Result<&mut [T], Cancelled> {
        let layout = Layout::array::<T>(values.len()).expect("a slice's layout fits");
        let ptr = self.alloc_layout(layout)?.cast::<T>();
        // SAFETY: the room is fresh, aligned and big enough for `values.len()` of `T`, so it
        // cannot overlap `values`.
        unsafe { std::ptr::copy_nonoverlapping(values.as_ptr(), ptr.as_ptr(), values.len()) };
        // SAFETY: those `values.len()` `T`s were just written, and no other reference to the
        // room is given out while `self` is borrowed.
        Ok(unsafe { std::slice::from_raw_parts_mut(ptr.as_ptr(), values.len()) })
    }

    #[allow(clippy::mut_from_ref)]
    pub fn alloc_str(&self, text: &str) -> Result<&mut str, Cancelled> {
        let bytes = self.alloc_slice(text.as_bytes())?;
        // SAFETY: the bytes are a copy of a `str`'s.
        Ok(unsafe { std::str::from_utf8_unchecked_mut(bytes) })
    }

    /// Frees every value, and every chunk but the last, which the next values reuse, so an
    /// arena reset between tasks calls the allocator only when a task needs more than that
    /// chunk; what it held at most is kept.
    pub fn reset(&mut self) {
        let chunks = self.chunks.get_mut();
        let last = chunks.pop();
        for chunk in chunks.drain(..) {
            free(chunk);
        }
        match last {
            Some(last) => {
                self.start.set(last.ptr.as_ptr());
                // SAFETY: one past the end of the chunk's allocation.
                self.ptr.set(unsafe { last.ptr.as_ptr().add(last.layout.size()) });
                self.held.set(last.layout.size() as u64);
                chunks.push(last);
            }
            None => {
                self.start.set(NO_CHUNK);
                self.ptr.set(NO_CHUNK);
                self.held.set(0);
            }
        }
    }

    #[inline(always)]
    fn fit(&self, layout: Layout) -> Option<NonNull<u8>> {
        let top = self.ptr.get();
        let low = top.addr().checked_sub(layout.size())? & !(layout.align() - 1);
        if low < self.start.get().addr() {
            return None;
        }
        // `low` is in `start..=top`, within the last chunk, whose provenance `top` carries.
        let at = top.with_addr(low);
        self.ptr.set(at);
        // SAFETY: `low >= start >= 1`.
        Some(unsafe { NonNull::new_unchecked(at) })
    }

    fn grow(&self, layout: Layout) -> Result<(), Cancelled> {
        let last = self.chunks.borrow().last().map_or(0, |it| it.layout.size());
        let wanted = layout.size().checked_add(layout.align()).expect("a value's size fits");
        let size = (last * 2).clamp(FIRST_CHUNK, LAST_CHUNK).max(wanted);
        // A value of any alignment fits: `wanted` has room to align it down within the chunk.
        let mapped = cfg!(target_os = "linux") && size >= HUGE_PAGE;
        let (size, align) = match size >= HUGE_PAGE {
            true if mapped => (size.next_multiple_of(HUGE_PAGE), HUGE_PAGE),
            true => (size, HUGE_PAGE.max(layout.align())),
            false => (size, CHUNK_ALIGN.max(layout.align())),
        };
        let chunk_layout = Layout::from_size_align(size, align).expect("a chunk's layout fits");
        let grant = match &self.cap {
            Some(cap) => Some(cap.reserve(size as u64)?),
            None => None,
        };
        let ptr = if mapped {
            map_chunk(chunk_layout)
        } else {
            // SAFETY: the layout's size is not zero.
            let ptr = unsafe { std::alloc::alloc(chunk_layout) };
            let Some(ptr) = NonNull::new(ptr) else { std::alloc::handle_alloc_error(chunk_layout) };
            ptr
        };
        self.chunks.borrow_mut().push(Chunk { ptr, layout: chunk_layout, mapped, _grant: grant });
        self.start.set(ptr.as_ptr());
        // SAFETY: one past the end of the chunk's allocation.
        self.ptr.set(unsafe { ptr.as_ptr().add(size) });
        let held = self.held.get() + size as u64;
        self.held.set(held);
        self.peak.set(self.peak.get().max(held));
        Ok(())
    }
}

fn free(chunk: Chunk) {
    if chunk.mapped {
        unmap_chunk(&chunk);
        return;
    }
    // SAFETY: the chunk was allocated by the global allocator with this layout, and the values
    // in it are borrowed from the `Bump`, which its caller borrows mutably.
    unsafe { std::alloc::dealloc(chunk.ptr.as_ptr(), chunk.layout) };
}

/// A chunk of `layout` (its size a multiple of [`HUGE_PAGE`], its alignment that) mapped by
/// the arena: anonymous memory, asked to be huge pages, and on a pool worker bound to the
/// worker's NUMA node before anything touches it, so its pages are made there. Counted in the
/// thread's count as an allocation is.
#[cfg(target_os = "linux")]
fn map_chunk(layout: Layout) -> NonNull<u8> {
    let (len, align) = (layout.size(), layout.align());
    debug_assert!(len.is_multiple_of(align) && align == HUGE_PAGE);
    // One huge page more than asked, so an aligned range of `len` is within it; the rest goes.
    let span = len + align;
    // SAFETY: a new anonymous private mapping, at no address asked for, overlaps nothing.
    let raw = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            span,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if raw == libc::MAP_FAILED {
        std::alloc::handle_alloc_error(layout);
    }
    let raw = raw.cast::<u8>();
    let head = raw.addr().next_multiple_of(align) - raw.addr();
    let tail = span - head - len;
    let start = raw.wrapping_add(head);
    if head > 0 {
        // SAFETY: the first `head` bytes of the mapping just made, which nothing uses.
        unsafe { libc::munmap(raw.cast(), head) };
    }
    if tail > 0 {
        // SAFETY: the last `tail` bytes of the mapping just made, past the chunk.
        unsafe { libc::munmap(start.wrapping_add(len).cast(), tail) };
    }
    // A hint, not a request that can fail the arena: the kernel's transparent huge page
    // setting decides whether it is taken; the bytes are the same either way.
    // SAFETY: the range is the chunk just mapped.
    _ = unsafe { libc::madvise(start.cast(), len, libc::MADV_HUGEPAGE) };
    if let Some(node) = crate::pool::node()
        && let Err(why) = bind(start, len, node)
    {
        panic!("binding a {len}-byte arena chunk to NUMA node {node} (mbind): {why}");
    }
    count(signed(len));
    // SAFETY: `mmap` did not fail, so `raw` and `start` within it are not null.
    unsafe { NonNull::new_unchecked(start) }
}

#[cfg(not(target_os = "linux"))]
fn map_chunk(_layout: Layout) -> NonNull<u8> {
    unreachable!("only Linux maps arena chunks")
}

#[cfg(target_os = "linux")]
fn unmap_chunk(chunk: &Chunk) {
    // SAFETY: the chunk is the range `map_chunk` kept, and its values are borrowed from the
    // `Bump`, which its caller borrows mutably or drops.
    unsafe { libc::munmap(chunk.ptr.as_ptr().cast(), chunk.layout.size()) };
    count(-signed(chunk.layout.size()));
}

#[cfg(not(target_os = "linux"))]
fn unmap_chunk(_chunk: &Chunk) {
    unreachable!("only Linux maps arena chunks")
}

/// `mempolicy.h`'s `MPOL_PREFERRED`: pages are made on the node given while it has room.
#[cfg(target_os = "linux")]
const MPOL_PREFERRED: libc::c_long = 1;
/// The nodes a node mask of [`bind`] can name.
#[cfg(target_os = "linux")]
const MASK_WORDS: usize = 16;

/// Binds the `len` bytes at `start`, not yet touched, to NUMA node `node` (`MPOL_PREFERRED`).
/// `Ok(false)` on a kernel built without NUMA (`ENOSYS`): it has one node, and its memory is
/// all on it.
#[cfg(target_os = "linux")]
fn bind(start: *mut u8, len: usize, node: u32) -> io::Result<bool> {
    let mut mask = [0 as libc::c_ulong; MASK_WORDS];
    let bits = libc::c_ulong::BITS as usize;
    let at = node as usize;
    let Some(word) = mask.get_mut(at / bits) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("node {node} is past the {} nodes a mask names", MASK_WORDS * bits),
        ));
    };
    *word |= 1 << (at % bits);
    // The kernel reads one bit fewer than `maxnode` says.
    let maxnode = (MASK_WORDS * bits + 1) as libc::c_ulong;
    // SAFETY: `mbind` reads `MASK_WORDS` words of `mask`, a live array, and changes only the
    // policy of the range, a mapping of the caller's.
    let status = unsafe {
        libc::syscall(libc::SYS_mbind, start, len, MPOL_PREFERRED, mask.as_ptr(), maxnode, 0)
    };
    if status == 0 {
        return Ok(true);
    }
    let why = io::Error::last_os_error();
    match why.raw_os_error() {
        Some(libc::ENOSYS) => Ok(false),
        _ => Err(why),
    }
}

impl Drop for Bump {
    fn drop(&mut self) {
        for chunk in self.chunks.get_mut().drain(..) {
            free(chunk);
        }
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
/// its [`Spills`] hears the bytes over it, once each time it is passed, so the query
/// database's spiller moves cold values to the store; a reserve that would pass `limit` waits until
/// enough is given back. A holder is the thread that reserved; when every thread holding
/// bytes is itself waiting in [`Cap::reserve`], nothing can be given back, and the newest of
/// those waits ends in [`Cancelled`], so its task unwinds and gives its bytes back.
///
/// A waiter is let in as soon as its bytes fit, whoever waited longer.
#[derive(Clone)]
pub struct Cap(Arc<Shared>);

/// What a [`Cap`] says each time what it holds passes `spill_at`: the bytes over it, for the
/// database's spiller, which moves cold values to the store and drops their grants on its own
/// thread. The reserve that passed it only sends: no database code runs inside a task's
/// allocation, and the reserve does not wait for the spill.
pub struct Spills(Receiver<u64>);

impl Spills {
    /// The next pass of `spill_at`, waiting for it; `None` once the cap and every grant of it
    /// are gone.
    pub fn next(&self) -> Option<u64> {
        self.0.recv().ok()
    }

    /// The passes not yet taken, in order, without waiting.
    pub fn ready(&self) -> impl Iterator<Item = u64> + '_ {
        self.0.try_iter()
    }
}

struct Shared {
    limit: u64,
    spill_at: u64,
    spills: Sender<u64>,
    state: Lock<State>,
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
        self.state.lock()
    }
}

impl Cap {
    /// A cap of `limit` bytes, and what hears each time it passes `spill_at`.
    pub fn new(limit: u64, spill_at: u64) -> (Cap, Spills) {
        // Each send is a pass of `spill_at`, which needs a drop back under it first: a few a
        // build, so the queue stays short.
        let (spills, heard) = unbounded();
        let shared = Shared {
            limit,
            spill_at,
            spills,
            state: Lock::new(State::default()),
            changed: Condvar::new(),
        };
        (Cap(Arc::new(shared)), Spills(heard))
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
        let me = thread::current().id();
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
                    // A cap whose `Spills` was dropped has no spiller: the send fails, and that
                    // changes nothing.
                    _ = shared.spills.send(over);
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
        // A wake is a system call even with no one waiting.
        let waiters = !state.waiters.is_empty();
        if waiters {
            state.unstick(shared.limit);
        }
        drop(state);
        if waiters {
            shared.changed.notify_all();
        }
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
/// v1's) less what blaze itself holds ([`own`]: [`held`], 0 when [`Counting`] is not the global
/// allocator, so the budget is then the smaller, and the pages `blaze_mem` committed). It is not the machine's `MemTotal` less
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
    let in_use = machine_in_use(own())?;
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
#[allow(clippy::disallowed_types, reason = "a test's own bookkeeping, off every hot path")]
mod tests {
    use std::sync::{Barrier, Mutex, atomic::AtomicUsize};

    use blaze_ids::rng::SplitMix64;

    use super::*;

    /// `blaze_alloc_count`'s counter under this crate's own: the allocation-budget tests count
    /// calls, the arena's tests bytes held.
    #[global_allocator]
    static ALLOC: Counting<blaze_alloc_count::Counting> = Counting(blaze_alloc_count::Counting);

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

    /// What blaze holds counts the pages `blaze_mem` maps itself, which the global allocator
    /// never sees, beside what goes through it.
    #[test]
    fn blaze_holds_what_blaze_mem_committed_too() {
        let mut column = blaze_mem::RegionVec::<u64>::new(4 * MB, blaze_mem::Tag::Plane).unwrap();
        column.extend_from_slice(&vec![7u64; MB]);
        let committed = blaze_mem::tag::snapshot().committed;
        assert!(committed >= 8 * MB as u64, "{committed}");
        assert!(own() >= 8 * MB as u64, "{}", own());
        std::hint::black_box(&column);
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
        // The last chunk, the one the 40 MB went in, is kept for what comes next.
        let kept = bump.held();
        assert!((40 * MB as u64..held).contains(&kept), "{kept} of {held}");
        assert_eq!(bump.peak(), held);
        assert_eq!(*bump.alloc(5u32).unwrap(), 5);
        let empty: &mut [u64] = bump.alloc_slice(&[]).unwrap();
        assert!(empty.is_empty());
        assert_eq!(bump.held(), kept);
        let fresh = Bump::new();
        let empty: &mut [u8] = fresh.alloc_slice(&[]).unwrap();
        assert!(empty.is_empty());
        assert_eq!(fresh.held(), 0);
    }

    /// Seeded layouts of every size and alignment: each value is aligned, inside the arena,
    /// apart from every other, and keeps what was written, across resets.
    #[test]
    fn bumped_values_are_aligned_apart_and_kept() {
        for seed in 0..40u64 {
            let mut rng = SplitMix64::new(seed);
            let mut bump = Bump::new();
            let mut values: Vec<(usize, usize, u8)> = Vec::new();
            for step in 0..3000usize {
                if rng.one_in(500) {
                    check_values(&values);
                    values.clear();
                    bump.reset();
                }
                let size = match rng.below(8) {
                    0 => 0,
                    1 => rng.index(200_000),
                    _ => rng.index(300),
                };
                let align = 1usize << rng.below(7);
                let layout = Layout::from_size_align(size, align).unwrap();
                let at = bump.alloc_layout(layout).unwrap();
                assert_eq!(at.as_ptr() as usize % align, 0, "seed {seed} step {step}");
                let byte = (step % 251) as u8;
                // SAFETY: the room is `size` bytes the arena gave and nothing else uses.
                unsafe { at.as_ptr().write_bytes(byte, size) };
                values.push((at.as_ptr() as usize, size, byte));
            }
            check_values(&values);
        }
    }

    fn check_values(values: &[(usize, usize, u8)]) {
        let mut sorted: Vec<_> = values.iter().filter(|it| it.1 > 0).copied().collect();
        sorted.sort_unstable();
        for pair in sorted.windows(2) {
            assert!(pair[0].0 + pair[0].1 <= pair[1].0, "{:?} overlaps {:?}", pair[0], pair[1]);
        }
        for &(at, size, byte) in &sorted {
            // SAFETY: the value's room is still the arena's, as no reset came since.
            let bytes = unsafe { std::slice::from_raw_parts(at as *const u8, size) };
            assert!(bytes.iter().all(|&it| it == byte));
        }
    }

    #[test]
    fn a_reset_arena_reuses_its_chunk_without_the_allocator() {
        let mut bump = Bump::new();
        for at in 0..1000u64 {
            bump.alloc(at).unwrap();
        }
        let held = bump.held();
        for _ in 0..100 {
            bump.reset();
            let ((), peak) = measure(|| {
                for at in 0..1000u64 {
                    bump.alloc(at).unwrap();
                }
            });
            assert_eq!(peak, 0);
            assert_eq!(bump.held(), held);
        }
    }

    #[test]
    fn big_chunks_are_aligned_to_a_huge_page() {
        let bump = Bump::new();
        bump.alloc_slice(&vec![1u8; 3 * MB]).unwrap();
        let chunks = bump.chunks.borrow();
        let chunk = chunks.last().unwrap();
        assert!(chunk.layout.size() >= HUGE_PAGE);
        assert_eq!(chunk.ptr.as_ptr() as usize % HUGE_PAGE, 0);
    }

    /// What this thread allocated less what it freed, exactly.
    fn net() -> i64 {
        LOCAL.with(|local| local.net.get())
    }

    fn address(bump: &Bump) -> usize {
        std::ptr::from_ref(bump).addr()
    }

    /// The calls of `with` in one task share its arena; once it ends the arena is reset, and
    /// the next task on the thread gets it with only its last chunk.
    #[test]
    fn a_task_keeps_its_arena_until_it_ends_and_the_next_gets_it_reset() {
        let (same, held) = task(|| {
            let first = with(|bump| {
                assert!(bump.alloc_slice(&[1u8; 300_000]).unwrap().iter().all(|&it| it == 1));
                address(bump)
            });
            with(|bump| {
                let second = bump.alloc_slice(&[2u8; 300_000]).unwrap();
                assert!(second.iter().all(|&it| it == 2));
                (address(bump) == first, bump.held())
            })
        });
        assert!(same);
        assert!(held >= 600_000, "{held}");
        // Only the last chunk is kept (all of them, if an earlier test on the thread left one
        // big enough for both).
        let kept = task(|| with(|bump| bump.held()));
        assert!(kept > 0 && kept <= held, "{kept} of {held}");
    }

    /// A task run inside another has an arena of its own, and the outer one's is back after.
    #[test]
    fn a_task_inside_a_task_has_its_own_arena() {
        task(|| {
            with(|outer| {
                let one = outer.alloc(1u64).unwrap();
                let inner = task(|| {
                    with(|inner| {
                        assert_eq!(*inner.alloc(2u64).unwrap(), 2);
                        address(inner)
                    })
                });
                assert_ne!(inner, address(outer));
                assert_eq!(*one, 1);
                assert_eq!(with(address), address(outer));
            });
        });
    }

    #[test]
    #[should_panic(expected = "arena::with outside a task")]
    fn an_arena_off_a_task_is_refused() {
        with(|_| ());
    }

    #[test]
    fn a_tasks_arena_is_reset_after_a_panic_too() {
        let caught = std::panic::catch_unwind(|| {
            task(|| {
                with(|bump| _ = bump.alloc_slice(&[0u8; 200_000]).unwrap());
                panic!("the task's work panics");
            })
        });
        assert!(caught.is_err());
        let refused = std::panic::catch_unwind(|| with(|_| ()));
        assert!(refused.is_err(), "no arena is installed once the task unwound");
        assert!(task(|| with(|bump| bump.held())) > 0);
    }

    /// A measure of a task sees its arena's chunks and their reset: what stays is the chunk
    /// the arena keeps for the next task, less the one it kept before.
    #[test]
    fn a_measure_sees_a_tasks_arena_and_its_reset() {
        task(|| with(|bump| _ = bump.alloc(0u8).unwrap()));
        let warm = task(|| with(|bump| bump.held()));
        // Bigger than the chunk kept, so new chunks are taken.
        let big = Layout::from_size_align(warm as usize + MB, 8).unwrap();
        let before = net();
        let (kept, peak) = measure(|| {
            task(|| {
                with(|bump| {
                    bump.alloc_layout(big).unwrap();
                    bump.alloc_layout(big).unwrap();
                });
            });
            task(|| with(|bump| bump.held()))
        });
        assert_eq!(net() - before, kept as i64 - warm as i64);
        assert!(peak >= 2 * big.size() as u64, "{peak}");
    }

    /// On Linux a huge chunk is the arena's own mapping: aligned, a multiple of a huge page,
    /// counted as an allocation, and given back when the arena goes.
    #[test]
    #[cfg(target_os = "linux")]
    fn huge_chunks_are_mapped_by_the_arena_and_counted() {
        let before = net();
        let bump = Bump::new();
        bump.alloc_layout(Layout::from_size_align(3 * MB, 8).unwrap()).unwrap();
        let (mapped, size) = {
            let chunks = bump.chunks.borrow();
            let chunk = chunks.last().unwrap();
            assert_eq!(chunk.ptr.as_ptr().addr() % HUGE_PAGE, 0);
            (chunk.mapped, chunk.layout.size())
        };
        assert!(mapped);
        assert!(size.is_multiple_of(HUGE_PAGE) && size >= 3 * MB, "{size}");
        // The chunk, and the arena's list of chunks.
        let grew = net() - before;
        assert!((size as i64..size as i64 + 4096).contains(&grew), "{grew} for {size}");
        drop(bump);
        assert_eq!(net(), before);
    }

    /// A chunk bound to a node says so to `get_mempolicy`; a kernel with no NUMA binds nothing.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_mapped_chunk_is_bound_to_the_node_asked() {
        let layout = Layout::from_size_align(HUGE_PAGE, HUGE_PAGE).unwrap();
        // Off the pool: not bound yet.
        let chunk = map_chunk(layout);
        if bind(chunk.as_ptr(), HUGE_PAGE, 0).unwrap() {
            let mut mode: libc::c_int = -1;
            let mut mask = [0 as libc::c_ulong; MASK_WORDS];
            let maxnode = (MASK_WORDS * libc::c_ulong::BITS as usize + 1) as libc::c_ulong;
            const MPOL_F_ADDR: libc::c_ulong = 2;
            // SAFETY: `mode` and `mask` are live and as big as `maxnode` says; the address is
            // the chunk's, mapped.
            let status = unsafe {
                libc::syscall(
                    libc::SYS_get_mempolicy,
                    &raw mut mode,
                    mask.as_mut_ptr(),
                    maxnode,
                    chunk.as_ptr(),
                    MPOL_F_ADDR,
                )
            };
            assert_eq!(status, 0, "{}", io::Error::last_os_error());
            assert_eq!(libc::c_long::from(mode), MPOL_PREFERRED);
            assert_eq!(mask[0] & 1, 1);
        }
        unmap_chunk(&Chunk { ptr: chunk, layout, mapped: true, _grant: None });
        let far = bind(std::ptr::null_mut(), 0, 1 << 20).unwrap_err();
        assert!(far.to_string().contains("past the"), "{far}");
    }

    /// A thread's count reaches the total in batches: what it added plus what it has not yet
    /// is exactly what it allocated less what it freed, what it has not stays under a batch,
    /// and at its exit the rest is added, after which every change is added at once.
    #[test]
    fn a_thread_adds_its_count_in_batches_and_the_rest_at_exit() {
        std::thread::spawn(|| {
            std::hint::black_box(vec![0u8; 16]);
            assert_eq!(LOCAL.with(|it| it.batch.get()), BATCH);
            let mut rng = SplitMix64::new(7);
            let mut kept: Vec<Vec<u8>> = Vec::with_capacity(4000);
            for _ in 0..4000 {
                if !kept.is_empty() && rng.one_in(3) {
                    let at = rng.index(kept.len());
                    drop(kept.swap_remove(at));
                } else {
                    kept.push(std::hint::black_box(vec![0u8; 1 + rng.index(9000)]));
                }
                LOCAL.with(|it| {
                    assert!(it.unflushed.get().unsigned_abs() < BATCH as u64);
                    assert_eq!(it.net.get(), it.flushed.get().wrapping_add(it.unflushed.get()));
                });
            }
            let left = LOCAL.with(|it| it.unflushed.get());
            assert_eq!(LOCAL.with(exit_flush), left);
            drop(kept);
            LOCAL.with(|it| {
                assert_eq!((it.unflushed.get(), it.batch.get()), (0, 0));
                assert_eq!(it.net.get(), it.flushed.get());
            });
        })
        .join()
        .unwrap();
    }

    #[test]
    fn a_bump_under_a_cap_reserves_its_chunks() {
        let (cap, _spills) = Cap::new(1 << 30, 1 << 30);
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
        let (cap, _spills) = Cap::new(100, 100);
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
            let (cap, _spills) = Cap::new(100, 100);
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
    fn spills_hear_once_each_time_spill_at_is_passed() {
        let (cap, spills) = Cap::new(1000, 500);
        let a = cap.reserve(300).unwrap();
        assert_eq!(spills.ready().collect::<Vec<_>>(), []);
        let b = cap.reserve(300).unwrap();
        assert_eq!(spills.ready().collect::<Vec<_>>(), [100]);
        let c = cap.reserve(100).unwrap();
        assert_eq!(spills.ready().collect::<Vec<_>>(), []);
        drop((a, b, c));
        let d = cap.reserve(650).unwrap();
        assert_eq!(spills.ready().collect::<Vec<_>>(), [150]);
        drop((d, cap));
        assert_eq!(spills.next(), None, "the cap and its grants are gone");
    }

    /// A cap whose spiller is gone still grants, and passing `spill_at` changes nothing else.
    #[test]
    fn a_cap_with_no_spiller_still_grants() {
        let (cap, spills) = Cap::new(100, 10);
        drop(spills);
        let grant = cap.reserve(50).unwrap();
        assert_eq!((grant.bytes(), cap.held()), (50, 50));
    }

    /// The spiller, on a thread of its own, gives back grants another thread took: a reserve
    /// waiting at the limit goes on once the spill it caused has dropped the cold values.
    #[test]
    fn a_spiller_gives_grants_back_and_a_waiting_reserve_goes_on() {
        let (cap, spills) = Cap::new(100, 50);
        let cold: Arc<Mutex<Vec<Grant>>> = Arc::new(Mutex::new(Vec::new()));
        std::thread::scope(|scope| {
            let spiller = {
                let cold = Arc::clone(&cold);
                scope.spawn(move || {
                    let over = spills.next().expect("a pass of `spill_at`");
                    cold.lock().unwrap().clear();
                    over
                })
            };
            // Taken by a task that has ended, so the waiter below is never the one cancelled.
            let task = scope.spawn(|| {
                cold.lock().unwrap().push(cap.reserve(40).unwrap());
                cold.lock().unwrap().push(cap.reserve(20).unwrap());
            });
            task.join().unwrap();
            let warm = cap.reserve(70).expect("the spill gives the cold bytes back");
            assert_eq!(spiller.join().unwrap(), 10);
            assert_eq!(cap.held(), 70);
            drop(warm);
        });
        assert_eq!(cap.held(), 0);
    }

    #[test]
    #[should_panic(expected = "more than the daemon's cap")]
    fn a_reserve_bigger_than_the_cap_is_refused() {
        _ = Cap::new(10, 10).0.reserve(11);
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

/// Under the model checker (`RUSTFLAGS="--cfg blaze_check" blaze test -p blaze-graph --release
/// model_`): a [`Cap`]'s total against the bytes held. [`HELD`] and the counts beside it are not
/// modelled: they are the allocator's, which the checker itself goes through.
#[cfg(all(test, blaze_check))]
mod model {
    use std::sync::Arc;

    use blaze_check::{
        linear::{History, Spec},
        model,
    };

    use super::{Cap, Grant};
    use crate::{Cancelled, sync::thread};

    const LIMIT: u64 = 100;

    #[derive(Debug, Clone, Copy)]
    enum Op {
        Reserve(u64),
        Release(u64),
    }

    #[derive(Debug, Clone, Copy, PartialEq)]
    enum Ret {
        Granted,
        Cancelled,
        Released,
    }

    /// The sequential cap: the bytes held. A reserve is granted when it fits; one that does not
    /// fit when it ends was cancelled.
    #[derive(Clone, PartialEq, Eq, Hash, Default)]
    struct Held(u64);

    impl Spec for Held {
        type Op = Op;
        type Ret = Ret;

        fn apply(&mut self, op: &Op) -> Ret {
            match *op {
                Op::Reserve(bytes) if self.0 + bytes <= LIMIT => {
                    self.0 += bytes;
                    Ret::Granted
                }
                Op::Reserve(_) => Ret::Cancelled,
                Op::Release(bytes) => {
                    self.0 -= bytes;
                    Ret::Released
                }
            }
        }
    }

    fn reserve(history: &History<Held>, cap: &Cap, bytes: u64) -> Option<Grant> {
        let mut grant = None;
        history.record(Op::Reserve(bytes), |_| match cap.reserve(bytes) {
            Ok(it) => {
                grant = Some(it);
                Ret::Granted
            }
            Err(Cancelled) => Ret::Cancelled,
        });
        grant
    }

    fn release(history: &History<Held>, grant: Grant) {
        history.record(Op::Release(grant.bytes()), |_| {
            drop(grant);
            Ret::Released
        });
    }

    /// Reserves `first`, then `then` while holding it, and gives both back: whether `then` was
    /// cancelled. A first reserve holds nothing while it waits, so it is never the one cancelled.
    fn hold_then_ask(history: &History<Held>, cap: &Cap, first: u64, then: u64) -> bool {
        let first = reserve(history, cap, first).expect("a thread holding nothing is not cancelled");
        let then = reserve(history, cap, then);
        let cancelled = then.is_none();
        if let Some(then) = then {
            release(history, then);
        }
        release(history, first);
        cancelled
    }

    /// Two threads each hold some of a cap of 100 and ask for more (50 then 40, 30 then 30).
    /// When both wait with their first grants held, nothing can be given back: the newer wait
    /// is cancelled, its bytes go back, and the other's fits. Each grant fit when given, each
    /// cancel did not, at most one is cancelled, nothing deadlocks, and all is given back.
    #[test]
    fn model_a_full_cap_cancels_one_of_two_stuck_holders() {
        model(|| {
            let (cap, _spills) = Cap::new(LIMIT, LIMIT);
            let history = Arc::new(History::<Held>::new());
            let other = {
                let (cap, history) = (cap.clone(), history.clone());
                thread::spawn(move || hold_then_ask(&history, &cap, 30, 30))
            };
            let mine = hold_then_ask(&history, &cap, 50, 40);
            let theirs = other.join().unwrap();
            history.check(Held::default());
            assert!(!(mine && theirs), "both stuck holders were cancelled");
            assert_eq!((cap.held(), cap.waiting()), (0, 0));
        });
    }
}

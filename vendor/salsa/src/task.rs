//! Tasks: what a thread runs now, and each task's own query state.
//!
//! A database is shared by `&` between threads, and one thread can run several tasks of a work
//! pool at once, each above the one below it: a task that waits runs (pumps) other jobs while it
//! waits, and such a job is a task of its own that the one below can't go on before. Each task
//! has its own [`ZalsaLocal`] (query stack, cancellation token, pages it allocates in): what a
//! pumped job reads is never recorded as a dependency of the query waiting below it, it starts
//! with an empty stack, and its claims are told apart from those of the tasks below it.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use crate::sync::thread::{self, ThreadId};
use crate::zalsa_local::ZalsaLocal;

static TASK_DEPTH: OnceLock<fn() -> usize> = OnceLock::new();

/// Sets how salsa learns which task the current thread runs: `depth()` is how many tasks the
/// thread runs above the one at the bottom of its stack (blaze's daemon passes
/// `blaze_graph::pump::depth`). Unset, every thread runs one task, at depth 0.
///
/// # Panics
///
/// If it was set before.
pub fn set_task_depth(depth: fn() -> usize) {
    assert!(
        TASK_DEPTH.set(depth).is_ok(),
        "`salsa::set_task_depth` may be called once per process"
    );
}

#[inline]
pub(crate) fn current_depth() -> u32 {
    TASK_DEPTH.get().map_or(0, |depth| {
        u32::try_from(depth()).expect("a thread runs more than `u32::MAX` tasks")
    })
}

/// A task: the thread that runs it and how many tasks that thread runs below it.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct TaskId {
    thread: ThreadId,
    depth: u32,
}

impl TaskId {
    /// The task the current thread runs now.
    #[inline]
    pub(crate) fn current() -> Self {
        Self {
            thread: thread::current().id(),
            depth: current_depth(),
        }
    }

    /// The thread that runs this task.
    pub fn thread(self) -> ThreadId {
        self.thread
    }

    /// How many tasks the thread runs below this one.
    pub fn depth(self) -> u32 {
        self.depth
    }

    /// Whether `self` runs on `other`'s thread below it, so it can't go on before `other`
    /// returns.
    pub(crate) fn is_below(self, other: TaskId) -> bool {
        self.thread == other.thread && self.depth < other.depth
    }
}

static NEXT_DATABASE: AtomicU64 = AtomicU64::new(0);

/// The task-local state of one database, kept per thread in [`ENTRIES`]. Owned by the database's
/// [`Storage`](crate::Storage).
pub(crate) struct TaskLocals {
    database: u64,
    /// Cleared when the database is dropped, so other threads drop its entries.
    alive: Arc<AtomicBool>,
}

struct Entry {
    database: u64,
    depth: u32,
    alive: Arc<AtomicBool>,
    local: Box<ZalsaLocal>,
}

crate::sync::thread_local! {
    /// The task-local state this thread holds, for every database and task depth it ran.
    static ENTRIES: RefCell<Vec<Entry>> = RefCell::new(Vec::new());
}

impl TaskLocals {
    pub(crate) fn new() -> Self {
        Self {
            database: NEXT_DATABASE.fetch_add(1, Ordering::Relaxed),
            alive: Arc::new(AtomicBool::new(true)),
        }
    }

    /// The state of the task the current thread runs now, made empty the first time.
    #[inline]
    pub(crate) fn current(&self) -> &ZalsaLocal {
        let depth = current_depth();
        let local: *const ZalsaLocal = ENTRIES.with(|entries| {
            let mut entries = entries.borrow_mut();
            if let Some(entry) = entries
                .iter()
                .find(|entry| entry.database == self.database && entry.depth == depth)
            {
                return &*entry.local as *const ZalsaLocal;
            }
            self.insert(&mut entries, depth)
        });
        // SAFETY: The `ZalsaLocal` is boxed, so it stays where it is while `ENTRIES` changes. It
        // is freed only when its database was dropped (`Drop for TaskLocals`, or the pruning in
        // `insert` of entries whose `alive` is cleared), which needs `&mut` to the storage and so
        // ends every `&self` borrow and every reference returned from it, or when this thread's
        // thread-locals are destroyed, after the thread ran its code. `ZalsaLocal` is `!Sync`, so
        // the reference does not leave this thread.
        unsafe { &*local }
    }

    #[cold]
    #[inline(never)]
    fn insert(&self, entries: &mut Vec<Entry>, depth: u32) -> *const ZalsaLocal {
        entries.retain(|entry| entry.alive.load(Ordering::Acquire));
        entries.push(Entry {
            database: self.database,
            depth,
            alive: self.alive.clone(),
            local: Box::new(ZalsaLocal::new()),
        });
        let entry = entries.last().expect("an entry was just pushed");
        &*entry.local as *const ZalsaLocal
    }
}

impl Drop for TaskLocals {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
        // Entries of other threads are dropped by those threads the next time they add one.
        // `try_with` fails only while this thread's thread-locals are being destroyed, which
        // drops the entries anyway.
        let _ = ENTRIES.try_with(|entries| {
            entries
                .borrow_mut()
                .retain(|entry| entry.database != self.database);
        });
    }
}

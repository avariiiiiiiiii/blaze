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
use std::sync::Arc;

use crate::sync::Mutex;
use crate::sync::thread::{self, ThreadId};
use crate::table::{PageIndex, Table};
use crate::zalsa::IngredientIndex;
use crate::zalsa_local::ZalsaLocal;

/// A task: the thread that runs it and how many tasks that thread runs below it.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct TaskId {
    thread: ThreadId,
    depth: u32,
}

impl TaskId {
    /// The task the current thread runs at `depth` ([`Database::task_depth`]).
    ///
    /// [`Database::task_depth`]: crate::Database::task_depth
    #[inline]
    pub(crate) fn current(depth: u32) -> Self {
        Self {
            thread: thread::current().id(),
            depth,
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
    shared: Arc<Shared>,
}

/// What a database's entries on every thread share with it.
#[derive(Default)]
struct Shared {
    /// Cleared when the database is dropped, so other threads drop its entries.
    dropped: AtomicBool,
    /// Pages that entries dropped with their thread allocated in and did not fill, for the
    /// database to hand to its table at the next write.
    unfilled: Mutex<Vec<(IngredientIndex, PageIndex)>>,
}

// A panic can't leave `unfilled` inconsistent: it is only extended and taken whole.
impl std::panic::RefUnwindSafe for Shared {}

struct Entry {
    database: u64,
    depth: u32,
    shared: Arc<Shared>,
    local: Box<ZalsaLocal>,
}

impl Drop for Entry {
    fn drop(&mut self) {
        if self.shared.dropped.load(Ordering::Acquire) {
            return;
        }
        let mut unfilled = self.shared.unfilled.lock();
        unfilled.extend(self.local.take_unfilled_pages());
    }
}

crate::sync::thread_local! {
    /// The task-local state this thread holds, for every database and task depth it ran.
    // alloc: `Vec::new` allocates nothing; one entry per database and task depth a thread runs
    static ENTRIES: RefCell<Vec<Entry>> = RefCell::new(Vec::new());
}

impl TaskLocals {
    pub(crate) fn new() -> Self {
        Self {
            database: NEXT_DATABASE.fetch_add(1, Ordering::Relaxed),
            shared: Arc::default(),
        }
    }

    /// Hands the pages of tasks whose thread ended to `table`, so later allocations fill them.
    pub(crate) fn return_unfilled_pages(&mut self, table: &Table) {
        let unfilled = std::mem::take(&mut *self.shared.unfilled.lock());
        for (ingredient, page) in unfilled {
            table.record_unfilled_page(ingredient, page);
        }
    }

    /// The state of the task the current thread runs now, made empty the first time. `depth` is
    /// the database's [`Database::task_depth`](crate::Database::task_depth).
    #[inline]
    pub(crate) fn current(&self, depth: u32) -> &ZalsaLocal {
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
        // `insert` of entries whose `dropped` is set), which needs ownership of the storage and so
        // ends every `&self` borrow and every reference returned from it, or when this thread's
        // thread-locals are destroyed, after the thread ran its code. `ZalsaLocal` is `!Sync`, so
        // the reference does not leave this thread.
        unsafe { &*local }
    }

    #[cold]
    #[inline(never)]
    fn insert(&self, entries: &mut Vec<Entry>, depth: u32) -> *const ZalsaLocal {
        entries.retain(|entry| !entry.shared.dropped.load(Ordering::Acquire));
        entries.push(Entry {
            database: self.database,
            depth,
            // alloc: once per database and task depth a thread first runs
            shared: self.shared.clone(),
            // alloc: once per database and task depth a thread first runs
            local: Box::new(ZalsaLocal::new(TaskId::current(depth))),
        });
        let entry = entries.last().expect("an entry was just pushed");
        &*entry.local as *const ZalsaLocal
    }
}

impl Drop for TaskLocals {
    fn drop(&mut self) {
        self.shared.dropped.store(true, Ordering::Release);
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

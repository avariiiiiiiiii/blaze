use self::dependency_graph::DependencyGraph;
use crate::durability::Durability;
use crate::function::{SyncGuard, SyncOwner};
use crate::key::DatabaseKeyIndex;
use crate::sync::Mutex;
use crate::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use crate::table::Table;
use crate::task::TaskId;
use crate::zalsa::Zalsa;
use crate::{Cancelled, Event, EventKind, Revision};

mod dependency_graph;

#[cfg_attr(feature = "persistence", derive(serde::Serialize, serde::Deserialize))]
pub struct Runtime {
    /// Distinguishes provisional cycle results created before and after a write within the same
    /// revision. Reset when the revision advances.
    #[cfg_attr(feature = "persistence", serde(skip))]
    cancellation_count: AtomicU8,

    /// Set by [`Database::stop_readers`](crate::Database::stop_readers): every query that
    /// starts from now unwinds with [`Cancelled::PendingWrite`], cycle heads included, until
    /// the next write clears it. Read on every query call next to `revisions[0]`.
    #[cfg_attr(feature = "persistence", serde(skip))]
    readers_stopped: AtomicBool,

    /// Stores the "last change" revision for values of each duration.
    /// This vector is always of length at least 1 (for Durability 0)
    /// but its total length depends on the number of durations. The
    /// element at index 0 is special as it represents the "current
    /// revision".  In general, we have the invariant that revisions
    /// in here are *declining* -- that is, `revisions[i] >=
    /// revisions[i + 1]`, for all `i`. This is because when you
    /// modify a value with durability D, that implies that values
    /// with durability less than D may have changed too.
    revisions: [Revision; Durability::LEN],

    /// The dependency graph tracks which tasks are blocked on one
    /// another, waiting for queries to terminate.
    #[cfg_attr(feature = "persistence", serde(skip))]
    dependency_graph: Mutex<DependencyGraph>,

    /// Data for instances
    #[cfg_attr(feature = "persistence", serde(skip))]
    table: Table,
}

#[derive(Copy, Clone, Debug)]
pub(super) enum WaitResult {
    Completed,
    Panicked,
    Cancelled,
}

#[derive(Debug)]
pub(crate) enum BlockResult<'me> {
    /// The query is running on another thread.
    Running(Running<'me>),

    /// Blocking resulted in a cycle.
    ///
    /// The lock is held by the current task, or another task waits for the current task through
    /// query waits alone, and blocking would result in a deadlock/cycle.
    Cycle,
}

pub(crate) enum BlockTransferredResult<'me> {
    /// The current task is the owner of the transferred query
    /// and it can claim it if it wants to.
    ImTheOwner,

    /// The query is owned/running on another task.
    OwnedBy(Box<BlockOnTransferredOwner<'me>>),

    /// The query has transferred its ownership to another query previously but that query has
    /// since then completed and released the lock.
    Released,
}

pub(super) struct BlockOnTransferredOwner<'me> {
    dg: crate::sync::MutexGuard<'me, DependencyGraph>,
    /// The query that we're trying to claim.
    database_key: DatabaseKeyIndex,
    /// The task that currently owns the lock for the transferred query.
    other_id: TaskId,
    /// The current task that is trying to claim the transferred query.
    thread_id: TaskId,
}

impl<'me> BlockOnTransferredOwner<'me> {
    /// Block on the other task to complete the computation.
    pub(super) fn block(self, query_mutex_guard: SyncGuard<'me>) -> BlockResult<'me> {
        let Self {
            dg,
            database_key,
            other_id,
            thread_id,
        } = self;
        block_checked(dg, database_key, other_id, thread_id, query_mutex_guard)
    }
}

/// Blocks `task_id` on `other_id`, which runs `database_key`, after checking that the wait
/// can't deadlock.
///
/// # Cancellation
///
/// A wait on a task below this one on its thread, or one that would close a cycle through a
/// task that pumped the current one, throws [`Cancelled::WouldWaitOnItself`]: no query cycle,
/// so the work pool reposts the task.
fn block_checked<'me>(
    dg: crate::sync::MutexGuard<'me, DependencyGraph>,
    database_key: DatabaseKeyIndex,
    other_id: TaskId,
    task_id: TaskId,
    query_mutex_guard: SyncGuard<'me>,
) -> BlockResult<'me> {
    // Cycle in the same task.
    if task_id == other_id {
        return BlockResult::Cycle;
    }

    let closes = if other_id.is_below(task_id) {
        dependency_graph::Closes::ThroughPumpedTask
    } else {
        dg.wait_closes(other_id, task_id)
    };
    match closes {
        // alloc: contention only: one box when a task blocks on a key another task is running
        dependency_graph::Closes::No => BlockResult::Running(Running(Box::new(BlockedOnInner {
            dg,
            query_mutex_guard,
            database_key,
            other_id,
            thread_id: task_id,
        }))),
        dependency_graph::Closes::QueryCycle => {
            crate::tracing::debug!(
                "block_on: cycle detected for {database_key:?} in task {task_id:?} on {other_id:?}"
            );
            BlockResult::Cycle
        }
        dependency_graph::Closes::ThroughPumpedTask => {
            crate::tracing::debug!(
                "block_on: task {task_id:?} would wait on itself for {database_key:?} in {other_id:?}"
            );
            drop(dg);
            drop(query_mutex_guard);
            Cancelled::WouldWaitOnItself.throw()
        }
    }
}

pub struct Running<'me>(Box<BlockedOnInner<'me>>);

struct BlockedOnInner<'me> {
    dg: crate::sync::MutexGuard<'me, DependencyGraph>,
    query_mutex_guard: SyncGuard<'me>,
    database_key: DatabaseKeyIndex,
    other_id: TaskId,
    thread_id: TaskId,
}

impl Running<'_> {
    /// Blocks on the other thread to complete the computation.
    ///
    /// Returns `true` if the computation was successful, and `false` if the other thread was locally cancelled.
    ///
    /// # Panics
    ///
    /// If the other thread panics, this function will panic as well.
    #[must_use]
    pub(crate) fn block_on(self, zalsa: &Zalsa) -> bool {
        let BlockedOnInner {
            dg,
            query_mutex_guard,
            database_key,
            other_id,
            thread_id,
        } = *self.0;

        zalsa.event(&|| {
            Event::new(EventKind::WillBlockOn {
                other_task: other_id,
                database_key,
            })
        });

        crate::tracing::debug!(
            "block_on: task {thread_id:?} is blocking on {database_key:?} in task {other_id:?}",
        );

        let result =
            DependencyGraph::block_on(dg, thread_id, database_key, other_id, query_mutex_guard);

        match result {
            WaitResult::Panicked => {
                // If the other thread panicked, then we consider this thread
                // cancelled. The assumption is that the panic will be detected
                // by the other thread and responded to appropriately.
                Cancelled::PropagatedPanic.throw()
            }
            WaitResult::Cancelled => false,
            WaitResult::Completed => true,
        }
    }
}

impl std::fmt::Debug for Running<'_> {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt.debug_struct("Running")
            .field("database_key", &self.0.database_key)
            .field("other_id", &self.0.other_id)
            .field("thread_id", &self.0.thread_id)
            .finish()
    }
}

#[derive(Copy, Clone, Debug)]
pub struct Stamp {
    pub durability: Durability,
    pub changed_at: Revision,
}

pub fn stamp(revision: Revision, durability: Durability) -> Stamp {
    Stamp {
        durability,
        changed_at: revision,
    }
}

impl Default for Runtime {
    fn default() -> Self {
        Runtime {
            revisions: [Revision::start(); Durability::LEN],
            cancellation_count: Default::default(),
            readers_stopped: Default::default(),
            dependency_graph: Default::default(),
            table: Default::default(),
        }
    }
}

impl std::fmt::Debug for Runtime {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt.debug_struct("Runtime")
            .field("revisions", &self.revisions)
            .field("dependency_graph", &self.dependency_graph)
            .finish()
    }
}

impl Runtime {
    #[inline]
    pub(crate) fn current_revision(&self) -> Revision {
        self.revisions[0]
    }

    /// Reports that an input with durability `durability` changed.
    /// This will update the 'last changed at' values for every durability
    /// less than or equal to `durability` to the current revision.
    ///
    /// # Panics
    ///
    /// Panics if `durability` is [`Durability::NEVER_CHANGE`].
    pub(crate) fn report_tracked_write(&mut self, durability: Durability) {
        assert_ne!(
            durability,
            Durability::NEVER_CHANGE,
            "never-changing inputs cannot be mutated"
        );
        let new_revision = self.current_revision();
        self.revisions[1..=durability.index()].fill(new_revision);
    }

    /// The revision in which values with durability `d` may have last
    /// changed.  For D0, this is just the current revision. But for
    /// higher levels of durability, this value may lag behind the
    /// current revision. If we encounter a value of durability Di,
    /// then, we can check this function to get a "bound" on when the
    /// value may have changed, which allows us to skip walking its
    /// dependencies.
    #[inline]
    pub(crate) fn last_changed_revision(&self, d: Durability) -> Revision {
        match self.revisions.get(d.index()) {
            Some(&revision) => revision,
            None => never_changed_revision(),
        }
    }

    pub(crate) fn cancellation_count(&self) -> u8 {
        self.cancellation_count.load(Ordering::Acquire)
    }

    pub(crate) fn bump_cancellation_count(&mut self) -> bool {
        let count = self.cancellation_count.get_mut();
        let Some(next) = count.checked_add(1) else {
            return true;
        };
        *count = next;
        false
    }

    #[inline]
    pub(crate) fn readers_stopped(&self) -> bool {
        self.readers_stopped.load(Ordering::Relaxed)
    }

    pub(crate) fn stop_readers(&self) {
        self.readers_stopped.store(true, Ordering::Relaxed);
    }

    pub(crate) fn clear_stop(&mut self) {
        *self.readers_stopped.get_mut() = false;
    }

    /// Returns the [`Table`] used to store the value of salsa structs
    #[inline]
    pub(crate) fn table(&self) -> &Table {
        &self.table
    }

    pub(crate) fn table_mut(&mut self) -> &mut Table {
        &mut self.table
    }

    /// Increments the "current revision" counter and resets
    /// the cancellation count.
    ///
    /// This should only be done by the storage when the state is "quiescent".
    pub(crate) fn new_revision(&mut self) -> Revision {
        let r_old = self.current_revision();
        let r_new = r_old.next();
        self.revisions[0] = r_new;
        *self.cancellation_count.get_mut() = 0;
        crate::tracing::info!("new_revision: {r_old:?} -> {r_new:?}");
        r_new
    }

    /// Block until `other_id` completes executing `database_key`, or return `BlockResult::Cycle`
    /// immediately in case of a cycle.
    ///
    /// `query_mutex_guard` is the guard for the current query's state;
    /// it will be dropped after we have successfully registered the
    /// dependency.
    ///
    /// # Propagating panics
    ///
    /// If the task `other_id` panics, then our task is considered
    /// cancelled, so this function will panic with a `Cancelled` value. A wait that can't
    /// end because `other_id` runs below this task on its thread (directly or through other
    /// tasks' waits) panics with [`Cancelled::WouldWaitOnItself`].
    pub(crate) fn block<'a>(
        &'a self,
        database_key: DatabaseKeyIndex,
        other_id: TaskId,
        task_id: TaskId,
        query_mutex_guard: SyncGuard<'a>,
    ) -> BlockResult<'a> {
        block_checked(
            self.dependency_graph.lock(),
            database_key,
            other_id,
            task_id,
            query_mutex_guard,
        )
    }

    /// Tries to claim ownership of a transferred query where `current_id` is the current task and `query`
    /// is the query (that had its ownership transferred) to claim.
    ///
    /// For this operation to be reasonable, the caller must ensure that the sync table lock on `query` is not released
    /// before this operation completes.
    pub(super) fn block_transferred(
        &self,
        query: DatabaseKeyIndex,
        current_id: TaskId,
    ) -> BlockTransferredResult<'_> {
        let dg = self.dependency_graph.lock();

        let owner_thread = dg.thread_id_of_transferred_query(query, None);

        let Some(owner_thread_id) = owner_thread else {
            // The query transferred its ownership but the owner has since then released the lock.
            return BlockTransferredResult::Released;
        };

        if owner_thread_id == current_id || dg.depends_on(owner_thread_id, current_id) {
            BlockTransferredResult::ImTheOwner
        } else {
            // Lock is owned by another thread, wait for it to be released.
            // alloc: contention only: one box when a task blocks on a key another task is running
            BlockTransferredResult::OwnedBy(Box::new(BlockOnTransferredOwner {
                dg,
                database_key: query,
                other_id: owner_thread_id,
                thread_id: current_id,
            }))
        }
    }

    /// Invoked when this runtime completed computing `database_key` with
    /// the given result `wait_result`.
    /// This function unblocks any dependent queries and allows them
    /// to continue executing.
    pub(crate) fn unblock_queries_blocked_on(
        &self,
        database_key: DatabaseKeyIndex,
        wait_result: WaitResult,
    ) {
        self.dependency_graph
            .lock()
            .unblock_runtimes_blocked_on(database_key, wait_result);
    }

    /// Unblocks all transferred queries that are owned by `database_key` recursively.
    ///
    /// Invoked when a query completes that has been marked as transfer target (it has
    /// queries that transferred their lock ownership to it) with the given `wait_result`.
    ///
    /// This function unblocks any dependent queries and allows them to continue executing. The
    /// query `database_key` is not unblocked by this function.
    #[cold]
    pub(crate) fn unblock_transferred_queries_owned_by(
        &self,
        database_key: DatabaseKeyIndex,
        wait_result: WaitResult,
    ) {
        self.dependency_graph
            .lock()
            .unblock_runtimes_blocked_on_transferred_queries_owned_by(database_key, wait_result);
    }

    /// Removes the ownership transfer of `query`'s lock if it exists.
    ///
    /// If `query` has transferred its lock ownership to another query, this function will remove that transfer,
    /// so that `query` now owns its lock again.
    #[cold]
    pub(super) fn undo_transfer_lock(&self, query: DatabaseKeyIndex) {
        self.dependency_graph.lock().undo_transfer_lock(query);
    }

    /// Transfers ownership of the lock for `query` to `new_owner_key`.
    ///
    /// For this operation to be reasonable, the caller must ensure that the sync table lock on `query` is not released
    /// and that `new_owner_key` is currently blocked on `query`. Otherwise, `new_owner_key` might
    /// complete before the lock is transferred, leaving `query` locked forever.
    pub(super) fn transfer_lock(
        &self,
        query: DatabaseKeyIndex,
        current_id: TaskId,
        new_owner_key: DatabaseKeyIndex,
        new_owner_id: SyncOwner,
        guard: SyncGuard,
    ) -> bool {
        let dg = self.dependency_graph.lock();
        DependencyGraph::transfer_lock(
            dg,
            query,
            current_id,
            new_owner_key,
            new_owner_id,
            guard,
        )
    }

    #[cfg(feature = "persistence")]
    pub(crate) fn deserialize_from(&mut self, other: &mut Runtime) {
        // The only field that is serialized is `revisions`.
        self.revisions = other.revisions;
    }
}

#[cold]
#[inline(never)]
fn never_changed_revision() -> Revision {
    Revision::start()
}

#[cfg(test)]
mod tests {
    use super::Runtime;
    use crate::Revision;

    #[test]
    fn cancellation_count_overflow_requires_revision_bump() {
        let mut runtime = Runtime::default();

        for _ in 0..u8::MAX {
            assert!(!runtime.bump_cancellation_count());
        }

        assert!(runtime.bump_cancellation_count());
        runtime.new_revision();

        assert_eq!(runtime.current_revision(), Revision::start().next());
        assert_eq!(runtime.cancellation_count(), 0);
    }
}

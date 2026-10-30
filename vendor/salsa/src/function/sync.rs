//! Claims: one task runs a key at a time (docs/v2/plan/lanes/db/speed-of-light.md, "Claims").
//!
//! A key's claim is a word in its function's memo column, at the key's row
//! ([`MemoTableWithTypes::claim`]), found from the page exactly as the row's memo is. It holds
//!
//! - [`FREE`]: no task runs the key;
//! - a task's token, the address of its [`ZalsaLocal`] (even): that task runs the key, nobody
//!   waits for it and it has no rare state;
//! - [`SLOW`]: the key's state is its entry in the shard table, read and written under the
//!   shard's lock: a claim another task waits on or peeked at, a transfer target, a transferred
//!   or twice-claimed key.
//!
//! An uncontended claim is one CAS from `FREE` to the token and its release one CAS back: no
//! lock, no hash. Those two are the only changes made without the shard lock, and only to words
//! that aren't `SLOW`; every other change (a token to `SLOW`, and anything from `SLOW`) is made
//! holding it. So under the lock a `SLOW` word always has its entry and the entry is the whole
//! state, while a token or `FREE` means there is no entry. A contender turns the owner's token
//! into `SLOW` with a CAS under the lock and then makes the entry; once that CAS wins, the
//! owner's release CAS fails and its release waits for the lock, so the owner, and the
//! `ZalsaLocal` its guard borrows, outlives the contender's read of the task behind the token
//! ([`owner_of`]). Waiting, wake-up, cycle detection and ownership transfer then run exactly as
//! they did when every claim was an entry: through the entry and the runtime's dependency graph.
//!
//! A row serves every generation of its slot, so entries are keyed by the slot (`Id::index`). A
//! slot is reused only for an id nobody runs in the current revision (a tracked struct is
//! deleted only if no query read it this revision, an interned value reused only when stale),
//! so two generations are never claimed at once; a stale `Transferred` entry of an old
//! generation is reclaimed by the new one like any released transfer.
//!
//! [`MemoTableWithTypes::claim`]: crate::table::memo::MemoTableWithTypes::claim

use crossbeam_utils::CachePadded;
use hashbrown::HashTable;
use hashbrown::hash_table::OccupiedEntry;
use portable_atomic::AtomicU64;
use rustc_hash::FxBuildHasher;
use std::hash::BuildHasher;
use std::sync::atomic::Ordering;

use crate::key::DatabaseKeyIndex;
use crate::plumbing::ZalsaLocal;
use crate::runtime::{
    BlockOnTransferredOwner, BlockResult, BlockTransferredResult, Running, WaitResult,
};
use crate::sync::thread::{self};
use crate::sync::{Mutex, max_parallelism};
use crate::table::memo::ClaimWord;
use crate::task::TaskId;
use crate::tracing;
use crate::zalsa::{MemoIngredientIndex, Zalsa, ZalsaDatabase};
use crate::{Database, Id, IngredientIndex};

pub(crate) type SyncGuard<'me> = crate::sync::MutexGuard<'me, HashTable<SyncState>>;

/// A claim word no task holds.
const FREE: usize = 0;

/// A claim word whose key's state is its shard table entry. Odd, so never a token.
const SLOW: usize = 1;

const _: () = assert!(std::mem::align_of::<ZalsaLocal>() > SLOW);

/// Entries of the struct ingredient → memo column cache.
const COLUMN_CACHE: usize = 4;

/// An empty column cache entry: no struct ingredient packs to it (they pack as `index + 1`).
const NO_COLUMN: u64 = 0;

/// Tracks the keys that are currently being processed; used to coordinate between
/// worker threads.
pub(crate) struct SyncTable {
    shards: Box<[CachePadded<SyncShard>]>,
    ingredient: IngredientIndex,

    /// This function's memo column in the tables of recently claimed struct ingredients, by the
    /// struct ingredient's index: `(struct + 1) << 32 | column`. A function's column in a
    /// struct's tables never changes once the database is made, so entries can't go stale;
    /// one struct ingredient (every function but those on supertype enums) always hits.
    columns: [AtomicU64; COLUMN_CACHE],
}

struct SyncShard {
    syncs: Mutex<HashTable<SyncState>>,
}

pub(crate) enum ClaimResult<'a, Guard = ClaimGuard<'a>> {
    /// Successfully claimed the query.
    Claimed(Guard),
    /// Can't claim the query because it is running on an other thread.
    Running(Running<'a>),
    /// Claiming the query results in a cycle.
    Cycle {
        /// `true` if this is a cycle with an inner query. For example, if `a` transferred its ownership to
        /// `b`. If the thread claiming `b` tries to claim `a`, then this results in a cycle except when calling
        /// [`SyncTable::try_claim`] with [`Reentrant::Allow`].
        inner: bool,
    },
}

#[derive(Debug)]
pub(crate) struct SyncState {
    key: Id,

    /// The thread id that currently owns this query (actively executing it or iterating it as part of a larger cycle).
    id: SyncOwner,

    /// Set to true if any other queries are blocked,
    /// waiting for this query to complete.
    anyone_waiting: bool,

    /// Whether any other query has transferred its lock ownership to this query.
    /// This is only an optimization so that the expensive unblocking of transferred queries
    /// can be skipped if `false`. This field might be `true` in cases where queries *were* transferred
    /// to this query, but have since then been transferred to another query (in a later iteration).
    is_transfer_target: bool,

    /// Whether this query has been claimed by the query that currently owns it.
    ///
    /// If `a` has been transferred to `b` and the stack for t1 is `b -> a`, then `a` can be claimed
    /// and `claimed_twice` is set to `true`. However, t2 won't be able to claim `a` because
    /// it doesn't own `b`.
    claimed_twice: bool,
}

impl SyncTable {
    pub(crate) fn new(ingredient: IngredientIndex) -> Self {
        // Four shards per core (concurrency.md), so claims on different keys rarely share a lock.
        let shard_count = (max_parallelism() * 4).next_power_of_two();

        Self {
            shards: (0..shard_count)
                .map(|_| {
                    CachePadded::new(SyncShard {
                        syncs: Mutex::default(),
                    })
                })
                // alloc: setup, once per ingredient when its jar registers
                .collect(),
            ingredient,
            columns: [const { AtomicU64::new(NO_COLUMN) }; COLUMN_CACHE],
        }
    }

    #[inline]
    fn shard_index(&self, hash: u64) -> usize {
        let hash = hash as usize;
        let shift = usize::BITS - self.shards.len().trailing_zeros();
        (hash << 7) >> shift
    }

    #[inline]
    fn shard_for(&self, hash: u64) -> &SyncShard {
        &self.shards[self.shard_index(hash)]
    }

    /// The claim word of `key` for this function: in the function's memo column of the key's
    /// struct ingredient, at the key's row.
    #[inline]
    fn word<'me>(&self, zalsa: &'me Zalsa, key: Id) -> &'me ClaimWord {
        let table = zalsa.table();
        let struct_ingredient = table.ingredient_index(key);
        // SAFETY: Only the row's claim word is taken from the table, and a claim word lives in
        // its column's region for the database's life whatever becomes of the row's slot and
        // memos (`MemoTableWithTypes::claim`), so the slot needn't stay alive.
        let memos = unsafe { table.dyn_memos_unchecked(key) };
        let column = self.column(zalsa, struct_ingredient, memos.column_count());
        memos.claim(column)
    }

    /// This function's memo column in `struct_ingredient`'s tables, which have `columns`.
    #[inline]
    fn column(
        &self,
        zalsa: &Zalsa,
        struct_ingredient: IngredientIndex,
        columns: usize,
    ) -> MemoIngredientIndex {
        let tag = u64::from(struct_ingredient.as_u32()) + 1;
        let entry = &self.columns[struct_ingredient.as_u32() as usize % COLUMN_CACHE];
        // Relaxed: the entry is a pure function of the database's setup, which happens before
        // any claim.
        let cached = entry.load(Ordering::Relaxed);
        if cached >> 32 == tag {
            return MemoIngredientIndex::from_usize(cached as u32 as usize);
        }
        self.find_column(zalsa, struct_ingredient, columns, entry)
    }

    #[cold]
    #[inline(never)]
    fn find_column(
        &self,
        zalsa: &Zalsa,
        struct_ingredient: IngredientIndex,
        columns: usize,
        entry: &AtomicU64,
    ) -> MemoIngredientIndex {
        let Some(column) = (0..columns).map(MemoIngredientIndex::from_usize).find(|&column| {
            zalsa.ingredient_index_for_memo(struct_ingredient, column) == self.ingredient
        }) else {
            panic!(
                "salsa: function ingredient {:?} has no memo column in the tables of {:?}",
                self.ingredient, struct_ingredient
            )
        };
        let tag = u64::from(struct_ingredient.as_u32()) + 1;
        entry.store(tag << 32 | column.as_usize() as u64, Ordering::Relaxed);
        column
    }

    #[inline]
    fn guard<'me>(
        &'me self,
        zalsa: &'me Zalsa,
        zalsa_local: &'me ZalsaLocal,
        key_index: Id,
        word: &'me ClaimWord,
        mode: ReleaseMode,
    ) -> ClaimGuard<'me> {
        ClaimGuard {
            key_index,
            zalsa,
            table: self,
            word,
            mode,
            zalsa_local,
        }
    }

    /// Claims the given key index, or blocks if it is running on another task.
    ///
    /// # Cancellation
    ///
    /// Throws [`Cancelled::WouldWaitOnItself`](crate::Cancelled::WouldWaitOnItself) if the task
    /// that runs it is below this one on its thread.
    #[inline]
    pub(crate) fn try_claim<'me>(
        &'me self,
        zalsa: &'me Zalsa,
        zalsa_local: &'me ZalsaLocal,
        key_index: Id,
        reentrant: Reentrancy,
    ) -> ClaimResult<'me> {
        let word = self.word(zalsa, key_index);
        if claim_word(word, token_of(zalsa_local)) {
            return ClaimResult::Claimed(self.guard(
                zalsa,
                zalsa_local,
                key_index,
                word,
                ReleaseMode::Default,
            ));
        }
        self.try_claim_slow(zalsa, zalsa_local, key_index, word, reentrant)
    }

    #[cold]
    #[inline(never)]
    fn try_claim_slow<'me>(
        &'me self,
        zalsa: &'me Zalsa,
        zalsa_local: &'me ZalsaLocal,
        key_index: Id,
        word: &'me ClaimWord,
        reentrant: Reentrancy,
    ) -> ClaimResult<'me> {
        let token = token_of(zalsa_local);
        let hash = hash_of(key_index);
        let mut write = self.shard_for(hash).syncs.lock();
        loop {
            schedule();
            match word.load(Ordering::Acquire) {
                FREE => {
                    if claim_word(word, token) {
                        return ClaimResult::Claimed(self.guard(
                            zalsa,
                            zalsa_local,
                            key_index,
                            word,
                            ReleaseMode::Default,
                        ));
                    }
                }
                // This task runs the key already: blocking on itself is a cycle, which
                // `Runtime::block` reports before anything else.
                owner if owner == token => return ClaimResult::Cycle { inner: false },
                SLOW => break,
                owner => {
                    if make_slow(&mut write, word, hash, key_index, owner) {
                        break;
                    }
                }
            }
        }

        let occupied_entry = slow_entry(&mut write, hash, key_index);
        let id = match occupied_entry.get().id {
            SyncOwner::Task(id) => id,
            SyncOwner::Transferred => {
                return match self.try_claim_transferred(
                    zalsa,
                    zalsa_local,
                    key_index,
                    word,
                    occupied_entry,
                    reentrant,
                ) {
                    Ok(claimed) => claimed,
                    Err(other_thread) => match other_thread.block(write) {
                        BlockResult::Cycle => ClaimResult::Cycle { inner: false },
                        BlockResult::Running(running) => ClaimResult::Running(running),
                    },
                };
            }
        };

        let SyncState { anyone_waiting, .. } = occupied_entry.into_mut();

        // NB: `Ordering::Relaxed` is sufficient here,
        // as there are no loads that are "gated" on this
        // value. Everything that is written is also protected
        // by a lock that must be acquired. The role of this
        // boolean is to decide *whether* to acquire the lock,
        // not to gate future atomic reads.
        *anyone_waiting = true;
        match zalsa.runtime().block(
            DatabaseKeyIndex::new(self.ingredient, key_index),
            id,
            zalsa_local.task(),
            write,
        ) {
            BlockResult::Running(blocked_on) => ClaimResult::Running(blocked_on),
            BlockResult::Cycle => ClaimResult::Cycle { inner: false },
        }
    }

    /// Claims the given key index for `task`, or blocks if it is running on another task.
    #[inline]
    pub(crate) fn peek_claim<'me>(
        &'me self,
        zalsa: &'me Zalsa,
        task: TaskId,
        key_index: Id,
        reentrant: Reentrancy,
    ) -> ClaimResult<'me, ()> {
        let word = self.word(zalsa, key_index);
        schedule();
        if word.load(Ordering::Acquire) == FREE {
            return ClaimResult::Claimed(());
        }
        self.peek_claim_slow(zalsa, task, key_index, word, reentrant)
    }

    #[cold]
    #[inline(never)]
    fn peek_claim_slow<'me>(
        &'me self,
        zalsa: &'me Zalsa,
        task: TaskId,
        key_index: Id,
        word: &'me ClaimWord,
        reentrant: Reentrancy,
    ) -> ClaimResult<'me, ()> {
        let hash = hash_of(key_index);
        let mut write = self.shard_for(hash).syncs.lock();
        loop {
            schedule();
            match word.load(Ordering::Acquire) {
                FREE => return ClaimResult::Claimed(()),
                SLOW => break,
                owner => {
                    if make_slow(&mut write, word, hash, key_index, owner) {
                        break;
                    }
                }
            }
        }

        let occupied_entry = slow_entry(&mut write, hash, key_index);
        let id = match occupied_entry.get().id {
            SyncOwner::Task(id) => id,
            SyncOwner::Transferred => {
                return match self.peek_claim_transferred(
                    zalsa,
                    task,
                    key_index,
                    occupied_entry,
                    reentrant,
                ) {
                    Ok(claimed) => claimed,
                    Err(other_thread) => match other_thread.block(write) {
                        BlockResult::Cycle => ClaimResult::Cycle { inner: false },
                        BlockResult::Running(running) => ClaimResult::Running(running),
                    },
                };
            }
        };

        let SyncState { anyone_waiting, .. } = occupied_entry.into_mut();

        // NB: `Ordering::Relaxed` is sufficient here,
        // as there are no loads that are "gated" on this
        // value. Everything that is written is also protected
        // by a lock that must be acquired. The role of this
        // boolean is to decide *whether* to acquire the lock,
        // not to gate future atomic reads.
        *anyone_waiting = true;
        match zalsa.runtime().block(
            DatabaseKeyIndex::new(self.ingredient, key_index),
            id,
            task,
            write,
        ) {
            BlockResult::Running(blocked_on) => ClaimResult::Running(blocked_on),
            BlockResult::Cycle => ClaimResult::Cycle { inner: false },
        }
    }

    #[cold]
    #[inline(never)]
    fn try_claim_transferred<'me>(
        &'me self,
        zalsa: &'me Zalsa,
        zalsa_local: &'me ZalsaLocal,
        key_index: Id,
        word: &'me ClaimWord,
        mut entry: OccupiedEntry<SyncState>,
        reentrant: Reentrancy,
    ) -> Result<ClaimResult<'me>, Box<BlockOnTransferredOwner<'me>>> {
        let database_key_index = DatabaseKeyIndex::new(self.ingredient, key_index);
        let thread_id = zalsa_local.task();

        match zalsa
            .runtime()
            .block_transferred(database_key_index, thread_id)
        {
            BlockTransferredResult::ImTheOwner if reentrant.is_allow() => {
                let SyncState {
                    key,
                    id,
                    claimed_twice,
                    ..
                } = entry.into_mut();
                debug_assert!(!*claimed_twice);

                *key = key_index;
                *id = SyncOwner::Task(thread_id);
                *claimed_twice = true;

                Ok(ClaimResult::Claimed(self.guard(
                    zalsa,
                    zalsa_local,
                    key_index,
                    word,
                    ReleaseMode::SelfOnly,
                )))
            }
            BlockTransferredResult::ImTheOwner => Ok(ClaimResult::Cycle { inner: true }),
            BlockTransferredResult::OwnedBy(other_thread) => {
                entry.get_mut().anyone_waiting = true;
                Err(other_thread)
            }
            BlockTransferredResult::Released => {
                reclaim_released(entry, word, token_of(zalsa_local));
                Ok(ClaimResult::Claimed(self.guard(
                    zalsa,
                    zalsa_local,
                    key_index,
                    word,
                    ReleaseMode::Default,
                )))
            }
        }
    }

    #[cold]
    #[inline(never)]
    fn peek_claim_transferred<'me>(
        &'me self,
        zalsa: &'me Zalsa,
        thread_id: TaskId,
        key_index: Id,
        mut entry: OccupiedEntry<SyncState>,
        reentrant: Reentrancy,
    ) -> Result<ClaimResult<'me, ()>, Box<BlockOnTransferredOwner<'me>>> {
        let database_key_index = DatabaseKeyIndex::new(self.ingredient, key_index);

        match zalsa
            .runtime()
            .block_transferred(database_key_index, thread_id)
        {
            BlockTransferredResult::ImTheOwner if reentrant.is_allow() => {
                Ok(ClaimResult::Claimed(()))
            }
            BlockTransferredResult::ImTheOwner => Ok(ClaimResult::Cycle { inner: true }),
            BlockTransferredResult::OwnedBy(other_thread) => {
                entry.get_mut().anyone_waiting = true;
                Err(other_thread)
            }
            BlockTransferredResult::Released => Ok(ClaimResult::Claimed(())),
        }
    }

    /// Marks `key_index` as a transfer target.
    ///
    /// Returns the `SyncOwnerId` of the thread that currently owns this query.
    ///
    /// Note: The result of this method will immediately become stale unless the thread owning `key_index`
    /// is currently blocked on this thread (claiming `key_index` from this thread results in a cycle).
    pub(super) fn mark_as_transfer_target(&self, zalsa: &Zalsa, key_index: Id) -> Option<SyncOwner> {
        let word = self.word(zalsa, key_index);
        let hash = hash_of(key_index);
        let mut syncs = self.shard_for(hash).syncs.lock();
        loop {
            schedule();
            match word.load(Ordering::Acquire) {
                FREE => return None,
                SLOW => break,
                owner => {
                    if make_slow(&mut syncs, word, hash, key_index, owner) {
                        break;
                    }
                }
            }
        }
        let state = slow_entry(&mut syncs, hash, key_index).into_mut();
        // We set `anyone_waiting` to true because it is used in `ClaimGuard::release`
        // to exit early if the query doesn't need to release any locks.
        // However, there are now dependent queries that need to be released, that's why we set `anyone_waiting` to true,
        // so that `ClaimGuard::release` no longer exits early.
        state.anyone_waiting = true;
        state.is_transfer_target = true;

        Some(state.id)
    }
}

/// The token a task's claims hold: its `ZalsaLocal`'s address, with its provenance exposed for
/// [`owner_of`].
#[inline]
fn token_of(zalsa_local: &ZalsaLocal) -> usize {
    std::ptr::from_ref(zalsa_local).expose_provenance()
}

/// The task whose token is `token`.
///
/// # Safety
///
/// The task holds a claim it can't release before the caller is done with the result: the
/// caller holds the claim's shard lock and turned its word from `token` to [`SLOW`], so the
/// task's release CAS fails and its release waits for the lock, and its `ZalsaLocal`, borrowed
/// by the task's `ClaimGuard`, stays alive and in place.
unsafe fn owner_of(token: usize) -> TaskId {
    let local = std::ptr::with_exposed_provenance::<ZalsaLocal>(token);
    // SAFETY: `local` is a live `ZalsaLocal` (our contract), and `task` reads only its `task`
    // field, written once when it was made and never again; the claim CAS that published
    // `token` (release) is read by the caller's CAS to `SLOW` (acquire), so that write happens
    // before this read.
    unsafe { (*local).task() }
}

#[inline]
fn hash_of(key: Id) -> u64 {
    FxBuildHasher.hash_one(key.index())
}

/// Whether `state` is the entry of `key`'s slot, of any generation.
#[inline]
fn is_slot_of(key: Id) -> impl Fn(&SyncState) -> bool {
    move |state| state.key.index() == key.index()
}

/// Under shuttle, a point where the scheduler may switch tasks: the claim word is a plain
/// atomic, which shuttle doesn't see.
#[inline(always)]
fn schedule() {
    #[cfg(feature = "shuttle")]
    thread::yield_now();
}

/// Claims a free word for `token`.
#[inline]
fn claim_word(word: &ClaimWord, token: usize) -> bool {
    schedule();
    // Acquire: the last owner's writes happen before our run. Release: publishes the task
    // behind `token` to a contender's `owner_of`.
    word.compare_exchange(FREE, token, Ordering::AcqRel, Ordering::Relaxed)
        .is_ok()
}

/// Releases `token`'s claim if nothing but the word knows of it.
#[inline]
fn release_word(word: &ClaimWord, token: usize) -> bool {
    schedule();
    word.compare_exchange(token, FREE, Ordering::Release, Ordering::Relaxed)
        .is_ok()
}

/// Holding the shard lock: turns `owner`'s claim of `key` into an entry, so its release takes
/// the lock. `false` if the word no longer holds `owner` (it was released, maybe claimed again).
fn make_slow(
    syncs: &mut HashTable<SyncState>,
    word: &ClaimWord,
    hash: u64,
    key: Id,
    owner: usize,
) -> bool {
    debug_assert!(owner != FREE && owner != SLOW);
    schedule();
    if word
        .compare_exchange(owner, SLOW, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        return false;
    }
    // SAFETY: We hold the shard lock and turned the word from `owner` to `SLOW`.
    let task = unsafe { owner_of(owner) };
    debug_assert!(
        syncs.find(hash, is_slot_of(key)).is_none(),
        "salsa: a claim word holding a token has no entry"
    );
    syncs.insert_unique(
        hash,
        SyncState {
            key,
            id: SyncOwner::Task(task),
            anyone_waiting: false,
            is_transfer_target: false,
            claimed_twice: false,
        },
        |state| hash_of(state.key),
    );
    true
}

/// Holding the shard lock, with the word `SLOW`: the key's entry.
fn slow_entry(syncs: &mut HashTable<SyncState>, hash: u64, key: Id) -> OccupiedEntry<'_, SyncState> {
    match syncs.find_entry(hash, is_slot_of(key)) {
        Ok(entry) => entry,
        Err(_) => panic!("salsa: the slow claim word of {key:?} has no entry"),
    }
}

/// Holding the shard lock: removes the entry of a claim whose word is `SLOW` and frees the word.
fn free_slow(
    syncs: &mut HashTable<SyncState>,
    word: &ClaimWord,
    hash: u64,
    key: Id,
    what: &'static str,
) -> SyncState {
    schedule();
    if word.load(Ordering::Relaxed) != SLOW {
        panic!("{what}");
    }
    let Ok(entry) = syncs.find_entry(hash, is_slot_of(key)) else {
        panic!("{what}");
    };
    let state = entry.remove().0;
    // Release: what the owner wrote happens before the next claim's run.
    word.store(FREE, Ordering::Release);
    state
}

/// Holding the shard lock: `token` claims a key whose transfer was released, which keeps nothing
/// of its entry, so the claim goes back to the word alone.
fn reclaim_released(entry: OccupiedEntry<'_, SyncState>, word: &ClaimWord, token: usize) {
    entry.remove();
    schedule();
    debug_assert_eq!(word.load(Ordering::Relaxed), SLOW);
    // Release: publishes the task behind `token` to a contender's `owner_of`.
    word.store(token, Ordering::Release);
}

#[derive(Copy, Clone, Debug)]
pub enum SyncOwner {
    /// Query is owned by this task
    Task(TaskId),

    /// The query's lock ownership has been transferred to another query.
    /// E.g. if `a` transfers its ownership to `b`, then only the thread in the critical path
    /// to complete `b` can claim `a` (in most instances, only the thread owning `b` can claim `a`).
    ///
    /// The thread owning `a` is stored in the `DependencyGraph`.
    ///
    /// A query can be marked as `Transferred` even if it has since then been released by the owning query.
    /// In that case, the query is effectively unclaimed and the `Transferred` state is stale. The reason
    /// for this is that it avoids the need for locking each sync table when releasing the transferred queries.
    Transferred,
}

/// Marks an active 'claim' in the synchronization map. The claim is
/// released when this value is dropped.
#[must_use]
pub(crate) struct ClaimGuard<'me> {
    key_index: Id,
    zalsa: &'me Zalsa,
    table: &'me SyncTable,
    word: &'me ClaimWord,
    mode: ReleaseMode,
    zalsa_local: &'me ZalsaLocal,
}

impl<'me> ClaimGuard<'me> {
    pub(crate) const fn zalsa(&self) -> &'me Zalsa {
        self.zalsa
    }

    pub(crate) fn zalsa_local(&self) -> &'me ZalsaLocal {
        self.zalsa_local
    }

    pub(crate) const fn database_key_index(&self) -> DatabaseKeyIndex {
        DatabaseKeyIndex::new(self.table.ingredient, self.key_index)
    }

    pub(crate) fn set_release_mode(&mut self, mode: ReleaseMode) {
        self.mode = mode;
    }

    #[inline]
    fn hash(&self) -> u64 {
        hash_of(self.key_index)
    }

    #[inline]
    fn syncs(&self) -> SyncGuard<'me> {
        self.table.shard_for(self.hash()).syncs.lock()
    }

    /// Releases the claim if nothing but its word knows of it: nobody waits, nothing to report.
    #[inline]
    fn release_fast(&self) -> bool {
        release_word(self.word, token_of(self.zalsa_local))
    }

    #[cold]
    #[inline(never)]
    fn release_panicking(&self) {
        let result = if self.zalsa_local.should_trigger_local_cancellation()
            || blaze_mem::frame::cancelling()
        {
            WaitResult::Cancelled
        } else {
            WaitResult::Panicked
        };
        tracing::debug!(
            "Release claim on {:?} due to {:?}",
            self.database_key_index(),
            result
        );
        if self.release_fast() {
            return;
        }
        let mut syncs = self.syncs();
        let state = free_slow(
            &mut syncs,
            self.word,
            self.hash(),
            self.key_index,
            "key claimed twice?",
        );
        self.release(state, result);
    }

    /// Releases a claim others know of, with its entry, and wakes them with `wait_result`.
    #[cold]
    #[inline(never)]
    fn release_slow(&self, wait_result: WaitResult) {
        let mut syncs = self.syncs();
        let state = free_slow(
            &mut syncs,
            self.word,
            self.hash(),
            self.key_index,
            "key should only be claimed/released once",
        );
        self.release(state, wait_result);
    }

    #[inline(always)]
    fn release(&self, state: SyncState, wait_result: WaitResult) {
        let SyncState {
            anyone_waiting,
            is_transfer_target,
            claimed_twice,
            ..
        } = state;

        if !anyone_waiting {
            return;
        }

        let runtime = self.zalsa.runtime();
        let database_key_index = self.database_key_index();

        if claimed_twice {
            runtime.undo_transfer_lock(database_key_index);
        }

        runtime.unblock_queries_blocked_on(database_key_index, wait_result);

        if is_transfer_target {
            runtime.unblock_transferred_queries_owned_by(database_key_index, wait_result);
        }
    }

    #[cold]
    #[inline(never)]
    fn release_self(&self) {
        if self.release_fast() {
            return;
        }
        let hash = self.hash();
        let mut syncs = self.syncs();
        if self.word.load(Ordering::Relaxed) != SLOW {
            panic!("key should only be claimed/released once");
        }
        let Ok(mut state) = syncs.find_entry(hash, is_slot_of(self.key_index)) else {
            panic!("key should only be claimed/released once");
        };

        if state.get().claimed_twice {
            state.get_mut().claimed_twice = false;
            state.get_mut().id = SyncOwner::Transferred;
        } else {
            drop(state);
            let state = free_slow(
                &mut syncs,
                self.word,
                hash,
                self.key_index,
                "key should only be claimed/released once",
            );
            self.release(state, WaitResult::Completed);
        }
    }

    #[cold]
    #[inline(never)]
    pub(crate) fn transfer(&self, new_owner: DatabaseKeyIndex) -> bool {
        let hash = self.hash();

        // Get the owning thread of `new_owner`.
        // The thread id is guaranteed to not be stale because `new_owner` must be blocked on `self_key`
        // or `transfer_lock` will panic (at least in debug builds).
        let Some(new_owner_thread_id) = self
            .zalsa
            .lookup_ingredient(new_owner.ingredient_index())
            .as_function()
            .expect("lock owners must be function ingredients")
            .sync_table()
            .mark_as_transfer_target(self.zalsa, new_owner.key_index())
        else {
            if !self.release_fast() {
                self.release_slow(WaitResult::Panicked);
            }

            panic!("new owner to be a locked query")
        };

        let mut syncs = self.syncs();

        let self_key = self.database_key_index();
        tracing::debug!(
            "Transferring lock ownership of {self_key:?} to {new_owner:?} ({new_owner_thread_id:?})"
        );

        schedule();
        let token = token_of(self.zalsa_local);
        if self.word.load(Ordering::Relaxed) != SLOW
            && !make_slow(&mut syncs, self.word, hash, self.key_index, token)
        {
            panic!("key should only be claimed/released once");
        }

        let SyncState {
            id, claimed_twice, ..
        } = slow_entry(&mut syncs, hash, self.key_index).into_mut();

        *id = SyncOwner::Transferred;
        *claimed_twice = false;

        self.zalsa
            .runtime()
            .transfer_lock(
                self_key,
                self.zalsa_local.task(),
                new_owner,
                new_owner_thread_id,
                syncs,
            )
    }

    /// Drops the claim on the memo.
    ///
    /// Returns `true` if the lock was transferred to another query and
    /// this thread blocked waiting for the new owner's lock to be released.
    /// In that case, any computed memo need to be refetched because they may have
    /// changed since `drop` was called.
    pub(crate) fn drop(mut self) -> bool {
        let refetch = self.drop_impl();
        std::mem::forget(self);
        refetch
    }

    #[inline]
    fn drop_impl(&mut self) -> bool {
        match self.mode {
            ReleaseMode::Default => {
                if !self.release_fast() {
                    self.release_slow(WaitResult::Completed);
                }
                false
            }
            ReleaseMode::SelfOnly => {
                self.release_self();
                false
            }
            ReleaseMode::TransferTo(new_owner) => self.transfer(new_owner),
        }
    }
}

impl Drop for ClaimGuard<'_> {
    fn drop(&mut self) {
        if thread::panicking() {
            self.release_panicking();
            return;
        }

        self.drop_impl();
    }
}

impl std::fmt::Debug for SyncTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncTable").finish()
    }
}

/// Controls how the lock is released when the `ClaimGuard` is dropped.
#[derive(Copy, Clone, Debug, Default)]
pub(crate) enum ReleaseMode {
    /// The default release mode.
    ///
    /// Releases the query for which this claim guard holds the lock and any queries that have
    /// transferred ownership to this query.
    #[default]
    Default,

    /// Only releases the lock for this query. Any query that has transferred ownership to this query
    /// will remain locked.
    ///
    /// If this thread panics, the query will be released as normal (default mode).
    SelfOnly,

    /// Transfers the ownership of the lock to the specified query.
    ///
    /// The query will remain locked and only the thread owning the transfer target will be resumed.
    ///
    /// The transfer target must be a query that's blocked on this query to guarantee that the transfer target doesn't complete
    /// before the transfer is finished (which would leave this query locked forever).
    ///
    /// If this thread panics, the query will be released as normal (default mode).
    TransferTo(DatabaseKeyIndex),
}

impl std::fmt::Debug for ClaimGuard<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaimGuard")
            .field("key_index", &self.key_index)
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

#[cfg(all(not(feature = "shuttle"), target_pointer_width = "64"))]
const _: [(); std::mem::size_of::<ClaimGuard<'static>>()] = [(); std::mem::size_of::<[usize; 7]>()];

#[cfg(all(not(feature = "shuttle"), target_pointer_width = "64"))]
const _: [(); std::mem::size_of::<CachePadded<SyncShard>>()] =
    [(); std::mem::size_of::<CachePadded<Mutex<HashTable<SyncState>>>>()];

/// Controls whether this thread can claim a query that transferred its ownership to a query
/// this thread currently holds the lock for.
///
/// For example: if query `a` transferred its ownership to query `b`, and this thread holds
/// the lock for `b`, then this thread can also claim `a` — but only when using [`Self::Allow`].
#[derive(Copy, Clone, PartialEq, Eq)]
pub(crate) enum Reentrancy {
    /// Allow `try_claim` to reclaim a query's that transferred its ownership to a query
    /// hold by this thread.
    Allow,

    /// Only allow claiming queries that haven't been claimed by any thread.
    Deny,
}

impl Reentrancy {
    const fn is_allow(self) -> bool {
        matches!(self, Reentrancy::Allow)
    }
}

/// One key's uncontended claim and release, as a query's execution takes them, for
/// `benches/claim.rs`.
#[doc(hidden)]
pub struct ClaimBench<'db> {
    zalsa: &'db Zalsa,
    zalsa_local: &'db ZalsaLocal,
    table: &'db SyncTable,
    key: Id,
}

impl<'db> ClaimBench<'db> {
    /// The claim of `key`, a tracked function's key, on the current task.
    // dyn: the edge: the claim benchmark's setup, once
    pub fn new(db: &'db dyn Database, key: DatabaseKeyIndex) -> Self {
        let (zalsa, zalsa_local) = db.zalsas();
        let table = zalsa
            .lookup_ingredient(key.ingredient_index())
            .as_function()
            .unwrap_or_else(|| panic!("salsa: {key:?} is not a tracked function's key"))
            .sync_table();
        Self {
            zalsa,
            zalsa_local,
            table,
            key: key.key_index(),
        }
    }

    /// Claims the key and releases it.
    ///
    /// # Panics
    ///
    /// If another claim holds the key.
    #[inline]
    pub fn claim_and_release(&self) {
        match self
            .table
            .try_claim(self.zalsa, self.zalsa_local, self.key, Reentrancy::Deny)
        {
            ClaimResult::Claimed(guard) => {
                guard.drop();
            }
            ClaimResult::Running(_) | ClaimResult::Cycle { .. } => {
                panic!("salsa: the benchmarked key is claimed already")
            }
        }
    }
}

#[cfg(all(test, not(feature = "shuttle")))]
mod tests {
    use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

    use blaze_ids::rng::SplitMix64;
    use hashbrown::HashTable;
    use rustc_hash::FxHashMap;

    use super::{
        FREE, SLOW, SyncOwner, SyncState, claim_word, free_slow, hash_of, is_slot_of, make_slow,
        reclaim_released, release_word, slow_entry, token_of,
    };
    use crate::Id;
    use crate::sync::Mutex;
    use crate::task::TaskId;
    use crate::zalsa_local::ZalsaLocal;

    fn local(depth: u32) -> Box<ZalsaLocal> {
        Box::new(ZalsaLocal::new(TaskId::current(depth)))
    }

    fn key(index: u32) -> Id {
        // SAFETY: Small indices are valid ids.
        unsafe { Id::from_index(index) }
    }

    const MISUSE: &str = "released twice";

    #[test]
    fn a_free_word_is_claimed_and_released_by_its_owner_alone() {
        let (a, b) = (local(0), local(1));
        let (a, b) = (token_of(&a), token_of(&b));
        assert!(a != b && a % 2 == 0 && b % 2 == 0, "tokens are distinct and even");
        let word = AtomicUsize::new(FREE);
        assert!(claim_word(&word, a));
        assert!(!claim_word(&word, b), "a claimed word can't be claimed");
        assert!(!claim_word(&word, a), "nor claimed again by its owner");
        assert!(!release_word(&word, b), "only the owner releases");
        assert!(release_word(&word, a));
        assert_eq!(word.load(Ordering::Relaxed), FREE);
        assert!(!release_word(&word, a), "a free word can't be released");
        assert!(claim_word(&word, b));
    }

    #[test]
    fn a_contended_claim_parks_its_state_in_the_shard_table() {
        let (owner, waiter) = (local(0), local(1));
        let (a, b) = (token_of(&owner), token_of(&waiter));
        let (word, mut syncs, k) = (AtomicUsize::new(FREE), HashTable::new(), key(7));
        let hash = hash_of(k);
        assert!(claim_word(&word, a));
        assert!(!claim_word(&word, b));

        assert!(make_slow(&mut syncs, &word, hash, k, a));
        assert_eq!(word.load(Ordering::Relaxed), SLOW);
        let entry = slow_entry(&mut syncs, hash, k);
        assert!(matches!(entry.get().id, SyncOwner::Task(task) if task == owner.task()));
        entry.into_mut().anyone_waiting = true;

        assert!(!claim_word(&word, b), "a slow word can't be claimed");
        assert!(!release_word(&word, a), "the owner's release must take the lock");
        let state = free_slow(&mut syncs, &word, hash, k, MISUSE);
        assert!(state.anyone_waiting, "the release sees the waiter, to wake it");
        assert_eq!(word.load(Ordering::Relaxed), FREE);
        assert!(syncs.is_empty());
        assert!(claim_word(&word, b), "the woken waiter claims the free word");
    }

    #[test]
    fn a_contender_that_loses_to_the_release_leaves_no_entry() {
        let owner = local(0);
        let a = token_of(&owner);
        let (word, mut syncs, k) = (AtomicUsize::new(FREE), HashTable::new(), key(3));
        assert!(claim_word(&word, a));
        assert!(release_word(&word, a));
        assert!(!make_slow(&mut syncs, &word, hash_of(k), k, a));
        assert_eq!(word.load(Ordering::Relaxed), FREE);
        assert!(syncs.is_empty());
    }

    #[test]
    #[should_panic(expected = "released twice")]
    fn freeing_a_word_that_isnt_slow_panics() {
        let owner = local(0);
        let (word, mut syncs, k) = (AtomicUsize::new(FREE), HashTable::new(), key(3));
        assert!(claim_word(&word, token_of(&owner)));
        free_slow(&mut syncs, &word, hash_of(k), k, MISUSE);
    }

    #[test]
    fn a_transferred_key_stays_slow_until_reclaimed() {
        let (owner, next) = (local(0), local(1));
        let (a, b) = (token_of(&owner), token_of(&next));
        let (word, mut syncs, k) = (AtomicUsize::new(FREE), HashTable::new(), key(9));
        let hash = hash_of(k);
        assert!(claim_word(&word, a));
        // `ClaimGuard::transfer`: the owner turns its own claim into an entry, `Transferred`.
        assert!(make_slow(&mut syncs, &word, hash, k, a));
        slow_entry(&mut syncs, hash, k).into_mut().id = SyncOwner::Transferred;
        // The owner's guard is gone; the word stays slow, so every claim goes to the entry.
        assert!(!claim_word(&word, b));
        assert!(!release_word(&word, a));
        // The transfer's target released it (`BlockTransferredResult::Released`): `b` claims.
        reclaim_released(slow_entry(&mut syncs, hash, k), &word, b);
        assert!(syncs.is_empty());
        assert_eq!(word.load(Ordering::Relaxed), b);
        assert!(release_word(&word, b), "the reclaimed key is released without the lock");
    }

    #[test]
    fn a_slots_generations_share_its_entry() {
        let owner = local(0);
        let (word, mut syncs) = (AtomicUsize::new(FREE), HashTable::new());
        let old = key(5);
        let new = old.next_generation().expect("a fresh id has generations left");
        assert_ne!(old, new);
        assert_eq!(hash_of(old), hash_of(new));
        assert!(claim_word(&word, token_of(&owner)));
        assert!(make_slow(&mut syncs, &word, hash_of(old), old, token_of(&owner)));
        assert!(syncs.find(hash_of(new), is_slot_of(new)).is_some());
        let state = free_slow(&mut syncs, &word, hash_of(new), new, MISUSE);
        assert_eq!(state.key, old);
    }

    /// The reference: the old shard-table claim, where a key is claimed while it has an entry.
    #[derive(Default)]
    struct Reference {
        owners: FxHashMap<u32, usize>,
    }

    impl Reference {
        fn claim(&mut self, row: u32, token: usize) -> bool {
            if self.owners.contains_key(&row) {
                return false;
            }
            self.owners.insert(row, token);
            true
        }

        fn release(&mut self, row: u32, token: usize) -> bool {
            if self.owners.get(&row) != Some(&token) {
                return false;
            }
            self.owners.remove(&row);
            true
        }
    }

    /// Random claims, contentions and releases on a few rows by a few tasks, one at a time: the
    /// words and entries agree with the reference on who holds every row after every step.
    #[test]
    fn claims_match_the_shard_table_reference() {
        let locals: Vec<_> = (0..4).map(local).collect();
        let tokens: Vec<usize> = locals.iter().map(|local| token_of(local)).collect();
        let words: Vec<AtomicUsize> = (0..8).map(|_| AtomicUsize::new(FREE)).collect();
        let mut syncs = HashTable::new();
        let mut reference = Reference::default();
        let mut rng = SplitMix64::new(0x5eed);
        for _ in 0..20_000 {
            let row = (rng.next_u64() % words.len() as u64) as u32;
            let task = (rng.next_u64() % tokens.len() as u64) as usize;
            let (token, word, k) = (tokens[task], &words[row as usize], key(row));
            let hash = hash_of(k);
            match rng.next_u64() % 3 {
                0 => assert_eq!(claim_word(word, token), reference.claim(row, token)),
                1 => {
                    // A contender: parks the claim in the table if another task holds it.
                    let current = word.load(Ordering::Relaxed);
                    if current != FREE && current != SLOW && current != token {
                        assert!(make_slow(&mut syncs, word, hash, k, current));
                        let entry = slow_entry(&mut syncs, hash, k);
                        assert!(matches!(
                            entry.get().id,
                            SyncOwner::Task(owner) if owner == locals[tokens.iter().position(|&t| t == current).unwrap()].task()
                        ));
                        entry.into_mut().anyone_waiting = true;
                    }
                }
                _ => {
                    let expected = reference.release(row, token);
                    let released = release_word(word, token)
                        || (expected && {
                            let state = free_slow(&mut syncs, word, hash, k, MISUSE);
                            assert!(state.anyone_waiting);
                            true
                        });
                    assert_eq!(released, expected);
                }
            }
            for (row, word) in words.iter().enumerate() {
                let held = match word.load(Ordering::Relaxed) {
                    FREE => None,
                    SLOW => {
                        let k = key(row as u32);
                        let state = syncs
                            .find(hash_of(k), is_slot_of(k))
                            .expect("a slow word has its entry");
                        let SyncOwner::Task(task) = state.id else {
                            panic!("nothing here transfers")
                        };
                        Some(tokens[locals.iter().position(|l| l.task() == task).unwrap()])
                    }
                    token => {
                        let k = key(row as u32);
                        assert!(syncs.find(hash_of(k), is_slot_of(k)).is_none());
                        Some(token)
                    }
                };
                assert_eq!(held, reference.owners.get(&(row as u32)).copied());
            }
        }
    }

    /// Tasks race to claim rows, few (much contention) or many: a row is never held by two at
    /// once, every contended claim is parked in the table and seen by its release, and at the
    /// end every word is free with no entry left.
    fn race(rows: usize) {
        const TASKS: usize = 8;
        const CLAIMS: usize = 20_000;
        let words: Vec<AtomicUsize> = (0..rows).map(|_| AtomicUsize::new(FREE)).collect();
        let holders: Vec<AtomicU32> = (0..rows).map(|_| AtomicU32::new(0)).collect();
        let syncs = Mutex::new(HashTable::<SyncState>::new());
        let (parked, woken) = (AtomicUsize::new(0), AtomicUsize::new(0));
        std::thread::scope(|scope| {
            for task in 0..TASKS {
                let (words, holders, syncs) = (&words, &holders, &syncs);
                let (parked, woken) = (&parked, &woken);
                scope.spawn(move || {
                    let local = local(0);
                    let token = token_of(&local);
                    let mut rng = SplitMix64::new(task as u64 + 1);
                    let mut claimed = 0;
                    while claimed < CLAIMS {
                        let row = (rng.next_u64() % rows as u64) as usize;
                        let (word, k) = (&words[row], key(row as u32));
                        let hash = hash_of(k);
                        if !claim_word(word, token) {
                            // Contended: park in the table, as a waiter, then wait for a change.
                            let mut table = syncs.lock();
                            match word.load(Ordering::Acquire) {
                                FREE => continue,
                                SLOW => {}
                                owner => {
                                    if !make_slow(&mut table, word, hash, k, owner) {
                                        continue;
                                    }
                                    parked.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                            slow_entry(&mut table, hash, k).into_mut().anyone_waiting = true;
                            drop(table);
                            while word.load(Ordering::Acquire) == SLOW {
                                std::hint::spin_loop();
                            }
                            continue;
                        }
                        assert_eq!(holders[row].fetch_add(1, Ordering::Relaxed), 0);
                        std::hint::spin_loop();
                        assert_eq!(holders[row].fetch_sub(1, Ordering::Relaxed), 1);
                        if !release_word(word, token) {
                            let state = free_slow(&mut syncs.lock(), word, hash, k, MISUSE);
                            assert!(state.anyone_waiting);
                            assert!(matches!(state.id, SyncOwner::Task(t) if t == local.task()));
                            woken.fetch_add(1, Ordering::Relaxed);
                        }
                        claimed += 1;
                    }
                });
            }
        });
        assert!(words.iter().all(|word| word.load(Ordering::Relaxed) == FREE));
        assert!(syncs.lock().is_empty());
        assert_eq!(parked.load(Ordering::Relaxed), woken.load(Ordering::Relaxed));
    }

    #[test]
    fn racing_claims_on_the_same_rows() {
        race(2);
    }

    #[test]
    fn racing_claims_on_different_rows() {
        race(4096);
    }
}

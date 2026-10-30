//! The trait solver's caches of solved goals, in the two scopes of
//! [`CacheScope`](rustc_type_ir::search_graph::CacheScope):
//!
//! - [`shared`]: a goal whose reads `salsa::capture_reads` recorded, for every query and
//!   thread of one database, crate and revision. A query that takes an answer from it replays
//!   the reads (`Tracked`), so it reruns when they change, as rustc's `with_cached_task` /
//!   `get_tracked` do. Reads are lock-free: a few loads under the task's `blaze_reclaim`
//!   guard. A cache is replaced by the first use in a later revision, and freed once no task
//!   can still hold it.
//! - [`of_query`]: a goal no other query may take on (its capture is `None`), one cache for
//!   each interner, in a table every thread shares, so a query that moves to another thread
//!   keeps what it solved.

use crate::db::HirDatabase;

use super::DbInterner;
use base_db::{Crate, Nonce};
use blaze_reclaim::Limbo;
use rustc_hash::FxBuildHasher;
use rustc_type_ir::search_graph::GlobalCache;
use salsa::Revision;
use std::{
    hash::{BuildHasher, Hash},
    mem, ptr,
    sync::{
        LazyLock, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering},
    },
};

/// The search graph's cache, with the lifetime of the interner it is used through erased: it
/// holds interned values (types, predicates, canonical inputs and responses), handles into
/// process-wide arenas that are never freed and that every thread reads, so it is the same
/// for any `'db`.
type Erased = GlobalCache<DbInterner<'static>>;

// Shared by every thread with no lock in `shared`, and moved between them under a slot's lock
// in `of_query`: both by the auto traits, which this checks.
const _: fn() = || {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<Erased>();
};

/// `cache`, as the cache of an interner of `'db`.
#[inline]
fn unerase<'a, 'db>(cache: &'a Erased) -> &'a GlobalCache<DbInterner<'db>> {
    // SAFETY: `'db` only borrows the database; the interned values the cache holds do not
    // depend on it (see `Erased`). A cache is handed only to interners of the database and
    // revision it was filled in (`of_query` checks the `State`, `shared` the `Generation`),
    // so whatever it gives back was made by that database as it still is.
    unsafe { mem::transmute::<&'a Erased, &'a GlobalCache<DbInterner<'db>>>(cache) }
}

/// Which goals a shared cache holds: those solved in one database, for one crate (whose lang
/// items and recursion limit every interner of it read when it was made), with or without
/// region constraints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Scope {
    nonce: Nonce,
    krate: Option<Crate>,
    regions: bool,
}

/// The value of one key (for the solver, the goals of one scope) in one state (revision).
/// Never changed once published but through the value's own `&self` methods; freed only
/// through `Shared::limbo`.
struct Generation<K, S, T> {
    key: K,
    state: S,
    /// When it was made: of a full window, the oldest is given away.
    born: u64,
    value: T,
}

/// How many scopes are kept, `1 << SHARED_BITS`: two (with and without regions) for each crate
/// being checked, in every database alive. A scope given away is only solved again.
const SHARED_BITS: u32 = 14;

/// An open-addressed table from a key to its value in the present state, shared by every
/// thread, of a fixed size. A lookup is up to `PROBE` loads from `hints` (two cache lines),
/// then one pointer load and the generation's key and state; nothing is written. A generation
/// is replaced by a compare-and-swap of its pointer, once per key and state. Two replaces of
/// one slot racing may leave it a stale hint: lookups then miss it and the key is made again
/// in another slot, until the slot is next replaced. That costs a solve, never an answer.
struct Shared<K, S, T> {
    /// A hash of the key each slot holds, 0 for none, so a probe loads one word; the
    /// generation says which key it is.
    hints: Box<[AtomicU64]>,
    generations: Box<[AtomicPtr<Generation<K, S, T>>]>,
    /// Generations replaced, until no task can still be reading them. Locked only to replace.
    limbo: Mutex<Limbo>,
    /// Counts generations made, for `Generation::born`. Written only to replace.
    born: AtomicU64,
}

static SHARED: LazyLock<Shared<Scope, Revision, Erased>> =
    LazyLock::new(|| Shared::new(SHARED_BITS));

/// Runs `f` on the shared cache of `krate` (and `regions`) in `db`, as it is now.
#[inline]
pub(super) fn shared<'db, R>(
    db: &'db dyn HirDatabase,
    krate: Option<Crate>,
    regions: bool,
    f: impl FnOnce(&GlobalCache<DbInterner<'db>>) -> R,
) -> R {
    let (nonce, revision) = db.nonce_and_revision();
    // Nested in the task the executor runs this in, so it writes nothing but a thread-local
    // count.
    let _task = blaze_reclaim::enter();
    f(unerase(SHARED.of(Scope { nonce, krate, regions }, revision)))
}

impl<K, S, T> Shared<K, S, T>
where
    K: Copy + Eq + Hash + Send + Sync + 'static,
    S: Copy + Eq + Send + Sync + 'static,
    T: Default + Send + Sync + 'static,
{
    fn new(bits: u32) -> Shared<K, S, T> {
        let len = 1usize << bits;
        Shared {
            hints: (0..len).map(|_| AtomicU64::new(EMPTY)).collect(),
            generations: (0..len).map(|_| AtomicPtr::new(ptr::null_mut())).collect(),
            limbo: Mutex::new(Limbo::new()),
            born: AtomicU64::new(0),
        }
    }

    /// The value of `key` in `state`, a new one if there is none. Good until the caller's
    /// `blaze_reclaim` guard is dropped: the caller must be in a task.
    fn of(&self, key: K, state: S) -> &T {
        debug_assert!(blaze_reclaim::in_task(), "read outside a task");
        let hint = FxBuildHasher.hash_one(key) | 1;
        'search: loop {
            for at in window(hint, self.generations.len()) {
                if self.hints[at].load(Ordering::Acquire) != hint {
                    continue;
                }
                let held = self.generations[at].load(Ordering::Acquire);
                let Some(generation) = self.get(held) else { continue };
                if generation.key != key {
                    continue;
                }
                if generation.state == state {
                    return &generation.value;
                }
                // The first use in a new state: what the old value holds may be untrue now
                // (for the solver: its goals read what may have changed since).
                match self.replace(at, held, hint, key, state) {
                    Some(made) => return made,
                    None => continue 'search,
                }
            }
            let (at, held) = self.victim(hint);
            if let Some(made) = self.replace(at, held, hint, key, state) {
                return made;
            }
            // Another thread changed the slot first: look again.
        }
    }

    /// The generation `held` points to, if any. Good until the caller's guard is dropped.
    #[inline]
    fn get(&self, held: *mut Generation<K, S, T>) -> Option<&Generation<K, S, T>> {
        // SAFETY: a generation is freed only by the limbo, after a grace started once it was
        // unlinked from its slot; the caller's guard was entered before `held` was loaded, so
        // that grace has not passed.
        unsafe { held.as_ref() }
    }

    /// The slot a key hashing to `hint` is given, and what it holds: an empty one, else the
    /// one holding the oldest generation.
    #[cold]
    fn victim(&self, hint: u64) -> (usize, *mut Generation<K, S, T>) {
        let mut oldest: Option<(usize, *mut Generation<K, S, T>, u64)> = None;
        for at in window(hint, self.generations.len()) {
            let held = self.generations[at].load(Ordering::Acquire);
            let Some(generation) = self.get(held) else { return (at, held) };
            if oldest.is_none_or(|(_, _, born)| generation.born < born) {
                oldest = Some((at, held, generation.born));
            }
        }
        let (at, held, _) = oldest.expect("a window has a slot");
        (at, held)
    }

    /// Puts a new generation of `key` (hashing to `hint`) in slot `at` if it still holds
    /// `held`, retiring `held`.
    #[cold]
    fn replace(
        &self,
        at: usize,
        held: *mut Generation<K, S, T>,
        hint: u64,
        key: K,
        state: S,
    ) -> Option<&T> {
        let born = self.born.fetch_add(1, Ordering::Relaxed);
        let made =
            Box::into_raw(Box::new(Generation { key, state, born, value: T::default() }));
        if self.generations[at]
            .compare_exchange(held, made, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            // SAFETY: `made` was never published, so this is its only owner.
            drop(unsafe { Box::from_raw(made) });
            return None;
        }
        self.hints[at].store(hint, Ordering::Release);
        if !held.is_null() {
            // SAFETY: every generation comes from `Box::into_raw` here, and the swap above
            // unlinked `held`: one swap alone can, so this is the one place that takes it
            // back, and the limbo frees it only after every task that may have loaded it has
            // ended.
            self.retire(unsafe { Box::from_raw(held) });
        }
        // SAFETY: published above; freed only by the limbo, after the caller's guard is
        // dropped (see `of`).
        Some(unsafe { &(*made).value })
    }

    fn retire(&self, generation: Box<Generation<K, S, T>>) {
        let mut limbo = self.limbo.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        limbo.retire(generation);
        limbo.collect();
    }

    /// Unlinks every generation.
    fn clear(&self) {
        for (hint, slot) in self.hints.iter().zip(&self.generations) {
            hint.store(EMPTY, Ordering::Release);
            let held = slot.swap(ptr::null_mut(), Ordering::AcqRel);
            if !held.is_null() {
                // SAFETY: as in `replace`: the swap unlinked it, and only one swap can.
                self.retire(unsafe { Box::from_raw(held) });
            }
        }
    }
}

/// The slots a key hashing to `hash` may be in, out of `len`, the one it hashes to first.
fn window(hash: u64, len: usize) -> impl Iterator<Item = usize> {
    let mask = len - 1;
    let start = (hash.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - len.trailing_zeros())) as usize;
    (0..PROBE.min(len)).map(move |step| (start + step) & mask)
}

/// Names the query-scoped cache of one interner. A goal whose reads were not captured is
/// only good for where it was solved: what was read to solve it was noted for the query that
/// asked alone, and another query handed the answer would not be run again when an impl
/// changes. An interner does not leave the query that made it, so those goals are kept for it
/// alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Solved(u64);

impl Solved {
    pub(super) fn new() -> Solved {
        // One count for the process: the table is shared by every thread. 0 is an empty slot.
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Solved(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// The database a cache was filled in, and its revision: a slot found in another state is
/// emptied before it is used.
type State = (Nonce, Revision);

/// The interners whose goals are kept, `1 << SLOT_BITS` of them: the queries under way on
/// every thread, as a query that solves makes one and asks other queries that make theirs.
const SLOT_BITS: u32 = 9;

/// How many slots from the one a key hashes to it may be kept in.
const PROBE: usize = 16;

static CACHES: LazyLock<Table<State, Erased>> = LazyLock::new(|| Table::new(SLOT_BITS));

/// Runs `f` on the cache of the interner `solved` alone, as `db` is now.
#[inline]
pub(super) fn of_query<'db, R>(
    db: &'db dyn HirDatabase,
    solved: Solved,
    f: impl FnOnce(&GlobalCache<DbInterner<'db>>) -> R,
) -> R {
    let state = db.nonce_and_revision();
    CACHES.with(solved.0, state, |cache| f(unerase(cache)))
}

/// Clears the trait solver's caches, of every query and thread.
///
/// Should be called before getting memory usage estimations, as the solver cache
/// is per-revision and usually should be excluded from estimations.
pub fn clear_tls_solver_cache() {
    CACHES.clear();
    SHARED.clear();
}

/// An open-addressed table from a key to a value of type `T`, filled in a state `S`, shared
/// by every thread, of a fixed size. A key is found by loads alone; a slot is claimed for a
/// key by a compare-and-swap of its key, with no lock on the table. When the slots a key may
/// go to are full, the first one not used since a claim last passed over it is given to it
/// (second chance). A lookup is up to `PROBE` loads from `keys` (two cache lines) and the
/// slot's lock.
struct Table<S, T> {
    /// The key in each slot, 0 for none; written only to claim or clear the slot.
    keys: Box<[AtomicU64]>,
    slots: Box<[Slot<S, T>]>,
    bits: u32,
}

/// One key's value, on cache lines of its own. Only the task solving through that key takes
/// its lock, but for a claim emptying it for another key, so it is not contended; and it is
/// held only around `Table::with`'s `f`, the search graph's lookup or insert, which calls
/// nothing in the database.
#[repr(align(128))]
struct Slot<S, T> {
    filled: Mutex<Filled<S, T>>,
    /// Set by a use, cleared by a claim passing over the slot.
    referenced: AtomicBool,
}

struct Filled<S, T> {
    /// The key `value` is of: a slot given to another key is emptied by its first use.
    key: u64,
    /// `None` when the slot is empty or what is in it may be half written.
    state: Option<S>,
    value: T,
}

// A slot, the search graph's cache included, takes at most four 128-byte pairs of cache lines.
const _: () = assert!(
    align_of::<Slot<State, Erased>>() == 128 && size_of::<Slot<State, Erased>>() <= 512
);
// A generation's header is one line; the rest is the search graph's cache.
const _: () =
    assert!(size_of::<Generation<Scope, Revision, Erased>>() - size_of::<Erased>() <= 64);

const EMPTY: u64 = 0;

impl<S: Copy + Eq, T: Default> Table<S, T> {
    fn new(bits: u32) -> Table<S, T> {
        let len = 1usize << bits;
        Table {
            keys: (0..len).map(|_| AtomicU64::new(EMPTY)).collect(),
            slots: (0..len)
                .map(|_| Slot {
                    filled: Mutex::new(Filled { key: EMPTY, state: None, value: T::default() }),
                    referenced: AtomicBool::new(false),
                })
                .collect(),
            bits,
        }
    }

    /// Runs `f` on the value of `key`, emptied first if it was filled in a state other than
    /// `state`, or a new one.
    fn with<R>(&self, key: u64, state: S, f: impl FnOnce(&mut T) -> R) -> R {
        debug_assert_ne!(key, EMPTY, "0 names no key");
        let (at, mut filled) = match self.find(key) {
            Some(found) => found,
            None => self.claim(key),
        };
        if filled.key != key || filled.state != Some(state) {
            filled.key = key;
            filled.state = Some(state);
            filled.value = T::default();
        }
        let referenced = &self.slots[at].referenced;
        if !referenced.load(Ordering::Relaxed) {
            referenced.store(true, Ordering::Relaxed);
        }
        f(&mut filled.value)
    }

    /// Empties every slot.
    fn clear(&self) {
        for (at, slot) in self.slots.iter().enumerate() {
            let old = {
                let mut filled = lock(&slot.filled);
                self.keys[at].store(EMPTY, Ordering::Release);
                filled.key = EMPTY;
                filled.state = None;
                mem::take(&mut filled.value)
            };
            drop(old);
        }
    }

    /// The slots `key` may be in, the one it hashes to first.
    fn window(&self, key: u64) -> impl Iterator<Item = usize> {
        window(key, 1 << self.bits)
    }

    /// The slot holding `key`, locked.
    fn find(&self, key: u64) -> Option<(usize, MutexGuard<'_, Filled<S, T>>)> {
        self.window(key).find_map(|at| self.locked(at, key))
    }

    /// Slot `at`, locked, if it holds `key`.
    fn locked(&self, at: usize, key: u64) -> Option<(usize, MutexGuard<'_, Filled<S, T>>)> {
        if self.keys[at].load(Ordering::Acquire) != key {
            return None;
        }
        let filled = lock(&self.slots[at].filled);
        // It may have been given to another key while the lock was awaited.
        (self.keys[at].load(Ordering::Acquire) == key).then_some((at, filled))
    }

    /// Gives `key` a slot: an empty one, else the first not used since a claim last passed.
    /// Two threads claiming one key at once take the same empty slot; a key that ends in two
    /// slots after a race for a full window is only kept twice.
    fn claim(&self, key: u64) -> (usize, MutexGuard<'_, Filled<S, T>>) {
        loop {
            let mut empty = None;
            for at in self.window(key) {
                match self.keys[at].load(Ordering::Acquire) {
                    found if found == key => {
                        if let Some(claimed) = self.locked(at, key) {
                            return claimed;
                        }
                    }
                    EMPTY => {
                        empty = Some(at);
                        break;
                    }
                    _ => {}
                }
            }
            let at = empty.unwrap_or_else(|| self.second_chance(key));
            let old = self.keys[at].load(Ordering::Acquire);
            if (old == EMPTY || empty.is_none())
                && old != key
                && self.keys[at]
                    .compare_exchange(old, key, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            {
                let mut filled = lock(&self.slots[at].filled);
                // Until its first use the slot still holds the value of the key it was taken
                // from; that is dropped here, not while a lookup of `key` waits on it.
                let old = (filled.key != key).then(|| {
                    filled.key = key;
                    filled.state = None;
                    mem::take(&mut filled.value)
                });
                drop(old);
                return (at, filled);
            }
            // Another claim took the slot first: look again.
        }
    }

    /// The first slot of `key`'s window not used since a claim last passed over it, clearing
    /// the mark of each one passed over.
    fn second_chance(&self, key: u64) -> usize {
        for _ in 0..2 {
            for at in self.window(key) {
                let referenced = &self.slots[at].referenced;
                if !referenced.load(Ordering::Relaxed) {
                    return at;
                }
                referenced.store(false, Ordering::Relaxed);
            }
        }
        self.window(key).next().expect("a window has a slot")
    }
}

/// A slot whose user panicked holding it may be half written: it is emptied.
fn lock<S, T: Default>(slot: &Mutex<Filled<S, T>>) -> MutexGuard<'_, Filled<S, T>> {
    slot.lock().unwrap_or_else(|poisoned| {
        slot.clear_poison();
        let mut filled = poisoned.into_inner();
        filled.key = EMPTY;
        filled.state = None;
        filled.value = T::default();
        filled
    })
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{
            Barrier, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
    };

    use super::{EMPTY, Shared, Solved, Table};

    type Goals = HashMap<u32, u32>;

    fn key() -> u64 {
        Solved::new().0
    }

    fn holding(table: &Table<u32, Goals>, key: u64) -> usize {
        table.keys.iter().filter(|it| it.load(Ordering::Relaxed) == key).count()
    }

    #[test]
    fn a_goal_solved_in_one_query_is_not_reused_by_another() {
        let table = Table::<u32, Goals>::new(4);
        let (one, other) = (key(), key());
        table.with(one, 0, |goals| goals.insert(7, 1));
        assert_eq!(table.with(other, 0, |goals| goals.get(&7).copied()), None);
        assert_eq!(table.with(one, 0, |goals| goals.get(&7).copied()), Some(1));
    }

    #[test]
    fn a_cache_filled_on_one_thread_serves_the_same_query_on_another() {
        let table = Table::<u32, Goals>::new(4);
        let query = key();
        thread::scope(|scope| {
            scope.spawn(|| table.with(query, 0, |goals| goals.insert(7, 1))).join().unwrap();
            let found = scope.spawn(|| table.with(query, 0, |goals| goals.get(&7).copied()));
            assert_eq!(found.join().unwrap(), Some(1));
        });
    }

    #[test]
    fn a_cache_is_not_used_in_another_state() {
        let table = Table::<u32, Goals>::new(4);
        let query = key();
        table.with(query, 0, |goals| goals.insert(7, 1));
        assert_eq!(table.with(query, 1, |goals| goals.get(&7).copied()), None);
        assert_eq!(table.with(query, 0, |goals| goals.get(&7).copied()), None);
    }

    #[test]
    fn clear_empties_every_slot() {
        let table = Table::<u32, Goals>::new(4);
        let queries: Vec<u64> = (0..8).map(|_| key()).collect();
        for &query in &queries {
            table.with(query, 0, |goals| goals.insert(7, 1));
        }
        table.clear();
        assert!(table.keys.iter().all(|it| it.load(Ordering::Relaxed) == EMPTY));
        for &query in &queries {
            assert_eq!(table.with(query, 0, |goals| goals.get(&7).copied()), None);
        }
    }

    #[test]
    fn a_key_claimed_by_many_threads_at_once_is_in_one_slot() {
        let table = Table::<u32, Goals>::new(4);
        let query = key();
        let start = Barrier::new(8);
        thread::scope(|scope| {
            for worker in 0..8u32 {
                let (table, start) = (&table, &start);
                scope.spawn(move || {
                    start.wait();
                    table.with(query, 0, |goals| goals.insert(worker, worker));
                });
            }
        });
        assert_eq!(holding(&table, query), 1);
        assert_eq!(table.with(query, 0, |goals| goals.len()), 8);
    }

    #[test]
    fn a_full_window_gives_away_a_slot_not_used_since_the_last_claim() {
        // Every slot is in every window of a 16-slot table.
        let table = Table::<u32, Goals>::new(4);
        let queries: Vec<u64> = (0..16).map(|_| key()).collect();
        for &query in &queries {
            table.with(query, 0, |goals| goals.insert(7, 1));
        }
        let (sixteenth, seventeenth) = (key(), key());
        table.with(sixteenth, 0, |goals| goals.insert(7, 1));
        let kept: Vec<u64> =
            queries.iter().copied().filter(|&it| holding(&table, it) == 1).collect();
        assert_eq!(kept.len(), 15);
        // That claim cleared every mark; all but one of the queries left are used again.
        let idle = kept[0];
        for &query in &kept[1..] {
            assert_eq!(table.with(query, 0, |goals| goals.get(&7).copied()), Some(1));
        }
        table.with(seventeenth, 0, |goals| goals.insert(7, 1));
        assert_eq!(holding(&table, idle), 0);
        for &query in kept[1..].iter().chain([&sixteenth, &seventeenth]) {
            assert_eq!(holding(&table, query), 1);
        }
    }

    /// The cache before this one: per thread, the last `KEPT` queries' goals, all dropped when
    /// the state changes.
    struct PerThread {
        caches: Vec<(u64, Goals)>,
        state: u32,
    }

    impl PerThread {
        const KEPT: usize = 32;

        fn of(&mut self, query: u64, state: u32) -> &mut Goals {
            if self.state != state {
                self.caches.clear();
                self.state = state;
            }
            match self.caches.iter().rposition(|it| it.0 == query) {
                Some(at) => {
                    let found = self.caches.remove(at);
                    self.caches.push(found);
                }
                None => {
                    if self.caches.len() == Self::KEPT {
                        self.caches.remove(0);
                    }
                    self.caches.push((query, Goals::new()));
                }
            }
            &mut self.caches.last_mut().unwrap().1
        }
    }

    /// What solving `goal` for `query` in `state` gives: different for each, so an answer
    /// handed to another query or state is seen.
    fn solve(query: u64, state: u32, goal: u32) -> u32 {
        (query as u32).wrapping_mul(0x9E37_79B9) ^ state.wrapping_mul(0x85EB_CA6B) ^ goal
    }

    /// Answers a goal through `goals`, solving it when it is not there.
    fn answer(goals: &mut Goals, query: u64, state: u32, goal: u32) -> u32 {
        *goals.entry(goal).or_insert_with(|| solve(query, state, goal))
    }

    #[test]
    fn answers_equal_those_of_a_cache_per_thread() {
        // A small table, so slots are taken from queries under way.
        let table = Table::<u32, Goals>::new(5);
        let queries: Vec<u64> = (0..96).map(|_| key()).collect();
        let shared_hits = AtomicUsize::new(0);
        thread::scope(|scope| {
            for worker in 0..8u64 {
                let (table, queries, shared_hits) = (&table, &queries, &shared_hits);
                scope.spawn(move || {
                    let mut per_thread = PerThread { caches: Vec::new(), state: 0 };
                    let mut seed = 0x2545_F491_4F6C_DD1Du64 ^ (worker + 1);
                    let mut next = move || {
                        seed ^= seed << 13;
                        seed ^= seed >> 7;
                        seed ^= seed << 17;
                        seed
                    };
                    for step in 0..20_000u32 {
                        // Each query is taken up by every thread, as a task moves; the state
                        // changes twice, as revisions do.
                        let query = queries[next() as usize % queries.len()];
                        let state = step / 8_000;
                        let goal = (next() % 64) as u32;
                        let truth = solve(query, state, goal);
                        let shared = table.with(query, state, |goals| {
                            if goals.contains_key(&goal) {
                                shared_hits.fetch_add(1, Ordering::Relaxed);
                            }
                            answer(goals, query, state, goal)
                        });
                        let local = answer(per_thread.of(query, state), query, state, goal);
                        assert_eq!(shared, truth, "{query} {state} {goal}");
                        assert_eq!(shared, local, "{query} {state} {goal}");
                    }
                });
            }
        });
        assert!(shared_hits.into_inner() > 0, "the shared table never answered from its cache");
    }
}

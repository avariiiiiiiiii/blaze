//! The trait solver's cache of solved goals: one for each interner, in a table every thread
//! shares, so a query that moves to another thread keeps what it solved.

use crate::db::HirDatabase;

use super::DbInterner;
use base_db::Nonce;
use rustc_type_ir::search_graph::GlobalCache;
use salsa::Revision;
use std::{
    mem,
    sync::{
        LazyLock, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
};

/// Names the cache of one interner. A solved goal is only good for where it was solved:
/// which impls there are to pick from depends on the crate, and what was read to solve it
/// was noted for the query that asked. Another query handed the answer would not be run
/// again when an impl changes. An interner does not leave the query that made it, so the
/// goals solved through it are kept for it alone.
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

/// The search graph's cache of one interner.
#[derive(Default)]
struct Kept(GlobalCache<DbInterner<'static>>);

// SAFETY: what the cache holds are interned values (types, predicates, canonical inputs and
// responses), handles into process-wide arenas that are never freed and that every thread
// reads (`DbInterner` is `Send` and `Sync`), in std collections. A slot is reached only under
// its lock.
unsafe impl Send for Kept {}

static CACHES: LazyLock<Table<State, Kept>> = LazyLock::new(|| Table::new(SLOT_BITS));

/// Runs `f` on the cache of `solved`, as `db` is now.
#[inline]
pub(super) fn borrow_assume_valid<'db, T>(
    db: &'db dyn HirDatabase,
    solved: Solved,
    f: impl FnOnce(&mut GlobalCache<DbInterner<'db>>) -> T,
) -> T {
    let state = db.nonce_and_revision();
    CACHES.with(solved.0, state, |kept| {
        // SAFETY: `'db` only borrows the database; the interned values the cache holds do
        // not depend on it (see `Kept`). The cache is handed only to the interner of
        // `solved`, in the state of the database it was filled in, so whatever it gives back
        // was made by that database as it still is.
        f(unsafe {
            mem::transmute::<
                &mut GlobalCache<DbInterner<'static>>,
                &mut GlobalCache<DbInterner<'db>>,
            >(&mut kept.0)
        })
    })
}

/// Clears the trait solver's cache, of every thread.
///
/// Should be called before getting memory usage estimations, as the solver cache
/// is per-revision and usually should be excluded from estimations.
pub fn clear_tls_solver_cache() {
    CACHES.clear();
}

/// An open-addressed table from a key to a value of type `T`, filled in a state `S`, shared
/// by every thread. Each slot has a lock of its own; finding a key reads only atomics, and
/// adding one takes the table's `claim` lock, so a key is in one slot at most. When the
/// slots a key may go to are full, the one used least lately is emptied for it.
struct Table<S, T> {
    /// The key in each slot, 0 for none. Written only under the slot's lock.
    keys: Box<[AtomicU64]>,
    /// The `clock` when each slot was last used.
    used: Box<[AtomicU64]>,
    slots: Box<[Mutex<Slot<S, T>>]>,
    /// Counts the keys added.
    clock: AtomicU64,
    claim: Mutex<()>,
    bits: u32,
}

struct Slot<S, T> {
    /// `None` when the slot is empty or what is in it may be half written.
    state: Option<S>,
    value: T,
}

const EMPTY: u64 = 0;

impl<S: Copy + Eq, T: Default> Table<S, T> {
    fn new(bits: u32) -> Table<S, T> {
        let len = 1usize << bits;
        Table {
            keys: (0..len).map(|_| AtomicU64::new(EMPTY)).collect(),
            used: (0..len).map(|_| AtomicU64::new(0)).collect(),
            slots: (0..len).map(|_| Mutex::new(Slot { state: None, value: T::default() })).collect(),
            clock: AtomicU64::new(0),
            claim: Mutex::new(()),
            bits,
        }
    }

    /// Runs `f` on the value of `key`, emptied first if it was filled in a state other than
    /// `state`, or a new one.
    fn with<R>(&self, key: u64, state: S, f: impl FnOnce(&mut T) -> R) -> R {
        assert_ne!(key, EMPTY, "0 names no key");
        let (at, mut slot) = match self.find(key) {
            Some(found) => found,
            None => self.add(key),
        };
        if slot.state != Some(state) {
            slot.value = T::default();
            slot.state = Some(state);
        }
        let now = self.clock.load(Ordering::Relaxed);
        if self.used[at].load(Ordering::Relaxed) != now {
            self.used[at].store(now, Ordering::Relaxed);
        }
        f(&mut slot.value)
    }

    /// Empties every slot.
    fn clear(&self) {
        for (at, slot) in self.slots.iter().enumerate() {
            let old = {
                let mut slot = lock(slot);
                self.keys[at].store(EMPTY, Ordering::Release);
                slot.state = None;
                mem::take(&mut slot.value)
            };
            drop(old);
        }
    }

    /// The slots `key` may be in, the one it hashes to first.
    fn window(&self, key: u64) -> impl Iterator<Item = usize> {
        let mask = (1usize << self.bits) - 1;
        let start = (key.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - self.bits)) as usize;
        (0..PROBE.min(mask + 1)).map(move |step| (start + step) & mask)
    }

    fn find(&self, key: u64) -> Option<(usize, MutexGuard<'_, Slot<S, T>>)> {
        for at in self.window(key) {
            if self.keys[at].load(Ordering::Acquire) != key {
                continue;
            }
            let slot = lock(&self.slots[at]);
            // It may have been given to another key while the lock was awaited.
            if self.keys[at].load(Ordering::Relaxed) == key {
                return Some((at, slot));
            }
        }
        None
    }

    fn add(&self, key: u64) -> (usize, MutexGuard<'_, Slot<S, T>>) {
        let claim = self.claim.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        // Another thread may have added it since `find`.
        if let Some(found) = self.find(key) {
            return found;
        }
        let at = self
            .window(key)
            .find(|&at| self.keys[at].load(Ordering::Acquire) == EMPTY)
            .or_else(|| self.window(key).min_by_key(|&at| self.used[at].load(Ordering::Relaxed)))
            .expect("a window has a slot");
        let mut slot = lock(&self.slots[at]);
        self.keys[at].store(key, Ordering::Release);
        self.used[at].store(self.clock.fetch_add(1, Ordering::Relaxed) + 1, Ordering::Relaxed);
        slot.state = None;
        let old = mem::take(&mut slot.value);
        drop(claim);
        drop(old);
        (at, slot)
    }
}

/// A slot whose user panicked holding it may be half written: it is emptied.
fn lock<S, T: Default>(slot: &Mutex<Slot<S, T>>) -> MutexGuard<'_, Slot<S, T>> {
    slot.lock().unwrap_or_else(|poisoned| {
        slot.clear_poison();
        let mut slot = poisoned.into_inner();
        slot.state = None;
        slot.value = T::default();
        slot
    })
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Barrier, thread};

    use super::{EMPTY, Solved, Table};

    type Goals = HashMap<u32, u32>;

    fn key() -> u64 {
        Solved::new().0
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
        assert!(table.keys.iter().all(|it| it.load(std::sync::atomic::Ordering::Relaxed) == EMPTY));
        for &query in &queries {
            assert_eq!(table.with(query, 0, |goals| goals.get(&7).copied()), None);
        }
    }

    #[test]
    fn a_key_added_by_many_threads_at_once_is_in_one_slot() {
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
        let holding = table
            .keys
            .iter()
            .filter(|it| it.load(std::sync::atomic::Ordering::Relaxed) == query)
            .count();
        assert_eq!(holding, 1);
        assert_eq!(table.with(query, 0, |goals| goals.len()), 8);
    }

    #[test]
    fn a_full_window_keeps_the_slots_used_last() {
        let table = Table::<u32, Goals>::new(4);
        let queries: Vec<u64> = (0..17).map(|_| key()).collect();
        for &query in &queries[..16] {
            table.with(query, 0, |goals| goals.insert(7, 1));
        }
        // Every slot is in every window of a 16-slot table; the first was used least lately.
        table.with(queries[16], 0, |goals| goals.insert(7, 1));
        assert_eq!(table.with(queries[0], 0, |goals| goals.get(&7).copied()), None);
        assert_eq!(table.with(queries[15], 0, |goals| goals.get(&7).copied()), Some(1));
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
        let shared_hits = std::sync::atomic::AtomicUsize::new(0);
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
                                shared_hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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

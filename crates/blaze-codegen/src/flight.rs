//! Single flight across builds: each function a build misses in the store is compiled by one
//! build of the process, whichever asks first. A build that reaches it while that compile is
//! under way waits for it; one that reaches it after takes the shape the memo keeps its
//! template under, and prepares nothing. Two proc macros of a job that both reach `syn`'s
//! parser compile it once (docs/v2/plan/lanes/compile-time-sched/notes.md).
//!
//! A claim is held from the store miss until its build lands the compile ([`Claims::land`]) or
//! lets it go ([`Claims::release`], and on drop). A build lets go of every claim it holds
//! before it waits for another's, so no two builds wait on each other's claims. A claim let go
//! of is taken by the next build that asks, which compiles it.
//!
//! One lock, taken once per build and phase (claim, land, release, wait) for a whole batch of
//! functions: never per function, never on a store hit. A batch only follows store misses,
//! each a compile of 50 to 200 µs, against which the lock is noise. Kept in the [`Memo`]
//! whose templates the shapes name: a landing outlives no template.
//!
//! [`Memo`]: crate::memo::Memo

#![forbid(unsafe_code)]

use std::{
    collections::hash_map::Entry,
    sync::{
        Condvar, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use blaze_ids::ContentId;
use rustc_hash::FxHashMap;

use crate::InstanceKey;

/// A function by the key its compiled code is stored under.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Flight {
    /// Being compiled by the build of this number.
    Flying(u64),
    /// Compiled: its template is in the memo under this shape.
    Landed(ContentId),
}

#[derive(Default)]
pub struct Flights {
    table: Mutex<FxHashMap<InstanceKey, Flight>>,
    /// Told on every landing and release.
    settled: Condvar,
    builds: AtomicU64,
}

/// What a build is told of a function it asks for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Asked {
    /// This build compiles it.
    Ours,
    /// Another build is compiling it: wait for it ([`Flights::wait`]).
    Theirs,
    /// Compiled already, its template in the memo under this shape.
    Landed(ContentId),
}

/// The claims of one build.
pub struct Claims<'f> {
    flights: &'f Flights,
    build: u64,
    held: Vec<InstanceKey>,
}

/// How often a waiting build looks at whether it was stopped.
const LOOK: Duration = Duration::from_millis(5);

impl Flights {
    pub fn new() -> Flights {
        Flights::default()
    }

    fn table(&self) -> MutexGuard<'_, FxHashMap<InstanceKey, Flight>> {
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A build's claims, none held yet.
    pub fn claims(&self) -> Claims<'_> {
        // alloc: none until a claim is held
        Claims { flights: self, build: self.builds.fetch_add(1, Ordering::Relaxed), held: Vec::new() }
    }

    /// Waits until each of `keys` landed or was let go of: its shape, or `None` for one let go
    /// of, which the waiting build asks for again. `None` overall once `stopped` says the
    /// waiting build stopped.
    pub fn wait(&self, keys: &[InstanceKey], stopped: impl Fn() -> bool) -> Option<Vec<Option<ContentId>>> {
        if keys.is_empty() {
            // alloc: none, the list is empty
            return Some(Vec::new());
        }
        let mut table = self.table();
        while keys.iter().any(|key| matches!(table.get(key), Some(Flight::Flying(_)))) {
            if stopped() {
                return None;
            }
            table = self.settled.wait_timeout(table, LOOK).unwrap_or_else(PoisonError::into_inner).0;
        }
        let shape = |key: &InstanceKey| match table.get(key) {
            Some(Flight::Landed(shape)) => Some(*shape),
            _ => None,
        };
        // alloc: per wait, sized by what it waited for
        Some(keys.iter().map(shape).collect())
    }

    /// How many functions have landed.
    pub fn landed(&self) -> usize {
        self.table().values().filter(|it| matches!(it, Flight::Landed(_))).count()
    }

    /// Keeps only the landings whose shape `kept` keeps: the memo's collection, between
    /// builds.
    pub(crate) fn retain(&mut self, kept: impl Fn(&ContentId) -> bool) {
        let table = self.table.get_mut().unwrap_or_else(PoisonError::into_inner);
        table.retain(|_, it| match it {
            Flight::Landed(shape) => kept(shape),
            Flight::Flying(_) => true,
        });
    }
}

impl Claims<'_> {
    /// Asks for each of `keys`, each once: this build's to compile when no build has it, else
    /// another's under way, else landed.
    pub fn claim(&mut self, keys: &[InstanceKey]) -> Vec<Asked> {
        let mut table = self.flights.table();
        // alloc: per batch of store misses, sized by it
        let mut asked = Vec::with_capacity(keys.len());
        for key in keys {
            asked.push(match table.entry(*key) {
                Entry::Vacant(slot) => {
                    slot.insert(Flight::Flying(self.build));
                    self.held.push(*key);
                    Asked::Ours
                }
                Entry::Occupied(it) => match *it.get() {
                    Flight::Flying(build) if build == self.build => Asked::Ours,
                    Flight::Flying(_) => Asked::Theirs,
                    Flight::Landed(shape) => Asked::Landed(shape),
                },
            });
        }
        asked
    }

    /// Lands each of `landed`, a function this build claimed and compiled into the memo under
    /// its shape, for every build that waits for it or asks after.
    pub fn land(&mut self, landed: &[(InstanceKey, ContentId)]) {
        if landed.is_empty() {
            return;
        }
        let mut table = self.flights.table();
        for (key, shape) in landed {
            if let Some(it) = table.get_mut(key)
                && *it == Flight::Flying(self.build)
            {
                *it = Flight::Landed(*shape);
            }
        }
        drop(table);
        self.flights.settled.notify_all();
    }

    /// Lets go of every claim this build holds and has not landed: a compile that failed, or
    /// that a stopped build never began. A build waiting for one asks for it again.
    pub fn release(&mut self) {
        if self.held.is_empty() {
            return;
        }
        let mut table = self.flights.table();
        for key in self.held.drain(..) {
            if table.get(&key) == Some(&Flight::Flying(self.build)) {
                table.remove(&key);
            }
        }
        drop(table);
        self.flights.settled.notify_all();
    }
}

impl Drop for Claims<'_> {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u8) -> InstanceKey {
        InstanceKey(ContentId([n; 32]))
    }

    fn shape(n: u8) -> ContentId {
        ContentId([n ^ 0x80; 32])
    }

    #[test]
    fn the_first_to_ask_compiles_and_the_rest_are_told() {
        let flights = Flights::new();
        let mut first = flights.claims();
        let mut second = flights.claims();
        assert_eq!(first.claim(&[key(1), key(2)]), [Asked::Ours, Asked::Ours]);
        assert_eq!(first.claim(&[key(1)]), [Asked::Ours], "a build's own claim is its own");
        assert_eq!(second.claim(&[key(1), key(3)]), [Asked::Theirs, Asked::Ours]);
        first.land(&[(key(1), shape(1))]);
        assert_eq!(second.claim(&[key(1)]), [Asked::Landed(shape(1))]);
        assert_eq!(flights.landed(), 1);
    }

    #[test]
    fn a_claim_let_go_of_is_the_next_askers() {
        let flights = Flights::new();
        let mut first = flights.claims();
        assert_eq!(first.claim(&[key(1)]), [Asked::Ours]);
        drop(first);
        let mut second = flights.claims();
        assert_eq!(second.claim(&[key(1)]), [Asked::Ours]);
        second.release();
        assert_eq!(flights.wait(&[key(1)], || false), Some(vec![None]));
    }

    #[test]
    fn a_landing_outlives_no_template_the_memo_drops() {
        let mut flights = Flights::new();
        let mut claims = flights.claims();
        claims.claim(&[key(1), key(2)]);
        claims.land(&[(key(1), shape(1)), (key(2), shape(2))]);
        drop(claims);
        flights.retain(|it| *it == shape(2));
        let mut again = flights.claims();
        assert_eq!(again.claim(&[key(1), key(2)]), [Asked::Ours, Asked::Landed(shape(2))]);
    }

    #[test]
    fn a_waiter_wakes_for_the_landing_and_a_stopped_one_gives_up() {
        let flights = &Flights::new();
        let mut owner = flights.claims();
        owner.claim(&[key(1), key(2)]);
        let (asked, told) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let waiter = scope.spawn(move || {
                let mut theirs = flights.claims();
                assert_eq!(theirs.claim(&[key(1), key(2)]), [Asked::Theirs, Asked::Theirs]);
                asked.send(()).unwrap();
                flights.wait(&[key(1), key(2)], || false)
            });
            told.recv().unwrap();
            std::thread::sleep(Duration::from_millis(20));
            owner.land(&[(key(1), shape(1))]);
            owner.release();
            assert_eq!(waiter.join().unwrap(), Some(vec![Some(shape(1)), None]));
        });
        let mut held = flights.claims();
        held.claim(&[key(3)]);
        assert_eq!(flights.wait(&[key(3)], || true), None);
    }

    /// Builds that each hold a claim the other waits for: each lets go of or lands its own
    /// before it waits, so both end, and every function is compiled once between them.
    #[test]
    fn builds_waiting_on_each_other_both_end_and_compile_each_once() {
        let flights = Flights::new();
        let compiled = Mutex::new(Vec::new());
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            for order in [[1u8, 2], [2, 1]] {
                let (flights, compiled, barrier) = (&flights, &compiled, &barrier);
                scope.spawn(move || {
                    let mut claims = flights.claims();
                    let mut missing: Vec<InstanceKey> = order.iter().map(|it| key(*it)).collect();
                    // Each first claims the one the other claims second.
                    claims.claim(&missing[..1]);
                    barrier.wait();
                    while !missing.is_empty() {
                        let asked = claims.claim(&missing);
                        let mut theirs = Vec::new();
                        let mut landed = Vec::new();
                        for (key, asked) in missing.iter().zip(asked) {
                            match asked {
                                Asked::Ours => {
                                    compiled.lock().unwrap().push(*key);
                                    landed.push((*key, shape(key.0.0[0])));
                                }
                                Asked::Theirs => theirs.push(*key),
                                Asked::Landed(_) => {}
                            }
                        }
                        claims.land(&landed);
                        claims.release();
                        let waited = flights.wait(&theirs, || false).unwrap();
                        missing = theirs.into_iter().zip(waited).filter(|it| it.1.is_none()).map(|it| it.0).collect();
                    }
                });
            }
        });
        let mut compiled = compiled.into_inner().unwrap();
        compiled.sort();
        assert_eq!(compiled, [key(1), key(2)]);
    }
}

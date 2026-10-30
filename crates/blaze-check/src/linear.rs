//! Linearizability against a sequential model: a [`History`] records each operation's call and
//! return, and [`History::check`] looks for one order of the operations that puts each after
//! every operation that returned before it was called and gives each operation the result it
//! had when applied to the [`Spec`] in that order (Wing and Gong's search, with Lowe's memo of
//! (operations done, model state) pairs as porcupine and lincheck keep, so a state reached twice
//! is searched once).
//!
//! In a model "returned before it was called" is happens-before, not execution order: under C11
//! an acquire load run after another thread's store may still read an older value, so a lookup
//! run after an insert returned, on a thread that never synchronized with it, may miss it (the
//! happens-before linearizability of Batty, Dodds and Gotsman's "Library abstraction for C/C++
//! concurrency"). Outside a model it is the order the calls ran in. The history's own
//! bookkeeping is `std`'s, invisible to the model: it adds no schedule point.

use std::{
    collections::HashSet,
    fmt::{self, Debug},
    hash::Hash,
    panic::Location,
    sync::{
        Mutex, PoisonError,
        atomic::{AtomicU32, Ordering::Relaxed},
    },
};

use rustc_hash::FxBuildHasher;

use crate::{clock::Clock, rt};

/// A sequential model of a structure: its state, and what each operation does to it.
pub trait Spec: Clone + Eq + Hash {
    type Op: Debug;
    type Ret: PartialEq + Debug;

    fn apply(&mut self, op: &Self::Op) -> Self::Ret;
}

/// The most operations a history checks: done sets are a `u64`.
pub const MAX_OPS: usize = 64;

struct Event<S: Spec> {
    thread: u8,
    op: S::Op,
    ret: S::Ret,
    /// Its call and return in the order the run made them, for the listing and outside a model.
    call: u32,
    done: u32,
    /// In a model: what the thread had seen of each thread at the call, and its own clock at
    /// the return.
    clocks: Option<(Clock, u32)>,
}

impl<S: Spec> Event<S> {
    /// `self` returned before `later` was called.
    fn precedes(&self, later: &Event<S>) -> bool {
        match (self.clocks, later.clocks) {
            (Some((_, returned)), Some((seen, _))) => seen.get(self.thread as usize) >= returned,
            _ => self.done < later.call,
        }
    }
}

/// The operations of one execution, each with its thread, result, call and return.
pub struct History<S: Spec> {
    events: Mutex<Vec<Event<S>>>,
    clock: AtomicU32,
}

impl<S: Spec> Default for History<S> {
    fn default() -> History<S> {
        History::new()
    }
}

impl<S: Spec> History<S> {
    pub fn new() -> History<S> {
        History { events: Mutex::new(Vec::with_capacity(MAX_OPS)), clock: AtomicU32::new(0) }
    }

    /// Runs `run` as the operation `op` of the calling thread, and records it with its result.
    pub fn record(&self, op: S::Op, run: impl FnOnce(&S::Op) -> S::Ret) -> S::Ret
    where
        S::Ret: Clone,
    {
        let seen = rt::with(|rt| (rt.current(), rt.threads[rt.current()].clock));
        let call = self.clock.fetch_add(1, Relaxed);
        let ret = run(&op);
        let done = self.clock.fetch_add(1, Relaxed);
        // A tick, so what the thread releases from here on carries the return and nothing it
        // released before does.
        let returned = rt::with(|rt| {
            let thread = rt.tick();
            rt.threads[thread].clock.get(thread)
        });
        let (thread, clocks) = match (seen, returned) {
            (Some((thread, seen)), Some(returned)) => (thread as u8, Some((seen, returned))),
            _ => (0, None),
        };
        let event = Event { thread, op, ret: ret.clone(), call, done, clocks };
        self.events.lock().unwrap_or_else(PoisonError::into_inner).push(event);
        ret
    }

    /// Panics, listing the history, if no linearization of it gives its results on `init`.
    /// Called inside a model, the panic is a finding with the schedule that made the history.
    #[track_caller]
    pub fn check(&self, init: S) {
        let mut events = self.events.lock().unwrap_or_else(PoisonError::into_inner);
        assert!(
            events.len() <= MAX_OPS,
            "blaze-check: a history of {} operations; `linear` checks up to {MAX_OPS}",
            events.len()
        );
        events.sort_unstable_by_key(|it| it.call);
        let before: Vec<u64> = events
            .iter()
            .map(|later| {
                events.iter().enumerate().filter(|(_, it)| it.precedes(later)).fold(0u64, |set, (at, _)| set | 1 << at)
            })
            .collect();
        let mut memo = HashSet::with_hasher(FxBuildHasher);
        if !linearizable(&events, &before, 0, init, &mut memo) {
            panic!(
                "blaze-check: not linearizable: no order of these operations, each placed after \
                 every one whose return happens before its call, gives these results on the \
                 sequential model (at {}):\n{}",
                Location::caller(),
                Listing(&events)
            );
        }
    }

    /// Forgets the recorded operations, for a history reused across calls.
    pub fn clear(&self) {
        self.events.lock().unwrap_or_else(PoisonError::into_inner).clear();
        self.clock.store(0, Relaxed);
    }
}

/// Whether the operations not in `done` can follow in some order allowed by the run: `before`
/// holds, per operation, the set of operations that must come before it.
fn linearizable<S: Spec>(
    events: &[Event<S>],
    before: &[u64],
    done: u64,
    state: S,
    memo: &mut HashSet<(u64, S), FxBuildHasher>,
) -> bool {
    let all = if events.len() == MAX_OPS { u64::MAX } else { (1u64 << events.len()) - 1 };
    if done == all {
        return true;
    }
    for (at, event) in events.iter().enumerate() {
        if done & (1 << at) != 0 || before[at] & !done != 0 {
            continue;
        }
        let mut next = state.clone();
        if next.apply(&event.op) != event.ret {
            continue;
        }
        let done = done | (1 << at);
        if memo.insert((done, next.clone())) && linearizable(events, before, done, next, memo) {
            return true;
        }
    }
    false
}

struct Listing<'a, S: Spec>(&'a [Event<S>]);

impl<S: Spec> fmt::Display for Listing<'_, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for event in self.0 {
            writeln!(
                f,
                "  thread {}: {:?} -> {:?} (called at {}, returned at {})",
                event.thread, event.op, event.ret, event.call, event.done
            )?;
        }
        Ok(())
    }
}

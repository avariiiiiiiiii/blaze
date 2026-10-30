//! Linearizability against a sequential model: a [`History`] records each operation's call and
//! return in the order a model execution ran them, and [`History::check`] looks for one order of
//! the operations that keeps every call-before-return order of the run and gives each operation
//! the result it had when applied to the [`Spec`] in that order (Wing and Gong's search, with
//! Lowe's memo of (operations done, model state) pairs as porcupine and lincheck keep, so a state
//! reached twice is searched once).
//!
//! The history's own bookkeeping is `std`'s, invisible to the model: it adds no schedule point,
//! and on the model's one OS thread its counter is the run's real-time order.

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

use crate::rt;

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
    call: u32,
    done: u32,
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
        let thread = rt::current().map_or(0, |(thread, _)| thread as u8);
        let call = self.clock.fetch_add(1, Relaxed);
        let ret = run(&op);
        let done = self.clock.fetch_add(1, Relaxed);
        let event = Event { thread, op, ret: ret.clone(), call, done };
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
        let mut memo = HashSet::with_hasher(FxBuildHasher);
        if !linearizable(&events, 0, init, &mut memo) {
            panic!(
                "blaze-check: not linearizable: no order of these operations, each placed between \
                 its call and its return, gives these results on the sequential model (at {}):\n{}",
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

/// Whether the operations not in `done` can follow in some order allowed by the run.
fn linearizable<S: Spec>(
    events: &[Event<S>],
    done: u64,
    state: S,
    memo: &mut HashSet<(u64, S), FxBuildHasher>,
) -> bool {
    let all = if events.len() == MAX_OPS { u64::MAX } else { (1u64 << events.len()) - 1 };
    if done == all {
        return true;
    }
    // An operation may go next if it was called before every pending one returned.
    let first_return = (0..events.len())
        .filter(|&at| done & (1 << at) == 0)
        .map(|at| events[at].done)
        .min()
        .unwrap_or(u32::MAX);
    for (at, event) in events.iter().enumerate() {
        if event.call > first_return {
            break;
        }
        if done & (1 << at) != 0 {
            continue;
        }
        let mut next = state.clone();
        if next.apply(&event.op) != event.ret {
            continue;
        }
        let done = done | (1 << at);
        if memo.insert((done, next.clone())) && linearizable(events, done, next, memo) {
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

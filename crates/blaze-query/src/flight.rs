//! Single flight: one runner at a time works on a key, whatever view asked, and the others
//! wait for it. A wait that would close a cycle of runners waiting on each other is
//! cancelled instead (#1301): the runner that would close it gets [`Cancelled`], lets go of
//! what it holds as it unwinds, and its request is reposted.

use std::{
    cell::RefCell,
    sync::{
        Condvar, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use crate::{
    Cancelled, QueryKey,
    table::{FxMap, lock},
};

/// One request's root and everything it runs inline, on one thread.
pub(crate) struct Run {
    pub(crate) id: u64,
    /// The keys it is running, outermost first: for naming a cycle.
    pub(crate) stack: RefCell<Vec<QueryKey>>,
}

static RUNS: AtomicU64 = AtomicU64::new(1);

impl Run {
    pub(crate) fn new() -> Run {
        Run { id: RUNS.fetch_add(1, Ordering::Relaxed), stack: RefCell::new(Vec::new()) }
    }
}

#[derive(Default)]
struct State {
    owner: FxMap<QueryKey, u64>,
    waiting: FxMap<u64, QueryKey>,
}

#[derive(Default)]
pub(crate) struct Flight {
    state: Mutex<State>,
    done: Condvar,
}

/// A key held by a run; let go when dropped, on any path out, unwinding included.
pub(crate) struct Claim<'a> {
    flight: &'a Flight,
    key: QueryKey,
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        lock(&self.flight.state).owner.remove(&self.key);
        self.flight.done.notify_all();
    }
}

impl Flight {
    /// Holds `key` for `run`, waiting while another run holds it.
    pub(crate) fn claim<'a>(&'a self, key: QueryKey, run: &Run) -> Result<Claim<'a>, Cancelled> {
        let mut state = lock(&self.state);
        loop {
            let owner = match state.owner.get(&key) {
                None => {
                    state.owner.insert(key, run.id);
                    return Ok(Claim { flight: self, key });
                }
                Some(&owner) => owner,
            };
            if owner == run.id {
                let stack = run.stack.borrow();
                panic!(
                    "a query cycle: {} asks {key:?} while running it",
                    stack.iter().map(|it| format!("{it:?}")).collect::<Vec<_>>().join(" -> ")
                );
            }
            let mut next = owner;
            let mut steps = 0usize;
            while let Some(wanted) = state.waiting.get(&next) {
                let Some(&holder) = state.owner.get(wanted) else { break };
                if holder == run.id {
                    return Err(Cancelled);
                }
                next = holder;
                steps += 1;
                assert!(steps <= state.waiting.len(), "the wait-for graph has a cycle without us");
            }
            state.waiting.insert(run.id, key);
            state = self.done.wait(state).unwrap_or_else(std::sync::PoisonError::into_inner);
            state.waiting.remove(&run.id);
        }
    }
}

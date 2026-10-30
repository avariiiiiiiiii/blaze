// Shuttle doesn't like panics inside of its runtime.
#![cfg(not(feature = "shuttle"))]

use std::cell::Cell;
use std::panic::{catch_unwind, resume_unwind};

use salsa::{Cancelled, Database};

use crate::setup::{Knobs, KnobsDatabase};
use crate::sync::{mpsc, thread};

std::thread_local! {
    /// Whether `cycle_query` abandons its work for a pending write, as a host does before it
    /// writes.
    static ABANDON: Cell<bool> = const { Cell::new(false) };
}

#[salsa::input]
struct Input {
    #[returns(copy)]
    value: u32,
}

/// A query with cycle recovery that will be abandoned for an LRU eviction.
/// This uses `cycle_initial` which gives it `FallbackImmediate` cycle recovery strategy,
/// meaning it goes through `execute_maybe_iterate` and has `PoisonProvisionalIfPanicking`.
#[salsa::tracked(returns(copy), cycle_initial = cycle_initial)]
fn cycle_query(db: &dyn KnobsDatabase, input: Input) -> u32 {
    // Signal that we've started the cycle query
    db.signal(1);
    // Wait for the go-ahead to continue
    db.wait_for(2);
    if ABANDON.with(Cell::get) {
        resume_unwind(Box::new(Cancelled::PendingWrite));
    }
    inner_query(db, input)
}

#[salsa::tracked(returns(copy))]
fn inner_query(db: &dyn KnobsDatabase, input: Input) -> u32 {
    input.value(db)
}

fn cycle_initial(_db: &dyn KnobsDatabase, _id: salsa::Id, _input: Input) -> u32 {
    0
}

/// Runs `cycle_query` until it abandons its work for a pending write.
fn abandon_cycle_query(db: &Knobs, input: Input) {
    // Single-threaded: let `cycle_query` through its waits.
    db.signal(2);
    ABANDON.with(|abandon| abandon.set(true));
    let abandoned = catch_unwind(|| cycle_query(db, input));
    ABANDON.with(|abandon| abandon.set(false));
    let err = abandoned.expect_err("the query should have been abandoned");
    assert!(
        matches!(err.downcast_ref::<Cancelled>(), Some(Cancelled::PendingWrite)),
        "the query should have been cancelled, got: {err:?}"
    );
}

/// Test that `trigger_lru_eviction` after a cycle query was abandoned causes the query to
/// recompute.
#[test]
fn lru_eviction_recomputes_cycle_query() {
    let mut db = Knobs::default();
    let input = Input::new(&db, 42);

    abandon_cycle_query(&db, input);

    // Evict, which does NOT bump the revision.
    db.trigger_lru_eviction();

    assert_eq!(cycle_query(&db, input), 42);
}

/// Test that local cancellation via CancellationToken does NOT poison
/// cycle queries, because local cancellation is properly disabled during
/// cycle iteration.
#[test]
fn local_cancellation_does_not_poison_cycle_query() {
    let db = Knobs::default();
    let input = Input::new(&db, 42);
    let db = &db;

    thread::scope(|scope| {
        // Thread 1: Start a cycle query, handing out the token of its task
        let (token_tx, token_rx) = mpsc::channel();
        let t1 = scope.spawn(move || {
            token_tx.send(db.cancellation_token()).unwrap();
            // This query has cycle recovery, so cancellation should be disabled
            cycle_query(db, input)
        });
        let token = token_rx.recv().unwrap();

        // Wait for thread 1 to enter the cycle query
        db.wait_for(1);

        // Try to cancel - but this should be ignored because cancellation is disabled
        token.cancel();

        // Let thread 1 continue
        db.signal(2);

        // Thread 1 should complete successfully despite cancellation request
        // because local cancellation is disabled during cycle iteration
        let r1 = t1.join().unwrap();
        assert_eq!(
            r1, 42,
            "Query should complete successfully despite cancellation request"
        );
    });
}

/// Test that the recomputed query remains valid after a revision bump.
#[test]
fn revision_bump_after_cancellation_recomputes_cycle_query() {
    let mut db = Knobs::default();
    let input = Input::new(&db, 42);

    abandon_cycle_query(&db, input);

    db.trigger_lru_eviction();

    assert_eq!(cycle_query(&db, input), 42);

    // Trigger a synthetic write to bump the revision
    db.synthetic_write(salsa::Durability::HIGH);

    assert_eq!(cycle_query(&db, input), 42);
}

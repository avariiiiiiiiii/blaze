// Shuttle doesn't like panics inside of its runtime.
#![cfg(not(feature = "shuttle"))]

//! `stop_readers` reaches a query inside a fixpoint cycle, where a task's own token is
//! ignored, and the next write clears it, leaving no provisional memo to reuse.

use std::sync::atomic::{AtomicBool, Ordering};

use salsa::{Cancelled, Database};

use crate::setup::{Knobs, KnobsDatabase};

static STOP: AtomicBool = AtomicBool::new(false);

#[salsa::input]
struct Input {
    #[returns(copy)]
    value: u32,
}

#[salsa::tracked(returns(copy), cycle_fn = cycle_fn, cycle_initial = cycle_initial)]
fn cycle_a(db: &dyn KnobsDatabase, input: Input) -> u32 {
    let value = cycle_b(db, input);
    if STOP.swap(false, Ordering::Relaxed) {
        db.cancellation_token().cancel();
        db.stop_readers();
    }
    inside(db, input);
    value
}

#[salsa::tracked(returns(copy), cycle_fn = cycle_fn, cycle_initial = cycle_initial)]
fn cycle_b(db: &dyn KnobsDatabase, input: Input) -> u32 {
    let value = cycle_a(db, input);
    value.saturating_add(1).min(input.value(db))
}

#[salsa::tracked]
fn inside(db: &dyn KnobsDatabase, input: Input) {
    input.value(db);
}

fn cycle_initial(_db: &dyn KnobsDatabase, _id: salsa::Id, _input: Input) -> u32 {
    0
}

fn cycle_fn(
    _db: &dyn KnobsDatabase,
    _cycle: &salsa::Cycle,
    _last_provisional_value: &u32,
    value: u32,
    _input: Input,
) -> u32 {
    value
}

#[test]
fn stop_readers_unwinds_a_cycle_head_and_a_write_clears_it() {
    let mut db = Knobs::default();
    let input = Input::new(&db, 3);
    STOP.store(true, Ordering::Relaxed);
    assert!(matches!(Cancelled::catch(|| cycle_a(&db, input)), Err(Cancelled::PendingWrite)));
    assert!(db.readers_stopped());
    assert!(matches!(Cancelled::catch(|| inside(&db, input)), Err(Cancelled::PendingWrite)));
    db.trigger_cancellation();
    assert!(!db.readers_stopped());
    assert_eq!(cycle_a(&db, input), 3);
}

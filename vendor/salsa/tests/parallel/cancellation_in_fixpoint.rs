// Shuttle doesn't like panics inside of its runtime.
#![cfg(not(feature = "shuttle"))]

//! A pending write must not leave reusable provisional memos behind.
//!
//! `cycle_a` and `cycle_b` establish a fixpoint cycle. The host abandons that cycle for a write
//! (it throws `Cancelled::PendingWrite`), and the write does not advance the revision. The retry
//! must start from a fresh cycle initial value.

use std::cell::Cell;
use std::panic::resume_unwind;

use salsa::{Cancelled, Database};

use crate::setup::{Knobs, KnobsDatabase};

std::thread_local! {
    /// Whether `cycle_a` abandons its work for a pending write when it reaches its
    /// cancellation point.
    static ABANDON: Cell<bool> = const { Cell::new(false) };
}

#[salsa::input]
struct Input {
    #[returns(copy)]
    value: u32,
}

#[salsa::tracked(returns(copy), cycle_fn = cycle_fn, cycle_initial = cycle_initial)]
fn cycle_a(db: &dyn KnobsDatabase, input: Input) -> u32 {
    let value = cycle_b(db, input);

    if ABANDON.with(Cell::get) {
        resume_unwind(Box::new(Cancelled::PendingWrite));
    }
    cancellation_point(db, input);

    value
}

#[salsa::tracked(returns(copy), cycle_fn = cycle_fn, cycle_initial = cycle_initial)]
fn cycle_b(db: &dyn KnobsDatabase, input: Input) -> u32 {
    let value = cycle_a(db, input);
    value.saturating_add(1).min(input.value(db))
}

#[salsa::tracked]
fn cancellation_point(db: &dyn KnobsDatabase, input: Input) {
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
fn same_revision_cancellation_leaves_fixpoint_cycle_reusable() {
    let mut db = Knobs::default();
    let input = Input::new(&db, 3);

    ABANDON.with(|abandon| abandon.set(true));
    let abandoned = Cancelled::catch(|| cycle_a(&db, input));
    ABANDON.with(|abandon| abandon.set(false));
    assert!(matches!(abandoned, Err(Cancelled::PendingWrite)));

    db.trigger_cancellation();

    assert_eq!(cycle_a(&db, input), 3);
}

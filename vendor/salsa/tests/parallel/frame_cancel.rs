// Shuttle doesn't like panics inside of its runtime.
#![cfg(not(feature = "shuttle"))]

//! A query whose run the frame cap cancels is run again by the thread that waited on it: the
//! waiter is not failed with a propagated panic.
use std::sync::atomic::{AtomicBool, Ordering};

use crate::setup::{Knobs, KnobsDatabase};

static CANCEL_FIRST_RUN: AtomicBool = AtomicBool::new(true);

#[salsa::tracked(returns(copy))]
// dyn: a test's database view, as the other parallel tests take it
fn query_a(db: &dyn KnobsDatabase) -> u32 {
    if CANCEL_FIRST_RUN.swap(false, Ordering::SeqCst) {
        db.signal(1);
        db.wait_for(2);
        blaze_mem::frame::cancel();
    }
    7
}

#[test]
fn a_waiter_reruns_a_query_whose_run_was_cancelled() {
    let db = Knobs::default();
    let db = &db;
    std::thread::scope(|scope| {
        let owner = scope.spawn(move || blaze_mem::frame::task(blaze_mem::Tag::Query, |_| query_a(db)));
        let waiter = scope.spawn(move || {
            db.wait_for(1);
            db.signal_on_will_block(2);
            query_a(db)
        });
        let cancelled = owner.join().expect_err("the owner's run was cancelled");
        assert!(cancelled.downcast_ref::<blaze_mem::frame::Cancelled>().is_some());
        assert_eq!(waiter.join().expect("the waiter ran it again"), 7);
    });
}

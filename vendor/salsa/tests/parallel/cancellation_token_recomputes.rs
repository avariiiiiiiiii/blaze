// Shuttle doesn't like panics inside of its runtime.
#![cfg(not(feature = "shuttle"))]

//! Test for cancellation when another query is blocked on the cancelled thread.
use salsa::{Cancelled, Database};

use crate::setup::{Knobs, KnobsDatabase};
use crate::sync::{mpsc, thread};

#[salsa::tracked(returns(copy))]
fn query_a(db: &dyn KnobsDatabase) -> u32 {
    query_b(db)
}

#[salsa::tracked(returns(copy))]
fn query_b(db: &dyn KnobsDatabase) -> u32 {
    db.signal(1);
    db.wait_for(3);
    query_c(db)
}

#[salsa::tracked(returns(copy))]
fn query_c(_db: &dyn KnobsDatabase) -> u32 {
    1
}

#[test]
fn execute() {
    let db = Knobs::default();
    let db = &db;
    thread::scope(|scope| {
        let (token_tx, token_rx) = mpsc::channel();
        let t1 = scope.spawn(move || {
            token_tx.send(db.cancellation_token()).unwrap();
            query_a(db)
        });
        let token = token_rx.recv().unwrap();
        db.wait_for(1);
        let t2 = scope.spawn(move || {
            db.signal_on_will_block(2);
            query_a(db)
        });
        db.wait_for(2);
        token.cancel();
        db.signal(3);
        let (r1, r2) = (t1.join(), t2.join());
        let r1 = *r1.unwrap_err().downcast::<salsa::Cancelled>().unwrap();
        assert!(matches!(r1, Cancelled::Local), "{r1:?}");
        assert_eq!(r2.unwrap(), 1);
    });
}

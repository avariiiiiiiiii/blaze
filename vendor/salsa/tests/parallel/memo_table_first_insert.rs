use crate::sync::thread;
use crate::{Knobs, KnobsDatabase};

#[salsa::input]
struct Input {
    #[returns(copy)]
    value: u32,
}

#[salsa::tracked(returns(copy))]
fn query_a(db: &dyn KnobsDatabase, input: Input) -> u32 {
    db.signal(1);
    db.wait_for(2);
    input.value(db)
}

#[salsa::tracked(returns(copy))]
fn query_b(db: &dyn KnobsDatabase, input: Input) -> u32 {
    db.wait_for(1);
    db.signal(2);
    input.value(db)
}

#[test_log::test]
fn concurrent_first_insert() {
    crate::sync::check(|| {
        let db = Knobs::default();
        let input = Input::new(&db, 42);
        let db = &db;

        thread::scope(|scope| {
            let a = scope.spawn(move || query_a(db, input));
            let b = scope.spawn(move || query_b(db, input));

            assert_eq!(a.join().unwrap(), 42);
            assert_eq!(b.join().unwrap(), 42);
        });
    });
}

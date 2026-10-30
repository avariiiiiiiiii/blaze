//! Claims racing on the same keys from several tasks, each starting at a different key: every
//! key's query runs once per revision however many tasks ask for it at once, the others wait
//! for it (the claim word's contended path: a contender parks the claim in the shard table while
//! the owner may be releasing it), and every task sees the value.

use std::sync::atomic::{AtomicUsize, Ordering};

use salsa::Setter;

use crate::Knobs;
use crate::KnobsDatabase;
use crate::sync::thread;

static RUNS: AtomicUsize = AtomicUsize::new(0);

#[salsa::input]
struct Input {
    #[returns(copy)]
    value: u32,
}

#[salsa::tracked(returns(copy))]
fn double(db: &dyn KnobsDatabase, input: Input) -> u32 {
    RUNS.fetch_add(1, Ordering::Relaxed);
    input.value(db) * 2
}

const TASKS: usize = 3;

/// Every task fetches every key, task `t` starting at key `t`, and checks it reads `2 * value`.
fn fetch_all(db: &Knobs, inputs: &[Input], values: &[u32]) {
    thread::scope(|scope| {
        for task in 0..TASKS {
            scope.spawn(move || {
                for step in 0..inputs.len() {
                    let key = (task + step) % inputs.len();
                    assert_eq!(double(db, inputs[key]), values[key] * 2);
                }
            });
        }
    });
}

#[test_log::test]
fn racing_claims_run_each_key_once() {
    crate::sync::check(|| {
        RUNS.store(0, Ordering::Relaxed);
        let mut db = Knobs::default();
        let mut values = vec![1, 2, 3];
        let inputs: Vec<Input> = values.iter().map(|&value| Input::new(&db, value)).collect();

        fetch_all(&db, &inputs, &values);
        assert_eq!(RUNS.load(Ordering::Relaxed), inputs.len());

        values[1] = 10;
        inputs[1].set_value(&mut db).to(values[1]);
        fetch_all(&db, &inputs, &values);
        assert_eq!(
            RUNS.load(Ordering::Relaxed),
            inputs.len() + 1,
            "only the changed key runs again, once"
        );
    });
}

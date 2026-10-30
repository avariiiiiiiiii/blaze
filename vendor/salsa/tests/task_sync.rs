#![cfg(all(feature = "inventory", not(feature = "shuttle")))]

//! One database shared by `&` across threads, and tasks pumped above a waiting task on the same
//! thread (`Database::task_depth`).

use std::cell::Cell;
use std::panic::AssertUnwindSafe;
use std::sync::Mutex;

use salsa::{Cancelled, Setter};

#[salsa::db]
trait LogDb: salsa::Database {
    fn log(&self, entry: String);
}

#[salsa::db]
#[derive(Default)]
struct Db {
    storage: salsa::Storage<Self>,
    log: Mutex<Vec<String>>,
}

impl Db {
    fn take_log(&self) -> Vec<String> {
        std::mem::take(&mut *self.log.lock().unwrap())
    }
}

#[salsa::db]
impl salsa::Database for Db {
    fn task_depth(&self) -> u32 {
        u32::try_from(depth()).expect("a thread runs more than `u32::MAX` tasks")
    }
}

#[salsa::db]
impl LogDb for Db {
    fn log(&self, entry: String) {
        self.log.lock().unwrap().push(entry);
    }
}

#[salsa::input]
struct Numbers {
    a: u32,
    b: u32,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Pump {
    Nothing,
    /// Runs `inner` as a task pumped above `outer`.
    Inner,
    /// Asks `outer`, which the task below holds, from a task pumped above it.
    Outer,
}

std::thread_local! {
    static DEPTH: Cell<usize> = const { Cell::new(0) };
    static PUMP: Cell<Pump> = const { Cell::new(Pump::Nothing) };
    static PUMPED: Cell<Option<Result<u32, bool>>> = const { Cell::new(None) };
}

fn depth() -> usize {
    DEPTH.with(Cell::get)
}

/// Runs `f` as the task the current thread runs above the one it runs now, as a work pool does
/// when a waiting task runs another job.
fn pumped<R>(f: impl FnOnce() -> R) -> R {
    struct Restore(usize);
    impl Drop for Restore {
        fn drop(&mut self) {
            DEPTH.with(|depth| depth.set(self.0));
        }
    }
    let below = depth();
    let _restore = Restore(below);
    DEPTH.with(|depth| depth.set(below + 1));
    f()
}

#[salsa::tracked]
fn outer(db: &dyn LogDb, numbers: Numbers) -> u32 {
    db.log("outer".to_owned());
    let a = numbers.a(db);
    match PUMP.with(Cell::get) {
        Pump::Nothing => {}
        Pump::Inner => {
            let value = pumped(|| inner(db, numbers));
            PUMPED.with(|pumped| pumped.set(Some(Ok(value))));
        }
        Pump::Outer => {
            let result = pumped(|| Cancelled::catch(AssertUnwindSafe(|| outer(db, numbers))));
            let result = result.map_err(|cancelled| matches!(cancelled, Cancelled::WouldWaitOnItself));
            PUMPED.with(|pumped| pumped.set(Some(result)));
        }
    }
    a * 10
}

#[salsa::tracked]
fn inner(db: &dyn LogDb, numbers: Numbers) -> u32 {
    db.log("inner".to_owned());
    numbers.b(db) + 1
}

#[salsa::tracked]
fn total(db: &dyn LogDb, numbers: Numbers, i: u32) -> u32 {
    db.log(format!("total({i})"));
    if i == 0 {
        numbers.a(db)
    } else {
        total(db, numbers, i - 1) + i
    }
}

/// `outer`'s value and whether it ran, the first time, after `b` changes and after `a` changes,
/// with `pump` the first time.
fn outer_runs(pump: Pump) -> Vec<(u32, bool)> {
    let mut db = Db::default();
    let numbers = Numbers::new(&db, 1, 2);
    let mut runs = Vec::new();
    let mut run = |db: &Db| {
        let value = outer(db, numbers);
        let ran = db.take_log().iter().any(|entry| entry == "outer");
        runs.push((value, ran));
    };

    PUMP.with(|cell| cell.set(pump));
    run(&db);
    PUMP.with(|cell| cell.set(Pump::Nothing));

    numbers.set_b(&mut db).to(5);
    run(&db);
    numbers.set_a(&mut db).to(2);
    run(&db);
    runs
}

#[test]
fn storage_is_sync() {
    fn assert_sync<T: Sync + Send>() {}
    assert_sync::<salsa::Storage<Db>>();
    assert_sync::<Db>();
}

#[test]
fn one_database_shared_across_threads_computes_each_value_once() {
    const THREADS: u32 = 8;
    const KEYS: u32 = 32;
    let db = Db::default();
    let numbers = Numbers::new(&db, 7, 0);

    std::thread::scope(|scope| {
        for thread in 0..THREADS {
            let db = &db;
            scope.spawn(move || {
                for step in 0..KEYS {
                    let i = (thread * 4 + step) % KEYS;
                    assert_eq!(total(db, numbers, i), 7 + i * (i + 1) / 2);
                }
            });
        }
    });

    let mut log = db.take_log();
    log.sort();
    let mut expected: Vec<String> = (0..KEYS).map(|i| format!("total({i})")).collect();
    expected.sort();
    assert_eq!(log, expected);
}

#[test]
fn pumped_task_reads_are_not_edges_of_the_query_below() {
    PUMP.with(|cell| cell.set(Pump::Inner));
    let mut db = Db::default();
    let numbers = Numbers::new(&db, 1, 2);

    assert_eq!(outer(&db, numbers), 10);
    PUMP.with(|cell| cell.set(Pump::Nothing));
    assert_eq!(PUMPED.with(Cell::take), Some(Ok(3)));
    assert_eq!(db.take_log(), ["outer", "inner"]);

    // `b` was read by the pumped `inner` alone, so `outer` does not depend on it.
    numbers.set_b(&mut db).to(5);
    assert_eq!(outer(&db, numbers), 10);
    assert_eq!(db.take_log(), Vec::<String>::new());
    assert_eq!(inner(&db, numbers), 6);
    assert_eq!(db.take_log(), ["inner"]);
}

#[test]
fn pumped_task_asking_the_key_below_is_cancelled() {
    PUMP.with(|cell| cell.set(Pump::Outer));
    let db = Db::default();
    let numbers = Numbers::new(&db, 1, 2);

    assert_eq!(outer(&db, numbers), 10);
    PUMP.with(|cell| cell.set(Pump::Nothing));
    assert_eq!(PUMPED.with(Cell::take), Some(Err(true)));
    assert_eq!(db.take_log(), ["outer"]);
}

#[test]
fn query_below_a_pumped_task_is_as_if_nothing_was_pumped() {
    let plain = outer_runs(Pump::Nothing);
    assert_eq!(plain, [(10, true), (10, false), (20, true)]);
    assert_eq!(outer_runs(Pump::Inner), plain);
    assert_eq!(outer_runs(Pump::Outer), plain);
    assert_eq!(PUMPED.with(Cell::take), Some(Err(true)));
}

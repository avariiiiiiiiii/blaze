#![cfg(feature = "inventory")]

//! Tracked functions over a concrete `#[salsa::db]` struct (`db: &Db`) rather than `&dyn Trait`
//! (docs/v2/plan/lanes/frontend/db.md, "salsa: tracked functions over a concrete database"):
//! they memoize, invalidate and evict as the `dyn` ones do, share a storage with them, and read
//! the task depth through the database's own `task_depth`.

#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicUsize, Ordering};

use salsa::Setter;

#[salsa::db]
#[derive(Default)]
struct Db {
    storage: salsa::Storage<Self>,
    executed: AtomicUsize,
    depth_reads: AtomicUsize,
}

#[salsa::db]
impl salsa::Database for Db {
    fn task_depth(&self) -> u32 {
        self.depth_reads.fetch_add(1, Ordering::Relaxed);
        0
    }
}

impl Db {
    fn executed(&self) -> usize {
        self.executed.load(Ordering::Relaxed)
    }
}

#[salsa::input]
struct Input {
    #[returns(copy)]
    n: u32,
}

#[salsa::tracked(returns(copy))]
fn double(db: &Db, input: Input) -> u32 {
    db.executed.fetch_add(1, Ordering::Relaxed);
    input.n(db) * 2
}

#[salsa::tracked(returns(copy))]
fn add(db: &Db, input: Input, k: u32) -> u32 {
    db.executed.fetch_add(1, Ordering::Relaxed);
    double(db, input) + k
}

#[salsa::tracked(returns(copy), lru = 2)]
fn triple(db: &Db, input: Input) -> u32 {
    db.executed.fetch_add(1, Ordering::Relaxed);
    input.n(db) * 3
}

#[salsa::tracked(returns(copy))]
fn through_dyn(db: &dyn salsa::Database, input: Input) -> u32 {
    input.n(db) + 1
}

#[salsa::tracked(returns(copy))]
fn calls_dyn(db: &Db, input: Input) -> u32 {
    through_dyn(db, input) * 10
}

#[salsa::tracked(returns(copy))]
fn on_default_db(db: &salsa::DatabaseImpl, input: Input) -> u32 {
    input.n(db) + 100
}

#[test]
fn memoizes_and_invalidates() {
    let mut db = Db::default();
    let input = Input::new(&db, 3);

    assert_eq!(double(&db, input), 6);
    assert_eq!(double(&db, input), 6);
    assert_eq!(db.executed(), 1);

    input.set_n(&mut db).to(4);
    assert_eq!(double(&db, input), 8);
    assert_eq!(db.executed(), 2);
}

#[test]
fn interned_arguments() {
    let mut db = Db::default();
    let input = Input::new(&db, 5);

    assert_eq!(add(&db, input, 1), 11);
    assert_eq!(add(&db, input, 2), 12);
    assert_eq!(add(&db, input, 1), 11);
    // `add` twice and `double` once: the second `add` reuses `double`'s memo.
    assert_eq!(db.executed(), 3);

    input.set_n(&mut db).to(6);
    assert_eq!(add(&db, input, 1), 13);
    assert_eq!(db.executed(), 5);
}

#[test]
fn lru_and_evict_values() {
    let mut db = Db::default();
    let inputs: Vec<Input> = (0..4).map(|n| Input::new(&db, n)).collect();
    for &input in &inputs {
        assert_eq!(triple(&db, input), input.n(&db) * 3);
    }
    assert_eq!(db.executed(), 4);

    triple::set_lru_capacity(&mut db, 4);
    assert_eq!(double(&db, inputs[1]), 2);
    assert_eq!(double::evict_values(&mut db), 1);
    assert_eq!(double(&db, inputs[1]), 2);
}

#[test]
fn shares_storage_with_dyn_functions() {
    let db = Db::default();
    let input = Input::new(&db, 2);
    assert_eq!(calls_dyn(&db, input), 30);
    assert_eq!(through_dyn(&db, input), 3);
}

#[test]
fn default_database_is_concrete_too() {
    let db = salsa::DatabaseImpl::new();
    let input = Input::new(&db, 1);
    assert_eq!(on_default_db(&db, input), 101);
    assert_eq!(on_default_db(&db, input), 101);
}

#[test]
fn hit_reads_the_databases_task_depth() {
    let db = Db::default();
    let input = Input::new(&db, 7);
    assert_eq!(double(&db, input), 14);

    let before = db.depth_reads.load(Ordering::Relaxed);
    assert_eq!(double(&db, input), 14);
    assert!(
        db.depth_reads.load(Ordering::Relaxed) > before,
        "a hit attaches the database and finds its task through `Database::task_depth`"
    );
}

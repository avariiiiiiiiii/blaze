//! A query execution's uncontended claim and release (`SyncTable::try_claim` and
//! `ClaimGuard::drop`, src/function/sync.rs), on one key.
//!
//! Bound (docs/v2/plan/lanes/db/speed-of-light.md, "Claims"): one CAS on the row's claim word to
//! claim and one to release, about 2 × 20 cycles, **≈ 12 ns** at 3.5 GHz. Finding the word (the
//! table page, the function's column from its cache, the column's claim region) is about 15
//! instructions on lines a fetch has just loaded for the memo lookup, which this loop keeps in
//! L1. The shard `Mutex` and `hashbrown` insert and remove the word replaced took 40 to 60 ns.
#![forbid(unsafe_code)]

use std::hint::black_box;
use std::sync::{Arc, Mutex};

use salsa::{DatabaseKeyIndex, EventKind, Storage};

fn main() {
    divan::main();
}

#[salsa::db]
struct Db {
    storage: Storage<Self>,
}

#[salsa::db]
impl salsa::Database for Db {}

#[salsa::input]
struct Input {
    #[returns(copy)]
    value: u32,
}

#[salsa::tracked(returns(copy))]
fn double(db: &dyn salsa::Database, input: Input) -> u32 {
    input.value(db) * 2
}

/// A database and the key `double` executed under.
fn executed_key() -> (Db, DatabaseKeyIndex) {
    let executed = Arc::new(Mutex::new(None));
    let db = Db {
        storage: Storage::new(Some(Box::new({
            let executed = executed.clone();
            move |event| {
                if let EventKind::WillExecute { database_key } = event.kind {
                    *executed.lock().unwrap() = Some(database_key);
                }
            }
        }))),
    };
    let input = Input::new(&db, 21);
    assert_eq!(double(&db, input), 42);
    let key = executed.lock().unwrap().expect("`double` executed");
    (db, key)
}

/// Claims and releases one key, nobody else claiming: the claim word's CAS there and back.
#[divan::bench]
fn uncontended_claim_and_release(bencher: divan::Bencher) {
    let (db, key) = executed_key();
    let claim = salsa::plumbing::function::ClaimBench::new(&db, key);
    bencher.bench_local(|| black_box(&claim).claim_and_release());
}

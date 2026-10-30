#![cfg(feature = "inventory")]

//! An `lru` function over enough keys (313 table pages, more than 64 pages per worker on any
//! machine with two cores) that its eviction walks the table on several threads keeps exactly
//! the values one serial walk keeps: the most recently verified, ties broken in key order.

#![forbid(unsafe_code)]

use std::sync::{Arc, Mutex};

use salsa::{Database as _, Durability};

/// The keys whose values were dropped, on whatever thread dropped them.
static DROPPED: Mutex<Vec<u32>> = Mutex::new(Vec::new());

#[derive(Debug, PartialEq, Eq, Hash)]
struct Value(u32);

impl Drop for Value {
    fn drop(&mut self) {
        DROPPED.lock().unwrap().push(self.0);
    }
}

#[salsa::input]
struct Key {
    #[returns(copy)]
    n: u32,
}

const KEYS: u32 = 40_000;
const CAPACITY: usize = 3_000;

#[salsa::tracked(returns(clone), lru = 3000)]
fn value(db: &dyn salsa::Database, key: Key) -> Arc<Value> {
    Arc::new(Value(key.n(db)))
}

/// The serial rule, per key: the revision its memo was last verified in, while it holds a value.
struct Model {
    verified: Vec<Option<u32>>,
}

impl Model {
    fn fetch(&mut self, key: u32, revision: u32) {
        self.verified[key as usize] = Some(revision);
    }

    /// The keys the next revision's LRU pass drops, in key order.
    fn new_revision(&mut self) -> Vec<u32> {
        let mut held: Vec<(u32, u32)> = (0..KEYS)
            .filter_map(|key| Some((self.verified[key as usize]?, key)))
            .collect();
        held.sort_unstable();
        let mut dropped: Vec<u32> = held
            .iter()
            .take(held.len().saturating_sub(CAPACITY))
            .map(|&(_, key)| key)
            .collect();
        for &key in &dropped {
            self.verified[key as usize] = None;
        }
        dropped.sort_unstable();
        dropped
    }
}

fn take_dropped() -> Vec<u32> {
    let mut dropped = std::mem::take(&mut *DROPPED.lock().unwrap());
    dropped.sort_unstable();
    dropped
}

#[test]
fn parallel_eviction_keeps_what_the_serial_walk_keeps() {
    let mut db = salsa::DatabaseImpl::new();
    let keys: Vec<Key> = (0..KEYS).map(|n| Key::new(&db, n)).collect();
    let mut model = Model {
        verified: vec![None; KEYS as usize],
    };

    // Each revision fetches a different mix: all keys, then some the last pass dropped and some
    // it kept, then a sparse spread, so the cutoff falls inside ranges and between them.
    let fetches: [&dyn Fn(u32) -> bool; 4] = [
        &|_| true,
        &|n| n % 3 == 0 && n >= KEYS / 2,
        &|n| n % 7 == 1,
        &|n| n % 1000 < 5 || n > KEYS - 100,
    ];
    for (revision, fetch) in (1u32..).zip(fetches) {
        for (n, &key) in (0..KEYS).zip(&keys) {
            if fetch(n) {
                assert_eq!(value(&db, key).0, n);
                model.fetch(n, revision);
            }
        }
        db.synthetic_write(Durability::HIGH);
        assert_eq!(
            take_dropped(),
            model.new_revision(),
            "the values dropped entering revision {}",
            revision + 1
        );
    }
    let held = model.verified.iter().filter(|at| at.is_some()).count();
    assert_eq!(held, CAPACITY);
}

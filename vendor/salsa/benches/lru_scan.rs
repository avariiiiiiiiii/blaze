//! One revision's LRU pass over 1M memos of one `lru` function
//! (docs/v2/plan/lanes/db/speed-of-light.md, "LRU eviction").
//!
//! Bound: the pass needs 4 bytes of `verified_at` per memo, 4 MB for 1M, ≈ 17 µs at all-core
//! memory bandwidth (≈ 0.27 ms on one core). The walk reads about 100 bytes a memo (slot, row
//! pointer, memo line): 100 MB, 3.3 ms on one core, ≈ 0.2 ms split by page over every core,
//! which is this design's target; the radix select over the collected 4 MB is one or two more
//! reads of it. What is left above the target is starting the workers (≈ 10 µs each).
//!
//! Each sample is a whole `synthetic_write`, so it also counts the revision's other per-revision
//! work, which for a database holding only this function is small next to the pass.

#![forbid(unsafe_code)]

use std::cell::RefCell;

use salsa::{Database as _, Durability};

fn main() {
    divan::main();
}

#[salsa::input]
struct Key {
    #[returns(copy)]
    n: u32,
}

#[salsa::tracked(returns(copy), lru = 1)]
fn value(db: &dyn salsa::Database, key: Key) -> u64 {
    u64::from(key.n(db)).wrapping_mul(0x9e37_79b9_7f4a_7c15)
}

const MEMOS: usize = 1 << 20;
const CHUNKS: usize = 8;
const CHUNK: usize = MEMOS / CHUNKS;

struct State {
    db: salsa::DatabaseImpl,
    keys: Vec<Key>,
    /// The chunk of keys the last pass dropped, which the next sample fetches again.
    dropped: usize,
}

fn state(capacity: usize) -> State {
    let mut db = salsa::DatabaseImpl::new();
    value::set_lru_capacity(&mut db, capacity);
    let keys: Vec<Key> = (0..MEMOS as u32).map(|n| Key::new(&db, n)).collect();
    for &key in &keys {
        divan::black_box(value(&db, key));
    }
    db.synthetic_write(Durability::LOW);
    State {
        db,
        keys,
        dropped: 0,
    }
}

/// The pass when every value fits: both walks' first half and the ranking's sum, nothing dropped.
#[divan::bench(sample_count = 20, sample_size = 1)]
fn pass_keeps_all(bencher: divan::Bencher) {
    let mut state = state(2 * MEMOS);
    bencher.bench_local(|| state.db.synthetic_write(Durability::LOW));
}

/// The pass when an eighth must go: collect, rank, and drop and fingerprint 131K values. Each
/// sample first fetches again (untimed) the chunk the last pass dropped, so the oldest chunk is
/// always the next one and every pass drops exactly one.
#[divan::bench(sample_count = 20, sample_size = 1)]
fn pass_drops_an_eighth(bencher: divan::Bencher) {
    let state = RefCell::new(state(MEMOS - CHUNK));
    bencher
        .with_inputs(|| {
            let state = &mut *state.borrow_mut();
            let chunk = state.dropped * CHUNK..(state.dropped + 1) * CHUNK;
            for &key in &state.keys[chunk] {
                divan::black_box(value(&state.db, key));
            }
            state.dropped = (state.dropped + 1) % CHUNKS;
        })
        .bench_local_values(|()| state.borrow_mut().db.synthetic_write(Durability::LOW));
}

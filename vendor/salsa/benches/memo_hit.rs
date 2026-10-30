//! Memo hits on many pages: 64K cached memos of one function over 512 table pages, fetched in
//! the revision they were made.
//!
//! Bound (docs/v2/plan/lanes/db/speed-of-light.md, "Memo hit"): with the task's page cache
//! (`table/page_cache.rs`) a hit reads the cache entry (L1), the column (L1, the same for every
//! key of the function), the row and the memo: 2 lines that miss, **≈ 5.4 ns** hot. 512 pages
//! are more than the cache's 64 entries, so `scattered` also pays the cache's misses; in order,
//! a page's 128 keys share one entry, the order a walk over a file's items gives. Before: the
//! page list and the `Page` on every hit, 4 lines, ≈ 11 ns.

#![forbid(unsafe_code)]

use std::hint::black_box;

const KEYS: usize = 65_536;

fn main() {
    divan::main();
}

#[divan::bench(args = [false, true])]
fn hits(bencher: divan::Bencher, scattered: bool) {
    bencher
        .with_inputs(|| {
            let db = salsa::DatabaseImpl::default();
            let keys: Vec<Key> = (0..KEYS).map(|i| Key::new(&db, i as u32)).collect();
            let order: Vec<Key> = (0..KEYS)
                .map(|i| keys[if scattered { (i * 40_503) % KEYS } else { i }])
                .collect();
            for &key in &order {
                black_box(double(&db, key));
            }
            (db, order)
        })
        .bench_local_refs(|(db, order)| {
            let mut sum = 0u64;
            for &key in order.iter() {
                sum += u64::from(double(db, black_box(key)));
            }
            black_box(sum);
        });
}

#[salsa::input]
struct Key {
    #[returns(copy)]
    value: u32,
}

#[salsa::tracked(returns(copy))]
#[inline(never)]
fn double(db: &dyn salsa::Database, key: Key) -> u32 {
    key.value(db) * 2
}

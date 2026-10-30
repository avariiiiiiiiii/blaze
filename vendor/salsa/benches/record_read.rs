//! Recording reads: a query that runs again and reads 64 distinct memos that are all still
//! valid, so its cost is the hits and the recording of each read.
//!
//! Bound (docs/v2/plan/lanes/db/speed-of-light.md, "Recording a read"): a compare with the last
//! edge, a short scan of the frame's edges (rustc's dedup, up to 8, then a hash index) and a
//! store of the edge, which carries its dependency's node from the memo header, so finishing the
//! query writes edge words with no node lookup: **≈ 3 ns** a new read on top of the hit (≈ 5 ns,
//! `memo_hit.rs`), so `rerun_wide` targets ≈ 64 × 8 ns ≈ 0.5 µs a run plus the run's own
//! overhead. Before: an `IndexSet` insert per read and a node probe per edge at finish, ≈ 15 ns
//! an edge on a node cache miss.

#![forbid(unsafe_code)]

use std::hint::black_box;

use salsa::Setter;

const READS: usize = 64;
const RUNS: usize = 256;

fn main() {
    divan::main();
}

#[divan::bench]
fn rerun_wide(bencher: divan::Bencher) {
    bencher
        .with_inputs(|| {
            let mut db = salsa::DatabaseImpl::default();
            let leaves: Vec<Leaf> = (0..READS).map(|i| Leaf::new(&db, i as u32)).collect();
            let wides: Vec<Wide> = (0..RUNS)
                .map(|salt| Wide::new(&db, salt as u32, leaves.clone()))
                .collect();
            for &wide in &wides {
                black_box(read_wide(&db, wide));
            }
            for &wide in &wides {
                let salt = wide.salt(&db);
                wide.set_salt(&mut db).to(salt + 1);
            }
            (db, wides)
        })
        .bench_local_refs(|(db, wides)| {
            for &wide in wides.iter() {
                black_box(read_wide(db, wide));
            }
        });
}

#[salsa::input]
struct Leaf {
    #[returns(copy)]
    value: u32,
}

#[salsa::input]
struct Wide {
    #[returns(copy)]
    salt: u32,
    #[returns(ref)]
    leaves: Vec<Leaf>,
}

#[salsa::tracked(returns(copy))]
#[inline(never)]
fn leaf(db: &dyn salsa::Database, input: Leaf) -> u32 {
    input.value(db)
}

#[salsa::tracked(returns(copy))]
#[inline(never)]
fn read_wide(db: &dyn salsa::Database, wide: Wide) -> u64 {
    let salt = u64::from(wide.salt(db));
    wide.leaves(db)
        .iter()
        .map(|&input| u64::from(leaf(db, input)))
        .sum::<u64>()
        + salt
}

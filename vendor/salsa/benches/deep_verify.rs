//! Deep verify over shared dependencies, the shape of a warm build after an edit: many readers
//! each read a few of a shared pool of memos, and an unrelated write makes every one verify its
//! edges. The first reader to reach a pool memo verifies it through its memo; every later edge
//! to it is checked by its node's color (`graph::colors`).
//!
//! Bound (docs/v2/plan/lanes/db/speed-of-light.md, "Deep verify"): one random load per edge,
//! issued 8 edges ahead. Here the pool's colors are 4096 × 8 bytes, in L2, so the target is
//! ≈ 2 ns an edge on one core: `verify_shared` reads 16384 × 16 edges, **≈ 0.5 ms**. Before
//! colors each edge found its dependency's memo through its key, ≈ 8 dependent loads (≈ 20 ns
//! an edge from cache).

#![forbid(unsafe_code)]

use std::hint::black_box;

use salsa::{Database, Durability};

const POOL: usize = 4096;
const READERS: usize = 16384;
const EDGES: usize = 16;

fn main() {
    divan::main();
}

#[divan::bench]
fn verify_shared(bencher: divan::Bencher) {
    bencher
        .with_inputs(|| {
            let mut db = salsa::DatabaseImpl::default();
            let pool: Vec<Leaf> = (0..POOL).map(|i| Leaf::new(&db, i as u32)).collect();
            let readers: Vec<Reader> = (0..READERS)
                .map(|r| {
                    let picks = (0..EDGES)
                        .map(|e| pool[(r * 2_654_435_761 + e * 40_503) % POOL])
                        .collect();
                    Reader::new(&db, picks)
                })
                .collect();
            let expected = sum(&db, &readers);
            db.synthetic_write(Durability::LOW);
            (db, readers, expected)
        })
        .bench_local_refs(|(db, readers, expected)| {
            assert_eq!(black_box(sum(db, readers)), *expected);
        });
}

fn sum(db: &salsa::DatabaseImpl, readers: &[Reader]) -> u64 {
    readers.iter().map(|&reader| read_all(db, reader)).sum()
}

#[salsa::input]
struct Leaf {
    #[returns(copy)]
    value: u32,
}

#[salsa::input]
struct Reader {
    #[returns(ref)]
    picks: Vec<Leaf>,
}

#[salsa::tracked(returns(copy))]
#[inline(never)]
fn leaf(db: &dyn salsa::Database, input: Leaf) -> u32 {
    input.value(db).wrapping_mul(3)
}

#[salsa::tracked(returns(copy))]
#[inline(never)]
fn read_all(db: &dyn salsa::Database, reader: Reader) -> u64 {
    reader
        .picks(db)
        .iter()
        .map(|&input| u64::from(leaf(db, input)))
        .sum()
}

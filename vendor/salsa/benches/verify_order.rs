//! Deep verify over a graph too big for the cache, after a compaction that renumbers the nodes
//! in walk order (`Graph::compact_renumbered`, src/graph.rs) and after one that keeps the order
//! keys were found in (docs/v2/plan/lanes/db/graph.md, section 2).
//!
//! The shape: `POOL` leaf memos, each over an input field of its own, run first in a shuffled
//! order (as parallel tasks would find them), so their nodes are numbered at random; then
//! `READERS` readers of `EDGES` leaves each, three quarters from the reader's own module of 8
//! leaves and the rest from the first 1% (hubs). A synthetic write makes every reader verify its
//! edges. About 4.5M nodes: 36 MB of colors, past the last-level cache.
//!
//! Bound (docs/v2/plan/lanes/db/speed-of-light.md, "verify walk layout"): with colors in walk
//! order a color load is ≈ 0.7 to 1 ns an edge (compute-bound: ≈ 10 instructions an edge at 4 IPC
//! and 3.5 GHz; the color lines stream at 30 GB/s); in key-discovery order it is a DRAM miss,
//! ≈ 5.5 ns with about 20 in flight. A round loads E ≈ 8.4M reader edges + 2.1M leaf edges +
//! 0.26M field edges ≈ 10.8M colors: ≈ 11 ms in walk order. Each of the 2.4M memos is also
//! verified once through its memo (its first edge this revision), ≈ 20 ns, ≈ 48 ms, which the
//! numbering doesn't change. Target: `verify(renumbered = true)` within 10% of ≈ 60 ms; the
//! key-discovery order pays ≈ 4.5 ns more on each of the ≈ 6.3M edges a color decides, ≈ 30 ms.

#![forbid(unsafe_code)]

use std::hint::black_box;

use salsa::plumbing::ZalsaDatabase as _;
use salsa::{Database, Durability};

const POOL: usize = 1 << 21;
const READERS: usize = 1 << 18;
const EDGES: usize = 32;
const MODULE: usize = POOL / READERS;
const HUBS: usize = POOL / 100;

fn main() {
    divan::main();
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

fn sum(db: &salsa::DatabaseImpl, readers: &[Reader]) -> u64 {
    readers.iter().map(|&reader| read_all(db, reader)).sum()
}

/// A small deterministic generator (xorshift), so every run builds the same graph.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }
}

#[divan::bench(args = [true, false], sample_count = 10, sample_size = 1)]
fn verify(bencher: divan::Bencher, renumbered: bool) {
    let mut db = salsa::DatabaseImpl::default();
    db.zalsa_mut().set_closure_filters(false);
    db.zalsa_mut().set_walk_order(renumbered);
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let pool: Vec<Leaf> = (0..POOL).map(|i| Leaf::new(&db, i as u32)).collect();
    let mut found: Vec<usize> = (0..POOL).collect();
    for i in (1..POOL).rev() {
        found.swap(i, rng.below(i + 1));
    }
    for &i in &found {
        black_box(leaf(&db, pool[i]));
    }
    let readers: Vec<Reader> = (0..READERS)
        .map(|r| {
            let picks = (0..EDGES)
                .map(|e| {
                    if e % 4 == 3 {
                        pool[rng.below(HUBS)]
                    } else {
                        pool[r * MODULE + rng.below(MODULE)]
                    }
                })
                .collect();
            Reader::new(&db, picks)
        })
        .collect();
    let expected = sum(&db, &readers);
    // One compaction, renumbering or not, then none while measuring.
    db.zalsa_mut().compact_every_revision(true);
    db.synthetic_write(Durability::LOW);
    db.zalsa_mut().compact_every_revision(false);
    assert_eq!(sum(&db, &readers), expected);
    bencher.bench_local(|| {
        db.synthetic_write(Durability::LOW);
        assert_eq!(black_box(sum(&db, &readers)), expected);
    });
}

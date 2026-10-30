//! Edges at rest (docs/v2/plan/lanes/db/speed-of-light.md, "Edges at rest"): the bytes an edge
//! takes once a compaction settles it (`Graph::compact`, src/graph.rs and src/graph/edges.rs),
//! and the cost of reading a settled span against a span of fresh 4-byte words.
//!
//! Bound: an edge names one of N nodes and its kind, ⌈log2 N⌉ + 1 bits. At N = 10M nodes
//! (nushell, about 100M edges) that is **25 bits an edge**, 3.125 bytes: 312 MB for 100M edges
//! against 400 MB of words. Reading must stay **within 10% of words**: a word is one load an
//! edge; a settled group of 8 edges is, for spans of 32 edges or more, four 16-byte loads, a byte
//! shuffle, a shift and a mask per 64-bit lane and a narrowing (AVX2 or NEON), and below that
//! one 8-byte load, shift and mask an edge. `get` (deep verify's random access) is one load,
//! a multiply, a shift and a mask either way.
//!
//! Scale: 10M nodes as nushell, 20M edges in place of 100M, so the four stores (words and settled,
//! at two span lengths) fit beside the rest of a bench machine's memory. The width, and so the
//! bytes an edge, depends only on the node count; the read cost an edge does not depend on the
//! count once the edges are far past the last-level cache (80 MB of words, 62 MB settled). Spans
//! average 10 edges (nushell's 100M edges over 10M memos) or 64 (the SIMD path's case).
//!
//! `main` prints the bytes an edge and checks both forms read the same words before the timings.
#![forbid(unsafe_code)]

use std::sync::OnceLock;

use divan::counter::ItemsCount;
use salsa::plumbing::graph::EdgesBench;

const NODES: u32 = 10_000_000;
const EDGES: u32 = 20_000_000;
const MEANS: [u32; 2] = [10, 64];

fn main() {
    for mean in MEANS {
        let words = store(mean, false);
        let settled = store(mean, true);
        assert_eq!(
            words.sum_iter(),
            settled.sum_iter(),
            "settled edges read other words than they were"
        );
        assert_eq!(words.sum_get(), settled.sum_get());
        println!(
            "edges at rest, {NODES} nodes, {} edges in spans of {mean} on average: words {:.3} \
             bytes an edge, settled {:.3} ({} bits; bound {:.3})",
            settled.edges(),
            words.bytes_per_edge(),
            settled.bytes_per_edge(),
            settled.width(),
            f64::from(settled.width()) / 8.0,
        );
    }
    divan::main();
}

/// The store of spans averaging `mean` edges, as recorded (words) or compacted (settled).
fn store(mean: u32, settled: bool) -> &'static EdgesBench {
    static STORES: [OnceLock<EdgesBench>; 4] = [const { OnceLock::new() }; 4];
    let mean_index = MEANS
        .iter()
        .position(|&m| m == mean)
        .expect("a bench mean");
    let index = 2 * mean_index + usize::from(settled);
    STORES[index].get_or_init(|| {
        let mut store = EdgesBench::new(NODES, EDGES, mean);
        if settled {
            store.compact();
        }
        store
    })
}

fn iterate(bencher: divan::Bencher, store: &'static EdgesBench) {
    bencher
        .counter(ItemsCount::new(store.edges() as usize))
        .bench_local(|| store.sum_iter());
}

fn get(bencher: divan::Bencher, store: &'static EdgesBench) {
    bencher
        .counter(ItemsCount::new(store.edges() as usize))
        .bench_local(|| store.sum_get());
}

/// Every span's words in order, fresh 4-byte words: the baseline.
#[divan::bench(args = MEANS, sample_count = 20)]
fn iterate_words(bencher: divan::Bencher, mean: u32) {
    iterate(bencher, store(mean, false));
}

/// Every span's words in order, settled at 25 bits: within 10% of `iterate_words`.
#[divan::bench(args = MEANS, sample_count = 20)]
fn iterate_settled(bencher: divan::Bencher, mean: u32) {
    iterate(bencher, store(mean, true));
}

/// Every span's words position by position, fresh 4-byte words.
#[divan::bench(args = MEANS, sample_count = 20)]
fn get_words(bencher: divan::Bencher, mean: u32) {
    get(bencher, store(mean, false));
}

/// Every span's words position by position, settled: within 10% of `get_words`.
#[divan::bench(args = MEANS, sample_count = 20)]
fn get_settled(bencher: divan::Bencher, mean: u32) {
    get(bencher, store(mean, true));
}

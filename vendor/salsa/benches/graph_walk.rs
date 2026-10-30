//! Deep verify's walk over a query graph the size of nushell's (#734: `synthetic(7, N, 10)`, N
//! of 1M and 10M keys, 10 edges a key) under three numberings of its nodes (as salsa makes them,
//! depth-first preorder, reverse Cuthill-McKee), with and without prefetching colors, and the
//! cost of renumbering itself, which runs once per frozen revision.
//!
//! The bound. A walk of E edges loads E colors. With the colors in DRAM and ~20 misses in flight
//! a random load costs ~5.5 ns of throughput per core, a sequential one ~0.3 ns (8 colors per
//! 64-byte line at ~30 GB/s); the edge words stream at 4 bytes each. So on one core: as made
//! ≈ 5.5 ns/edge, and the bound with a walk-order layout ≈ (E × 4 B + touched color lines × 64 B)
//! / 30 GB/s, which with the per-edge work (load the target, its color and its seen bit, add,
//! branch: ~10 instructions, ~0.7 ns at 4 a cycle and 3.5 GHz) comes to 0.5 to 1 ns/edge.
//! Targets: `walk_preorder_prefetch` at 10M nodes within 10% of that bound (≤ 1.1 ns/edge, about
//! 110 ms for the 100M edges); `number_dfs_preorder` plus `renumber_to_preorder` under 2 s at 10M
//! nodes / 100M edges (O(N + E) at ~20 ns per random access).
//!
//! The walk benches count edges, so divan's throughput is edges a second: 1e9 / it is ns/edge.

#![forbid(unsafe_code)]

use std::hint::black_box;
use std::sync::OnceLock;

use blaze_ids::rng::SplitMix64;
use divan::counter::ItemsCount;
use salsa::graph_bench::order::{self, Csr};

const SIZES: &[u32] = &[1_000_000, 10_000_000];
const MEAN_EDGES: u32 = 10;
const SEED: u64 = 7;

fn main() {
    divan::main();
}

/// One numbering of the graph, with its roots and colors moved to match.
struct Layout {
    csr: Csr,
    roots: Vec<u32>,
    colors: Vec<u64>,
}

struct Graphs {
    as_made: Layout,
    preorder: Layout,
    rcm: Layout,
    /// The preorder numbering of `as_made`, for timing `renumber`.
    to_preorder: Vec<u32>,
}

fn graphs(nodes: u32) -> &'static Graphs {
    static SMALL: OnceLock<Graphs> = OnceLock::new();
    static LARGE: OnceLock<Graphs> = OnceLock::new();
    let cell = match nodes {
        1_000_000 => &SMALL,
        10_000_000 => &LARGE,
        _ => panic!("no graph of {nodes} nodes"),
    };
    cell.get_or_init(|| build(nodes))
}

fn build(nodes: u32) -> Graphs {
    let csr = order::synthetic(SEED, nodes, MEAN_EDGES);
    let mut rng = SplitMix64::new(SEED);
    let colors: Vec<u64> = (0..nodes).map(|_| rng.next_u64()).collect();
    let to_preorder = order::dfs_preorder(&csr);
    let to_rcm = order::reverse_cuthill_mckee(&csr);
    let preorder = moved(&csr, &colors, &to_preorder);
    let rcm = moved(&csr, &colors, &to_rcm);
    let as_made = Layout { roots: order::roots(&csr), csr, colors };

    let expected = walk_once(&as_made, false);
    assert_eq!(walk_once(&preorder, true), expected, "renumbering keeps the walk");
    assert_eq!(walk_once(&rcm, true), expected, "renumbering keeps the walk");
    eprintln!(
        "graph_walk: {nodes} nodes, {} edges, {} roots; line changes: as made {}, dfs preorder {}, \
         rcm {}",
        as_made.csr.targets.len(),
        as_made.roots.len(),
        order::line_changes(&as_made.csr),
        order::line_changes(&preorder.csr),
        order::line_changes(&rcm.csr),
    );
    Graphs { as_made, preorder, rcm, to_preorder }
}

fn moved(csr: &Csr, colors: &[u64], new_of_old: &[u32]) -> Layout {
    let renumbered = order::renumber(csr, new_of_old);
    let mut moved_colors = vec![0u64; colors.len()];
    for (old, &new) in new_of_old.iter().enumerate() {
        moved_colors[new as usize] = colors[old];
    }
    Layout { roots: order::roots(&renumbered), csr: renumbered, colors: moved_colors }
}

fn walk_once(layout: &Layout, prefetch: bool) -> u64 {
    let (mut stack, mut seen) = (Vec::new(), Vec::new());
    order::verify_walk_from(
        &layout.csr,
        &layout.roots,
        &layout.colors,
        prefetch,
        &mut stack,
        &mut seen,
    )
}

/// Times one verify walk over every node of `layout`, from its roots.
fn walk(bencher: divan::Bencher, layout: &'static Layout, prefetch: bool) {
    let mut stack = Vec::new();
    let mut seen = Vec::new();
    bencher.counter(ItemsCount::new(layout.csr.targets.len())).bench_local(|| {
        seen.fill(0);
        black_box(order::verify_walk_from(
            black_box(&layout.csr),
            &layout.roots,
            &layout.colors,
            prefetch,
            &mut stack,
            &mut seen,
        ))
    });
}

#[divan::bench(args = SIZES, sample_count = 10)]
fn walk_as_made(bencher: divan::Bencher, nodes: u32) {
    walk(bencher, &graphs(nodes).as_made, false);
}

#[divan::bench(args = SIZES, sample_count = 10)]
fn walk_as_made_prefetch(bencher: divan::Bencher, nodes: u32) {
    walk(bencher, &graphs(nodes).as_made, true);
}

#[divan::bench(args = SIZES, sample_count = 10)]
fn walk_preorder(bencher: divan::Bencher, nodes: u32) {
    walk(bencher, &graphs(nodes).preorder, false);
}

#[divan::bench(args = SIZES, sample_count = 10)]
fn walk_preorder_prefetch(bencher: divan::Bencher, nodes: u32) {
    walk(bencher, &graphs(nodes).preorder, true);
}

#[divan::bench(args = SIZES, sample_count = 10)]
fn walk_rcm(bencher: divan::Bencher, nodes: u32) {
    walk(bencher, &graphs(nodes).rcm, false);
}

#[divan::bench(args = SIZES, sample_count = 10)]
fn walk_rcm_prefetch(bencher: divan::Bencher, nodes: u32) {
    walk(bencher, &graphs(nodes).rcm, true);
}

/// Numbering the graph as made in walk order.
#[divan::bench(args = SIZES, sample_count = 5)]
fn number_dfs_preorder(bencher: divan::Bencher, nodes: u32) {
    let graphs = graphs(nodes);
    bencher.counter(ItemsCount::new(graphs.as_made.csr.targets.len())).bench_local(|| {
        black_box(order::dfs_preorder(black_box(&graphs.as_made.csr))).len()
    });
}

/// Applying that numbering: the frozen graph's new edge column.
#[divan::bench(args = SIZES, sample_count = 5)]
fn renumber_to_preorder(bencher: divan::Bencher, nodes: u32) {
    let graphs = graphs(nodes);
    bencher.counter(ItemsCount::new(graphs.as_made.csr.targets.len())).bench_local(|| {
        black_box(order::renumber(black_box(&graphs.as_made.csr), &graphs.to_preorder))
            .targets
            .len()
    });
}

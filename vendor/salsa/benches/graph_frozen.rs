//! The graph at rest (`graph::frozen`): its size an edge, opening it, and reading every node's
//! edges out of it in place, on a nushell-sized synthetic graph (`order::synthetic(7, N, 10)`,
//! N of 1M and 10M keys, 10 edges a key) numbered as made and in depth-first preorder.
//!
//! Bound (docs/v2/plan/lanes/db/graph.md, "Packed, mapped graph at rest"):
//! - size: edges ≈ 1 to 1.5 bytes each when deltas are small (one StreamVByte data byte, a
//!   quarter control byte, a count byte a node); the target for the preorder stream is ≤ 1.6
//!   bytes an edge, and ≤ 4 bytes an edge for the whole section (keys and filter words are 16
//!   bytes a node, offsets ≈ 2.1).
//! - opening: the header and four length checks, no decode, O(1): ≤ 1 µs at 10M nodes (the
//!   image's CRC check of the section is the image's, not timed here).
//! - reading: `decode_edges` at ≈ 2.5 G edges/s on one core (benches/graph_varint.rs's bound)
//!   plus ≈ 5 ns a node to find and start its list; at 10 edges a node ≈ 9 ns a node, 90 ms for
//!   10M nodes. Target: `read_all` within 10% of that in preorder.
//!
//! The read benches count edges, so divan's throughput is edges a second.

#![forbid(unsafe_code)]

use std::hint::black_box;
use std::sync::OnceLock;

use divan::counter::ItemsCount;
use salsa::graph_bench::{frozen, order};

const SIZES: &[u32] = &[1_000_000, 10_000_000];

fn main() {
    divan::main();
}

struct Sections {
    edges: usize,
    as_made: Vec<u8>,
    preorder: Vec<u8>,
}

fn sections(nodes: u32) -> &'static Sections {
    static SMALL: OnceLock<Sections> = OnceLock::new();
    static LARGE: OnceLock<Sections> = OnceLock::new();
    let cell = match nodes {
        1_000_000 => &SMALL,
        10_000_000 => &LARGE,
        _ => panic!("no graph of {nodes} nodes"),
    };
    cell.get_or_init(|| {
        let csr = order::synthetic(7, nodes, 10);
        let to_preorder = order::dfs_preorder(&csr);
        let sections = Sections {
            edges: csr.targets.len(),
            as_made: frozen::write(&csr, None),
            preorder: frozen::write(&csr, Some(&to_preorder)),
        };
        let mut out = Vec::new();
        assert_eq!(
            frozen::read_all(&sections.as_made, &mut out),
            csr.targets.iter().map(|&t| u64::from(t)).sum::<u64>(),
            "the section reads back the graph"
        );
        for (name, section) in [("as made", &sections.as_made), ("dfs preorder", &sections.preorder)] {
            let (stream, whole) = frozen::bytes_per_edge(section, sections.edges);
            eprintln!(
                "graph_frozen: {nodes} nodes, {} edges, {name}: {stream:.3} stream bytes an edge, \
                 {whole:.3} section bytes an edge",
                sections.edges
            );
        }
        sections
    })
}

#[divan::bench(args = SIZES)]
fn open(bencher: divan::Bencher, nodes: u32) {
    let sections = sections(nodes);
    bencher.bench(|| frozen::open_nodes(black_box(&sections.preorder)));
}

#[divan::bench(args = SIZES)]
fn read_all_as_made(bencher: divan::Bencher, nodes: u32) {
    let sections = sections(nodes);
    let mut out = Vec::new();
    bencher
        .counter(ItemsCount::new(sections.edges))
        .bench_local(|| frozen::read_all(black_box(&sections.as_made), &mut out));
}

#[divan::bench(args = SIZES)]
fn read_all_preorder(bencher: divan::Bencher, nodes: u32) {
    let sections = sections(nodes);
    let mut out = Vec::new();
    bencher
        .counter(ItemsCount::new(sections.edges))
        .bench_local(|| frozen::read_all(black_box(&sections.preorder), &mut out));
}

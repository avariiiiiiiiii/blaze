//! StreamVByte decoding of salsa's packed edges (`salsa::graph_bench::varint`), on 10M edge
//! words of a memo-shaped distribution: lists of geometric length (mean 10) whose nodes mostly
//! follow the last by a small positive step, one in ten anywhere among 2^21 nodes.
//!
//! The bound: StreamVByte decodes 4 values per ~6 instructions (load, table lookup, shuffle,
//! store, advance): at 3.5 GHz and ~3 IPC about 7 G values/s per core, memory permitting (1.3
//! bytes a value: 9 GB/s, under one core's 30 GB/s streaming). The target is within 10% of 4 G
//! values/s for `decode` on groups, and for `decode_edges` (adds the prefix sum, 1 cycle a value)
//! within 10% of 2.5 G values/s. Bytes per edge for this distribution, including the count byte
//! and the control bytes, are printed before the benches run (target 1 to 1.5).

#![forbid(unsafe_code)]

use std::hint::black_box;
use std::sync::LazyLock;

use blaze_ids::rng::SplitMix64;
use divan::counter::{BytesCount, ItemsCount};
use salsa::graph_bench::varint;

const EDGES: usize = 10_000_000;
const NODES: u64 = 1 << 21;

struct Data {
    /// Each list's base node and edge count, in stream order.
    lists: Vec<(u32, usize)>,
    /// Every list through `encode_edges`, back to back.
    edges: Vec<u8>,
    /// Every list's deltas through one `encode`.
    raw: Vec<u8>,
    /// The words of every list, back to back.
    words: Vec<u32>,
}

static DATA: LazyLock<Data> = LazyLock::new(|| {
    let mut rng = SplitMix64::new(0x6ed6_e5);
    let mut lists = Vec::new();
    let mut words = Vec::with_capacity(EDGES);
    let mut deltas = Vec::with_capacity(EDGES);
    let mut edges = Vec::new();
    let mut scratch = Vec::new();
    let mut next = 0u32;
    while words.len() < EDGES {
        let mut len = 1;
        while !rng.one_in(10) {
            len += 1;
        }
        let len = len.min(EDGES - words.len());
        let base = next;
        let mut node = base;
        let start = words.len();
        for _ in 0..len {
            node = if rng.one_in(10) {
                rng.below(NODES) as u32
            } else {
                (node + 1 + rng.below(8) as u32) % NODES as u32
            };
            words.push(node | u32::from(rng.one_in(20)) << 31);
        }
        next = (next + 1 + rng.below(4) as u32) % NODES as u32;
        varint::encode_edges(&words[start..], base, &mut scratch, &mut edges);
        deltas.extend_from_slice(&scratch);
        lists.push((base, len));
    }
    let mut raw = Vec::new();
    varint::encode(&deltas, &mut raw);
    Data { lists, edges, raw, words }
});

fn main() {
    let data = &*DATA;
    check(data);
    eprintln!(
        "graph_varint: {} edges in {} lists, {:.3} bytes an edge packed (count, control and data), {:.3} as one stream",
        data.words.len(),
        data.lists.len(),
        data.edges.len() as f64 / data.words.len() as f64,
        data.raw.len() as f64 / data.words.len() as f64,
    );
    divan::main();
}

/// The decoded words match what was encoded, on both paths, before anything is timed.
fn check(data: &Data) {
    let mut at = 0;
    let mut word = 0;
    let mut out = Vec::new();
    for &(base, len) in &data.lists {
        at += varint::decode_edges(&data.edges[at..], base, &mut out).unwrap();
        assert_eq!(out, data.words[word..word + len]);
        word += len;
    }
    assert_eq!(at, data.edges.len());

    let mut fast = vec![0; data.words.len()];
    let mut slow = vec![0; data.words.len()];
    assert_eq!(varint::decode(&data.raw, &mut fast), Ok(data.raw.len()));
    assert_eq!(varint::decode_scalar(&data.raw, &mut slow), Ok(data.raw.len()));
    assert_eq!(fast, slow);
}

/// Every list through `decode_edges`, as deep verify reads a memo's edges.
#[divan::bench]
fn decode_edges(bencher: divan::Bencher) {
    let data = &*DATA;
    let mut out = Vec::with_capacity(1024);
    bencher.counter(ItemsCount::new(data.words.len())).bench_local(|| {
        let mut at = 0;
        let mut sum = 0u32;
        for &(base, len) in &data.lists {
            at += varint::decode_edges(&data.edges[at..], base, &mut out).unwrap();
            debug_assert_eq!(out.len(), len);
            sum = sum.wrapping_add(out.last().copied().unwrap_or(0));
        }
        black_box(sum)
    });
}

/// The deltas as one stream through `decode`: the SIMD path on every group.
#[divan::bench]
fn decode(bencher: divan::Bencher) {
    let data = &*DATA;
    let mut out = vec![0; data.words.len()];
    bencher.counter(ItemsCount::new(data.words.len())).bench_local(|| {
        black_box(varint::decode(black_box(&data.raw), &mut out).unwrap());
        black_box(out[out.len() - 1])
    });
}

/// The same stream through the one-value-at-a-time reference.
#[divan::bench]
fn decode_scalar(bencher: divan::Bencher) {
    let data = &*DATA;
    let mut out = vec![0; data.words.len()];
    bencher.counter(ItemsCount::new(data.words.len())).bench_local(|| {
        black_box(varint::decode_scalar(black_box(&data.raw), &mut out).unwrap());
        black_box(out[out.len() - 1])
    });
}

/// Every list skipped by `encoded_len`, from its control bytes alone.
#[divan::bench]
fn skip_edges(bencher: divan::Bencher) {
    let data = &*DATA;
    bencher.counter(BytesCount::new(data.edges.len())).bench_local(|| {
        let mut at = 0;
        for &(_, len) in &data.lists {
            let head = if len < 128 { 1 } else if len < 1 << 14 { 2 } else { 3 };
            at += head + varint::encoded_len(&data.edges[at + head..], len).unwrap();
        }
        black_box(at)
    });
}

/// Every list through `encode_edges` into a reused buffer.
#[divan::bench]
fn encode_edges(bencher: divan::Bencher) {
    let data = &*DATA;
    let mut scratch = Vec::with_capacity(1024);
    let mut out = Vec::with_capacity(data.edges.len() + 64);
    bencher.counter(ItemsCount::new(data.words.len())).bench_local(|| {
        out.clear();
        let mut at = 0;
        for &(base, len) in &data.lists {
            varint::encode_edges(&data.words[at..at + len], base, &mut scratch, &mut out);
            at += len;
        }
        black_box(out.len())
    });
}
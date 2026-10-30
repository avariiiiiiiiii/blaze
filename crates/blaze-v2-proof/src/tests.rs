use std::collections::BTreeMap;

use blaze_ids::{ContentId, Fingerprint};
use blaze_query::{Edge, QueryKey, QueryKind};

use super::*;

fn key(n: u8) -> QueryKey {
    QueryKey { kind: QueryKind(0x2000), input: ContentId::of("blaze v2 proof test key 1", &[n]) }
}

fn edge(read: u8, saw: u128) -> Edge {
    Edge { read: key(read), saw: Fingerprint(saw) }
}

/// A file (1) read by a parse (2), read by a type check (3), read by codegen (4); and a
/// result (5) of another root, read by nothing here.
fn links(file: u128, parse: u128, check: u128, codegen: u128, ran: bool) -> TraceBuilder {
    let mut trace = TraceBuilder::default();
    trace.insert(key(1), Fingerprint(file), &[], ran);
    trace.insert(key(2), Fingerprint(parse), &[edge(1, file)], ran);
    trace.insert(key(3), Fingerprint(check), &[edge(2, parse)], ran);
    trace.insert(key(4), Fingerprint(codegen), &[edge(3, check)], ran);
    trace
}

fn chain(file: u128, parse: u128, check: u128, codegen: u128, ran: bool) -> Trace {
    let mut trace = links(file, parse, check, codegen, ran);
    trace.insert(key(5), Fingerprint(50), &[], ran);
    trace.finish()
}

#[test]
fn equal_traces_have_nothing_stale() {
    let scratch = chain(10, 20, 30, 40, true);
    let incremental = chain(10, 20, 30, 40, false);
    assert_eq!(under(&incremental, &scratch, &[]), vec![]);
}

#[test]
fn a_result_that_read_the_same_and_came_out_otherwise_is_the_culprit() {
    let scratch = chain(10, 20, 30, 40, true);
    let incremental = chain(10, 20, 31, 41, true);
    let stale = under(&incremental, &scratch, &[]);
    assert_eq!(
        stale,
        vec![
            Stale::UnrecordedRead { query: key(3), incremental: Fingerprint(31), scratch: Fingerprint(30) },
            Stale::Downstream { query: key(4), incremental: Fingerprint(41), scratch: Fingerprint(40) },
        ]
    );
    assert!(stale[0].is_culprit() && !stale[1].is_culprit());
}

#[test]
fn a_result_kept_though_what_it_read_changed_is_unverified() {
    let scratch = chain(11, 21, 30, 40, true);
    // The file and the parse reran; the type check was kept with its edge to the old parse.
    let mut incremental = links(10, 20, 30, 40, false);
    incremental.insert(key(1), Fingerprint(11), &[], true);
    incremental.insert(key(2), Fingerprint(21), &[edge(1, 11)], true);
    incremental.insert(key(5), Fingerprint(50), &[], false);
    let stale = under(&incremental.finish(), &scratch, &[]);
    assert_eq!(
        stale[0],
        Stale::Unverified { query: key(3), read: key(2), saw: Fingerprint(20), now: Some(Fingerprint(21)) }
    );
    assert!(stale[0].is_culprit());
}

#[test]
fn a_cut_off_edge_is_not_unverified() {
    // The file changed, the parse reran to the same fingerprint: keeping the check is right.
    let scratch = chain(11, 20, 30, 40, true);
    let mut incremental = links(10, 20, 30, 40, false);
    incremental.insert(key(1), Fingerprint(11), &[], true);
    incremental.insert(key(2), Fingerprint(20), &[edge(1, 11)], true);
    incremental.insert(key(5), Fingerprint(50), &[], false);
    assert_eq!(under(&incremental.finish(), &scratch, &[]), vec![]);
}

#[test]
fn a_result_missing_from_the_incremental_build_is_absent() {
    let scratch = chain(10, 20, 30, 40, true);
    let incremental = links(10, 20, 30, 40, false).finish();
    assert_eq!(under(&incremental, &scratch, &[]), vec![Stale::Absent { query: key(5) }]);
}

#[test]
fn only_what_the_roots_reach_is_compared() {
    let scratch = chain(10, 20, 30, 40, true);
    let mut incremental = links(10, 20, 30, 40, false);
    incremental.insert(key(5), Fingerprint(51), &[], true);
    let incremental = incremental.finish();
    assert_eq!(under(&incremental, &scratch, &[key(4)]), vec![]);
    assert_eq!(under(&incremental, &scratch, &[key(5)]).len(), 1);
    let reached = scratch.reachable(&[key(3)]);
    let names: Vec<QueryKey> =
        (0..scratch.keys.len() as u32).filter(|&i| reached.get(i)).map(|i| scratch.keys[i as usize]).collect();
    let mut want = vec![key(1), key(2), key(3)];
    want.sort();
    assert_eq!(names, want);
}

#[test]
fn over_counts_reruns_for_nothing_cutoffs_and_new_results() {
    let before = chain(10, 20, 30, 40, true);
    let mut after = links(11, 20, 30, 40, false);
    after.insert(key(5), Fingerprint(50), &[], false);
    after.insert(key(1), Fingerprint(11), &[], true);
    // The parse reran and came out the same: cut off.
    after.insert(key(2), Fingerprint(20), &[edge(1, 11)], true);
    // The check reran though its edge is what it was: for nothing.
    after.insert(key(3), Fingerprint(30), &[edge(2, 20)], true);
    after.insert(key(6), Fingerprint(60), &[], true);
    let (work, needless) = over(&before, &after.finish());
    assert_eq!(work, Work { ran: 4, needless: 1, cut_off: 1, new: 1 });
    assert_eq!(needless, vec![key(3)]);
}

#[test]
fn apart_names_every_key_that_differs_in_order() {
    let first: BTreeMap<u32, &str> = [(1, "a"), (2, "b"), (4, "d")].into_iter().collect();
    let second: BTreeMap<u32, &str> = [(2, "B"), (3, "c"), (4, "d")].into_iter().collect();
    assert_eq!(
        apart(&first, &second),
        vec![
            Apart::OnlyFirst { key: 1, value: "a" },
            Apart::Differ { key: 2, first: "b", second: "B" },
            Apart::OnlySecond { key: 3, value: "c" },
        ]
    );
    assert_eq!(apart(&first, &first), vec![]);
}

#[test]
fn rng_is_the_same_from_one_seed_and_stays_below() {
    let a: Vec<u64> = (0..8).scan(Rng::new(7), |r, _| Some(r.next_u64())).collect();
    let b: Vec<u64> = (0..8).scan(Rng::new(7), |r, _| Some(r.next_u64())).collect();
    assert_eq!(a, b);
    assert_ne!(a, (0..8).scan(Rng::new(8), |r, _| Some(r.next_u64())).collect::<Vec<_>>());
    // SplitMix64's first output from 0, as its reference implementation gives it.
    assert_eq!(Rng::new(0).next_u64(), 0xe220_a839_7b1d_cdaf);
    let mut rng = Rng::new(1);
    assert!((0..1000).all(|_| rng.below(3) < 3));
}

#[test]
fn sample_chooses_distinct_items_in_order() {
    let items: Vec<u32> = (0..100).collect();
    let chosen = sample(&items, 10, 42);
    assert_eq!(chosen.len(), 10);
    assert!(chosen.windows(2).all(|w| w[0] < w[1]));
    assert_eq!(chosen, sample(&items, 10, 42));
    assert_eq!(sample(&items[..5], 10, 42), items[..5].to_vec());
}

#[test]
fn a_trace_round_trips_in_deterministic_cbor() {
    let trace = chain(10, 20, 30, 40, true);
    let bytes = blaze_protocol::cbor::to_vec(&trace).unwrap();
    let back: Trace = blaze_protocol::cbor::from_slice(&bytes).unwrap();
    assert_eq!(back, trace);
}

#[test]
fn every_simd_path_finds_the_first_difference_the_scalar_one_does() {
    let mut rng = Rng::new(1606);
    let paths = bytes::available();
    assert_eq!(paths.last(), Some(&bytes::Simd::Scalar));
    for len in (0..300).chain([1 << 12, (1 << 16) + 7]) {
        let a: Vec<u8> = (0..len).map(|_| rng.next_u64() as u8).collect();
        let mut cases = vec![(a.clone(), None)];
        for _ in 0..4 {
            if len == 0 {
                break;
            }
            let at = rng.below(len as u64) as usize;
            let mut b = a.clone();
            b[at] ^= 1 << rng.below(8);
            // A second difference after the first must not hide it.
            if at + 1 < len {
                b[len - 1] ^= 0x80;
            }
            cases.push((b, Some(at)));
        }
        let mut shorter = a.clone();
        shorter.truncate(len / 2);
        cases.push((shorter, if len / 2 < len { Some(len / 2) } else { None }));
        for (b, want) in &cases {
            for &path in &paths {
                assert_eq!(bytes::first_difference_with(path, &a, b), *want, "{path:?}, length {len}");
                assert_eq!(bytes::first_difference_with(path, b, &a), *want, "{path:?}, length {len}, swapped");
            }
        }
    }
}

#[test]
fn files_are_compared_across_buffer_boundaries() {
    let dir = std::env::temp_dir().join(format!("blaze-v2-proof-bytes-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (a, b) = (dir.join("a"), dir.join("b"));
    let body: Vec<u8> = (0..(3 << 20) + 5).map(|i: u32| (i % 251) as u8).collect();
    std::fs::write(&a, &body).unwrap();
    std::fs::write(&b, &body).unwrap();
    assert_eq!(bytes::first_file_difference(&a, &b).unwrap(), None);
    let mut other = body.clone();
    other[(2 << 20) + 3] ^= 1;
    std::fs::write(&b, &other).unwrap();
    assert_eq!(bytes::first_file_difference(&a, &b).unwrap(), Some((2 << 20) + 3));
    std::fs::write(&b, &body[..1 << 20]).unwrap();
    assert_eq!(bytes::first_file_difference(&a, &b).unwrap(), Some(1 << 20));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn the_builder_keeps_the_last_insert_and_names_every_read() {
    let mut trace = TraceBuilder::default();
    trace.insert(key(2), Fingerprint(20), &[edge(1, 10)], true);
    trace.insert(key(2), Fingerprint(21), &[edge(1, 11), edge(3, 30)], false);
    let trace = trace.finish();
    assert_eq!(trace.keys.len(), 3);
    assert_eq!(trace.results(), 1);
    let two = trace.index(&key(2)).unwrap();
    assert_eq!(trace.result(two), Some(Fingerprint(21)));
    assert!(!trace.ran.get(two));
    let read: Vec<(QueryKey, Fingerprint)> = trace.edges(two).map(|(r, s)| (trace.keys[r as usize], s)).collect();
    assert_eq!(read, vec![(key(1), Fingerprint(11)), (key(3), Fingerprint(30))]);
    assert_eq!(trace.result(trace.index(&key(1)).unwrap()), None);
    assert_eq!(trace.edges_from.len(), trace.keys.len() + 1);
}

#[test]
fn bits_cross_word_boundaries() {
    let mut bits = Bits::with_len(130);
    for i in [0, 63, 64, 129] {
        bits.set(i);
    }
    assert_eq!(bits.0.len(), 3);
    assert!([0, 63, 64, 129].iter().all(|&i| bits.get(i)));
    assert!(!bits.get(1) && !bits.get(65) && !bits.get(128));
    assert_eq!(bits.count(), 4);
}

fn le_bytes(column: &[u32]) -> Vec<u8> {
    column.iter().flat_map(|v| v.to_le_bytes()).collect()
}

#[test]
fn an_exact_encoding_round_trips_every_shape() {
    let decode = |bytes: &Vec<u8>| -> Result<Vec<u32>, String> {
        if bytes.len() % 4 != 0 {
            return Err(format!("{} bytes is no whole number of u32s", bytes.len()));
        }
        Ok(bytes.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect())
    };
    let checked = columns::round_trips(le_bytes, decode, 17).unwrap();
    assert_eq!(checked, columns::shapes(17).len());
    assert_eq!(columns::shapes(17), columns::shapes(17));
}

#[test]
fn a_lossy_encoding_is_named_with_where_it_lost() {
    let encode = |column: &[u32]| -> Vec<u16> { column.iter().map(|&v| v as u16).collect() };
    let decode = |packed: &Vec<u16>| -> Result<Vec<u32>, String> { Ok(packed.iter().map(|&v| u32::from(v)).collect()) };
    let wrong = columns::round_trips(encode, decode, 17).unwrap_err();
    assert!(wrong.starts_with("max of 1:"), "{wrong}");
    let shapes = columns::shapes(3);
    assert!(shapes.iter().any(|(name, c)| *name == "CSR offsets" && c.windows(2).all(|w| w[0] <= w[1])));
    assert!(shapes.iter().any(|(name, c)| *name == "strictly increasing" && c.windows(2).all(|w| w[0] < w[1])));
}

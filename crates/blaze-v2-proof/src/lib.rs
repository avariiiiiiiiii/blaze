//! The invalidation oracle on the query database's own record (docs/v2/proof.md).
//!
//! `harness/v2/oracle.py` checks from outside that an incremental build equals a build from
//! scratch of the same sources: the programs, the tests, the bytes, the codegen inputs. This
//! crate says the same of the [`Trace`] each build leaves, every result's fingerprint and edges,
//! so a difference names the query that went stale and why, not only the program that came out
//! wrong:
//!
//! - [`under`]: an incremental trace against one from scratch of the same view. A result that
//!   differs is [`Stale`]; the culprits are a result kept though what it read changed
//!   ([`Stale::Unverified`], verification missed it) and a result that read the same as the
//!   build from scratch and still came out otherwise ([`Stale::UnrecordedRead`]: it read
//!   something it has no edge for, a fact missing from docs/v2/items.md section 6.2's table).
//!   Under-invalidation is a failure.
//! - [`over`]: an incremental build's reruns against the build before it. A rerun that read
//!   what it read before and came out as before is work for nothing; it is measured and
//!   budgeted.
//! - [`apart`]: where two maps differ, for the codegen inputs (each `InstanceKey` with its
//!   `CodeHash`) and the output files.
//! - [`Rng`] and [`sample`]: the seeded choice of what to check when not everything is.
//! - [`bytes`]: where two outputs first differ, by SIMD chosen at run time.
//! - [`columns`]: the exact round trip every column encoding must make.
//!
//! A trace of nushell has millions of results and tens of millions of edges, so it is columns
//! over `u32` indices (docs/v2/plan/data.md): keys sorted once, flags as bitsets, edges
//! as one CSR array.

pub mod bytes;
pub mod columns;

use std::collections::BTreeMap;

use blaze_ids::Fingerprint;
use blaze_query::{Edge, Memo, QueryKey};
use serde::{Deserialize, Serialize};

/// A bit per index, 64 to a word.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
pub struct Bits(pub Vec<u64>);

impl Bits {
    pub fn with_len(len: usize) -> Bits {
        Bits(vec![0; len.div_ceil(64)])
    }

    pub fn get(&self, i: u32) -> bool {
        (self.0[i as usize / 64] >> (i % 64)) & 1 == 1
    }

    pub fn set(&mut self, i: u32) {
        self.0[i as usize / 64] |= 1 << (i % 64);
    }

    pub fn count(&self) -> usize {
        self.0.iter().map(|w| w.count_ones() as usize).sum()
    }
}

/// What one build left in the database: every key it names (its results and what they read),
/// sorted, and per key index whether there is a result, its fingerprint, whether this build
/// ran it, and its edges in read order.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
pub struct Trace {
    pub keys: Vec<QueryKey>,
    pub has: Bits,
    /// Executed by this build, rather than kept from an earlier one.
    pub ran: Bits,
    /// `Fingerprint(0)` where there is no result.
    pub fingerprints: Vec<Fingerprint>,
    /// Key `i`'s edges are `edges_from[i]..edges_from[i + 1]` of `reads` and `saw`.
    pub edges_from: Vec<u32>,
    /// The key index each edge read.
    pub reads: Vec<u32>,
    /// Its fingerprint when it was read.
    pub saw: Vec<Fingerprint>,
}

/// An edge costs its two columns and nothing more.
const _: () = assert!(size_of::<u32>() + size_of::<Fingerprint>() == 20);

/// The results of a build as they are found, in any order, made a [`Trace`] by
/// [`TraceBuilder::finish`]. A key inserted twice keeps the last.
#[derive(Default)]
pub struct TraceBuilder {
    results: BTreeMap<QueryKey, (Fingerprint, bool, u32, u32)>,
    edges: Vec<Edge>,
}

impl TraceBuilder {
    pub fn insert(&mut self, key: QueryKey, fingerprint: Fingerprint, edges: &[Edge], ran: bool) {
        let from = u32::try_from(self.edges.len()).expect("a trace has fewer than 2^32 edges");
        self.edges.extend_from_slice(edges);
        let len = u32::try_from(edges.len()).expect("a result has fewer than 2^32 edges");
        self.results.insert(key, (fingerprint, ran, from, len));
    }

    pub fn record(&mut self, memo: &Memo, ran: bool) {
        self.insert(memo.key, memo.fingerprint, &memo.edges, ran);
    }

    pub fn finish(self) -> Trace {
        let mut keys: Vec<QueryKey> = self.results.keys().copied().collect();
        for &(_, _, from, len) in self.results.values() {
            keys.extend(self.edges[from as usize..(from + len) as usize].iter().map(|e| e.read));
        }
        keys.sort_unstable();
        keys.dedup();
        let n = keys.len();
        let mut trace = Trace {
            has: Bits::with_len(n),
            ran: Bits::with_len(n),
            fingerprints: vec![Fingerprint(0); n],
            edges_from: Vec::with_capacity(n + 1),
            ..Trace::default()
        };
        let index = |key: &QueryKey| keys.binary_search(key).expect("every read is a key") as u32;
        trace.edges_from.push(0);
        for (i, key) in keys.iter().enumerate() {
            if let Some(&(fingerprint, ran, from, len)) = self.results.get(key) {
                let i = i as u32;
                trace.has.set(i);
                if ran {
                    trace.ran.set(i);
                }
                trace.fingerprints[i as usize] = fingerprint;
                for edge in &self.edges[from as usize..(from + len) as usize] {
                    trace.reads.push(index(&edge.read));
                    trace.saw.push(edge.saw);
                }
            }
            trace.edges_from.push(u32::try_from(trace.reads.len()).expect("a trace has fewer than 2^32 edges"));
        }
        trace.keys = keys;
        trace
    }
}

impl Trace {
    pub fn index(&self, key: &QueryKey) -> Option<u32> {
        self.keys.binary_search(key).ok().map(|i| i as u32)
    }

    /// Key `i`'s fingerprint, `None` when the build has no result for it.
    pub fn result(&self, i: u32) -> Option<Fingerprint> {
        self.has.get(i).then(|| self.fingerprints[i as usize])
    }

    pub fn results(&self) -> usize {
        self.has.count()
    }

    /// Key `i`'s edges as (key index read, fingerprint seen).
    pub fn edges(&self, i: u32) -> impl Iterator<Item = (u32, Fingerprint)> + '_ {
        let range = self.edges_from[i as usize] as usize..self.edges_from[i as usize + 1] as usize;
        self.reads[range.clone()].iter().copied().zip(self.saw[range].iter().copied())
    }

    /// The key indices reachable from `roots` through edges, the roots included, as a bitset;
    /// every result when `roots` is empty. A root the trace does not name is not reachable.
    pub fn reachable(&self, roots: &[QueryKey]) -> Bits {
        if roots.is_empty() {
            return self.has.clone();
        }
        let mut seen = Bits::with_len(self.keys.len());
        let mut next: Vec<u32> = roots.iter().filter_map(|k| self.index(k)).collect();
        while let Some(i) = next.pop() {
            if seen.get(i) {
                continue;
            }
            seen.set(i);
            next.extend(self.edges(i).map(|(read, _)| read));
        }
        seen
    }
}

/// Whether key `i` of `a` and key `j` of `b` read the same keys with the same fingerprints, in
/// the same order.
fn same_edges(a: &Trace, i: u32, b: &Trace, j: u32) -> bool {
    let (mut x, mut y) = (a.edges(i), b.edges(j));
    loop {
        match (x.next(), y.next()) {
            (None, None) => return true,
            (Some((ra, sa)), Some((rb, sb))) if sa == sb && a.keys[ra as usize] == b.keys[rb as usize] => {}
            _ => return false,
        }
    }
}

/// A result of the incremental build that is not the one the build from scratch made.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum Stale {
    /// Kept from an earlier build though an edge's fingerprint is not that of what it read now
    /// (`now`, `None` when the build has no result for it): verification missed a change.
    Unverified { query: QueryKey, read: QueryKey, saw: Fingerprint, now: Option<Fingerprint> },
    /// Read the same things with the same fingerprints as the build from scratch, and came out
    /// otherwise: the query reads something it records no edge for, or is not a function of
    /// what it reads.
    UnrecordedRead { query: QueryKey, incremental: Fingerprint, scratch: Fingerprint },
    /// The build from scratch has a result the incremental build has none of.
    Absent { query: QueryKey },
    /// Read other things than the build from scratch did and came out otherwise: what it read
    /// is stale, so a culprit is below it.
    Downstream { query: QueryKey, incremental: Fingerprint, scratch: Fingerprint },
}

impl Stale {
    pub fn query(&self) -> QueryKey {
        match self {
            Stale::Unverified { query, .. }
            | Stale::UnrecordedRead { query, .. }
            | Stale::Absent { query }
            | Stale::Downstream { query, .. } => *query,
        }
    }

    /// A difference that starts here, rather than one passed on from below.
    pub fn is_culprit(&self) -> bool {
        matches!(self, Stale::Unverified { .. } | Stale::UnrecordedRead { .. })
    }

    fn rank(&self) -> u8 {
        match self {
            Stale::Unverified { .. } => 0,
            Stale::UnrecordedRead { .. } => 1,
            Stale::Absent { .. } => 2,
            Stale::Downstream { .. } => 3,
        }
    }
}

/// Where the incremental trace is not the one from scratch, over the results the build from
/// scratch reached from `roots` (every result when empty): the culprits first, then by key.
pub fn under(incremental: &Trace, scratch: &Trace, roots: &[QueryKey]) -> Vec<Stale> {
    let reached = scratch.reachable(roots);
    let mut stale = Vec::new();
    for i in 0..scratch.keys.len() as u32 {
        let Some(theirs) = scratch.result(i).filter(|_| reached.get(i)) else { continue };
        let query = scratch.keys[i as usize];
        let Some((j, ours)) = incremental.index(&query).and_then(|j| Some((j, incremental.result(j)?))) else {
            stale.push(Stale::Absent { query });
            continue;
        };
        if !incremental.ran.get(j) {
            if let Some((read, saw)) = incremental.edges(j).find(|&(read, saw)| incremental.result(read) != Some(saw)) {
                let now = incremental.result(read);
                stale.push(Stale::Unverified { query, read: incremental.keys[read as usize], saw, now });
                continue;
            }
        }
        if ours == theirs {
            continue;
        }
        stale.push(if same_edges(incremental, j, scratch, i) {
            Stale::UnrecordedRead { query, incremental: ours, scratch: theirs }
        } else {
            Stale::Downstream { query, incremental: ours, scratch: theirs }
        });
    }
    stale.sort_by_key(|s| (s.rank(), s.query()));
    stale
}

/// What an incremental build executed, against the build before it.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Work {
    /// Results the build executed.
    pub ran: usize,
    /// Of those, ones whose edges and result are what they were before: work for nothing.
    pub needless: usize,
    /// Ones whose edges changed and whose result did not: early cutoff stopped there.
    pub cut_off: usize,
    /// Ones the build before had no result for.
    pub new: usize,
}

/// What `after` ran, against `before`, and the needless reruns by key.
pub fn over(before: &Trace, after: &Trace) -> (Work, Vec<QueryKey>) {
    let mut work = Work::default();
    let mut needless = Vec::new();
    for i in 0..after.keys.len() as u32 {
        let Some(now) = after.result(i).filter(|_| after.ran.get(i)) else { continue };
        work.ran += 1;
        let key = after.keys[i as usize];
        match before.index(&key).and_then(|j| Some((j, before.result(j)?))) {
            None => work.new += 1,
            // Same edges and another result is an input that changed (a file read), not waste.
            Some((j, was)) if was == now && same_edges(before, j, after, i) => {
                work.needless += 1;
                needless.push(key);
            }
            Some((_, was)) if was == now => work.cut_off += 1,
            Some(_) => {}
        }
    }
    (work, needless)
}

/// A key where two maps differ.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum Apart<K, V> {
    OnlyFirst { key: K, value: V },
    OnlySecond { key: K, value: V },
    Differ { key: K, first: V, second: V },
}

/// Every key where `first` and `second` differ, in key order.
pub fn apart<K: Ord + Clone, V: PartialEq + Clone>(
    first: &BTreeMap<K, V>,
    second: &BTreeMap<K, V>,
) -> Vec<Apart<K, V>> {
    let mut out = Vec::new();
    for (key, value) in first {
        match second.get(key) {
            None => out.push(Apart::OnlyFirst { key: key.clone(), value: value.clone() }),
            Some(other) if other != value => out.push(Apart::Differ {
                key: key.clone(),
                first: value.clone(),
                second: other.clone(),
            }),
            Some(_) => {}
        }
    }
    for (key, value) in second {
        if !first.contains_key(key) {
            out.push(Apart::OnlySecond { key: key.clone(), value: value.clone() });
        }
    }
    out.sort_by(|a, b| key_of(a).cmp(key_of(b)));
    out
}

fn key_of<K, V>(apart: &Apart<K, V>) -> &K {
    match apart {
        Apart::OnlyFirst { key, .. } | Apart::OnlySecond { key, .. } | Apart::Differ { key, .. } => key,
    }
}

/// SplitMix64: the same numbers from the same seed on every machine.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A number in `0..n`; `n` is at least 1.
    pub fn below(&mut self, n: u64) -> u64 {
        assert!(n > 0, "Rng::below(0)");
        ((u128::from(self.next_u64()) * u128::from(n)) >> 64) as u64
    }
}

/// `n` of `items` chosen by `seed` (all of them when there are no more), in their order.
pub fn sample<T: Clone>(items: &[T], n: usize, seed: u64) -> Vec<T> {
    if n >= items.len() {
        return items.to_vec();
    }
    let mut rng = Rng::new(seed);
    let mut at: Vec<usize> = (0..items.len()).collect();
    for i in 0..n {
        let j = i + rng.below((items.len() - i) as u64) as usize;
        at.swap(i, j);
    }
    let mut chosen = at[..n].to_vec();
    chosen.sort_unstable();
    chosen.into_iter().map(|i| items[i].clone()).collect()
}

#[cfg(test)]
mod tests;

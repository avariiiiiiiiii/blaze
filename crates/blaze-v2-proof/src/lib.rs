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
//! - [`over`]: an incremental build's reruns against the build before it. A rerun whose edges
//!   are the ones it had is work for nothing; it is measured and budgeted.
//! - [`apart`]: where two maps differ, for the codegen inputs (each `InstanceKey` with its
//!   `CodeHash`) and the output files.
//! - [`Rng`] and [`sample`]: the seeded choice of what to check when not everything is.
//! - [`bytes`]: where two outputs first differ, by SIMD chosen at run time.

pub mod bytes;

use std::collections::{BTreeMap, BTreeSet};

use blaze_ids::Fingerprint;
use blaze_query::{Edge, Memo, QueryKey};
use serde::{Deserialize, Serialize};

/// One result as a build left it.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Recorded {
    pub fingerprint: Fingerprint,
    /// In the order they were read.
    pub edges: Vec<Edge>,
    /// Executed by this build, rather than kept from an earlier one.
    pub ran: bool,
}

/// What one build left in the database: every result it ran or kept, by key.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
pub struct Trace {
    pub results: BTreeMap<QueryKey, Recorded>,
}

impl Trace {
    pub fn insert(&mut self, key: QueryKey, fingerprint: Fingerprint, edges: Vec<Edge>, ran: bool) {
        self.results.insert(key, Recorded { fingerprint, edges, ran });
    }

    pub fn record(&mut self, memo: &Memo, ran: bool) {
        self.insert(memo.key, memo.fingerprint, memo.edges.clone(), ran);
    }

    /// The results reachable from `roots` through edges, the roots included; every result when
    /// `roots` is empty. A key the trace has no result for is reachable but has no edges.
    pub fn reachable(&self, roots: &[QueryKey]) -> BTreeSet<QueryKey> {
        if roots.is_empty() {
            return self.results.keys().copied().collect();
        }
        let mut seen = BTreeSet::new();
        let mut next: Vec<QueryKey> = roots.to_vec();
        while let Some(key) = next.pop() {
            if !seen.insert(key) {
                continue;
            }
            if let Some(result) = self.results.get(&key) {
                next.extend(result.edges.iter().map(|edge| edge.read));
            }
        }
        seen
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

/// Where the incremental trace is not the one from scratch, over what the build from scratch
/// reached from `roots` (everything when empty): the culprits first, then by key.
pub fn under(incremental: &Trace, scratch: &Trace, roots: &[QueryKey]) -> Vec<Stale> {
    let mut stale = Vec::new();
    for key in scratch.reachable(roots) {
        let Some(theirs) = scratch.results.get(&key) else { continue };
        let Some(ours) = incremental.results.get(&key) else {
            stale.push(Stale::Absent { query: key });
            continue;
        };
        if !ours.ran {
            let now = |read: &QueryKey| incremental.results.get(read).map(|r| r.fingerprint);
            if let Some(edge) = ours.edges.iter().find(|edge| now(&edge.read) != Some(edge.saw)) {
                stale.push(Stale::Unverified {
                    query: key,
                    read: edge.read,
                    saw: edge.saw,
                    now: now(&edge.read),
                });
                continue;
            }
        }
        if ours.fingerprint == theirs.fingerprint {
            continue;
        }
        let (incremental, scratch) = (ours.fingerprint, theirs.fingerprint);
        stale.push(if ours.edges == theirs.edges {
            Stale::UnrecordedRead { query: key, incremental, scratch }
        } else {
            Stale::Downstream { query: key, incremental, scratch }
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
    /// Of those, ones whose edges are what they were before: work for nothing.
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
    for (key, result) in after.results.iter().filter(|(_, r)| r.ran) {
        work.ran += 1;
        match before.results.get(key) {
            None => work.new += 1,
            Some(was) if was.edges == result.edges => {
                work.needless += 1;
                needless.push(*key);
            }
            Some(was) if was.fingerprint == result.fingerprint => work.cut_off += 1,
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

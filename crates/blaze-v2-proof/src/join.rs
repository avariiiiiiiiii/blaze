//! [`under`] and [`over`] across cores, on owned [`Trace`]s or on [`View`]s of mapped files read in
//! place. The keys of one trace are split into one range per core; each range finds where it
//! starts in the other trace with one gallop and walks both in order from there, so a range costs
//! its keys' bytes streamed plus one probe per edge, and the ranges share nothing. Keys are
//! content hashes, so equal key ranges carry about equal edges. The pieces are joined in key
//! order and sorted as the serial join sorts, so the answer is the same bytes whatever the core
//! count: `crate::under` and `crate::over` are the reference the tests hold these to.
//!
//! [`View`]: crate::wire::View

use std::{ops::Range, thread};

use blaze_ids::Fingerprint;
use blaze_query::QueryKey;

use crate::{Bits, Stale, Trace, Work};

/// A trace's columns, read by index: implemented by [`Trace`] and by [`crate::wire::View`], and
/// monomorphized for each, so nothing here is dispatched at run time.
pub trait Columns: Sync {
    fn keys(&self) -> usize;
    fn key(&self, i: usize) -> QueryKey;
    fn has(&self, i: usize) -> bool;
    fn ran(&self, i: usize) -> bool;
    fn fingerprint(&self, i: usize) -> Fingerprint;
    /// Key `i`'s edges, as indices into [`Columns::read`] and [`Columns::saw`].
    fn edges_of(&self, i: usize) -> Range<usize>;
    fn read(&self, e: usize) -> u32;
    fn saw(&self, e: usize) -> Fingerprint;
}

impl Columns for Trace {
    fn keys(&self) -> usize {
        self.keys.len()
    }

    fn key(&self, i: usize) -> QueryKey {
        self.keys[i]
    }

    fn has(&self, i: usize) -> bool {
        self.has.get(i as u32)
    }

    fn ran(&self, i: usize) -> bool {
        self.ran.get(i as u32)
    }

    fn fingerprint(&self, i: usize) -> Fingerprint {
        self.fingerprints[i]
    }

    fn edges_of(&self, i: usize) -> Range<usize> {
        self.edges_from[i] as usize..self.edges_from[i + 1] as usize
    }

    fn read(&self, e: usize) -> u32 {
        self.reads[e]
    }

    fn saw(&self, e: usize) -> Fingerprint {
        self.saw[e]
    }
}

/// Below this many elements a piece is not worth a thread (its spawn, about 10 µs, is the time of
/// that many keys joined).
const LEAST_PIECE: usize = 1 << 14;

/// How many pieces to split `len` elements into: one per core, none smaller than [`LEAST_PIECE`].
pub(crate) fn pieces(len: usize) -> usize {
    let cores = thread::available_parallelism().map_or(1, |n| n.get());
    (len / LEAST_PIECE).clamp(1, cores)
}

/// `work` over `0..len` cut into `pieces` consecutive ranges, the first on this thread and the
/// rest each on a scoped thread of its own, the answers in range order.
pub(crate) fn split<T: Send>(len: usize, pieces: usize, work: impl Fn(Range<usize>) -> T + Sync) -> Vec<T> {
    let pieces = pieces.clamp(1, len.max(1));
    let range = |p: usize| p * len / pieces..(p + 1) * len / pieces;
    if pieces == 1 {
        return vec![work(0..len)];
    }
    thread::scope(|scope| {
        let work = &work;
        let others: Vec<_> = (1..pieces).map(|p| scope.spawn(move || work(range(p)))).collect();
        let mut out = Vec::with_capacity(pieces);
        out.push(work(range(0)));
        out.extend(others.into_iter().map(|h| h.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic))));
        out
    })
}

fn result<C: Columns>(c: &C, i: usize) -> Option<Fingerprint> {
    c.has(i).then(|| c.fingerprint(i))
}

/// The first index at or after `from` whose key is not below `key`: `crate::seek` on columns.
fn seek<C: Columns>(c: &C, from: usize, key: &QueryKey) -> usize {
    let n = c.keys();
    let (mut lo, mut step) = (from, 1);
    while lo + step <= n && c.key(lo + step - 1) < *key {
        lo += step;
        step *= 2;
    }
    let mut hi = (lo + step).min(n);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if c.key(mid) < *key {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

fn index_from<C: Columns>(c: &C, cursor: &mut usize, key: &QueryKey) -> Option<usize> {
    *cursor = seek(c, *cursor, key);
    (*cursor < c.keys() && c.key(*cursor) == *key).then_some(*cursor)
}

/// Where the piece starting at key `start` of `of` begins in `other`.
fn start_in<A: Columns, B: Columns>(other: &A, of: &B, start: usize) -> usize {
    if start == 0 { 0 } else { seek(other, 0, &of.key(start)) }
}

fn same_edges<A: Columns, B: Columns>(a: &A, i: usize, b: &B, j: usize) -> bool {
    let (x, y) = (a.edges_of(i), b.edges_of(j));
    x.len() == y.len()
        && x.zip(y).all(|(ea, eb)| a.saw(ea) == b.saw(eb) && a.key(a.read(ea) as usize) == b.key(b.read(eb) as usize))
}

/// The keys reachable from `roots`, the roots included; `None`, every result, when there are no
/// roots.
fn reachable<C: Columns>(c: &C, roots: &[QueryKey]) -> Option<Bits> {
    if roots.is_empty() {
        return None;
    }
    let mut seen = Bits::with_len(c.keys());
    let mut next: Vec<usize> = roots
        .iter()
        .filter_map(|k| {
            let at = seek(c, 0, k);
            (at < c.keys() && c.key(at) == *k).then_some(at)
        })
        .collect();
    while let Some(i) = next.pop() {
        if seen.get(i as u32) {
            continue;
        }
        seen.set(i as u32);
        next.extend(c.edges_of(i).map(|e| c.read(e) as usize));
    }
    Some(seen)
}

/// [`crate::under`] across cores.
pub fn under<A: Columns, B: Columns>(incremental: &A, scratch: &B, roots: &[QueryKey]) -> Vec<Stale> {
    under_in(incremental, scratch, roots, pieces(scratch.keys()))
}

fn under_in<A: Columns, B: Columns>(incremental: &A, scratch: &B, roots: &[QueryKey], pieces: usize) -> Vec<Stale> {
    let reached = reachable(scratch, roots);
    let found = split(scratch.keys(), pieces, |range| {
        let mut stale = Vec::new();
        let mut cursor = start_in(incremental, scratch, range.start);
        for i in range {
            let reaches = reached.as_ref().is_none_or(|r| r.get(i as u32));
            let Some(theirs) = result(scratch, i).filter(|_| reaches) else { continue };
            let query = scratch.key(i);
            let found = index_from(incremental, &mut cursor, &query);
            let Some((j, ours)) = found.and_then(|j| Some((j, result(incremental, j)?))) else {
                stale.push(Stale::Absent { query });
                continue;
            };
            if !incremental.ran(j) {
                let missed =
                    incremental.edges_of(j).find(|&e| result(incremental, incremental.read(e) as usize) != Some(incremental.saw(e)));
                if let Some(e) = missed {
                    let read = incremental.read(e) as usize;
                    let (saw, now) = (incremental.saw(e), result(incremental, read));
                    stale.push(Stale::Unverified { query, read: incremental.key(read), saw, now });
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
        stale
    });
    let mut stale: Vec<Stale> = found.into_iter().flatten().collect();
    stale.sort_by_key(|s| (s.rank(), s.query()));
    stale
}

/// [`crate::over`] across cores.
pub fn over<A: Columns, B: Columns>(before: &A, after: &B) -> (Work, Vec<QueryKey>) {
    over_in(before, after, pieces(after.keys()))
}

fn over_in<A: Columns, B: Columns>(before: &A, after: &B, pieces: usize) -> (Work, Vec<QueryKey>) {
    let found = split(after.keys(), pieces, |range| {
        let mut work = Work::default();
        let mut needless = Vec::new();
        let mut cursor = start_in(before, after, range.start);
        for i in range {
            let Some(now) = result(after, i).filter(|_| after.ran(i)) else { continue };
            work.ran += 1;
            let key = after.key(i);
            match index_from(before, &mut cursor, &key).and_then(|j| Some((j, result(before, j)?))) {
                None => work.new += 1,
                Some((j, was)) if was == now && same_edges(before, j, after, i) => {
                    work.needless += 1;
                    needless.push(key);
                }
                Some((_, was)) if was == now => work.cut_off += 1,
                Some(_) => {}
            }
        }
        (work, needless)
    });
    let mut work = Work::default();
    let mut needless = Vec::new();
    for (piece, keys) in found {
        work.ran += piece.ran;
        work.needless += piece.needless;
        work.cut_off += piece.cut_off;
        work.new += piece.new;
        needless.extend(keys);
    }
    (work, needless)
}

#[cfg(test)]
mod tests {
    use blaze_ids::Fingerprint;
    use blaze_query::QueryKey;

    use super::{over_in, under_in};
    use crate::{Trace, wire::View, wire::tests::seeded};

    /// A trace of the same keys as `base` with some results changed, kept or dropped by `seed`,
    /// so `under` and `over` against `base` find every kind of answer.
    fn edited(base: &Trace, seed: u64) -> Trace {
        let mut rng = crate::Rng::new(seed);
        let mut trace = base.clone();
        for i in 0..trace.keys.len() as u32 {
            match rng.below(8) {
                0 => trace.fingerprints[i as usize] = Fingerprint(trace.fingerprints[i as usize].0 + 1),
                1 => trace.ran.0[i as usize / 64] ^= 1 << (i % 64),
                2 if trace.has.get(i) => trace.has.0[i as usize / 64] &= !(1 << (i % 64)),
                _ => {}
            }
        }
        for saw in trace.saw.iter_mut() {
            if rng.below(10) == 0 {
                *saw = Fingerprint(saw.0 + 1);
            }
        }
        trace
    }

    #[test]
    fn under_and_over_across_pieces_answer_as_the_serial_join() {
        for seed in 0..12 {
            let scratch = seeded(seed, 40 + seed * 31, 60 + seed as usize * 50);
            let incremental = edited(&scratch, seed + 100);
            let other = seeded(seed + 1000, 40 + seed * 31, 60 + seed as usize * 50);
            let bytes = [&incremental, &scratch, &other].map(|t| blaze_protocol::cbor::to_vec(t).unwrap());
            let [inc_view, scratch_view, other_view] = [0, 1, 2].map(|k| View::parse(&bytes[k]).unwrap());
            let roots: Vec<QueryKey> = scratch.keys.iter().step_by(5).copied().collect();
            for roots in [&[][..], &roots[..2.min(roots.len())]] {
                for (inc, scr) in [(&incremental, &scratch), (&other, &scratch), (&scratch, &other)] {
                    let want = crate::under(inc, scr, roots);
                    for pieces in 1..=7 {
                        assert_eq!(under_in(inc, scr, roots, pieces), want, "seed {seed}, {pieces} pieces");
                    }
                }
                let want = crate::under(&incremental, &scratch, roots);
                for pieces in 1..=7 {
                    assert_eq!(under_in(&inc_view, &scratch_view, roots, pieces), want, "seed {seed} in place");
                }
            }
            for (before, after) in [(&scratch, &incremental), (&other, &incremental), (&incremental, &scratch)] {
                let want = crate::over(before, after);
                for pieces in 1..=7 {
                    assert_eq!(over_in(before, after, pieces), want, "seed {seed}, {pieces} pieces");
                }
            }
            let want = crate::over(&other, &incremental);
            for pieces in 1..=7 {
                assert_eq!(over_in(&other_view, &inc_view, pieces), want, "seed {seed} in place");
            }
        }
    }
}

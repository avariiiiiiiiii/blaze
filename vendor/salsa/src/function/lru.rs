//! LRU eviction on every core (docs/v2/plan/lanes/db/speed-of-light.md, "LRU eviction").
//!
//! A pass drops the values of the memos verified longest ago until at most `capacity` hold one;
//! memos verified in the same revision go in slot order, so the choice is deterministic. Each
//! worker walks a contiguous range of the function's table pages (`table::scan`) twice:
//!
//! 1. it collects the `verified_at` of its range's evictable memos into a buffer of its own,
//! 2. the workers find the cutoff revision together, by a radix select over the buffers (8 bits
//!    a round over `verified_at - min`, so one round when the revisions span under 256, at most
//!    four): each worker counts its buffer into a histogram, and after a barrier every worker
//!    sums the same histograms and picks the same bucket,
//! 3. each range drops the values verified before the cutoff and, of those verified at it, the
//!    share of the "first `at_cutoff` in slot order" that falls in it: the ranges before it hold
//!    a prefix sum of the at-cutoff memos, so a range skips none and takes
//!    `at_cutoff - before`, clamped to what it has.
//!
//! So the values dropped are those the serial walk drops, for any number of workers.

use std::sync::Barrier;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::Revision;
use crate::table::scan::{MemoPages, run_split};

/// Bits of `verified_at` one round of the select ranks.
const DIGIT_BITS: u32 = 8;
const BUCKETS: usize = 1 << DIGIT_BITS;

/// A worker's words in [`Ranker::shared`]: its evictable count, its least and greatest
/// `verified_at`, and two histograms (rounds alternate, so a round's counts are written while
/// the last round's are still being read), padded to whole cache lines.
const LEN: usize = 0;
const MIN: usize = 1;
const MAX: usize = 2;
const HISTOGRAMS: usize = 3;
const STRIDE: usize = (HISTOGRAMS + 2 * BUCKETS).next_multiple_of(16);

/// One worker's share of a pass: memos visited in the same order by both walks.
pub(super) trait Evictable: Send {
    /// Calls `f` with the `verified_at` of each memo holding a value the LRU may drop.
    fn each_verified(&mut self, f: impl FnMut(Revision));

    /// Visits the same memos in the same order, dropping each value for which `evicts` (called
    /// with the memo's `verified_at`) returns true.
    fn evict(&mut self, evicts: impl FnMut(Revision) -> bool);
}

/// What an LRU pass keeps between revisions, so a warm pass allocates nothing.
#[derive(Default)]
pub(super) struct LruScratch {
    pub(super) pages: MemoPages,
    pub(super) ranker: Ranker,
}

#[derive(Default)]
pub(super) struct Ranker {
    /// Per worker, the `verified_at` of its range's evictable memos, in walk order.
    verified: Vec<Vec<Revision>>,
    /// Per worker, [`STRIDE`] words it writes and the others read after a barrier.
    shared: Vec<AtomicU32>,
}

impl Ranker {
    /// Drops values until at most `capacity` of the memos in `ranges` (in range order) hold one.
    pub(super) fn evict<R: Evictable>(
        &mut self,
        ranges: impl ExactSizeIterator<Item = R>,
        capacity: usize,
    ) {
        let workers = ranges.len();
        if workers == 0 {
            return;
        }
        if self.verified.len() < workers {
            // alloc: grows to the worker count once; each buffer then to its range's memo count
            self.verified.resize_with(workers, Vec::new);
        }
        if self.shared.len() < workers * STRIDE {
            // alloc: grows to the worker count once
            self.shared
                .resize_with(workers * STRIDE, || AtomicU32::new(0));
        }
        let shared = &self.shared[..workers * STRIDE];
        let barrier = Barrier::new(workers);
        let abort = AtomicBool::new(false);
        run_split(
            ranges.zip(&mut self.verified[..workers]),
            |worker, mut range, verified| {
                let mut phases = Phases {
                    barrier: &barrier,
                    abort: &abort,
                    done: false,
                };
                let pass = Pass {
                    worker,
                    workers,
                    shared,
                    capacity,
                };
                pass.run(&mut range, verified, &mut phases);
            },
        );
    }
}

/// The barriers between a pass's phases. A worker that panics before the last one still meets
/// the barrier the others wait at and tells them to stop, so a panic is raised, not a hang.
struct Phases<'a> {
    barrier: &'a Barrier,
    abort: &'a AtomicBool,
    /// Past the last barrier: a panic now waits for nobody.
    done: bool,
}

impl Phases<'_> {
    /// Waits for every worker; false if one panicked, and then nobody waits again.
    fn sync(&mut self) -> bool {
        self.barrier.wait();
        !self.abort.load(Ordering::Relaxed)
    }
}

impl Drop for Phases<'_> {
    fn drop(&mut self) {
        if !self.done && std::thread::panicking() {
            // Relaxed: the barrier orders it before the others' loads.
            self.abort.store(true, Ordering::Relaxed);
            self.barrier.wait();
        }
    }
}

/// One worker's view of a pass.
struct Pass<'a> {
    worker: usize,
    workers: usize,
    shared: &'a [AtomicU32],
    capacity: usize,
}

impl Pass<'_> {
    /// Word `word` of worker `worker`.
    fn word(&self, worker: usize, word: usize) -> &AtomicU32 {
        &self.shared[worker * STRIDE + word]
    }

    // Relaxed loads and stores throughout: every load of another worker's word follows a barrier
    // that its store precedes.
    fn load(&self, worker: usize, word: usize) -> u32 {
        self.word(worker, word).load(Ordering::Relaxed)
    }

    fn run<R: Evictable>(
        &self,
        range: &mut R,
        verified: &mut Vec<Revision>,
        phases: &mut Phases<'_>,
    ) {
        verified.clear();
        let (mut least, mut greatest) = (u32::MAX, 0);
        range.each_verified(|at| {
            // alloc: none once the buffer has grown to its range's memo count
            verified.push(at);
            least = least.min(at.as_u32());
            greatest = greatest.max(at.as_u32());
        });
        let len = u32::try_from(verified.len())
            .unwrap_or_else(|_| panic!("salsa: an LRU range holds 2^32 memos"));
        self.word(self.worker, LEN).store(len, Ordering::Relaxed);
        self.word(self.worker, MIN).store(least, Ordering::Relaxed);
        self.word(self.worker, MAX).store(greatest, Ordering::Relaxed);
        if !phases.sync() {
            return;
        }

        let workers = || 0..self.workers;
        let total: usize = workers().map(|w| self.load(w, LEN) as usize).sum();
        let Some(excess) = total.checked_sub(self.capacity).filter(|&n| n > 0) else {
            phases.done = true;
            return;
        };
        // Empty ranges report `u32::MAX` and 0, which change neither bound of the others.
        let min = workers().map(|w| self.load(w, MIN)).min().unwrap_or(0);
        let max = workers().map(|w| self.load(w, MAX)).max().unwrap_or(0);
        let rounds = (u32::BITS - (max - min).leading_zeros()).div_ceil(DIGIT_BITS);

        // The cutoff's offset from `min`, digit by digit, and its rank among the values that
        // share the digits found so far.
        let mut prefix = 0u32;
        let mut rank = excess - 1;
        let mut last_histogram = None;
        for round in 0..rounds {
            let shift = (rounds - 1 - round) * DIGIT_BITS;
            let histogram = HISTOGRAMS + (round as usize % 2) * BUCKETS;
            let mut counts = [0u32; BUCKETS];
            for at in verified.iter() {
                let offset = at.as_u32() - min;
                if u64::from(offset) >> (shift + DIGIT_BITS) == u64::from(prefix) {
                    counts[(offset >> shift) as usize % BUCKETS] += 1;
                }
            }
            for (bucket, &count) in counts.iter().enumerate() {
                self.word(self.worker, histogram + bucket)
                    .store(count, Ordering::Relaxed);
            }
            if !phases.sync() {
                return;
            }
            let mut digit = BUCKETS;
            for bucket in 0..BUCKETS {
                let count: usize = workers()
                    .map(|w| self.load(w, histogram + bucket) as usize)
                    .sum();
                if rank < count {
                    digit = bucket;
                    break;
                }
                rank -= count;
            }
            assert!(digit < BUCKETS, "salsa: the LRU's histograms hold its rank");
            prefix = (prefix << DIGIT_BITS) | digit as u32;
            last_histogram = Some(histogram + digit);
        }
        phases.done = true;

        // Memos at the cutoff, in range `w`.
        let at_cutoff_in = |w: usize| match last_histogram {
            Some(word) => self.load(w, word) as usize,
            None => self.load(w, LEN) as usize,
        };
        let at_cutoff = rank + 1;
        let before: usize = (0..self.worker).map(at_cutoff_in).sum();
        let mut cutoff = EvictionCutoff {
            cutoff: Revision::from_u32(min + prefix)
                .unwrap_or_else(|| unreachable!("a cutoff is a revision some memo has")),
            at_cutoff: at_cutoff
                .saturating_sub(before)
                .min(at_cutoff_in(self.worker)),
        };
        range.evict(|at| cutoff.evicts(at));
    }
}

/// Which values an LRU pass drops: those verified before `cutoff`, and the first `at_cutoff` (in
/// slot order) of those verified in `cutoff` itself.
struct EvictionCutoff {
    cutoff: Revision,
    at_cutoff: usize,
}

impl EvictionCutoff {
    /// The serial ranking, the reference the parallel one is tested against: the cutoff that
    /// keeps `capacity` of the values whose memos were verified at `verified` (in slot order;
    /// reordered here), or `None` if they fit.
    #[cfg(test)]
    fn new(verified: &mut [Revision], capacity: usize) -> Option<Self> {
        let excess = verified.len().checked_sub(capacity).filter(|&n| n > 0)?;
        let cutoff = *verified.select_nth_unstable(excess - 1).1;
        let below = verified.iter().filter(|&&at| at < cutoff).count();
        Some(Self {
            cutoff,
            at_cutoff: excess - below,
        })
    }

    /// Whether the next value, in slot order, verified at `at` is dropped.
    fn evicts(&mut self, at: Revision) -> bool {
        if at < self.cutoff {
            return true;
        }
        if at == self.cutoff && self.at_cutoff > 0 {
            self.at_cutoff -= 1;
            return true;
        }
        false
    }
}

#[cfg(test)]
#[cfg(not(feature = "shuttle"))]
mod tests {
    use blaze_ids::rng::SplitMix64;

    use super::{Evictable, EvictionCutoff, Ranker};
    use crate::Revision;
    use crate::table::scan::part;

    /// The safe reference: sort the slots by (verified-at, slot) and drop the first `excess`.
    fn reference(verified: &[Revision], capacity: usize) -> Vec<bool> {
        let mut order: Vec<usize> = (0..verified.len()).collect();
        order.sort_by_key(|&slot| (verified[slot], slot));
        let mut evicted = vec![false; verified.len()];
        for &slot in order.iter().take(verified.len().saturating_sub(capacity)) {
            evicted[slot] = true;
        }
        evicted
    }

    fn evicted(verified: &[Revision], capacity: usize) -> Vec<bool> {
        let mut scratch = verified.to_vec();
        match EvictionCutoff::new(&mut scratch, capacity) {
            None => vec![false; verified.len()],
            Some(mut cutoff) => verified.iter().map(|&at| cutoff.evicts(at)).collect(),
        }
    }

    #[test]
    fn cutoff_matches_sorted_reference() {
        let mut rng = SplitMix64::new(0x5eed);
        for _ in 0..2000 {
            let len = rng.index(40);
            let revisions = 1 + rng.below(6);
            let capacity = 1 + rng.index(40);
            let verified: Vec<Revision> = (0..len)
                .map(|_| Revision::from(1 + rng.below(revisions) as usize))
                .collect();
            let got = evicted(&verified, capacity);
            assert_eq!(got, reference(&verified, capacity), "{verified:?} cap {capacity}");
            let kept = got.iter().filter(|&&e| !e).count();
            assert_eq!(kept, len.min(capacity));
            let newest_evicted = verified.iter().zip(&got).filter(|p| *p.1).map(|p| *p.0).max();
            let oldest_kept = verified.iter().zip(&got).filter(|p| !*p.1).map(|p| *p.0).min();
            if let (Some(evicted), Some(kept)) = (newest_evicted, oldest_kept) {
                assert!(evicted <= kept);
            }
        }
    }

    #[test]
    fn ties_go_in_slot_order() {
        let r = Revision::from(3);
        assert_eq!(evicted(&[r, r, r, r], 2), [true, true, false, false]);
        let old = Revision::from(1);
        assert_eq!(evicted(&[r, old, r, old], 3), [false, true, false, false]);
    }

    /// A slot of a fake table: its memo's `verified_at`, whether its value may be dropped, and
    /// whether it was.
    #[derive(Clone, Copy)]
    struct Fake {
        at: Revision,
        evictable: bool,
        evicted: bool,
    }

    struct FakeRange<'a> {
        slots: &'a mut [Fake],
        panics: bool,
    }

    impl Evictable for FakeRange<'_> {
        fn each_verified(&mut self, mut f: impl FnMut(Revision)) {
            assert!(!self.panics, "a range that panics");
            for slot in self.slots.iter().filter(|s| s.evictable && !s.evicted) {
                f(slot.at);
            }
        }

        fn evict(&mut self, mut evicts: impl FnMut(Revision) -> bool) {
            for slot in self.slots.iter_mut().filter(|s| s.evictable && !s.evicted) {
                slot.evicted = evicts(slot.at);
            }
        }
    }

    /// `slots` split by pages (`pages[p]` slots each) into `workers` ranges of whole pages, as
    /// `PageWalk::ranges` splits them.
    fn ranges<'a>(slots: &'a mut [Fake], pages: &[usize], workers: usize) -> Vec<FakeRange<'a>> {
        let mut rest = slots;
        (0..workers)
            .map(|index| {
                let len: usize = pages[part(pages.len(), workers, index)].iter().sum();
                let (range, after) = std::mem::take(&mut rest).split_at_mut(len);
                rest = after;
                FakeRange {
                    slots: range,
                    panics: false,
                }
            })
            .collect()
    }

    /// The serial walk: one buffer over every slot, `EvictionCutoff::new`, then one pass.
    fn serial(slots: &mut [Fake], capacity: usize) {
        let mut verified: Vec<Revision> = slots
            .iter()
            .filter(|s| s.evictable && !s.evicted)
            .map(|s| s.at)
            .collect();
        if let Some(mut cutoff) = EvictionCutoff::new(&mut verified, capacity) {
            for slot in slots.iter_mut().filter(|s| s.evictable && !s.evicted) {
                slot.evicted = cutoff.evicts(slot.at);
            }
        }
    }

    /// Random tables (many pages, partly filled, random `verified_at` over spans that take zero
    /// to four rounds of the select, some memos not evictable or already empty), random
    /// capacities and several worker counts: the parallel pass drops what the serial one does.
    #[test]
    fn parallel_matches_serial() {
        let mut rng = SplitMix64::new(0x1a0_5ca9);
        let spans = [1u64, 2, 7, 255, 256, 3000, 70_000, 1 << 24, u32::MAX as u64 - 2];
        let mut ranker = Ranker::default();
        for case in 0..300 {
            let page_count = rng.index(400);
            let pages: Vec<usize> = (0..page_count)
                .map(|_| if rng.one_in(4) { 128 } else { rng.index(129) })
                .collect();
            let span = spans[rng.index(spans.len())];
            let base = 1 + rng.below(u32::MAX as u64 - span);
            let ties = rng.one_in(3);
            let slots: Vec<Fake> = (0..pages.iter().sum::<usize>())
                .map(|_| Fake {
                    at: Revision::from(
                        (base + if ties { rng.below(span.min(3)) } else { rng.below(span) })
                            as usize,
                    ),
                    evictable: !rng.one_in(6),
                    evicted: rng.one_in(10),
                })
                .collect();
            let total = slots.iter().filter(|s| s.evictable && !s.evicted).count();
            let capacity = match rng.index(4) {
                0 => total + rng.index(10),
                1 => rng.index(total + 1),
                2 => total.saturating_sub(1 + rng.index(3)),
                _ => rng.index(16),
            };
            let mut expected = slots.clone();
            serial(&mut expected, capacity);
            for workers in [1, 2, 3, 5, 8, 16] {
                let mut got = slots.clone();
                ranker.evict(ranges(&mut got, &pages, workers).into_iter(), capacity);
                let same = got.iter().zip(&expected).all(|(g, e)| g.evicted == e.evicted);
                assert!(
                    same,
                    "case {case}: {workers} workers, {page_count} pages, span {span}, cap {capacity}"
                );
            }
            let kept = expected.iter().filter(|s| s.evictable && !s.evicted).count();
            assert_eq!(kept, total.min(capacity));
        }
    }

    /// A worker that panics before the others pass a barrier stops the pass with its panic, not
    /// a hang.
    #[test]
    fn a_panicking_range_raises_instead_of_hanging() {
        let mut slots = vec![
            Fake {
                at: Revision::from(1),
                evictable: true,
                evicted: false,
            };
            4 * 128
        ];
        let pages = [128; 4];
        for panicking in 0..4 {
            let mut ranges = ranges(&mut slots, &pages, 4);
            ranges[panicking].panics = true;
            let mut ranker = Ranker::default();
            let raised = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                ranker.evict(ranges.into_iter(), 10);
            }));
            assert!(raised.is_err());
        }
    }
}

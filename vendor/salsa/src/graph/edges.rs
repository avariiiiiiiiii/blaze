//! The database's edges: one CSR array of `u32` words, each a node and the edge's kind.
//!
//! A memo's edges are a range of this array (`EdgeSpan`), rustc's one edge list for the whole
//! graph (`SerializedDepGraph`), so a memo's edges cost 4 bytes each and no allocation. The array
//! is pages of words that double and never move, and a range never crosses a page, so a memo's
//! edges are one slice. A task reserves 256 words at a time (one compare-and-swap per 256 edges)
//! and writes a query's edges when it finishes; the memo's publication orders them for readers
//! (docs/v2/plan/design/concurrency.md, "Dependencies are recorded per task").
//!
//! Replaced memos leave dead ranges; `Graph::compact` copies the live ones into a new array at a
//! new revision, with `&mut`.

use std::cell::Cell;

use crate::sync::OnceLock;
use crate::sync::atomic::{AtomicU32, Ordering};

/// Page `p` holds `1 << (FIRST_PAGE_BITS + p)` words; 20 pages hold all but the last 4096 of
/// 2^32.
const FIRST_PAGE_BITS: u32 = 12;
const PAGES: usize = 20;

/// The words of every page together.
const CAPACITY: u64 = page_start(PAGES);

/// Words a task reserves at a time. A query with more than half this many edges reserves its
/// own range, so a chunk wastes at most half itself.
pub(crate) const CHUNK: u32 = 256;

/// A memo's edges: `len` words from `start`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub(crate) struct EdgeSpan {
    pub(crate) start: u32,
    pub(crate) len: u32,
}

impl EdgeSpan {
    pub(crate) const EMPTY: EdgeSpan = EdgeSpan { start: 0, len: 0 };
}

/// The part of the array a task reserved and has not filled.
#[derive(Copy, Clone, Default)]
pub(crate) struct Chunk {
    /// The [`Edges::generation`] it was reserved from; 0, which none has, before the first.
    generation: u32,
    next: u32,
    end: u32,
}

pub(crate) struct Edges {
    pages: Box<[OnceLock<Box<[AtomicU32]>>]>,
    /// The words reserved: every range starts below it.
    len: AtomicU32,
    /// Which array this is; a compaction makes the next, and a task's chunk of another is
    /// dropped.
    generation: u32,
}

const fn page_start(page: usize) -> u64 {
    (1 << (FIRST_PAGE_BITS as u64 + page as u64)) - (1 << FIRST_PAGE_BITS)
}

const fn page_len(page: usize) -> u64 {
    1 << (FIRST_PAGE_BITS as u64 + page as u64)
}

/// The page of `position` and its offset there.
#[inline]
fn locate(position: u64) -> (usize, usize) {
    let x = position + (1 << FIRST_PAGE_BITS);
    let bits = u64::BITS - 1 - x.leading_zeros();
    (
        (bits - FIRST_PAGE_BITS) as usize,
        (x - (1 << bits)) as usize,
    )
}

/// The first position at or after `start` from which `n` words lie in one page.
fn fit(start: u64, n: u64) -> u64 {
    let (mut page, offset) = locate(start);
    if offset as u64 + n <= page_len(page) {
        return start;
    }
    page += 1;
    while page_len(page) < n {
        page += 1;
    }
    page_start(page)
}

impl Edges {
    pub(crate) fn new(generation: u32) -> Edges {
        assert_ne!(generation, 0, "salsa: edge array generation 0 marks a task's empty chunk");
        Edges {
            // alloc: setup, once per database: the edge array's page directory
            pages: (0..PAGES).map(|_| OnceLock::new()).collect(),
            len: AtomicU32::new(0),
            generation,
        }
    }

    pub(crate) fn generation(&self) -> u32 {
        self.generation
    }

    /// The words reserved so far, live or dead.
    pub(crate) fn len(&self) -> u32 {
        self.len.load(Ordering::Relaxed)
    }

    /// Reserves `n` words in one page.
    pub(crate) fn reserve(&self, n: u32) -> u32 {
        let n = u64::from(n);
        let mut start = self.len.load(Ordering::Relaxed);
        loop {
            let begin = fit(u64::from(start), n);
            let end = begin + n;
            assert!(
                end <= CAPACITY,
                "salsa: the edge array is full (2^32 words reserved since the last compaction)"
            );
            match self.len.compare_exchange_weak(
                start,
                end as u32,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    let (page, _) = locate(begin);
                    self.page(page);
                    return begin as u32;
                }
                Err(now) => start = now,
            }
        }
    }

    /// Reserves `n` words from the task's chunk, reserving a new one when it is short.
    pub(crate) fn reserve_for_task(&self, chunk: &Cell<Chunk>, n: u32) -> u32 {
        let current = chunk.get();
        if current.generation == self.generation && current.end - current.next >= n {
            chunk.set(Chunk {
                next: current.next + n,
                ..current
            });
            return current.next;
        }
        if n > CHUNK / 2 {
            return self.reserve(n);
        }
        let start = self.reserve(CHUNK);
        chunk.set(Chunk {
            generation: self.generation,
            next: start + n,
            end: start + CHUNK,
        });
        start
    }

    fn page(&self, page: usize) -> &[AtomicU32] {
        self.pages[page].get_or_init(|| super::zeroed_u32s(page_len(page) as usize))
    }

    /// The words of `span`, which [`Edges::reserve`] or [`Edges::reserve_for_task`] gave.
    #[inline]
    pub(crate) fn slice(&self, span: EdgeSpan) -> &[AtomicU32] {
        if span.len == 0 {
            return &[];
        }
        let (page, offset) = locate(u64::from(span.start));
        let page = self.pages[page]
            .get()
            .expect("salsa: a memo's edges lie in a page their reservation allocated");
        &page[offset..offset + span.len as usize]
    }
}

#[cfg(test)]
#[cfg(not(feature = "shuttle"))]
mod tests {
    use std::cell::Cell;

    use super::{CAPACITY, CHUNK, Chunk, EdgeSpan, Edges, fit, locate, page_len, page_start};
    use crate::sync::atomic::Ordering;

    #[test]
    fn pages_tile_the_positions() {
        assert_eq!(locate(0), (0, 0));
        assert_eq!(locate(4095), (0, 4095));
        assert_eq!(locate(4096), (1, 0));
        assert_eq!(locate(page_start(5) + 7), (5, 7));
        assert_eq!(CAPACITY, (1 << 32) - 4096);
        assert_eq!(fit(4000, 96), 4000);
        assert_eq!(fit(4000, 97), 4096);
        assert_eq!(fit(4000, 20_000), page_start(3));
        assert!(page_len(3) >= 20_000);
    }

    /// The safe reference is a list of the ranges handed out: they are disjoint, each in one page.
    #[test]
    fn ranges_never_overlap_or_cross_a_page() {
        let edges = Edges::new(1);
        let chunk = Cell::new(Chunk::default());
        let mut ranges = Vec::new();
        let mut state = 3u64;
        for _ in 0..5000 {
            state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let n = match state >> 61 {
                0 => 0,
                1 => 300 + (state >> 40) as u32 % 3000,
                _ => 1 + (state >> 40) as u32 % 40,
            };
            if n == 0 {
                continue;
            }
            let start = edges.reserve_for_task(&chunk, n);
            let (first, _) = locate(u64::from(start));
            let (last, _) = locate(u64::from(start + n - 1));
            assert_eq!(first, last, "a range crosses a page");
            let slice = edges.slice(EdgeSpan { start, len: n });
            for word in slice {
                assert_eq!(word.load(Ordering::Relaxed), 0, "a range was handed out twice");
                word.store(1, Ordering::Relaxed);
            }
            ranges.push((start, n));
        }
        ranges.sort_unstable();
        for pair in ranges.windows(2) {
            assert!(pair[0].0 + pair[0].1 <= pair[1].0);
        }
    }

    #[test]
    fn a_chunk_of_another_generation_is_dropped() {
        let old = Edges::new(1);
        let chunk = Cell::new(Chunk::default());
        let first = old.reserve_for_task(&chunk, 3);
        assert_eq!(old.reserve_for_task(&chunk, 3), first + 3);
        assert_eq!(old.len(), CHUNK);
        let new = Edges::new(2);
        assert_eq!(new.reserve_for_task(&chunk, 3), 0);
        assert_eq!(new.len(), CHUNK);
    }

    #[test]
    fn racing_tasks_get_disjoint_ranges() {
        let edges = Edges::new(1);
        let ranges: Vec<Vec<(u32, u32)>> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..8u32)
                .map(|t| {
                    let edges = &edges;
                    scope.spawn(move || {
                        let chunk = Cell::new(Chunk::default());
                        (0..2000u32)
                            .map(|i| {
                                let n = 1 + (i * 7 + t) % 200;
                                (edges.reserve_for_task(&chunk, n), n)
                            })
                            .collect()
                    })
                })
                .collect();
            workers.into_iter().map(|w| w.join().unwrap()).collect()
        });
        let mut all: Vec<(u32, u32)> = ranges.into_iter().flatten().collect();
        all.sort_unstable();
        for pair in all.windows(2) {
            assert!(pair[0].0 + pair[0].1 <= pair[1].0);
        }
    }
}

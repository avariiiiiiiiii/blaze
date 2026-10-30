//! One function's memos walked on several threads (docs/v2/plan/lanes/db/speed-of-light.md,
//! "LRU eviction"). The pages holding the function's memos are split into contiguous ranges,
//! one per worker, and each range is walked in page and slot order, so what the workers compute
//! per range, joined in range order, is what one walk over every page computes, whatever the
//! number of workers.

use std::ops::Range;

use super::{SlotIndex, Table};
use crate::sync::atomic::Ordering;
use crate::table::memo::{Memo, MemoTableWithTypesMut};
use crate::zalsa::{IngredientIndex, MemoIngredientIndex};

/// The fewest pages a worker takes: 64 pages of 128 slots, about 8K memos and 25 µs of walking
/// at ~100 bytes a memo, so the walk pays for starting a thread (≈ 10 µs). A smaller table is
/// walked on the caller's thread alone.
pub(crate) const MIN_PAGES_PER_WORKER: usize = 64;

#[derive(Clone, Copy)]
struct MemoPage {
    page: u32,
    memo_index: MemoIngredientIndex,
}

/// The pages of a table that hold one function's memos, in increasing page order: filled only by
/// [`Table::memo_pages`], so no page is in it twice, and kept by the function between revisions
/// so filling it allocates only when the table outgrows it.
#[derive(Default)]
pub(crate) struct MemoPages {
    pages: Vec<MemoPage>,
}

impl MemoPages {
    pub(crate) fn len(&self) -> usize {
        self.pages.len()
    }
}

/// The workers a walk of `pages` pages takes: one per [`MIN_PAGES_PER_WORKER`] pages, at least
/// one and at most the machine's parallelism. Always one under shuttle, whose atomics may not be
/// touched from threads it doesn't run.
pub(crate) fn workers_for(pages: usize) -> usize {
    if cfg!(feature = "shuttle") {
        return 1;
    }
    (pages / MIN_PAGES_PER_WORKER).clamp(1, crate::sync::max_parallelism())
}

/// The `index`th of `parts` contiguous ranges splitting `0..len` as evenly as possible.
pub(crate) fn part(len: usize, parts: usize, index: usize) -> Range<usize> {
    len * index / parts..len * (index + 1) / parts
}

/// Runs `work(index, item, state)` for each pair `items` yields, numbered from 0: the first on
/// this thread, each other on a thread of its own, so workers that wait for each other all run
/// at once. Returns when every one has; a panic in one is raised here after all have stopped.
pub(crate) fn run_split<'s, R: Send, S: Send + 's>(
    mut items: impl Iterator<Item = (R, &'s mut S)>,
    work: impl Fn(usize, R, &mut S) + Sync,
) {
    let Some((first, first_state)) = items.next() else {
        return;
    };
    let mut rest = items.peekable();
    if rest.peek().is_none() {
        work(0, first, first_state);
        return;
    }
    std::thread::scope(|scope| {
        let work = &work;
        for (index, (item, state)) in rest.enumerate() {
            // alloc: one thread per worker per pass, each walking at least 64 pages
            scope.spawn(move || work(index + 1, item, state));
        }
        work(0, first, first_state);
    });
}

impl Table {
    /// Fills `pages` with the pages holding memos of type `M` at the index `memo_index` returns
    /// for the page's ingredient, in page order.
    pub(crate) fn memo_pages<M: Memo>(
        &self,
        memo_index: impl Fn(IngredientIndex) -> Option<MemoIngredientIndex>,
        pages: &mut MemoPages,
    ) {
        pages.pages.clear();
        for (page_index, page) in self.pages.iter() {
            let Some(memo_index) = memo_index(page.ingredient) else {
                continue;
            };
            if page.memo_types.holds::<M>(memo_index) {
                // alloc: grows only when the table has more of the function's pages than ever before
                pages.pages.push(MemoPage {
                    page: page_index,
                    memo_index,
                });
            }
        }
    }

    /// A walk of `pages`' memo tables to split between workers; `&mut self` makes it the table's
    /// only user while it lasts.
    pub(crate) fn page_walk<'t>(&'t mut self, pages: &'t MemoPages) -> PageWalk<'t> {
        PageWalk {
            table: self,
            pages: &pages.pages,
        }
    }
}

/// The memo tables of a [`MemoPages`], with the table's exclusive access.
pub(crate) struct PageWalk<'t> {
    table: &'t Table,
    pages: &'t [MemoPage],
}

impl PageWalk<'_> {
    /// `parts` contiguous ranges of the walk's pages, in page order; no page is in two of them,
    /// and while they live (they borrow the walk mutably) no other range exists.
    pub(crate) fn ranges<'w>(
        &'w mut self,
        parts: usize,
    ) -> impl ExactSizeIterator<Item = PageRange<'w>> + 'w {
        let table: &'w Table = self.table;
        let pages: &'w [MemoPage] = self.pages;
        (0..parts).map(move |index| PageRange {
            table,
            pages: &pages[part(pages.len(), parts, index)],
        })
    }
}

/// One worker's share of a [`PageWalk`]: pages no other range has.
pub(crate) struct PageRange<'w> {
    table: &'w Table,
    pages: &'w [MemoPage],
}

impl PageRange<'_> {
    /// Calls `f` with the memo table of every allocated slot on the range's pages, in page and
    /// slot order.
    pub(crate) fn for_each_memo_table_mut(
        &mut self,
        mut f: impl FnMut(MemoTableWithTypesMut<'_>, MemoIngredientIndex),
    ) {
        for &MemoPage { page, memo_index } in self.pages {
            let page = self.table.page_at(page as usize);
            let stride = page.slot_vtable.layout.size();
            for slot in 0..page.allocated.load(Ordering::Acquire) {
                let row = page.memo_row(SlotIndex::new(slot));
                // SAFETY: `slot` is below the page's initialized length, so the pointer is to an
                // initialized slot of the page's slot type, whose vtable this is. The slot is in
                // an `UnsafeCell`, and only this range reaches it: `PageWalk::ranges` hands out
                // ranges of distinct pages (`MemoPages` holds each page once), and
                // `Table::page_walk` holds the table's `&mut` for as long as they live.
                let memos = unsafe {
                    &mut *(page.slot_vtable.memos_mut)(page.data.as_ptr().byte_add(slot * stride))
                };
                // SAFETY: `row` is the row of the slot `memos` belongs to, of this page's
                // ingredient.
                let memos = unsafe { page.memo_types.attach_memos_mut(row, memos) };
                f(memos, memo_index);
            }
        }
    }
}

#[cfg(test)]
#[cfg(not(feature = "shuttle"))]
mod tests {
    use super::{MemoPages, part, run_split};
    use crate::sync::Arc;
    use crate::table::memo::{Memo, MemoEntryType, MemoTable, MemoTableTypes};
    use crate::table::{Slot, Table};
    use crate::zalsa::{IngredientIndex, MemoIngredientIndex, Zalsa};
    use crate::{DatabaseKeyIndex, Id, Revision};

    struct TestSlot(MemoTable);

    // SAFETY: `TestSlot` is private to these tests, and only its own ingredient's pages hold it.
    unsafe impl Slot for TestSlot {
        unsafe fn memos(slot: *const Self, _: Revision) -> *const MemoTable {
            // SAFETY: The caller passes an initialized slot.
            unsafe { &raw const (*slot).0 }
        }

        unsafe fn memos_unchecked(slot: *const Self) -> *const MemoTable {
            // SAFETY: The caller passes an initialized slot.
            unsafe { &raw const (*slot).0 }
        }

        fn memos_mut(&mut self) -> &mut MemoTable {
            &mut self.0
        }
    }

    /// A memo naming its slot and counting the walks that reached it.
    struct Mark {
        id: u32,
        seen: u32,
    }

    impl Memo for Mark {
        fn has_value(&self) -> bool {
            true
        }

        fn remove_outputs(&self, _zalsa: &Zalsa, _executor: DatabaseKeyIndex) {}

        #[cfg(feature = "salsa_unstable")]
        fn memory_usage(&self) -> crate::database::MemoInfo {
            unreachable!()
        }
    }

    const MARKS: MemoIngredientIndex = MemoIngredientIndex::from_usize(0);

    #[test]
    fn parts_cover_in_order() {
        for len in [0, 1, 5, 64, 1000] {
            for parts in 1..9 {
                let mut next = 0;
                for index in 0..parts {
                    let range = part(len, parts, index);
                    assert_eq!(range.start, next);
                    next = range.end;
                }
                assert_eq!(next, len);
            }
        }
    }

    /// Ranges walked on threads visit every memo of the function once, and joined in range order
    /// they are the serial walk's order; pages of other ingredients are skipped.
    #[test]
    fn ranges_join_to_the_serial_walk() {
        let mut types = MemoTableTypes::default();
        types.set(MARKS, MemoEntryType::of::<Mark>());
        let types = Arc::new(types);
        let others = Arc::new(MemoTableTypes::default());
        let mine = IngredientIndex::new(1);
        let mut table = Table::default();
        for page_number in 0..300usize {
            let (ingredient, page_types) = if page_number % 7 == 3 {
                (IngredientIndex::new(2), &others)
            } else {
                (mine, &types)
            };
            let page = table.push_page::<TestSlot>(ingredient, page_types.clone());
            for _ in 0..(page_number * 37) % 129 {
                // SAFETY: This thread is the page's only writer.
                let allocated = unsafe {
                    table.page::<TestSlot>(page).allocate(page, |_: Id| {
                        // SAFETY: The table is only accessed with these types.
                        TestSlot(unsafe { MemoTable::new(page_types) })
                    })
                };
                let Ok((id, _)) = allocated else {
                    panic!("a page holds 128 slots");
                };
                if ingredient == mine {
                    // SAFETY: The table has one revision, the start.
                    let memos = unsafe { table.memos::<TestSlot>(id, Revision::start()) };
                    memos.insert(
                        MARKS,
                        Mark {
                            id: id.index(),
                            seen: 0,
                        },
                    );
                }
            }
        }
        let memo_index = |ingredient| (ingredient == mine).then_some(MARKS);

        let mut serial = Vec::new();
        table.for_each_memo_table_mut::<Mark>(memo_index, |memos, index| {
            memos.map_memo(index, |mark: &mut Mark| serial.push(mark.id));
        });
        assert!(serial.windows(2).all(|pair| pair[0] < pair[1]));

        let mut pages = MemoPages::default();
        let worker_counts = [1, 2, 3, 7, 16];
        for workers in worker_counts {
            table.memo_pages::<Mark>(memo_index, &mut pages);
            let mut walk = table.page_walk(&pages);
            let mut seen: Vec<Vec<u32>> = (0..workers).map(|_| Vec::new()).collect();
            run_split(walk.ranges(workers).zip(seen.iter_mut()), |_, mut range, seen| {
                range.for_each_memo_table_mut(|memos, index| {
                    memos.map_memo(index, |mark: &mut Mark| {
                        mark.seen += 1;
                        seen.push(mark.id);
                    });
                });
            });
            assert_eq!(seen.concat(), serial, "{workers} workers");
        }

        table.for_each_memo_table_mut::<Mark>(memo_index, |memos, index| {
            memos.map_memo(index, |mark: &mut Mark| {
                assert_eq!(mark.seen as usize, worker_counts.len());
            });
        });
    }
}

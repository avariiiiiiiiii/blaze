//! A task's cache of the table pages its memo lookups use (docs/v2/plan/lanes/db/speed-of-light.md,
//! "Memo hit"). Finding a memo from an `Id` reads the page list's line and the `Page`'s line
//! before the column, the row and the memo; neither changes once the page is published, so each
//! task keeps what it read of a page in a direct-mapped cache of [`PageCache::LEN`] entries and a
//! hit reads that entry (L1) instead, leaving the row and the memo: the bound.
//!
//! An entry can't go stale. A page's slots (`data`), slot vtable, memo columns and first memo
//! row are set when it is made and never change; the `MemoTableTypes` the columns came from only
//! changes through `Arc::get_mut` before any page shares it; pages are never removed or moved
//! while the table lives; and the cache remembers its table, emptying itself when used with
//! another. (A `ZalsaLocal`, which holds the cache, lives in a database handle that keeps its
//! database, so its table, alive, so the table's address can't be reused under it.) The one thing
//! that grows is the page's allocated count: an entry keeps what it was when cached, and a slot
//! at or past it reads the page again.

use std::cell::Cell;
use std::ptr::NonNull;

use super::memo::{MemoColumns, MemoTableWithTypes};
use super::{PageIndex, SlotIndex, SlotMemosFnErased, Table, split_id};
use crate::sync::atomic::Ordering;
use crate::{Id, Revision};

/// What a memo lookup reads from one page.
#[derive(Clone, Copy)]
struct CachedPage {
    /// The page's index.
    page: u32,
    /// Slots below this are allocated.
    allocated: u32,
    memo_rows: u32,
    slot_size: u32,
    data: NonNull<()>,
    memos: SlotMemosFnErased,
    memo_columns: MemoColumns,
}

/// A task's direct-mapped cache of pages, by page index (see the module docs).
pub(crate) struct PageCache {
    /// The table the entries are of: a lookup in another empties the cache first, so a cache
    /// can't hand out one table's pages for another's ids.
    table: Cell<*const Table>,
    entries: [Cell<Option<CachedPage>>; PageCache::LEN],
}

// SAFETY: an entry's pointers are copies of a `Page`'s, which is `Send` and `Sync`, and the
// memory they name belongs to the table, not the cache; moving the cache to another thread moves
// only the copies. (The `Cell`s keep it `!Sync`, as its task is.)
unsafe impl Send for PageCache {}

impl Default for PageCache {
    fn default() -> Self {
        PageCache {
            table: Cell::new(std::ptr::null()),
            entries: [const { Cell::new(None) }; PageCache::LEN],
        }
    }
}

impl std::fmt::Debug for PageCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PageCache").finish_non_exhaustive()
    }
}

impl PageCache {
    /// Entries: 64 of 48 bytes, 3 KB beside the task's node cache.
    pub(crate) const LEN: usize = 64;

    /// Empties the cache and makes it `table`'s.
    #[cold]
    #[inline(never)]
    fn adopt(&self, table: &Table) {
        for entry in &self.entries {
            entry.set(None);
        }
        self.table.set(table);
    }
}

impl Table {
    /// [`Table::dyn_memos`] through `cache`: the memo table of slot `id`, its slot validated
    /// (a tracked struct's read lock) through the page's vtable, reading the page itself only
    /// on the cache's first lookup in it.
    ///
    /// # Safety
    ///
    /// `current_revision` is the current revision of the database owning this table.
    #[inline]
    pub(crate) unsafe fn memos_cached(
        &self,
        cache: &PageCache,
        id: Id,
        current_revision: Revision,
    ) -> MemoTableWithTypes<'_> {
        if !std::ptr::eq(cache.table.get(), self) {
            cache.adopt(self);
        }
        let (page, slot) = split_id(id);
        let entry = &cache.entries[page.0 % PageCache::LEN];
        let cached = match entry.get() {
            Some(cached) if cached.page as usize == page.0 && slot.0 < cached.allocated as usize => {
                cached
            }
            _ => self.cache_page(entry, page, slot),
        };
        // SAFETY: `slot` is below the page's allocated count (checked against the entry, which
        // holds a count the page had), so the pointer is in its data and the slot initialized.
        let slot_data = unsafe {
            cached
                .data
                .as_ptr()
                .byte_add(slot.0 * cached.slot_size as usize)
        };
        // SAFETY: `memos` is the page's slot vtable's, for the slot type its data holds, and the
        // caller passes the current revision.
        let _ = unsafe { (cached.memos)(slot_data, current_revision) };
        // SAFETY: the columns are the page's, alive and unchanged while the table lives (the
        // module docs), and the row is of a slot of this page.
        unsafe { cached.memo_columns.attach(cached.memo_rows + slot.0 as u32) }
    }

    #[cold]
    #[inline(never)]
    fn cache_page(
        &self,
        entry: &Cell<Option<CachedPage>>,
        page: PageIndex,
        slot: SlotIndex,
    ) -> CachedPage {
        let page_data = self.page_at(page.0);
        page_data.assert_allocated(slot);
        let allocated = page_data.allocated.load(Ordering::Acquire);
        let cached = CachedPage {
            page: page.0 as u32,
            allocated: allocated as u32,
            memo_rows: page_data.memo_rows,
            slot_size: page_data.slot_vtable.layout.size() as u32,
            data: page_data.data,
            memos: page_data.slot_vtable.memos,
            memo_columns: page_data.memo_columns,
        };
        entry.set(Some(cached));
        cached
    }
}

#[cfg(test)]
#[cfg(not(feature = "shuttle"))]
mod tests {
    use super::PageCache;
    use crate::sync::Arc;
    use crate::table::memo::{Memo, MemoEntryType, MemoTable, MemoTableTypes};
    use crate::table::{PageIndex, Slot, Table};
    use crate::zalsa::{IngredientIndex, MemoIngredientIndex, Zalsa};
    use crate::{DatabaseKeyIndex, Id, Revision};

    struct TestSlot(MemoTable);

    // SAFETY: `TestSlot` is private to these tests, and only their pages hold it.
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

    /// A memo naming its table and slot.
    struct Mark(u32, Id);

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

    fn fill(
        (tag, table): (u32, &Table),
        types: &Arc<MemoTableTypes>,
        page: PageIndex,
        slots: usize,
    ) -> Vec<Id> {
        (0..slots)
            .map(|_| {
                // SAFETY: This thread is the page's only writer.
                let allocated = unsafe {
                    table.page::<TestSlot>(page).allocate(page, |_: Id| {
                        // SAFETY: The table is only accessed with these types.
                        TestSlot(unsafe { MemoTable::new(types) })
                    })
                };
                let Ok((id, _)) = allocated else {
                    panic!("a page holds 128 slots");
                };
                // SAFETY: The table has one revision, the start.
                unsafe { table.memos::<TestSlot>(id, Revision::start()) }
                    .insert(MARKS, Mark(tag, id));
                id
            })
            .collect()
    }

    fn mark(table: &Table, cache: &PageCache, id: Id) -> (u32, Id) {
        // SAFETY: The table has one revision, the start.
        let memos = unsafe { table.memos_cached(cache, id, Revision::start()) };
        let mark = memos.get::<Mark>(MARKS).expect("every slot has a mark");
        // SAFETY: The table owns the mark while it lives.
        let mark = unsafe { mark.as_ref() };
        (mark.0, mark.1)
    }

    /// Through the cache every slot finds its own memo: pages that share an entry (more pages
    /// than entries) evict each other, a page that grew after it was cached finds its new slots,
    /// and a cache used with a second table forgets the first.
    #[test]
    fn cached_lookups_find_the_slots_own_memos() {
        let mut types = MemoTableTypes::default();
        types.set(MARKS, MemoEntryType::of::<Mark>());
        let types = Arc::new(types);
        let table = Table::default();
        let pages = PageCache::LEN * 2 + 3;
        let mut ids = Vec::new();
        for page_number in 0..pages {
            let page = table.push_page::<TestSlot>(IngredientIndex::new(1), types.clone());
            ids.extend(fill((0, &table), &types, page, 1 + page_number % 5));
        }
        let cache = PageCache::default();
        for round in 0..3 {
            for n in 0..ids.len() {
                let id = ids[(n * 7 + round * 13) % ids.len()];
                assert_eq!(mark(&table, &cache, id), (0, id));
            }
        }

        let first = crate::table::split_id(ids[0]).0;
        assert_eq!(mark(&table, &cache, ids[0]), (0, ids[0]));
        let grown = fill((0, &table), &types, first, 20);
        for &id in &grown {
            assert_eq!(mark(&table, &cache, id), (0, id));
        }

        let other = Table::default();
        let page = other.push_page::<TestSlot>(IngredientIndex::new(1), types.clone());
        let other_ids = fill((1, &other), &types, page, 3);
        assert_eq!(other_ids[0], ids[0]);
        for &id in &other_ids {
            assert_eq!(mark(&other, &cache, id), (1, id));
        }
        assert_eq!(mark(&table, &cache, ids[0]), (0, ids[0]));
    }
}

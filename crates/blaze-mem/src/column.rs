//! [`Column<T>`]: an append-only column any number of threads push to at once, over a
//! reserved region (docs/v2/plan/design/concurrency.md "Tables are append-only", "Values are
//! immutable once published"). The column for interners and shared tables: a push claims an
//! index with one `fetch_add`, writes the value in place and publishes it with one release
//! `fetch_or` on its ready bit; a read is a bounds check against the committed length and one
//! acquire load of the ready word, and writes nothing.
//!
//! It never moves, so `&T` from [`Column::get`] lives as long as the column, and an index is a
//! stable `u32` ID. Values are never changed or removed once published.
//!
//! Cost per push: one `fetch_add`, a store of the value and one `fetch_or`, the last two on
//! lines other pushers rarely share (a ready word covers 64 values); a commit system call per
//! quarter of growth. Per read: a bounds check and one acquire load of the ready word.

use std::{
    fmt,
    marker::PhantomData,
    mem,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};

use crate::{MemError, Region, Tag};

pub struct Column<T> {
    values: Region,
    ready: Region,
    claimed: AtomicUsize,
    max: usize,
    _owns: PhantomData<T>,
}

// SAFETY: a value is written once by the pusher that claimed its index, before its ready bit
// is set with release, and read only after the bit is seen with acquire; `T: Send` lets a value
// made on one thread be dropped on another, `T: Sync` lets every thread read it.
unsafe impl<T: Send + Sync> Sync for Column<T> {}
// SAFETY: the column owns its values as a `Vec` does.
unsafe impl<T: Send> Send for Column<T> {}

const WORD: usize = mem::size_of::<AtomicU64>();

impl<T> Column<T> {
    const SIZE: usize = {
        assert!(mem::size_of::<T>() != 0, "a column of a zero-sized type holds nothing");
        assert!(mem::align_of::<T>() <= 4096, "a column's values align to at most a page");
        mem::size_of::<T>()
    };

    /// A column of at most `max` values for `tag` (at most `u32::MAX`), on huge pages where
    /// the kernel offers them.
    pub fn new(max: usize, tag: Tag) -> Result<Column<T>, MemError> {
        let max = max.min(u32::MAX as usize);
        let bytes = max.checked_mul(Self::SIZE).ok_or(MemError::TooLarge {
            tag,
            what: "a column's length",
            wanted: max,
        })?;
        Ok(Column {
            values: Region::reserve_huge(bytes, tag)?,
            ready: Region::reserve(max.div_ceil(64) * WORD, tag)?,
            claimed: AtomicUsize::new(0),
            max,
            _owns: PhantomData,
        })
    }

    pub fn max_len(&self) -> usize {
        self.max
    }

    /// Indices claimed so far; a pusher between its claim and its publish has one of them, not
    /// yet readable.
    pub fn len(&self) -> usize {
        self.claimed.load(Ordering::Relaxed).min(self.max)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn word(&self, index: usize) -> Option<&AtomicU64> {
        let offset = (index / 64) * WORD;
        if offset + WORD > self.ready.committed() {
            return None;
        }
        // SAFETY: the word is committed (checked just now), aligned, and committed memory is
        // zeroed by the kernel, a valid `AtomicU64`; it lives as long as `self`.
        Some(unsafe { self.ready.at(offset).cast::<AtomicU64>().as_ref() })
    }

    /// Appends `value`; returns its index, readable by every thread from now on.
    pub fn push(&self, value: T) -> Result<u32, MemError> {
        let index = self.claimed.fetch_add(1, Ordering::Relaxed);
        if index >= self.max {
            self.claimed.fetch_min(self.max, Ordering::Relaxed);
            return Err(MemError::Exhausted {
                tag: self.values.tag(),
                reserved: self.values.reserved(),
                wanted: (index + 1) * Self::SIZE,
            });
        }
        self.values.ensure((index + 1) * Self::SIZE)?;
        self.ready.ensure((index / 64 + 1) * WORD)?;
        let slot = self.values.at(index * Self::SIZE).cast::<T>();
        // SAFETY: the slot is committed, aligned, and claimed by this thread alone (the
        // `fetch_add`); no reader reaches it before its ready bit is set below.
        unsafe { slot.write(value) };
        let Some(word) = self.word(index) else {
            unreachable!("the ready word was committed above")
        };
        // Release: the value's bytes are seen by a reader that sees the bit.
        word.fetch_or(1 << (index % 64), Ordering::Release);
        Ok(index as u32)
    }

    /// The value at `index` once it is published.
    #[inline]
    pub fn get(&self, index: u32) -> Option<&T> {
        let index = index as usize;
        let word = self.word(index)?;
        if word.load(Ordering::Acquire) & (1 << (index % 64)) == 0 {
            return None;
        }
        // SAFETY: the ready bit is set, so the value was written whole before it (release,
        // acquire above) and is never written again; it lives as long as `self`.
        Some(unsafe { self.values.at(index * Self::SIZE).cast::<T>().as_ref() })
    }

    /// Every published value with its index, in index order.
    pub fn iter(&self) -> impl Iterator<Item = (u32, &T)> {
        (0..self.len() as u32).filter_map(|index| Some((index, self.get(index)?)))
    }
}

impl<T> Drop for Column<T> {
    fn drop(&mut self) {
        if !mem::needs_drop::<T>() {
            return;
        }
        for index in 0..self.len() {
            if self.get(index as u32).is_some() {
                let slot = self.values.at(index * Self::SIZE).cast::<T>();
                // SAFETY: `&mut self`: no reader is left, and a published value is dropped once.
                unsafe { slot.drop_in_place() };
            }
        }
    }
}

impl<T: fmt::Debug> fmt::Debug for Column<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

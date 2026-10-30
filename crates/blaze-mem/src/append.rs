//! [`AppendVec<T>`]: a column one writer appends to while any number of threads read it with
//! no lock (docs/v2/plan/design/concurrency.md "no lock and no shared write on any read
//! path"). The writer writes the value in place and publishes the new length with one release
//! store; a reader loads the length with acquire and indexes from a base that never moves.
//!
//! Unlike [`Column`](crate::Column), whose pushers claim indices at once and publish each value
//! by its own ready bit, the length here is the whole truth, so [`AppendVec::as_slice`] is the
//! published prefix in one load: for tables a scan reads whole (directory tables, path lists).
//!
//! Cost per append: an uncontended swap and store of the writer flag, the value's store and one
//! release store of the length; a commit system call per quarter of growth. Per read: one
//! acquire load and a bounds compare.

use std::{
    fmt,
    marker::PhantomData,
    mem, slice,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use crate::{MemError, Region, Tag, error::fail};

pub struct AppendVec<T> {
    region: Region,
    /// Elements published: every one below it is written and never written again.
    len: AtomicUsize,
    /// Set while an append runs, so a second writer at once stops instead of racing.
    writing: AtomicBool,
    max: usize,
    _owns: PhantomData<T>,
}

// SAFETY: an element is written once, by the one writer (`writing`), before the release store
// of a length that covers it, and read only below a length loaded with acquire; `T: Sync` lets
// every thread read it, `T: Send` lets the thread that drops the column drop it.
unsafe impl<T: Send + Sync> Sync for AppendVec<T> {}
// SAFETY: the column owns its values as a `Vec` does.
unsafe impl<T: Send> Send for AppendVec<T> {}

impl<T> AppendVec<T> {
    const SIZE: usize = {
        assert!(mem::size_of::<T>() != 0, "an AppendVec of a zero-sized type holds nothing");
        assert!(mem::align_of::<T>() <= 4096, "an AppendVec's values align to at most a page");
        mem::size_of::<T>()
    };

    /// A column of at most `max` values (at most `u32::MAX`) for `tag`, committing none.
    pub fn new(max: usize, tag: Tag) -> Result<AppendVec<T>, MemError> {
        let max = max.min(u32::MAX as usize);
        AppendVec::over(Region::reserve(AppendVec::<T>::bytes(max, tag)?, tag)?, max)
    }

    /// As [`AppendVec::new`], on huge pages where the kernel offers them.
    pub fn new_huge(max: usize, tag: Tag) -> Result<AppendVec<T>, MemError> {
        let max = max.min(u32::MAX as usize);
        AppendVec::over(Region::reserve_huge(AppendVec::<T>::bytes(max, tag)?, tag)?, max)
    }

    fn bytes(max: usize, tag: Tag) -> Result<usize, MemError> {
        max.checked_mul(Self::SIZE).ok_or(MemError::TooLarge {
            tag,
            what: "a column's length",
            wanted: max,
        })
    }

    fn over(region: Region, max: usize) -> Result<AppendVec<T>, MemError> {
        Ok(AppendVec {
            region,
            len: AtomicUsize::new(0),
            writing: AtomicBool::new(false),
            max,
            _owns: PhantomData,
        })
    }

    pub fn tag(&self) -> Tag {
        self.region.tag()
    }

    pub fn max_len(&self) -> usize {
        self.max
    }

    /// Elements published.
    #[inline]
    pub fn len(&self) -> u32 {
        self.len.load(Ordering::Acquire) as u32
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Appends `value` and returns its index, readable by every thread from now on. One writer
    /// at a time: the caller serializes its appends (its own lock or a single owner); a second
    /// append while one runs stops.
    #[track_caller]
    pub fn try_append(&self, value: T) -> Result<u32, MemError> {
        assert!(
            !self.writing.swap(true, Ordering::Acquire),
            "two threads appended to one AppendVec at once; its appends must be serialized"
        );
        struct Done<'a>(&'a AtomicBool);
        impl Drop for Done<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let _done = Done(&self.writing);
        // Only the writer changes `len`, and the flag's acquire orders this after the last
        // writer's store.
        let len = self.len.load(Ordering::Relaxed);
        if len >= self.max {
            return Err(MemError::Exhausted {
                tag: self.tag(),
                reserved: self.region.reserved(),
                wanted: (len + 1) * Self::SIZE,
            });
        }
        self.region.ensure((len + 1) * Self::SIZE)?;
        let slot = self.region.at(len * Self::SIZE).cast::<T>();
        // SAFETY: the slot is committed (just ensured), aligned (a page-aligned base plus a
        // multiple of `size_of::<T>()`), past the published length so no reader reaches it,
        // and written by this writer alone (`writing`).
        unsafe { slot.write(value) };
        // Release: the value's bytes are seen by a reader that sees the length.
        self.len.store(len + 1, Ordering::Release);
        Ok(len as u32)
    }

    /// Appends `value` and returns its index; stops when the reservation is used up or memory
    /// cannot be had.
    #[inline]
    #[track_caller]
    pub fn append(&self, value: T) -> u32 {
        match self.try_append(value) {
            Ok(index) => index,
            Err(error) => fail(error),
        }
    }

    /// The published elements.
    #[inline]
    pub fn as_slice(&self) -> &[T] {
        let len = self.len.load(Ordering::Acquire);
        // SAFETY: the first `len` slots were written before the release store of `len` this
        // load synchronized with, are never written again through `&self`, and stay committed
        // for the column's life; the base is not null and aligned.
        unsafe { slice::from_raw_parts(self.region.base().as_ptr().cast::<T>(), len) }
    }

    /// The element at `index`, once published.
    #[inline]
    pub fn get(&self, index: u32) -> Option<&T> {
        self.as_slice().get(index as usize)
    }

    pub fn iter(&self) -> slice::Iter<'_, T> {
        self.as_slice().iter()
    }

    /// The elements, to change in place: `&mut self` says no reader is left.
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        let len = *self.len.get_mut();
        // SAFETY: as for `as_slice`, and `&mut self` makes the borrow unique.
        unsafe { slice::from_raw_parts_mut(self.region.base().as_ptr().cast::<T>(), len) }
    }
}

impl<T> Drop for AppendVec<T> {
    fn drop(&mut self) {
        if mem::needs_drop::<T>() {
            let values: *mut [T] = self.as_mut_slice();
            // SAFETY: the published elements are initialized, owned by the column and dropped
            // once: it is going away.
            unsafe { values.drop_in_place() };
        }
    }
}

impl<T: fmt::Debug> fmt::Debug for AppendVec<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.as_slice()).finish()
    }
}

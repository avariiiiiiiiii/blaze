//! A reserved region: address space taken once, pages committed as the data grows, never
//! moved (docs/v2/plan/design/memory.md "Virtual memory reserved up front").
//!
//! Growing commits the next pages in place, so every address and `u32` offset into the region
//! stays valid for its life and nothing is ever copied to grow. Commits are geometric (a
//! quarter of what is committed, at least a granule, at most 64 MB), so a region that grows to
//! `n` bytes makes `O(log n)` system calls before it reaches 64 MB and one per 64 MB after.

use std::{io, ptr::NonNull, sync::atomic::AtomicUsize, sync::atomic::Ordering};

use crate::{MemError, Tag, os, tag};

/// The largest single commit: past it a region grows 64 MB per system call.
const MAX_STEP: usize = 64 << 20;

/// A range of reserved address space whose prefix `[base, base + committed)` is readable and
/// writable. Any thread may commit more ([`Region::ensure`]); giving pages back takes `&mut`.
pub struct Region {
    base: NonNull<u8>,
    reserved: usize,
    committed: AtomicUsize,
    granule: usize,
    huge: bool,
    tag: Tag,
}

// SAFETY: a region is an address range and counters; commits from any thread are atomic and
// idempotent (`ensure`), and what lives in the range is the owner's to synchronize.
unsafe impl Send for Region {}
// SAFETY: as for `Send`: `&Region` only commits, which any number of threads may do at once.
unsafe impl Sync for Region {}

impl Region {
    /// Reserves `bytes` (rounded up to whole pages) for `tag`, committing nothing.
    pub fn reserve(bytes: usize, tag: Tag) -> Result<Region, MemError> {
        Region::with(bytes, false, tag)
    }

    /// As [`Region::reserve`], backed by huge pages where the kernel offers them (Linux with
    /// transparent huge pages): for the hot regions (frames, interners, memo tables). It
    /// commits 2 MB at a time.
    pub fn reserve_huge(bytes: usize, tag: Tag) -> Result<Region, MemError> {
        Region::with(bytes, true, tag)
    }

    fn with(bytes: usize, huge: bool, tag: Tag) -> Result<Region, MemError> {
        let page = os::page_size();
        let granule = if huge { crate::HUGE_PAGE.max(page) } else { page.max(64 << 10) };
        let reserved = crate::align::checked_align_up(bytes.max(1), granule).ok_or(MemError::TooLarge {
            tag,
            what: "a reservation",
            wanted: bytes,
        })?;
        let base = os::reserve(reserved, huge).map_err(|os| MemError::Reserve {
            tag,
            bytes: reserved,
            os,
        })?;
        Ok(Region { base, reserved, committed: AtomicUsize::new(0), granule, huge, tag })
    }

    pub fn base(&self) -> NonNull<u8> {
        self.base
    }

    /// The address `offset` bytes into the reservation (`offset <= reserved`).
    #[inline]
    pub fn at(&self, offset: usize) -> NonNull<u8> {
        debug_assert!(offset <= self.reserved);
        // SAFETY: `base` is not null and `base + offset` stays inside the reservation, which
        // the kernel gave whole, so it does not wrap around the address space.
        unsafe { NonNull::new_unchecked(self.base.as_ptr().wrapping_add(offset)) }
    }

    pub fn reserved(&self) -> usize {
        self.reserved
    }

    /// Bytes from the base that are readable and writable now.
    pub fn committed(&self) -> usize {
        self.committed.load(Ordering::Acquire)
    }

    pub fn tag(&self) -> Tag {
        self.tag
    }

    pub fn is_huge(&self) -> bool {
        self.huge
    }

    /// Makes at least `[base, base + len)` readable and writable. Lock-free: threads that
    /// grow at once may commit overlapping pages, which is harmless, and each byte is counted
    /// once. Errs when `len` passes the reservation or the kernel refuses.
    #[inline]
    pub fn ensure(&self, len: usize) -> Result<(), MemError> {
        if len <= self.committed.load(Ordering::Acquire) {
            return Ok(());
        }
        self.grow(len)
    }

    #[cold]
    fn grow(&self, len: usize) -> Result<(), MemError> {
        if len > self.reserved {
            return Err(MemError::Exhausted { tag: self.tag, reserved: self.reserved, wanted: len });
        }
        let from = self.committed.load(Ordering::Acquire);
        if len <= from {
            return Ok(());
        }
        let step = (from / 4).clamp(self.granule, MAX_STEP.max(self.granule));
        let target = crate::align::checked_align_up(len.max(from + step), self.granule)
            .unwrap_or(self.reserved)
            .min(self.reserved);
        let start = crate::align::align_down(from, self.granule);
        // SAFETY: `[start, target)` is granule-aligned (so page-aligned) and inside the
        // reservation (`target <= reserved`), which lives as long as `self`.
        unsafe { os::commit(self.at(start), target - start) }.map_err(|os| {
            MemError::Commit { tag: self.tag, bytes: target - start, os }
        })?;
        // Release: what the commit made accessible is ordered before a reader that sees the
        // new length.
        let before = self.committed.fetch_max(target, Ordering::AcqRel);
        if target > before {
            tag::add(self.tag, target - before);
            tag::committed(target - before);
        }
        Ok(())
    }

    /// Gives every page past the first `keep` bytes (rounded up to a granule) back to the
    /// kernel: what a frame or table does after a burst, so resident memory returns to the
    /// working set.
    pub fn decommit_after(&mut self, keep: usize) -> io::Result<()> {
        let keep = crate::align::checked_align_up(keep, self.granule)
            .unwrap_or(self.reserved)
            .min(self.reserved);
        let committed = *self.committed.get_mut();
        if keep >= committed {
            return Ok(());
        }
        // SAFETY: `[keep, committed)` is granule-aligned, inside the reservation, and `&mut
        // self` says nothing borrows the region's memory.
        unsafe { os::decommit(self.at(keep), committed - keep, self.huge) }?;
        *self.committed.get_mut() = keep;
        tag::sub(self.tag, committed - keep);
        tag::decommitted(committed - keep);
        Ok(())
    }

    /// Lets the kernel reclaim the pages of `[from, from + len)` (rounded inward to pages)
    /// while they stay committed; their contents become unspecified.
    ///
    /// # Safety
    ///
    /// Nothing reads the range's contents again before writing them.
    pub unsafe fn purge(&self, from: usize, len: usize) {
        let page = os::page_size();
        let Some(start) = crate::align::checked_align_up(from, page) else { return };
        let end = crate::align::align_down(from.saturating_add(len), page).min(self.committed());
        if end > start {
            // SAFETY: `[start, end)` is page-aligned, committed, and the caller says nothing
            // holds data in it.
            unsafe { os::purge(self.at(start), end - start) };
        }
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        let committed = *self.committed.get_mut();
        tag::sub(self.tag, committed);
        tag::decommitted(committed);
        // SAFETY: `base` and `reserved` are this reservation's own, and the region is gone, so
        // nothing reaches its memory.
        unsafe { os::release(self.base, self.reserved) };
    }
}

impl std::fmt::Debug for Region {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Region")
            .field("tag", &self.tag)
            .field("reserved", &self.reserved)
            .field("committed", &self.committed())
            .field("huge", &self.huge)
            .finish()
    }
}

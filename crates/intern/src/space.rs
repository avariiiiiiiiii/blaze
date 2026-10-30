//! The reserved region every interner of the process keeps its values and probe tables in
//! (docs/v2/plan/design/memory.md, "tables, interners and columns"): 64 GiB of address space
//! reserved on first use, committed as it fills, never moved, reused or freed, its pages
//! counted under [`Tag::Intern`] and backed by huge pages where the kernel offers them.
//!
//! Space is claimed with one `fetch_add` on a cursor and one [`Region::ensure`]: a shard's bump
//! allocator claims a chunk at a time (2 MiB once warm), a probe table all its groups at once.
//! The kernel hands out committed pages zeroed and no byte is claimed twice, so claimed space
//! reads as zeroes: a probe table needs no initializing.
//!
//! Cost: a claim is one `fetch_add`, one acquire load of the committed length and, a quarter of
//! growth at a time, a commit system call.

use std::{
    alloc::Layout,
    ptr::NonNull,
    sync::{
        OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
};

use blaze_mem::{Region, Tag};

/// Address space for every interned value and probe table of the process.
const RESERVED: usize = 64 << 30;

/// Every claim starts and ends on a cache line, so two claims never share one.
pub(crate) const LINE: usize = 64;

struct Space {
    region: Region,
    /// Bytes from the region's base claimed so far.
    cursor: AtomicUsize,
}

fn space() -> &'static Space {
    static SPACE: OnceLock<Space> = OnceLock::new();
    SPACE.get_or_init(|| Space {
        region: Region::reserve_huge(RESERVED, Tag::Intern)
            .unwrap_or_else(|error| panic!("intern: {error}")),
        cursor: AtomicUsize::new(0),
    })
}

/// `layout.size()` fresh bytes (at least one) aligned to `layout.align()`, all zero, never freed.
/// Stops with the region's error when its reservation is used up or the kernel refuses to
/// commit (no-fallback).
#[track_caller]
pub(crate) fn claim(layout: Layout) -> NonNull<u8> {
    let space = space();
    let size = layout.size().max(1).next_multiple_of(LINE);
    let align = layout.align().max(LINE);
    // The base is page-aligned and the cursor moves by whole lines, so `align - LINE` bytes of
    // padding reach any alignment.
    let start = space.cursor.fetch_add(size + (align - LINE), Ordering::Relaxed);
    let base = space.region.base().as_ptr().addr();
    let at = (base + start).next_multiple_of(align) - base;
    if let Err(error) = space.region.ensure(at + size) {
        panic!("intern: {error}");
    }
    space.region.at(at)
}

/// `len` `T`s in fresh claimed space, every byte zero, never freed.
///
/// # Safety
///
/// All zero bytes must be a valid `T`.
pub(crate) unsafe fn zeroed<T>(len: usize) -> &'static [T] {
    let at = claim(Layout::array::<T>(len).expect("an interner table's layout")).cast::<T>();
    // SAFETY: `claim` gives `len * size_of::<T>()` committed bytes aligned for `T`, all zero
    // (fresh pages, claimed by no one before) and never freed; zero bytes are a `T` (the
    // caller's precondition), so every element is initialized.
    unsafe { std::slice::from_raw_parts(at.as_ptr(), len) }
}

/// `value` moved into fresh claimed space, never dropped or freed: `Box::leak` without the
/// global allocator.
pub(crate) fn place<T>(value: T) -> &'static T {
    let at = claim(Layout::new::<T>()).cast::<T>();
    // SAFETY: `at` is committed, aligned for `T`, room for one, claimed by this call alone and
    // never freed; it is written here before the reference that reads it exists.
    unsafe {
        at.write(value);
        at.as_ref()
    }
}

#[cfg(all(test, not(blaze_check)))]
mod tests {
    use super::*;

    /// Claims from many threads at once never overlap, keep their alignment and read as zero.
    #[test]
    fn claims_are_disjoint_aligned_and_zero() {
        let taken: Vec<(usize, usize)> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..8usize)
                .map(|worker| {
                    scope.spawn(move || {
                        (0..2_000usize)
                            .map(|n| {
                                let size = (n * 37 + worker) % 5_000;
                                let align = 1 << ((n + worker) % 13);
                                let at = claim(Layout::from_size_align(size, align).unwrap());
                                assert_eq!(at.as_ptr().addr() % align.max(LINE), 0);
                                // SAFETY: `claim` gave `size.max(1)` committed bytes.
                                let bytes = unsafe { std::slice::from_raw_parts(at.as_ptr(), size) };
                                assert!(bytes.iter().all(|&byte| byte == 0), "fresh space is zero");
                                // SAFETY: as above; the bytes are this claim's alone.
                                unsafe { at.as_ptr().write_bytes(0xa5, size.max(1)) };
                                (at.as_ptr().addr(), size.max(1))
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            workers.into_iter().flat_map(|it| it.join().unwrap()).collect()
        });
        let mut taken = taken;
        taken.sort_unstable();
        for pair in taken.windows(2) {
            assert!(pair[0].0 + pair[0].1 <= pair[1].0, "{pair:?}");
        }
    }

    /// A warm claim allocates nothing from the global allocator.
    #[test]
    fn claims_allocate_nothing() {
        claim(Layout::new::<u64>());
        blaze_alloc_count::assert_at_most(0, "claiming interner space", || {
            for n in 1..10_000 {
                claim(Layout::from_size_align(n % 3_000, 8).unwrap());
            }
            place([7u64; 4]);
        });
    }
}

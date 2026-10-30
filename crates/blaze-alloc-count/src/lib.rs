//! A counting global allocator for allocation-budget tests.
//!
//! A hot crate's tests install it once:
//!
//! ```ignore
//! #[cfg(test)]
//! #[global_allocator]
//! static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;
//! ```
//!
//! and then hold a hot path to a budget:
//!
//! ```ignore
//! let (_, used) = blaze_alloc_count::measure(|| db.get(key));
//! assert_eq!(used.allocations, 0, "a memo hit allocates nothing");
//! ```
//!
//! Counts are per thread, so tests running in parallel don't see each other's allocations.
//! Work a closure hands to other threads is not counted; measure on the thread that does it.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};

/// The system allocator, counting each thread's allocations while [`measure`] runs on it.
pub struct Counting;

/// What a measured closure allocated.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Used {
    /// Calls to `alloc`, `alloc_zeroed` and `realloc`.
    pub allocations: u64,
    /// Bytes asked for by those calls (for `realloc`, the new size).
    pub bytes: u64,
}

thread_local! {
    static ON: Cell<bool> = const { Cell::new(false) };
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
    static BYTES: Cell<u64> = const { Cell::new(0) };
}

#[inline]
fn record(size: usize) {
    // `try_with`: a thread being torn down may allocate after its locals are gone.
    if ON.try_with(Cell::get).unwrap_or(false) {
        let _ = ALLOCATIONS.try_with(|c| c.set(c.get() + 1));
        let _ = BYTES.try_with(|c| c.set(c.get() + size as u64));
    }
}

// SAFETY: every call forwards to `System` with the same arguments, which upholds
// `GlobalAlloc`'s contract; the counting touches only this thread's `Cell`s and never
// allocates.
unsafe impl GlobalAlloc for Counting {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        // SAFETY: the caller's `layout` obligations pass through unchanged.
        unsafe { System.alloc(layout) }
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        // SAFETY: as for `alloc`.
        unsafe { System.alloc_zeroed(layout) }
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from this allocator, which is `System`, with `layout`.
        unsafe { System.dealloc(ptr, layout) }
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record(new_size);
        // SAFETY: `ptr` came from `System` with `layout`; the caller upholds `new_size`'s rules.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

/// Runs `f` and returns what it allocated on this thread. Measurements nest: an inner one
/// counts only its own closure, and the outer one includes it.
pub fn measure<R>(f: impl FnOnce() -> R) -> (R, Used) {
    let was_on = ON.with(|on| on.replace(true));
    let before = (ALLOCATIONS.with(Cell::get), BYTES.with(Cell::get));
    let result = f();
    let after = (ALLOCATIONS.with(Cell::get), BYTES.with(Cell::get));
    ON.with(|on| on.set(was_on));
    (result, Used { allocations: after.0 - before.0, bytes: after.1 - before.1 })
}

/// Asserts `f` makes at most `max` allocations on this thread, naming `what` when it doesn't.
#[track_caller]
pub fn assert_at_most<R>(max: u64, what: &str, f: impl FnOnce() -> R) -> R {
    let (result, used) = measure(f);
    assert!(
        used.allocations <= max,
        "{what}: {} allocations ({} bytes), budget {max}",
        used.allocations,
        used.bytes
    );
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[global_allocator]
    static ALLOC: Counting = Counting;

    #[test]
    fn counts_only_inside_measure() {
        let _outside = vec![1u8; 10];
        let (_, used) = measure(|| ());
        assert_eq!(used, Used::default());
    }

    #[test]
    fn counts_allocations_and_bytes() {
        let (v, used) = measure(|| Vec::<u8>::with_capacity(64));
        assert_eq!(v.capacity(), 64);
        assert_eq!(used.allocations, 1);
        assert_eq!(used.bytes, 64);
    }

    #[test]
    fn nests() {
        let ((_, inner), outer) = measure(|| {
            let _a = Box::new(1u64);
            measure(|| Box::new(2u64))
        });
        assert_eq!(inner.allocations, 1);
        assert_eq!(outer.allocations, 2);
    }

    #[test]
    #[should_panic(expected = "budget 0")]
    fn budget_fails_loudly() {
        assert_at_most(0, "no allocation", || Box::new(3u8));
    }
}

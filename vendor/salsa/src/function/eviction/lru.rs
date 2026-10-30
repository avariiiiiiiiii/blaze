//! Least Recently Used (LRU) eviction policy.
//!
//! When more values are memoized than the capacity, the values of the memos verified longest
//! ago are evicted at the start of each new revision.

use std::num::NonZeroUsize;

use super::{EvictionPolicy, HasCapacity};

/// Least Recently Used eviction policy.
///
/// "Used" is the revision a memo was last verified in: a read verifies the memo before it
/// returns, so a revision's reads keep their values over the values of older revisions. Memos
/// verified in the same revision are evicted in slot order.
pub struct Lru {
    capacity: Option<NonZeroUsize>,
}

impl EvictionPolicy for Lru {
    fn new(cap: usize) -> Self {
        Self {
            capacity: NonZeroUsize::new(cap),
        }
    }

    fn set_capacity(&mut self, capacity: usize) {
        self.capacity = NonZeroUsize::new(capacity);
    }

    #[inline]
    fn capacity(&self) -> Option<NonZeroUsize> {
        self.capacity
    }
}

impl HasCapacity for Lru {}

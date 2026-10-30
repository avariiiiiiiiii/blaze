//! Pluggable cache eviction strategies for memoized function values.
//!
//! This module provides the [`EvictionPolicy`] trait that allows different
//! eviction strategies to be used for salsa tracked functions.

mod lru;
mod noop;

use std::num::NonZeroUsize;

pub use lru::Lru;
pub use noop::NoopEviction;

/// Trait for cache eviction strategies.
///
/// Implementations control when memoized values are evicted from the cache.
/// The eviction policy is selected at compile time via the `Configuration` trait.
///
/// A policy holds no per-read state: a read writes no shared memory. Values are ranked by the
/// revision their memo was last verified in, once per revision, with `&mut` (see
/// `IngredientImpl::evict_least_recently_verified`).
pub trait EvictionPolicy: Send + Sync {
    /// Create a new eviction policy with the given capacity.
    fn new(capacity: usize) -> Self;

    /// Set the maximum capacity.
    fn set_capacity(&mut self, capacity: usize);

    /// How many values may be kept across a new revision, or `None` for no limit.
    fn capacity(&self) -> Option<NonZeroUsize>;
}

/// Marker trait for eviction policies that have a configurable capacity.
///
/// This trait is used to conditionally generate the `set_lru_capacity` method
/// on tracked functions. Only policies that implement this trait will expose
/// runtime capacity configuration.
pub trait HasCapacity: EvictionPolicy {}

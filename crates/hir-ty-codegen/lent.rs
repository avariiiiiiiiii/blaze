//! A list that lends what it holds for as long as it lives, while it keeps growing: how a
//! build keeps the layouts it looked up and hands out `&Layout`s, with no refcount write per
//! lookup (an `Arc` clone per lookup writes a line every thread that holds the layout reads).
//!
//! The one unsafe block of the module is [`Lent::alloc`]'s; its API is safe.

use std::{cell::RefCell, ops::Deref, sync::Arc};

/// Values lent for as long as the list lives. Each is behind a pointer of its own, so the
/// list growing moves the pointers, never what they point at. 24 bytes plus one pointer per
/// value; nothing is taken out before the list is dropped.
pub(crate) struct Lent<P>(RefCell<Vec<P>>);

impl<P> Default for Lent<P> {
    fn default() -> Self {
        Lent(RefCell::new(Vec::new()))
    }
}

/// A pointer whose target stays where it is when the pointer moves.
///
/// # Safety
///
/// Moving a value of the type (into a `Vec`, or by the `Vec` growing) must leave `&*value`
/// pointing at the same live target, valid until the value is dropped.
pub(crate) unsafe trait Stable: Deref {}

// SAFETY: an `Arc`'s target is in its own heap allocation, which moving the `Arc` leaves in
// place and which lives at least as long as this `Arc`.
unsafe impl<T> Stable for Arc<T> {}

// SAFETY: a `Box`'s target is in its own heap allocation, which moving the `Box` leaves in
// place and which lives until the `Box` is dropped.
unsafe impl<T> Stable for Box<T> {}

impl<P: Stable> Lent<P> {
    /// Keeps `value` and lends its target for as long as the list lives.
    pub(crate) fn alloc(&self, value: P) -> &P::Target {
        let mut list = self.0.borrow_mut();
        list.push(value);
        let at: *const P::Target = &**list.last().expect("a value was just pushed");
        drop(list);
        // SAFETY: `at` is taken from the pointer in its final place in the list (not before
        // the move into it, which would retag a `Box` and invalidate it under Stacked
        // Borrows). `P` is `Stable`, so later growth of the list moves the pointer and not its
        // target, and nothing is taken out of the list before it is dropped, which `&self`
        // outlives. Taken out early, the target would be freed under a live reference.
        unsafe { &*at }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What is lent is the value kept, the same as a safe list read by index gives, however
    /// much the list grows after it: seeded values, enough for many reallocations.
    #[test]
    fn a_lent_value_is_the_one_kept_whatever_the_list_does_after() {
        let boxes: Lent<Box<u64>> = Lent::default();
        let arcs: Lent<Arc<u64>> = Lent::default();
        let mut reference = Vec::new();
        let mut lent = Vec::new();
        let mut given = Vec::new();
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        for _ in 0..5000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            reference.push(seed);
            lent.push(boxes.alloc(Box::new(seed)));
            let arc = Arc::new(seed);
            given.push(arc.clone());
            let from_arc = arcs.alloc(arc);
            assert!(std::ptr::eq(from_arc, &**given.last().unwrap()));
        }
        assert!(lent.iter().map(|it| **it).eq(reference.iter().copied()));
        assert!(given.iter().map(|it| **it).eq(reference.iter().copied()));
    }
}

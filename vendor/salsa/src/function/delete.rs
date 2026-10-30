use std::ptr::NonNull;

use crate::function::Configuration;
use crate::function::memo::Memo;

/// Stores the list of memos that have been deleted so they can be freed
/// once the next revision starts. See the comment on the field
/// `deleted_entries` of [`IngredientImpl`](super::IngredientImpl) for more details.
pub(super) struct DeletedEntries<C: Configuration> {
    memos: boxcar::Vec<SharedBox<Memo<C>>>,
}

// SAFETY: A `SharedBox` uniquely owns its allocation (the contract of `DeletedEntries::push`), so
// it is a `Box<T>` and sends under the same bound.
unsafe impl<T: Send> Send for SharedBox<T> {}
// SAFETY: `&SharedBox<T>` gives no access to the `T` at all (only `Drop`, through `&mut`, touches
// it), so sharing it is at most as permissive as sharing a `Box<T>`.
unsafe impl<T: Sync> Sync for SharedBox<T> {}

impl<C: Configuration> Default for DeletedEntries<C> {
    fn default() -> Self {
        Self {
            memos: Default::default(),
        }
    }
}

impl<C: Configuration> DeletedEntries<C> {
    /// # Safety
    ///
    /// The memo must come from `Box::leak` (or `Box::into_raw`), be owned by nothing else, and be
    /// safe to free when the `DeletedEntries` list is cleared or dropped.
    pub(super) unsafe fn push(&self, memo: NonNull<Memo<C>>) {
        self.memos.push(SharedBox(memo));
    }

    /// Free all deleted memos, keeping the list available for reuse.
    pub(super) fn clear(&mut self) {
        self.memos.clear();
    }
}

/// A wrapper around `NonNull` that frees the allocation when it is dropped.
struct SharedBox<T>(NonNull<T>);

impl<T> Drop for SharedBox<T> {
    fn drop(&mut self) {
        // SAFETY: Guaranteed by the caller of `DeletedEntries::push`.
        unsafe { drop(Box::from_raw(self.0.as_ptr())) };
    }
}

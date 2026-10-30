//! `UnsafeCell` with loom's `with`/`with_mut`: in a model each is a tracked non-atomic read or
//! write, and one that happens-before does not order with another thread's write (or a write
//! with another thread's read) is a data race, a finding. Accesses are not schedule points:
//! whether two race depends only on the clocks the atomics gave each thread.

use std::{panic::Location, sync::atomic::AtomicU64 as Id};

use crate::rt::{self, At, Object, memory};

/// `std::cell::UnsafeCell`, reached through [`UnsafeCell::with`] and [`UnsafeCell::with_mut`]
/// so the model sees each access. A crate's `std` twin of it (its `sync.rs`) has the same two
/// methods, calling the closure on `get()`.
pub struct UnsafeCell<T: ?Sized> {
    id: Id,
    data: std::cell::UnsafeCell<T>,
}

impl<T> UnsafeCell<T> {
    pub const fn new(value: T) -> UnsafeCell<T> {
        UnsafeCell { id: Id::new(0), data: std::cell::UnsafeCell::new(value) }
    }

    pub fn into_inner(self) -> T {
        self.data.into_inner()
    }
}

impl<T: ?Sized> UnsafeCell<T> {
    fn access(&self, at: At, write: bool) {
        let checked = rt::with(|rt| {
            let obj = rt.object(&self.id, |_| Object::Cell(memory::Cell::new()));
            if write { rt.cell_write(obj, at) } else { rt.cell_read(obj, at) }
        });
        if let Some(Err(found)) = checked {
            rt::stop(found);
        }
    }

    /// A read through `*const T`, for as long as `f` runs.
    #[track_caller]
    pub fn with<R>(&self, f: impl FnOnce(*const T) -> R) -> R {
        self.access(Location::caller(), false);
        f(self.data.get())
    }

    /// A write through `*mut T`, for as long as `f` runs.
    #[track_caller]
    pub fn with_mut<R>(&self, f: impl FnOnce(*mut T) -> R) -> R {
        self.access(Location::caller(), true);
        f(self.data.get())
    }

    #[track_caller]
    pub fn get_mut(&mut self) -> &mut T {
        self.access(Location::caller(), true);
        self.data.get_mut()
    }
}

impl<T: Default> Default for UnsafeCell<T> {
    fn default() -> UnsafeCell<T> {
        UnsafeCell::new(T::default())
    }
}

impl<T> From<T> for UnsafeCell<T> {
    fn from(value: T) -> UnsafeCell<T> {
        UnsafeCell::new(value)
    }
}

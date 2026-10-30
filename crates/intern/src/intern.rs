//! Interning of single values.
//!
//! A value is interned once, into its type's [`InternStorage`], and lives there unmoved for the
//! life of the process (see [`crate::arena`]): an [`Interned`] is a pointer to it, `Copy`, with
//! no reference count, and two are equal when they point at the same value. An [`InternedRef`]
//! is the same pointer with a lifetime, which only marks where a handle is meant to be used
//! (Salsa refuses to store one, as it has no `salsa::Update`); [`InternedRef::to_owned`] and
//! [`InternedRef::change_lifetime`] move between the two freely.
//!
//! Interned values are never freed: memory is bounded by the database's reachability GC
//! rebuilding what it keeps (docs/v2/plan/lanes/db.md).

use std::{
    alloc::Layout,
    fmt::{self, Debug, Display},
    hash::{BuildHasher, Hash, Hasher},
    marker::PhantomData,
    ops::Deref,
    ptr::{self, NonNull},
    sync::OnceLock,
};

use rustc_hash::FxBuildHasher;

use crate::arena::{Entry, Store};

pub struct Interned<T: Internable> {
    /// Points into `T::storage()`, which never moves or frees a value.
    ptr: &'static T,
}

impl<T: Internable> Interned<T> {
    #[inline]
    pub fn new(obj: T) -> Self {
        Self { ptr: intern(obj) }
    }

    /// Interns `obj`, as [`Interned::new`] does, handing out the handle with a lifetime.
    #[inline]
    pub fn new_gc<'a>(obj: T) -> InternedRef<'a, T> {
        Self::new(obj).as_ref_static()
    }

    /// # Safety
    ///
    /// The pointer should originate from an `Interned` or an `InternedRef`.
    #[inline]
    pub unsafe fn from_raw(ptr: *const T) -> Self {
        // SAFETY: Our precondition: it points at a value of `T::storage()`, live for the
        // process.
        Self { ptr: unsafe { &*ptr } }
    }

    #[inline]
    pub fn as_ref(&self) -> InternedRef<'_, T> {
        self.as_ref_static()
    }

    #[inline]
    fn as_ref_static<'a>(self) -> InternedRef<'a, T> {
        InternedRef { ptr: NonNull::from(self.ptr), _marker: PhantomData }
    }
}

/// The one copy of `obj` in `T::storage()`, interning it if it is not there.
#[inline]
fn intern<T: Internable>(obj: T) -> &'static T {
    let store = T::storage().get();
    let hash = FxBuildHasher.hash_one(&obj);
    // SAFETY: Every value of `T`'s store is a `T`, initialized before it was published.
    let same = |at: NonNull<u8>| unsafe { *at.cast::<T>().as_ref() == obj };
    let at = match store.entry(hash, same) {
        Entry::Found(at) => at,
        Entry::Vacant(vacant) => vacant.insert(Layout::new::<T>(), |to| {
            // SAFETY: `to` is fresh memory of `T`'s layout.
            unsafe { to.cast::<T>().write(obj) }
        }),
    };
    // SAFETY: A value of the store is never moved or freed, so it lives for `'static`.
    unsafe { at.cast::<T>().as_ref() }
}

impl<T: Internable> Clone for Interned<T> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: Internable> Copy for Interned<T> {}

/// Compares interned `Ref`s using pointer equality.
impl<T: Internable> PartialEq for Interned<T> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        ptr::eq(self.ptr, other.ptr)
    }
}

impl<T: Internable> Eq for Interned<T> {}

impl<T: Internable> Hash for Interned<T> {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_usize(ptr::from_ref(self.ptr).addr())
    }
}

impl<T: Internable> AsRef<T> for Interned<T> {
    #[inline]
    fn as_ref(&self) -> &T {
        self.ptr
    }
}

impl<T: Internable> Deref for Interned<T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.ptr
    }
}

impl<T: Debug + Internable> Debug for Interned<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        <T as Debug>::fmt(self.ptr, f)
    }
}

impl<T: Display + Internable> Display for Interned<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        <T as Display>::fmt(self.ptr, f)
    }
}

#[repr(transparent)]
pub struct InternedRef<'a, T> {
    /// # Invariant
    ///
    /// Points at a value of an [`InternStorage`], never moved or freed.
    ptr: NonNull<T>,
    _marker: PhantomData<&'a T>,
}

// SAFETY: An `InternedRef` is a shared reference to a value that lives for the process.
unsafe impl<T: Sync> Send for InternedRef<'_, T> {}
unsafe impl<T: Sync> Sync for InternedRef<'_, T> {}

impl<'a, T: Internable> InternedRef<'a, T> {
    #[inline]
    pub fn as_raw(self) -> *const T {
        self.ptr.as_ptr()
    }

    /// # Safety
    ///
    /// The pointer needs to originate from `Interned` or `InternedRef`.
    #[inline]
    pub unsafe fn from_raw(ptr: *const T) -> Self {
        // SAFETY: Our precondition: it is a value's pointer, so not null.
        Self { ptr: unsafe { NonNull::new_unchecked(ptr.cast_mut()) }, _marker: PhantomData }
    }

    #[inline]
    pub fn to_owned(self) -> Interned<T> {
        // SAFETY: `self.ptr`'s invariant: the value lives for the process.
        Interned { ptr: unsafe { self.ptr.as_ref() } }
    }

    #[inline]
    pub fn get(self) -> &'a T {
        // SAFETY: `self.ptr`'s invariant.
        unsafe { self.ptr.as_ref() }
    }

    /// Changes the attached lifetime: interned values live for the process, so the lifetime is
    /// only a lint against misuse, not a soundness check.
    #[inline]
    pub fn change_lifetime<'b>(self) -> InternedRef<'b, T> {
        InternedRef { ptr: self.ptr, _marker: PhantomData }
    }
}

impl<T> Clone for InternedRef<'_, T> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for InternedRef<'_, T> {}

impl<T: Hash> Hash for InternedRef<'_, T> {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_usize(self.ptr.as_ptr().addr());
    }
}

impl<T: PartialEq> PartialEq for InternedRef<'_, T> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.ptr == other.ptr
    }
}

impl<T: Eq> Eq for InternedRef<'_, T> {}

impl<T> Deref for InternedRef<'_, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &Self::Target {
        // SAFETY: `self.ptr`'s invariant.
        unsafe { self.ptr.as_ref() }
    }
}

impl<T: Debug> Debug for InternedRef<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(f)
    }
}

impl<T: Display> Display for InternedRef<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(f)
    }
}

pub struct InternStorage<T: ?Sized> {
    store: OnceLock<Store>,
    _of: PhantomData<fn() -> *const T>,
}

#[allow(
    clippy::new_without_default,
    reason = "this a const fn, so it can't be default yet. See <https://github.com/rust-lang/rust/issues/63065>"
)]
impl<T: ?Sized> InternStorage<T> {
    pub const fn new() -> Self {
        Self { store: OnceLock::new(), _of: PhantomData }
    }
}

impl<T: Internable + ?Sized> InternStorage<T> {
    pub(crate) fn get(&self) -> &Store {
        self.store.get_or_init(Store::new)
    }
}

pub trait Internable: Hash + Eq + Send + Sync + 'static {
    fn storage() -> &'static InternStorage<Self>;
}

/// Implements `Internable` for a given list of types, making them usable with `Interned`.
/// `gc;` is accepted and means the same: every interned value lives for the process.
#[macro_export]
#[doc(hidden)]
macro_rules! _impl_internable {
    ( gc; $($t:ty),+ $(,)? ) => {
        $crate::_impl_internable!($($t),+);
    };
    ( $($t:ty),+ $(,)? ) => { $(
        impl $crate::Internable for $t {
            fn storage() -> &'static $crate::InternStorage<Self> {
                static STORAGE: $crate::InternStorage<$t> = $crate::InternStorage::new();
                &STORAGE
            }
        }
    )+ };
}
pub use crate::_impl_internable as impl_internable;

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(PartialEq, Eq, Hash, Debug)]
    struct Name(u32);
    impl_internable!(Name);

    const _: () = assert!(size_of::<Interned<Name>>() == size_of::<usize>());
    const _: () = assert!(size_of::<InternedRef<'static, Name>>() == size_of::<usize>());

    /// A value interned from many threads at once is one value: every thread gets the handle
    /// interned before them, and values first interned by racing threads are one per value.
    #[test]
    fn interned_from_many_threads() {
        let held: Vec<_> = (0..64).map(|i| Interned::new(Name(i))).collect();
        std::thread::scope(|scope| {
            for _ in 0..16 {
                scope.spawn(|| {
                    for round in 0..20_000u32 {
                        let found = Interned::new(Name(round % 64));
                        assert!(found == held[(round % 64) as usize]);
                        let churned = Interned::new(Name(1000 + round % 7));
                        let again = Interned::new(Name(1000 + round % 7));
                        assert!(churned == again);
                    }
                });
            }
        });
        let after: Vec<_> = (0..64).map(|i| Interned::new(Name(i))).collect();
        assert!(after == held);
    }

    #[derive(PartialEq, Eq, Hash, Debug)]
    struct Racing(u64);
    impl_internable!(Racing);

    /// Threads interning overlapping new values in different orders get pointer-equal handles.
    #[test]
    fn new_values_race_to_one_handle() {
        const THREADS: u64 = 16;
        const VALUES: u64 = 20_000;
        let seen: Vec<Vec<Interned<Racing>>> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..THREADS)
                .map(|worker| {
                    scope.spawn(move || {
                        (0..VALUES)
                            .map(|n| Interned::new(Racing((n + worker * 1_237) % VALUES)))
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            workers.into_iter().map(|it| it.join().unwrap()).collect()
        });
        assert_eq!(<Racing as Internable>::storage().get().len(), VALUES as usize);
        for (worker, handles) in seen.iter().enumerate() {
            for (i, handle) in handles.iter().enumerate() {
                let n = (i as u64 + worker as u64 * 1_237) % VALUES;
                assert!(ptr::eq(&**handle, &*Interned::new(Racing(n))));
                assert_eq!(handle.0, n);
            }
        }
    }

    #[derive(PartialEq, Eq, Hash, Debug)]
    struct Text(String);
    impl_internable!(Text);

    /// Enough values to fill many chunks of every shard: the first handles still read their
    /// values and are still what interning them again gives.
    #[test]
    fn handles_stay_valid() {
        let early: Vec<_> = (0..256).map(|n| Interned::new(Text(format!("early {n}")))).collect();
        let addresses: Vec<_> = early.iter().map(|it| ptr::from_ref(&**it)).collect();
        for n in 0..200_000 {
            Interned::new(Text(format!("later {n}")));
        }
        for (n, (handle, &at)) in early.iter().zip(&addresses).enumerate() {
            assert_eq!(handle.0, format!("early {n}"));
            assert!(ptr::eq(&**handle, at));
            assert_eq!(Interned::new(Text(format!("early {n}"))), *handle);
        }
    }

    crate::impl_internable!(String);

    #[test]
    fn simple_interned() {
        let a = Interned::new("abc".to_owned());
        let b = Interned::new("abc".to_owned());
        assert_eq!(a, b);
        assert_eq!(a.as_ref(), b.as_ref());
        assert_eq!(a.as_ref(), a.as_ref());
        #[allow(clippy::clone_on_copy)]
        let cloned = a.clone();
        assert_eq!(a, cloned);
        assert_eq!(*a, "abc");
        assert_eq!(*b, "abc");
        assert_eq!(b.as_ref().to_owned(), a);
        let c = Interned::new("def".to_owned());
        assert_ne!(a, c);
        assert_ne!(b, c);
        assert_ne!(b.as_ref(), c.as_ref());
        assert_eq!(*c.as_ref(), "def");
        assert_eq!(format!("{a} {c:?}"), "abc \"def\"");
    }

    #[derive(Debug, PartialEq, Eq, Hash)]
    struct GcString(String);
    crate::impl_internable!(gc; GcString);

    #[test]
    fn new_gc_is_new() {
        let r = Interned::new_gc(GcString("abc".to_owned()));
        let owned = Interned::new(GcString("abc".to_owned()));
        assert_eq!(r.to_owned(), owned);
        assert_eq!(r, owned.as_ref());
        assert_eq!(r.get().0, "abc");
        let raw = r.as_raw();
        // SAFETY: The pointer is an `InternedRef`'s.
        let back = unsafe { InternedRef::<GcString>::from_raw(raw) };
        assert_eq!(back, r);
        // SAFETY: As above.
        assert_eq!(unsafe { Interned::from_raw(raw) }, owned);
        let longer: InternedRef<'static, GcString> = r.change_lifetime();
        assert_eq!(longer, r);
    }
}

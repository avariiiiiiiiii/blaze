//! Interning of slices, potentially with a header.
//!
//! See [`crate::intern`] for how interned values live. [`InternedSlice`] and
//! [`InternedSliceRef`] are essentially [`Interned<(Header, Box<[SliceType]>)>`][crate::Interned]
//! and [`InternedRef`][crate::InternedRef] with the same types, but more optimized: the header,
//! the length and the slice are stored once, together, and the pointer is thin.

use std::{
    alloc::Layout,
    fmt::{self, Debug},
    hash::{BuildHasher, Hash, Hasher},
    marker::PhantomData,
    ops::Deref,
    ptr::{self, NonNull},
    sync::OnceLock,
};

use rustc_hash::FxBuildHasher;

use crate::arena::{Entry, Store};

/// A header followed by a slice, laid out as one value.
#[derive(Debug, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct HeaderSlice<H, S: ?Sized> {
    pub header: H,
    pub slice: S,
}

/// A header with the length of the slice after it.
#[derive(Debug, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct HeaderWithLength<H> {
    pub header: H,
    pub length: usize,
}

type Pointee<T> = HeaderSlice<
    HeaderWithLength<<T as SliceInternable>::Header>,
    [<T as SliceInternable>::SliceType],
>;

pub struct InternedSlice<T: SliceInternable> {
    /// # Invariant
    ///
    /// Points at the start of a `Pointee<T>` of `T::storage()`, never moved or freed.
    ptr: NonNull<u8>,
    _marker: PhantomData<&'static T>,
}

// SAFETY: An `InternedSlice` is a shared reference to a header and a slice of `Send + Sync`
// types that live for the process.
unsafe impl<T: SliceInternable> Send for InternedSlice<T> {}
unsafe impl<T: SliceInternable> Sync for InternedSlice<T> {}

impl<T: SliceInternable> InternedSlice<T> {
    #[inline]
    pub fn from_header_and_slice<'a>(
        header: T::Header,
        slice: &[T::SliceType],
    ) -> InternedSliceRef<'a, T> {
        let store = T::storage().get();
        let hash = FxBuildHasher.hash_one((&header, slice));
        let same = |at: NonNull<u8>| {
            // SAFETY: Every value of `T`'s store is a published `Pointee<T>`.
            let other = unsafe { pointee::<T>(at) };
            other.header.header == header && other.slice == *slice
        };
        let ptr = match store.entry(hash, same) {
            Entry::Found(at) => at,
            Entry::Vacant(vacant) => {
                vacant.insert(layout::<T>(slice.len()), |to| {
                    // SAFETY: `to` is fresh memory of the layout of a `Pointee<T>` of
                    // `slice.len()` elements; `SliceType: Copy`, so copying its bytes is a copy.
                    unsafe { write::<T>(to, header, slice) }
                })
            }
        };
        InternedSliceRef { ptr, _marker: PhantomData }
    }

    #[inline]
    pub fn as_ref(&self) -> InternedSliceRef<'_, T> {
        InternedSliceRef { ptr: self.ptr, _marker: PhantomData }
    }
}

/// The layout of a `Pointee<T>` of `len` elements, as `repr(C)` lays it out.
fn layout<T: SliceInternable>(len: usize) -> Layout {
    let (layout, _) = Layout::new::<HeaderWithLength<T::Header>>()
        .extend(Layout::array::<T::SliceType>(len).expect("an interned slice's layout"))
        .expect("an interned slice's layout");
    layout.pad_to_align()
}

/// A fat pointer to the `Pointee<T>` at `at`, of `len` elements.
fn fat<T: SliceInternable>(at: NonNull<u8>, len: usize) -> *mut Pointee<T> {
    ptr::slice_from_raw_parts_mut(at.as_ptr().cast::<T::SliceType>(), len) as *mut Pointee<T>
}

/// # Safety
///
/// `to` is writable memory of `layout::<T>(slice.len())`.
unsafe fn write<T: SliceInternable>(to: NonNull<u8>, header: T::Header, slice: &[T::SliceType]) {
    let fat = fat::<T>(to, slice.len());
    // SAFETY: Our precondition; the places are fields of the value `fat` points at.
    unsafe {
        (&raw mut (*fat).header).write(HeaderWithLength { header, length: slice.len() });
        (&raw mut (*fat).slice)
            .cast::<T::SliceType>()
            .copy_from_nonoverlapping(slice.as_ptr(), slice.len());
    }
}

/// # Safety
///
/// `at` points at the start of a published `Pointee<T>` of `T::storage()`, which lives for the
/// process.
unsafe fn pointee<T: SliceInternable>(at: NonNull<u8>) -> &'static Pointee<T> {
    // SAFETY: Our precondition; the value starts with its `HeaderWithLength`, whose `length`
    // is its slice's.
    unsafe {
        let header = at.cast::<HeaderWithLength<T::Header>>().as_ptr();
        let length = (&raw const (*header).length).read();
        &*fat::<T>(at, length)
    }
}

/// Compares interned `Ref`s using pointer equality.
impl<T: SliceInternable> PartialEq for InternedSlice<T> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.ptr == other.ptr
    }
}

impl<T: SliceInternable> Eq for InternedSlice<T> {}

impl<T: SliceInternable> Hash for InternedSlice<T> {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_usize(self.ptr.as_ptr().addr())
    }
}

impl<T: SliceInternable> Deref for InternedSlice<T> {
    type Target = Pointee<T>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        // SAFETY: `self.ptr`'s invariant.
        unsafe { pointee::<T>(self.ptr) }
    }
}

impl<T: SliceInternable> Clone for InternedSlice<T> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: SliceInternable> Copy for InternedSlice<T> {}

impl<T> Debug for InternedSlice<T>
where
    T: SliceInternable,
    T::SliceType: Debug,
    T::Header: Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(f)
    }
}

#[repr(transparent)]
pub struct InternedSliceRef<'a, T> {
    /// # Invariant
    ///
    /// Points at the start of a `Pointee<T>` of `T::storage()`, never moved or freed.
    ptr: NonNull<u8>,
    _marker: PhantomData<&'a T>,
}

// SAFETY: As for `InternedSlice`: a shared reference to `Send + Sync` values that live for the
// process.
unsafe impl<T: Send + Sync> Send for InternedSliceRef<'_, T> {}
unsafe impl<T: Send + Sync> Sync for InternedSliceRef<'_, T> {}

impl<'a, T: SliceInternable> InternedSliceRef<'a, T> {
    #[inline]
    pub fn to_owned(self) -> InternedSlice<T> {
        InternedSlice { ptr: self.ptr, _marker: PhantomData }
    }

    #[inline]
    pub fn get(self) -> &'a Pointee<T> {
        // SAFETY: `self.ptr`'s invariant.
        unsafe { pointee::<T>(self.ptr) }
    }

    /// Changes the attached lifetime: interned values live for the process, so the lifetime is
    /// only a lint against misuse, not a soundness check.
    #[inline]
    pub fn change_lifetime<'b>(self) -> InternedSliceRef<'b, T> {
        InternedSliceRef { ptr: self.ptr, _marker: PhantomData }
    }
}

impl<T> Clone for InternedSliceRef<'_, T> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for InternedSliceRef<'_, T> {}

impl<T: SliceInternable> Hash for InternedSliceRef<'_, T> {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_usize(self.ptr.as_ptr().addr());
    }
}

impl<T: SliceInternable> PartialEq for InternedSliceRef<'_, T> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.ptr == other.ptr
    }
}

impl<T: SliceInternable> Eq for InternedSliceRef<'_, T> {}

impl<T: SliceInternable> Deref for InternedSliceRef<'_, T> {
    type Target = Pointee<T>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.get()
    }
}

impl<T> Debug for InternedSliceRef<'_, T>
where
    T: SliceInternable,
    T::SliceType: Debug,
    T::Header: Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(f)
    }
}

pub struct InternSliceStorage<T: SliceInternable> {
    store: OnceLock<Store>,
    _of: PhantomData<fn() -> T>,
}

#[allow(
    clippy::new_without_default,
    reason = "this a const fn, so it can't be default yet. See <https://github.com/rust-lang/rust/issues/63065>"
)]
impl<T: SliceInternable> InternSliceStorage<T> {
    pub const fn new() -> Self {
        Self { store: OnceLock::new(), _of: PhantomData }
    }
}

impl<T: SliceInternable> InternSliceStorage<T> {
    pub(crate) fn get(&self) -> &Store {
        self.store.get_or_init(Store::new)
    }
}

pub trait SliceInternable: Sized + 'static {
    type Header: Eq + Hash + Send + Sync;
    type SliceType: Eq + Hash + Send + Sync + Copy + 'static;
    fn storage() -> &'static InternSliceStorage<Self>;
}

/// Implements `SliceInternable` for a given list of types, making them usable with `InternedSlice`.
/// `gc;` is accepted and means the same: every interned slice lives for the process.
#[macro_export]
#[doc(hidden)]
macro_rules! _impl_slice_internable {
    ( gc; $tag:ident, $h:ty, $t:ty $(,)? ) => {
        $crate::_impl_slice_internable!($tag, $h, $t);
    };
    ( $tag:ident, $h:ty, $t:ty $(,)? ) => {
        #[allow(unreachable_pub)]
        pub struct $tag;
        impl $crate::SliceInternable for $tag {
            type Header = $h;
            type SliceType = $t;
            fn storage() -> &'static $crate::InternSliceStorage<Self> {
                static STORAGE: $crate::InternSliceStorage<$tag> =
                    $crate::InternSliceStorage::new();
                &STORAGE
            }
        }
    };
}
pub use crate::_impl_slice_internable as impl_slice_internable;

#[cfg(test)]
mod tests {
    use super::*;

    crate::impl_slice_internable!(gc; Numbers, String, u32);
    type NumbersSlice = InternedSlice<Numbers>;

    const _: () = assert!(size_of::<InternedSlice<Numbers>>() == size_of::<usize>());
    const _: () = assert!(size_of::<InternedSliceRef<'static, Numbers>>() == size_of::<usize>());

    #[test]
    fn simple_slices() {
        let d = NumbersSlice::from_header_and_slice("abc".to_owned(), &[123, 456]);
        let e = NumbersSlice::from_header_and_slice("abc".to_owned(), &[123, 456]);
        assert_eq!(d, e);
        assert_eq!(d.to_owned(), e.to_owned());
        assert_eq!(d.header.length, 2);
        assert_eq!(d.header.header, "abc");
        assert_eq!(d.slice, [123, 456]);
        let other_header = NumbersSlice::from_header_and_slice("abd".to_owned(), &[123, 456]);
        let longer = NumbersSlice::from_header_and_slice("abc".to_owned(), &[123, 456, 0]);
        let empty = NumbersSlice::from_header_and_slice("abc".to_owned(), &[]);
        assert_ne!(d, other_header);
        assert_ne!(d, longer);
        assert_ne!(d, empty);
        assert!(empty.slice.is_empty());
        assert_eq!(empty.header.length, 0);
        assert_eq!(empty, NumbersSlice::from_header_and_slice("abc".to_owned(), &[]));
        assert_eq!(d.to_owned().as_ref(), d);
        let longer_life: InternedSliceRef<'static, Numbers> = d.change_lifetime();
        assert_eq!(longer_life, d);
    }

    crate::impl_slice_internable!(Wide, u8, u128);

    /// A header smaller than the slice's alignment: the elements are still aligned.
    #[test]
    fn slices_are_aligned() {
        for len in 0..64u128 {
            let slice: Vec<u128> = (0..len).collect();
            let interned = InternedSlice::<Wide>::from_header_and_slice(len as u8, &slice);
            assert_eq!(interned.slice.as_ptr().addr() % align_of::<u128>(), 0);
            assert_eq!(interned.slice, slice[..]);
            assert_eq!(interned.header.header, len as u8);
        }
    }

    crate::impl_slice_internable!(Racing, (), u64);

    /// Threads interning overlapping new slices in different orders get pointer-equal handles.
    #[test]
    fn slices_race_to_one_handle() {
        const THREADS: u64 = 16;
        const VALUES: u64 = 5_000;
        let slice = |n: u64| -> Vec<u64> { (0..n % 17).map(|i| n * 31 + i).collect() };
        let seen: Vec<Vec<InternedSlice<Racing>>> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..THREADS)
                .map(|worker| {
                    scope.spawn(move || {
                        (0..VALUES)
                            .map(|n| {
                                let n = (n + worker * 331) % VALUES;
                                InternedSlice::<Racing>::from_header_and_slice((), &slice(n))
                                    .to_owned()
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            workers.into_iter().map(|it| it.join().unwrap()).collect()
        });
        assert_eq!(<Racing as SliceInternable>::storage().get().len(), VALUES as usize);
        for (worker, handles) in seen.iter().enumerate() {
            for (i, handle) in handles.iter().enumerate() {
                let n = (i as u64 + worker as u64 * 331) % VALUES;
                assert_eq!(handle.slice, slice(n)[..]);
                assert_eq!(
                    handle.as_ref(),
                    InternedSlice::<Racing>::from_header_and_slice((), &slice(n))
                );
            }
        }
    }

    crate::impl_slice_internable!(Kept, String, u16);

    /// Enough slices to fill many chunks of every shard: the first still read their contents.
    #[test]
    fn slice_handles_stay_valid() {
        let early: Vec<_> = (0..256u16)
            .map(|n| {
                InternedSlice::<Kept>::from_header_and_slice(format!("early {n}"), &[n, n + 1])
                    .to_owned()
            })
            .collect();
        for n in 0..100_000u32 {
            InternedSlice::<Kept>::from_header_and_slice(format!("later {n}"), &[n as u16; 5]);
        }
        for (n, handle) in (0..256u16).zip(&early) {
            assert_eq!(handle.header.header, format!("early {n}"));
            assert_eq!(handle.slice, [n, n + 1]);
            assert_eq!(
                handle.as_ref(),
                InternedSlice::<Kept>::from_header_and_slice(format!("early {n}"), &[n, n + 1])
            );
        }
    }
}

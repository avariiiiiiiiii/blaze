//! A tree's columns in one allocation: a [`Block`] is the allocation, a [`Col`] a column in it,
//! read and written as a slice. A parse or an edit makes one block for its tree, not one a
//! column, and dropping the tree frees it.
//!
//! The invariant every `unsafe` below rests on: a non-empty [`Col`] is a field of the tree (or
//! of its `Kept`) whose block it was placed in, and stays so until that tree is dropped.
//! `FlatTree` and `Kept` implement `Drop`, so the compiler refuses to move a column out of one;
//! `flat.rs` never swaps or replaces a column of one tree with another's. Columns hold values
//! without drop glue only ([`Block::place`]), so dropping a block is freeing its bytes.

use std::{
    alloc::{self, Layout},
    fmt,
    hash::{Hash, Hasher},
    marker::PhantomData,
    mem::{align_of, needs_drop, size_of},
    ops::{Deref, DerefMut},
    ptr::{self, NonNull},
    slice,
};

/// Every block's alignment: the widest column's (`u64`).
const ALIGN: usize = 8;

/// The bytes a block needs for `len` `T`s placed after `bytes` bytes of columns, as
/// [`Block::put`] and [`Block::fill`] place them: at the next multiple of `T`'s alignment.
pub(super) fn room<T>(bytes: usize, len: usize) -> usize {
    let size = len.checked_mul(size_of::<T>()).expect("a column of under usize::MAX bytes");
    let at = bytes.next_multiple_of(align_of::<T>());
    at.checked_add(size).expect("a block of under usize::MAX bytes")
}

/// One allocation of `size` bytes, aligned to [`ALIGN`], that columns are placed in one after
/// another: every column is placed past the last, so no two overlap.
pub(super) struct Block {
    ptr: NonNull<u8>,
    size: usize,
    /// The bytes placed so far.
    filled: usize,
}

// SAFETY: a block is bytes it owns alone and never reads; its columns are read through `Col`,
// which is `Send` and `Sync` only as its `T` is.
unsafe impl Send for Block {}
// SAFETY: as for `Send`: a `&Block` reads and writes nothing.
unsafe impl Sync for Block {}

impl Block {
    /// A block of `size` bytes ([`room`]); none is allocated for 0.
    pub(super) fn new(size: usize) -> Block {
        if size == 0 {
            return Block { ptr: NonNull::<u64>::dangling().cast(), size, filled: 0 };
        }
        let layout =
            Layout::from_size_align(size, ALIGN).expect("a block of under isize::MAX bytes");
        // SAFETY: `layout`'s size is not zero.
        let ptr = unsafe { alloc::alloc(layout) };
        let ptr = NonNull::new(ptr).unwrap_or_else(|| alloc::handle_alloc_error(layout));
        Block { ptr, size, filled: 0 }
    }

    /// The bytes it holds.
    pub(super) fn size(&self) -> usize {
        self.size
    }

    /// Room for `len` `T`s past every column placed so far, aligned for `T`.
    fn place<T>(&mut self, len: usize) -> NonNull<T> {
        const { assert!(align_of::<T>() <= ALIGN && !needs_drop::<T>()) };
        let at = self.filled.next_multiple_of(align_of::<T>());
        let end = room::<T>(self.filled, len);
        assert!(end <= self.size, "a column of {len} past its block's {} bytes", self.size);
        self.filled = end;
        // SAFETY: `at <= end <= size`: the offset is in the allocation or one past its end (0
        // in a block of size 0, whose pointer is dangling but aligned).
        unsafe { self.ptr.add(at) }.cast()
    }

    /// A column of a copy of `from`.
    pub(super) fn put<T: Copy>(&mut self, from: &[T]) -> Col<T> {
        let ptr = self.place::<T>(from.len());
        // SAFETY: `place` gave room for `from.len()` `T`s, aligned, past every column placed
        // before (so not overlapping `from` when it is one of them) and in another allocation
        // from any slice outside the block.
        unsafe { ptr::copy_nonoverlapping(from.as_ptr(), ptr.as_ptr(), from.len()) };
        Col { ptr, len: from.len(), _of: PhantomData }
    }

    /// A column of `len` values, the `n`th `make(n)`, made in order of `n`.
    pub(super) fn fill<T>(&mut self, len: usize, mut make: impl FnMut(usize) -> T) -> Col<T> {
        let ptr = self.place::<T>(len);
        for n in 0..len {
            // SAFETY: slot `n` of the room `place` gave for `len` `T`s, aligned for `T`.
            unsafe { ptr.add(n).write(make(n)) };
        }
        Col { ptr, len, _of: PhantomData }
    }
}

impl Drop for Block {
    fn drop(&mut self) {
        if self.size != 0 {
            // SAFETY: `ptr` was allocated by `new` with this size and alignment, which
            // `Layout::from_size_align` accepted there; no column's values need dropping.
            unsafe {
                alloc::dealloc(
                    self.ptr.as_ptr(),
                    Layout::from_size_align_unchecked(self.size, ALIGN),
                )
            };
        }
    }
}

/// A column of `T`s in a [`Block`], read and written as a slice.
pub(super) struct Col<T> {
    ptr: NonNull<T>,
    len: usize,
    _of: PhantomData<T>,
}

// SAFETY: a column is `T`s its tree owns alone, as a `Box<[T]>` is.
unsafe impl<T: Send> Send for Col<T> {}
// SAFETY: as for `Send`: `&Col<T>` lends only `&[T]`.
unsafe impl<T: Sync> Sync for Col<T> {}

impl<T> Col<T> {
    /// A column of nothing, in no block.
    pub(super) const fn empty() -> Col<T> {
        Col { ptr: NonNull::dangling(), len: 0, _of: PhantomData }
    }
}

impl<T> Default for Col<T> {
    fn default() -> Col<T> {
        Col::empty()
    }
}

impl<T> Deref for Col<T> {
    type Target = [T];

    #[inline]
    fn deref(&self) -> &[T] {
        // SAFETY: the `len` `T`s at `ptr` were written by `put` or `fill` (none for `empty`),
        // aligned, and stay allocated while this column lives (the module's invariant). They
        // are written only through `&mut self`, and no other column overlaps them (`place`).
        unsafe { slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl<T> DerefMut for Col<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut [T] {
        // SAFETY: as for `deref`; `&mut self` is the only way to these `T`s, as no other column
        // overlaps them and a column is neither `Clone` nor `Copy`.
        unsafe { slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl<T: PartialEq> PartialEq for Col<T> {
    fn eq(&self, other: &Col<T>) -> bool {
        **self == **other
    }
}

impl<T: Eq> Eq for Col<T> {}

/// As a `Box<[T]>` hashes: the slice's.
impl<T: Hash> Hash for Col<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (**self).hash(state);
    }
}

impl<T: fmt::Debug> fmt::Debug for Col<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

#[cfg(test)]
mod tests {
    use blaze_ids::rng::SplitMix64;

    use super::{room, Block, Col};

    /// Columns of every type a tree has, of random lengths, placed in one block in a random
    /// order: each reads back what it was made of (the safe reference: the `Vec`s it was copied
    /// from), so none overlaps another, and the block is exactly the room summed for them.
    #[test]
    fn columns_read_back_what_they_were_made_of() {
        for seed in 0..500 {
            let mut rng = SplitMix64::new(seed);
            let mut len = || rng.index(70);
            let words: Vec<u32> = (0..len()).map(|it| it as u32 ^ 0x5a5a_0000).collect();
            let wide: Vec<u64> = (0..len()).map(|it| (it as u64) << 40 | 7).collect();
            let pairs: Vec<(u32, u32)> = (0..len()).map(|it| (it as u32, !(it as u32))).collect();
            let kinds: Vec<(u32, u16)> =
                (0..len()).map(|it| (it as u32, it as u16 ^ 0x3c3c)).collect();
            let filled = len();
            let mut order = [0, 1, 2, 3, 4];
            for at in (1..order.len()).rev() {
                order.swap(at, rng.index(at + 1));
            }
            let mut size = 0;
            for &it in &order {
                size = match it {
                    0 => room::<u32>(size, words.len()),
                    1 => room::<u64>(size, wide.len()),
                    2 => room::<(u32, u32)>(size, pairs.len()),
                    3 => room::<(u32, u16)>(size, kinds.len()),
                    _ => room::<u64>(size, filled),
                };
            }
            let mut block = Block::new(size);
            let (mut a, mut b, mut c, mut d, mut e) =
                (Col::empty(), Col::empty(), Col::empty(), Col::empty(), Col::empty());
            for &it in &order {
                match it {
                    0 => a = block.put(&words),
                    1 => b = block.put(&wide),
                    2 => c = block.put(&pairs),
                    3 => d = block.put(&kinds),
                    _ => e = block.fill(filled, |n| n as u64 * 3),
                }
            }
            assert_eq!(block.filled, block.size(), "seed {seed}");
            assert_eq!((&*a, &*b, &*c, &*d), (&words[..], &wide[..], &pairs[..], &kinds[..]));
            assert!(e.iter().enumerate().all(|(n, &it)| it == n as u64 * 3), "seed {seed}");
            if let Some(first) = a.first_mut() {
                *first = 1;
                assert_eq!((&*b, &*c), (&wide[..], &pairs[..]), "a write stays in its column");
            }
        }
    }

    #[test]
    #[should_panic(expected = "past its block")]
    fn a_column_past_the_room_is_refused() {
        let mut block = Block::new(room::<u32>(0, 3));
        let _ = block.put(&[1u32, 2, 3, 4]);
    }
}

//! What the passes keep while they run, in the task's frame: sets of block or local or move
//! path indices, as `blaze_ids`' sets over words bumped from the frame, and hash maps. A set is
//! a bump of the frame, and all of it goes when the task ends.

use blaze_ids::bits::BitSliceMut;
use blaze_mem::Frame;
use rustc_hash::FxBuildHasher;

/// A set of indices below `len`, in words bumped from the task's frame.
pub(crate) type Bits<'f, I = usize> = BitSliceMut<'f, I>;

pub(crate) fn bits_in<'f, I>(len: usize, frame: &'f Frame) -> Bits<'f, I> {
    Bits::from_words(frame.alloc_slice_fill_with(len.div_ceil(64), |_| 0))
}

pub(crate) type FrameMap<'f, K, V> = hashbrown::HashMap<K, V, FxBuildHasher, &'f Frame>;

pub(crate) fn map_in<'f, K, V>(frame: &'f Frame) -> FrameMap<'f, K, V> {
    hashbrown::HashMap::with_hasher_in(FxBuildHasher, frame)
}

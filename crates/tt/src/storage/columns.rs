//! The columns a tree holds as plain words, so that the wire writes and reads them as their bytes.
//!
//! `zerocopy`'s derives check the layouts (`repr(C)`, no padding, every bit pattern valid) and
//! emit the `unsafe impl`s of their traits; this module allows them and holds nothing else. The
//! safe version stores or decodes each column a word at a time after zeroing the buffer, about
//! twice the copy's cycles (docs/v2/plan/lanes/frontend/speed-of-light.md, the wire).
#![allow(unsafe_code)]

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// One token tree, 8 bytes. `head` holds the tag in its low two bits and above it:
/// - a subtree: its delimiter kind (2 bits), then its ordinal (the number of subtrees before it,
///   the top one being 0); `data` is its length, the number of token trees in it;
/// - a punct: its spacing (2 bits); `data` is its character;
/// - an ident: whether it is raw (1 bit); `data` is its symbol's index;
/// - a literal: its kind (4 bits), the hashes of a raw one (8 bits) and its suffix's length
///   (the 18 bits left, below [`super::MAX_SUFFIX`]); `data` is its symbol's index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub(super) struct Node {
    pub(super) head: u32,
    pub(super) data: u32,
}

/// A span, 8 bytes: where it starts, and its length in the low 16 bits of `len_part` with its
/// part's index in the high 16. A length or index of `0xFFFF` or more makes it wide: then
/// `len_part` is [`super::WIDE`] and `start` the index of a [`WideSpan`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub(super) struct PackedSpan {
    pub(super) start: u32,
    pub(super) len_part: u32,
}

/// A span too large to pack, 12 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub(super) struct WideSpan {
    pub(super) start: u32,
    pub(super) len: u32,
    pub(super) part: u32,
}

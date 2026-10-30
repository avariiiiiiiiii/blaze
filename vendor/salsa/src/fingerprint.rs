//! 128-bit fingerprints of memoized values.
//!
//! A memo gets a value [`Fingerprint`] when its value is dropped: by the LRU of a function with
//! `lru` (which requires its output to implement [`Hash`]), or by `evict_values` /
//! `evict_values_of` (which exist for every tracked function whose output implements [`Hash`]).
//! Values that are never dropped are never hashed. A value computed again after it was dropped
//! is hashed the same way: equal fingerprints backdate it exactly as an equal value would (unless
//! its durability dropped), the new memo keeps its fingerprint, and a query that reads it records
//! the fingerprint it saw. While a value is present, `Eq` decides, as without fingerprints.
//! Functions with `no_eq` get no fingerprint.
//!
//! The `Hash` impl of such an output must agree with its `Eq` (equal values write equal bytes)
//! and must cover what `Eq` compares (unequal values write different bytes, short of a hash
//! collision). Hashing an address is sound when the allocation is never freed while the process
//! runs, as for the interned values and symbols of blaze's `intern` crate: their address is equal
//! exactly when their value is. It is not sound for an allocation that can be freed and reused by
//! a different value.
//!
//! A value fingerprint is SipHash-1-3 with 128-bit output and a fixed key over the bytes the
//! value's [`Hash`] impl writes. Integers are written little-endian and `usize`/`isize` as 64
//! bits. Salsa IDs hash their index and generation (see [`Id`](crate::Id)'s `Hash`), which name
//! the same entity for as long as the database that assigned them exists: an ID is only reused
//! after its generation is bumped. A fingerprint is therefore meaningful within one process and
//! one database, which is the only place salsa compares them; fingerprints are not persisted.

use std::fmt;
use std::hash::{Hash, Hasher};

use crate::Revision;

/// The 128-bit fingerprint of a memoized value, taken when the value is dropped (see the module
/// docs), or of a revision.
///
/// Value fingerprints are SipHash-1-3 over what the value's [`Hash`] impl writes (see
/// [`StableHasher`]); they compare within one database.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Default)]
#[repr(C)]
pub struct Fingerprint([u32; 4]);

const VALUE_BIT: u128 = 1 << 127;

impl Fingerprint {
    /// No fingerprint: the memo has no value yet, or the value is provisional.
    pub const NONE: Self = Self([0; 4]);

    /// The stable hash of `value`.
    pub fn of_value<T: Hash + ?Sized>(value: &T) -> Self {
        let mut hasher = StableHasher::new();
        value.hash(&mut hasher);
        let (low, high) = hasher.finish128();
        Self::from_u128((u128::from(high) << 64 | u128::from(low)) | VALUE_BIT)
    }

    /// The fingerprint of a value without a stable hash: the revision it last changed in.
    pub fn of_revision(revision: Revision) -> Self {
        Self::from_u128(revision.as_usize() as u128)
    }

    /// Whether this is the hash of a value, as opposed to a revision or [`NONE`](Self::NONE).
    pub fn is_value(self) -> bool {
        self.as_u128() & VALUE_BIT != 0
    }

    pub fn as_u128(self) -> u128 {
        let [a, b, c, d] = self.0;
        u128::from(a) | u128::from(b) << 32 | u128::from(c) << 64 | u128::from(d) << 96
    }

    fn from_u128(bits: u128) -> Self {
        Self([
            bits as u32,
            (bits >> 32) as u32,
            (bits >> 64) as u32,
            (bits >> 96) as u32,
        ])
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fingerprint({:032x})", self.as_u128())
    }
}

/// A [`Hasher`] whose output depends only on the bytes written, on every host: SipHash-1-3 with
/// 128-bit output and a fixed key, integers written little-endian and `usize`/`isize` as 64 bits.
pub type StableHasher = Sip<1, 3>;

const KEY: (u64, u64) = (0x7361_6c73_615f_6670, 0x626c_617a_655f_7632);

#[derive(Clone)]
pub struct Sip<const C: usize, const D: usize> {
    v0: u64,
    v1: u64,
    v2: u64,
    v3: u64,
    tail: u64,
    ntail: usize,
    length: usize,
}

impl Sip<1, 3> {
    pub fn new() -> Self {
        Self::with_keys(KEY.0, KEY.1)
    }
}

impl Default for Sip<1, 3> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const C: usize, const D: usize> Sip<C, D> {
    fn with_keys(k0: u64, k1: u64) -> Self {
        Self {
            v0: k0 ^ 0x736f_6d65_7073_6575,
            v1: k1 ^ 0x646f_7261_6e64_6f6d ^ 0xee,
            v2: k0 ^ 0x6c79_6765_6e65_7261,
            v3: k1 ^ 0x7465_6462_7974_6573,
            tail: 0,
            ntail: 0,
            length: 0,
        }
    }

    #[inline]
    fn round(&mut self) {
        self.v0 = self.v0.wrapping_add(self.v1);
        self.v1 = self.v1.rotate_left(13);
        self.v1 ^= self.v0;
        self.v0 = self.v0.rotate_left(32);
        self.v2 = self.v2.wrapping_add(self.v3);
        self.v3 = self.v3.rotate_left(16);
        self.v3 ^= self.v2;
        self.v0 = self.v0.wrapping_add(self.v3);
        self.v3 = self.v3.rotate_left(21);
        self.v3 ^= self.v0;
        self.v2 = self.v2.wrapping_add(self.v1);
        self.v1 = self.v1.rotate_left(17);
        self.v1 ^= self.v2;
        self.v2 = self.v2.rotate_left(32);
    }

    #[inline]
    fn compress(&mut self, word: u64) {
        self.v3 ^= word;
        for _ in 0..C {
            self.round();
        }
        self.v0 ^= word;
    }

    /// The two 64-bit halves of the 128-bit hash, low half first.
    pub fn finish128(&self) -> (u64, u64) {
        let mut state = self.clone();
        let last = ((self.length as u64 & 0xff) << 56) | self.tail;
        state.compress(last);
        state.v2 ^= 0xee;
        for _ in 0..D {
            state.round();
        }
        let low = state.v0 ^ state.v1 ^ state.v2 ^ state.v3;
        state.v1 ^= 0xdd;
        for _ in 0..D {
            state.round();
        }
        let high = state.v0 ^ state.v1 ^ state.v2 ^ state.v3;
        (low, high)
    }
}

fn load_le(bytes: &[u8]) -> u64 {
    debug_assert!(bytes.len() < 8);
    bytes
        .iter()
        .enumerate()
        .fold(0, |word, (i, &byte)| word | u64::from(byte) << (8 * i))
}

impl<const C: usize, const D: usize> Hasher for Sip<C, D> {
    fn write(&mut self, mut bytes: &[u8]) {
        self.length = self.length.wrapping_add(bytes.len());
        if self.ntail != 0 {
            let needed = 8 - self.ntail;
            let fill = needed.min(bytes.len());
            self.tail |= load_le(&bytes[..fill]) << (8 * self.ntail);
            if fill < needed {
                self.ntail += fill;
                return;
            }
            let word = self.tail;
            self.compress(word);
            self.tail = 0;
            self.ntail = 0;
            bytes = &bytes[fill..];
        }
        let mut words = bytes.chunks_exact(8);
        for word in &mut words {
            self.compress(u64::from_le_bytes(word.try_into().unwrap()));
        }
        let rest = words.remainder();
        self.tail = load_le(rest);
        self.ntail = rest.len();
    }

    fn write_u8(&mut self, i: u8) {
        self.write(&[i]);
    }

    fn write_u16(&mut self, i: u16) {
        self.write(&i.to_le_bytes());
    }

    fn write_u32(&mut self, i: u32) {
        self.write(&i.to_le_bytes());
    }

    fn write_u64(&mut self, i: u64) {
        self.write(&i.to_le_bytes());
    }

    fn write_u128(&mut self, i: u128) {
        self.write(&i.to_le_bytes());
    }

    fn write_usize(&mut self, i: usize) {
        self.write_u64(i as u64);
    }

    fn write_i8(&mut self, i: i8) {
        self.write_u8(i as u8);
    }

    fn write_i16(&mut self, i: i16) {
        self.write_u16(i as u16);
    }

    fn write_i32(&mut self, i: i32) {
        self.write_u32(i as u32);
    }

    fn write_i64(&mut self, i: i64) {
        self.write_u64(i as u64);
    }

    fn write_i128(&mut self, i: i128) {
        self.write_u128(i as u128);
    }

    fn write_isize(&mut self, i: isize) {
        self.write_u64(i as i64 as u64);
    }

    fn finish(&self) -> u64 {
        self.finish128().0
    }
}

#[cfg(test)]
mod tests {
    use std::hash::{Hash, Hasher};

    use super::{Fingerprint, Sip, StableHasher};

    const REFERENCE_KEY: (u64, u64) = (0x0706_0504_0302_0100, 0x0f0e_0d0c_0b0a_0908);

    fn reference_bytes(message: &[u8]) -> [u8; 16] {
        let mut hasher = Sip::<2, 4>::with_keys(REFERENCE_KEY.0, REFERENCE_KEY.1);
        hasher.write(message);
        let (low, high) = hasher.finish128();
        let mut out = [0; 16];
        out[..8].copy_from_slice(&low.to_le_bytes());
        out[8..].copy_from_slice(&high.to_le_bytes());
        out
    }

    #[test]
    fn siphash_2_4_128_reference_vectors() {
        assert_eq!(
            reference_bytes(&[]),
            [
                0xa3, 0x81, 0x7f, 0x04, 0xba, 0x25, 0xa8, 0xe6, 0x6d, 0xf6, 0x72, 0x14, 0xc7, 0x55,
                0x02, 0x93
            ]
        );
        assert_eq!(
            reference_bytes(&[0x00]),
            [
                0xda, 0x87, 0xc1, 0xd8, 0x6b, 0x99, 0xaf, 0x44, 0x34, 0x76, 0x59, 0x11, 0x9b, 0x22,
                0xfc, 0x45
            ]
        );
    }

    #[test]
    fn chunking_does_not_change_the_hash() {
        let message: Vec<u8> = (0..=200).collect();
        let mut whole = StableHasher::new();
        whole.write(&message);
        for split in [1, 3, 7, 8, 9, 15, 16, 17, 63, 64, 65] {
            let mut pieces = StableHasher::new();
            for chunk in message.chunks(split) {
                pieces.write(chunk);
            }
            assert_eq!(pieces.finish128(), whole.finish128(), "chunks of {split}");
        }
    }

    #[test]
    fn integers_hash_little_endian_and_usize_as_u64() {
        let mut a = StableHasher::new();
        a.write_usize(0x0102_0304);
        a.write_i16(-2);
        let mut b = StableHasher::new();
        b.write(&0x0102_0304u64.to_le_bytes());
        b.write(&(-2i16).to_le_bytes());
        assert_eq!(a.finish128(), b.finish128());
    }

    #[test]
    fn value_fingerprints_are_deterministic_and_distinguish_values() {
        let a = Fingerprint::of_value(&("parse", 1u32, vec![1u8, 2, 3]));
        let b = Fingerprint::of_value(&("parse", 1u32, vec![1u8, 2, 3]));
        let c = Fingerprint::of_value(&("parse", 1u32, vec![1u8, 2, 4]));
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.is_value() && c.is_value());
        assert!(!Fingerprint::NONE.is_value());
        let revision = crate::Revision::start().next();
        assert!(!Fingerprint::of_revision(revision).is_value());
        assert_ne!(Fingerprint::of_revision(revision), Fingerprint::NONE);
    }

    #[test]
    fn hasher_finish_is_the_low_half() {
        let mut hasher = StableHasher::new();
        "text".hash(&mut hasher);
        assert_eq!(hasher.finish(), hasher.finish128().0);
    }
}

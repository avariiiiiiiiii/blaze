//! blaze v2's IDs (docs/v2/format.md section 5, "IDs, two kinds").
//!
//! - A [`ContentId`] is the BLAKE3 hash of what a thing is, under a domain that names the
//!   kind of thing and its schema. It is the identity used in every hash, key and output.
//! - An [`Idx`] is a dense `u32` index into one arena, assigned in arrival order by parallel
//!   workers: it differs between runs, so it never goes into a hash, a key or the bytes of an
//!   output. It may be kept in the daemon's image, which is one run's state.
//! - An [`Interner`] holds one arena and maps each way between the two.
//! - A [`Fingerprint`] is the 128 bits of a result kept for early cutoff (docs/v2/items.md
//!   section 6.2).

use std::{fmt, hash, marker::PhantomData};

use serde::{Deserialize, Serialize};

/// The BLAKE3 hash of what something is. Two things with one content ID are the same thing.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
#[repr(transparent)]
pub struct ContentId(pub [u8; 32]);

impl ContentId {
    /// The content ID of `bytes` as a thing of `domain`. A domain names what the bytes are
    /// and the version of their schema (`"blaze v2 file 1"`): equal bytes of two domains have
    /// two IDs.
    pub fn of(domain: &'static str, bytes: &[u8]) -> ContentId {
        let mut hasher = Hasher::new(domain);
        hasher.update(bytes);
        hasher.finish()
    }

    /// The first 128 bits, as a result's fingerprint.
    pub fn fingerprint(&self) -> Fingerprint {
        let mut low = [0; 16];
        low.copy_from_slice(&self.0[..16]);
        Fingerprint(u128::from_le_bytes(low))
    }
}

impl fmt::Debug for ContentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl fmt::Display for ContentId {
    /// The first 12 hex digits, as git shows a short id.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0[..6].iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

impl fmt::LowerHex for ContentId {
    /// All 64 hex digits.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

/// Builds a [`ContentId`] from parts. It takes bytes and other content IDs only, so a dense
/// [`Idx`] can't be hashed but by spelling out [`Idx::raw`], which a reviewer sees.
pub struct Hasher(blake3::Hasher);

impl Hasher {
    pub fn new(domain: &'static str) -> Hasher {
        Hasher(blake3::Hasher::new_derive_key(domain))
    }

    pub fn update(&mut self, bytes: &[u8]) -> &mut Hasher {
        self.0.update(bytes);
        self
    }

    /// A length-prefixed part, so `("ab", "c")` and `("a", "bc")` hash apart.
    pub fn part(&mut self, bytes: &[u8]) -> &mut Hasher {
        self.0.update(&(bytes.len() as u64).to_le_bytes());
        self.0.update(bytes);
        self
    }

    pub fn id(&mut self, id: &ContentId) -> &mut Hasher {
        self.0.update(&id.0);
        self
    }

    pub fn finish(&self) -> ContentId {
        ContentId(*self.0.finalize().as_bytes())
    }
}

/// 128 bits that stand for a result's value: a rerun that produces the same fingerprint stops
/// there (early cutoff). Kept with every dependency edge and when the value is evicted.
/// Serialized as 16 little-endian bytes: CBOR has no integer this wide.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
#[repr(transparent)]
pub struct Fingerprint(pub u128);

impl Serialize for Fingerprint {
    fn serialize<S: serde::Serializer>(&self, to: S) -> Result<S::Ok, S::Error> {
        self.0.to_le_bytes().serialize(to)
    }
}

impl<'de> Deserialize<'de> for Fingerprint {
    fn deserialize<D: serde::Deserializer<'de>>(from: D) -> Result<Fingerprint, D::Error> {
        <[u8; 16]>::deserialize(from).map(|bytes| Fingerprint(u128::from_le_bytes(bytes)))
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:08x}", (self.0 >> 96) as u32)
    }
}

/// A dense index into the arena of `T`s that made it. Only meaningful for that arena.
#[repr(transparent)]
pub struct Idx<T> {
    raw: u32,
    of: PhantomData<fn() -> T>,
}

impl<T> Idx<T> {
    pub const fn from_raw(raw: u32) -> Idx<T> {
        Idx { raw, of: PhantomData }
    }

    pub const fn raw(self) -> u32 {
        self.raw
    }
}

impl<T> Clone for Idx<T> {
    fn clone(&self) -> Idx<T> {
        *self
    }
}

impl<T> Copy for Idx<T> {}

impl<T> PartialEq for Idx<T> {
    fn eq(&self, other: &Idx<T>) -> bool {
        self.raw == other.raw
    }
}

impl<T> Eq for Idx<T> {}

impl<T> PartialOrd for Idx<T> {
    fn partial_cmp(&self, other: &Idx<T>) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<T> Ord for Idx<T> {
    fn cmp(&self, other: &Idx<T>) -> std::cmp::Ordering {
        self.raw.cmp(&other.raw)
    }
}

impl<T> hash::Hash for Idx<T> {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        self.raw.hash(state);
    }
}

impl<T> fmt::Debug for Idx<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.raw)
    }
}

/// One arena of `T`s, append-only and sharded by content ID: interning from many workers
/// takes no global lock, and a reader never locks. A value once interned never moves or
/// changes, so `&T` lives as long as the interner.
pub trait Interner<T>: Sync {
    /// The index of the value whose content ID is `id`, interning `value` if it has none.
    /// `value` must be what `id` is the content ID of.
    fn intern(&self, id: ContentId, value: impl FnOnce() -> T) -> Idx<T>;
    fn get(&self, idx: Idx<T>) -> &T;
    fn content(&self, idx: Idx<T>) -> ContentId;
    fn find(&self, id: &ContentId) -> Option<Idx<T>>;
    fn len(&self) -> usize;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domains_part_equal_bytes() {
        assert_ne!(ContentId::of("blaze v2 a 1", b"x"), ContentId::of("blaze v2 b 1", b"x"));
        assert_eq!(ContentId::of("blaze v2 a 1", b"x"), ContentId::of("blaze v2 a 1", b"x"));
    }

    #[test]
    fn parts_are_framed() {
        let ab_c = Hasher::new("blaze v2 test 1").part(b"ab").part(b"c").finish();
        let a_bc = Hasher::new("blaze v2 test 1").part(b"a").part(b"bc").finish();
        assert_ne!(ab_c, a_bc);
    }
}

//! A value's derived [`std::hash::Hash`] as bytes the same on every target and in every run:
//! every integer written little-endian at a fixed width, `usize` and `isize` as 64 bits.
//!
//! [`fixed_width_writes!`](crate::fixed_width_writes) is the `write_u8`..`write_isize` half of a
//! `std::hash::Hasher` that does this, for any hasher with its own `write`; [`StdHash`] is one
//! over a [`Hasher`], so a type whose `Hash` covers what its `Eq` compares and writes no address
//! (a foreign type such as `rustc_abi`'s layouts) is hashed as content with [`Hasher::std`]. A
//! `Hash` that writes an address (an `Interned` of `crates/intern`, a raw pointer) is not
//! deterministic and must not be fed this way; feed such parts by content ID.

use std::hash::Hash;

use crate::Hasher;

/// The integer methods of a `std::hash::Hasher`, each writing its argument's little-endian
/// bytes through `self.write`, `usize` as `u64` and `isize` as `i64`. Goes inside the
/// `impl std::hash::Hasher for ..` block, beside its own `write` and `finish`.
#[macro_export]
macro_rules! fixed_width_writes {
    () => {
        #[inline]
        fn write_u8(&mut self, it: u8) {
            self.write(&[it]);
        }

        #[inline]
        fn write_u16(&mut self, it: u16) {
            self.write(&it.to_le_bytes());
        }

        #[inline]
        fn write_u32(&mut self, it: u32) {
            self.write(&it.to_le_bytes());
        }

        #[inline]
        fn write_u64(&mut self, it: u64) {
            self.write(&it.to_le_bytes());
        }

        #[inline]
        fn write_u128(&mut self, it: u128) {
            self.write(&it.to_le_bytes());
        }

        #[inline]
        fn write_usize(&mut self, it: usize) {
            self.write(&(it as u64).to_le_bytes());
        }

        #[inline]
        fn write_i8(&mut self, it: i8) {
            self.write(&it.to_le_bytes());
        }

        #[inline]
        fn write_i16(&mut self, it: i16) {
            self.write(&it.to_le_bytes());
        }

        #[inline]
        fn write_i32(&mut self, it: i32) {
            self.write(&it.to_le_bytes());
        }

        #[inline]
        fn write_i64(&mut self, it: i64) {
            self.write(&it.to_le_bytes());
        }

        #[inline]
        fn write_i128(&mut self, it: i128) {
            self.write(&it.to_le_bytes());
        }

        #[inline]
        fn write_isize(&mut self, it: isize) {
            self.write(&(it as i64).to_le_bytes());
        }
    };
}

/// A `std::hash::Hasher` feeding a [`Hasher`], integers at a fixed width.
pub struct StdHash<'a>(pub &'a mut Hasher);

impl std::hash::Hasher for StdHash<'_> {
    /// The first 64 bits of what was fed so far: no `Hash` impl reads it, but the trait asks.
    fn finish(&self) -> u64 {
        let mut word = [0; 8];
        word.copy_from_slice(&self.0.finish().0[..8]);
        u64::from_le_bytes(word)
    }

    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    crate::fixed_width_writes!();
}

impl Hasher {
    /// Feeds what `value`'s [`Hash`] writes, integers at a fixed width.
    pub fn std<T: Hash + ?Sized>(&mut self, value: &T) -> &mut Hasher {
        value.hash(&mut StdHash(self));
        self
    }
}

#[cfg(test)]
mod tests {
    use std::hash::{Hash, Hasher as _};

    use hegel::TestCase;
    use hegel::generators as gs;

    use crate::Hasher;

    /// Bytes written through [`fixed_width_writes!`](crate::fixed_width_writes), to compare.
    struct Bytes(Vec<u8>);

    impl std::hash::Hasher for Bytes {
        fn finish(&self) -> u64 {
            0
        }

        fn write(&mut self, bytes: &[u8]) {
            self.0.extend_from_slice(bytes);
        }

        crate::fixed_width_writes!();
    }

    fn bytes<T: Hash + ?Sized>(value: &T) -> Vec<u8> {
        let mut out = Bytes(Vec::new());
        value.hash(&mut out);
        out.0
    }

    #[test]
    fn std_writes_fixed_width_little_endian() {
        let mut fed = Hasher::new("blaze v2 ids test std 1");
        fed.std(&(7usize, -2isize, "ab", 3u16));
        let mut expected = Hasher::new("blaze v2 ids test std 1");
        expected
            .update(&7u64.to_le_bytes())
            .update(&(-2i64).to_le_bytes())
            .update(b"ab")
            .update(&[0xff])
            .update(&3u16.to_le_bytes());
        assert_eq!(fed.finish(), expected.finish());
    }

    #[test]
    fn every_width() {
        let mut out = Bytes(Vec::new());
        out.write_u8(1);
        out.write_i8(-1);
        out.write_u16(0x0302);
        out.write_i16(-2);
        out.write_u32(0x0706_0504);
        out.write_i32(-3);
        out.write_u64(8);
        out.write_i64(-4);
        out.write_u128(9);
        out.write_i128(-5);
        out.write_usize(10);
        out.write_isize(-6);
        let mut want = vec![1, 0xff, 2, 3];
        want.extend((-2i16).to_le_bytes());
        want.extend([4, 5, 6, 7]);
        want.extend((-3i32).to_le_bytes());
        want.extend(8u64.to_le_bytes());
        want.extend((-4i64).to_le_bytes());
        want.extend(9u128.to_le_bytes());
        want.extend((-5i128).to_le_bytes());
        want.extend(10u64.to_le_bytes());
        want.extend((-6i64).to_le_bytes());
        assert_eq!(out.0, want);
    }

    /// Against the reference: the hand-written forwarders `blaze-hashcons`, `hir-ty-share`,
    /// `hir-ty-codegen` and salsa each carried, here as the bytes they wrote.
    #[hegel::test]
    fn matches_the_reference(tc: TestCase) {
        let a: u8 = tc.draw(gs::integers());
        let b: i16 = tc.draw(gs::integers());
        let c: u32 = tc.draw(gs::integers());
        let d: i64 = tc.draw(gs::integers());
        let e: u128 = tc.draw(gs::integers());
        let f: usize = tc.draw(gs::integers());
        let g: isize = tc.draw(gs::integers());
        let s: String = tc.draw(gs::text());
        let mut want = vec![a];
        want.extend(b.to_le_bytes());
        want.extend(c.to_le_bytes());
        want.extend(d.to_le_bytes());
        want.extend(e.to_le_bytes());
        want.extend((f as u64).to_le_bytes());
        want.extend((g as i64 as u64).to_le_bytes());
        want.extend(s.as_bytes());
        want.push(0xff);
        assert_eq!(bytes(&(a, b, c, d, e, f, g, s.as_str())), want);
    }
}

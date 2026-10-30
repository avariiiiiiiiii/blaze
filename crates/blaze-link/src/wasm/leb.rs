//! LEB128 as the wasm binary format writes numbers, the 5-byte padded form an object leaves
//! where the linker writes an index or an address, and the framing of sections and names.

/// How many bytes an object leaves for an index or an address it relocates.
pub(super) const PADDED: usize = 5;

pub(super) fn uleb(out: &mut Vec<u8>, value: u64) {
    let mut bytes = [0; 10];
    let len = uleb_into(&mut bytes, value);
    out.extend_from_slice(&bytes[..len]);
}

/// How many bytes [`uleb`] writes for `value`.
pub(super) const fn uleb_len(value: u64) -> usize {
    let bits = 64 - (value | 1).leading_zeros() as usize;
    bits.div_ceil(7)
}

/// Writes `value` as a LEB128 at the start of `place`, which has room for it
/// ([`uleb_len`]), and says how many bytes that took.
pub(super) fn uleb_into(place: &mut [u8], mut value: u64) -> usize {
    let mut at = 0;
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            place[at] = byte;
            return at + 1;
        }
        place[at] = byte | 0x80;
        at += 1;
    }
}

pub(super) fn sleb(out: &mut Vec<u8>, mut value: i64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        let done = (value == 0 && byte & 0x40 == 0) || (value == -1 && byte & 0x40 != 0);
        if done {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// `value` as a LEB128 of exactly [`PADDED`] bytes.
pub(super) fn padded_uleb(mut value: u32) -> [u8; PADDED] {
    let mut bytes = [0; PADDED];
    for (at, byte) in bytes.iter_mut().enumerate() {
        *byte = (value & 0x7f) as u8;
        value >>= 7;
        if at < PADDED - 1 {
            *byte |= 0x80;
        }
    }
    bytes
}

/// `value` as a signed LEB128 of exactly [`PADDED`] bytes.
pub(super) fn padded_sleb(mut value: i32) -> [u8; PADDED] {
    let mut bytes = [0; PADDED];
    for (at, byte) in bytes.iter_mut().enumerate() {
        *byte = (value & 0x7f) as u8;
        value >>= 7;
        if at < PADDED - 1 {
            *byte |= 0x80;
        }
    }
    bytes
}

pub(super) fn name(out: &mut Vec<u8>, name: &str) {
    uleb(out, name.len() as u64);
    out.extend(name.as_bytes());
}

/// A section: its id, its size, then `contents`.
pub(super) fn section(out: &mut Vec<u8>, id: u8, contents: &[u8]) {
    out.push(id);
    uleb(out, contents.len() as u64);
    out.extend(contents);
}

/// A custom section: id 0, its size, its name, then `payload`.
pub(super) fn custom(out: &mut Vec<u8>, custom: &str, payload: &[u8]) {
    let mut contents = Vec::with_capacity(custom.len() + 5 + payload.len());
    name(&mut contents, custom);
    contents.extend(payload);
    section(out, 0, &contents);
}

/// `i32.const value; end`: a constant expression.
pub(super) fn i32_const(out: &mut Vec<u8>, value: i32) {
    out.push(0x41);
    sleb(out, value.into());
    out.push(0x0b);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wasm::reader::Reader;

    fn u(value: u64) -> Vec<u8> {
        let mut out = Vec::new();
        uleb(&mut out, value);
        out
    }

    fn s(value: i64) -> Vec<u8> {
        let mut out = Vec::new();
        sleb(&mut out, value);
        out
    }

    #[test]
    fn unsigned() {
        assert_eq!(u(0), [0]);
        assert_eq!(u(127), [0x7f]);
        assert_eq!(u(128), [0x80, 0x01]);
        assert_eq!(u(624_485), [0xe5, 0x8e, 0x26]);
        assert_eq!(u(u32::MAX.into()), [0xff, 0xff, 0xff, 0xff, 0x0f]);
        assert_eq!(u(u64::MAX).len(), 10);
    }

    #[test]
    fn lengths() {
        for value in [0, 1, 127, 128, 16_383, 16_384, u64::from(u32::MAX), u64::MAX] {
            assert_eq!(uleb_len(value), u(value).len(), "{value}");
            let mut place = [0xaa; 11];
            assert_eq!(uleb_into(&mut place, value), u(value).len());
            assert_eq!(&place[..u(value).len()], u(value));
        }
    }

    #[test]
    fn signed() {
        assert_eq!(s(0), [0]);
        assert_eq!(s(-1), [0x7f]);
        assert_eq!(s(63), [0x3f]);
        assert_eq!(s(64), [0xc0, 0x00]);
        assert_eq!(s(-64), [0x40]);
        assert_eq!(s(-65), [0xbf, 0x7f]);
        assert_eq!(s(-123_456), [0xc0, 0xbb, 0x78]);
        assert_eq!(s(i32::MIN.into()), [0x80, 0x80, 0x80, 0x80, 0x78]);
    }

    #[test]
    fn padded() {
        assert_eq!(padded_uleb(0), [0x80, 0x80, 0x80, 0x80, 0x00]);
        assert_eq!(padded_uleb(5), [0x85, 0x80, 0x80, 0x80, 0x00]);
        assert_eq!(padded_uleb(624_485), [0xe5, 0x8e, 0xa6, 0x80, 0x00]);
        assert_eq!(padded_uleb(u32::MAX), [0xff, 0xff, 0xff, 0xff, 0x0f]);
        assert_eq!(padded_sleb(0), [0x80, 0x80, 0x80, 0x80, 0x00]);
        assert_eq!(padded_sleb(-1), [0xff, 0xff, 0xff, 0xff, 0x7f]);
        assert_eq!(padded_sleb(i32::MIN), [0x80, 0x80, 0x80, 0x80, 0x78]);
        assert_eq!(padded_sleb(i32::MAX), [0xff, 0xff, 0xff, 0xff, 0x07]);
        assert_eq!(padded_sleb(0x10_0000), [0x80, 0x80, 0xc0, 0x80, 0x00]);
    }

    #[test]
    fn round_trips() {
        let unsigned = [0, 1, 127, 128, 300, 65_535, 1 << 20, u32::MAX];
        for value in unsigned {
            assert_eq!(Reader::new(&u(value.into())).u32().unwrap(), value);
            assert_eq!(Reader::new(&padded_uleb(value)).u32().unwrap(), value);
        }
        let signed = [0, 1, -1, 63, 64, -64, -65, 1 << 20, i32::MIN, i32::MAX];
        for value in signed {
            assert_eq!(Reader::new(&s(value.into())).i32().unwrap(), value);
            assert_eq!(Reader::new(&padded_sleb(value)).i32().unwrap(), value);
        }
    }

    #[test]
    fn writes_into_room_allocate_nothing() {
        let mut out = Vec::with_capacity(64);
        let mut place = [0; 10];
        let written = blaze_alloc_count::assert_at_most(0, "LEB128 writes into room", || {
            uleb(&mut out, 624_485);
            sleb(&mut out, -123_456);
            i32_const(&mut out, -2);
            name(&mut out, "ab");
            (padded_uleb(5), padded_sleb(-1), uleb_into(&mut place, u64::MAX))
        });
        assert_eq!(written, (padded_uleb(5), padded_sleb(-1), 10));
        assert_eq!(out, [0xe5, 0x8e, 0x26, 0xc0, 0xbb, 0x78, 0x41, 0x7e, 0x0b, 2, b'a', b'b']);
    }

    #[test]
    fn framing() {
        let mut out = Vec::new();
        custom(&mut out, "ab", &[7]);
        assert_eq!(out, [0, 4, 2, b'a', b'b', 7]);
        let mut out = Vec::new();
        i32_const(&mut out, -2);
        assert_eq!(out, [0x41, 0x7e, 0x0b]);
    }
}

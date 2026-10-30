//! Reads the wasm binary format: numbers, names, and the sections of a module or an object.

use std::ops::Range;

pub(super) struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    pub(super) fn new(bytes: &'a [u8]) -> Reader<'a> {
        Reader { bytes, at: 0 }
    }

    /// Where the next byte is, from the start of the bytes read.
    pub(super) fn at(&self) -> usize {
        self.at
    }

    pub(super) fn done(&self) -> bool {
        self.at == self.bytes.len()
    }

    pub(super) fn u8(&mut self) -> Result<u8, String> {
        let byte = *self.bytes.get(self.at).ok_or("truncated")?;
        self.at += 1;
        Ok(byte)
    }

    pub(super) fn bytes(&mut self, len: usize) -> Result<&'a [u8], String> {
        let end = self.at.checked_add(len).filter(|&end| end <= self.bytes.len());
        let end = end.ok_or("truncated")?;
        let bytes = &self.bytes[self.at..end];
        self.at = end;
        Ok(bytes)
    }

    pub(super) fn rest(&mut self) -> &'a [u8] {
        let rest = &self.bytes[self.at..];
        self.at = self.bytes.len();
        rest
    }

    fn unsigned(&mut self, bits: u32) -> Result<u64, String> {
        let mut value = 0u64;
        let mut shift = 0;
        loop {
            let byte = self.u8()?;
            if shift >= bits {
                return Err(format!("a LEB128 longer than {bits} bits"));
            }
            let low = u64::from(byte & 0x7f);
            if bits - shift < 7 && low >> (bits - shift) != 0 {
                return Err(format!("a LEB128 past {bits} bits"));
            }
            value |= low << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
    }

    fn signed(&mut self, bits: u32) -> Result<i64, String> {
        let mut value = 0i64;
        let mut shift = 0;
        loop {
            let byte = self.u8()?;
            if shift >= bits {
                return Err(format!("a LEB128 longer than {bits} bits"));
            }
            value |= i64::from(byte & 0x7f) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                if shift < 64 && byte & 0x40 != 0 {
                    value |= -1i64 << shift;
                }
                if bits < 64 {
                    let half = 1i64 << (bits - 1);
                    if value < -half || value >= half {
                        return Err(format!("a signed LEB128 past {bits} bits"));
                    }
                }
                return Ok(value);
            }
        }
    }

    pub(super) fn u32(&mut self) -> Result<u32, String> {
        Ok(self.unsigned(32)? as u32)
    }

    pub(super) fn u64(&mut self) -> Result<u64, String> {
        self.unsigned(64)
    }

    pub(super) fn i32(&mut self) -> Result<i32, String> {
        Ok(self.signed(32)? as i32)
    }

    pub(super) fn i64(&mut self) -> Result<i64, String> {
        self.signed(64)
    }

    /// A count of things that follow, each at least a byte: more than the bytes left is
    /// malformed, and is not allocated for.
    pub(super) fn count(&mut self) -> Result<usize, String> {
        let count = self.u32()? as usize;
        if count > self.bytes.len() - self.at {
            return Err(format!("a count of {count} with {} bytes left", self.bytes.len() - self.at));
        }
        Ok(count)
    }

    pub(super) fn name(&mut self) -> Result<&'a str, String> {
        let len = self.u32()? as usize;
        let bytes = self.bytes(len)?;
        std::str::from_utf8(bytes).map_err(|_| "a name that is not UTF-8".to_owned())
    }

    /// `i32.const value; end`, the offset of an active segment of wasm32.
    pub(super) fn i32_const(&mut self) -> Result<i32, String> {
        match self.u8()? {
            0x41 => {}
            other => return Err(format!("an offset expression of opcode {other:#04x}")),
        }
        let value = self.i32()?;
        match self.u8()? {
            0x0b => Ok(value),
            _ => Err("an offset expression longer than a constant".to_owned()),
        }
    }
}

/// A section of a module or an object.
pub(super) struct Section<'a> {
    pub id: u8,
    /// What follows its id and size, and where that is in the file.
    pub contents: &'a [u8],
    pub at: usize,
}

impl Section<'_> {
    pub(super) fn range(&self) -> Range<usize> {
        self.at..self.at + self.contents.len()
    }
}

pub(super) const MAGIC: &[u8; 8] = b"\0asm\x01\0\0\0";

/// The sections of a wasm file, in order.
pub(super) fn sections(bytes: &[u8]) -> Result<Vec<Section<'_>>, String> {
    if !bytes.starts_with(MAGIC) {
        return Err("not a wasm file of version 1".to_owned());
    }
    let mut reader = Reader::new(bytes);
    reader.bytes(MAGIC.len())?;
    let mut sections = Vec::new();
    while !reader.done() {
        let id = reader.u8()?;
        let len = reader.u32()? as usize;
        let at = reader.at();
        let contents = reader.bytes(len).map_err(|_| format!("section {id} is truncated"))?;
        sections.push(Section { id, contents, at });
    }
    Ok(sections)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers() {
        let mut reader = Reader::new(&[0xe5, 0x8e, 0x26, 0x7f, 0x80, 0x01]);
        assert_eq!(reader.u32().unwrap(), 624_485);
        assert_eq!(reader.i32().unwrap(), -1);
        assert_eq!(reader.u64().unwrap(), 128);
        assert!(reader.done());
        assert!(reader.u8().is_err());
    }

    #[test]
    fn too_long_or_too_large() {
        assert!(Reader::new(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x00]).u32().is_err());
        assert!(Reader::new(&[0xff, 0xff, 0xff, 0xff, 0x1f]).u32().is_err());
        assert_eq!(Reader::new(&[0xff, 0xff, 0xff, 0xff, 0x0f]).u32().unwrap(), u32::MAX);
        assert!(Reader::new(&[0xff, 0xff, 0xff, 0xff, 0x77]).i32().is_err());
        assert!(Reader::new(&[0x80, 0x80, 0x80, 0x80, 0x08]).i32().is_err());
        assert_eq!(Reader::new(&[0x80, 0x80, 0x80, 0x80, 0x78]).i32().unwrap(), i32::MIN);
        assert_eq!(Reader::new(&[0xc0, 0xbb, 0x78]).i64().unwrap(), -123_456);
    }

    #[test]
    fn names_and_counts() {
        let mut reader = Reader::new(&[2, b'h', b'i', 9]);
        assert_eq!(reader.name().unwrap(), "hi");
        assert!(reader.count().is_err());
        assert!(Reader::new(&[1, 0xff]).name().is_err());
    }

    #[test]
    fn sections_in_order() {
        let mut bytes = MAGIC.to_vec();
        bytes.extend([1, 2, 0xaa, 0xbb, 0, 1, 0]);
        let sections = sections(&bytes).unwrap();
        assert_eq!(sections.len(), 2);
        assert_eq!((sections[0].id, sections[0].contents), (1, &[0xaa, 0xbb][..]));
        assert_eq!(sections[0].range(), 10..12);
        assert_eq!(sections[1].id, 0);
        assert!(super::sections(b"\0asm\x02\0\0\0").is_err());
        bytes.extend([1, 9]);
        assert!(super::sections(&bytes).is_err());
    }
}

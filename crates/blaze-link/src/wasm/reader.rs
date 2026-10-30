//! Reads the wasm binary format: numbers, names, and the sections of a module or an object.

use std::ops::Range;

use blaze_format::leb::{self, LebError};

pub(super) struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    pub(super) fn new(bytes: &'a [u8]) -> Reader<'a> {
        Reader { bytes, at: 0 }
    }

    /// A reader of `bytes` from `at` on, `at` counted from the start of `bytes`.
    pub(super) fn starting_at(bytes: &'a [u8], at: usize) -> Reader<'a> {
        Reader { bytes, at: at.min(bytes.len()) }
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

    fn leb<T>(&mut self, read: fn(&[u8], &mut usize) -> Result<T, LebError>) -> Result<T, String> {
        // alloc: error path
        read(self.bytes, &mut self.at).map_err(|error| error.to_string())
    }

    pub(super) fn u32(&mut self) -> Result<u32, String> {
        self.leb(leb::read_u32)
    }

    pub(super) fn u64(&mut self) -> Result<u64, String> {
        self.leb(leb::read_u64)
    }

    pub(super) fn i32(&mut self) -> Result<i32, String> {
        self.leb(leb::read_i32)
    }

    pub(super) fn i64(&mut self) -> Result<i64, String> {
        self.leb(leb::read_i64)
    }

    /// A count of things that follow, each at least a byte: more than the bytes left is
    /// malformed, and is not allocated for.
    pub(super) fn count(&mut self) -> Result<usize, String> {
        let count = self.u32()? as usize;
        if count > self.bytes.len() - self.at {
            // alloc: error path
            return Err(format!("a count of {count} with {} bytes left", self.bytes.len() - self.at));
        }
        Ok(count)
    }

    pub(super) fn name(&mut self) -> Result<&'a str, String> {
        let len = self.u32()? as usize;
        let bytes = self.bytes(len)?;
        // alloc: error path
        std::str::from_utf8(bytes).map_err(|_| "a name that is not UTF-8".to_owned())
    }

    /// `i32.const value; end`, the offset of an active segment of wasm32.
    pub(super) fn i32_const(&mut self) -> Result<i32, String> {
        match self.u8()? {
            0x41 => {}
            // alloc: error path
            other => return Err(format!("an offset expression of opcode {other:#04x}")),
        }
        let value = self.i32()?;
        match self.u8()? {
            0x0b => Ok(value),
            // alloc: error path
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

/// The sections of a wasm file, in order: one exact allocation, the headers walked twice
/// (the first walk counts and checks them, a few bytes per section).
pub(super) fn sections(bytes: &[u8]) -> Result<Vec<Section<'_>>, String> {
    if !bytes.starts_with(MAGIC) {
        // alloc: error path
        return Err("not a wasm file of version 1".to_owned());
    }
    let mut count = 0;
    each_section(bytes, |_| count += 1)?;
    let mut sections = Vec::with_capacity(count);
    each_section(bytes, |section| sections.push(section))?;
    Ok(sections)
}

fn each_section<'a>(bytes: &'a [u8], mut f: impl FnMut(Section<'a>)) -> Result<(), String> {
    let mut reader = Reader::starting_at(bytes, MAGIC.len());
    while !reader.done() {
        let id = reader.u8()?;
        let len = reader.u32()? as usize;
        let at = reader.at();
        // alloc: error path
        let contents = reader.bytes(len).map_err(|_| format!("section {id} is truncated"))?;
        f(Section { id, contents, at });
    }
    Ok(())
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
    fn reads_allocate_nothing() {
        let numbers =
            [0xe5, 0x8e, 0x26, 0x7f, 0x80, 0x01, 0xc0, 0xbb, 0x78, 0xff, 0xff, 0xff, 0xff, 0x0f];
        let named = [2, b'h', b'i', 0x41, 0x7f, 0x0b, 1, 0];
        let (numbers, rest) = blaze_alloc_count::assert_at_most(0, "LEB128 and name reads", || {
            let mut r = Reader::new(&numbers);
            let numbers = (r.u32(), r.i32(), r.u64(), r.i64(), r.u32());
            let mut r = Reader::new(&named);
            let name = r.name().map(str::len);
            (numbers, (name, r.i32_const(), r.count(), r.bytes(0).map(<[u8]>::len), r.done()))
        });
        assert_eq!(numbers, (Ok(624_485), Ok(-1), Ok(128), Ok(-123_456), Ok(u32::MAX)));
        assert_eq!(rest, (Ok(2), Ok(-1), Ok(1), Ok(0), false));
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

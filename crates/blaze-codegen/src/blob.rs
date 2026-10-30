//! Templates as the store keeps them ([`blaze_cas::Kind::Code`]): little-endian, every length
//! before what it counts, one way to write each template. Reading checks every length against
//! what is left and every tag against what it can be, so bytes that are not a template are an
//! error, never a panic or an allocation of what a length claims.

use blaze_format::SpanRef;
use blaze_ids::ContentId;
use blaze_items::CodeHash;

use crate::{
    Line,
    content::{arch_of, arch_tag, kind_of, kind_tag},
    fold::{Constant, Masked},
    template::{Ref, StaticTemplate, Template, TemplateData, TemplateInit, TemplateReloc, TemplateUnwind},
};

const TEMPLATE_MAGIC: [u8; 4] = *b"BZT1";
const STATIC_MAGIC: [u8; 4] = *b"BZS1";

/// Why bytes are not a template.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct BlobError {
    pub at: usize,
    pub what: &'static str,
}

impl std::fmt::Display for BlobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "not a template at byte {}: {}", self.at, self.what)
    }
}

struct Out(Vec<u8>);

impl Out {
    fn u8(&mut self, it: u8) {
        self.0.push(it);
    }

    fn u32(&mut self, it: u32) {
        self.0.extend_from_slice(&it.to_le_bytes());
    }

    fn i64(&mut self, it: i64) {
        self.0.extend_from_slice(&it.to_le_bytes());
    }

    fn id(&mut self, it: &ContentId) {
        self.0.extend_from_slice(&it.0);
    }

    fn len(&mut self, len: usize) {
        self.u32(u32::try_from(len).expect("a template's parts are under 4 GiB"));
    }

    fn bytes(&mut self, it: &[u8]) {
        self.len(it.len());
        self.0.extend_from_slice(it);
    }

    fn relocs(&mut self, relocs: &[TemplateReloc]) {
        self.len(relocs.len());
        for reloc in relocs {
            self.u32(reloc.at);
            self.u8(kind_tag(reloc.kind));
            self.i64(reloc.addend);
            match reloc.target {
                Ref::Place(at) => {
                    self.u8(0);
                    self.u32(at);
                }
                Ref::Data(at) => {
                    self.u8(1);
                    self.u32(at);
                }
                Ref::Position(SpanRef { anchor, index }) => {
                    self.u8(2);
                    self.id(&anchor);
                    self.u32(index);
                }
            }
        }
    }

    fn data(&mut self, data: &[TemplateData]) {
        self.len(data.len());
        for it in data {
            self.u32(it.align);
            self.bytes(&it.bytes);
            self.relocs(&it.relocs);
        }
    }

    fn index(&mut self, it: Option<u32>) {
        match it {
            Some(at) => {
                self.u8(1);
                self.u32(at);
            }
            None => self.u8(0),
        }
    }

    fn unwind(&mut self, unwind: Option<&TemplateUnwind>) {
        match unwind {
            None => self.u8(0),
            Some(TemplateUnwind::Dwarf { code_align, data_align, return_address, cie, fde, personality, lsda }) => {
                self.u8(1);
                self.u32(*code_align);
                self.0.extend_from_slice(&data_align.to_le_bytes());
                self.0.extend_from_slice(&return_address.to_le_bytes());
                self.bytes(cie);
                self.bytes(fde);
                self.index(*personality);
                self.index(*lsda);
            }
            Some(TemplateUnwind::Windows { info, handler, lsda }) => {
                self.u8(2);
                self.bytes(info);
                self.index(*handler);
                self.index(*lsda);
            }
        }
    }

    fn lines(&mut self, lines: &[Line]) {
        self.len(lines.len());
        for line in lines {
            self.u32(line.at);
            self.id(&line.span.anchor);
            self.u32(line.span.index);
        }
    }
}

struct In<'a> {
    bytes: &'a [u8],
    at: usize,
}

/// The least bytes one reloc, one data object and one inlined hash take: what bounds a count.
const RELOC_BYTES: usize = 4 + 1 + 8 + 1 + 4;
const DATA_BYTES: usize = 4 + 4 + 4;

impl<'a> In<'a> {
    fn fail<T>(&self, what: &'static str) -> Result<T, BlobError> {
        Err(BlobError { at: self.at, what })
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], BlobError> {
        match self.bytes.get(self.at..).and_then(|rest| rest.get(..len)) {
            Some(it) => {
                self.at += len;
                Ok(it)
            }
            None => self.fail("ends early"),
        }
    }

    fn u8(&mut self) -> Result<u8, BlobError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, BlobError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("four bytes")))
    }

    fn i64(&mut self) -> Result<i64, BlobError> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().expect("eight bytes")))
    }

    fn id(&mut self) -> Result<ContentId, BlobError> {
        Ok(ContentId(self.take(32)?.try_into().expect("32 bytes")))
    }

    /// A count of things at least `each` bytes long, checked against what is left.
    fn count(&mut self, each: usize) -> Result<usize, BlobError> {
        let count = self.u32()? as usize;
        match count.checked_mul(each) {
            Some(need) if need <= self.bytes.len() - self.at => Ok(count),
            _ => self.fail("a count longer than what is left"),
        }
    }

    fn bytes(&mut self) -> Result<Vec<u8>, BlobError> {
        let len = self.count(1)?;
        Ok(self.take(len)?.to_vec())
    }

    fn relocs(&mut self) -> Result<Vec<TemplateReloc>, BlobError> {
        let count = self.count(RELOC_BYTES)?;
        let mut relocs = Vec::with_capacity(count);
        for _ in 0..count {
            let at = self.u32()?;
            let Some(kind) = kind_of(self.u8()?) else { return self.fail("no such relocation kind") };
            let addend = self.i64()?;
            let target = match self.u8()? {
                0 => Ref::Place(self.u32()?),
                1 => Ref::Data(self.u32()?),
                2 => Ref::Position(SpanRef { anchor: self.id()?, index: self.u32()? }),
                _ => return self.fail("no such reference"),
            };
            relocs.push(TemplateReloc { at, kind, target, addend });
        }
        Ok(relocs)
    }

    fn data(&mut self) -> Result<Vec<TemplateData>, BlobError> {
        let count = self.count(DATA_BYTES)?;
        let mut data = Vec::with_capacity(count);
        for _ in 0..count {
            let align = self.u32()?;
            let bytes = self.bytes()?;
            let relocs = self.relocs()?;
            data.push(TemplateData { align, bytes, relocs });
        }
        Ok(data)
    }

    fn bool(&mut self) -> Result<bool, BlobError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => self.fail("not a bool"),
        }
    }

    fn index(&mut self) -> Result<Option<u32>, BlobError> {
        Ok(match self.bool()? {
            true => Some(self.u32()?),
            false => None,
        })
    }

    fn unwind(&mut self) -> Result<Option<TemplateUnwind>, BlobError> {
        Ok(match self.u8()? {
            0 => None,
            1 => Some(TemplateUnwind::Dwarf {
                code_align: self.u32()?,
                data_align: i32::from_le_bytes(self.take(4)?.try_into().expect("four bytes")),
                return_address: u16::from_le_bytes(self.take(2)?.try_into().expect("two bytes")),
                cie: self.bytes()?,
                fde: self.bytes()?,
                personality: self.index()?,
                lsda: self.index()?,
            }),
            2 => Some(TemplateUnwind::Windows { info: self.bytes()?, handler: self.index()?, lsda: self.index()? }),
            _ => return self.fail("no such unwind information"),
        })
    }

    fn lines(&mut self) -> Result<Vec<Line>, BlobError> {
        let count = self.count(4 + 32 + 4)?;
        let mut lines = Vec::with_capacity(count);
        for _ in 0..count {
            let at = self.u32()?;
            lines.push(Line { at, span: SpanRef { anchor: self.id()?, index: self.u32()? } });
        }
        Ok(lines)
    }

    fn magic(&mut self, magic: [u8; 4]) -> Result<(), BlobError> {
        if self.take(4)? != magic {
            return self.fail("another kind of blob, or another version of this one");
        }
        Ok(())
    }

    fn end(&self) -> Result<(), BlobError> {
        if self.at != self.bytes.len() {
            return self.fail("bytes after the end");
        }
        Ok(())
    }
}

pub fn encode(template: &Template) -> Vec<u8> {
    let mut out = Out(Vec::with_capacity(64 + template.code.len()));
    out.0.extend_from_slice(&TEMPLATE_MAGIC);
    out.u8(arch_tag(template.arch));
    out.u32(template.align);
    out.bytes(&template.code);
    out.relocs(&template.relocs);
    out.data(&template.data);
    out.len(template.inlined.len());
    for code in &template.inlined {
        out.id(&code.0);
    }
    match &template.side {
        Some(side) => {
            out.u8(1);
            out.id(side);
        }
        None => out.u8(0),
    }
    out.unwind(template.unwind.as_ref());
    out.lines(&template.lines);
    out.u32(template.refs);
    out.0
}

pub fn decode(bytes: &[u8]) -> Result<Template, BlobError> {
    let mut input = In { bytes, at: 0 };
    input.magic(TEMPLATE_MAGIC)?;
    let Some(arch) = arch_of(input.u8()?) else { return input.fail("no such architecture") };
    let align = input.u32()?;
    if !align.is_power_of_two() {
        return input.fail("an alignment that is not a power of two");
    }
    let code = input.bytes()?;
    let relocs = input.relocs()?;
    let data = input.data()?;
    let count = input.count(32)?;
    let inlined = (0..count).map(|_| input.id().map(CodeHash)).collect::<Result<_, _>>()?;
    let side = match input.bool()? {
        true => Some(input.id()?),
        false => None,
    };
    let unwind = input.unwind()?;
    let lines = input.lines()?;
    let refs = input.u32()?;
    input.end()?;
    Ok(Template { arch, align, code, relocs, data, inlined, side, unwind, lines, refs })
}

pub fn encode_static(template: &StaticTemplate) -> Vec<u8> {
    let mut out = Out(Vec::with_capacity(64));
    out.0.extend_from_slice(&STATIC_MAGIC);
    out.u32(template.align);
    match &template.init {
        TemplateInit::Bytes { bytes, relocs } => {
            out.u8(0);
            out.bytes(bytes);
            out.relocs(relocs);
        }
        TemplateInit::Zeroed { size } => {
            out.u8(1);
            out.0.extend_from_slice(&size.to_le_bytes());
        }
    }
    out.data(&template.data);
    out.u8(template.mutable as u8);
    out.u8(template.thread_local as u8);
    match &template.section {
        Some(section) => {
            out.u8(1);
            out.bytes(section.as_bytes());
        }
        None => out.u8(0),
    }
    out.u32(template.refs);
    out.0
}

pub fn decode_static(bytes: &[u8]) -> Result<StaticTemplate, BlobError> {
    let mut input = In { bytes, at: 0 };
    input.magic(STATIC_MAGIC)?;
    let align = input.u32()?;
    if !align.is_power_of_two() {
        return input.fail("an alignment that is not a power of two");
    }
    let init = match input.u8()? {
        0 => TemplateInit::Bytes { bytes: input.bytes()?, relocs: input.relocs()? },
        1 => TemplateInit::Zeroed { size: u64::from_le_bytes(input.take(8)?.try_into().expect("eight bytes")) },
        _ => return input.fail("no such initializer"),
    };
    let data = input.data()?;
    let mutable = input.bool()?;
    let thread_local = input.bool()?;
    let section = match input.bool()? {
        true => match String::from_utf8(input.bytes()?) {
            Ok(it) => Some(it),
            Err(_) => return input.fail("a section name that is not UTF-8"),
        },
        false => None,
    };
    let refs = input.u32()?;
    input.end()?;
    Ok(StaticTemplate { align, init, data, mutable, thread_local, section, refs })
}

const MASKED_MAGIC: [u8; 4] = *b"BZM1";

/// What a function is with its constants masked (#1292), or that it can't be folded: kept per
/// instance, so a warm build groups without building any IR.
pub fn encode_masked(masked: Option<&Masked>) -> Vec<u8> {
    let mut out = Out(MASKED_MAGIC.to_vec());
    let Some(masked) = masked else {
        out.u8(0);
        return out.0;
    };
    out.u8(1);
    out.bytes(&masked.shape);
    out.u32(masked.size);
    out.len(masked.constants.len());
    for constant in &masked.constants {
        match *constant {
            Constant::Int { bits, value } => {
                out.u8(0);
                out.u8(bits);
                out.0.extend_from_slice(&value.to_le_bytes());
            }
            Constant::F32(bits) => {
                out.u8(1);
                out.u32(bits);
            }
            Constant::F64(bits) => {
                out.u8(2);
                out.0.extend_from_slice(&bits.to_le_bytes());
            }
            Constant::Place(at) => {
                out.u8(3);
                out.u32(at);
            }
        }
    }
    out.0
}

pub fn decode_masked(bytes: &[u8]) -> Result<Option<Masked>, BlobError> {
    let mut input = In { bytes, at: 0 };
    input.magic(MASKED_MAGIC)?;
    if !input.bool()? {
        input.end()?;
        return Ok(None);
    }
    let shape = input.bytes()?;
    let size = input.u32()?;
    let count = input.count(5)?;
    let mut constants = Vec::with_capacity(count);
    for _ in 0..count {
        constants.push(match input.u8()? {
            0 => {
                let bits = input.u8()?;
                Constant::Int { bits, value: u128::from_le_bytes(input.take(16)?.try_into().expect("16 bytes")) }
            }
            1 => Constant::F32(input.u32()?),
            2 => Constant::F64(u64::from_le_bytes(input.take(8)?.try_into().expect("8 bytes"))),
            3 => Constant::Place(input.u32()?),
            _ => return input.fail("no such constant"),
        });
    }
    input.end()?;
    Ok(Some(Masked { shape, constants, size }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Arch, RelocKind};

    #[test]
    fn a_masked_shape_reads_back() {
        let masked = Masked {
            shape: vec![1, 2, 3],
            constants: vec![
                Constant::Int { bits: 64, value: 24 },
                Constant::F32(0x3f80_0000),
                Constant::F64(1),
                Constant::Place(4),
            ],
            size: 40,
        };
        assert_eq!(decode_masked(&encode_masked(Some(&masked))).unwrap(), Some(masked));
        assert_eq!(decode_masked(&encode_masked(None)).unwrap(), None);
    }

    fn template() -> Template {
        let reloc = |target| TemplateReloc { at: 3, kind: RelocKind::Call26, target, addend: -8 };
        Template {
            arch: Arch::Aarch64,
            align: 4,
            code: vec![1, 2, 3, 4, 5, 6, 7, 8],
            relocs: vec![
                reloc(Ref::Place(2)),
                reloc(Ref::Data(0)),
                reloc(Ref::Position(SpanRef { anchor: ContentId([4; 32]), index: 9 })),
            ],
            data: vec![TemplateData { align: 8, bytes: vec![9; 8], relocs: vec![reloc(Ref::Place(0))] }],
            inlined: vec![CodeHash(ContentId([5; 32]))],
            side: Some(ContentId([6; 32])),
            unwind: Some(TemplateUnwind::Dwarf {
                code_align: 4,
                data_align: -8,
                return_address: 30,
                cie: vec![0x0c, 31, 0],
                fde: vec![0x44, 0x0e, 16],
                personality: Some(1),
                lsda: Some(0),
            }),
            lines: vec![Line { at: 4, span: SpanRef { anchor: ContentId([7; 32]), index: 3 } }],
            refs: 3,
        }
    }

    #[test]
    fn windows_unwinding_and_none_read_back() {
        let mut it = template();
        it.unwind = Some(TemplateUnwind::Windows { info: vec![1, 2], handler: None, lsda: Some(0) });
        assert_eq!(decode(&encode(&it)).unwrap(), it);
        it.unwind = None;
        it.lines.clear();
        assert_eq!(decode(&encode(&it)).unwrap(), it);
    }

    #[test]
    fn a_template_reads_back() {
        let template = template();
        assert_eq!(decode(&encode(&template)).unwrap(), template);
    }

    #[test]
    fn a_static_reads_back() {
        let reloc = TemplateReloc { at: 8, kind: RelocKind::Abs64, target: Ref::Data(0), addend: 0 };
        let template = StaticTemplate {
            align: 16,
            init: TemplateInit::Bytes { bytes: vec![0; 24], relocs: vec![reloc] },
            data: vec![TemplateData { align: 1, bytes: b"hi".to_vec(), relocs: Vec::new() }],
            mutable: true,
            thread_local: true,
            section: Some("__DATA,__mod_init_func".to_owned()),
            refs: 0,
        };
        let bytes = encode_static(&template);
        assert_eq!(decode_static(&bytes).unwrap(), template);
        assert!(decode(&bytes).is_err());
        for len in 0..bytes.len() {
            assert!(decode_static(&bytes[..len]).is_err(), "{len} bytes read as a static");
        }
        let zeroed = StaticTemplate { init: TemplateInit::Zeroed { size: 1 << 30 }, section: None, ..template };
        assert_eq!(decode_static(&encode_static(&zeroed)).unwrap(), zeroed);
    }

    #[test]
    fn every_cut_is_an_error() {
        let bytes = encode(&template());
        for len in 0..bytes.len() {
            assert!(decode(&bytes[..len]).is_err(), "{len} bytes read as a template");
        }
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(decode(&longer).is_err());
    }

    #[test]
    fn a_count_past_the_end_allocates_nothing() {
        let mut bytes = TEMPLATE_MAGIC.to_vec();
        bytes.push(0);
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(decode(&bytes).unwrap_err().what, "a count longer than what is left");
    }

    #[test]
    fn every_flipped_byte_is_an_error_or_another_template() {
        let template = template();
        let bytes = encode(&template);
        for at in 0..bytes.len() {
            let mut flipped = bytes.clone();
            flipped[at] ^= 0xff;
            if let Ok(other) = decode(&flipped) {
                assert_ne!(other, template);
                assert_eq!(encode(&other), flipped);
            }
        }
    }
}

//! Call frame information: the `.eh_frame` records a panic unwinds by.
//!
//! Each function with [`blaze_codegen::Unwind::Dwarf`] gets an FDE, and the CIE its fields
//! name (one per distinct CIE, shared). Native objects' `.eh_frame` sections are split into
//! their records, each kept as long as the code it describes is. Records are 4-aligned and a
//! multiple of 4 long, so placed one after another they leave no gap an unwinder would read
//! as the terminator.

use std::ops::Range;

use blaze_format::cursor::{self, Put, Reader};

/// A record's bytes and the places in it the core relocates.
#[cfg(test)]
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Record {
    pub bytes: Vec<u8>,
    pub fixups: Fields,
}

/// A record's relocated fields, at most three (an FDE's CIE pointer, function and LSDA),
/// kept inline: making a record allocates nothing but what its bytes outgrow.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Fields {
    items: [(u64, Field); 3],
    len: u8,
}

impl Fields {
    const NONE: Fields = Fields { items: [(0, Field::CiePointer); 3], len: 0 };

    fn push(&mut self, at: u64, field: Field) {
        self.items[self.len as usize] = (at, field);
        self.len += 1;
    }

    pub fn iter(&self) -> std::slice::Iter<'_, (u64, Field)> {
        self.items[..self.len as usize].iter()
    }

    pub fn len(&self) -> usize {
        self.len as usize
    }
}

/// What a field of a record is relocated to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Field {
    /// An FDE's CIE pointer: from the field back to its CIE (`Delta32` of the record's own
    /// address + 4, minus the CIE's).
    CiePointer,
    /// An FDE's first address, PC-relative (`Rel32`) to the function.
    Function,
    /// An FDE's LSDA, PC-relative to the function's data.
    Lsda,
    /// A CIE's personality: PC-relative to a GOT cell holding the routine's address
    /// (`DW_EH_PE_indirect`).
    Personality,
}

const PCREL_SDATA4: u8 = 0x1b;
const INDIRECT_PCREL_SDATA4: u8 = 0x9b;

/// A CIE: version 1, augmentation `zR`, or `zPLR` for one with a personality.
#[cfg(test)]
pub(crate) fn cie(
    code_align: u32,
    data_align: i32,
    return_address: u16,
    instructions: &[u8],
    personality: bool,
) -> Result<Record, String> {
    let mut bytes = Vec::with_capacity(64);
    let fixups = cie_into(code_align, data_align, return_address, instructions, personality, &mut bytes)?;
    Ok(Record { bytes, fixups })
}

/// An FDE, as [`fde_into`] writes it.
#[cfg(test)]
pub(crate) fn fde(size: u64, instructions: &[u8], lsda: bool, has_lsda: bool) -> Result<Record, String> {
    let mut bytes = Vec::with_capacity(64);
    let fixups = fde_into(size, instructions, lsda, has_lsda, &mut bytes)?;
    Ok(Record { bytes, fixups })
}

/// Writes a CIE over `bytes` (version 1, augmentation `zR`, or `zPLR` for one with a
/// personality): its fields to relocate. A reused buffer allocates nothing.
pub(crate) fn cie_into(
    code_align: u32,
    data_align: i32,
    return_address: u16,
    instructions: &[u8],
    personality: bool,
    bytes: &mut Vec<u8>,
) -> Result<Fields, String> {
    let ra = u8::try_from(return_address)
        // alloc: error path
        .map_err(|_| format!("return address register {return_address} past CIE version 1's 255"))?;
    // Header 8, version 1, augmentation 6, alignments 5 + 10, register 1, augmentation data
    // 9, then the instructions and at most 7 of padding.
    bytes.clear();
    bytes.reserve(40 + instructions.len() + 7);
    bytes.extend_from_slice(&[0; 8]);
    bytes.push(1);
    bytes.extend_from_slice(if personality { b"zPLR\0" } else { b"zR\0" });
    uleb(bytes, u64::from(code_align));
    sleb(bytes, i64::from(data_align));
    bytes.push(ra);
    let mut fixups = Fields::NONE;
    if personality {
        uleb(bytes, 7);
        bytes.push(INDIRECT_PCREL_SDATA4);
        fixups.push(bytes.len() as u64, Field::Personality);
        bytes.extend_from_slice(&[0; 4]);
        bytes.push(PCREL_SDATA4);
        bytes.push(PCREL_SDATA4);
    } else {
        uleb(bytes, 1);
        bytes.push(PCREL_SDATA4);
    }
    bytes.extend_from_slice(instructions);
    finish(bytes)?;
    Ok(fixups)
}

/// Writes over `bytes` an FDE of a function `size` bytes long, its CIE pointer and
/// addresses left to fixups: its fields to relocate. `lsda` says whether its CIE has an `L`
/// augmentation (one with a personality does): then the field is there, relocated when
/// `has_lsda`, 0 (none) otherwise. A reused buffer allocates nothing.
pub(crate) fn fde_into(
    size: u64,
    instructions: &[u8],
    lsda: bool,
    has_lsda: bool,
    bytes: &mut Vec<u8>,
) -> Result<Fields, String> {
    let range = u32::try_from(size).map_err(|_| "a function past 4 GiB in an FDE")?;
    bytes.clear();
    bytes.reserve(16 + 1 + 4 + instructions.len() + 7);
    bytes.extend_from_slice(&[0; 12]);
    bytes.put_u32(range);
    let mut fixups = Fields::NONE;
    fixups.push(4, Field::CiePointer);
    fixups.push(8, Field::Function);
    if lsda {
        uleb(bytes, 4);
        if has_lsda {
            fixups.push(bytes.len() as u64, Field::Lsda);
        }
        bytes.extend_from_slice(&[0; 4]);
    } else {
        uleb(bytes, 0);
    }
    bytes.extend_from_slice(instructions);
    finish(bytes)?;
    Ok(fixups)
}

/// Pads with `DW_CFA_nop` to a multiple of 8 and writes the length.
fn finish(bytes: &mut Vec<u8>) -> Result<(), String> {
    bytes.resize(bytes.len().next_multiple_of(8), 0);
    let len = u32::try_from(bytes.len() - 4).map_err(|_| "a frame record past 4 GiB")?;
    if len == u32::MAX {
        // alloc: error path
        return Err("a frame record as long as the 64-bit length escape".to_owned());
    }
    bytes.patch_u32(0, len);
    Ok(())
}

pub(crate) use blaze_format::leb::{write_i64 as sleb, write_u64 as uleb};

/// A record of a native `.eh_frame` section.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Native {
    pub range: Range<usize>,
    /// `None` for a CIE; for an FDE, the start of its CIE in the section.
    pub cie: Option<usize>,
}

/// Splits a native `.eh_frame` into its records, stopping at a terminator. An FDE's CIE is
/// found among the records before it by binary search (they are met in order).
pub(crate) fn split(section: &[u8]) -> Result<Vec<Native>, String> {
    // Compilers' records are mostly 24 to 64 bytes.
    let mut records: Vec<Native> = Vec::with_capacity(section.len() / 32);
    let mut read = Reader::new(section);
    while read.left() >= 4 {
        let at = read.at();
        let len = read.u32()? as usize;
        if len == 0 {
            break;
        }
        if len == u32::MAX as usize {
            // alloc: error path
            return Err("an .eh_frame record with a 64-bit length".to_owned());
        }
        let Some(body) = read.take(len).ok().filter(|_| len >= 4) else {
            // alloc: error path
            return Err(format!("an .eh_frame record at {at:#x} past the section's end"));
        };
        let end = read.at();
        let id = Reader::new(body).u32()? as usize;
        let cie = match id {
            0 => None,
            back => Some(
                // alloc: error path
                (at + 4).checked_sub(back).ok_or_else(|| format!("an FDE at {at:#x} before its CIE"))?,
            ),
        };
        if let Some(cie) = cie {
            let found = records.binary_search_by_key(&cie, |it| it.range.start);
            if !found.is_ok_and(|it| records[it].cie.is_none()) {
                // alloc: error path
                return Err(format!("an FDE at {at:#x} whose CIE at {cie:#x} is no CIE before it"));
            }
        }
        records.push(Native { range: at..end, cie });
    }
    Ok(records)
}

pub(crate) const DW_EH_PE_OMIT: u8 = 0xff;
const DW_EH_PE_PCREL: u8 = 0x10;

/// The FDE pointer encoding of the CIE `reader` is in (past its ID), what leads to it checked
/// as gimli checks it. `reader`'s positions are from the start of the section at `section`.
pub(crate) fn cie_encoding(reader: &mut Reader<'_>, section: u64) -> Result<u8, &'static str> {
    const UNKNOWN: &str = "a CIE of an unknown augmentation";
    let version = reader.u8().map_err(cursor::Error::as_str)?;
    if !matches!(version, 1 | 3 | 4) {
        return Err("a CIE of an unknown version");
    }
    let augmentation = reader.cstr().map_err(cursor::Error::as_str)?;
    reader.uleb().map_err(cursor::Error::as_str)?;
    reader.sleb().map_err(cursor::Error::as_str)?;
    if version == 1 {
        reader.u8().map_err(cursor::Error::as_str)?;
    } else {
        reader.uleb().map_err(cursor::Error::as_str)?;
    }
    let mut encoding = 0;
    let mut data: Option<Reader<'_>> = None;
    for (index, &ch) in augmentation.iter().enumerate() {
        match ch {
            b'z' if index == 0 => {
                let len = reader.uleb().map_err(cursor::Error::as_str)?;
                let start = reader.at();
                usize::try_from(len)
                    .ok()
                    .and_then(|it| reader.skip(it).ok())
                    .ok_or("augmentation data longer than its CIE")?;
                let bytes = &reader.bytes()[..reader.at()];
                data = Some(Reader::starting_at(bytes, start).map_err(cursor::Error::as_str)?);
            }
            b'L' => {
                let data = data.as_mut().ok_or(UNKNOWN)?;
                pointer_encoding(data.u8().map_err(cursor::Error::as_str)?)?;
            }
            b'P' => {
                let data = data.as_mut().ok_or(UNKNOWN)?;
                let personality = pointer_encoding(data.u8().map_err(cursor::Error::as_str)?)?;
                eh_pointer(data, personality, section)?;
            }
            b'R' => {
                let data = data.as_mut().ok_or(UNKNOWN)?;
                encoding = pointer_encoding(data.u8().map_err(cursor::Error::as_str)?)?;
            }
            b'S' => {}
            _ => return Err(UNKNOWN),
        }
    }
    Ok(encoding)
}

/// `encoding` if it is a `DW_EH_PE_*` pointer encoding.
fn pointer_encoding(encoding: u8) -> Result<u8, &'static str> {
    let format = matches!(encoding & 0x0f, 0x00..=0x04 | 0x09..=0x0c);
    if encoding == DW_EH_PE_OMIT || (format && encoding & 0x70 <= 0x50) {
        Ok(encoding)
    } else {
        Err("an unknown pointer encoding")
    }
}

/// A value in `encoding`'s format, signed ones sign-extended.
pub(crate) fn eh_value(reader: &mut Reader<'_>, encoding: u8) -> Result<u64, &'static str> {
    let value = match encoding & 0x0f {
        0x00 | 0x04 | 0x0c => reader.u64(),
        0x01 => reader.uleb(),
        0x02 => reader.u16().map(u64::from),
        0x03 => reader.u32().map(u64::from),
        0x09 => reader.sleb().map(|it| it as u64),
        0x0a => reader.i16().map(|it| it as u64),
        0x0b => reader.i32().map(|it| it as u64),
        _ => return Err("an unknown pointer encoding"),
    };
    value.map_err(cursor::Error::as_str)
}

/// A pointer in `encoding` with only the section's address (`section`) known: absolute or
/// relative to where it is.
pub(crate) fn eh_pointer(reader: &mut Reader<'_>, encoding: u8, section: u64) -> Result<u64, &'static str> {
    if encoding == DW_EH_PE_OMIT {
        return Err("a pointer whose encoding says it is omitted");
    }
    let base = match encoding & 0x70 {
        0x00 => 0,
        DW_EH_PE_PCREL => section.wrapping_add(reader.at() as u64),
        _ => return Err("a pointer relative to a base other than where it is"),
    };
    Ok(base.wrapping_add(eh_value(reader, encoding)?))
}

/// The offsets in an image's `.eh_frame` (at `section`, `len` bytes) of every `every`th FDE
/// the core placed, sorted: record starts a walk of the section can begin at, so its pieces are
/// walked in parallel. None below two pieces' worth of functions.
pub(crate) fn fde_splits(placed: &crate::model::Placed<'_>, section: u64, len: usize, every: usize) -> Vec<usize> {
    if placed.functions.len() < 2 * every {
        // alloc: empty, which allocates nothing
        return Vec::new();
    }
    // alloc: once per image, one offset per `every` functions
    let mut splits = Vec::with_capacity(placed.functions.len() / every + 1);
    splits.extend(
        (placed.functions.iter())
            .filter_map(|it| it.fde?.checked_sub(section))
            .filter(|&it| it > 0 && it < len as u64)
            .step_by(every)
            .map(|it| it as usize),
    );
    splits.sort_unstable();
    splits.dedup();
    splits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_are_padded_and_split_back() {
        let cie = cie(1, -8, 16, &[0x0c, 7, 8, 0x90, 1], true).unwrap();
        assert_eq!(cie.bytes.len() % 8, 0);
        assert_eq!(&cie.bytes[8..14], b"\x01zPLR\0");
        let fde = fde(0x40, &[0x41, 0x0e, 0x10], true, false).unwrap();
        assert_eq!(fde.bytes.len() % 8, 0);
        assert_eq!(u32::from_le_bytes(fde.bytes[12..16].try_into().unwrap()), 0x40);
        assert!(!fde.fixups.iter().any(|it| it.1 == Field::Lsda));

        let mut section = cie.bytes.clone();
        let mut first = fde.bytes.clone();
        let back = (section.len() + 4) as u32;
        first[4..8].copy_from_slice(&back.to_le_bytes());
        section.extend_from_slice(&first);
        section.extend_from_slice(&[0; 4]);
        let records = split(&section).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0], Native { range: 0..cie.bytes.len(), cie: None });
        assert_eq!(records[1].cie, Some(0));
    }

    #[test]
    fn an_fde_without_its_cie_is_refused() {
        let mut fde = fde(4, &[], false, false).unwrap().bytes;
        fde[4..8].copy_from_slice(&4u32.to_le_bytes());
        assert!(split(&fde).is_err());
    }

    /// Many CIEs, each FDE naming the latest one: the binary search finds every CIE, and an
    /// FDE pointing at another FDE is refused.
    #[test]
    fn fdes_find_their_cies_among_many() {
        let mut section = Vec::new();
        let mut want = Vec::new();
        for n in 0..50u64 {
            let start = section.len();
            section.extend_from_slice(&cie(1, -8, 16, &[], false).unwrap().bytes);
            want.push(None);
            for _ in 0..3 {
                let mut record = fde(n + 1, &[], false, false).unwrap();
                assert_eq!(record.fixups.len(), 2);
                let back = (section.len() + 4 - start) as u32;
                record.bytes[4..8].copy_from_slice(&back.to_le_bytes());
                section.extend_from_slice(&record.bytes);
                want.push(Some(start));
            }
        }
        let records = split(&section).unwrap();
        assert_eq!(records.iter().map(|it| it.cie).collect::<Vec<_>>(), want);

        let mut bad = cie(1, -8, 16, &[], false).unwrap().bytes;
        let first = bad.len();
        let mut fde_one = fde(4, &[], false, false).unwrap().bytes;
        fde_one[4..8].copy_from_slice(&((first + 4) as u32).to_le_bytes());
        bad.extend_from_slice(&fde_one);
        let mut fde_two = fde(4, &[], false, false).unwrap().bytes;
        fde_two[4..8].copy_from_slice(&((bad.len() + 4 - first) as u32).to_le_bytes());
        bad.extend_from_slice(&fde_two);
        assert!(split(&bad).unwrap_err().contains("no CIE"));
    }
}

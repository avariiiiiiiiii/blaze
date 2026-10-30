//! Call frame information: the `.eh_frame` records a panic unwinds by.
//!
//! Each function with [`blaze_codegen::Unwind::Dwarf`] gets an FDE, and the CIE its fields
//! name (one per distinct CIE, shared). Native objects' `.eh_frame` sections are split into
//! their records, each kept as long as the code it describes is. Records are 4-aligned and a
//! multiple of 4 long, so placed one after another they leave no gap an unwinder would read
//! as the terminator.

use std::ops::Range;

/// A record's bytes and the places in it the core relocates.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Record {
    pub bytes: Vec<u8>,
    pub fixups: Vec<(u64, Field)>,
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
pub(crate) fn cie(
    code_align: u32,
    data_align: i32,
    return_address: u16,
    instructions: &[u8],
    personality: bool,
) -> Result<Record, String> {
    let ra = u8::try_from(return_address)
        .map_err(|_| format!("return address register {return_address} past CIE version 1's 255"))?;
    let mut bytes = vec![0; 8];
    bytes.push(1);
    bytes.extend_from_slice(if personality { b"zPLR\0" } else { b"zR\0" });
    uleb(&mut bytes, u64::from(code_align));
    sleb(&mut bytes, i64::from(data_align));
    bytes.push(ra);
    let mut fixups = Vec::new();
    if personality {
        uleb(&mut bytes, 7);
        bytes.push(INDIRECT_PCREL_SDATA4);
        fixups.push((bytes.len() as u64, Field::Personality));
        bytes.extend_from_slice(&[0; 4]);
        bytes.push(PCREL_SDATA4);
        bytes.push(PCREL_SDATA4);
    } else {
        uleb(&mut bytes, 1);
        bytes.push(PCREL_SDATA4);
    }
    bytes.extend_from_slice(instructions);
    finish(&mut bytes)?;
    Ok(Record { bytes, fixups })
}

/// An FDE of a function `size` bytes long, its CIE pointer and addresses left to fixups.
/// `lsda` says whether its CIE has an `L` augmentation (one with a personality does): then
/// the field is there, relocated when `has_lsda`, 0 (none) otherwise.
pub(crate) fn fde(
    size: u64,
    instructions: &[u8],
    lsda: bool,
    has_lsda: bool,
) -> Result<Record, String> {
    let range = u32::try_from(size).map_err(|_| "a function past 4 GiB in an FDE")?;
    let mut bytes = vec![0; 16];
    bytes[12..16].copy_from_slice(&range.to_le_bytes());
    let mut fixups = vec![(4, Field::CiePointer), (8, Field::Function)];
    if lsda {
        uleb(&mut bytes, 4);
        if has_lsda {
            fixups.push((bytes.len() as u64, Field::Lsda));
        }
        bytes.extend_from_slice(&[0; 4]);
    } else {
        uleb(&mut bytes, 0);
    }
    bytes.extend_from_slice(instructions);
    finish(&mut bytes)?;
    Ok(Record { bytes, fixups })
}

/// Pads with `DW_CFA_nop` to a multiple of 8 and writes the length.
fn finish(bytes: &mut Vec<u8>) -> Result<(), String> {
    while bytes.len() % 8 != 0 {
        bytes.push(0);
    }
    let len = u32::try_from(bytes.len() - 4).map_err(|_| "a frame record past 4 GiB")?;
    if len == u32::MAX {
        return Err("a frame record as long as the 64-bit length escape".to_owned());
    }
    bytes[..4].copy_from_slice(&len.to_le_bytes());
    Ok(())
}

fn uleb(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn sleb(out: &mut Vec<u8>, mut value: i64) {
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

/// A record of a native `.eh_frame` section.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Native {
    pub range: Range<usize>,
    /// `None` for a CIE; for an FDE, the start of its CIE in the section.
    pub cie: Option<usize>,
}

/// Splits a native `.eh_frame` into its records, stopping at a terminator.
pub(crate) fn split(section: &[u8]) -> Result<Vec<Native>, String> {
    let mut records = Vec::new();
    let mut at = 0;
    while at + 4 <= section.len() {
        let word = |at: usize| -> Result<u32, String> {
            let bytes = section.get(at..at + 4).ok_or("an .eh_frame record cut short")?;
            Ok(u32::from_le_bytes(bytes.try_into().expect("four bytes")))
        };
        let len = word(at)? as usize;
        if len == 0 {
            break;
        }
        if len == u32::MAX as usize {
            return Err("an .eh_frame record with a 64-bit length".to_owned());
        }
        let end = at + 4 + len;
        if end > section.len() || len < 4 {
            return Err(format!("an .eh_frame record at {at:#x} past the section's end"));
        }
        let id = word(at + 4)? as usize;
        let cie = match id {
            0 => None,
            back => Some(
                (at + 4).checked_sub(back).ok_or(format!("an FDE at {at:#x} before its CIE"))?,
            ),
        };
        if let Some(cie) = cie
            && !records.iter().any(|it: &Native| it.range.start == cie && it.cie.is_none())
        {
            return Err(format!("an FDE at {at:#x} whose CIE at {cie:#x} is no CIE before it"));
        }
        records.push(Native { range: at..end, cie });
        at = end;
    }
    Ok(records)
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

    #[test]
    fn leb128_encodes_both_signs() {
        let mut out = Vec::new();
        sleb(&mut out, -8);
        sleb(&mut out, 63);
        sleb(&mut out, 64);
        uleb(&mut out, 300);
        assert_eq!(out, [0x78, 0x3f, 0xc0, 0x00, 0xac, 0x02]);
    }
}

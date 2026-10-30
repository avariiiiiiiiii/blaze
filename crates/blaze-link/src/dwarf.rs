//! The debug info a link makes from the functions' line tables and the span side tables:
//! one compilation unit with a line program over every function and a subprogram for each,
//! so a debugger breaks by line and a backtrace names frames. Written anew by each link from
//! what is placed, so it is never stale.
//!
//! DWARF 4, 32-bit, encoded straight into one buffer per section, each sized up front from the
//! function, row and file counts: no tree of entries, no per-row or per-file allocation. A row
//! is one special opcode when its line and address advance fit one (most do), with
//! `DW_LNS_set_file` and `DW_LNS_set_column` only where those change.

use std::ops::Range;

use crate::unwind::{sleb, uleb};

/// A placed function's debug view: its rows are `rows[lines]` of [`sections`]' rows.
pub(crate) struct DebugFunction<'a> {
    pub addr: u64,
    pub size: u64,
    pub name: Option<&'a str>,
    pub lines: Range<u32>,
}

/// A line-table row: from code offset `at` of its function, `file` (an index into
/// [`sections`]' files), its line and column. 16 bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Row {
    pub at: u32,
    pub file: u32,
    pub line: u32,
    pub column: u32,
}

const _: () = assert!(size_of::<Row>() == 16);

const DW_TAG_COMPILE_UNIT: u8 = 0x11;
const DW_TAG_SUBPROGRAM: u8 = 0x2e;
const DW_AT_NAME: u8 = 0x03;
const DW_AT_STMT_LIST: u8 = 0x10;
const DW_AT_LOW_PC: u8 = 0x11;
const DW_AT_HIGH_PC: u8 = 0x12;
const DW_AT_LANGUAGE: u8 = 0x13;
const DW_AT_COMP_DIR: u8 = 0x1b;
const DW_AT_PRODUCER: u8 = 0x25;
const DW_AT_LINKAGE_NAME: u8 = 0x6e;
const DW_FORM_ADDR: u8 = 0x01;
const DW_FORM_DATA2: u8 = 0x05;
const DW_FORM_STRING: u8 = 0x08;
const DW_FORM_STRP: u8 = 0x0e;
const DW_FORM_UDATA: u8 = 0x0f;
const DW_FORM_SEC_OFFSET: u8 = 0x17;
const DW_LANG_RUST: u16 = 0x1c;

const DW_LNS_COPY: u8 = 0x01;
const DW_LNS_ADVANCE_PC: u8 = 0x02;
const DW_LNS_ADVANCE_LINE: u8 = 0x03;
const DW_LNS_SET_FILE: u8 = 0x04;
const DW_LNS_SET_COLUMN: u8 = 0x05;
const DW_LNE_END_SEQUENCE: u8 = 0x01;
const DW_LNE_SET_ADDRESS: u8 = 0x02;

/// The line program's encoding: as gimli's and LLVM's default.
const LINE_BASE: i64 = -5;
const LINE_RANGE: u64 = 14;
const OPCODE_BASE: u8 = 13;
const STANDARD_LENGTHS: [u8; 12] = [0, 1, 1, 1, 1, 0, 0, 0, 1, 0, 0, 1];

/// Abbreviation codes: the unit, a named subprogram, an unnamed one.
const UNIT: u8 = 1;
const NAMED: u8 = 2;
const UNNAMED: u8 = 3;

const PRODUCER: &[u8] = b"blaze\0";

/// The DWARF sections, by name (`.debug_info`), of the functions; empty sections left out.
pub(crate) fn sections(
    functions: &[DebugFunction<'_>],
    rows: &[Row],
    files: &[&str],
    address_size: u8,
) -> Result<Vec<(&'static str, Vec<u8>)>, String> {
    for file in files {
        if file.is_empty() || file.contains('\0') {
            return Err(format!("a span in the file {file:?}, which DWARF can't name"));
        }
    }
    let mut order: Vec<u32> = (0..functions.len() as u32).collect();
    order.sort_unstable_by_key(|it| functions[*it as usize].addr);
    let asz = usize::from(address_size);
    let addr = |out: &mut Vec<u8>, value: u64| -> Result<(), String> {
        match address_size {
            4 => out.extend_from_slice(
                &u32::try_from(value).map_err(|_| format!("the address {value:#x} past 32 bits"))?.to_le_bytes(),
            ),
            8 => out.extend_from_slice(&value.to_le_bytes()),
            other => return Err(format!("an address size of {other} bytes")),
        }
        Ok(())
    };

    let names: usize = functions.iter().filter_map(|it| it.name).map(|it| it.len() + 1).sum();
    let mut strings = Vec::with_capacity(PRODUCER.len() + names);
    strings.extend_from_slice(PRODUCER);

    let mut info = Vec::with_capacity(11 + 32 + asz + functions.len() * (1 + 4 + asz + 5) + 1);
    info.extend_from_slice(&[0; 4]);
    info.extend_from_slice(&4u16.to_le_bytes());
    info.extend_from_slice(&0u32.to_le_bytes());
    info.push(address_size);
    info.push(UNIT);
    info.extend_from_slice(&0u32.to_le_bytes());
    info.extend_from_slice(&DW_LANG_RUST.to_le_bytes());
    info.extend_from_slice(b"blaze\0");
    info.extend_from_slice(b".\0");
    info.extend_from_slice(&0u32.to_le_bytes());
    let (low, high) = match (order.first(), order.last()) {
        (Some(first), Some(last)) => {
            let (first, last) = (&functions[*first as usize], &functions[*last as usize]);
            (first.addr, last.addr + last.size - first.addr)
        }
        _ => (0, 0),
    };
    addr(&mut info, low)?;
    uleb(&mut info, high);
    for at in &order {
        let function = &functions[*at as usize];
        match function.name {
            Some(name) => {
                if name.contains('\0') {
                    return Err(format!("the function name {name:?}, which DWARF can't hold"));
                }
                info.push(NAMED);
                let offset = u32::try_from(strings.len()).map_err(|_| "debug strings past 4 GiB")?;
                info.extend_from_slice(&offset.to_le_bytes());
                strings.extend_from_slice(name.as_bytes());
                strings.push(0);
            }
            None => info.push(UNNAMED),
        }
        addr(&mut info, function.addr)?;
        uleb(&mut info, function.size);
    }
    info.push(0);
    let len = u32::try_from(info.len() - 4).map_err(|_| "debug info past 4 GiB")?;
    info[..4].copy_from_slice(&len.to_le_bytes());

    let file_bytes: usize = files.iter().map(|it| it.len() + 4).sum();
    let program = 4 * rows.len() + functions.len() * (3 + asz + 14);
    let mut line = Vec::with_capacity(10 + 6 + STANDARD_LENGTHS.len() + 2 + file_bytes + program);
    line.extend_from_slice(&[0; 4]);
    line.extend_from_slice(&4u16.to_le_bytes());
    line.extend_from_slice(&[0; 4]);
    let header = line.len();
    line.extend_from_slice(&[1, 1, 1, LINE_BASE as i8 as u8, LINE_RANGE as u8, OPCODE_BASE]);
    line.extend_from_slice(&STANDARD_LENGTHS);
    line.push(0);
    for file in files {
        line.extend_from_slice(file.as_bytes());
        line.extend_from_slice(&[0, 0, 0, 0]);
    }
    line.push(0);
    let header_len = u32::try_from(line.len() - header).map_err(|_| "a line program header past 4 GiB")?;
    line[6..10].copy_from_slice(&header_len.to_le_bytes());
    for at in &order {
        let function = &functions[*at as usize];
        let rows = rows
            .get(function.lines.start as usize..function.lines.end as usize)
            .ok_or("a function's lines past the line table")?;
        if rows.is_empty() {
            continue;
        }
        line.extend_from_slice(&[0, 1 + address_size, DW_LNE_SET_ADDRESS]);
        addr(&mut line, function.addr)?;
        let (mut offset, mut file, mut number, mut column) = (0u64, 1u64, 1i64, 0u64);
        for row in rows {
            let at = u64::from(row.at);
            if at >= function.size || at < offset {
                return Err(format!(
                    "a line at offset {at:#x} of a function {:#x} bytes long, or out of order",
                    function.size
                ));
            }
            let this_file = u64::from(row.file) + 1;
            if row.file as usize >= files.len() {
                return Err(format!("a line in file {} of {}", row.file, files.len()));
            }
            if this_file != file {
                line.push(DW_LNS_SET_FILE);
                uleb(&mut line, this_file);
                file = this_file;
            }
            if u64::from(row.column) != column {
                column = u64::from(row.column);
                line.push(DW_LNS_SET_COLUMN);
                uleb(&mut line, column);
            }
            let advance = at - offset;
            let delta = i64::from(row.line) - number;
            let special = (LINE_BASE..LINE_BASE + LINE_RANGE as i64)
                .contains(&delta)
                .then(|| (delta - LINE_BASE) as u64 + LINE_RANGE * advance + u64::from(OPCODE_BASE))
                .filter(|it| *it <= 255);
            if let Some(special) = special {
                line.push(special as u8);
            } else {
                if delta != 0 {
                    line.push(DW_LNS_ADVANCE_LINE);
                    sleb(&mut line, delta);
                }
                if advance != 0 {
                    line.push(DW_LNS_ADVANCE_PC);
                    uleb(&mut line, advance);
                }
                line.push(DW_LNS_COPY);
            }
            offset = at;
            number = i64::from(row.line);
        }
        if function.size > offset {
            line.push(DW_LNS_ADVANCE_PC);
            uleb(&mut line, function.size - offset);
        }
        line.extend_from_slice(&[0, 1, DW_LNE_END_SEQUENCE]);
    }
    let len = u32::try_from(line.len() - 4).map_err(|_| "a line program past 4 GiB")?;
    line[..4].copy_from_slice(&len.to_le_bytes());

    let mut abbrev = Vec::with_capacity(48);
    abbrev.extend_from_slice(&[UNIT, DW_TAG_COMPILE_UNIT, 1]);
    abbrev.extend_from_slice(&[
        DW_AT_PRODUCER,
        DW_FORM_STRP,
        DW_AT_LANGUAGE,
        DW_FORM_DATA2,
        DW_AT_NAME,
        DW_FORM_STRING,
        DW_AT_COMP_DIR,
        DW_FORM_STRING,
        DW_AT_STMT_LIST,
        DW_FORM_SEC_OFFSET,
        DW_AT_LOW_PC,
        DW_FORM_ADDR,
        DW_AT_HIGH_PC,
        DW_FORM_UDATA,
        0,
        0,
    ]);
    abbrev.extend_from_slice(&[NAMED, DW_TAG_SUBPROGRAM, 0]);
    abbrev.extend_from_slice(&[DW_AT_LINKAGE_NAME, DW_FORM_STRP, DW_AT_LOW_PC, DW_FORM_ADDR, DW_AT_HIGH_PC, DW_FORM_UDATA, 0, 0]);
    abbrev.extend_from_slice(&[UNNAMED, DW_TAG_SUBPROGRAM, 0]);
    abbrev.extend_from_slice(&[DW_AT_LOW_PC, DW_FORM_ADDR, DW_AT_HIGH_PC, DW_FORM_UDATA, 0, 0]);
    abbrev.push(0);

    Ok(vec![
        (".debug_abbrev", abbrev),
        (".debug_info", info),
        (".debug_line", line),
        (".debug_str", strings),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use gimli::{EndianSlice, LittleEndian};

    type Seen = Vec<(u64, Option<u64>, u64, u64)>;

    /// Reads the sections back with gimli: every row (address, line, file, column), and each
    /// subprogram's name and range.
    fn read(sections: &[(&'static str, Vec<u8>)], address_size: u8) -> (Seen, Vec<(Option<String>, u64, u64)>) {
        let get = |name: &str| sections.iter().find(|it| it.0 == name).map_or(&[][..], |it| &it.1[..]);
        let dwarf = gimli::Dwarf::load(|id| -> Result<_, ()> { Ok(EndianSlice::new(get(id.name()), LittleEndian)) })
            .unwrap();
        let mut units = dwarf.units();
        let header = units.next().unwrap().unwrap();
        assert_eq!(header.address_size(), address_size);
        let unit = dwarf.unit(header).unwrap();
        let program = unit.line_program.clone().unwrap();
        let mut rows = program.rows();
        let mut seen = Vec::new();
        while let Some((_, row)) = rows.next_row().unwrap() {
            if !row.end_sequence() {
                let column = match row.column() {
                    gimli::ColumnType::LeftEdge => 0,
                    gimli::ColumnType::Column(it) => it.get(),
                };
                seen.push((row.address(), row.line().map(|it| it.get()), row.file_index(), column));
            }
        }
        let mut subprograms = Vec::new();
        let mut entries = unit.entries();
        while let Some(entry) = entries.next_dfs().unwrap() {
            if entry.tag() != gimli::DW_TAG_subprogram {
                continue;
            }
            let name = entry
                .attr_value(gimli::DW_AT_linkage_name)
                .map(|it| dwarf.attr_string(&unit, it).unwrap().to_string_lossy().into_owned());
            let low = match entry.attr_value(gimli::DW_AT_low_pc) {
                Some(gimli::AttributeValue::Addr(it)) => it,
                other => panic!("low pc {other:?}"),
            };
            let size = entry.attr_value(gimli::DW_AT_high_pc).and_then(|it| it.udata_value()).unwrap();
            subprograms.push((name, low, size));
        }
        (seen, subprograms)
    }

    #[test]
    fn lines_read_back_at_their_addresses() {
        let rows = [
            Row { at: 0, file: 0, line: 3, column: 1 },
            Row { at: 0x10, file: 0, line: 4, column: 5 },
            Row { at: 0, file: 1, line: 9, column: 2 },
        ];
        let functions = [
            DebugFunction { addr: 0x2000, size: 0x20, name: Some("_RNvCs1_4main4main"), lines: 0..2 },
            DebugFunction { addr: 0x1000, size: 0x8, name: None, lines: 2..3 },
        ];
        let sections = sections(&functions, &rows, &["src/main.rs", "src/lib.rs"], 8).unwrap();
        let (seen, subprograms) = read(&sections, 8);
        assert_eq!(seen, [(0x1000, Some(9), 2, 2), (0x2000, Some(3), 1, 1), (0x2010, Some(4), 1, 5)]);
        assert_eq!(subprograms, [(None, 0x1000, 0x8), (Some("_RNvCs1_4main4main".to_owned()), 0x2000, 0x20)]);
    }

    /// Line jumps and address advances past one special opcode's reach, repeated rows at one
    /// address, and 4-byte addresses.
    #[test]
    fn long_advances_and_small_addresses_round_trip() {
        let mut rows = Vec::new();
        let mut want = Vec::new();
        let mut at = 0;
        for n in 0..200u32 {
            let line = if n % 7 == 0 { 1000 + n * 50 } else { 1000 + n };
            rows.push(Row { at, file: n % 3, line, column: n % 4 });
            want.push((0x40_0000 + u64::from(at), Some(u64::from(line)), u64::from(n % 3) + 1, u64::from(n % 4)));
            at += [0, 1, 17, 300, 5][n as usize % 5];
        }
        let functions = [DebugFunction { addr: 0x40_0000, size: u64::from(at) + 4, name: Some("f"), lines: 0..200 }];
        let sections = sections(&functions, &rows, &["a.rs", "b.rs", "c.rs"], 4).unwrap();
        let (seen, _) = read(&sections, 4);
        assert_eq!(seen, want);
    }

    #[test]
    fn a_line_past_its_function_or_a_bad_name_is_an_error() {
        let rows = [Row { at: 4, file: 0, line: 1, column: 1 }];
        let functions = [DebugFunction { addr: 0, size: 4, name: None, lines: 0..1 }];
        assert!(sections(&functions, &rows, &["a.rs"], 8).is_err());
        let fits = [DebugFunction { addr: 0, size: 8, name: None, lines: 0..1 }];
        assert!(sections(&fits, &rows, &["a.rs"], 8).is_ok());
        assert!(sections(&fits, &rows, &["a\0.rs"], 8).is_err());
        assert!(sections(&fits, &rows, &[], 8).is_err());
        let far = [DebugFunction { addr: 1 << 33, size: 8, name: None, lines: 0..1 }];
        assert!(sections(&far, &rows, &["a.rs"], 4).is_err());
    }
}

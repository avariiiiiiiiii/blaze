//! The debug info a link makes from the functions' line tables and the span side tables:
//! one compilation unit with a line program over every function and a subprogram for each,
//! so a debugger breaks by line and a backtrace names frames. Written anew by each link from
//! what is placed, so it is never stale.

use gimli::write::{
    Address, AttributeValue, DwarfUnit, EndianVec, LineProgram, LineString, Sections,
};

/// A placed function's debug view.
pub(crate) struct DebugFunction<'a> {
    pub addr: u64,
    pub size: u64,
    pub name: Option<&'a str>,
    /// By code offset: the file name, line and column.
    pub lines: Vec<(u64, &'a str, u32, u32)>,
}

/// The DWARF sections, by name (`.debug_info`), of the functions; empty sections left out.
pub(crate) fn sections(
    functions: &[DebugFunction<'_>],
    address_size: u8,
) -> Result<Vec<(&'static str, Vec<u8>)>, String> {
    let encoding =
        gimli::Encoding { format: gimli::Format::Dwarf32, version: 4, address_size };
    let mut dwarf = DwarfUnit::new(encoding);
    dwarf.unit.line_program = LineProgram::new(
        encoding,
        gimli::LineEncoding::default(),
        LineString::String(b".".to_vec()),
        None,
        LineString::String(b"blaze".to_vec()),
        None,
    );
    let directory = dwarf.unit.line_program.default_directory();
    let mut files = rustc_hash::FxHashMap::default();
    let mut ordered: Vec<&DebugFunction<'_>> = functions.iter().collect();
    ordered.sort_by_key(|it| it.addr);
    for function in &ordered {
        if function.lines.is_empty() {
            continue;
        }
        let program = &mut dwarf.unit.line_program;
        program.begin_sequence(Some(Address::Constant(function.addr)));
        let mut last = None;
        for &(at, file, line, column) in &function.lines {
            if at >= function.size || last.is_some_and(|it| at < it) {
                return Err(format!(
                    "a line at offset {at:#x} of a function {:#x} bytes long, or out of order",
                    function.size
                ));
            }
            last = Some(at);
            if file.is_empty() || file.contains('\0') {
                return Err(format!("a span in the file {file:?}, which DWARF can't name"));
            }
            let id = *files.entry(file).or_insert_with(|| {
                program.add_file(LineString::String(file.as_bytes().to_vec()), directory, None)
            });
            let row = program.row();
            row.address_offset = at;
            row.file = id;
            row.line = u64::from(line);
            row.column = u64::from(column);
            row.is_statement = true;
            program.generate_row();
        }
        program.end_sequence(function.size);
    }
    let root = dwarf.unit.root();
    let producer = dwarf.strings.add("blaze");
    let entry = dwarf.unit.get_mut(root);
    entry.set(gimli::DW_AT_producer, AttributeValue::StringRef(producer));
    entry.set(gimli::DW_AT_language, AttributeValue::Language(gimli::DW_LANG_Rust));
    entry.set(gimli::DW_AT_name, AttributeValue::String(b"blaze".to_vec()));
    entry.set(gimli::DW_AT_comp_dir, AttributeValue::String(b".".to_vec()));
    if let (Some(first), Some(last)) = (ordered.first(), ordered.last()) {
        entry.set(gimli::DW_AT_low_pc, AttributeValue::Address(Address::Constant(first.addr)));
        let high = last.addr + last.size - first.addr;
        entry.set(gimli::DW_AT_high_pc, AttributeValue::Udata(high));
    }
    for function in &ordered {
        let id = dwarf.unit.add(root, gimli::DW_TAG_subprogram);
        let name = function.name.map(|it| dwarf.strings.add(it));
        let entry = dwarf.unit.get_mut(id);
        if let Some(name) = name {
            entry.set(gimli::DW_AT_linkage_name, AttributeValue::StringRef(name));
        }
        entry.set(gimli::DW_AT_low_pc, AttributeValue::Address(Address::Constant(function.addr)));
        entry.set(gimli::DW_AT_high_pc, AttributeValue::Udata(function.size));
    }
    let mut sections = Sections::new(EndianVec::new(gimli::LittleEndian));
    dwarf.write(&mut sections).map_err(|it| format!("writing the debug info: {it}"))?;
    let mut out = Vec::new();
    sections
        .for_each(|id, section| -> Result<(), ()> {
            if !section.slice().is_empty() {
                out.push((id.name(), section.slice().to_vec()));
            }
            Ok(())
        })
        .expect("collecting sections fails never");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gimli::{EndianSlice, LittleEndian};

    #[test]
    fn lines_read_back_at_their_addresses() {
        let functions = [
            DebugFunction {
                addr: 0x2000,
                size: 0x20,
                name: Some("_RNvCs1_4main4main"),
                lines: vec![(0, "src/main.rs", 3, 1), (0x10, "src/main.rs", 4, 5)],
            },
            DebugFunction { addr: 0x1000, size: 0x8, name: None, lines: vec![(0, "src/lib.rs", 9, 2)] },
        ];
        let sections = sections(&functions, 8).unwrap();
        let get = |name: &str| {
            sections.iter().find(|it| it.0 == name).map_or(&[][..], |it| &it.1[..])
        };
        let dwarf = gimli::Dwarf::load(|id| -> Result<_, ()> {
            Ok(EndianSlice::new(get(id.name()), LittleEndian))
        })
        .unwrap();
        let mut units = dwarf.units();
        let header = units.next().unwrap().unwrap();
        let unit = dwarf.unit(header).unwrap();
        let program = unit.line_program.clone().unwrap();
        let mut rows = program.rows();
        let mut seen = Vec::new();
        while let Some((_, row)) = rows.next_row().unwrap() {
            if !row.end_sequence() {
                seen.push((row.address(), row.line().map(|it| it.get())));
            }
        }
        assert_eq!(seen, [(0x1000, Some(9)), (0x2000, Some(3)), (0x2010, Some(4))]);
    }

    #[test]
    fn a_line_past_its_function_is_an_error() {
        let functions =
            [DebugFunction { addr: 0, size: 4, name: None, lines: vec![(4, "a.rs", 1, 1)] }];
        assert!(sections(&functions, 8).is_err());
    }
}

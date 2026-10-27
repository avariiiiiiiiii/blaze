//! The rows of a linked executable for files laid out again ([`crate::Respace`]): the code of
//! such a file is not compiled again, so neither are its rows, and what a debugger reads of
//! them is the line program of each unit that names the file, written again with the rows
//! where the code is now. The unit is pointed at the program written again, behind the
//! linker's, and back at its own once its file is compiled again.

use cranelift_codegen::gimli::{
    self, ColumnType, DW_AT_comp_dir, DW_AT_stmt_list, DW_FORM_data4, DW_FORM_sec_offset,
    EndianSlice, LittleEndian,
    write::{
        Address, DebugLine, DirectoryId, EndianVec, FileId, LineProgram, LineString,
        LineStringTable, StringTable,
    },
};

use crate::Respace;

type Slice<'a> = EndianSlice<'a, LittleEndian>;

/// The units of an executable that say where their rows are, and the files they name.
#[derive(Default)]
pub struct LineUnits {
    units: Vec<LineUnit>,
}

struct LineUnit {
    /// Where its `DW_AT_stmt_list` is in `.debug_info`, four bytes.
    field: u64,
    /// Where its line program is in `.debug_line`, and how long it is.
    program: u64,
    len: u64,
    address_size: u8,
    /// The files its rows are of, written out whole.
    files: Box<[Box<[u8]>]>,
}

/// A unit of [`LineUnits`]: where it says its rows are, which a splice writes over.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct LineUnitId(u32);

impl LineUnits {
    /// The units of `.debug_info` with rows in `.debug_line`. A unit that can't be read is
    /// left out, and so are its rows: they stay as the linker wrote them.
    pub fn read(
        info: &[u8],
        abbrev: &[u8],
        line: &[u8],
        strings: &[u8],
        line_strings: &[u8],
    ) -> LineUnits {
        let mut dwarf = gimli::Dwarf::<Slice<'_>>::default();
        dwarf.debug_info = gimli::DebugInfo::new(info, LittleEndian);
        dwarf.debug_abbrev = gimli::DebugAbbrev::new(abbrev, LittleEndian);
        dwarf.debug_line = gimli::DebugLine::new(line, LittleEndian);
        dwarf.debug_str = gimli::DebugStr::new(strings, LittleEndian);
        dwarf.debug_line_str = gimli::DebugLineStr::new(line_strings, LittleEndian);
        let mut units = Vec::new();
        let mut headers = dwarf.units();
        while let Ok(Some(header)) = headers.next() {
            if let Some(unit) = Self::unit(&dwarf, header, line) {
                units.push(unit);
            }
        }
        LineUnits { units }
    }

    fn unit(
        dwarf: &gimli::Dwarf<Slice<'_>>,
        header: gimli::UnitHeader<Slice<'_>>,
        line: &[u8],
    ) -> Option<LineUnit> {
        let start = header.offset().0 as u64;
        let abbreviations = header.abbreviations(&dwarf.debug_abbrev).ok()?;
        let mut entries = header.entries_raw(&abbreviations, None).ok()?;
        let abbreviation = entries.read_abbreviation().ok()??;
        let (mut field, mut program, mut directory) = (None, None, None);
        for spec in abbreviation.attributes() {
            let at = entries.next_offset();
            let attribute = entries.read_attribute(*spec).ok()?;
            if spec.name() == DW_AT_stmt_list
                && [DW_FORM_sec_offset, DW_FORM_data4].contains(&spec.form())
                && header.format() == gimli::Format::Dwarf32
            {
                field = Some(start + at.0 as u64);
                program = match attribute.value() {
                    gimli::AttributeValue::DebugLineRef(it) => Some(it.0 as u64),
                    gimli::AttributeValue::SecOffset(it) => Some(it as u64),
                    other => other.udata_value(),
                };
            } else if spec.name() == DW_AT_comp_dir {
                directory = match attribute.value() {
                    gimli::AttributeValue::String(it) => Some(it),
                    gimli::AttributeValue::DebugStrRef(it) => dwarf.debug_str.get_str(it).ok(),
                    gimli::AttributeValue::DebugLineStrRef(it) => {
                        dwarf.debug_line_str.get_str(it).ok()
                    }
                    _ => None,
                };
            }
        }
        let (field, program) = (field?, program?);
        let directory = directory.map_or(&[][..], |it| it.slice());
        let bytes = line.get(program as usize..)?;
        let table = gimli::DebugLine::new(bytes, LittleEndian);
        let address_size = header.address_size();
        let read = table.program(gimli::DebugLineOffset(0), address_size, None, None).ok()?;
        let header = read.header();
        let len = header.unit_length() as u64 + header.format().initial_length_size() as u64;
        let mut files = Vec::new();
        for file in header.file_names() {
            let name = inline(file.path_name())?;
            let within = match file.directory_index() {
                0 => Some(directory),
                index => header.directory(index).and_then(inline),
            };
            files.push(whole(directory, within?, name));
        }
        Some(LineUnit { field, program, len, address_size, files: files.into() })
    }

    pub fn len(&self) -> usize {
        self.units.len()
    }

    pub fn is_empty(&self) -> bool {
        self.units.is_empty()
    }

    /// The files the unit's rows are of, by their whole names.
    pub fn files(&self, unit: LineUnitId) -> &[Box<[u8]>] {
        &self.units[unit.0 as usize].files
    }

    /// The units with rows of any file `respaced` lays out again, by its whole name.
    pub fn naming<'a>(
        &'a self,
        respaced: impl Fn(&[u8]) -> bool + 'a,
    ) -> impl Iterator<Item = LineUnitId> + 'a {
        let units = self.units.iter().enumerate();
        units
            .filter(move |(_, it)| it.files.iter().any(|it| respaced(it)))
            .map(|(k, _)| LineUnitId(k as u32))
    }

    /// Where the unit says its rows are, in `.debug_info`, and where the linker put them.
    pub fn field(&self, unit: LineUnitId) -> (u64, u32) {
        let unit = &self.units[unit.0 as usize];
        (unit.field, unit.program as u32)
    }

    /// Where the unit's line program is in `.debug_line`, and how long it is.
    pub fn program(&self, unit: LineUnitId) -> (u64, u64) {
        let unit = &self.units[unit.0 as usize];
        (unit.program, unit.len)
    }

    /// The unit's line program `bytes` written again, its rows of each file `respace` has
    /// where they are now. `None` when it does not read as it did.
    pub fn moved<'r>(
        &self,
        unit: LineUnitId,
        bytes: &[u8],
        respace: impl Fn(&[u8]) -> Option<&'r Respace>,
    ) -> Option<Vec<u8>> {
        let unit = &self.units[unit.0 as usize];
        let table = gimli::DebugLine::new(bytes, LittleEndian);
        let read = table.program(gimli::DebugLineOffset(0), unit.address_size, None, None).ok()?;
        let header = read.header();
        let encoding = header.encoding();
        let mut program = LineProgram::new(
            encoding,
            header.line_encoding(),
            LineString::String(b".".to_vec()),
            None,
            LineString::String(b"<ra-jit>".to_vec()),
            None,
        );
        // Directory 0 is the unit's own, which a program of DWARF 4 does not write.
        let mut directories = vec![program.default_directory()];
        for directory in header.include_directories() {
            let directory = inline(directory.clone())?.to_vec();
            directories.push(program.add_directory(LineString::String(directory)));
        }
        let mut files: Vec<(FileId, Option<&Respace>)> = Vec::new();
        for (file, whole) in header.file_names().iter().zip(&unit.files) {
            let name = inline(file.path_name())?.to_vec();
            let directory: DirectoryId = *directories.get(file.directory_index() as usize)?;
            files.push((
                program.add_file(LineString::String(name), directory, None),
                respace(whole),
            ));
        }
        let mut rows = read.rows();
        let mut start = None;
        while let Ok(Some((_, row))) = rows.next_row() {
            let at = *start.get_or_insert_with(|| {
                program.begin_sequence(Some(Address::Constant(row.address())));
                row.address()
            });
            let offset = row.address().checked_sub(at)?;
            if row.end_sequence() {
                program.end_sequence(offset);
                start = None;
                continue;
            }
            // DWARF 4 counts files from one.
            let index = row.file_index().checked_sub(u64::from(encoding.version < 5))?;
            let &(file, respace) = files.get(index as usize)?;
            let mut line = row.line().map_or(0, |it| it.get());
            let mut column = match row.column() {
                ColumnType::LeftEdge => 0,
                ColumnType::Column(it) => it.get(),
            };
            if let Some(respace) = respace
                && line != 0
            {
                let (now, at) = respace.place(line as u32, column.max(1) as u32);
                line = now.into();
                column = if column == 0 { 0 } else { at.into() };
            }
            let written = program.row();
            written.address_offset = offset;
            written.op_index = row.op_index();
            written.file = file;
            written.line = line;
            written.column = column;
            written.discriminator = row.discriminator();
            written.is_statement = row.is_stmt();
            written.basic_block = row.basic_block();
            written.prologue_end = row.prologue_end();
            written.epilogue_begin = row.epilogue_begin();
            written.isa = row.isa();
            program.generate_row();
        }
        if start.is_some() {
            return None;
        }
        let mut written = DebugLine(EndianVec::new(LittleEndian));
        let (mut line_strings, mut strings) = (LineStringTable::default(), StringTable::default());
        program.write(&mut written, encoding, &mut line_strings, &mut strings).ok()?;
        Some(written.0.into_vec())
    }
}

/// A name a line program writes in itself: what a splice can write again as it is.
fn inline(value: gimli::AttributeValue<Slice<'_>>) -> Option<&[u8]> {
    match value {
        gimli::AttributeValue::String(it) => Some(it.slice()),
        _ => None,
    }
}

/// A file's name with the directories it is in, as the source files are named.
fn whole(unit: &[u8], directory: &[u8], name: &[u8]) -> Box<[u8]> {
    let mut path = Vec::new();
    if !name.starts_with(b"/") {
        if !directory.starts_with(b"/") && !unit.is_empty() && directory != unit {
            path.extend_from_slice(unit);
            path.push(b'/');
        }
        if !directory.is_empty() {
            path.extend_from_slice(directory);
            path.push(b'/');
        }
    }
    path.extend_from_slice(name);
    path.into()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use cranelift_codegen::gimli::{
        DW_AT_name, Encoding, Format, LineEncoding,
        write::{AttributeValue, DwarfUnit, EndianVec, Sections},
    };

    use super::*;

    /// A unit of a program at `/root` with rows of two files, as [`super::super::Dwarf`]
    /// writes them: `src/a.rs` lines 1 and 3, `src/b.rs` line 1.
    fn sections() -> Sections<EndianVec<LittleEndian>> {
        let encoding = Encoding { format: Format::Dwarf32, version: 4, address_size: 8 };
        let mut dwarf = DwarfUnit::new(encoding);
        let mut program = LineProgram::new(
            encoding,
            LineEncoding::default(),
            LineString::String(b"/root".to_vec()),
            None,
            LineString::String(b"<ra-jit>".to_vec()),
            None,
        );
        let directory = program.default_directory();
        let a = program.add_file(LineString::String(b"src/a.rs".to_vec()), directory, None);
        let b = program.add_file(LineString::String(b"src/b.rs".to_vec()), directory, None);
        program.begin_sequence(Some(Address::Constant(0x1000)));
        for (offset, file, line, column) in [(0, a, 1, 9), (4, a, 3, 1), (8, b, 1, 1)] {
            let row = program.row();
            (row.address_offset, row.file, row.line, row.column) = (offset, file, line, column);
            program.generate_row();
        }
        program.end_sequence(12);
        dwarf.unit.line_program = program;
        let root = dwarf.unit.root();
        let entry = dwarf.unit.get_mut(root);
        entry.set(DW_AT_name, AttributeValue::String(b"<ra-jit>".to_vec()));
        entry.set(DW_AT_comp_dir, AttributeValue::String(b"/root".to_vec()));
        entry.set(DW_AT_stmt_list, AttributeValue::LineProgramRef);
        let mut sections = Sections::new(EndianVec::new(LittleEndian));
        dwarf.write(&mut sections).unwrap();
        sections
    }

    fn rows(bytes: &[u8]) -> Vec<(u64, u64, u64, u64)> {
        let table = gimli::DebugLine::new(bytes, LittleEndian);
        let program = table.program(gimli::DebugLineOffset(0), 8, None, None).unwrap();
        let mut rows = program.rows();
        let mut all = Vec::new();
        while let Some((_, row)) = rows.next_row().unwrap() {
            let column = match row.column() {
                ColumnType::LeftEdge => 0,
                ColumnType::Column(it) => it.get(),
            };
            let line = row.line().map_or(0, |it| it.get());
            all.push((row.address(), row.file_index(), line, column));
        }
        all
    }

    #[test]
    fn the_rows_of_a_file_laid_out_again_are_where_its_code_is_now() {
        let sections = sections();
        let (info, abbrev) = (sections.debug_info.slice(), sections.debug_abbrev.slice());
        let line = sections.debug_line.slice();
        let units = LineUnits::read(info, abbrev, line, sections.debug_str.slice(), &[]);
        let named: Vec<_> = units.naming(|it| it == b"/root/src/a.rs").collect();
        assert_eq!(named.len(), 1);
        let (field, program) = units.field(named[0]);
        assert_eq!(u32::from_le_bytes(info[field as usize..][..4].try_into().unwrap()), program);
        let (at, len) = units.program(named[0]);
        assert_eq!((at, len), (0, line.len() as u64));

        // `fn f(){ g(a,b) }` once rustfmt is through with it.
        let (was, now) = ("fn f(){ g(a,b) }\n\n}\n", "fn f() {\n    g(a, b)\n}\n\n}\n");
        let was_at = [0, 3, 4, 5, 6, 8, 9, 10, 11, 12, 13, 15, 18];
        let now_at = [0, 3, 4, 5, 7, 13, 14, 15, 16, 18, 19, 21, 24];
        let moves = was_at.iter().zip(&now_at).map(|(&a, &b)| (a, b, 1)).collect();
        let respace = Respace::new(Arc::from(was), Arc::from(now), moves);
        let written =
            units.moved(named[0], line, |it| (it == b"/root/src/a.rs").then_some(&respace));
        let written = written.unwrap();
        assert_eq!(
            rows(&written),
            [(0x1000, 1, 2, 5), (0x1004, 1, 5, 1), (0x1008, 2, 1, 1), (0x100c, 2, 1, 1)]
        );
        assert_eq!(rows(line)[2..], rows(&written)[2..]);
    }
}

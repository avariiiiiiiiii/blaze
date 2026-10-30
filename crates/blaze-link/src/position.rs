//! Positions the program observes, filled at link time (docs/v2/plan/lanes/link/notes.md, "Positions").
//!
//! Code never holds a position: `line!()`, `file!()`, `column!()`, a panic's and a
//! `#[track_caller]` function's `Location` are a [`blaze_codegen::Target::Position`]
//! relocation to a `core::panic::Location` record the linker makes from the span side table
//! of the span's anchor. A reformat changes the table, so the record is rewritten in place and
//! no function is compiled again.

use blaze_ids::ContentId;

/// The span side table of one anchor, as [`crate::Change::spans`] carries it.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct SpanTable {
    /// The file names the spans are in, as the program observes them (`file!()`).
    pub files: Vec<String>,
    /// By the span's index: the file (an index into `files`), its 1-based line and column.
    pub spans: Vec<Position>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Position {
    pub file: u32,
    pub line: u32,
    pub column: u32,
}

const MAGIC: [u8; 8] = *b"BZSPANS1";

impl SpanTable {
    /// `MAGIC`, the file count and each file as a length and its UTF-8, the span count and
    /// each span as three little-endian `u32`s.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        out.extend_from_slice(&(self.files.len() as u32).to_le_bytes());
        for file in &self.files {
            out.extend_from_slice(&(file.len() as u32).to_le_bytes());
            out.extend_from_slice(file.as_bytes());
        }
        out.extend_from_slice(&(self.spans.len() as u32).to_le_bytes());
        for span in &self.spans {
            for word in [span.file, span.line, span.column] {
                out.extend_from_slice(&word.to_le_bytes());
            }
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<SpanTable, String> {
        let mut read = Reader { bytes, at: 0 };
        if read.take(8)? != MAGIC {
            return Err("not a span table: its magic is wrong".to_owned());
        }
        let files = read.word()? as usize;
        let mut table = SpanTable::default();
        for _ in 0..files {
            let len = read.word()? as usize;
            let name = std::str::from_utf8(read.take(len)?).map_err(|_| "a file name not UTF-8")?;
            table.files.push(name.to_owned());
        }
        let spans = read.word()? as usize;
        for _ in 0..spans {
            let span = Position { file: read.word()?, line: read.word()?, column: read.word()? };
            if span.file as usize >= table.files.len() {
                return Err(format!("a span in file {} of {}", span.file, table.files.len()));
            }
            table.spans.push(span);
        }
        if read.at != bytes.len() {
            return Err("bytes after a span table".to_owned());
        }
        Ok(table)
    }

    /// The content ID of the table, which the image keeps to tell a changed one.
    pub fn id(&self) -> ContentId {
        ContentId::of(SPAN_TABLE_DOMAIN, &self.encode())
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self.at.checked_add(n).ok_or("a span table cut short")?;
        let part = self.bytes.get(self.at..end).ok_or("a span table cut short")?;
        self.at = end;
        Ok(part)
    }

    fn word(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("four bytes")))
    }
}

pub(crate) const SPAN_TABLE_DOMAIN: &str = "blaze v2 span table 1";
pub(crate) const FILE_NAME_DOMAIN: &str = "blaze v2 link file name 1";

/// `core::panic::Location<'static>` as blaze lays it out (rustc's field order by alignment):
/// the file name's pointer and length (`NonNull<str>`), then `line` and `col` as `u32`s.
/// codegen reads `line!()` at [`LINE`], `column!()` at [`COLUMN`] and `file!()`'s `&str` at 0.
pub(crate) const LOCATION_SIZE: u64 = 24;
pub(crate) const LOCATION_ALIGN: u64 = 8;
pub const LINE: i64 = 16;
pub const COLUMN: i64 = 20;

/// A `Location` record's bytes, its pointer left for a relocation to the file name.
pub(crate) fn location(name_len: usize, position: Position) -> Vec<u8> {
    let mut record = vec![0; LOCATION_SIZE as usize];
    record[8..16].copy_from_slice(&(name_len as u64).to_le_bytes());
    record[16..20].copy_from_slice(&position.line.to_le_bytes());
    record[20..24].copy_from_slice(&position.column.to_le_bytes());
    record
}

/// The bytes of a file name a `Location` points at: NUL-terminated, as
/// `Location::file_as_c_str` reads one byte past the length.
pub(crate) fn file_name(name: &str) -> Vec<u8> {
    let mut bytes = name.as_bytes().to_vec();
    bytes.push(0);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> SpanTable {
        SpanTable {
            files: vec!["src/main.rs".to_owned(), "src/lib/ünï.rs".to_owned()],
            spans: vec![
                Position { file: 0, line: 3, column: 5 },
                Position { file: 1, line: 120, column: 1 },
            ],
        }
    }

    #[test]
    fn a_table_round_trips() {
        let bytes = table().encode();
        assert_eq!(SpanTable::decode(&bytes).unwrap(), table());
    }

    #[test]
    fn a_bad_table_is_refused_not_misread() {
        let bytes = table().encode();
        for len in 0..bytes.len() {
            assert!(SpanTable::decode(&bytes[..len]).is_err(), "cut at {len}");
        }
        let mut wrong_file = table();
        wrong_file.spans[0].file = 2;
        assert!(SpanTable::decode(&wrong_file.encode()).is_err());
        let mut more = bytes.clone();
        more.push(0);
        assert!(SpanTable::decode(&more).is_err());
    }

    #[test]
    fn a_location_has_lines_where_codegen_reads_them() {
        let record = location(11, Position { file: 0, line: 7, column: 9 });
        assert_eq!(u64::from_le_bytes(record[8..16].try_into().unwrap()), 11);
        assert_eq!(u32::from_le_bytes(record[LINE as usize..][..4].try_into().unwrap()), 7);
        assert_eq!(u32::from_le_bytes(record[COLUMN as usize..][..4].try_into().unwrap()), 9);
        assert_eq!(file_name("a.rs"), b"a.rs\0");
    }
}

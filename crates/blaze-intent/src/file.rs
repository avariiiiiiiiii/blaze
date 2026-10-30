//! One file as a request's edits leave it: its text, its tree reparsed in place after each
//! splice, and the splices made, which are the text edits the VFS commits.

use blaze_vfs::Op;
use syntax::{Edition, Parse, SourceFile, SyntaxNode, TextRange, TextSize};

/// `delete` replaced by `insert`, in the text as the splices before it left it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Splice {
    pub delete: TextRange,
    pub insert: String,
}

pub struct Working {
    /// The file's path in the view.
    pub path: Box<str>,
    pub edition: Edition,
    /// The view's bytes, then what the front end appends to a crate root (never edited).
    pub text: String,
    /// Where the view's bytes end in `text`.
    pub end: usize,
    pub parse: Parse<SourceFile>,
    /// The tree before any splice: where touched items are named and keyed.
    pub base: SyntaxNode,
    pub splices: Vec<Splice>,
}

impl Working {
    /// A file whose view bytes are `text[..end]`, parsed as `parse`.
    pub fn new(path: &str, edition: Edition, text: String, end: usize, parse: Parse<SourceFile>) -> Working {
        let base = parse.syntax_node();
        Working { path: path.into(), edition, text, end, parse, base, splices: Vec::new() }
    }

    /// A file parsed here: for one the front end has not parsed, and for tests.
    pub fn parsed(path: &str, edition: Edition, text: &str) -> Working {
        let parse = SourceFile::parse(text, edition);
        Working::new(path, edition, text.to_owned(), text.len(), parse)
    }

    pub fn root(&self) -> SyntaxNode {
        self.parse.syntax_node()
    }

    /// The view's bytes as the edits so far leave them.
    pub fn bytes(&self) -> &str {
        &self.text[..self.end]
    }

    /// Replaces `delete` by `insert` in the text and reparses only what it touched.
    pub fn splice(&mut self, delete: TextRange, insert: String) -> Result<(), String> {
        let start = usize::from(delete.start());
        let stop = usize::from(delete.end());
        if stop > self.end {
            return Err(format!(
                "an edit of {} reaches past the file's own text, into the module the front end \
                 appends to a crate root",
                self.path
            ));
        }
        if !self.text.is_char_boundary(start) || !self.text.is_char_boundary(stop) {
            return Err(format!("an edit of {} splits a character", self.path));
        }
        self.parse = self.parse.reparse(delete, &insert, self.edition);
        self.text.replace_range(start..stop, &insert);
        self.end = self.end - (stop - start) + insert.len();
        self.splices.push(Splice { delete, insert });
        Ok(())
    }

    /// Where `range` of the base text is now; `None` when a splice replaced any of it.
    pub fn to_now(&self, range: TextRange) -> Option<TextRange> {
        let mut range = range;
        for splice in &self.splices {
            let delete = splice.delete;
            let inserted = TextSize::of(splice.insert.as_str());
            if range.end() <= delete.start() {
                continue;
            }
            if range.start() >= delete.end() {
                range = TextRange::at(range.start() - delete.len() + inserted, range.len());
            } else {
                return None;
            }
        }
        Some(range)
    }

    /// Where `at` in the text now was before any splice; `None` inside text a splice inserted.
    pub fn to_base(&self, at: TextSize) -> Option<TextSize> {
        let mut at = at;
        for splice in self.splices.iter().rev() {
            let start = splice.delete.start();
            let inserted = TextSize::of(splice.insert.as_str());
            if at <= start {
                continue;
            }
            if at >= start + inserted {
                at = at - inserted + splice.delete.len();
            } else {
                return None;
            }
        }
        Some(at)
    }

    /// The splices as the VFS's text edits, in order: each applies to what the one before left.
    pub fn ops(&self) -> impl Iterator<Item = Op> + '_ {
        self.splices.iter().map(|splice| Op::Edit {
            at: u64::from(u32::from(splice.delete.start())),
            len: u64::from(u32::from(splice.delete.len())),
            path: self.path.to_string(),
            bytes: splice.insert.as_bytes().to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_map_back_through_every_splice() {
        let mut file = Working::parsed("lib.rs", Edition::CURRENT, "fn a() {}\nfn b() {}\n");
        file.splice(TextRange::new(7.into(), 9.into()), "{ 1 }".to_owned()).unwrap();
        assert_eq!(file.bytes(), "fn a() { 1 }\nfn b() {}\n");
        assert_eq!(file.to_base(13.into()), Some(10.into()));
        assert_eq!(file.to_base(7.into()), Some(7.into()));
        assert_eq!(file.to_base(9.into()), None);
        assert_eq!(file.to_base(12.into()), Some(9.into()));
        let ops: Vec<Op> = file.ops().collect();
        assert_eq!(
            ops,
            [Op::Edit { at: 7, len: 2, path: "lib.rs".to_owned(), bytes: b"{ 1 }".to_vec() }]
        );
        assert_eq!(file.to_now(TextRange::new(10.into(), 12.into())), Some(TextRange::new(13.into(), 15.into())));
        assert_eq!(file.to_now(TextRange::new(3.into(), 4.into())), Some(TextRange::new(3.into(), 4.into())));
        assert_eq!(file.to_now(TextRange::new(8.into(), 9.into())), None);
    }

    #[test]
    fn the_appended_module_is_never_edited() {
        let text = "fn a() {}\nmod __appended {}\n".to_owned();
        let parse = SourceFile::parse(&text, Edition::CURRENT);
        let mut file = Working::new("lib.rs", Edition::CURRENT, text, 10, parse);
        assert!(file.splice(TextRange::new(8.into(), 12.into()), String::new()).is_err());
        file.splice(TextRange::new(0.into(), 0.into()), "// x\n".to_owned()).unwrap();
        assert_eq!(file.bytes(), "// x\nfn a() {}\n");
        assert_eq!(&file.text[file.end..], "mod __appended {}\n");
    }
}

//! The last parse of each of a few files, which [`SourceFile::parse_file`] makes the next
//! parse of the file from: direct-mapped by the file's key, so a lookup is one index and one
//! short lock, and the parses kept are at most [`SLOTS`]. A slot another file took is a whole
//! parse for the file it held, never a wrong one: [`Parse::reparse_to`] diffs against the text
//! of the tree it is given.

use std::sync::{Mutex, PoisonError};

use crate::{Parse, SourceFile};

/// How many parses are kept: the files being edited, and the last few others.
const SLOTS: usize = 128;

type Slot = Mutex<Option<(u32, Parse<SourceFile>)>>;

static KEPT: [Slot; SLOTS] = [const { Mutex::new(None) }; SLOTS];

fn slot(key: u32) -> &'static Slot {
    // Keys are dense file numbers with the edition in the top bits: the low bits spread them.
    &KEPT[key as usize % SLOTS]
}

/// The parse kept of `key`, if its slot holds one of `key`; the slot keeps it.
pub(crate) fn take(key: u32) -> Option<Parse<SourceFile>> {
    let kept = slot(key).lock().unwrap_or_else(PoisonError::into_inner);
    kept.as_ref().filter(|(of, _)| *of == key).map(|(_, parse)| parse.clone())
}

/// Keeps `parse` as the last of `key`, in place of what its slot held.
pub(crate) fn keep(key: u32, parse: &Parse<SourceFile>) {
    let replaced =
        slot(key).lock().unwrap_or_else(PoisonError::into_inner).replace((key, parse.clone()));
    // Dropped after the lock: a tree let go of goes to the drop thread.
    drop(replaced);
}

#[cfg(test)]
mod tests {
    use parser::Edition;

    use crate::{SourceFile, Text};

    /// Each parse of a file made by `parse_file` from the last one is the whole parse of the
    /// text: a run of edits, then a key whose slot another file took, then the kind changed.
    #[test]
    fn a_kept_parse_makes_the_parse_of_the_new_text() {
        let texts = [
            "fn f() { 1 }\nstruct S;\n",
            "fn f() { 1 + 2 }\nstruct S;\n",
            "fn f() { 1 + 2 }\nstruct S { a: u8 }\n",
            "use x;\nfn f() { 1 + 2 }\nstruct S { a: u8 }\n",
            "use x;\nfn g() {}\n",
            "",
            "mod m { fn h() { let s = \"é\"; } }\n",
            "mod m { fn h() { let s = \"è\"; } }\n",
        ];
        let key = 7 + super::SLOTS as u32 * 3;
        for outline in [false, true] {
            for text in texts {
                let made = SourceFile::parse_file(key, Text::from(text), Edition::CURRENT, outline);
                let whole = match outline {
                    true => SourceFile::parse_outline(Text::from(text), Edition::CURRENT),
                    false => SourceFile::parse(text, Edition::CURRENT),
                };
                assert_eq!(made.is_outline(), outline);
                assert_eq!(
                    format!("{:#?}", made.tree().syntax()),
                    format!("{:#?}", whole.tree().syntax()),
                    "{text:?}"
                );
                assert_eq!(made.errors_with_bodies().len(), whole.errors_with_bodies().len());
                assert_eq!(made.tree().syntax().text().to_string(), text);
            }
            // Another file in the same slot: the next parse of `key` is a whole one.
            let other = key + super::SLOTS as u32;
            SourceFile::parse_file(other, "fn other() {}", Edition::CURRENT, outline);
            assert!(super::take(key).is_none());
        }
    }

    /// Looking a parse up and keeping one allocate nothing: a slot holds the parse's handles.
    #[test]
    fn taking_and_keeping_allocate_nothing() {
        let key = 11 + super::SLOTS as u32 * 5;
        let parse = SourceFile::parse("fn f() { 1 }\n", Edition::CURRENT);
        super::keep(key, &parse);
        let taken =
            blaze_alloc_count::assert_at_most(0, "a kept parse looked up", || super::take(key));
        assert!(taken.is_some());
        blaze_alloc_count::assert_at_most(0, "a parse kept in place of itself", || {
            super::keep(key, &parse)
        });
    }

    /// The parse of a file whose text did not change allocates nothing, however often: it is
    /// the kept parse, and every count dropped on the way is not its tree's last, so nothing
    /// goes to the drop thread (whose channel allocates a block every 31 sends).
    #[test]
    fn an_unchanged_file_parses_again_allocating_nothing() {
        for outline in [false, true] {
            let key = 13 + 4 * outline as u32 + super::SLOTS as u32 * 9;
            let text = Text::from("fn f() { 1 + 2 }\nstruct S { a: u8 }\n");
            let first = SourceFile::parse_file(key, text.clone(), Edition::CURRENT, outline);
            let again = blaze_alloc_count::assert_at_most(0, "an unchanged file parsed again", || {
                SourceFile::parse_file(key, text.clone(), Edition::CURRENT, outline)
            });
            assert!(std::sync::Arc::ptr_eq(
                again.tree.as_ref().unwrap().flat(),
                first.tree.as_ref().unwrap().flat()
            ));
            // Equal bytes at another address: compared, and the same parse.
            let copy = Text::from(&*text);
            let same = SourceFile::parse_file(key, copy.clone(), Edition::CURRENT, outline);
            assert!(std::sync::Arc::ptr_eq(
                same.tree.as_ref().unwrap().flat(),
                first.tree.as_ref().unwrap().flat()
            ));
            blaze_alloc_count::assert_at_most(0, "an unchanged file parsed 100 times", || {
                for _ in 0..100 {
                    drop(SourceFile::parse_file(key, text.clone(), Edition::CURRENT, outline));
                }
            });
        }
    }
}

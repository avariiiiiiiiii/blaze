use std::{
    ops::Range,
    path::{Path, PathBuf},
};

use base_db::{EditionedFileId, SourceDatabase};
use test_fixture::WithFixture;

use crate::{
    item_tree::{ItemTree, lower_whole, tests::assert_assembled_is_whole},
    test_db::TestDB,
};

/// Real files with nested and outline modules, `use` trees, macros, `extern` blocks, `impl`s
/// and attributes: the sources of the item tree, of name resolution and of the syntax tree.
fn corpus() -> Vec<(String, String)> {
    let root = test_utils::project_root().into_std_path_buf();
    let mut files = Vec::new();
    for dir in ["crates/hir-def/src/item_tree", "crates/hir-def/src/nameres", "crates/syntax/src/ast"]
    {
        collect_rs(&root.join(dir), &mut files);
    }
    files.sort();
    assert!(files.len() > 20, "the sources of the item tree, name resolution and the syntax tree");
    (files.into_iter())
        .map(|path| {
            let text = std::fs::read_to_string(&path).expect("a source file");
            (path.display().to_string(), text)
        })
        .collect()
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("a directory of the tree") {
        let path = entry.expect("a directory entry").path();
        if path.is_dir() {
            collect_rs(&path, out);
        } else if path.extension().is_some_and(|it| it == "rs") {
            out.push(path);
        }
    }
}

/// A database of one file, the crate's root, holding `text`.
fn db_of(text: &str) -> (TestDB, EditionedFileId) {
    let (mut db, file) = TestDB::with_single_file("");
    db.set_file_text(file.file_id(&db), text);
    (db, file)
}

fn assembled(db: &TestDB, file: EditionedFileId) -> ItemTree {
    super::file_item_tree(db, file, db.test_crate())
}

#[test]
fn real_files_assemble_to_their_whole_trees() {
    let (mut db, file) = TestDB::with_single_file("");
    for (path, text) in &corpus() {
        db.set_file_text(file.file_id(&db), text);
        assert_assembled_is_whole(&db, file, path);
    }
}

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n.max(1) as u64) as usize
    }
}

const INSERTS: &[&str] = &[
    "", " ", "\n", "{", "}", "(", ")", ";", ",", "x", "::", "#[a]", "#![a]", "#[cfg(false)] ",
    "#![cfg(false)]\n", "/// d\n", "//! d\n", "pub ", "pub(crate) ", "pub(in crate::m) ",
    "fn f() {}", "fn g() { #![inline] 1 }", "mod m {}", "mod m;", "mod n { pub fn i() {} }",
    "struct S(pub u8, u16);", "struct R { a: u8 }", "enum E { A }", "union U { a: u8 }",
    "use a::{b, c as d, e::*};", "use self::x as _;", "extern crate core as c;",
    "impl S { fn g() {} }", "impl T for S {}", "trait T { fn t(); }", "type A = u8;",
    "static X: u8 = 0;", "const _: () = ();", "m!();", "m! { x }", "macro_rules! m { () => {} }",
    "macro m() {}", "extern \"C\" { fn e(); static E: u8; m!(); }", "let x = 1;",
];

/// An edit of `text`: a range of it and what replaces it, from [`INSERTS`] or `text` itself.
fn random_edit(rng: &mut Rng, text: &str) -> (Range<usize>, String) {
    let boundary = |mut at: usize| {
        while !text.is_char_boundary(at) {
            at -= 1;
        }
        at
    };
    let start = boundary(rng.below(text.len() + 1));
    let end = boundary((start + rng.below(12)).min(text.len())).max(start);
    let insert = match rng.below(3) {
        0 => {
            let from = boundary(rng.below(text.len() + 1));
            let to = boundary((from + rng.below(48)).min(text.len())).max(from);
            text[from..to].to_owned()
        }
        _ => INSERTS[rng.below(INSERTS.len())].to_owned(),
    };
    (start..end, insert)
}

/// After each of a run of edits, the tree a database assembles from the items it kept and the
/// items it lowered again is the tree a fresh database lowers the text to whole.
#[test]
fn random_edits_assemble_to_a_fresh_whole_tree() {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let (mut db, file) = TestDB::with_single_file("");
    for (path, text) in corpus().iter().step_by(2) {
        let mut text = text.clone();
        db.set_file_text(file.file_id(&db), &text);
        assembled(&db, file);
        for _ in 0..6 {
            let (range, insert) = random_edit(&mut rng, &text);
            text.replace_range(range.clone(), &insert);
            db.set_file_text(file.file_id(&db), &text);
            let (fresh, fresh_file) = db_of(&text);
            let edited = assembled(&db, file);
            let whole = lower_whole(&fresh, fresh_file.into(), fresh.test_crate());
            assert_eq!(edited, whole, "{path} after replacing {range:?} with {insert:?}");
            assert_eq!(
                blaze_format::to_bytes(&edited),
                blaze_format::to_bytes(&whole),
                "{path} after replacing {range:?} with {insert:?}"
            );
        }
    }
}

const ITEMS: &str = r#"
use std::fmt;
pub struct S(pub u8);
pub fn f(s: S) -> u8 { s.0 }
impl S {
    fn g(&self) -> u8 { 1 }
    fn h(&self) {}
}
pub trait T { fn t(&self) { let x = 1; } }
const C: u8 = { 1 + 1 };
mod m { pub fn inner() {} }
"#;

/// How many items lowering the file's tree lowers once it holds `text`, its tree lowered
/// before.
fn items_lowered_by(db: &mut TestDB, file: EditionedFileId, text: &str) -> usize {
    let krate = db.test_crate();
    crate::file_item_tree(&*db, file.into(), krate);
    let file_id = file.file_id(&*db);
    db.set_file_text(file_id, text);
    let db = &*db;
    let events = db.log_executed(|| {
        crate::file_item_tree(db, file.into(), krate);
    });
    assert_assembled_is_whole(db, file, text);
    events.iter().filter(|it| it.contains("item_fragment")).count()
}

#[test]
fn an_edit_inside_bodies_lowers_no_item() {
    let (mut db, file) = db_of(ITEMS);
    let mut text = ITEMS.to_owned();
    for (from, to) in [
        ("{ s.0 }", "{ let y = s.0 + 1; y * 2 }"),
        ("{ 1 }", "{\n        let z = [1, 2];\n        z.len() as u8\n    }"),
        ("fn h(&self) {}", "fn h(&self) { loop {} }"),
        // The attributes of an `impl`'s `fn` are the `fn`'s, not the `impl`'s.
        ("{ loop {} }", "{ #![inline] loop {} }"),
        ("let x = 1;", "let x = 2; x"),
        ("{ 1 + 1 }", "{ 2 }"),
    ] {
        text = text.replace(from, to);
        assert_eq!(items_lowered_by(&mut db, file, &text), 0, "after {to}");
    }
}

#[test]
fn an_edit_of_one_item_lowers_it_alone() {
    for (from, to) in [
        ("pub fn f(", "pub fn renamed("),
        ("pub struct S(pub u8)", "pub struct S(u8)"),
        ("impl S {", "impl fmt::Debug for S {"),
        ("const C", "pub const C"),
        ("pub trait T", "struct New;\npub trait T"),
        ("pub fn inner", "fn inner"),
        ("pub fn f(s: S) -> u8 { s.0 }", "pub fn f(s: S) -> u8 {\n    #![allow(x)]\n    s.0\n}"),
    ] {
        let (mut db, file) = db_of(ITEMS);
        let edited = ITEMS.replace(from, to);
        assert_eq!(items_lowered_by(&mut db, file, &edited), 1, "after {to}");
    }
}

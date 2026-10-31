"""tools/v2/salsa_to_query.py's rewrites and what it leaves, on snippets.

  python3 -m unittest tools/v2/test_salsa_to_query.py
"""

import sys
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parents[1] / "python" / "typed-input" / "src"))
sys.path.insert(0, str(HERE))

import salsa_to_query as s  # noqa: E402


def hand(text: str) -> list[tuple[int, str]]:
    return [(it.line, it.why) for it in s.migrate(text).left if it.kind == "hand"]


class Rewrites(unittest.TestCase):
    def test_attributes_lose_revisions(self) -> None:
        text = "#[salsa::interned(debug, unsafe(no_lifetime), revisions = usize::MAX)]\nstruct A {}\n"
        self.assertEqual(s.migrate(text).text, "#[blaze_query::interned(debug, unsafe(no_lifetime))]\nstruct A {}\n")
        only = "#[salsa::interned(revisions = usize::MAX)]\nstruct A {}\n"
        self.assertEqual(s.migrate(only).text, "#[blaze_query::interned]\nstruct A {}\n")
        tracked = "#[salsa::tracked(returns(ref), cycle_result = f, lru = 0, mem_tag = Mir)]\nfn q() {}\n"
        self.assertEqual(s.migrate(tracked).text, tracked.replace("salsa", "blaze_query"))

    def test_use_trees_split_by_what_maps(self) -> None:
        text = "use salsa::{\n    Database as _, Id,\n    plumbing::{AsId, FromId, ZalsaDatabase as _},\n};\n"
        result = s.migrate(text)
        self.assertEqual(
            result.text,
            "use blaze_query::db::{AsId, Database as _, FromId, Id};\nuse salsa::plumbing::ZalsaDatabase as _;\n",
        )
        self.assertEqual([line for line, _ in hand(text)], [2])

    def test_mixed_roots_and_dropped_value(self) -> None:
        text = "use salsa::{SalsaValue, Supertype, plumbing::AsId};\n#[derive(Clone, SalsaValue)]\nstruct A;\nunsafe impl SalsaValue for B {}\n"
        self.assertEqual(
            s.migrate(text).text,
            "use blaze_query::{Supertype, db::AsId};\n#[derive(Clone, blaze_query::Value)]\nstruct A;\n"
            "unsafe impl blaze_query::db::Value for B {}\n",
        )
        alone = "    use salsa::SalsaValue;\nfn f() {}\n"
        self.assertEqual(s.migrate(alone).text, "fn f() {}\n")

    def test_paths_renames_and_suffixes(self) -> None:
        text = "fn f(r: salsa::Revision, d: salsa::Durability) -> u32 { salsa::Id::MAX_U32 + base_db::salsa::Durability::HIGH }\n"
        self.assertEqual(
            s.migrate(text).text,
            "fn f(r: blaze_query::db::View, d: blaze_query::db::Durability) -> u32 "
            "{ blaze_query::db::Id::MAX_U32 + blaze_query::db::Durability::HIGH }\n",
        )

    def test_bits_on_ids_only(self) -> None:
        text = (
            "use salsa::plumbing::Id;\n"
            "fn f(i: u32) { let a = Id::from_bits(u64::from(i) + 1); let b = x.as_id().as_bits();"
            " let c = f32::from_bits(1); let d = Handle::from_bits(2); let e = salsa::Id::from_bits(7); }\n"
        )
        new = s.migrate(text).text
        self.assertIn("Id::from_u32(i + 1)", new)
        self.assertIn("x.as_id().as_u32()", new)
        self.assertIn("f32::from_bits(1)", new)
        self.assertIn("Handle::from_bits(2)", new)
        self.assertIn("blaze_query::db::Id::from_u32(7)", new)

    def test_generation_bits_are_left(self) -> None:
        text = "fn f() { salsa::Id::from_bits(u64::from(i) | (3 << 32)) }\n"
        result = s.migrate(text)
        self.assertIn("from_bits", result.text)
        self.assertTrue(any("generation" in why for _, why in hand(text)))

    def test_comments_and_strings_are_not_rewritten(self) -> None:
        text = '// salsa::Id here\nconst A: &str = "salsa::tracked";\nfn f() {}\n'
        result = s.migrate(text)
        self.assertEqual(result.text, text)
        kinds = sorted((it.line, it.kind) for it in result.left)
        self.assertEqual(kinds, [(1, "comment"), (2, "string")])


class Left(unittest.TestCase):
    def test_database_methods_and_plumbing(self) -> None:
        text = (
            "use salsa::Database as _;\n"
            "fn f(db: &Db) {\n"
            "    db.unwind_if_revision_cancelled();\n"
            "    salsa::Database::ingredient_debug_name(db, i);\n"
            "    impl salsa::plumbing::interned::NamesIds for A {}\n"
            "    match k { ReadKind::Query => {} ReadKind::TrackedField(_) => {} }\n"
            "    salsa::plumbing::ZalsaDatabase::zalsa(db);\n"
            "}\n"
        )
        lines = sorted(line for line, _ in hand(text))
        self.assertEqual(lines, [3, 4, 6, 7])
        self.assertIn("impl blaze_query::db::NamesIds for A {}", s.migrate(text).text)

    def test_unsized_generic_database_is_no_case(self) -> None:
        text = "pub fn text<'db, Db: ?Sized + salsa::Database>(self, db: &'db Db) {}\n"
        self.assertEqual(hand(text), [])

    def test_lines_are_the_new_files(self) -> None:
        text = "use salsa::SalsaValue;\nuse salsa::Durability;\nfn f() { salsa::DatabaseImpl::new(); }\n"
        result = s.migrate(text)
        self.assertEqual(result.text.split("\n")[1], "fn f() { salsa::DatabaseImpl::new(); }")
        self.assertEqual([line for line, _ in hand(text)], [2])


class StorageCalls(unittest.TestCase):
    def test_a_call_on_the_storage_is_no_case(self) -> None:
        text = ("use blaze_query::db::Durability;\n"
                "fn f(db: &mut Db) { db.storage_mut().synthetic_write(Durability::LOW); db.storage().stop_readers(); }\n")
        self.assertEqual(s.migrate(text).left, [])
        self.assertTrue(hand("fn f(db: &mut Db) { salsa::Database::synthetic_write(db, d); db.synthetic_write(d); }\n"))


class Feature(unittest.TestCase):
    def test_spans_feature_is_query(self) -> None:
        out = s.migrate('#[cfg(not(feature = "salsa"))]\nfn f() {}\n')
        self.assertEqual(out.text, '#[cfg(not(feature = "query"))]\nfn f() {}\n')
        self.assertEqual(out.left, [])


class Replay(unittest.TestCase):
    def check(self, before: str, after: str) -> None:
        edits = s.hand_edits(before, after)
        self.assertEqual(s.replay("x.rs", before, edits), after)
        for edit in edits:
            self.assertTrue(edit["old"])

    def test_round_trips(self) -> None:
        base = "".join(f"line {i}\n" for i in range(20)) + "same\nsame\nsame\n"
        self.check(base, base.replace("line 3\n", "line three\n"))
        self.check(base, base.replace("line 5\n", "line 5\nnew\n").replace("line 17\n", ""))
        self.check(base, base.replace("same\nsame\nsame\n", "same\nother\nsame\n"))
        self.check(base, "head\n" + base + "tail\n")
        self.check("a\n", "")

    def test_a_stale_edit_stops(self) -> None:
        edits = s.hand_edits("a\nb\nc\n", "a\nB\nc\n")
        with self.assertRaises(SystemExit):
            s.replay("x.rs", "a\nchanged\nc\n", edits)


class Manifest(unittest.TestCase):
    def test_dependency(self) -> None:
        text = "[dependencies]\nfoo.workspace = true\nsalsa.workspace = true\n\n[dev-dependencies]\n"
        new, left = s.migrate_manifest(text)
        self.assertEqual(new, "[dependencies]\nfoo.workspace = true\nblaze-query.workspace = true\n\n[dev-dependencies]\n")
        self.assertEqual(left, [])

    def test_already_has_blaze_query(self) -> None:
        text = "[dependencies]\nblaze-query.workspace = true\nsalsa.workspace = true\n"
        self.assertEqual(s.migrate_manifest(text)[0], "[dependencies]\nblaze-query.workspace = true\n")

    def test_other_forms_are_left(self) -> None:
        text = '[dependencies]\nsalsa = { workspace = true, optional = true }\n[features]\ndefault = ["salsa"]\n'
        new, left = s.migrate_manifest(text)
        self.assertEqual(new, text)
        self.assertEqual([it.line for it in left], [2, 4])


if __name__ == "__main__":
    unittest.main()

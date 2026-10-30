use expect_test::{Expect, expect};
use span::Edition;
use test_fixture::WithFixture;

use crate::test_db::TestDB;

fn check(#[rust_analyzer::rust_fixture] ra_fixture: &str, expect: Expect) {
    let (db, file_id) = TestDB::with_single_file(ra_fixture);
    let item_tree = crate::file_item_tree(&db, file_id.into(), db.test_crate());
    let pretty = item_tree.pretty_print(&db, Edition::CURRENT);
    expect.assert_eq(&pretty);
    crate::item_tree::persist::assert_round_trip(&item_tree);
}

#[test]
fn imports() {
    check(
        r#"
//! file comment
#![no_std]
//! another file comment

extern crate self as renamed;
pub(super) extern crate bli;

pub use crate::path::{nested, items as renamed, Trait as _};
use globs::*;

/// docs on import
use crate::{A, B};

use a::{c, d::{e}};
        "#,
        expect![[r#"
            #![no_std]

            // AstId: ExternCrate[070B, 0]
            pub(self) extern crate self as renamed;

            // AstId: ExternCrate[1EA5, 0]
            pub(in super) extern crate bli;

            // AstId: Use[0000, 0]
            pub use crate::path::{nested, items as renamed, Trait as _};

            // AstId: Use[0000, 1]
            pub(self) use globs::*;

            // AstId: Use[0000, 2]
            pub(self) use crate::{A, B};

            // AstId: Use[0000, 3]
            pub(self) use a::{c, d::{e}};
        "#]],
    );
}

#[test]
fn extern_blocks() {
    check(
        r#"
#[on_extern_block]
extern "C" {
    #[on_extern_type]
    type ExType;

    #[on_extern_static]
    static EX_STATIC: u8;

    #[on_extern_fn]
    fn ex_fn();
}
        "#,
        expect![[r#"
            #[on_extern_block]
            // AstId: ExternBlock[0000, 0]
            extern {
                #[on_extern_type]
                // AstId: TypeAlias[A09C, 0]
                pub(self) type ExType;

                #[on_extern_static]
                // AstId: Static[D85E, 0]
                pub(self) static EX_STATIC = _;

                #[on_extern_fn]
                // AstId: Fn[B240, 0]
                pub(self) fn ex_fn;
            }
        "#]],
    );
}

#[test]
fn adts() {
    check(
        r#"
struct Unit;

#[derive(Debug)]
struct Struct {
    /// fld docs
    fld: (),
}

struct Tuple(#[attr] u8);

union Ize {
    a: (),
    b: (),
}

enum E {
    /// comment on Unit
    Unit,
    /// comment on Tuple
    Tuple(u8),
    Struct {
        /// comment on a: u8
        a: u8,
    }
}
        "#,
        expect![[r#"
            // AstId: Struct[ED35, 0]
            pub(self) struct Unit { ... }

            #[derive(Debug)]
            // AstId: Struct[A47C, 0]
            pub(self) struct Struct { ... }

            // AstId: Struct[C8C9, 0]
            pub(self) struct Tuple { ... }

            // AstId: Union[2797, 0]
            pub(self) union Ize { ... }

            // AstId: Enum[7D23, 0]
            pub(self) enum E { ... }
        "#]],
    );
}

#[test]
fn misc() {
    check(
        r#"
pub static mut ST: () = ();

const _: Anon = ();

#[attr]
fn f(#[attr] arg: u8, _: ()) {
    #![inner_attr_in_fn]
}

trait Tr: SuperTrait + 'lifetime {
    type Assoc: AssocBound = Default;
    fn method(&self);
}
        "#,
        expect![[r#"
            // AstId: Static[F7C1, 0]
            pub static ST = _;

            // AstId: Const[84BB, 0]
            pub(self) const _ = _;

            #[attr]
            #[inner_attr_in_fn]
            // AstId: Fn[BE8F, 0]
            pub(self) fn f;

            // AstId: Trait[9320, 0]
            pub(self) trait Tr { ... }
        "#]],
    );
}

#[test]
fn modules() {
    check(
        r#"
/// outer
mod inline {
    //! inner

    use super::*;

    fn fn_in_module() {}
}

mod outline;
        "#,
        expect![[r##"
            // AstId: Module[03AE, 0]
            pub(self) mod inline {
                // AstId: Use[0000, 0]
                pub(self) use super::*;

                // AstId: Fn[2A78, 0]
                pub(self) fn fn_in_module;
            }

            // AstId: Module[C08B, 0]
            pub(self) mod outline;
        "##]],
    );
}

#[test]
fn macros() {
    check(
        r#"
macro_rules! m {
    () => {};
}

pub macro m2() {}

m!();
        "#,
        expect![[r#"
            // AstId: MacroRules[7E68, 0]
            macro_rules! m { ... }

            // AstId: MacroDef[1C1E, 0]
            pub macro m2 { ... }

            // AstId: MacroCall[7E68, 0], SyntaxContextId: ROOT2024, ExpandTo: Items
            m!(...);
        "#]],
    );
}

#[test]
fn pub_self() {
    check(
        r#"
pub(self) struct S;
        "#,
        expect![[r#"
            // AstId: Struct[5024, 0]
            pub(self) struct S { ... }
        "#]],
    )
}

#[test]
fn crate_attrs_should_preserve_order() {
    check(
        r#"
//- /main.rs crate:foo crate-attr:no_std crate-attr:features(f16) crate-attr:crate_type="bin"
        "#,
        expect![[r##"
            #![no_std]
            #![features(f16)]
            #![crate_type = "bin"]
        "##]],
    );
}

#[test]
fn crate_attrs_with_disabled_cfg_injected() {
    check(
        r#"
//- /main.rs crate:foo crate-attr:no_std crate-attr:cfg(false) crate-attr:features(f16,f128) crate-attr:crate_type="bin"
        "#,
        expect![[r#"
            #![no_std]
            #![cfg(false)]
        "#]],
    );
}

#[test]
fn crate_attrs_with_disabled_cfg_in_source() {
    check(
        r#"
//- /lib.rs crate:foo crate-attr:no_std
#![cfg(false)]
#![no_core]
        "#,
        expect![[r#"
            #![no_std]
            #![cfg(false)]
        "#]],
    );
}

#[test]
fn use_trees_expand_in_order_of_their_syntax() {
    use la_arena::{Idx, RawIdx};

    use crate::item_tree::ModItemKind;

    let (db, file_id) = TestDB::with_single_file("use a::{b, c::{d, *}, self, e as f};");
    let tree = crate::file_item_tree(&db, file_id.into(), db.test_crate());
    let [item] = tree.top_level_items() else { panic!("one item") };
    let ModItemKind::Use(_, use_item) = tree.index(*item) else { panic!("a `use`") };
    let mut imports = Vec::new();
    use_item.expand(tree, |idx, path, kind, alias| {
        let alias = alias.map(|alias| alias.display(Edition::CURRENT).to_string());
        let path = path.display(&db, Edition::CURRENT);
        imports.push((idx, format!("{path} {kind:?} {alias:?}")));
    });
    let tree_at = |index| Idx::from_raw(RawIdx::from_u32(index));
    assert_eq!(
        imports,
        [
            (tree_at(1), "a::b Plain None".to_owned()),
            (tree_at(3), "a::c::d Plain None".to_owned()),
            (tree_at(4), "a::c Glob None".to_owned()),
            (tree_at(5), "a TypeOnly None".to_owned()),
            (tree_at(6), r#"a::e Plain Some("f")"#.to_owned()),
        ]
    );
}

#[test]
fn struct_constructors_have_the_least_visibility_of_their_fields() {
    use crate::item_tree::{ModItemKind, RawVisibility, StructValueNsCtor};

    let (db, file_id) = TestDB::with_single_file(
        r#"
pub struct R { a: u8 }
pub(crate) struct U;
pub struct T(pub u8, pub(crate) u16);
pub struct M(pub(in crate::a) u8, pub(in crate::b) u8);
"#,
    );
    let tree = crate::file_item_tree(&db, file_id.into(), db.test_crate());
    let ctors: Vec<Option<Vec<RawVisibility>>> = (tree.top_level_items().iter())
        .map(|&item| {
            let ModItemKind::Struct(_, strukt) = tree.index(item) else { panic!("a struct") };
            match tree.value_ns_ctor(strukt) {
                StructValueNsCtor::NoValueNsCtor => None,
                StructValueNsCtor::ValueNsCtorWithVis(vis) => Some(vec![tree[*vis].clone()]),
                StructValueNsCtor::ValueNsCtorWithMinVis(all) => {
                    Some(all.iter().map(|vis| tree[*vis].clone()).collect())
                }
            }
        })
        .collect();
    let pub_crate = Some(vec![RawVisibility::PubCrate]);
    assert_eq!(ctors[..3], [None, pub_crate.clone(), pub_crate]);
    let least = ctors[3].as_ref().expect("a tuple struct's constructor");
    assert_eq!(least.len(), 2);
    assert!(least.iter().all(|vis| matches!(vis, RawVisibility::Module(..))));
}

/// The tree of `file`, owned (decoded from its bytes) so it outlives an edit, and its bytes.
fn owned_tree(db: &TestDB, file: HirFileId) -> (ItemTree, Vec<u8>) {
    let tree = crate::file_item_tree(db, file, db.test_crate());
    let bytes = blaze_format::to_bytes(tree);
    let buffer = blaze_format::Aligned::copy_from(&bytes);
    let owned = ItemTree::read(&buffer).and_then(|read| read.decode()).expect("a tree written");
    assert_eq!(owned, *tree);
    (owned, bytes)
}

const EDITED: &str = r#"
use std::fmt;
pub struct S(pub u8);
mod m {
    pub fn inner() { let x = 1; }
}
pub fn f(s: S) -> u8 { s.0 }
impl S { fn g(&self) {} }
"#;

#[test]
fn an_edit_inside_a_body_changes_no_byte_of_the_tree() {
    let (mut db, file_id) = TestDB::with_single_file(EDITED);
    let file: HirFileId = file_id.into();
    let (before, bytes) = owned_tree(&db, file);
    for edited in [
        EDITED.replace("{ s.0 }", "{ let y = s.0 + 1; y * 2 }"),
        EDITED.replace("let x = 1;", "let x = [1, 2, 3].len(); x"),
        EDITED.replace("fn g(&self) {}", "fn g(&self) { loop {} }"),
    ] {
        db.set_file_text(file_id.file_id(&db), &edited);
        let (after, after_bytes) = owned_tree(&db, file);
        assert_eq!(after, before, "{edited}");
        assert_eq!(after_bytes, bytes, "{edited}");
    }
}

#[test]
fn an_edit_of_one_item_changes_only_its_rows() {
    let (mut db, file_id) = TestDB::with_single_file(EDITED);
    let file: HirFileId = file_id.into();
    let (before, _) = owned_tree(&db, file);
    db.set_file_text(file_id.file_id(&db), &EDITED.replace("pub fn f(", "pub fn renamed("));
    let (after, _) = owned_tree(&db, file);

    let f = (before.top_level.iter().copied())
        .find(|&item| {
            matches!(before.index(item), ModItemKind::Function(_, it) if before[it.name].as_str() == "f")
        })
        .expect("`f` at the top level");
    let ModItemKind::Function(_, it) = after.index(f) else { panic!("`f`'s row is a fn") };
    assert_eq!(after[it.name].as_str(), "renamed");

    assert_eq!(after.top_level, before.top_level);
    assert_eq!(after.kinds, before.kinds);
    assert_eq!(after.data, before.data);
    assert_eq!(after.functions, before.functions);
    assert_eq!(after.children, before.children);
    assert_eq!((&after.structs, &after.mods, &after.uses), (&before.structs, &before.mods, &before.uses));
    for (at, (old, new)) in before.ast_ids.iter().zip(&after.ast_ids).enumerate() {
        if at != f.index as usize {
            assert_eq!(old, new, "the ast id of item {at}, not the one edited");
        }
    }
    let changed: Vec<usize> = (0..before.names.len())
        .filter(|&at| before.names[at] != after.names[at])
        .collect();
    assert_eq!(changed, [it.name.raw as usize], "only the edited item's name");
}

#[test]
fn a_reused_builder_lowers_as_a_fresh_one() {
    let small = "pub fn a() {}\nmod m { struct S; }\nuse x::{y, z};\n";
    let big = r#"
#![no_std]
extern crate core as c;
pub mod outer {
    pub mod inner { pub struct T(pub(in crate::outer) u8, pub(in crate::x) u8); }
    use a::{b::{c, d}, e::*};
}
extern "C" { fn ext(); static S: u8; }
#[cfg(false)] fn gone() {}
macro_rules! m { () => {} }
m!();
"#;
    let lower = |text: &str| {
        let (db, file_id) = TestDB::with_single_file(text);
        (owned_tree(&db, file_id.into()).1, db.test_crate())
    };
    let (first, _) = lower(small);
    lower(big);
    let (again, _) = lower(small);
    assert_eq!(again, first, "a tree lowered after a bigger one on the same thread");
}

#[test]
fn constructor_visibilities_keep_the_order_of_their_fields() {
    use crate::item_tree::{RawVisibility, StructValueNsCtor};

    let (db, file_id) = TestDB::with_single_file(
        "pub struct M(pub(in crate::b) u8, pub(in crate::a) u8, pub u8, pub(in crate::b) u16);",
    );
    let tree = crate::file_item_tree(&db, file_id.into(), db.test_crate());
    let [item] = tree.top_level_items() else { panic!("one item") };
    let ModItemKind::Struct(_, strukt) = tree.index(*item) else { panic!("a struct") };
    let StructValueNsCtor::ValueNsCtorWithMinVis(all) = tree.value_ns_ctor(strukt) else {
        panic!("several visibilities")
    };
    let paths: Vec<String> = (all.iter())
        .map(|vis| match &tree[*vis] {
            RawVisibility::Module(path, _) => path.display(&db, Edition::CURRENT).to_string(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(paths, ["crate::b", "crate::a"]);
}

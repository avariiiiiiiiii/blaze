//! What name resolution records of how names resolved: the imports each path used and how
//! (`used_imports`), the bindings a scope keeps beside the one a name has, and how macro calls
//! and derive helpers resolved.

use base_db::{Crate, all_crates};
use expect_test::{Expect, expect};
use hir_expand::name::Name;
use intern::sym;
use syntax::{AstNode, ast};

use super::*;
use crate::{
    Lookup, ModuleDefId,
    item_scope::{GlobId, ImportId, ImportOrExternCrate, ItemInNs},
    nameres::uses::{MacroVia, NameRecords, used_imports},
    per_ns::Namespace,
    src::use_tree_to_ast,
    test_db::TestDBExt,
};

fn crate_named(db: &TestDB, name: &str) -> Crate {
    *all_crates(db)
        .iter()
        .find(|it| it.extra_data(db).display_name.as_ref().is_some_and(|it| it.to_string() == name))
        .unwrap()
}

/// The source of an import or `extern crate`: its use tree, or the item.
fn import_text(db: &TestDB, import: ImportOrExternCrate) -> String {
    match import {
        ImportOrExternCrate::Import(ImportId { use_, idx })
        | ImportOrExternCrate::Glob(GlobId { use_, idx }) => {
            use_tree_to_ast(db, use_.lookup(db).id, idx).syntax().text().to_string()
        }
        ImportOrExternCrate::ExternCrate(it) => {
            it.lookup(db).id.to_node(db).syntax().text().to_string()
        }
    }
}

/// [`used_imports`] of `krate`, a line for each import, `#[macro_use]` and ambiguous use,
/// sorted.
pub(super) fn render_used(db: &TestDB, krate: Crate) -> String {
    let used = used_imports(db, krate);
    let mut lines = Vec::new();
    for (import, how) in used.imports() {
        let mut line = format!("{} {how:?}", import_text(db, import));
        if let ImportOrExternCrate::Glob(glob) = import {
            let names: Vec<&str> = used.glob_names(glob).iter().map(Name::as_str).collect();
            line += &format!(" [{}]", names.join(", "));
        }
        lines.push(line);
    }
    for (extern_crate, how) in used.macro_uses() {
        let text = import_text(db, ImportOrExternCrate::ExternCrate(extern_crate));
        lines.push(format!("macro_use {text} {how:?}"));
    }
    for &(glob, ref name, ns) in used.ambiguous() {
        let text = import_text(db, ImportOrExternCrate::Glob(glob));
        lines.push(format!("ambiguous {} {ns:?} through {text}", name.as_str()));
    }
    lines.sort();
    lines.iter().map(|it| format!("{it}\n")).collect()
}

/// The records of `def_map`, each in its order, naming everything by its source.
pub(super) fn render_records(db: &TestDB, def_map: &DefMap) -> String {
    let NameRecords { uses, unresolved, glob_targets, macro_calls, derive_helpers, .. } =
        def_map.name_records();
    let mut out = String::new();
    for it in uses {
        let text = import_text(db, it.import);
        out += &format!("use {} {:?} {:?} {text}\n", it.name.as_str(), it.ns, it.used);
    }
    for &it in unresolved {
        out += &format!("unresolved {}\n", import_text(db, it));
    }
    for &(glob, module) in glob_targets {
        let target = module.name(db).map_or("crate".to_owned(), |it| it.as_str().to_owned());
        out += &format!("glob {} reads {target}\n", import_text(db, ImportOrExternCrate::Glob(glob)));
    }
    for it in macro_calls {
        out += &format!("call {}\n", via_text(db, it.via));
    }
    for it in derive_helpers {
        out += &format!("helper on {} before {}\n", item_name(db, it.item), it.before_derive);
    }
    out
}

fn via_text(db: &TestDB, via: MacroVia) -> String {
    match via {
        MacroVia::Textual => "textual".to_owned(),
        MacroVia::Path(None) => "declaration".to_owned(),
        MacroVia::Path(Some(it)) => format!("import {}", import_text(db, it)),
        MacroVia::MacroUse(None) => "macro_use of the prelude's crate".to_owned(),
        MacroVia::MacroUse(Some(it)) => {
            format!("macro_use {}", import_text(db, ImportOrExternCrate::ExternCrate(it)))
        }
        MacroVia::Prelude => "prelude".to_owned(),
    }
}

fn item_name(db: &TestDB, item: crate::AstId<ast::Item>) -> String {
    let node = item.to_node(db);
    let name = node.syntax().descendants().find_map(ast::Name::cast).unwrap();
    name.text().to_string()
}

fn check_used(#[rust_analyzer::rust_fixture] ra_fixture: &str, expect: Expect) {
    let db = TestDB::with_files(ra_fixture);
    expect.assert_eq(&render_used(&db, db.fetch_test_crate()));
}

fn check_used_in(name: &str, #[rust_analyzer::rust_fixture] ra_fixture: &str, expect: Expect) {
    let db = TestDB::with_files(ra_fixture);
    expect.assert_eq(&render_used(&db, crate_named(&db, name)));
}

fn check_records_in(name: &str, #[rust_analyzer::rust_fixture] ra_fixture: &str, expect: Expect) {
    let db = TestDB::with_files(ra_fixture);
    let def_map = crate_def_map(&db, crate_named(&db, name));
    expect.assert_eq(&render_records(&db, def_map));
}

#[test]
fn a_glob_records_the_names_used_through_it_only() {
    check_used(
        r#"
mod a { pub struct S; pub struct T; }
use a::*;
fn f(_: S) {}
"#,
        expect![[r#"
            a::* Scope [S]
        "#]],
    );
}

#[test]
fn two_globs_giving_a_name_different_items_keep_both() {
    let fixture = r#"
mod a { pub fn x() {} }
mod b { pub fn x() {} }
use a::*;
use b::*;
fn f() { x(); }
"#;
    check_used(
        fixture,
        expect![[r#"
            a::* Scope [x]
            ambiguous x Values through a::*
        "#]],
    );
    let db = TestDB::with_files(fixture);
    let def_map = crate_def_map(&db, db.fetch_test_crate());
    let scope = def_map.module(&db, def_map.root).scope(&db);
    let x = Name::new_symbol_root(intern::Symbol::intern("x"));
    let bound = scope.get(&x).values.unwrap();
    let [other] = scope.glob_ambiguities() else { panic!("{:?}", scope.glob_ambiguities()) };
    assert_eq!(scope.glob_ambiguity(&x, Namespace::Values), Some(other));
    let ItemInNs::Values(other_def) = other.other else { panic!("{other:?}") };
    assert_ne!(other_def, bound.def);
    assert_ne!(Some(ImportOrExternCrate::Glob(other.glob)), bound.import.map(Into::into));
    assert_eq!(import_text(&db, ImportOrExternCrate::Glob(other.glob)), "b::*");
}

#[test]
fn an_import_is_visible_as_the_prelude_gives_it_too() {
    let db = TestDB::with_files(
        r#"
//- minicore: option
use core::option::Option;
"#,
    );
    let def_map = crate_def_map(&db, db.fetch_test_crate());
    let scope = def_map.module(&db, def_map.root).scope(&db);
    let option = Name::new_symbol_root(sym::Option);
    let (import, _) = scope.import_of(&option, Namespace::Types).unwrap();
    assert_eq!(import_text(&db, import), "core::option::Option");
    let in_prelude = def_map.resolve_in_prelude(&db, &option).types.unwrap();
    assert_eq!(scope.get(&option).types.unwrap().def, in_prelude.def);
    assert!(matches!(in_prelude.def, ModuleDefId::AdtId(_)));
    assert_eq!(scope.import_of(&option, Namespace::Values), None);
}

#[test]
fn a_first_segment_is_found_in_scope_the_others_relative() {
    check_used(
        r#"
mod m { pub mod n { pub struct S; } pub struct T; }
use m::n;
use m::T as A;
use self::m::T as B;
fn f(_: n::S, _: A, _: self::B) {}
"#,
        expect![[r#"
            m::T as A Scope
            m::n Scope
            self::m::T as B Other
        "#]],
    );
}

#[test]
fn a_glob_read_through_another_glob_uses_both() {
    check_used(
        r#"
mod a { pub struct S; }
mod b { pub use super::a::*; }
use b::*;
fn f(_: S) {}
"#,
        expect![[r#"
            b::* Scope [S]
            super::a::* Other [S]
        "#]],
    );
}

#[test]
fn an_import_read_through_another_import_uses_both() {
    check_used(
        r#"
mod a { pub struct S; }
mod b { pub use super::a::S; }
use b::S;
fn f(_: S) {}
"#,
        expect![[r#"
            b::S Scope
            super::a::S Other
        "#]],
    );
}

#[test]
fn an_import_no_path_names_is_not_used() {
    check_used(
        r#"
mod a { pub struct S; pub struct T; }
use a::{S, T};
fn f(_: S) {}
"#,
        expect![[r#"
            S Scope
        "#]],
    );
}

#[test]
fn an_unresolved_import_counts_as_used() {
    check_used(
        r#"
use missing::thing;
"#,
        expect![[r#"
            missing::thing Other
        "#]],
    );
}

#[test]
fn an_extern_crate_the_extern_prelude_has_anyway_is_used_only_relative() {
    check_used_in(
        "main",
        r#"
//- /main.rs crate:main deps:dep
extern crate dep;
extern crate dep as renamed;
fn f(_: dep::S, _: renamed::S, _: crate::dep::S) {}
//- /dep.rs crate:dep
pub struct S;
"#,
        expect![[r#"
            extern crate dep as renamed; Scope
            extern crate dep; Other
        "#]],
    );
}

#[test]
fn a_macro_of_the_macro_use_prelude_uses_its_extern_crate() {
    let fixture = r#"
//- /main.rs crate:main deps:dep
#[macro_use]
extern crate dep;
made!();
//- /dep.rs crate:dep
#[macro_export]
macro_rules! made { () => {} }
"#;
    check_used_in(
        "main",
        fixture,
        expect![[r#"
            macro_use extern crate dep; Scope
        "#]],
    );
    let db = TestDB::with_files(fixture);
    let def_map = crate_def_map(&db, crate_named(&db, "main"));
    let [call] = def_map.name_records().macro_calls() else { panic!() };
    assert_eq!(via_text(&db, call.via), "macro_use extern crate dep;");
}

#[test]
fn a_macro_call_records_how_its_name_was_found() {
    let fixture = r#"
macro_rules! here { () => {} }
here!();
mod a {
    #[macro_export]
    macro_rules! there { () => {} }
}
use crate::there as again;
again!();
crate::there!();
"#;
    check_used(
        fixture,
        expect![[r#"
            crate::there as again Scope
        "#]],
    );
    let db = TestDB::with_files(fixture);
    let def_map = crate_def_map(&db, db.fetch_test_crate());
    let mut vias: Vec<String> =
        def_map.name_records().macro_calls().iter().map(|it| via_text(&db, it.via)).collect();
    vias.sort();
    assert_eq!(vias, ["declaration", "import crate::there as again", "textual"]);
}

#[test]
fn a_derive_helper_before_its_derive_resolves_as_rustc_s_compat_helper() {
    let db = TestDB::with_files(
        r#"
//- /main.rs crate:main deps:proc
#[rustc_builtin_macro]
pub macro derive($item:item) {}

#[helper]
#[derive(proc::Derive)]
struct Before;

#[derive(proc::Derive)]
#[helper]
struct After;

//- /proc.rs crate:proc
#![crate_type="proc-macro"]
#[proc_macro_derive(Derive, attributes(helper))]
fn derive() {}
"#,
    );
    let def_map = crate_def_map(&db, crate_named(&db, "main"));
    let mut helpers: Vec<(String, bool)> = (def_map.name_records().derive_helpers().iter())
        .map(|it| (item_name(&db, it.item), it.before_derive))
        .collect();
    helpers.sort();
    assert_eq!(helpers, [("After".to_owned(), false), ("Before".to_owned(), true)]);
    let dump = def_map.dump(&db);
    assert!(dump.contains("- After : type value") && dump.contains("- Before : type value"));
    let unresolved = def_map.diagnostics().iter().map(|it| format!("{:?}", it.kind));
    assert_eq!(unresolved.collect::<Vec<_>>(), Vec::<String>::new());
}

#[test]
fn the_records_name_what_a_def_map_resolved_through() {
    check_records_in(
        "main",
        r#"
//- /main.rs crate:main deps:dep
#[macro_use]
extern crate dep;
mod a { pub struct S; }
mod b { pub use super::a::*; }
use b::S;
use missing::thing;
made!();
//- /dep.rs crate:dep
#[macro_export]
macro_rules! made { () => {} }
"#,
        expect![[r#"
            use S Types Other super::a::*
            use S Values Other super::a::*
            use made Macros Scope extern crate dep;
            unresolved missing::thing
            glob super::a::* reads a
            call macro_use extern crate dep;
        "#]],
    );
}

#[test]
fn an_underscore_import_and_a_shadowed_one_are_kept() {
    let db = TestDB::with_files(
        r#"
mod m { pub trait Tr {} pub struct S; }
use m::Tr as _;
struct S;
use m::S;
"#,
    );
    let def_map = crate_def_map(&db, db.fetch_test_crate());
    let scope = def_map.module(&db, def_map.root).scope(&db);
    let [underscore] = scope.underscore_imports() else { panic!() };
    let import = ImportOrExternCrate::Import(underscore.import);
    assert_eq!(import_text(&db, import), "m::Tr as _");
    assert!(matches!(underscore.def, Some(ModuleDefId::TraitId(_))));
    let shadowed: Vec<_> = scope
        .shadowed_imports()
        .iter()
        .map(|it| (import_text(&db, it.import), it.name.as_str().to_owned(), it.item))
        .collect();
    let s = Name::new_symbol_root(intern::Symbol::intern("S"));
    let declared = scope.get(&s);
    assert_eq!(declared.types.unwrap().import, None);
    assert_eq!(shadowed.len(), 2, "{shadowed:?}");
    for (text, name, item) in shadowed {
        assert_eq!((text.as_str(), name.as_str()), ("m::S", "S"));
        let def = match item {
            ItemInNs::Types(it) | ItemInNs::Values(it) => it,
            ItemInNs::Macros(_) => panic!(),
        };
        assert_ne!(Some(def), declared.types.map(|it| it.def));
    }
}

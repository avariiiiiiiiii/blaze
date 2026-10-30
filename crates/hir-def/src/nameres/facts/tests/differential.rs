//! The per-module resolver against the old collector on every fixture of the name resolution and
//! macro expansion tests, with the builtin-derive fast path on and off, crate maps and the block
//! maps of every body.
//!
//! The fixtures are read out of the test files themselves (`include_str!`), not copied: every
//! raw string literal of a file outside an `expect![...]` is a fixture, named by the file, the
//! item it is in and its place there. A literal that is not a fixture (a file's new text in an
//! incremental test) is still a crate, compared like any other; one `ChangeFixture` can't parse
//! is listed by name.

use std::panic::{self, AssertUnwindSafe};

use super::{Exempt, compare_fixture, fixture_db, with_fast_path};
use crate::{
    ModuleDefId,
    nameres::facts::tests::{CDef, CModule, new_crate_local_def_map},
    test_db::TestDBExt,
};

/// One fixture of a test file: where it is, and its text.
pub(super) struct Fixture {
    pub(super) name: String,
    pub(super) text: &'static str,
}

/// The raw string literals of `source`, each named `file::item#k` by the `fn`, `const` or
/// `static` it is in. Comments, plain strings, char literals and `expect![...]` arguments are
/// skipped, so a fixture that holds `#[test]` or a quote is read whole.
pub(super) fn fixtures(file: &str, source: &'static str) -> Vec<Fixture> {
    let b = source.as_bytes();
    let ident = |at: usize| b[at].is_ascii_alphanumeric() || b[at] == b'_';
    let mut out = Vec::new();
    let mut item = "top";
    let mut k = 0;
    let mut i = 0;
    while i < b.len() {
        let rest = &source[i..];
        if rest.starts_with("//") {
            i += rest.find('\n').unwrap_or(rest.len());
        } else if rest.starts_with("/*") {
            i += rest.find("*/").map_or(rest.len(), |it| it + 2);
        } else if b[i] == b'"' {
            let mut j = i + 1;
            while j < b.len() && b[j] != b'"' {
                j += if b[j] == b'\\' { 2 } else { 1 };
            }
            i = j + 1;
        } else if b[i] == b'\'' {
            // A char literal (`'"'`, `'\''`), or a lifetime, which has no closing quote.
            if b.get(i + 1) == Some(&b'\\') {
                i += rest[2..].find('\'').map_or(rest.len(), |it| it + 3);
            } else if b.get(i + 2) == Some(&b'\'') {
                i += 3;
            } else {
                i += 1;
            }
        } else if b[i] == b'r' && (i == 0 || !ident(i - 1)) && {
            let hashes = rest[1..].bytes().take_while(|&it| it == b'#').count();
            rest.as_bytes().get(1 + hashes) == Some(&b'"')
        } {
            let hashes = rest[1..].bytes().take_while(|&it| it == b'#').count();
            let body_start = i + 2 + hashes;
            let close = format!("\"{}", "#".repeat(hashes));
            let Some(len) = source[body_start..].find(&close) else { break };
            let text = &source[body_start..body_start + len];
            let before = source[..i].trim_end();
            if !(before.ends_with("expect![[") || before.ends_with("expect![")) {
                out.push(Fixture { name: format!("{file}::{item}#{k}"), text });
                k += 1;
            }
            i = body_start + len + close.len();
        } else if (i == 0 || !ident(i - 1))
            && ["fn ", "const ", "static "].iter().any(|kw| rest.starts_with(kw))
        {
            let after = rest.find(' ').map_or(rest.len(), |it| it + 1);
            let name_len = rest[after..]
                .bytes()
                .take_while(|it| it.is_ascii_alphanumeric() || *it == b'_')
                .count();
            if name_len > 0 {
                item = &source[i + after..i + after + name_len];
                k = 0;
            }
            i += after;
        } else {
            i += 1;
        }
    }
    out
}

/// Every fixture of `files`, compared; panics listing each difference, each fixture whose
/// comparison panicked, and (on stderr) each literal that is not a fixture.
fn check_files(files: &[(&str, &'static str)]) {
    let mut failures = String::new();
    let mut compared = 0;
    let mut not_fixtures = Vec::new();
    for &(file, source) in files {
        for Fixture { name, text } in fixtures(file, source) {
            if ORDER_DEPENDENT.iter().any(|it| it.name == name) {
                continue;
            }
            // `ChangeFixture` panics on text it can't read as a fixture; that literal is not one.
            let parses = panic::catch_unwind(|| drop(fixture_db(text))).is_ok();
            if !parses {
                not_fixtures.push(name);
                continue;
            }
            compared += 1;
            let result = panic::catch_unwind(AssertUnwindSafe(|| {
                let mut out = String::new();
                compare_fixture(&name, text, &Exempt::NONE, &mut out);
                out
            }));
            match result {
                Ok(out) => failures.push_str(&out),
                Err(payload) => {
                    let message = payload
                        .downcast_ref::<String>()
                        .map(String::as_str)
                        .or_else(|| payload.downcast_ref::<&str>().copied())
                        .unwrap_or("(no message)");
                    failures.push_str(&format!("== {name}: panicked: {message}\n{text}\n"));
                }
            }
        }
    }
    if !not_fixtures.is_empty() {
        eprintln!("not fixtures ({}): {}", not_fixtures.len(), not_fixtures.join(", "));
    }
    assert!(compared > 0, "no fixture in {:?}", files.iter().map(|it| it.0).collect::<Vec<_>>());
    assert!(failures.is_empty(), "old and new def maps differ:\n{failures}");
}

macro_rules! sources {
    ($($path:literal),* $(,)?) => {
        &[$(($path, include_str!(concat!("../../", $path)))),*]
    };
}

/// The test files whose fixtures are compared, relative to `nameres/`.
const NAMERES: &[(&str, &str)] = sources![
    "tests.rs",
    "tests/backdate.rs",
    "tests/batch_order.rs",
    "tests/globs.rs",
    "tests/imports.rs",
    "tests/incremental.rs",
    "tests/macros.rs",
    "tests/mod_resolution.rs",
    "tests/paths.rs",
    "tests/worklist.rs",
];

const MACRO_EXPANSION: &[(&str, &str)] = sources![
    "../macro_expansion_tests/mod.rs",
    "../macro_expansion_tests/builtin_derive_macro.rs",
    "../macro_expansion_tests/builtin_fn_macro.rs",
    "../macro_expansion_tests/mbe.rs",
    "../macro_expansion_tests/proc_macros.rs",
    "../macro_expansion_tests/mbe/matching.rs",
    "../macro_expansion_tests/mbe/meta_syntax.rs",
    "../macro_expansion_tests/mbe/metavar_expr.rs",
    "../macro_expansion_tests/mbe/regression.rs",
    "../macro_expansion_tests/mbe/tt_conversion.rs",
];

#[test]
fn every_test_file_is_compared() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/src/nameres/");
    let listed: Vec<&str> = NAMERES.iter().chain(MACRO_EXPANSION).map(|it| it.0).collect();
    for dir in ["tests", "../macro_expansion_tests", "../macro_expansion_tests/mbe"] {
        for entry in std::fs::read_dir(format!("{root}{dir}")).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|it| it == "rs") {
                let file = format!("{dir}/{}", path.file_name().unwrap().to_str().unwrap());
                assert!(listed.contains(&file.as_str()), "{file} is not in the differential's lists");
            }
        }
    }
}

#[test]
fn nameres_tests() {
    check_files(&NAMERES[..1]);
}

#[test]
fn nameres_tests_globs_imports_paths() {
    check_files(&[NAMERES[3], NAMERES[4], NAMERES[8]]);
}

#[test]
fn nameres_tests_macros() {
    check_files(&[NAMERES[6]]);
}

#[test]
fn nameres_tests_mod_resolution() {
    check_files(&[NAMERES[7]]);
}

#[test]
fn nameres_tests_incremental_and_scheduling() {
    check_files(&[NAMERES[1], NAMERES[2], NAMERES[5], NAMERES[9]]);
}

#[test]
fn macro_expansion_tests() {
    check_files(&MACRO_EXPANSION[..5]);
}

#[test]
fn macro_expansion_tests_mbe() {
    check_files(&MACRO_EXPANSION[5..]);
}

#[test]
fn the_reader_finds_fixtures_and_skips_expectations() {
    let source = concat!(
        "// r#\"not this\"#\n",
        "const A: &str = r#\"//- /lib.rs\nstruct S;\"#;\n",
        "#[test]\nfn t() {\n",
        "    let c = '\"'; let d = '\\''; let s = \"r#\\\"no\\\"#\";\n",
        "    check(r##\"#[test] fn x() {} \"# \"##, expect![[r#\"crate\"#]]);\n",
        "}\n",
    );
    let found = fixtures("f.rs", source);
    let found: Vec<(&str, &str)> = found.iter().map(|it| (it.name.as_str(), it.text)).collect();
    assert_eq!(
        found,
        [("f.rs::A#0", "//- /lib.rs\nstruct S;"), ("f.rs::t#0", "#[test] fn x() {} \"# ")]
    );
}

// ---------------------------------------------------------------------------------------------
// Fixtures of the notes' §8: where the old collector's answer followed its schedule, on
// programs rustc rejects. Compared on everything but what `exempt` names.

pub(super) struct OrderDependent {
    /// Our own name, or the name [`fixtures`] gives one of the test files' fixtures.
    pub(super) name: &'static str,
    /// What rustc (the pinned nightly) says of the program.
    pub(super) rustc: &'static str,
    /// The text; empty for a fixture of the test files, which is read from there.
    pub(super) fixture: &'static str,
    pub(super) exempt: Exempt,
}

impl OrderDependent {
    fn text(&self) -> &'static str {
        if !self.fixture.is_empty() {
            return self.fixture;
        }
        NAMERES
            .iter()
            .chain(MACRO_EXPANSION)
            .flat_map(|&(file, source)| fixtures(file, source))
            .find(|it| it.name == self.name)
            .unwrap_or_else(|| panic!("no fixture `{}` in the test files", self.name))
            .text
    }
}

pub(super) const ORDER_DEPENDENT: &[OrderDependent] = &[
    OrderDependent {
        name: "two_globs_bring_one_name",
        rustc: "error[E0659]: `S` is ambiguous (glob import vs glob import in the same namespace)",
        fixture: r#"
mod a { pub struct S; }
mod b { pub struct S; }
use a::*;
use b::*;
pub use S as Exported;
"#,
        exempt: Exempt { names: &["S", "Exported"], unresolved_imports: false },
    },
    OrderDependent {
        name: "two_globs_through_a_cycle",
        rustc: "error[E0659]: `T` is ambiguous (glob import vs glob import in the same namespace)",
        fixture: r#"
mod x { pub struct T; }
mod y { pub fn T() {} pub struct T {} }
mod p { pub use crate::q::*; pub use crate::x::*; }
mod q { pub use crate::p::*; pub use crate::y::*; }
use p::T;
"#,
        exempt: Exempt { names: &["T"], unresolved_imports: false },
    },
    OrderDependent {
        name: "import_resolved_before_an_expansion_shadows_its_glob",
        rustc: "error[E0659]: `T` is ambiguous (macro-expanded name vs less macro-expanded name \
                from import or glob)",
        fixture: r#"
//- /lib.rs crate:main deps:m
mod x { pub struct T; }
mod foo {
    pub use crate::x::*;
    m::make_t!();
}
use foo::T;

//- /m.rs crate:m
#[macro_export]
macro_rules! make_t { () => { pub struct T; } }
"#,
        exempt: Exempt { names: &["T"], unresolved_imports: false },
    },
    OrderDependent {
        name: "unresolved_attributes_in_two_modules",
        rustc: "error: cannot find attribute `first` in this scope; \
                error: cannot find attribute `second` in this scope",
        fixture: r#"
mod a {
    #[first]
    pub struct A;
    pub use crate::b::B as BB;
}
mod b {
    #[second]
    pub struct B;
    pub use crate::a::A as AA;
}
use a::BB;
use b::AA;
"#,
        exempt: Exempt { names: &[], unresolved_imports: true },
    },
];

#[test]
fn order_dependent_fixtures_agree_on_everything_else() {
    let mut failures = String::new();
    for it in ORDER_DEPENDENT {
        assert!(!it.rustc.is_empty(), "{}: say what rustc says of it", it.name);
        compare_fixture(it.name, it.text(), &it.exempt, &mut failures);
    }
    assert!(failures.is_empty(), "old and new def maps differ:\n{failures}");
}

/// Of two globs bringing one name into a module, the new resolver keeps the first by position
/// (the notes' §8: component order, then module order, then position).
#[test]
fn two_globs_keep_the_first_by_position() {
    let fixture = ORDER_DEPENDENT[0].fixture;
    for on in [true, false] {
        with_fast_path(on, || {
            let db = fixture_db(fixture);
            let krate = db.fetch_test_crate();
            let map = new_crate_local_def_map(&db, krate).def_map(&db);
            let p = super::Projector::new(&db, map);
            let s = hir_expand::name::Name::new_symbol_root(intern::Symbol::intern("S"));
            let (def, _) = map.module(&db, map.root).scope(&db).type_(&s).expect("`S` is in scope");
            let ModuleDefId::AdtId(adt) = def else { panic!("`S` is {def:?}") };
            let CDef::Struct(loc) = p.adt(adt) else { panic!("`S` is {def:?}") };
            let CModule::Local(key) = loc.container else { panic!("`S` is foreign") };
            assert_eq!(key.display(), "crate::a", "fast path {on}");
        });
    }
}

// ---------------------------------------------------------------------------------------------
// Fixtures of our own, for what the test files leave out.

const OWN: &[(&str, &str)] = &[
    (
        "glob_cycle_with_shadowing",
        r#"
mod a { pub use crate::b::*; pub struct A; pub fn shared() {} }
mod b { pub use crate::c::*; pub struct B; }
mod c { pub use crate::a::*; pub struct C; pub struct shared; }
pub use a::*;
fn shared() {}
"#,
    ),
    (
        "restricted_visibilities",
        r#"
pub mod outer {
    pub mod inner {
        pub(in crate::outer) struct InOuter;
        pub(super) struct ToOuter;
        pub(crate) struct ToCrate;
        pub(self) struct Here;
    }
    pub use inner::*;
}
use outer::*;
"#,
    ),
    (
        "extern_crate_self_and_dollar_crate",
        r#"
//- /lib.rs crate:main deps:dep
extern crate self as this;
pub mod m { pub struct M; }
use this::m::M;
dep::make!();
use generated::G;

//- /dep.rs crate:dep
#[macro_export]
macro_rules! make {
    () => { pub mod generated { pub struct G; pub use $crate::Dep as D; } };
}
pub struct Dep;
"#,
    ),
    (
        "macro_use_and_export_through_modules",
        r#"
#[macro_use]
mod macros {
    macro_rules! inner { () => { pub struct FromInner; } }
    #[macro_export]
    macro_rules! exported { ($n:ident) => { pub mod $n { pub use crate::Root; } } }
}
pub struct Root;
inner!();
exported!(made);
mod later { crate::exported!(nested); use self::nested::Root as R; }
"#,
    ),
    (
        "cfg_items_and_imports",
        r#"
//- /lib.rs crate:main cfg:on
#[cfg(on)] pub mod yes { pub struct Y; }
#[cfg(not(on))] pub mod no { pub struct N; }
#[cfg(on)] use yes::Y;
#[cfg(not(on))] use no::N;
"#,
    ),
    (
        "within_round_write_seen_next_round",
        r#"
macro_rules! define { () => { pub mod gen { #[macro_export] macro_rules! late { () => { pub struct Late; } } } }; }
define!();
crate::late!();
mod user { crate::late!(); }
"#,
    ),
    (
        "blocks_with_items_and_imports",
        r#"
mod m { pub struct S; pub mod n { pub fn f() {} } }
fn outer() {
    use m::*;
    struct Local;
    fn inner() {
        use super::m::n::f;
        mod deep { pub use crate::m::S; }
        { use deep::S as T; }
    }
}
"#,
    ),
];

#[test]
fn own_fixtures() {
    let mut failures = String::new();
    for &(name, fixture) in OWN {
        compare_fixture(name, fixture, &Exempt::NONE, &mut failures);
    }
    assert!(failures.is_empty(), "old and new def maps differ:\n{failures}");
}

//! The stability lints (`deprecated`, `deprecated_in_future`, `useless_deprecated`, the
//! `unused_attributes` of a `#[deprecated]` with no effect, `#[macro_escape]`) against rustc's
//! UI tests.

use crate::{
    Options,
    emit::Style,
    tests::{Mode, checked, checked_with, compiletest, normalized},
};

/// What rustc writes compiling `fixture` (its files named `$DIR/<path>`) as compiletest runs a
/// UI test of pass mode `mode`.
fn ui(fixture: &str, mode: Mode) -> String {
    let checked = checked_with(fixture, "$DIR/", &compiletest(mode, &Options::default()));
    normalized(&checked.stderr_with(Style { ui_testing: true, ..Style::default() }))
}

/// The UI test `source`, named `$DIR/<name>`, of edition 2015 with no other crate.
fn one(name: &str, source: &str, mode: Mode) -> String {
    ui(&format!("//- /{name} crate:main edition:2015\n{source}"), mode)
}

#[test]
fn deprecation_in_future() {
    let source = include_str!("stability/deprecation-in-future.rs");
    let expected = include_str!("stability/deprecation-in-future.stderr");
    assert_eq!(one("deprecation-in-future.rs", source, Mode::Other), expected);
}

#[test]
fn deprecated_macro_escape() {
    let source = include_str!("stability/deprecated-macro_escape.rs");
    let expected = include_str!("stability/deprecated-macro_escape.stderr");
    assert_eq!(one("deprecated-macro_escape.rs", source, Mode::RunPass), expected);
}

#[test]
fn deprecated_macro_escape_inner() {
    let source = include_str!("stability/deprecated-macro_escape-inner.rs");
    let expected = include_str!("stability/deprecated-macro_escape-inner.stderr");
    assert_eq!(one("deprecated-macro_escape-inner.rs", source, Mode::RunPass), expected);
}

#[test]
fn unit_and_tuple_struct() {
    let source = include_str!("stability/unit_and_tuple_struct.rs");
    let expected = include_str!("stability/unit_and_tuple_struct.stderr");
    assert_eq!(one("unit_and_tuple_struct.rs", source, Mode::Other), expected);
}

#[test]
fn deprecation_lint_nested() {
    let source = include_str!("stability/deprecation-lint-nested.rs");
    let expected = include_str!("stability/deprecation-lint-nested.stderr");
    let fixture = format!("//- minicore: fmt\n//- /deprecation-lint-nested.rs crate:main edition:2015\n{source}");
    assert_eq!(ui(&fixture, Mode::Other), expected);
}

/// `auxiliary/deprecation-lint.rs` as the crate `deprecation_lint`, with the UI test `source`
/// named `$DIR/<name>` depending on it.
fn with_deprecation_lint(name: &str, source: &str, header: &str, deps: &str) -> String {
    let aux = include_str!("stability/auxiliary/deprecation-lint.rs");
    format!(
        "{header}//- /auxiliary/deprecation-lint.rs crate:deprecation_lint edition:2015 library\n{aux}\
         //- /{name} crate:main edition:2015 deps:deprecation_lint{deps}\n{source}"
    )
}

#[test]
fn deprecation_lint() {
    let source = include_str!("stability/deprecation-lint.rs");
    let expected = include_str!("stability/deprecation-lint.stderr");
    // The test's `Option<String>` and `#[derive(Debug)]`: the prelude of a `std` with them.
    let std = "//- minicore: option, fmt, derive
//- /std.rs crate:std edition:2015 library
pub mod string {
    pub struct String;
}
pub mod prelude {
    pub mod rust_2015 {
        pub use core::prelude::rust_2015::*;
        pub use crate::string::String;
    }
}
";
    let fixture = with_deprecation_lint("deprecation-lint.rs", source, std, ",std");
    assert_eq!(ui(&fixture, Mode::Other), expected);
}

#[test]
fn deprecation_lint_2() {
    let source = include_str!("stability/deprecation-lint-2.rs");
    let expected = include_str!("stability/deprecation-lint-2.stderr");
    assert_eq!(ui(&with_deprecation_lint("deprecation-lint-2.rs", source, "", ""), Mode::Other), expected);
}

#[test]
fn deprecation_lint_3() {
    let source = include_str!("stability/deprecation-lint-3.rs");
    let expected = include_str!("stability/deprecation-lint-3.stderr");
    assert_eq!(ui(&with_deprecation_lint("deprecation-lint-3.rs", source, "", ""), Mode::Other), expected);
}

#[test]
fn lint_stability_deprecated() {
    let source = include_str!("stability/lint-stability-deprecated.rs");
    let expected = include_str!("stability/lint-stability-deprecated.stderr");
    let fixture = format!(
        "//- /auxiliary/lint_stability.rs crate:lint_stability edition:2015 library\n{}\
         //- /auxiliary/inherited_stability.rs crate:inherited_stability edition:2015 library\n{}\
         //- /auxiliary/stability_cfg1.rs crate:stability_cfg1 edition:2015 library\n{}\
         //- /auxiliary/stability-cfg2.rs crate:stability_cfg2 edition:2015 library cfg:foo\n{}\
         //- /lint-stability-deprecated.rs crate:main edition:2015 \
         deps:lint_stability,inherited_stability,stability_cfg1,stability_cfg2\n{source}",
        include_str!("stability/auxiliary/lint_stability.rs"),
        include_str!("stability/auxiliary/inherited_stability.rs"),
        include_str!("stability/auxiliary/stability_cfg1.rs"),
        include_str!("stability/auxiliary/stability-cfg2.rs"),
    );
    assert_eq!(ui(&fixture, Mode::Other), expected);
}

/// `deprecation-sanity.rs`'s `useless_deprecated`, as its `.stderr` has it. The test's other
/// errors are those of the parser of `#[deprecated]`.
#[test]
fn deprecated_on_a_trait_impl_is_useless() {
    let source = include_str!("stability/deprecation-sanity.rs");
    let expected = "error: the `deprecated` attribute cannot be used on trait impl blocks
  --> $DIR/deprecation-sanity.rs:40:3
   |
LL | #[deprecated = \"hello\"]
   |   ^^^^^^^^^^
   |
   = help: the `deprecated` attribute can be applied to associated consts, associated types, constants, crates, data types, enum variants, foreign statics, functions, inherent impl blocks, macro defs, modules, statics, struct fields, traits, type aliases, and use statements
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: `#[deny(useless_deprecated)]` on by default
";
    let stderr = one("deprecation-sanity.rs", source, Mode::Other);
    assert!(stderr.contains(expected), "{stderr}");
}

#[test]
fn deprecated_on_an_item_of_a_trait_impl_has_no_effect() {
    let checked = checked(
        r#"//- /main.rs crate:main edition:2021
trait Tr {
    const C: u8;
    type T;
}
struct S;
impl Tr for S {
    #[deprecated]
    const C: u8 = 0;
    #[deprecated]
    type T = ();
}
fn main() {}
"#,
    );
    let messages: Vec<String> =
        checked.messages().into_iter().filter(|it| it.contains("`#[deprecated]` annotation")).collect();
    assert_eq!(
        messages,
        [
            "unused_attributes: this `#[deprecated]` annotation has no effect",
            "unused_attributes: this `#[deprecated]` annotation has no effect",
        ]
    );
}

#[test]
fn a_use_in_a_definition_deprecated_by_the_same_attribute_is_not_linted() {
    let checked = checked(
        r#"//- /main.rs crate:main edition:2021
#[deprecated]
mod old {
    pub fn f() {}
    pub fn g() { f(); }
}
#[deprecated(note = "use h")]
fn e() {}
#[deprecated]
fn d() { e(); }
fn main() {
    old::g();
}
"#,
    );
    let messages: Vec<String> = checked.messages().into_iter().filter(|it| it.starts_with("deprecated")).collect();
    assert_eq!(
        messages,
        ["deprecated: use of deprecated function `e`: use h", "deprecated: use of deprecated function `old::g`"]
    );
}

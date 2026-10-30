//! The item lints of `lints::names::items` (naming, lifetimes, `#[no_mangle]`, privacy,
//! `improper_ctypes`) against rustc's UI tests, and across two crates.

use crate::tests::{checked, ui};

/// The UI test `items/<name>` against its `.stderr`, as compiletest runs it.
fn ui_test(name: &str, source: &str, expected: &str) {
    assert_eq!(ui(name, "2015", source), expected);
}

#[test]
fn lint_non_uppercase_statics() {
    ui_test(
        "lint-non-uppercase-statics.rs",
        include_str!("items/lint-non-uppercase-statics.rs"),
        include_str!("items/lint-non-uppercase-statics.stderr"),
    );
}

#[test]
fn zero_uses_in_fn() {
    ui_test(
        "zero-uses-in-fn.rs",
        include_str!("items/zero-uses-in-fn.rs"),
        include_str!("items/zero-uses-in-fn.stderr"),
    );
}

#[test]
fn zero_uses_in_impl() {
    ui_test(
        "zero-uses-in-impl.rs",
        include_str!("items/zero-uses-in-impl.rs"),
        include_str!("items/zero-uses-in-impl.stderr"),
    );
}

#[test]
fn one_use_in_fn_argument() {
    ui_test(
        "one-use-in-fn-argument.rs",
        include_str!("items/one-use-in-fn-argument.rs"),
        include_str!("items/one-use-in-fn-argument.stderr"),
    );
}

#[test]
fn one_use_in_inherent_impl_header() {
    ui_test(
        "one-use-in-inherent-impl-header.rs",
        include_str!("items/one-use-in-inherent-impl-header.rs"),
        include_str!("items/one-use-in-inherent-impl-header.stderr"),
    );
}

#[test]
fn generic_no_mangle() {
    ui_test(
        "generic-no-mangle.rs",
        include_str!("items/generic-no-mangle.rs"),
        include_str!("items/generic-no-mangle.stderr"),
    );
}

#[test]
fn improper_ctypes_73251_1() {
    ui_test("lint-73251-1.rs", include_str!("items/lint-73251-1.rs"), include_str!("items/lint-73251-1.stderr"));
}

#[test]
fn improper_ctypes_73249_2() {
    ui_test("lint-73249-2.rs", include_str!("items/lint-73249-2.rs"), include_str!("items/lint-73249-2.stderr"));
}

/// A struct of another crate without `repr(C)` is not FFI-safe in an `extern` block or an
/// `extern "C"` fn of this one; rustc points at its definition only when it is local, and the
/// diagnostic reads the struct's representation.
#[test]
fn improper_ctypes_of_a_foreign_struct() {
    let fixture = r#"
//- /dep.rs crate:dep library
#[repr(C)]
pub struct Good { pub x: u8 }
pub struct Bad { pub x: u8 }
//- /main.rs crate:main deps:dep
#![allow(dead_code)]
use dep::{Bad, Good};
extern "C" {
    fn good(x: Good);
    fn bad(x: Bad);
}
pub extern "C" fn definition(_x: Bad) {}
fn main() {}
"#;
    let checked = checked(fixture);
    let messages = checked.messages();
    assert_eq!(
        messages,
        [
            "improper_ctypes: `extern` block uses type `Bad`, which is not FFI-safe",
            "improper_ctypes_definitions: `extern` fn uses type `Bad`, which is not FFI-safe",
        ],
        "{messages:?}"
    );
    let stderr = checked.stderr();
    assert!(!stderr.contains("the type is defined here"), "{stderr}");
    for diag in &checked.diags {
        assert!(diag.facts.iter().any(|it| it.what.contains("representation of") && it.what.contains("Bad")), "{diag:?}");
    }
}

/// rustc's `clashing_extern_declarations` compares the declarations of one crate only: the same
/// symbol declared differently by a dependency is no clash.
#[test]
fn clashing_extern_declarations_are_per_crate() {
    let fixture = r#"
//- /dep.rs crate:dep library
extern "C" {
    pub fn clash(x: u8);
}
//- /main.rs crate:main deps:dep
#![allow(dead_code)]
extern "C" {
    fn clash(x: u16);
}
mod inner {
    extern "C" {
        fn clash(x: u32);
    }
}
fn main() {}
"#;
    let messages = checked(fixture).messages();
    assert_eq!(messages, ["clashing_extern_declarations: `clash` redeclared with a different signature"], "{messages:?}");
}

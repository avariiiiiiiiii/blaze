//! The item lints of `lints::names::items` (naming, lifetimes, `#[no_mangle]`, privacy,
//! `improper_ctypes`, runtime symbols) against rustc's UI tests, and across two crates.

use crate::{
    Options,
    emit::Style,
    tests::{Mode, checked, checked_with, compiletest, normalized, ui},
};

/// The UI test `items/<name>` against its `.stderr`, as compiletest runs it.
fn ui_test(name: &str, source: &str, expected: &str) {
    assert_eq!(ui(name, "2015", source), expected);
}

/// What rustc writes compiling `fixture` (its files named `$DIR/<path>`) as compiletest runs a
/// UI test that is not `run-pass`.
fn ui_fixture(fixture: &str) -> String {
    let options = compiletest(Mode::Other, &Options::default());
    let stderr = checked_with(fixture, "$DIR/", &options).stderr_with(Style { ui_testing: true, ..Style::default() });
    normalized(&stderr)
}

/// A `core` and a `std` of what the FFI tests use: `Option`, `ptr::null_mut`, `unreachable!`, and
/// `ffi` with `c_void` and the runtime's functions declared `#[rustc_canonical_symbol]` as
/// `library/core/src/ffi/mod.rs` declares them.
const FFI_STD: &str = r#"//- /core.rs crate:core edition:2021 library
#![feature(no_core, lang_items, rustc_attrs, fundamental)]
#![no_core]
pub mod marker {
    #[lang = "pointee_sized"]
    #[fundamental]
    #[rustc_specialization_trait]
    #[rustc_coinductive]
    pub trait PointeeSized {}
    #[lang = "meta_sized"]
    #[fundamental]
    #[rustc_specialization_trait]
    #[rustc_coinductive]
    pub trait MetaSized: PointeeSized {}
    #[lang = "sized"]
    #[fundamental]
    #[rustc_specialization_trait]
    #[rustc_coinductive]
    pub trait Sized: MetaSized {}
}
pub mod option {
    #[lang = "Option"]
    pub enum Option<T> {
        #[lang = "None"]
        None,
        #[lang = "Some"]
        Some(T),
    }
}
pub mod ptr {
    pub const fn null_mut<T>() -> *mut T {
        0 as *mut T
    }
}
pub mod ffi {
    pub type c_char = i8;
    pub type c_int = i32;
    #[lang = "c_void"]
    #[repr(u8)]
    pub enum c_void {
        __variant1,
        __variant2,
    }
    mod runtime_symbols {
        use super::{c_char, c_int, c_void};
        unsafe extern "C" {
            #[rustc_canonical_symbol]
            fn memcpy(dest: *mut c_void, src: *const c_void, n: usize) -> *mut c_void;
            #[rustc_canonical_symbol]
            fn memmove(dest: *mut c_void, src: *const c_void, n: usize) -> *mut c_void;
            #[rustc_canonical_symbol]
            fn memset(s: *mut c_void, c: c_int, n: usize) -> *mut c_void;
            #[rustc_canonical_symbol]
            fn memcmp(s1: *const c_void, s2: *const c_void, n: usize) -> c_int;
            #[rustc_canonical_symbol]
            fn bcmp(s1: *const c_void, s2: *const c_void, n: usize) -> c_int;
            #[rustc_canonical_symbol]
            fn strlen(s: *const c_char) -> usize;
        }
    }
}
//- /std.rs crate:std edition:2021 deps:core library
#![no_std]
pub use core::{ffi, option, ptr};
#[macro_export]
macro_rules! unreachable {
    () => {
        loop {}
    };
}
pub mod prelude {
    pub mod rust_2015 {
        pub use crate::unreachable;
        pub use core::option::Option::{self, None, Some};
    }
    pub mod rust_2021 {
        pub use crate::unreachable;
        pub use core::option::Option::{self, None, Some};
    }
}
"#;

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

/// `tests/ui/lint/runtime-symbols.rs`, its `core` the symbols `core` declares canonical, with the
/// test's `normalize-stderr` of `*const i8`/`*const u8` applied.
#[test]
fn runtime_symbols() {
    let source = include_str!("items/runtime-symbols.rs");
    let fixture = format!("{FFI_STD}//- /runtime-symbols.rs crate:main edition:2021 deps:core,std\n{source}");
    let stderr = ui_fixture(&fixture).replace("*const i8", "*const U8").replace("*const u8", "*const U8");
    assert_eq!(stderr, include_str!("items/runtime-symbols.stderr"));
}

/// A definition of a runtime symbol with the runtime's signature, a `#[no_mangle]` function
/// whose symbol is no runtime's, and a mangled `strlen` give nothing; the lint reads the
/// signatures of both functions.
#[test]
fn runtime_symbols_matching_and_mangled() {
    let source = r#"
#![allow(dead_code)]
use core::ffi::{c_char, c_void};
#[no_mangle]
pub unsafe extern "C" fn strlen(s: *const c_char) -> usize { 0 }
#[no_mangle]
pub extern "C" fn not_a_runtime_symbol() {}
mod inner {
    pub fn memcpy() {}
}
#[no_mangle]
pub extern "C" fn memset(s: *mut c_void, c: i64, n: usize) -> *mut c_void { s }
fn main() {}
"#;
    let fixture = format!("{FFI_STD}//- /main.rs crate:main edition:2021 deps:core,std\n{source}");
    let checked = checked(&fixture);
    let messages = checked.messages();
    assert_eq!(
        messages,
        ["suspicious definition of the runtime `memset` symbol used by the standard library"],
        "{messages:?}"
    );
    let facts: Vec<&str> = checked.diags[0].facts.iter().map(|it| it.what.as_str()).collect();
    assert!(facts.contains(&"the signature of the runtime `memset`"), "{facts:?}");
    assert!(facts.contains(&"the signature of `memset`"), "{facts:?}");
}

/// `tests/ui/lint/c-void-returns.rs`: a free function, a foreign function and a `fn` pointer
/// type returning `c_void`.
#[test]
fn c_void_returns() {
    let source = include_str!("items/c-void-returns.rs");
    let fixture = format!("{FFI_STD}//- /c-void-returns.rs crate:main edition:2015 deps:core,std\n{source}");
    assert_eq!(ui_fixture(&fixture), include_str!("items/c-void-returns.stderr"));
}

/// A method and a closure returning `c_void` get the message of a foreign declaration, as
/// rustc's `FnKind` other than a Rust-ABI item function does; `#[allow]` on the closure's
/// statement allows its lint.
#[test]
fn c_void_returns_of_methods_and_closures() {
    let source = r#"
#![allow(dead_code)]
use std::ffi::c_void;
struct S;
impl S {
    fn m() -> c_void { unreachable!() }
}
fn f() {
    let _a = || -> c_void { unreachable!() };
    #[allow(c_void_returns)]
    let _b = || -> c_void { unreachable!() };
}
fn main() {}
"#;
    let fixture = format!("{FFI_STD}//- /main.rs crate:main edition:2021 deps:core,std\n{source}");
    let messages = checked(&fixture).messages();
    let extern_ = "declarations returning `c_void` are not compatible with C functions returning `void`";
    assert_eq!(messages, [extern_, extern_], "{messages:?}");
}

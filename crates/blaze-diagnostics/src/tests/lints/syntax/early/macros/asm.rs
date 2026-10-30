//! The lints of `asm!`, `global_asm!` and `naked_asm!` templates, against rustc's UI tests in
//! `tests/ui/asm` (MIT or Apache-2.0), kept byte for byte in `asm/`.

use super::{compiletest, ui_fixture};

/// The std of a UI test that uses `std::arch`'s macros: those and `include_str!`, in the
/// prelude, as std has them.
const STD: &str = r#"//- /std.rs crate:std library
pub mod arch {
    #[rustc_builtin_macro]
    pub macro asm("assembly template", $(operands,)* $(options($(option),*))?) {
        /* compiler built-in */
    }
    #[rustc_builtin_macro]
    pub macro global_asm("assembly template", $(operands,)* $(options($(option),*))?) {
        /* compiler built-in */
    }
    #[rustc_builtin_macro]
    pub macro naked_asm("assembly template", $(operands,)* $(options($(option),*))?) {
        /* compiler built-in */
    }
}
#[rustc_builtin_macro]
pub macro include_str($file:expr $(,)?) {
    /* compiler built-in */
}
pub mod prelude {
    pub mod rust_2015 {
        pub use crate::include_str;
    }
}
"#;

/// The asm macros of `tests/auxiliary/minicore.rs`, for a test with `//@ add-minicore`.
const MINICORE: &str = r#"//- /minicore.rs crate:minicore library
#[rustc_builtin_macro]
pub macro asm("assembly template", $(operands,)* $(options($(option),*))?) {
    /* compiler built-in */
}
#[rustc_builtin_macro]
pub macro global_asm("assembly template", $(operands,)* $(options($(option),*))?) {
    /* compiler built-in */
}
#[rustc_builtin_macro]
pub macro naked_asm("assembly template", $(operands,)* $(options($(option),*))?) {
    /* compiler built-in */
}
"#;

/// The crate a UI test takes the asm macros from.
#[derive(Clone, Copy)]
enum Dep {
    Std,
    Minicore,
}

/// What rustc writes compiling the UI test `source`, `$DIR/<name>`, in edition 2015 for
/// `target_arch`, with `dep` the crate it uses and `files` the fixture of the files it
/// includes.
fn ui_asm(name: &str, arch: &str, dep: Dep, source: &str, files: &str, passes: bool) -> String {
    let (krate, fixture) = match dep {
        Dep::Std => ("std", STD),
        Dep::Minicore => ("minicore", MINICORE),
    };
    let fixture = format!(
        "{fixture}//- /{name} crate:main edition:2015 deps:{krate} cfg:target_arch={arch}\n{source}{files}"
    );
    ui_fixture(&fixture, &compiletest(passes))
}

/// `stderr` as rustc writes it without `-Z deduplicate-diagnostics=no`, which compiletest gives
/// every UI test and blaze has no flag for: without the diagnostics noted as duplicates. The
/// summary's count of errors is left as it was.
fn deduplicated(stderr: &str) -> String {
    const DUPLICATE: &str = "= note: duplicate diagnostic emitted due to `-Z deduplicate-diagnostics=no`\n";
    stderr.split_inclusive("\n\n").filter(|diag| !diag.contains(DUPLICATE)).collect()
}

#[test]
fn named_labels_are_denied_outside_naked_functions() {
    let source = include_str!("asm/named-asm-labels.rs");
    let files = format!("//- /named-asm-labels.s\n{}", include_str!("asm/named-asm-labels.s"));
    let stderr = include_str!("asm/named-asm-labels.stderr");
    let expected = deduplicated(stderr).replace(
        "error: aborting due to 55 previous errors; 1 warning emitted",
        "error: aborting due to 49 previous errors; 1 warning emitted",
    );
    assert_eq!(ui_asm("named-asm-labels.rs", "x86_64", Dep::Std, source, &files, false), expected);
}

#[test]
fn syntax_directives_are_bad_style_on_x86() {
    let source = include_str!("asm/inline-syntax.rs");
    let expected = include_str!("asm/inline-syntax.x86_64.stderr");
    assert_eq!(ui_asm("inline-syntax.rs", "x86_64", Dep::Minicore, source, "", true), expected);
}

#[test]
fn binary_labels_are_denied_on_x86() {
    let source = include_str!("asm/binary_asm_labels.rs");
    let expected = include_str!("asm/binary_asm_labels.stderr");
    assert_eq!(ui_asm("binary_asm_labels.rs", "x86_64", Dep::Std, source, "", false), expected);
}

#[test]
fn binary_labels_are_allowed_off_x86() {
    let source = include_str!("asm/binary_asm_labels_allowed.rs");
    assert_eq!(ui_asm("binary_asm_labels_allowed.rs", "aarch64", Dep::Std, source, "", true), "");
}

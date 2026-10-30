//! The driver over whole crates: every error of every file given, whatever was edited, and
//! rustc's abort before its late passes.

use super::{checked, ui};

/// #1522: an error in a `#[cfg(test)]` module is an error of the crate built for tests.
#[test]
fn a_missing_field_in_a_test_module_is_an_error() {
    let checked = checked(
        r#"
//- /lib.rs crate:main cfg:test
pub struct S { pub a: u32, pub b: u32 }

#[cfg(test)]
mod tests {
    fn t() -> super::S {
        super::S { a: 1 }
    }
}
"#,
    );
    assert_eq!(checked.messages(), [
        "E0063: missing field `b` in initializer of `S`",
        "aborting due to 1 previous error",
        "For more information about this error, try `rustc --explain E0063`.",
    ]);
}

/// #780: the errors of a crate are those of all its files, not of the files an edit touched.
#[test]
fn an_error_in_a_module_file_is_the_crates() {
    let checked = checked(
        r#"
//- /main.rs crate:main
mod a;
fn main() {}
//- /a.rs
pub struct S { pub a: u32 }
pub fn f() -> S {
    S {}
}
"#,
    );
    assert_eq!(checked.diags[0].message, "missing field `a` in initializer of `S`");
    assert_eq!(checked.diags[0].span.primary[0].file, "a.rs");
}

/// rustc stops before its late passes once there is an error: no `dead_code` beside it.
#[test]
fn an_error_leaves_out_the_late_lints() {
    let checked = checked(
        r#"
//- /main.rs crate:main
struct S { a: u32 }
fn unused() {}
fn main() {
    let _ = S {};
}
"#,
    );
    assert_eq!(checked.messages(), [
        "E0063: missing field `a` in initializer of `S`",
        "aborting due to 1 previous error",
        "For more information about this error, try `rustc --explain E0063`.",
    ]);
}

#[test]
fn without_errors_the_late_lints_are_given() {
    let checked = checked(
        r#"
//- /main.rs crate:main
fn unused() {}
fn main() {}
"#,
    );
    assert_eq!(checked.messages(), [
        "dead_code: function `unused` is never used",
        "1 warning emitted",
    ]);
}

/// #1440: a call of a non-const function in a constant is refused, as rustc's
/// `tests/ui/consts/const-call.rs`.
#[test]
fn a_non_const_call_in_a_constant_is_refused() {
    let source = r#"fn f(x: usize) -> usize {
    x
}

fn main() {
    let _ = [0; f(2)];
    //~^ ERROR cannot call non-const function
}
"#;
    assert_eq!(ui("const-call.rs", "2015", source), r#"error[E0015]: cannot call non-const function `f` in constants
  --> $DIR/const-call.rs:6:17
   |
LL |     let _ = [0; f(2)];
   |                 ^^^^
   |
note: function `f` is not const
  --> $DIR/const-call.rs:1:1
   |
LL | fn f(x: usize) -> usize {
   | ^^^^^^^^^^^^^^^^^^^^^^^
   = note: calls in constants are limited to constant functions, tuple structs and tuple variants

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0015`.
"#);
}

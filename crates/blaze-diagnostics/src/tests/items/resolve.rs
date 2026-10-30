//! rustc's UI tests of names that do not resolve: imports (E0432), modules with no file (E0583),
//! and the style lints of the names items are given.

use crate::tests::ui;

#[test]
fn an_import_from_an_unknown_crate_suggests_the_extern_crate() {
    // tests/ui/error-codes/E0432.rs
    let source = "//@ edition:2015\nuse something::Foo; //~ ERROR E0432\n\nfn main () {\n}\n";
    let stderr = r#"error[E0432]: unresolved import `something`
  --> $DIR/E0432.rs:2:5
   |
LL | use something::Foo;
   |     ^^^^^^^^^ use of unresolved module or unlinked crate `something`
   |
help: you might be missing a crate named `something`, add it to your project and import it in your code
   |
LL + extern crate something;
   |

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0432`.
"#;
    assert_eq!(ui("E0432.rs", "2015", source), stderr);
}

#[test]
fn a_module_with_no_file_says_where_to_create_it() {
    // tests/ui/error-codes/E0583.rs
    let source = "mod module_that_doesnt_exist; //~ ERROR E0583\n\nfn main() {\n}\n";
    let stderr = r#"error[E0583]: file not found for module `module_that_doesnt_exist`
  --> $DIR/E0583.rs:1:1
   |
LL | mod module_that_doesnt_exist;
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: to create the module `module_that_doesnt_exist`, create file "$DIR/module_that_doesnt_exist.rs" or "$DIR/module_that_doesnt_exist/mod.rs"
   = note: if there is a `mod module_that_doesnt_exist` elsewhere in the crate already, import it with `use crate::...` instead

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0583`.
"#;
    assert_eq!(ui("E0583.rs", "2015", source), stderr);
}

#[test]
fn statics_should_have_upper_case_names() {
    // tests/ui/lint/lint-non-uppercase-statics.rs
    let source = r#"#![forbid(non_upper_case_globals)]
#![allow(dead_code)]

static foo: isize = 1; //~ ERROR static variable `foo` should have an upper case name

static mut bar: isize = 1; //~ ERROR static variable `bar` should have an upper case name

#[no_mangle]
pub static extern_foo: isize = 1; // OK, because #[no_mangle] supersedes the warning

fn main() { }
"#;
    let stderr = r#"error: static variable `foo` should have an upper case name
  --> $DIR/lint-non-uppercase-statics.rs:4:8
   |
LL | static foo: isize = 1;
   |        ^^^ help: convert the identifier to upper case (notice the capitalization): `FOO`
   |
note: the lint level is defined here
  --> $DIR/lint-non-uppercase-statics.rs:1:11
   |
LL | #![forbid(non_upper_case_globals)]
   |           ^^^^^^^^^^^^^^^^^^^^^^

error: static variable `bar` should have an upper case name
  --> $DIR/lint-non-uppercase-statics.rs:6:12
   |
LL | static mut bar: isize = 1;
   |            ^^^ help: convert the identifier to upper case: `BAR`

error: aborting due to 2 previous errors

"#;
    assert_eq!(ui("lint-non-uppercase-statics.rs", "2015", source), stderr);
}

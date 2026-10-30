//! rustc's UI tests of names that do not resolve: imports (E0432), modules with no file (E0583),
//! paths of bodies (E0425 in a module, E0433 of a prefix), and the style lints of the names
//! items are given.

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
fn under_cargo_the_missing_crate_help_says_cargo_add() {
    // tests/ui/error-codes/E0432.rs with `rustc-env:CARGO_CRATE_NAME=foo`, as blaze runs it.
    let source = "//@ edition:2015\nuse something::Foo; //~ ERROR E0432\n\nfn main () {\n}\n";
    let stderr = r#"error[E0432]: unresolved import `something`
  --> $DIR/E0432.rs:2:5
   |
LL | use something::Foo;
   |     ^^^^^^^^^ use of unresolved module or unlinked crate `something`
   |
help: if you wanted to use a crate named `something`, use `cargo add something` to add it to your `Cargo.toml` and import it in your code
   |
LL + extern crate something;
   |

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0432`.
"#;
    let mut options = crate::check::Options::default();
    options.lints.check_cfg.cargo = true;
    assert_eq!(crate::tests::ui_with("E0432.rs", "2015", source, &options), stderr);
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

#[test]
fn a_function_a_module_lacks_is_e0425_in_that_module() {
    // tests/ui/namespace/namespaced-enum-glob-import-no-impls.rs
    let source = r##"mod m2 {
    pub enum Foo {
        A,
        B(isize),
        C { a: isize },
    }

    impl Foo {
        pub fn foo() {}
        pub fn bar(&self) {}
    }
}

mod m {
    pub use crate::m2::Foo::*;
}

pub fn main() {
    use crate::m2::Foo::*;

    foo(); //~ ERROR cannot find function `foo` in this scope
    m::foo(); //~ ERROR cannot find function `foo` in module `m`
    bar(); //~ ERROR cannot find function `bar` in this scope
    m::bar(); //~ ERROR cannot find function `bar` in module `m`
}
"##;
    let stderr = r##"error[E0425]: cannot find function `foo` in module `m`
  --> $DIR/namespaced-enum-glob-import-no-impls.rs:22:8
   |
LL |     m::foo();
   |        ^^^ not found in `m`

error[E0425]: cannot find function `bar` in module `m`
  --> $DIR/namespaced-enum-glob-import-no-impls.rs:24:8
   |
LL |     m::bar();
   |        ^^^ not found in `m`

error[E0425]: cannot find function `foo` in this scope
  --> $DIR/namespaced-enum-glob-import-no-impls.rs:21:5
   |
LL |     foo();
   |     ^^^ not found in this scope

error[E0425]: cannot find function `bar` in this scope
  --> $DIR/namespaced-enum-glob-import-no-impls.rs:23:5
   |
LL |     bar();
   |     ^^^ not found in this scope

error: aborting due to 4 previous errors

For more information about this error, try `rustc --explain E0425`.
"##;
    assert_eq!(ui("namespaced-enum-glob-import-no-impls.rs", "2015", source), stderr);
}

#[test]
fn a_prefix_of_a_body_path_that_names_nothing_is_e0433() {
    // tests/ui/resolve/export-fully-qualified-2018.rs
    let source = r##"//@ edition:2018

// In this test baz isn't resolved when called as foo.baz even though
// it's called from inside foo. This is somewhat surprising and may
// want to change eventually.

mod foo {
    pub fn bar() { foo::baz(); } //~ ERROR: cannot find
    //~^ NOTE: use of unresolved module or unlinked crate `foo`

    fn baz() { }
}

fn main() { }
"##;
    let stderr = r##"error[E0433]: cannot find module or crate `foo` in this scope
  --> $DIR/export-fully-qualified-2018.rs:8:20
   |
LL |     pub fn bar() { foo::baz(); }
   |                    ^^^ use of unresolved module or unlinked crate `foo`
   |
   = help: you might be missing a crate named `foo`

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0433`.
"##;
    assert_eq!(ui("export-fully-qualified-2018.rs", "2018", source), stderr);
}

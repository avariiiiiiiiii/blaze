//! rustc's UI tests of macros: calls of macros that do not exist, `compile_error!`, and
//! `macro_rules!` definitions rustc refuses.

use crate::tests::ui;

#[test]
fn an_undefined_macro_suggests_the_similar_one() {
    // tests/ui/macros/macro_undefined.rs
    let source = r#"// Test macro_undefined issue

mod m {
    #[macro_export]
    macro_rules! kl {
        () => ()
    }
}

fn main() {
    k!(); //~ ERROR cannot find
    kl!();
}
"#;
    let stderr = r#"error: cannot find macro `k` in this scope
  --> $DIR/macro_undefined.rs:11:5
   |
LL |     macro_rules! kl {
   |     --------------- similarly named macro `kl` defined here
...
LL |     k!();
   |     ^
   |
help: a macro with a similar name exists
   |
LL |     kl!();
   |      +

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("macro_undefined.rs", "2015", source), stderr);
}

#[test]
fn a_macro_that_does_not_exist_is_not_found() {
    // tests/ui/resolve/nonexistent-macro.rs
    let source = r#"//! Test error handling for undefined macro calls

fn main() {
    iamnotanextensionthatexists!("");
    //~^ ERROR cannot find macro `iamnotanextensionthatexists` in this scope
}
"#;
    let stderr = r#"error: cannot find macro `iamnotanextensionthatexists` in this scope
  --> $DIR/nonexistent-macro.rs:4:5
   |
LL |     iamnotanextensionthatexists!("");
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("nonexistent-macro.rs", "2015", source), stderr);
}

#[test]
fn compile_error_is_an_error_at_the_call() {
    // tests/ui/macros/compile_error_macro.rs
    let source = r#"fn main() {
    compile_error!("a very descriptive error message"); //~ ERROR: a very descriptive error message
}
"#;
    let stderr = r#"error: a very descriptive error message
  --> $DIR/compile_error_macro.rs:2:5
   |
LL |     compile_error!("a very descriptive error message");
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("compile_error_macro.rs", "2015", source), stderr);
}

#[test]
fn a_matcher_must_be_delimited() {
    // tests/ui/macros/invalid-macro-matcher.rs
    let source = r#"#![allow(unused_macros)]

macro_rules! invalid {
    _ => (); //~ ERROR invalid macro matcher
}

fn main() {
}
"#;
    let stderr = r#"error: invalid macro matcher; matchers must be contained in balanced delimiters
  --> $DIR/invalid-macro-matcher.rs:4:5
   |
LL |     _ => ();
   |     ^

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("invalid-macro-matcher.rs", "2015", source), stderr);
}

#[test]
fn a_call_of_a_macro_rustc_refused_to_define_is_not_an_error_again() {
    // tests/ui/macros/malformed_macro_lhs.rs
    let source = r#"macro_rules! my_precioooous {
    t => (1); //~ ERROR invalid macro matcher
}

fn main() {
    my_precioooous!();
}
"#;
    let stderr = r#"error: invalid macro matcher; matchers must be contained in balanced delimiters
  --> $DIR/malformed_macro_lhs.rs:2:5
   |
LL |     t => (1);
   |     ^

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("malformed_macro_lhs.rs", "2015", source), stderr);
}

#[test]
fn a_rule_with_no_right_hand_side_ends_the_definition() {
    // tests/ui/parser/macro/bad-macro-definition.rs, its first macro
    let source = r#"#![crate_type = "lib"]

macro_rules! a { {} => }
//~^ ERROR: macro definition ended unexpectedly
"#;
    let stderr = r#"error: macro definition ended unexpectedly
  --> $DIR/bad-macro-definition.rs:3:23
   |
LL | macro_rules! a { {} => }
   |                       ^ expected right-hand side of macro rule

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("bad-macro-definition.rs", "2015", source), stderr);
}

#[test]
fn a_rule_with_no_arrow_expects_one() {
    // tests/ui/parser/macro/bad-macro-definition.rs, its macro `e` alone
    let source = r#"#![crate_type = "lib"]

macro_rules! e { {} }
//~^ ERROR: expected `=>`, found end of macro arguments
"#;
    let stderr = r#"error: expected `=>`, found end of macro arguments
  --> $DIR/bad-macro-definition.rs:3:20
   |
LL | macro_rules! e { {} }
   |                    ^ expected `=>`

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("bad-macro-definition.rs", "2015", source), stderr);
}

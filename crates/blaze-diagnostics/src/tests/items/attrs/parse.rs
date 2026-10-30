//! rustc's UI tests of the errors `parse_attribute_list` gives for every attribute, whatever
//! its parser: the arguments, `unsafe`, and the attributes that are not allowed where they are.

use crate::tests::ui;

#[test]
fn attribute_items_are_separated_with_commas() {
    // tests/ui/parser/attribute/attr-missing-comma.rs
    let source = r#"fn main() {}

#[deprecated(
    since = "since" //~ ERROR attribute items not separated with `,`
    note = "note"
)]
fn f0() {}

#[link(
    name = "name" //~ ERROR attribute items not separated with `,`
    kind = "static"
    modifiers = "modifiers"
)]
fn f1() {}
"#;
    let stderr = r#"error: attribute items not separated with `,`
  --> $DIR/attr-missing-comma.rs:4:20
   |
LL |     since = "since"
   |                    ^ help: try adding `,` here

error: attribute items not separated with `,`
  --> $DIR/attr-missing-comma.rs:10:18
   |
LL |     name = "name"
   |                  ^
   |
help: try adding `,` here
   |
LL |     name = "name",
   |                  +
help: try adding `,` here
   |
LL |     kind = "static",
   |                    +

error: aborting due to 2 previous errors

"#;
    assert_eq!(ui("attr-missing-comma.rs", "2015", source), stderr);
}

#[test]
fn suffixed_literals_are_not_arguments() {
    // tests/ui/attributes/suffixed-literal-meta.rs
    let source = r#"#![feature(rustc_attrs)]

#[rustc_dummy = 1usize] //~ ERROR: suffixed literals are not allowed in attributes
#[rustc_dummy = 1u8] //~ ERROR: suffixed literals are not allowed in attributes
#[rustc_dummy = 1u16] //~ ERROR: suffixed literals are not allowed in attributes
#[rustc_dummy = 1u32] //~ ERROR: suffixed literals are not allowed in attributes
#[rustc_dummy = 1u64] //~ ERROR: suffixed literals are not allowed in attributes
#[rustc_dummy = 1isize] //~ ERROR: suffixed literals are not allowed in attributes
#[rustc_dummy = 1i8] //~ ERROR: suffixed literals are not allowed in attributes
#[rustc_dummy = 1i16] //~ ERROR: suffixed literals are not allowed in attributes
#[rustc_dummy = 1i32] //~ ERROR: suffixed literals are not allowed in attributes
#[rustc_dummy = 1i64] //~ ERROR: suffixed literals are not allowed in attributes
#[rustc_dummy = 1.0f32] //~ ERROR: suffixed literals are not allowed in attributes
#[rustc_dummy = 1.0f64] //~ ERROR: suffixed literals are not allowed in attributes
fn main() {}
"#;
    let stderr = r#"error: suffixed literals are not allowed in attributes
  --> $DIR/suffixed-literal-meta.rs:3:17
   |
LL | #[rustc_dummy = 1usize]
   |                 ^^^^^^
   |
   = help: instead of using a suffixed literal (`1u8`, `1.0f32`, etc.), use an unsuffixed version (`1`, `1.0`, etc.)

error: suffixed literals are not allowed in attributes
  --> $DIR/suffixed-literal-meta.rs:4:17
   |
LL | #[rustc_dummy = 1u8]
   |                 ^^^
   |
   = help: instead of using a suffixed literal (`1u8`, `1.0f32`, etc.), use an unsuffixed version (`1`, `1.0`, etc.)

error: suffixed literals are not allowed in attributes
  --> $DIR/suffixed-literal-meta.rs:5:17
   |
LL | #[rustc_dummy = 1u16]
   |                 ^^^^
   |
   = help: instead of using a suffixed literal (`1u8`, `1.0f32`, etc.), use an unsuffixed version (`1`, `1.0`, etc.)

error: suffixed literals are not allowed in attributes
  --> $DIR/suffixed-literal-meta.rs:6:17
   |
LL | #[rustc_dummy = 1u32]
   |                 ^^^^
   |
   = help: instead of using a suffixed literal (`1u8`, `1.0f32`, etc.), use an unsuffixed version (`1`, `1.0`, etc.)

error: suffixed literals are not allowed in attributes
  --> $DIR/suffixed-literal-meta.rs:7:17
   |
LL | #[rustc_dummy = 1u64]
   |                 ^^^^
   |
   = help: instead of using a suffixed literal (`1u8`, `1.0f32`, etc.), use an unsuffixed version (`1`, `1.0`, etc.)

error: suffixed literals are not allowed in attributes
  --> $DIR/suffixed-literal-meta.rs:8:17
   |
LL | #[rustc_dummy = 1isize]
   |                 ^^^^^^
   |
   = help: instead of using a suffixed literal (`1u8`, `1.0f32`, etc.), use an unsuffixed version (`1`, `1.0`, etc.)

error: suffixed literals are not allowed in attributes
  --> $DIR/suffixed-literal-meta.rs:9:17
   |
LL | #[rustc_dummy = 1i8]
   |                 ^^^
   |
   = help: instead of using a suffixed literal (`1u8`, `1.0f32`, etc.), use an unsuffixed version (`1`, `1.0`, etc.)

error: suffixed literals are not allowed in attributes
  --> $DIR/suffixed-literal-meta.rs:10:17
   |
LL | #[rustc_dummy = 1i16]
   |                 ^^^^
   |
   = help: instead of using a suffixed literal (`1u8`, `1.0f32`, etc.), use an unsuffixed version (`1`, `1.0`, etc.)

error: suffixed literals are not allowed in attributes
  --> $DIR/suffixed-literal-meta.rs:11:17
   |
LL | #[rustc_dummy = 1i32]
   |                 ^^^^
   |
   = help: instead of using a suffixed literal (`1u8`, `1.0f32`, etc.), use an unsuffixed version (`1`, `1.0`, etc.)

error: suffixed literals are not allowed in attributes
  --> $DIR/suffixed-literal-meta.rs:12:17
   |
LL | #[rustc_dummy = 1i64]
   |                 ^^^^
   |
   = help: instead of using a suffixed literal (`1u8`, `1.0f32`, etc.), use an unsuffixed version (`1`, `1.0`, etc.)

error: suffixed literals are not allowed in attributes
  --> $DIR/suffixed-literal-meta.rs:13:17
   |
LL | #[rustc_dummy = 1.0f32]
   |                 ^^^^^^
   |
   = help: instead of using a suffixed literal (`1u8`, `1.0f32`, etc.), use an unsuffixed version (`1`, `1.0`, etc.)

error: suffixed literals are not allowed in attributes
  --> $DIR/suffixed-literal-meta.rs:14:17
   |
LL | #[rustc_dummy = 1.0f64]
   |                 ^^^^^^
   |
   = help: instead of using a suffixed literal (`1u8`, `1.0f32`, etc.), use an unsuffixed version (`1`, `1.0`, etc.)

error: aborting due to 12 previous errors

"#;
    assert_eq!(ui("suffixed-literal-meta.rs", "2015", source), stderr);
}

#[test]
fn a_safe_attribute_is_not_unsafe() {
    // tests/ui/attributes/unsafe/unsafe-safe-attribute.rs
    let source = r#"#[unsafe(repr(C))] //~ ERROR: is not an unsafe attribute
struct Foo {}

fn main() {}
"#;
    let stderr = r#"error: `repr` is not an unsafe attribute
  --> $DIR/unsafe-safe-attribute.rs:1:3
   |
LL | #[unsafe(repr(C))]
   |   ^^^^^^ this is not an unsafe attribute
   |
   = note: extraneous unsafe is not allowed in attributes

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("unsafe-safe-attribute.rs", "2015", source), stderr);
}

#[test]
fn an_unsafe_attribute_needs_unsafe_in_2024() {
    // tests/ui/rust-2024/unsafe-attributes/unsafe-attributes.rs
    let source = r#"//@ revisions: edition2021 edition2024
//@[edition2021] edition:2021
//@[edition2021] check-pass
//@[edition2024] edition:2024


#[no_mangle] //[edition2024]~ ERROR: unsafe attribute used without unsafe
extern "C" fn foo() {}

fn main() {}
"#;
    let stderr = r#"error: unsafe attribute used without unsafe
  --> $DIR/unsafe-attributes.rs:7:3
   |
LL | #[no_mangle]
   |   ^^^^^^^^^ usage of unsafe attribute
   |
help: wrap the attribute in `unsafe(...)`
   |
LL | #[unsafe(no_mangle)]
   |   +++++++         +

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("unsafe-attributes.rs", "2024", source), stderr);
}

#[test]
fn where_predicates_take_only_cfg() {
    // tests/ui/attributes/where-doc.rs
    let source = r#"#![feature(where_clause_attrs)]
#![allow(invalid_doc_attributes)]

fn test()
where
#[doc(alias = ":(")]
//~^ ERROR most attributes are not supported in `where` clauses
//~| ERROR `#[doc(alias = "...")]` isn't allowed on where predicate
():,

#[doc(hidden)]
//~^ ERROR most attributes are not supported in `where` clauses
():,

#[doc = ""]
//~^ ERROR most attributes are not supported in `where` clauses
():,

// == That the doc attributes below don't trigger the error is a bug
#[doc()]
():,

#[doc(5)]
():,

#[doc]
():,

#[doc = 5]
():,

{ }

fn main() {}
"#;
    let stderr = r#"error: most attributes are not supported in `where` clauses
  --> $DIR/where-doc.rs:6:1
   |
LL | #[doc(alias = ":(")]
   | ^^^^^^^^^^^^^^^^^^^^
   |
   = help: only `#[cfg]` and `#[cfg_attr]` are supported

error: most attributes are not supported in `where` clauses
  --> $DIR/where-doc.rs:11:1
   |
LL | #[doc(hidden)]
   | ^^^^^^^^^^^^^^
   |
   = help: only `#[cfg]` and `#[cfg_attr]` are supported

error: most attributes are not supported in `where` clauses
  --> $DIR/where-doc.rs:15:1
   |
LL | #[doc = ""]
   | ^^^^^^^^^^^
   |
   = help: only `#[cfg]` and `#[cfg_attr]` are supported

error: `#[doc(alias = "...")]` isn't allowed on where predicate
  --> $DIR/where-doc.rs:6:15
   |
LL | #[doc(alias = ":(")]
   |               ^^^^

error: aborting due to 4 previous errors

"#;
    assert_eq!(ui("where-doc.rs", "2015", source), stderr);
}

#[test]
fn test_is_not_a_crate_level_attribute() {
    // tests/ui/feature-gates/issue-43106-gating-of-test.rs
    let source = r#"// The non-crate level cases are in issue-43106-gating-of-builtin-attrs.rs.

#![test                    = "4200"]
//~^ ERROR `test` attribute cannot be used at crate level
fn main() {}
"#;
    let stderr = r#"error: `test` attribute cannot be used at crate level
  --> $DIR/issue-43106-gating-of-test.rs:3:1
   |
LL | #![test                    = "4200"]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
LL |
LL | fn main() {}
   | ------------ the inner attribute doesn't annotate this item
   |
help: perhaps you meant to use an outer attribute
   |
LL - #![test                    = "4200"]
LL + #[test                    = "4200"]
   |

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("issue-43106-gating-of-test.rs", "2015", source), stderr);
}

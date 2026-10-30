//! rustc's UI tests of the names in signatures: types and traits that name nothing (E0425,
//! E0405) or something else (E0573, E0404), private items (E0603), parameters of an outer item
//! (E0401) and undeclared lifetimes (E0261).

use crate::tests::{items::ui_core, ui};

#[test]
fn an_impl_of_a_struct_is_not_a_trait() {
    // tests/ui/error-codes/E0404.rs
    let source = "struct Foo;\nstruct Bar;\n\nimpl Foo for Bar {} //~ ERROR E0404\n\nfn main() {}\n\nfn baz<T: Foo>(_: T) {} //~ ERROR E0404\n";
    let stderr = r#"error[E0404]: expected trait, found struct `Foo`
  --> $DIR/E0404.rs:4:6
   |
LL | impl Foo for Bar {}
   |      ^^^ not a trait

error[E0404]: expected trait, found struct `Foo`
  --> $DIR/E0404.rs:8:11
   |
LL | fn baz<T: Foo>(_: T) {}
   |           ^^^ not a trait

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0404`.
"#;
    assert_eq!(ui("E0404.rs", "2015", source), stderr);
}

#[test]
fn an_impl_of_an_unknown_trait() {
    // tests/ui/error-codes/E0405.rs
    let source = "struct Foo;\n\nimpl SomeTrait for Foo {} //~ ERROR E0405\n\nfn main() {\n}\n";
    let stderr = r#"error[E0405]: cannot find trait `SomeTrait` in this scope
  --> $DIR/E0405.rs:3:6
   |
LL | impl SomeTrait for Foo {}
   |      ^^^^^^^^^ not found in this scope

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0405`.
"#;
    assert_eq!(ui("E0405.rs", "2015", source), stderr);
}

#[test]
fn an_impl_of_an_unknown_type() {
    // tests/ui/error-codes/E0412.rs
    let source = "impl Something {} //~ ERROR E0425\n\nfn main() {\n}\n";
    let stderr = r#"error[E0425]: cannot find type `Something` in this scope
  --> $DIR/E0412.rs:1:6
   |
LL | impl Something {}
   |      ^^^^^^^^^ not found in this scope

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0425`.
"#;
    assert_eq!(ui("E0412.rs", "2015", source), stderr);
}

#[test]
fn a_variant_is_not_a_type_but_its_enum_is() {
    // tests/ui/enum/enum-variant-type-2.rs
    let source = "//@ edition:2015\n// Test that enum variants are not actually types.\n\nenum Foo {\n    Bar\n}\n\nfn foo(x: Foo::Bar) {} //~ ERROR expected type, found variant `Foo::Bar`\n\nfn main() {}\n";
    let stderr = r#"error[E0573]: expected type, found variant `Foo::Bar`
  --> $DIR/enum-variant-type-2.rs:8:11
   |
LL | fn foo(x: Foo::Bar) {}
   |           ^^^^^^^^ not a type
   |
help: try using the variant's enum
   |
LL - fn foo(x: Foo::Bar) {}
LL + fn foo(x: Foo) {}
   |

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0573`.
"#;
    assert_eq!(ui("enum-variant-type-2.rs", "2015", source), stderr);
}

#[test]
fn a_module_is_not_a_type_but_a_trait_of_a_similar_name_is() {
    // tests/ui/traits/impl-for-module.rs
    let source = "mod a {\n}\n\ntrait A {\n}\n\nimpl A for a { //~ ERROR expected type, found module\n}\n\nfn main() {\n}\n";
    let stderr = r#"error[E0573]: expected type, found module `a`
  --> $DIR/impl-for-module.rs:7:12
   |
LL | trait A {
   | ------- similarly named trait `A` defined here
...
LL | impl A for a {
   |            ^
   |
help: a trait with a similar name exists
   |
LL - impl A for a {
LL + impl A for A {
   |

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0573`.
"#;
    assert_eq!(ui("impl-for-module.rs", "2015", source), stderr);
}

#[test]
fn a_const_parameter_is_not_a_type() {
    // tests/ui/const-generics/struct-with-invalid-const-param.rs
    let source = "// Checks that a const param cannot be stored in a struct.\n\nstruct S<const C: u8>(C); //~ ERROR cannot find type `C` in this scope\n\nfn main() {}\n";
    let stderr = r#"error[E0573]: cannot find type `C` in this scope
  --> $DIR/struct-with-invalid-const-param.rs:3:23
   |
LL | struct S<const C: u8>(C);
   |                       ^ not found in this scope
   |
   = note: a const parameter named `C` exists in another namespace

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0573`.
"#;
    assert_eq!(ui("struct-with-invalid-const-param.rs", "2015", source), stderr);
}

#[test]
fn a_private_struct_of_a_module_in_a_signature() {
    // tests/ui/privacy/struct-field-and-impl-expose-10545.rs
    let source = "//! Regression test for https://github.com/rust-lang/rust/issues/10545\n\nmod a {\n    struct S;\n    impl S { }\n}\n\nfn foo(_: a::S) { //~ ERROR: struct `S` is private\n}\n\nfn main() {}\n";
    let stderr = r#"error[E0603]: struct `S` is private
  --> $DIR/struct-field-and-impl-expose-10545.rs:8:14
   |
LL | fn foo(_: a::S) {
   |              ^ private struct
   |
note: the struct `S` is defined here
  --> $DIR/struct-field-and-impl-expose-10545.rs:4:5
   |
LL |     struct S;
   |     ^^^^^^^^^

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0603`.
"#;
    assert_eq!(ui("struct-field-and-impl-expose-10545.rs", "2015", source), stderr);
}

#[test]
fn an_inner_function_cannot_use_a_parameter_of_its_outer_one() {
    // tests/ui/resolve/bad-type-env-capture.rs
    let source = "fn foo<T>() {\n    fn bar(b: T) { } //~ ERROR can't use generic parameters from outer\n}\nfn main() { }\n";
    let stderr = r#"error[E0401]: can't use generic parameters from outer item
  --> $DIR/bad-type-env-capture.rs:2:15
   |
LL | fn foo<T>() {
   |        - type parameter from outer item
LL |     fn bar(b: T) { }
   |        ---    ^ use of generic parameter from outer item
   |        |
   |        generic parameter used in this inner function
   |
   = note: nested items are independent from their parent item for everything except for privacy and name resolution
help: try introducing a local generic parameter here
   |
LL |     fn bar<T>(b: T) { }
   |           +++

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0401`.
"#;
    assert_eq!(ui("bad-type-env-capture.rs", "2015", source), stderr);
}

#[test]
fn an_inner_enum_cannot_use_a_parameter_of_its_outer_function() {
    // tests/ui/generics/enum-definition-with-outer-generic-parameter-5997.rs
    let source = r#"// https://github.com/rust-lang/rust/issues/5997
fn f<Z>() -> bool {
    enum E { V(Z) }
    //~^ ERROR can't use generic parameters from outer item
    true
}

fn main() {
    let b = f::<isize>();
    assert!(b);
}
"#;
    let stderr = r#"error[E0401]: can't use generic parameters from outer item
  --> $DIR/enum-definition-with-outer-generic-parameter-5997.rs:3:16
   |
LL | fn f<Z>() -> bool {
   |      - type parameter from outer item
LL |     enum E { V(Z) }
   |          -     ^ use of generic parameter from outer item
   |          |
   |          generic parameter used in this inner enum
   |
   = note: nested items are independent from their parent item for everything except for privacy and name resolution
help: try introducing a local generic parameter here
   |
LL |     enum E<Z> { V(Z) }
   |           +++

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0401`.
"#;
    assert_eq!(ui_core("enum-definition-with-outer-generic-parameter-5997.rs", "2015", "assert", source), stderr);
}

#[test]
fn a_struct_in_a_closure_of_an_impl_cannot_use_the_impls_parameter() {
    // tests/ui/generics/issue-98432.rs
    let source = r#"struct Struct<T>(T);

impl<T> Struct<T> {
    const CONST: fn() = || {
        struct _Obligation where T:; //~ ERROR can't use generic parameters from outer item
    };
}

fn main() {}
"#;
    let stderr = r#"error[E0401]: can't use generic parameters from outer item
  --> $DIR/issue-98432.rs:5:34
   |
LL | impl<T> Struct<T> {
   |      - type parameter from outer item
LL |     const CONST: fn() = || {
LL |         struct _Obligation where T:;
   |                -----------       ^ use of generic parameter from outer item
   |                |
   |                generic parameter used in this inner struct
   |
   = note: nested items are independent from their parent item for everything except for privacy and name resolution
help: try introducing a local generic parameter here
   |
LL |         struct _Obligation<T> where T:;
   |                           +++

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0401`.
"#;
    assert_eq!(ui("issue-98432.rs", "2015", source), stderr);
}

#[test]
fn an_undeclared_lifetime_suggests_introducing_it_on_the_item() {
    // tests/ui/error-codes/E0261.rs
    let source = r#"fn foo(x: &'a str) { } //~ ERROR E0261
                       //~| NOTE undeclared lifetime

struct Foo {
    x: &'a str, //~ ERROR E0261
                //~| NOTE undeclared lifetime
}

fn main() {}
"#;
    let stderr = r#"error[E0261]: use of undeclared lifetime name `'a`
  --> $DIR/E0261.rs:1:12
   |
LL | fn foo(x: &'a str) { }
   |            ^^ undeclared lifetime
   |
help: consider introducing lifetime `'a` here
   |
LL | fn foo<'a>(x: &'a str) { }
   |       ++++

error[E0261]: use of undeclared lifetime name `'a`
  --> $DIR/E0261.rs:5:9
   |
LL |     x: &'a str,
   |         ^^ undeclared lifetime
   |
help: consider introducing lifetime `'a` here
   |
LL | struct Foo<'a> {
   |           ++++

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0261`.
"#;
    assert_eq!(ui("E0261.rs", "2015", source), stderr);
}

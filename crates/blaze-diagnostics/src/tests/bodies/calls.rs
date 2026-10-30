//! rustc's UI tests of the errors type checking gives of calls, operators, casts and
//! assignments in bodies: E0061, E0057 and E0308's "arguments to this function are incorrect"
//! (a call with the wrong arguments), E0618 (a call of what is not a function), E0606, E0604,
//! E0054 and E0620 (casts), E0369 and E0600 (operators), E0614 (a dereference), E0608 (an
//! index), E0070 and E0067 (an assignment), E0040 (`Drop::drop`) and a method with a `Sized`
//! bound called on a trait object. Each test's file is the UI test's source byte for byte.

mod args;

use crate::{
    Options,
    emit::Style,
    tests::{Mode, checked_with, compiletest, normalized, ui},
};

/// [`ui`] of a file with minicore's items for `flags` (`crates/test-utils/src/minicore.rs`),
/// under compiletest's flags.
fn ui_core(name: &str, flags: &str, source: &str) -> String {
    let fixture = format!("//- minicore: {flags}\n//- /{name} crate:main edition:2015\n{source}");
    ui_fixture(&fixture)
}

/// [`ui_core`] of a file that uses std: a crate `std` of `std_items` over minicore, with its
/// prelude.
fn ui_std(name: &str, flags: &str, std_items: &str, source: &str) -> String {
    let fixture = format!(
        "//- minicore: {flags}\n//- /std.rs crate:std deps:core\n\
         pub mod prelude {{ pub mod rust_2015 {{ pub use core::prelude::rust_2015::*; }} }}\n\
         {std_items}\n//- /{name} crate:main edition:2015 deps:std\n{source}"
    );
    ui_fixture(&fixture)
}

fn ui_fixture(fixture: &str) -> String {
    let checked = checked_with(fixture, "$DIR/", &compiletest(Mode::Other, &Options::default()));
    normalized(&checked.stderr_with(Style { ui_testing: true, ..Style::default() }))
}

#[test]
fn a_call_of_a_variant_or_a_local_that_is_not_a_function() {
    // tests/ui/error-codes/E0618.rs
    let source = "enum X {
    Entry,
}

fn main() {
    X::Entry();
    //~^ ERROR expected function, found enum variant `X::Entry` [E0618]
    let x = 0i32;
    x();
    //~^ ERROR expected function, found `i32` [E0618]
}
";
    let stderr = "error[E0618]: expected function, found enum variant `X::Entry`
  --> $DIR/E0618.rs:6:5
   |
LL |     Entry,
   |     ----- enum variant `X::Entry` defined here
...
LL |     X::Entry();
   |     ^^^^^^^^--
   |     |
   |     call expression requires function
   |
help: `X::Entry` is a unit enum variant, and does not take parentheses to be constructed
   |
LL -     X::Entry();
LL +     X::Entry;
   |

error[E0618]: expected function, found `i32`
  --> $DIR/E0618.rs:9:5
   |
LL |     let x = 0i32;
   |         - `x` has type `i32`
LL |     x();
   |     ^--
   |     |
   |     call expression requires function

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0618`.
";
    assert_eq!(ui_core("E0618.rs", "fn", source), stderr);
}

#[test]
fn a_cast_of_a_reference_borrows_too_much_or_is_not_dereferenced() {
    // tests/ui/error-codes/E0606.rs
    let source = "fn main() {
    let x = &(&0u8 as u8); //~ ERROR E0606
    x as u8; //~ ERROR casting `&u8` as `u8` is invalid [E0606]
}
";
    let stderr = "error[E0606]: casting `&u8` as `u8` is invalid
  --> $DIR/E0606.rs:2:14
   |
LL |     let x = &(&0u8 as u8);
   |              ^^^^^^^^^^^^
   |
help: remove the unneeded borrow
   |
LL -     let x = &(&0u8 as u8);
LL +     let x = &(0u8 as u8);
   |

error[E0606]: casting `&u8` as `u8` is invalid
  --> $DIR/E0606.rs:3:5
   |
LL |     x as u8;
   |     ^^^^^^^
   |
help: dereference the expression
   |
LL |     *x as u8;
   |     +

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0606`.
";
    assert_eq!(ui_core("E0606.rs", "sized", source), stderr);
}

#[test]
fn only_u8_is_cast_as_char() {
    // tests/ui/error-codes/E0604.rs
    let source = "fn main() {
    1u32 as char; //~ ERROR E0604
}
";
    let stderr = "error[E0604]: only `u8` can be cast as `char`, not `u32`
  --> $DIR/E0604.rs:2:5
   |
LL |     1u32 as char;
   |     ^^^^^^^^^^^^ invalid cast
   |
help: consider using `char::from_u32` instead
   |
LL -     1u32 as char;
LL +     char::from_u32(1u32);
   |

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0604`.
";
    assert_eq!(ui_core("E0604.rs", "sized", source), stderr);
}

#[test]
fn an_integer_is_compared_with_zero_not_cast_as_bool() {
    // tests/ui/error-codes/E0054.rs
    let source = "fn main() {
    let x = 5;
    let x_is_nonzero = x as bool; //~ ERROR E0054
}
";
    let stderr = "error[E0054]: cannot cast `i32` as `bool`
  --> $DIR/E0054.rs:3:24
   |
LL |     let x_is_nonzero = x as bool;
   |                        ^^^^^^^^^
   |
help: compare with zero instead
   |
LL -     let x_is_nonzero = x as bool;
LL +     let x_is_nonzero = x != 0;
   |

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0054`.
";
    assert_eq!(ui_core("E0054.rs", "sized", source), stderr);
}

#[test]
fn a_cast_to_an_unsized_type_is_to_a_reference() {
    // tests/ui/error-codes/E0620.rs
    let source = "fn main() {
    let _foo = &[1_usize, 2] as [usize]; //~ ERROR E0620
}
";
    let stderr = "error[E0620]: cast to unsized type: `&[usize; 2]` as `[usize]`
  --> $DIR/E0620.rs:2:16
   |
LL |     let _foo = &[1_usize, 2] as [usize];
   |                ^^^^^^^^^^^^^^^^^^^^^^^^
   |
help: consider casting to a reference instead
   |
LL |     let _foo = &[1_usize, 2] as &[usize];
   |                                 +

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0620`.
";
    assert_eq!(ui_core("E0620.rs", "sized, slice", source), stderr);
}

#[test]
fn a_binary_operator_of_types_without_its_impl() {
    // tests/ui/binop/binop-typeck.rs
    let source = "// issue #500

fn main() {
    let x = true;
    let y = 1;
    let z = x + y;
    //~^ ERROR cannot add `{integer}` to `bool`
}
";
    let stderr = "error[E0369]: cannot add `{integer}` to `bool`
  --> $DIR/binop-typeck.rs:6:15
   |
LL |     let z = x + y;
   |             - ^ - {integer}
   |             |
   |             bool

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0369`.
";
    assert_eq!(ui_core("binop-typeck.rs", "add", source), stderr);
}

#[test]
fn a_unary_operator_of_a_type_without_its_impl() {
    // tests/ui/error-codes/E0600.rs
    let source = "fn main() {
    !\"a\"; //~ ERROR E0600
}
";
    let stderr = "error[E0600]: cannot apply unary operator `!` to type `&'static str`
  --> $DIR/E0600.rs:2:5
   |
LL |     !\"a\";
   |     ^^^^ cannot apply unary operator `!`

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0600`.
";
    assert_eq!(ui_core("E0600.rs", "unary_ops, str", source), stderr);
}

#[test]
fn a_dereference_of_an_integer() {
    // tests/ui/error-codes/E0614.rs
    let source = "fn main() {
    let y = 0u32;
    *y; //~ ERROR E0614
}
";
    let stderr = "error[E0614]: type `u32` cannot be dereferenced
  --> $DIR/E0614.rs:3:5
   |
LL |     *y;
   |     ^^ can't be dereferenced

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0614`.
";
    assert_eq!(ui_core("E0614.rs", "deref", source), stderr);
}

#[test]
fn an_index_into_an_integer() {
    // tests/ui/error-codes/E0608.rs
    let source = "fn main() {
    0u8[2]; //~ ERROR E0608
}
";
    let stderr = "error[E0608]: cannot index into a value of type `u8`
  --> $DIR/E0608.rs:2:8
   |
LL |     0u8[2];
   |        ^^^

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0608`.
";
    assert_eq!(ui_core("E0608.rs", "index", source), stderr);
}

#[test]
fn an_assignment_to_a_constant_a_literal_and_a_call() {
    // tests/ui/error-codes/E0070.rs
    let source = "const SOME_CONST : i32 = 12;

fn some_other_func() {}

fn some_function() {
    SOME_CONST = 14; //~ ERROR E0070
    1 = 3; //~ ERROR E0070
    some_other_func() = 4; //~ ERROR E0070
}

fn main() {
}
";
    let stderr = "error[E0070]: invalid left-hand side of assignment
  --> $DIR/E0070.rs:6:16
   |
LL |     SOME_CONST = 14;
   |     ---------- ^
   |     |
   |     cannot assign to this expression

error[E0070]: invalid left-hand side of assignment
  --> $DIR/E0070.rs:7:7
   |
LL |     1 = 3;
   |     - ^
   |     |
   |     cannot assign to this expression

error[E0070]: invalid left-hand side of assignment
  --> $DIR/E0070.rs:8:23
   |
LL |     some_other_func() = 4;
   |     ----------------- ^
   |     |
   |     cannot assign to this expression

error: aborting due to 3 previous errors

For more information about this error, try `rustc --explain E0070`.
";
    assert_eq!(ui("E0070.rs", "2015", source), stderr);
}

#[test]
fn a_compound_assignment_to_a_borrow_is_dereferenced() {
    // tests/ui/typeck/assign-non-lval-needs-deref.rs
    let source = "// issue #101376

use std::ops::AddAssign;
struct Foo;

impl AddAssign<()> for Foo {
    fn add_assign(&mut self, _: ()) {}
}

impl AddAssign<()> for &mut Foo {
    fn add_assign(&mut self, _: ()) {}
}

fn main() {
    (&mut Foo) += ();
    //~^ ERROR invalid left-hand side of assignment
    //~| NOTE cannot assign to this expression
    //~| HELP consider dereferencing the left-hand side of this operation
}
";
    let stderr = "error[E0067]: invalid left-hand side of assignment
  --> $DIR/assign-non-lval-needs-deref.rs:15:16
   |
LL |     (&mut Foo) += ();
   |     ---------- ^^
   |     |
   |     cannot assign to this expression
   |
help: consider dereferencing the left-hand side of this operation
   |
LL |     *(&mut Foo) += ();
   |     +

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0067`.
";
    let std_items = "pub use core::ops;";
    assert_eq!(ui_std("assign-non-lval-needs-deref.rs", "add", std_items, source), stderr);
}

#[test]
fn drop_called_as_a_path_is_the_drop_function() {
    // tests/ui/drop/explicit-drop-call-error.rs
    let source = "//! Test error for explicit destructor method calls via UFCS

//@ run-rustfix

#![allow(dropping_references)]

struct Foo;

impl Drop for Foo {
    fn drop(&mut self) {}
}

fn main() {
    Drop::drop(&mut Foo) //~ ERROR explicit use of destructor method
}
";
    let stderr = "error[E0040]: explicit use of destructor method
  --> $DIR/explicit-drop-call-error.rs:14:5
   |
LL |     Drop::drop(&mut Foo)
   |     ^^^^^^^^^^ explicit destructor calls not allowed
   |
help: consider using `drop` function
   |
LL -     Drop::drop(&mut Foo)
LL +     drop(Foo)
   |

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0040`.
";
    assert_eq!(ui_core("explicit-drop-call-error.rs", "drop", source), stderr);
}

#[test]
fn a_method_with_a_sized_bound_on_a_trait_object() {
    // tests/ui/illegal-sized-bound/regular.rs
    let source = "struct MutType;

pub trait MutTrait {
    fn function(&mut self)
    where
        Self: Sized;
    //~^ NOTE this has a `Sized` requirement
}

impl MutTrait for MutType {
    fn function(&mut self) {}
}

struct Type;

pub trait Trait {
    fn function(&self)
    where
        Self: Sized;
    //~^ NOTE this has a `Sized` requirement
}

impl Trait for Type {
    fn function(&self) {}
}

fn main() {
    (&mut MutType as &mut dyn MutTrait).function();
    //~^ ERROR the `function` method cannot be invoked on a trait object
    (&Type as &dyn Trait).function();
    //~^ ERROR the `function` method cannot be invoked on a trait object
}
";
    let stderr = "error: the `function` method cannot be invoked on a trait object
  --> $DIR/regular.rs:28:41
   |
LL |         Self: Sized;
   |               ----- this has a `Sized` requirement
...
LL |     (&mut MutType as &mut dyn MutTrait).function();
   |                                         ^^^^^^^^

error: the `function` method cannot be invoked on a trait object
  --> $DIR/regular.rs:30:27
   |
LL |         Self: Sized;
   |               ----- this has a `Sized` requirement
...
LL |     (&Type as &dyn Trait).function();
   |                           ^^^^^^^^

error: aborting due to 2 previous errors

";
    assert_eq!(ui_core("regular.rs", "sized, unsize, coerce_unsized", source), stderr);
}

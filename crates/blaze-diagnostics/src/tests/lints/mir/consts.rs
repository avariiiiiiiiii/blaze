//! rustc's const checking (`tests/ui/consts/`, `tests/ui/error-codes/E0015.rs`), and blaze's
//! own `blaze::const_impure`, given only in a crate that uses cached functions.

use crate::tests::{checked, ui};

#[test]
fn a_call_of_a_function_that_is_not_const_in_a_constant() {
    // tests/ui/error-codes/E0015.rs, with an `Option` of its own after `main` for core's:
    // the lines and columns are the test's.
    let source = r#"fn create_some() -> Option<u8> {
    Some(1)
}

const FOO: Option<u8> = create_some();
//~^ ERROR cannot call non-const function `create_some` in constants [E0015]

fn main() {}
enum Option<T> { Some(T), None }
use Option::*;
"#;
    let stderr = r#"error[E0015]: cannot call non-const function `create_some` in constants
  --> $DIR/E0015.rs:5:25
   |
LL | const FOO: Option<u8> = create_some();
   |                         ^^^^^^^^^^^^^
   |
note: function `create_some` is not const
  --> $DIR/E0015.rs:1:1
   |
LL | fn create_some() -> Option<u8> {
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   = note: calls in constants are limited to constant functions, tuple structs and tuple variants

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0015`.
"#;
    assert_eq!(ui("E0015.rs", "2015", source), stderr);
}

#[test]
fn a_call_of_a_function_that_is_not_const_in_a_const_fn() {
    // tests/ui/consts/const-fn-not-safe-for-const.rs, its `use std::mem::transmute;` made a
    // comment and `sub`'s body one that needs no std: the lines of the error are the test's.
    let source = r#"// Test that we can't call random fns in a const fn or do other bad things.

// use std::mem::transmute;

fn random() -> u32 {
    0
}

const fn sub(x: &u32) -> usize {
    { let _ = x; 0 }
}

const fn sub1() -> u32 {
    random() //~ ERROR E0015
}

static Y: u32 = 0;

const fn get_Y() -> u32 {
    Y
}

const fn get_Y_addr() -> &'static u32 {
    &Y
}

const fn get() -> u32 {
    let x = 22;
    let y = 44;
    x + y
}

fn main() {}
"#;
    let stderr = r#"error[E0015]: cannot call non-const function `random` in constant functions
  --> $DIR/const-fn-not-safe-for-const.rs:14:5
   |
LL |     random()
   |     ^^^^^^^^
   |
note: function `random` is not const
  --> $DIR/const-fn-not-safe-for-const.rs:5:1
   |
LL | fn random() -> u32 {
   | ^^^^^^^^^^^^^^^^^^
   = note: calls in constant functions are limited to constant functions, tuple structs and tuple variants

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0015`.
"#;
    assert_eq!(ui("const-fn-not-safe-for-const.rs", "2015", source), stderr);
}

/// In a `static`, rustc's const checking runs as it evaluates the static, in its type checking
/// of the crate: before its unsafety checking, whose errors come after.
#[test]
fn a_call_in_a_static_comes_before_its_unsafety() {
    // tests/ui/consts/issue-16538.rs
    let source = r#"mod Y {
    pub type X = usize;
    extern "C" {
        pub static x: *const usize;
    }
    pub fn foo(value: *const X) -> *const X {
        value
    }
}

static foo: &Y::X = &*Y::foo(Y::x as *const Y::X);
//~^ ERROR dereference of raw pointer
//~| ERROR E0015
//~| ERROR use of extern static is unsafe and requires

fn main() {}
"#;
    let stderr = r#"error[E0015]: cannot call non-const function `Y::foo` in statics
  --> $DIR/issue-16538.rs:11:23
   |
LL | static foo: &Y::X = &*Y::foo(Y::x as *const Y::X);
   |                       ^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
note: function `foo` is not const
  --> $DIR/issue-16538.rs:6:5
   |
LL |     pub fn foo(value: *const X) -> *const X {
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   = note: calls in statics are limited to constant functions, tuple structs and tuple variants
   = note: consider wrapping this expression in `std::sync::LazyLock::new(|| ...)`

error[E0133]: dereference of raw pointer is unsafe and requires unsafe function or block
  --> $DIR/issue-16538.rs:11:22
   |
LL | static foo: &Y::X = &*Y::foo(Y::x as *const Y::X);
   |                      ^^^^^^^^^^^^^^^^^^^^^^^^^^^^ dereference of raw pointer
   |
   = note: raw pointers may be null, dangling or unaligned; they can violate aliasing rules and cause data races: all of these are undefined behavior

error[E0133]: use of extern static is unsafe and requires unsafe function or block
  --> $DIR/issue-16538.rs:11:30
   |
LL | static foo: &Y::X = &*Y::foo(Y::x as *const Y::X);
   |                              ^^^^ use of extern static
   |
   = note: extern statics are not controlled by the Rust type system: invalid data, aliasing violations or data races will cause undefined behavior

error: aborting due to 3 previous errors

Some errors have detailed explanations: E0015, E0133.
For more information about an error, try `rustc --explain E0015`.
"#;
    assert_eq!(ui("issue-16538.rs", "2015", source), stderr);
}

#[test]
fn a_pointer_cast_to_an_integer_in_a_constant() {
    // tests/ui/consts/issue-18294.rs, its `println!("{}", Y);` written `let _ = Y;`, which
    // needs no std.
    let source = r#"fn main() {
    const X: u32 = 1;
    const Y: usize = unsafe { &X as *const u32 as usize }; //~ ERROR pointers cannot be cast to integers
    let _ = Y;
}
"#;
    let stderr = r#"error: pointers cannot be cast to integers during const eval
  --> $DIR/issue-18294.rs:3:31
   |
LL |     const Y: usize = unsafe { &X as *const u32 as usize };
   |                               ^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = note: at compile-time, pointers do not have an integer value
   = note: avoiding this restriction via `transmute`, `union`, or raw pointers leads to compile-time undefined behavior

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("issue-18294.rs", "2015", source), stderr);
}

#[test]
fn a_function_pointer_call_in_a_const_fn() {
    // tests/ui/consts/issue-56164.rs, its second error; the first, the call of a closure, is
    // not given yet (v2/requests/diagnostics.md, diagnostics-lints).
    let source = r#"const fn bad(input: fn()) {
    input()
    //~^ ERROR function pointer calls are not allowed
}

fn main() {
}
"#;
    let stderr = r#"error: function pointer calls are not allowed in constant functions
  --> $DIR/issue-56164.rs:2:5
   |
LL |     input()
   |     ^^^^^^^

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("issue-56164.rs", "2015", source), stderr);
}

const IMPURE: &str = r#"
static mut COUNT: u32 = 0;

pub const fn count() -> u32 {
    unsafe { COUNT }
}
"#;

/// The lints of a fixture's crates, as `name: message`.
fn lints(fixture: &str) -> Vec<String> {
    let checked = checked(fixture);
    checked
        .diags
        .iter()
        .filter_map(|it| Some(format!("{}: {}", it.lint_name()?, it.message)))
        .collect()
}

#[test]
fn an_impure_const_fn_is_told_where_cached_functions_are_used() {
    let fixture = format!("//- /main.rs crate:main deps:blaze\n{IMPURE}\n//- /blaze.rs crate:blaze\n");
    assert_eq!(lints(&fixture), [
        "blaze::const_impure: const fn reads or writes mutable state: `COUNT` is a `static mut`"
    ]);
}

#[test]
fn an_impure_const_fn_is_not_told_where_rustc_would_not() {
    let fixture = format!("//- /main.rs crate:main\n{IMPURE}");
    assert_eq!(lints(&fixture), Vec::<String>::new());
}

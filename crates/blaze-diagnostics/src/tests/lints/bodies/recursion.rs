//! `unconditional_recursion`, from rustc's UI tests (`tests/ui/enum/`, `tests/ui/consts/`) with
//! their `.stderr`.

use crate::tests::ui;

#[test]
fn a_function_that_calls_itself_in_a_module_named_as_an_enum() {
    // tests/ui/enum/enum-and-module-in-same-scope.rs
    let source = r##"enum Foo {
    X
}

mod Foo { //~ ERROR the name `Foo` is defined multiple times
    pub static X: isize = 42;
    fn f() { f() } // Check that this does not result in a resolution error
    //~^ WARN cannot return without recursing
}

fn main() {}
"##;
    let stderr = r##"error[E0428]: the name `Foo` is defined multiple times
  --> $DIR/enum-and-module-in-same-scope.rs:5:1
   |
LL | enum Foo {
   | -------- previous definition of the type `Foo` here
...
LL | mod Foo {
   | ^^^^^^^ `Foo` redefined here
   |
   = note: `Foo` must be defined only once in the type namespace of this module

warning: function cannot return without recursing
  --> $DIR/enum-and-module-in-same-scope.rs:7:5
   |
LL |     fn f() { f() } // Check that this does not result in a resolution error
   |     ^^^^^^   --- recursive call site
   |     |
   |     cannot return without recursing
   |
   = help: a `loop` may express intention better if this is on purpose
   = note: `#[warn(unconditional_recursion)]` on by default

error: aborting due to 1 previous error; 1 warning emitted

For more information about this error, try `rustc --explain E0428`.
"##;
    assert_eq!(ui("enum-and-module-in-same-scope.rs", "2015", source), stderr);
}

#[test]
fn a_const_fn_that_calls_itself() {
    // tests/ui/consts/recursive.rs
    let source = r##"#![allow(unused)]

const fn f<T>(x: T) { //~ WARN function cannot return without recursing
    f(x);
}

const X: () = f(1); //~ ERROR reached the configured maximum number of stack frames

fn main() {}
"##;
    let stderr = r##"warning: function cannot return without recursing
  --> $DIR/recursive.rs:3:1
   |
LL | const fn f<T>(x: T) {
   | ^^^^^^^^^^^^^^^^^^^ cannot return without recursing
LL |     f(x);
   |     ---- recursive call site
   |
   = help: a `loop` may express intention better if this is on purpose
   = note: `#[warn(unconditional_recursion)]` on by default

error[E0080]: reached the configured maximum number of stack frames
  --> $DIR/recursive.rs:7:15
   |
LL | const X: () = f(1);
   |               ^^^^ evaluation of `X` failed inside this call
   |
note: [... 126 additional calls inside `f::<i32>` ...]
  --> $DIR/recursive.rs:4:5
   |
LL |     f(x);
   |     ^^^^
note: inside `f::<i32>`
  --> $DIR/recursive.rs:4:5
   |
LL |     f(x);
   |     ^^^^ the failure occurred here

error: aborting due to 1 previous error; 1 warning emitted

For more information about this error, try `rustc --explain E0080`.
"##;
    assert_eq!(ui("recursive.rs", "2015", source), stderr);
}

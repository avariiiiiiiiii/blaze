//! rustc's UI tests of a call with the wrong arguments: too few, too many, swapped and
//! permuted, of a function and of a closure.

use super::ui_core;
use crate::tests::ui;

#[test]
fn a_missing_argument_is_provided() {
    // tests/ui/error-codes/E0061.rs
    let source = "fn f(a: u16, b: &str) {}

fn f2(a: u16) {}

fn main() {
    f(0);
    //~^ ERROR E0061

    f2();
    //~^ ERROR E0061
}
";
    let stderr = "error[E0061]: this function takes 2 arguments but 1 argument was supplied
  --> $DIR/E0061.rs:6:5
   |
LL |     f(0);
   |     ^--- argument #2 of type `&str` is missing
   |
note: function defined here
  --> $DIR/E0061.rs:1:4
   |
LL | fn f(a: u16, b: &str) {}
   |    ^         -------
help: provide the argument
   |
LL |     f(0, /* &str */);
   |        ++++++++++++

error[E0061]: this function takes 1 argument but 0 arguments were supplied
  --> $DIR/E0061.rs:9:5
   |
LL |     f2();
   |     ^^-- argument #1 of type `u16` is missing
   |
note: function defined here
  --> $DIR/E0061.rs:3:4
   |
LL | fn f2(a: u16) {}
   |    ^^ ------
help: provide the argument
   |
LL |     f2(/* u16 */);
   |        +++++++++

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0061`.
";
    assert_eq!(ui("E0061.rs", "2015", source), stderr);
}

#[test]
fn a_closure_called_with_too_few_and_too_many_arguments() {
    // tests/ui/error-codes/E0057.rs
    let source = "fn main() {
    let f = |x| x * 3;
    let a = f(); //~ ERROR E0057
    let b = f(4);
    let c = f(2, 3); //~ ERROR E0057
}
";
    let stderr = "error[E0057]: this function takes 1 argument but 0 arguments were supplied
  --> $DIR/E0057.rs:3:13
   |
LL |     let a = f();
   |             ^-- argument #1 is missing
   |
note: closure defined here
  --> $DIR/E0057.rs:2:13
   |
LL |     let f = |x| x * 3;
   |             ^^^
help: provide the argument
   |
LL |     let a = f(/* x */);
   |               +++++++

error[E0057]: this function takes 1 argument but 2 arguments were supplied
  --> $DIR/E0057.rs:5:13
   |
LL |     let c = f(2, 3);
   |             ^    - unexpected argument #2 of type `{integer}`
   |
note: closure defined here
  --> $DIR/E0057.rs:2:13
   |
LL |     let f = |x| x * 3;
   |             ^^^
help: remove the extra argument
   |
LL -     let c = f(2, 3);
LL +     let c = f(2);
   |

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0057`.
";
    assert_eq!(ui_core("E0057.rs", "fn", source), stderr);
}

#[test]
fn each_form_of_wrong_arguments() {
    // tests/ui/argument-suggestions/basic.rs
    let source = "// Some basic \"obvious\" cases for the heuristic error messages added for #65853
// One for each of the detected cases

enum E { X, Y }
enum F { X2, Y2 }
struct G {}
struct H {}
struct X {}
struct Y {}
struct Z {}


fn invalid(_i: u32) {}
fn extra() {}
fn missing(_i: u32) {}
fn swapped(_i: u32, _s: &str) {}
fn permuted(_x: X, _y: Y, _z: Z) {}

fn main() {
    invalid(1.0); //~ ERROR mismatched types
    extra(\"\"); //~ ERROR function takes
    missing(); //~ ERROR function takes
    swapped(\"\", 1); //~ ERROR arguments to this function are incorrect
    permuted(Y {}, Z {}, X {}); //~ ERROR arguments to this function are incorrect

    let closure = |x| x;
    closure(); //~ ERROR function takes
}
";
    let stderr = "error[E0308]: mismatched types
  --> $DIR/basic.rs:20:13
   |
LL |     invalid(1.0);
   |     ------- ^^^ expected `u32`, found floating-point number
   |     |
   |     arguments to this function are incorrect
   |
note: function defined here
  --> $DIR/basic.rs:13:4
   |
LL | fn invalid(_i: u32) {}
   |    ^^^^^^^ -------

error[E0061]: this function takes 0 arguments but 1 argument was supplied
  --> $DIR/basic.rs:21:5
   |
LL |     extra(\"\");
   |     ^^^^^ -- unexpected argument of type `&'static str`
   |
note: function defined here
  --> $DIR/basic.rs:14:4
   |
LL | fn extra() {}
   |    ^^^^^
help: remove the extra argument
   |
LL -     extra(\"\");
LL +     extra();
   |

error[E0061]: this function takes 1 argument but 0 arguments were supplied
  --> $DIR/basic.rs:22:5
   |
LL |     missing();
   |     ^^^^^^^-- argument #1 of type `u32` is missing
   |
note: function defined here
  --> $DIR/basic.rs:15:4
   |
LL | fn missing(_i: u32) {}
   |    ^^^^^^^ -------
help: provide the argument
   |
LL |     missing(/* u32 */);
   |             +++++++++

error[E0308]: arguments to this function are incorrect
  --> $DIR/basic.rs:23:5
   |
LL |     swapped(\"\", 1);
   |     ^^^^^^^ --  - expected `&str`, found `{integer}`
   |             |
   |             expected `u32`, found `&'static str`
   |
note: function defined here
  --> $DIR/basic.rs:16:4
   |
LL | fn swapped(_i: u32, _s: &str) {}
   |    ^^^^^^^
help: swap these arguments
   |
LL -     swapped(\"\", 1);
LL +     swapped(1, \"\");
   |

error[E0308]: arguments to this function are incorrect
  --> $DIR/basic.rs:24:5
   |
LL |     permuted(Y {}, Z {}, X {});
   |     ^^^^^^^^ ----  ----  ---- expected `Z`, found `X`
   |              |     |
   |              |     expected `Y`, found `Z`
   |              expected `X`, found `Y`
   |
note: function defined here
  --> $DIR/basic.rs:17:4
   |
LL | fn permuted(_x: X, _y: Y, _z: Z) {}
   |    ^^^^^^^^
help: reorder these arguments
   |
LL -     permuted(Y {}, Z {}, X {});
LL +     permuted(X {}, Y {}, Z {});
   |

error[E0057]: this function takes 1 argument but 0 arguments were supplied
  --> $DIR/basic.rs:27:5
   |
LL |     closure();
   |     ^^^^^^^-- argument #1 is missing
   |
note: closure defined here
  --> $DIR/basic.rs:26:19
   |
LL |     let closure = |x| x;
   |                   ^^^
help: provide the argument
   |
LL |     closure(/* x */);
   |             +++++++

error: aborting due to 6 previous errors

Some errors have detailed explanations: E0057, E0061, E0308.
For more information about an error, try `rustc --explain E0057`.
";
    assert_eq!(ui_core("basic.rs", "fn", source), stderr);
}

//! rustc's UI tests of the type errors type checking gives of bodies: E0308 in its forms (a
//! `let`'s type, an argument, a return type, a body with no tail), E0317, E0069, E0277 with
//! `#[diagnostic::on_unimplemented]` and the callee's bound, E0525, E0282 and `_` as an
//! expression; and #1522, an E0277 inside a macro like `assert_eq!`. A test whose file has
//! errors rustc gives in a form not yet given here keeps the functions of the forms that are,
//! its other lines left out (and its line numbers changed with them), as each test says.

use crate::{
    Options,
    emit::Style,
    tests::{Mode, checked, checked_with, compiletest, normalized, ui},
};

/// [`ui`] of a file with minicore's items for `flags` (`crates/test-utils/src/minicore.rs`).
fn ui_core(name: &str, flags: &str, source: &str) -> String {
    let fixture = format!("//- minicore: {flags}\n//- /{name} crate:main edition:2015\n{source}");
    let checked = checked_with(&fixture, "$DIR/", &compiletest(Mode::Other, &Options::default()));
    normalized(&checked.stderr_with(Style { ui_testing: true, ..Style::default() }))
}

#[test]
fn an_argument_of_another_type_points_at_the_function() {
    // tests/ui/mismatched_types/array-arg-expected-usize.rs
    let source = "//! Regression test for <https://github.com/rust-lang/rust/issues/4517>.
//! Test error message use correct array syntax.
//@ dont-require-annotations: NOTE

fn bar(int_param: usize) {}

fn main() {
    let foo: [u8; 4] = [1; 4];
    bar(foo);
    //~^ ERROR mismatched types
    //~| NOTE expected `usize`, found `[u8; 4]`
}
";
    let stderr = "error[E0308]: mismatched types
  --> $DIR/array-arg-expected-usize.rs:9:9
   |
LL |     bar(foo);
   |     --- ^^^ expected `usize`, found `[u8; 4]`
   |     |
   |     arguments to this function are incorrect
   |
note: function defined here
  --> $DIR/array-arg-expected-usize.rs:5:4
   |
LL | fn bar(int_param: usize) {}
   |    ^^^ ----------------

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0308`.
";
    assert_eq!(ui("array-arg-expected-usize.rs", "2015", source), stderr);
}

#[test]
fn a_let_type_is_why_and_a_suffix_is_changed() {
    // tests/ui/mismatched_types/float-literal-inference-restrictions.rs, its second `let`: the
    // first's `1` rustc calls an integer, of a type not yet decided.
    let source = "fn main() {
    let y: f32 = 1f64; //~ ERROR mismatched types
}
";
    let stderr = "error[E0308]: mismatched types
  --> $DIR/float-literal-inference-restrictions.rs:2:18
   |
LL |     let y: f32 = 1f64;
   |            ---   ^^^^ expected `f32`, found `f64`
   |            |
   |            expected due to this
   |
help: change the type of the numeric literal from `f64` to `f32`
   |
LL -     let y: f32 = 1f64;
LL +     let y: f32 = 1f32;
   |

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0308`.
";
    assert_eq!(ui("float-literal-inference-restrictions.rs", "2015", source), stderr);
}

#[test]
fn a_returned_value_of_another_type_names_both_in_full() {
    // tests/ui/mismatched_types/abridged.rs to its first function: the others suggest
    // wrapping in `Some` and `Ok` and print an integer not yet decided.
    let source = "enum Bar {
    Qux,
    Zar,
}

struct Foo {
    bar: usize,
}

struct X<T1, T2> {
    x: T1,
    y: T2,
}

fn a() -> Foo {
    Some(Foo { bar: 1 }) //~ ERROR mismatched types
}

fn main() {}
";
    let stderr = "error[E0308]: mismatched types
  --> $DIR/abridged.rs:16:5
   |
LL | fn a() -> Foo {
   |           --- expected `Foo` because of return type
LL |     Some(Foo { bar: 1 })
   |     ^^^^^^^^^^^^^^^^^^^^ expected `Foo`, found `Option<Foo>`
   |
   = note: expected struct `Foo`
                found enum `Option<Foo>`

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0308`.
";
    assert_eq!(ui_core("abridged.rs", "option", source), stderr);
}

#[test]
fn a_body_with_no_tail_points_at_the_return_type() {
    // tests/ui/block-result/consider-removing-last-semi.rs to `macro_tests`, its literal
    // `1u32`: rustc decides `1`'s type from the return type only once the error is found.
    let source = "pub fn macro_tests() -> u32 {  //~ ERROR mismatched types
    macro_rules! mac {
        () => (1u32);
    }
    mac!();
}

fn main() {}
";
    let stderr = "error[E0308]: mismatched types
  --> $DIR/consider-removing-last-semi.rs:1:25
   |
LL | pub fn macro_tests() -> u32 {
   |        -----------      ^^^ expected `u32`, found `()`
   |        |
   |        implicitly returns `()` as its body has no tail or `return` expression
...
LL |     mac!();
   |           - help: remove this semicolon to return this value

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0308`.
";
    assert_eq!(ui("consider-removing-last-semi.rs", "2015", source), stderr);
}

#[test]
fn an_if_with_no_else_where_a_value_is_returned() {
    // tests/ui/expr/if/if-without-else-as-fn-expr.rs to its first function.
    let source = "fn foo(bar: usize) -> usize {
    if bar % 5 == 0 {
        return 3;
    }
    //~^^^ ERROR `if` may be missing an `else` clause
}

fn main() {}
";
    let stderr = "error[E0317]: `if` may be missing an `else` clause
  --> $DIR/if-without-else-as-fn-expr.rs:2:5
   |
LL |   fn foo(bar: usize) -> usize {
   |                         ----- expected `usize` because of this return type
LL | /     if bar % 5 == 0 {
LL | |         return 3;
LL | |     }
   | |_____^ expected `usize`, found `()`
   |
   = note: `if` expressions without `else` evaluate to `()`
   = help: consider adding an `else` block that evaluates to the expected type

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0317`.
";
    assert_eq!(ui("if-without-else-as-fn-expr.rs", "2015", source), stderr);
}

#[test]
fn a_return_with_no_value() {
    // tests/ui/error-codes/E0069.rs
    let source = "fn foo() -> u8 {
    return;
    //~^ ERROR `return;` in a function whose return type is not `()`
}

fn main() {
}
";
    let stderr = "error[E0069]: `return;` in a function whose return type is not `()`
  --> $DIR/E0069.rs:2:5
   |
LL | fn foo() -> u8 {
   |             -- expected `u8` because of this return type
LL |     return;
   |     ^^^^^^ return type is not `()`
   |
help: give the `return` a value of the expected type
   |
LL |     return 42;
   |            ++

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0069`.
";
    assert_eq!(ui("E0069.rs", "2015", source), stderr);
}

#[test]
fn a_bound_of_the_callee_that_does_not_hold() {
    // tests/ui/error-codes/E0277.rs without `fn f(p: Path)`, whose `Path` is std's: its lines
    // are left empty.
    let source = "

trait Foo {
    fn bar(&self);
}

fn some_func<T: Foo>(foo: T) {
    foo.bar();
}




fn main() {
    some_func(5i32);
    //~^ ERROR the trait bound `i32: Foo` is not satisfied
}
";
    let stderr = "error[E0277]: the trait bound `i32: Foo` is not satisfied
  --> $DIR/E0277.rs:15:15
   |
LL |     some_func(5i32);
   |     --------- ^^^^ the trait `Foo` is not implemented for `i32`
   |     |
   |     required by a bound introduced by this call
   |
help: this trait has no implementations, consider adding one
  --> $DIR/E0277.rs:3:1
   |
LL | trait Foo {
   | ^^^^^^^^^
note: required by a bound in `some_func`
  --> $DIR/E0277.rs:7:17
   |
LL | fn some_func<T: Foo>(foo: T) {
   |                 ^^^ required by this bound in `some_func`

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0277`.
";
    assert_eq!(ui("E0277.rs", "2015", source), stderr);
}

#[test]
fn on_unimplemented_gives_the_message_label_and_notes() {
    // tests/ui/diagnostic_namespace/on_unimplemented/multiple_notes.rs
    let source = r#"//@ reference: attributes.diagnostic.on_unimplemented.note-repetition
#[diagnostic::on_unimplemented(message = "Foo", label = "Bar", note = "Baz", note = "Boom")]
trait Foo {}

#[diagnostic::on_unimplemented(message = "Bar", label = "Foo", note = "Baz")]
#[diagnostic::on_unimplemented(note = "Baz2")]
trait Bar {}

fn takes_foo(_: impl Foo) {}
fn takes_bar(_: impl Bar) {}

fn main() {
    takes_foo(());
    //~^ERROR Foo
    takes_bar(());
    //~^ERROR Bar
}
"#;
    let stderr = "error[E0277]: Foo
  --> $DIR/multiple_notes.rs:13:15
   |
LL |     takes_foo(());
   |     --------- ^^ Bar
   |     |
   |     required by a bound introduced by this call
   |
   = help: the trait `Foo` is not implemented for `()`
   = note: Baz
   = note: Boom
help: this trait has no implementations, consider adding one
  --> $DIR/multiple_notes.rs:3:1
   |
LL | trait Foo {}
   | ^^^^^^^^^
note: required by a bound in `takes_foo`
  --> $DIR/multiple_notes.rs:9:22
   |
LL | fn takes_foo(_: impl Foo) {}
   |                      ^^^ required by this bound in `takes_foo`

error[E0277]: Bar
  --> $DIR/multiple_notes.rs:15:15
   |
LL |     takes_bar(());
   |     --------- ^^ Foo
   |     |
   |     required by a bound introduced by this call
   |
   = help: the trait `Bar` is not implemented for `()`
   = note: Baz
   = note: Baz2
help: this trait has no implementations, consider adding one
  --> $DIR/multiple_notes.rs:7:1
   |
LL | trait Bar {}
   | ^^^^^^^^^
note: required by a bound in `takes_bar`
  --> $DIR/multiple_notes.rs:10:22
   |
LL | fn takes_bar(_: impl Bar) {}
   |                      ^^^ required by this bound in `takes_bar`

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0277`.
";
    assert_eq!(ui("multiple_notes.rs", "2015", source), stderr);
}

#[test]
fn a_closure_that_moves_given_to_a_bound_of_fn() {
    // tests/ui/closures/closure-wrong-kind.rs
    let source = "/* Any copyright is dedicated to the Public Domain.
 * http://creativecommons.org/publicdomain/zero/1.0/ */

struct X;
fn foo<T>(_: T) {}
fn bar<T: Fn(u32)>(_: T) {}

fn main() {
    let x = X;
    let closure = |_| foo(x);  //~ ERROR E0525
    bar(closure);
}
";
    let stderr = "error[E0525]: expected a closure that implements the `Fn` trait, but this closure only implements `FnOnce`
  --> $DIR/closure-wrong-kind.rs:10:19
   |
LL |     let closure = |_| foo(x);
   |                   ^^^     - closure is `FnOnce` because it moves the variable `x` out of its environment
   |                   |
   |                   this closure implements `FnOnce`, not `Fn`
LL |     bar(closure);
   |     --- ------- the requirement to implement `Fn` derives from here
   |     |
   |     required by a bound introduced by this call
   |
note: required by a bound in `bar`
  --> $DIR/closure-wrong-kind.rs:6:11
   |
LL | fn bar<T: Fn(u32)>(_: T) {}
   |           ^^^^^^^ required by this bound in `bar`

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0525`.
";
    assert_eq!(ui_core("closure-wrong-kind.rs", "fn", source), stderr);
}

#[test]
#[ignore = "the front end decides `{integer}` as `i32` before reporting (v2/requests/diagnostics.md, diagnostics-bodies-types 2)"]
fn match_arms_of_incompatible_types_skip_the_diverging_one() {
    // tests/ui/match/match-arm-resolving-to-never.rs, its fifth arm's call of std's macro that
    // panics as not implemented written `loop {}`, which diverges as it does.
    let source = "enum E {
    A,
    B,
    C,
    D,
    E,
    F,
}

fn main() {
    match E::F {
        E::A => 1,
        E::B => 2,
        E::C => 3,
        E::D => 4,
        E::E => loop {},
        E::F => \"\", //~ ERROR `match` arms have incompatible types
    };
}
";
    let stderr = "error[E0308]: `match` arms have incompatible types
  --> $DIR/match-arm-resolving-to-never.rs:17:17
   |
LL | /     match E::F {
LL | |         E::A => 1,
   | |                 - this is found to be of type `{integer}`
LL | |         E::B => 2,
   | |                 - this is found to be of type `{integer}`
LL | |         E::C => 3,
   | |                 - this is found to be of type `{integer}`
LL | |         E::D => 4,
   | |                 - this is found to be of type `{integer}`
LL | |         E::E => loop {},
LL | |         E::F => \"\",
   | |                 ^^ expected integer, found `&str`
LL | |     };
   | |_____- `match` arms have incompatible types

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0308`.
";
    assert_eq!(ui("match-arm-resolving-to-never.rs", "2015", source), stderr);
}

#[test]
fn a_let_with_no_type_and_no_use() {
    // tests/ui/error-codes/E0282.rs
    let source = "fn main() {
    let x;
    //~^ ERROR E0282
}
";
    let stderr = "error[E0282]: type annotations needed
  --> $DIR/E0282.rs:2:9
   |
LL |     let x;
   |         ^
   |
help: consider giving `x` an explicit type
   |
LL |     let x: /* Type */;
   |          ++++++++++++

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0282`.
";
    assert_eq!(ui("E0282.rs", "2015", source), stderr);
}

#[test]
fn an_underscore_as_an_argument() {
    // tests/ui/suggestions/fn-or-tuple-struct-with-underscore-args.rs
    let source = "fn foo(a: usize, b: usize) -> usize { a }

struct S(usize, usize);

trait T {
    fn baz(x: usize, y: usize) -> usize { x }
}

fn main() {
    let _: usize = foo(_, _);
    //~^ ERROR `_` can only be used on the left-hand side of an assignment
    //~| ERROR `_` can only be used on the left-hand side of an assignment
    let _: S = S(_, _);
    //~^ ERROR `_` can only be used on the left-hand side of an assignment
    //~| ERROR `_` can only be used on the left-hand side of an assignment
    let _: usize = T::baz(_, _);
    //~^ ERROR `_` can only be used on the left-hand side of an assignment
    //~| ERROR `_` can only be used on the left-hand side of an assignment
}
";
    let mut stderr = String::new();
    for (line, col, text) in [
        (10, 24, "    let _: usize = foo(_, _);\n   |                        ^"),
        (10, 27, "    let _: usize = foo(_, _);\n   |                           ^"),
        (13, 18, "    let _: S = S(_, _);\n   |                  ^"),
        (13, 21, "    let _: S = S(_, _);\n   |                     ^"),
        (16, 27, "    let _: usize = T::baz(_, _);\n   |                           ^"),
        (16, 30, "    let _: usize = T::baz(_, _);\n   |                              ^"),
    ] {
        stderr.push_str(&format!(
            "error: in expressions, `_` can only be used on the left-hand side of an assignment
  --> $DIR/fn-or-tuple-struct-with-underscore-args.rs:{line}:{col}
   |
LL | {text} `_` not allowed here

"
        ));
    }
    stderr.push_str("error: aborting due to 6 previous errors\n\n");
    assert_eq!(ui("fn-or-tuple-struct-with-underscore-args.rs", "2015", source), stderr);
}

/// #1522: `assert_eq!` of values whose type does not implement `Debug` gave no error. Its
/// expansion calls `core::panicking::assert_failed`, bounded by `Debug`, through references
/// to the values, and the bound fails through an impl: here a local `Debug`, `Wrapper<T>`'s
/// impl of it needing `T: Debug`, and a macro of `assert_eq!`'s shape, `assert_failed` bounded
/// as core's is, `T: Debug + ?Sized`.
#[test]
fn an_unmet_bound_inside_an_assert_eq_like_macro_is_reported() {
    let fixture = "//- minicore: sized
//- /main.rs crate:main
trait Debug {}
struct Digest;
struct Wrapper<T>(T);
impl<T: Debug> Debug for Wrapper<T> {}
fn assert_failed<T: Debug + ?Sized>(_left: &T, _right: &T) {}
macro_rules! assert_eq {
    ($left:expr, $right:expr) => {
        match (&$left, &$right) {
            (left_val, right_val) => {
                if false {
                    assert_failed(&*left_val, &*right_val);
                }
            }
        }
    };
}
fn main() {
    assert_eq!(Wrapper(Digest), Wrapper(Digest));
}
";
    let checked = checked(fixture);
    assert_eq!(checked.messages(), ["E0277: the trait bound `Digest: Debug` is not satisfied"]);
    let notes: Vec<&str> = checked.diags[0].children.iter().map(|it| it.message.as_str()).collect();
    assert!(notes.contains(&"required for `Wrapper<Digest>` to implement `Debug`"), "{notes:?}");
    assert!(notes.contains(&"required by a bound in `assert_failed`"), "{notes:?}");
}

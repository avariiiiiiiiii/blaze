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
fn a_closure_that_moves_a_field_names_the_field() {
    // tests/ui/closures/2229_closure_analysis/diagnostics/closure-origin-tuple-diagnostics.rs
    let source = r##"//@ edition:2021

struct S(String, String);

fn expect_fn<F: Fn()>(_f: F) {}

fn main() {
    let s = S(format!("s"), format!("s"));
    let c = || { //~ ERROR expected a closure that implements the `Fn`
        let s = s.1;
    };
    expect_fn(c);
}
"##;
    let stderr = r##"error[E0525]: expected a closure that implements the `Fn` trait, but this closure only implements `FnOnce`
  --> $DIR/closure-origin-tuple-diagnostics.rs:9:13
   |
LL |     let c = || {
   |             ^^ this closure implements `FnOnce`, not `Fn`
LL |         let s = s.1;
   |                 --- closure is `FnOnce` because it moves the variable `s.1` out of its environment
LL |     };
LL |     expect_fn(c);
   |     --------- - the requirement to implement `Fn` derives from here
   |     |
   |     required by a bound introduced by this call
   |
note: required by a bound in `expect_fn`
  --> $DIR/closure-origin-tuple-diagnostics.rs:5:17
   |
LL | fn expect_fn<F: Fn()>(_f: F) {}
   |                 ^^^^ required by this bound in `expect_fn`

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0525`.
"##;
    let fixture = format!(
        "//- minicore: fn\n//- /std.rs crate:std deps:core\n\
         pub mod prelude {{ pub mod rust_2021 {{ pub use core::prelude::rust_2021::*; pub use crate::string::String; }} }}\n\
         pub mod string {{ pub struct String; }}\n\
         #[macro_export]\n\
         macro_rules! format {{ ($($arg:tt)*) => {{ $crate::string::String }}; }}\n\
         //- /closure-origin-tuple-diagnostics.rs crate:main edition:2021 deps:std\n{source}"
    );
    let checked = checked_with(&fixture, "$DIR/", &compiletest(Mode::Other, &Options::default()));
    assert_eq!(normalized(&checked.stderr_with(Style { ui_testing: true, ..Style::default() })), stderr);
}

#[test]
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

#[test]
fn a_let_of_a_type_with_an_undecided_parameter() {
    // tests/ui/array-slice-vec/vector-no-ann.rs
    let source = "fn main() {
    let _foo = Vec::new();
    //~^ ERROR type annotations needed
}
";
    let stderr = "error[E0282]: type annotations needed for `Vec<_>`
  --> $DIR/vector-no-ann.rs:2:9
   |
LL |     let _foo = Vec::new();
   |         ^^^^   ---------- type must be known at this point
   |
help: consider giving `_foo` an explicit type, where the type for type parameter `T` is specified
   |
LL |     let _foo: Vec<T> = Vec::new();
   |             ++++++++

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0282`.
";
    assert_eq!(ui("vector-no-ann.rs", "2015", source), stderr);
}

#[test]
fn a_let_of_an_empty_vec_macro() {
    // tests/ui/type/type-check/cannot_infer_local_or_vec.rs
    let source = "fn main() {
    let x = vec![];
    //~^ ERROR type annotations needed
}
";
    let stderr = "error[E0282]: type annotations needed for `Vec<_>`
  --> $DIR/cannot_infer_local_or_vec.rs:2:9
   |
LL |     let x = vec![];
   |         ^   ------ type must be known at this point
   |
help: consider giving `x` an explicit type, where the placeholder `_` is specified
   |
LL |     let x: Vec<_> = vec![];
   |          ++++++++

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0282`.
";
    assert_eq!(ui("cannot_infer_local_or_vec.rs", "2015", source), stderr);
}

#[test]
fn a_let_of_undecided_types_counts_its_placeholders() {
    // tests/ui/inference/need_type_info/underscore-placeholder-wording.rs
    let source = "// Check that `need_type_info` distinguishes between a single placeholder and
// multiple placeholders when suggesting an explicit type.

fn singular() {
    let v = &[];
    //~^ ERROR type annotations needed
    let _ = v.iter();
}

fn plural() {
    let x = (vec![], vec![]);
    //~^ ERROR type annotations needed
    let _ = x;
}

fn main() {}
";
    let stderr = "error[E0282]: type annotations needed for `&[_; 0]`
  --> $DIR/underscore-placeholder-wording.rs:5:9
   |
LL |     let v = &[];
   |         ^   --- type must be known at this point
   |
help: consider giving `v` an explicit type, where the placeholder `_` is specified
   |
LL |     let v: &[_; 0] = &[];
   |          +++++++++

error[E0282]: type annotations needed for `(Vec<_>, Vec<_>)`
  --> $DIR/underscore-placeholder-wording.rs:11:9
   |
LL |     let x = (vec![], vec![]);
   |         ^   ---------------- type must be known at this point
   |
help: consider giving `x` an explicit type, where the placeholders `_` are specified
   |
LL |     let x: (Vec<_>, Vec<_>) = (vec![], vec![]);
   |          ++++++++++++++++++

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0282`.
";
    assert_eq!(ui("underscore-placeholder-wording.rs", "2015", source), stderr);
}

#[test]
#[ignore = "needs the named type of a `let` an ambiguous goal's variable is in (`v2/requests/db-ty.md`): `top_term` is the goal's variable alone, so `W` and `Z` print `_`"]
fn a_let_of_an_ambiguous_call_erases_its_decided_parameters() {
    // tests/ui/inference/erase-type-params-in-label.rs
    let source = r#"fn main() {
    let foo = foo(1, ""); //~ ERROR E0283
}
fn baz() {
    let bar = bar(1, ""); //~ ERROR E0283
}

struct Bar<T, K, N: Default> {
    t: T,
    k: K,
    n: N,
}

fn bar<T, K, Z: Default>(t: T, k: K) -> Bar<T, K, Z> {
    Bar { t, k, n: Default::default() }
}

struct Foo<T, K, N: Default, M: Default> {
    t: T,
    k: K,
    n: N,
    m: M,
}

fn foo<T, K, W: Default, Z: Default>(t: T, k: K) -> Foo<T, K, W, Z> {
    Foo { t, k, n: Default::default(), m: Default::default() }
}
"#;
    let stderr = "error[E0283]: type annotations needed for `Foo<i32, &str, _, _>`
  --> $DIR/erase-type-params-in-label.rs:2:9
   |
LL |     let foo = foo(1, \"\");
   |         ^^^   ---------- type must be known at this point
   |
   = note: the type must implement `Default`
note: required by a bound in `foo`
  --> $DIR/erase-type-params-in-label.rs:25:17
   |
LL | fn foo<T, K, W: Default, Z: Default>(t: T, k: K) -> Foo<T, K, W, Z> {
   |                 ^^^^^^^ required by this bound in `foo`
help: consider giving `foo` an explicit type, where the type for type parameter `W` is specified
   |
LL |     let foo: Foo<_, &_, W, Z> = foo(1, \"\");
   |            ++++++++++++++++++

error[E0283]: type annotations needed for `Bar<i32, &str, _>`
  --> $DIR/erase-type-params-in-label.rs:5:9
   |
LL |     let bar = bar(1, \"\");
   |         ^^^   ---------- type must be known at this point
   |
   = note: the type must implement `Default`
note: required by a bound in `bar`
  --> $DIR/erase-type-params-in-label.rs:14:17
   |
LL | fn bar<T, K, Z: Default>(t: T, k: K) -> Bar<T, K, Z> {
   |                 ^^^^^^^ required by this bound in `bar`
help: consider giving `bar` an explicit type, where the type for type parameter `Z` is specified
   |
LL |     let bar: Bar<_, &_, Z> = bar(1, \"\");
   |            +++++++++++++++

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0283`.
";
    assert_eq!(ui("erase-type-params-in-label.rs", "2015", source), stderr);
}

#[test]
#[ignore = "needs the named type of a `let` an ambiguous goal's variable is in (`v2/requests/db-ty.md`): `top_term` is the goal's variable alone, so `T` and `U` print `_`"]
fn a_let_assigned_later_names_both_parameters() {
    // tests/ui/traits/next-solver/well-formed-in-relate.rs
    let source = "fn main() {
    let x;
    //~^ ERROR type annotations needed for `Map<_, _>`
    higher_ranked();
    x = unconstrained_map();
}

fn higher_ranked() where for<'a> &'a (): Sized {}

struct Map<T, U> where T: Fn() -> U {
    t: T,
}

trait Mirror {
    type Assoc;
}
impl<T> Mirror for T {
    type Assoc = T;
}

fn unconstrained_map<T: Fn() -> U, U>() -> <Map<T, U> as Mirror>::Assoc { todo!() }
";
    let stderr = "error[E0283]: type annotations needed for `Map<_, _>`
  --> $DIR/well-formed-in-relate.rs:2:9
   |
LL |     let x;
   |         ^
...
LL |     x = unconstrained_map();
   |         ------------------- type must be known at this point
   |
   = note: multiple `impl`s satisfying `_: Fn()` found in the following crates: `alloc`, `core`:
           - impl<A, F> Fn<A> for &F
             where A: std::marker::Tuple, F: Fn<A>, F: ?Sized;
           - impl<Args, F, A> Fn<Args> for Box<F, A>
             where Args: std::marker::Tuple, F: Fn<Args>, A: Allocator, F: ?Sized;
           - impl<F, Args> Fn<Args> for SyncView<F>
             where F: Sync, F: Fn<Args>, Args: std::marker::Tuple;
note: required by a bound in `unconstrained_map`
  --> $DIR/well-formed-in-relate.rs:21:25
   |
LL | fn unconstrained_map<T: Fn() -> U, U>() -> <Map<T, U> as Mirror>::Assoc { todo!() }
   |                         ^^^^^^^^^ required by this bound in `unconstrained_map`
help: consider giving `x` an explicit type, where the type for type parameter `T` is specified
   |
LL |     let x: Map<T, U>;
   |          +++++++++++

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0283`.
";
    assert_eq!(ui("well-formed-in-relate.rs", "2015", source), stderr);
}

#[test]
fn an_ambiguous_into_between_local_impls() {
    // tests/ui/inference/multiple-impl-apply.rs
    let source = "struct Foo {
    inner: u32,
}

struct Bar {
    inner: u32,
}

#[derive(Clone, Copy)]
struct Baz {
    inner: u32,
}

impl From<Baz> for Bar {
    fn from(other: Baz) -> Self {
        Self {
            inner: other.inner,
        }
    }
}

impl From<Baz> for Foo {
    fn from(other: Baz) -> Self {
        Self {
            inner: other.inner,
        }
    }
}

fn main() {
    let x: Baz = Baz { inner: 42 };

    // DOESN'T Compile: Multiple options!
    let y = x.into(); //~ ERROR E0283

    let y_1: Foo = x.into();
    let y_2: Bar = x.into();

    let z_1 = Foo::from(y_1);
    let z_2 = Bar::from(y_2);

    // No type annotations needed, the compiler KNOWS the type must be `Foo`!
    let m = magic_foo(x);
}

fn magic_foo(arg: Baz) -> Foo {
    arg.into()
}
";
    let stderr = "error[E0283]: type annotations needed
  --> $DIR/multiple-impl-apply.rs:34:9
   |
LL |     let y = x.into();
   |         ^     ---- type must be known at this point
   |
note: multiple `impl`s satisfying `_: From<Baz>` found
  --> $DIR/multiple-impl-apply.rs:14:1
   |
LL | impl From<Baz> for Bar {
   | ^^^^^^^^^^^^^^^^^^^^^^
...
LL | impl From<Baz> for Foo {
   | ^^^^^^^^^^^^^^^^^^^^^^
   = note: required for `Baz` to implement `Into<_>`
help: consider giving `y` an explicit type
   |
LL |     let y: /* Type */ = x.into();
   |          ++++++++++++

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0283`.
";
    assert_eq!(ui("multiple-impl-apply.rs", "2015", source), stderr);
}

#[test]
fn an_ambiguous_bound_of_a_call_names_the_bound() {
    // tests/ui/type/type-annotation-needed.rs
    let source = "fn foo<T: Into<String>>(x: i32) {}
//~^ NOTE required by
//~| NOTE required by

fn main() {
    foo(42);
    //~^ ERROR type annotations needed
    //~| NOTE cannot infer type
    //~| NOTE the type must implement
}
";
    let stderr = "error[E0283]: type annotations needed
  --> $DIR/type-annotation-needed.rs:6:5
   |
LL |     foo(42);
   |     ^^^ cannot infer type of the type parameter `T` declared on the function `foo`
   |
   = note: the type must implement `Into<String>`
note: required by a bound in `foo`
  --> $DIR/type-annotation-needed.rs:1:11
   |
LL | fn foo<T: Into<String>>(x: i32) {}
   |           ^^^^^^^^^^^^ required by this bound in `foo`
help: consider specifying a concrete type for the type parameter `T`
   |
LL |     foo::</* Type */>(42);
   |        ++++++++++++++

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0283`.
";
    assert_eq!(ui("type-annotation-needed.rs", "2015", source), stderr);
}

#[test]
fn a_projection_mismatch_points_at_the_selected_impls_type() {
    // tests/ui/associated-types/mismatch-two-relevant-impls.rs
    let source = "trait Tr {
    type Assoc;
}

struct W<T>(T);

impl Tr for W<i32> {
    type Assoc = u32;
}

impl Tr for W<u32> {
    type Assoc = i32;
}

fn needs_unit<T: Tr<Assoc = ()>>() {}

fn main() {
    needs_unit::<W<i32>>();
    //~^ ERROR type mismatch resolving `<W<i32> as Tr>::Assoc == ()`
}
";
    let stderr = "error[E0271]: type mismatch resolving `<W<i32> as Tr>::Assoc == ()`
  --> $DIR/mismatch-two-relevant-impls.rs:18:18
   |
LL |     needs_unit::<W<i32>>();
   |                  ^^^^^^ type mismatch resolving `<W<i32> as Tr>::Assoc == ()`
   |
note: expected this to be `()`
  --> $DIR/mismatch-two-relevant-impls.rs:8:18
   |
LL |     type Assoc = u32;
   |                  ^^^
note: required by a bound in `needs_unit`
  --> $DIR/mismatch-two-relevant-impls.rs:15:21
   |
LL | fn needs_unit<T: Tr<Assoc = ()>>() {}
   |                     ^^^^^^^^^^ required by this bound in `needs_unit`

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0271`.
";
    assert_eq!(ui("mismatch-two-relevant-impls.rs", "2015", source), stderr);
}

#[test]
#[ignore = "needs the cause of a supertrait's bound checked at an impl (rustc's `WhereClause`), which the front end does not record"]
fn a_projection_mismatch_of_an_iterator_names_what_it_yields() {
    // tests/ui/traits/assoc-type-in-superbad.rs
    let source = "// Test case where an associated type is referenced from within the
// supertrait definition, and the impl makes the wrong
// associations. Issue #20220.

use std::vec::IntoIter;

pub trait Foo: Iterator<Item = <Self as Foo>::Key> {
    type Key;
}

impl Foo for IntoIter<i32> {
    type Key = u32;
    //~^ ERROR expected `IntoIter<i32>` to be an iterator that yields `u32`, but it yields `i32`
}

fn main() {}
";
    let stderr = "error[E0271]: expected `IntoIter<i32>` to be an iterator that yields `u32`, but it yields `i32`
  --> $DIR/assoc-type-in-superbad.rs:12:16
   |
LL |     type Key = u32;
   |                ^^^ expected `u32`, found `i32`
   |
note: required by a bound in `Foo`
  --> $DIR/assoc-type-in-superbad.rs:7:25
   |
LL | pub trait Foo: Iterator<Item = <Self as Foo>::Key> {
   |                         ^^^^^^^^^^^^^^^^^^^^^^^^^ required by this bound in `Foo`

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0271`.
";
    assert_eq!(ui("assoc-type-in-superbad.rs", "2015", source), stderr);
}

#[test]
#[ignore = "needs a const call's `[const] Trait` bound to reach the solver as a HostEffect goal; a body's const calls are checked by `effects::non_const_calls` instead"]
fn an_impl_that_is_not_const_is_suggested_const() {
    // tests/ui/traits/const-traits/const-default-method-bodies.rs
    let source = "//@ compile-flags: -Znext-solver
#![feature(const_trait_impl)]

const trait ConstDefaultFn: Sized {
    fn b(self);

    fn a(self) {
        self.b();
    }
}

struct NonConstImpl;
struct ConstImpl;

impl ConstDefaultFn for NonConstImpl {
    fn b(self) {}
}

const impl ConstDefaultFn for ConstImpl {
    fn b(self) {}
}

const fn test() {
    NonConstImpl.a();
    //~^ ERROR the trait bound `NonConstImpl: [const] ConstDefaultFn` is not satisfied
    ConstImpl.a();
}

fn main() {}
";
    let stderr = "error[E0277]: the trait bound `NonConstImpl: [const] ConstDefaultFn` is not satisfied
  --> $DIR/const-default-method-bodies.rs:24:18
   |
LL |     NonConstImpl.a();
   |                  ^
   |
help: make the `impl` of trait `ConstDefaultFn` `const`
   |
LL | const impl ConstDefaultFn for NonConstImpl {
   | +++++

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0277`.
";
    assert_eq!(ui("const-default-method-bodies.rs", "2015", source), stderr);
}

#[test]
fn an_overflow_assigning_a_variable_to_a_type_holding_it() {
    // tests/ui/traits/mutual-recursion-issue-75860.rs
    let source = "pub fn iso<A, B, F1, F2>(a: F1, b: F2) -> (Box<dyn Fn(A) -> B>, Box<dyn Fn(B) -> A>)
    where
        F1: (Fn(A) -> B) + 'static,
        F2: (Fn(B) -> A) + 'static,
{
    (Box::new(a), Box::new(b))
}
pub fn iso_un_option<A, B>() -> (Box<dyn Fn(A) -> B>, Box<dyn Fn(B) -> A>) {
    let left = |o_a: Option<_>| o_a.unwrap();
    //~^ ERROR overflow
    let right = |o_b: Option<_>| o_b.unwrap();
    iso(left, right)
}

fn main() {}
";
    let stderr = "error[E0275]: overflow assigning `_` to `Option<_>`
  --> $DIR/mutual-recursion-issue-75860.rs:9:33
   |
LL |     let left = |o_a: Option<_>| o_a.unwrap();
   |                                 ^^^^^^^^^^^^

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0275`.
";
    assert_eq!(ui("mutual-recursion-issue-75860.rs", "2015", source), stderr);
}

#[test]
fn an_impl_trait_arguments_bound_and_the_impls_where_clause_are_pointed_at() {
    // tests/ui/traits/next-solver/diagnostics/where-clause-doesnt-apply.rs
    let source = "trait Foo {}
trait Bar {}

impl<T> Foo for T where T: Bar {}
fn needs_foo(_: impl Foo) {}

trait Mirror {
    type Mirror;
}
impl<T> Mirror for T {
    type Mirror = T;
}

// Make sure the `Alias: Foo` bound doesn't \"shadow\" the impl, since the
// impl is really the only candidate we care about here for the purpose
// of error reporting.
fn hello<T>() where <T as Mirror>::Mirror: Foo {
    needs_foo(());
    //~^ ERROR the trait bound `(): Foo` is not satisfied
}

fn main() {}
";
    let stderr = "error[E0277]: the trait bound `(): Foo` is not satisfied
  --> $DIR/where-clause-doesnt-apply.rs:18:15
   |
LL |     needs_foo(());
   |     --------- ^^ the trait `Bar` is not implemented for `()`
   |     |
   |     required by a bound introduced by this call
   |
help: this trait has no implementations, consider adding one
  --> $DIR/where-clause-doesnt-apply.rs:2:1
   |
LL | trait Bar {}
   | ^^^^^^^^^
note: required for `()` to implement `Foo`
  --> $DIR/where-clause-doesnt-apply.rs:4:9
   |
LL | impl<T> Foo for T where T: Bar {}
   |         ^^^     ^          --- unsatisfied trait bound introduced here
note: required by a bound in `needs_foo`
  --> $DIR/where-clause-doesnt-apply.rs:5:22
   |
LL | fn needs_foo(_: impl Foo) {}
   |                      ^^^ required by this bound in `needs_foo`

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0277`.
";
    assert_eq!(ui("where-clause-doesnt-apply.rs", "2015", source), stderr);
}

#[test]
fn the_impls_bound_a_step_went_through_is_counted_past_a_projection() {
    // tests/ui/traits/next-solver/diagnostics/point-at-failing-nested.rs
    let source = "//@ compile-flags: -Znext-solver

trait Foo {}
trait Bar {}
trait Constrain {
    type Output;
}

impl<T, U> Foo for T
where
    T: Constrain<Output = U>,
    U: Bar,
{
}

impl Constrain for () {
    type Output = ();
}

fn needs_foo<T: Foo>() {}
fn main() {
    needs_foo::<()>();
    //~^ ERROR the trait bound `(): Foo` is not satisfied
}
";
    let stderr = "error[E0277]: the trait bound `(): Foo` is not satisfied
  --> $DIR/point-at-failing-nested.rs:22:17
   |
LL |     needs_foo::<()>();
   |                 ^^ the trait `Bar` is not implemented for `()`
   |
help: this trait has no implementations, consider adding one
  --> $DIR/point-at-failing-nested.rs:4:1
   |
LL | trait Bar {}
   | ^^^^^^^^^
note: required for `()` to implement `Foo`
  --> $DIR/point-at-failing-nested.rs:9:12
   |
LL | impl<T, U> Foo for T
   |            ^^^     ^
...
LL |     U: Bar,
   |        --- unsatisfied trait bound introduced here
note: required by a bound in `needs_foo`
  --> $DIR/point-at-failing-nested.rs:20:17
   |
LL | fn needs_foo<T: Foo>() {}
   |                 ^^^ required by this bound in `needs_foo`

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0277`.
";
    assert_eq!(ui("point-at-failing-nested.rs", "2015", source), stderr);
}

#[test]
fn an_implicit_sized_bound_is_noted_and_relaxing_it_suggested() {
    // tests/ui/coercion/fudge-inference/expectated-input-not-satisfying-fn-bounds-issue-149881.rs
    // (both revisions' `.stderr` are this)
    let source = "//@ revisions: current next
//@ ignore-compare-mode-next-solver (explicit revisions)
//@[next] compile-flags: -Znext-solver

// FIXME(#149379): This should pass, but fails due to fudged expactation
// types which are potentially not well-formed or for whom the function
// where-bounds don't actually hold. And this results in weird bugs when
// later treating these expectations as if they were actually correct..

fn id<T>(x: Box<T>) -> Box<T> {
    x
}

fn main() {
    <[_]>::into_vec(id(Box::new([0, 1, 2])));
    //~^ ERROR: the size for values of type `[{integer}]` cannot be known at compilation time
}
";
    let stderr = "error[E0277]: the size for values of type `[{integer}]` cannot be known at compilation time
  --> $DIR/expectated-input-not-satisfying-fn-bounds-issue-149881.rs:15:24
   |
LL |     <[_]>::into_vec(id(Box::new([0, 1, 2])));
   |                     -- ^^^^^^^^^^^^^^^^^^^ doesn't have a size known at compile-time
   |                     |
   |                     required by a bound introduced by this call
   |
   = help: the trait `Sized` is not implemented for `[{integer}]`
note: required by an implicit `Sized` bound in `id`
  --> $DIR/expectated-input-not-satisfying-fn-bounds-issue-149881.rs:10:7
   |
LL | fn id<T>(x: Box<T>) -> Box<T> {
   |       ^ required by the implicit `Sized` requirement on this type parameter in `id`
help: consider relaxing the implicit `Sized` restriction
   |
LL | fn id<T: ?Sized>(x: Box<T>) -> Box<T> {
   |        ++++++++

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0277`.
";
    assert_eq!(ui("expectated-input-not-satisfying-fn-bounds-issue-149881.rs", "2015", source), stderr);
}

#[test]
#[ignore = "needs rustc's \"consider specifying the generic arguments\" with the inferred arguments filled in (`foo::<17, M>`)"]
fn an_undecided_const_argument_points_at_its_const_parameter() {
    // tests/ui/const-generics/infer/one-param-uninferred.rs
    let source = "// Test that we emit an error if we cannot properly infer a constant.
fn foo<const N: usize, const M: usize>() -> [u8; N] {
    todo!()
}

fn main() {
    // FIXME(const_generics): Currently this only suggests one const parameter,
    // but instead it should suggest to provide all parameters.
    let _: [u8; 17] = foo();
                  //~^ ERROR type annotations needed
}
";
    let stderr = "error[E0284]: type annotations needed
  --> $DIR/one-param-uninferred.rs:9:23
   |
LL |     let _: [u8; 17] = foo();
   |                       ^^^ cannot infer the value of the const parameter `M` declared on the function `foo`
   |
note: required by a const generic parameter in `foo`
  --> $DIR/one-param-uninferred.rs:2:24
   |
LL | fn foo<const N: usize, const M: usize>() -> [u8; N] {
   |                        ^^^^^^^^^^^^^^ required by this const generic parameter in `foo`
help: consider specifying the generic arguments
   |
LL |     let _: [u8; 17] = foo::<17, M>();
   |                          +++++++++

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0284`.
";
    assert_eq!(ui("one-param-uninferred.rs", "2015", source), stderr);
}

/// core's `unsafe_binder` macros, as `library/core/src/unsafe_binder.rs` writes them (their
/// `$expr ; $ty` arms left out).
const STD_UNSAFE_BINDER: &str = "//- /std.rs crate:std
#![feature(decl_macro, allow_internal_unstable)]
pub mod unsafe_binder {
    #[allow_internal_unstable(builtin_syntax)]
    pub macro unwrap_binder {
        ($expr:expr) => {
            builtin # unwrap_binder ( $expr )
        },
    }
    #[allow_internal_unstable(builtin_syntax)]
    pub macro wrap_binder {
        ($expr:expr) => {
            builtin # wrap_binder ( $expr )
        },
    }
}
";

#[test]
fn a_cast_of_a_type_that_is_not_an_unsafe_binder() {
    // tests/ui/unsafe-binders/mismatch.rs, its functions `b` and `c`: `a` and `e` write an
    // `unsafe<'a>` type, which blaze does not parse yet, and `d` is E0282's.
    let source = "#![feature(unsafe_binders)]

use std::unsafe_binder::{wrap_binder, unwrap_binder};

fn b() {
    let _: i32 = wrap_binder!(&());
    //~^ ERROR `wrap_binder!()` can only wrap into unsafe binder
}

fn c() {
    let y = 1;
    unwrap_binder!(y);
    //~^ ERROR expected unsafe binder, found integer as input
}

fn main() {}
";
    let stderr = "error: `wrap_binder!()` can only wrap into unsafe binder, not `i32`
  --> $DIR/mismatch.rs:6:18
   |
LL |     let _: i32 = wrap_binder!(&());
   |                  ^^^^^^^^^^^^^^^^^
   |
   = note: unsafe binders are the only valid output of wrap

error: expected unsafe binder, found integer as input of `unwrap_binder!()`
  --> $DIR/mismatch.rs:12:20
   |
LL |     unwrap_binder!(y);
   |                    ^
   |
   = note: only an unsafe binder type can be unwrapped

error: aborting due to 2 previous errors

";
    let fixture = format!("{STD_UNSAFE_BINDER}//- /mismatch.rs crate:main edition:2021 deps:std\n{source}");
    let checked = checked_with(&fixture, "$DIR/", &compiletest(Mode::Other, &Options::default()));
    assert_eq!(normalized(&checked.stderr_with(Style { ui_testing: true, ..Style::default() })), stderr);
}

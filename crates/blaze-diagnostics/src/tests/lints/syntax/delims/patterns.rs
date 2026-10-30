//! Parentheses around a pattern: of a `let`, a `for`, an argument, a match arm and the
//! subpatterns of one.

use crate::{
    Options,
    tests::{ui, ui_run_pass},
};

#[test]
fn a_space_takes_the_place_of_parentheses_after_a_keyword() {
    let source = r#"//@ run-rustfix
#![deny(unused_parens)]

fn main() {
    if let(Some(_))= Some(1) {}
    //~^ ERROR unnecessary parentheses around pattern

    for(_x)in 1..10 {}
    //~^ ERROR unnecessary parentheses around pattern

    if(2 == 1) {}
    //~^ ERROR unnecessary parentheses around `if` condition

    // reported by parser
    for(_x in 1..10) {}
    //~^ ERROR unexpected parentheses surrounding
}
"#;
    let expected = r#"error: unexpected parentheses surrounding `for` loop head
  --> $DIR/issue-103435-extra-parentheses.rs:15:8
   |
LL |     for(_x in 1..10) {}
   |        ^           ^
   |
help: remove parentheses in `for` loop
   |
LL -     for(_x in 1..10) {}
LL +     for _x in 1..10 {}
   |

error: unnecessary parentheses around pattern
  --> $DIR/issue-103435-extra-parentheses.rs:5:11
   |
LL |     if let(Some(_))= Some(1) {}
   |           ^       ^
   |
note: the lint level is defined here
  --> $DIR/issue-103435-extra-parentheses.rs:2:9
   |
LL | #![deny(unused_parens)]
   |         ^^^^^^^^^^^^^
help: remove these parentheses
   |
LL -     if let(Some(_))= Some(1) {}
LL +     if let Some(_) = Some(1) {}
   |

error: unnecessary parentheses around pattern
  --> $DIR/issue-103435-extra-parentheses.rs:8:8
   |
LL |     for(_x)in 1..10 {}
   |        ^  ^
   |
help: remove these parentheses
   |
LL -     for(_x)in 1..10 {}
LL +     for _x in 1..10 {}
   |

error: unnecessary parentheses around `if` condition
  --> $DIR/issue-103435-extra-parentheses.rs:11:7
   |
LL |     if(2 == 1) {}
   |       ^      ^
   |
help: remove these parentheses
   |
LL -     if(2 == 1) {}
LL +     if 2 == 1 {}
   |

error: aborting due to 4 previous errors

"#;
    assert_eq!(ui("issue-103435-extra-parentheses.rs", "2015", source), expected);
}

/// What a macro wrote is linted without a suggestion, once for each expansion.
#[test]
fn parentheses_a_macro_wrote_are_linted_in_each_expansion() {
    let source = r#"//@ run-pass

#![warn(unused_parens)]
#![allow(dead_code)]

trait Foo {
    fn bar();
    fn tar();
}

macro_rules! unused_parens {
    ($ty:ident) => {
        impl<$ty: Foo> Foo for ($ty,) {
            fn bar() { <$ty>::bar() }
            fn tar() {}
        }
    };

    ($ty:ident $(, $rest:ident)*) => {
        impl<$ty: Foo, $($rest: Foo),*> Foo for ($ty, $($rest),*) {
            fn bar() {
                <$ty>::bar();
                <($($rest),*)>::bar() //~WARN unnecessary parentheses around type
            }
            fn tar() {
              let (_t) = 1; //~WARN unnecessary parentheses around pattern
                            //~| WARN unnecessary parentheses around pattern
              let (_t1,) = (1,);
              let (_t2, _t3) = (1, 2);
            }
        }

        unused_parens!($($rest),*);
    }
}

unused_parens!(T1, T2, T3);

fn main() {}
"#;
    let expected = r#"warning: unnecessary parentheses around pattern
  --> $DIR/unused-parens-in-macro-issue-120642.rs:26:19
   |
LL |               let (_t) = 1;
   |                   ^^^^
...
LL | unused_parens!(T1, T2, T3);
   | -------------------------- in this macro invocation
   |
note: the lint level is defined here
  --> $DIR/unused-parens-in-macro-issue-120642.rs:3:9
   |
LL | #![warn(unused_parens)]
   |         ^^^^^^^^^^^^^
   = note: this warning originates in the macro `unused_parens` (in Nightly builds, run with -Z macro-backtrace for more info)

warning: unnecessary parentheses around type
  --> $DIR/unused-parens-in-macro-issue-120642.rs:23:18
   |
LL |                 <($($rest),*)>::bar()
   |                  ^^^^^^^^^^^^
...
LL | unused_parens!(T1, T2, T3);
   | -------------------------- in this macro invocation
   |
   = note: this warning originates in the macro `unused_parens` (in Nightly builds, run with -Z macro-backtrace for more info)

warning: unnecessary parentheses around pattern
  --> $DIR/unused-parens-in-macro-issue-120642.rs:26:19
   |
LL |               let (_t) = 1;
   |                   ^^^^
...
LL | unused_parens!(T1, T2, T3);
   | -------------------------- in this macro invocation
   |
   = note: this warning originates in the macro `unused_parens` (in Nightly builds, run with -Z macro-backtrace for more info)

warning: 3 warnings emitted

"#;
    assert_eq!(ui_run_pass("unused-parens-in-macro-issue-120642.rs", "2015", source, &Options::default()), expected);
}

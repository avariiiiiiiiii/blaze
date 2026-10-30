//! `raw_borrows_via_references`: rustc's `tests/ui/lint/lint-raw-borrows-via-references.rs`.

use super::{Ui, ui};

#[test]
fn lint_raw_borrows_via_references() {
    let source = r##"//@ check-pass
//@ run-rustfix
//@ rustfix-only-machine-applicable

#![allow(dead_code, unused_variables)]
#![warn(raw_borrows_via_references)]

struct A {
    a: i32,
    b: u8,
}

fn via_ref(x: *const (i32, i32)) -> *const i32 {
    unsafe { &(*x).0 as *const i32 }
    //~^ WARN creating an intermediate reference implies aliasing requirements even when immediately cast to a raw pointers [raw_borrows_via_references]
}

fn via_ref_struct(x: *const A) -> *const u8 {
    unsafe { &(*x).b as *const u8 }
    //~^ WARN creating an intermediate reference implies aliasing requirements even when immediately cast to a raw pointers [raw_borrows_via_references]
}

fn via_ref_mut(x: *mut (i32, i32)) -> *mut i32 {
    unsafe { &mut (*x).0 as *mut i32 }
    //~^ WARN creating an intermediate reference implies aliasing requirements even when immediately cast to a raw pointers [raw_borrows_via_references]
}

fn via_ref_struct_mut(x: *mut A) -> *mut i32 {
    unsafe { &mut (*x).a as *mut i32 }
    //~^ WARN creating an intermediate reference implies aliasing requirements even when immediately cast to a raw pointers [raw_borrows_via_references]
}

fn multiple_casts(x: *const (i32, i32)) -> *const u8 {
    unsafe { &(*x).0 as *const i32 as *const u8 }
    //~^ WARN creating an intermediate reference implies aliasing requirements even when immediately cast to a raw pointers [raw_borrows_via_references]
}

fn ret_i32() -> i32 {
    0
}

fn temporaries() {
    let _ = &1 as *const i32;
    let _ = &(1 + 2) as *const i32;
    let _ = &ret_i32() as *const i32;
    let _ = &(1, 2) as *const (i32, i32);
    let _ = &[1, 2, 3] as *const [i32; 3];
    let _ = &A { a: 0, b: 0 } as *const A;
    let _ = &mut 4 as *mut i32;
}

fn inner_blocks() {
    let x = 0;
    let _ = { &x as *const i32 };
    //~^ WARN creating an intermediate reference implies aliasing requirements even when immediately cast to a raw pointers [raw_borrows_via_references]

    let _ = {
        let y = 0;
        { &y as *const i32 }
        //~^ WARN creating an intermediate reference implies aliasing requirements even when immediately cast to a raw pointers [raw_borrows_via_references]
    };

    let _ = { &1 as *const i32 };
}

macro_rules! ref_cast {
    ($e:expr) => {
        &$e as *const i32
        //~^ WARN creating an intermediate reference implies aliasing requirements even when immediately cast to a raw pointers [raw_borrows_via_references]
    };
}

fn from_macro(x: *const i32) -> *const i32 {
    unsafe { ref_cast!(*x) }
}

fn main() {
    let a = 0;
    let a = &a as *const i32;
    //~^ WARN creating an intermediate reference implies aliasing requirements even when immediately cast to a raw pointers [raw_borrows_via_references]

    let mut b = 0;
    let b = &mut b as *mut i32;
    //~^ WARN creating an intermediate reference implies aliasing requirements even when immediately cast to a raw pointers [raw_borrows_via_references]

    let i = &1 as *const i32;
}
"##;
    let expected = r##"warning: creating an intermediate reference implies aliasing requirements even when immediately cast to a raw pointers
  --> $DIR/lint-raw-borrows-via-references.rs:14:14
   |
LL |     unsafe { &(*x).0 as *const i32 }
   |              ^^^^^^^^^^^^^^^^^^^^^
   |
note: the lint level is defined here
  --> $DIR/lint-raw-borrows-via-references.rs:6:9
   |
LL | #![warn(raw_borrows_via_references)]
   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^
help: consider using `&raw const` for a safer and more explicit raw pointer
   |
LL -     unsafe { &(*x).0 as *const i32 }
LL +     unsafe { &raw const (*x).0 }
   |

warning: creating an intermediate reference implies aliasing requirements even when immediately cast to a raw pointers
  --> $DIR/lint-raw-borrows-via-references.rs:19:14
   |
LL |     unsafe { &(*x).b as *const u8 }
   |              ^^^^^^^^^^^^^^^^^^^^
   |
help: consider using `&raw const` for a safer and more explicit raw pointer
   |
LL -     unsafe { &(*x).b as *const u8 }
LL +     unsafe { &raw const (*x).b }
   |

warning: creating an intermediate reference implies aliasing requirements even when immediately cast to a raw pointers
  --> $DIR/lint-raw-borrows-via-references.rs:24:14
   |
LL |     unsafe { &mut (*x).0 as *mut i32 }
   |              ^^^^^^^^^^^^^^^^^^^^^^^
   |
help: consider using `&raw mut` for a safer and more explicit raw pointer
   |
LL -     unsafe { &mut (*x).0 as *mut i32 }
LL +     unsafe { &raw mut (*x).0 }
   |

warning: creating an intermediate reference implies aliasing requirements even when immediately cast to a raw pointers
  --> $DIR/lint-raw-borrows-via-references.rs:29:14
   |
LL |     unsafe { &mut (*x).a as *mut i32 }
   |              ^^^^^^^^^^^^^^^^^^^^^^^
   |
help: consider using `&raw mut` for a safer and more explicit raw pointer
   |
LL -     unsafe { &mut (*x).a as *mut i32 }
LL +     unsafe { &raw mut (*x).a }
   |

warning: creating an intermediate reference implies aliasing requirements even when immediately cast to a raw pointers
  --> $DIR/lint-raw-borrows-via-references.rs:34:14
   |
LL |     unsafe { &(*x).0 as *const i32 as *const u8 }
   |              ^^^^^^^^^^^^^^^^^^^^^
   |
help: consider using `&raw const` for a safer and more explicit raw pointer
   |
LL -     unsafe { &(*x).0 as *const i32 as *const u8 }
LL +     unsafe { &raw const (*x).0 as *const u8 }
   |

warning: creating an intermediate reference implies aliasing requirements even when immediately cast to a raw pointers
  --> $DIR/lint-raw-borrows-via-references.rs:54:15
   |
LL |     let _ = { &x as *const i32 };
   |               ^^^^^^^^^^^^^^^^
   |
help: consider using `&raw const` for a safer and more explicit raw pointer
   |
LL -     let _ = { &x as *const i32 };
LL +     let _ = { &raw const x };
   |

warning: creating an intermediate reference implies aliasing requirements even when immediately cast to a raw pointers
  --> $DIR/lint-raw-borrows-via-references.rs:59:11
   |
LL |         { &y as *const i32 }
   |           ^^^^^^^^^^^^^^^^
   |
help: consider using `&raw const` for a safer and more explicit raw pointer
   |
LL -         { &y as *const i32 }
LL +         { &raw const y }
   |

warning: creating an intermediate reference implies aliasing requirements even when immediately cast to a raw pointers
  --> $DIR/lint-raw-borrows-via-references.rs:68:10
   |
LL |         &$e as *const i32
   |          ^^^^^^^^^^^^^^^^
...
LL |     unsafe { ref_cast!(*x) }
   |              ------------- in this macro invocation
   |
   = help: consider using `&raw const` for a safer and more explicit raw pointer
   = note: this warning originates in the macro `ref_cast` (in Nightly builds, run with -Z macro-backtrace for more info)

warning: creating an intermediate reference implies aliasing requirements even when immediately cast to a raw pointers
  --> $DIR/lint-raw-borrows-via-references.rs:79:13
   |
LL |     let a = &a as *const i32;
   |             ^^^^^^^^^^^^^^^^
   |
help: consider using `&raw const` for a safer and more explicit raw pointer
   |
LL -     let a = &a as *const i32;
LL +     let a = &raw const a;
   |

warning: creating an intermediate reference implies aliasing requirements even when immediately cast to a raw pointers
  --> $DIR/lint-raw-borrows-via-references.rs:83:13
   |
LL |     let b = &mut b as *mut i32;
   |             ^^^^^^^^^^^^^^^^^^
   |
help: consider using `&raw mut` for a safer and more explicit raw pointer
   |
LL -     let b = &mut b as *mut i32;
LL +     let b = &raw mut b;
   |

warning: 10 warnings emitted

"##;
    let stderr = ui(Ui {
        name: "lint-raw-borrows-via-references.rs",
        core: "add, sized",
        source,
        ..Ui::default()
    });
    assert_eq!(stderr, expected);
}

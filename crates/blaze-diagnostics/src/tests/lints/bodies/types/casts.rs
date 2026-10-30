//! rustc's UI tests of the late lint passes of casts and borrows of statics and places in
//! bodies. compiletest gives `-A unused -W unused_attributes` to every UI test but a
//! `//@ run-pass` one (`runtest.rs`, `allow_unused`), so those are [`Test::Passes`] here.

mod function_casts;
mod provenance;
mod raw_borrows;
mod static_mut_refs;

use super::{Test, Ui, ui};

//! rustc's UI tests of the late lint passes of casts and borrows of statics and places in
//! bodies. A test with no core is [`crate::tests::ui`]'s. One with minicore or a std is
//! [`ui`]'s, whose `-A unused` goes to [`Test::Passes`]; compiletest gives it to every UI test
//! but a `//@ run-pass` one (`runtest.rs`, `allow_unused`), so every such test but a
//! `run-pass` one is [`Test::Passes`] here.

mod function_casts;
mod provenance;
mod raw_borrows;
mod static_mut_refs;

use super::{Test, Ui, ui};

//! rustc's UI tests of the late lint passes of values, numbers, pointers and casts in bodies.

mod casts;
mod limits;
mod pointers;
mod values;

pub(crate) use super::calls::{Test, Ui, ui};

/// [`ui`] of a failing UI test without compile flags that uses std: core is minicore with
/// `flags` (`crates/test-utils/src/minicore.rs`), and `std` a crate of core's prelude and
/// `std_items`, the items of std the test uses, written as std writes them, their
/// `#[rustc_diagnostic_item]`s included. The test's file is the UI test's source byte for byte,
/// so the lines and columns are its `.stderr`'s.
pub(crate) fn ui_std(name: &str, edition: &str, flags: &str, std_items: &str, source: &str) -> String {
    ui(Ui { name, edition, core: flags, std: Some(("", std_items)), source, ..Ui::default() })
}

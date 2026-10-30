//! rustc's UI tests of the late lint passes of values, numbers, pointers and casts in bodies.

mod casts;
mod limits;
mod pointers;
mod values;

use std::fmt::Write as _;

pub(crate) use crate::tests::Mode;
use crate::{Options, emit::Style, lints::CommandLine};

/// A UI test: its file's name and edition, what its crate is built with, and its source byte
/// for byte, so that the lines and columns are its `.stderr`'s.
pub(crate) struct Ui<'a> {
    pub(crate) name: &'a str,
    pub(crate) edition: &'a str,
    /// [`Mode::RunPass`] for a `//@ run-pass` test, which compiletest gives no `-A unused`.
    pub(crate) mode: Mode,
    /// minicore's flags (`crates/test-utils/src/minicore.rs`) when the test needs core.
    pub(crate) core: &'a str,
    /// What std's prelude has beyond core's that the test uses, and the items of std it uses,
    /// written as std writes them (their `#[rustc_diagnostic_item]`s and `#[lang]`s too), when
    /// it uses std; std needs `core`.
    pub(crate) std: Option<(&'a str, &'a str)>,
    /// The test's own `//@ compile-flags` of lints: `-D unused-comparisons`.
    pub(crate) flags: &'a [&'a str],
    pub(crate) source: &'a str,
}

impl Default for Ui<'_> {
    fn default() -> Self {
        Ui { name: "", edition: "2015", mode: Mode::Other, core: "", std: None, flags: &[], source: "" }
    }
}

/// What rustc writes to stderr for `ui` run as compiletest runs it
/// ([`crate::tests::compiletest`]): its `.stderr`.
pub(crate) fn ui(ui: Ui<'_>) -> String {
    let mut fixture = String::new();
    if !ui.core.is_empty() {
        writeln!(fixture, "//- minicore: {}", ui.core).unwrap();
    }
    let deps = match ui.std {
        Some((prelude, items)) => {
            assert!(!ui.core.is_empty(), "std's prelude is core's: give minicore's flags");
            fixture.push_str("//- /std.rs crate:std\npub mod prelude {\n");
            for edition in ["rust_2015", "rust_2018", "rust_2021", "rust_2024"] {
                writeln!(
                    fixture,
                    "    pub mod {edition} {{ pub use core::prelude::{edition}::*; pub use crate::__prelude::*; }}"
                )
                .unwrap();
            }
            writeln!(fixture, "}}\n#[doc(hidden)]\npub mod __prelude {{\n{prelude}\n}}\n{items}").unwrap();
            " deps:std"
        }
        None => "",
    };
    write!(fixture, "//- /{} crate:other:main edition:{}{deps}\n{}", ui.name, ui.edition, ui.source).unwrap();
    let args: Vec<String> = ui.flags.iter().flat_map(|flag| flag.split(' ')).map(str::to_owned).collect();
    let lints = CommandLine::parse(&args).expect("the test's flags are rustc's");
    let options = crate::tests::compiletest(ui.mode, &Options { lints, ..Options::default() });
    let stderr = crate::tests::checked_with(&fixture, "$DIR/", &options)
        .stderr_with(Style { ui_testing: true, ..Style::default() });
    crate::tests::normalized(&stderr)
}

/// [`ui`] of a UI test without compile flags that uses std: core is minicore with `flags`, and
/// `std` a crate of core's prelude and `std_items`, the items of std the test uses.
pub(crate) fn ui_std(name: &str, edition: &str, flags: &str, std_items: &str, source: &str) -> String {
    ui(Ui { name, edition, core: flags, std: Some(("", std_items)), source, ..Ui::default() })
}

//! rustc's UI tests of the late lint passes of values, numbers, pointers and casts in bodies.

mod casts;
mod limits;
mod pointers;
mod values;

use crate::{Options, emit::Style};

/// The prelude of the `std` of [`ui_std`]: core's, as std's prelude is.
pub(crate) const STD_PRELUDE: &str = "\
pub mod prelude {
    pub mod rust_2015 { pub use core::prelude::rust_2015::*; }
    pub mod rust_2018 { pub use core::prelude::rust_2018::*; }
    pub mod rust_2021 { pub use core::prelude::rust_2021::*; }
    pub mod rust_2024 { pub use core::prelude::rust_2024::*; }
}
";

/// [`crate::tests::ui`] of a UI test that uses std: core is minicore with `flags`
/// (`crates/test-utils/src/minicore.rs`), and `std` a crate of [`STD_PRELUDE`] and `std_items`,
/// the items of std the test uses, written as std writes them, their
/// `#[rustc_diagnostic_item]`s included. The test's file is the UI test's source byte for byte,
/// so the lines and columns are its `.stderr`'s.
pub(crate) fn ui_std(name: &str, edition: &str, flags: &str, std_items: &str, source: &str) -> String {
    ui_std_with(name, edition, flags, std_items, source, &Options::default())
}

/// [`ui_std`] with `options` (a UI test's `//@ compile-flags: -D warnings`).
pub(crate) fn ui_std_with(
    name: &str,
    edition: &str,
    flags: &str,
    std_items: &str,
    source: &str,
    options: &Options,
) -> String {
    let fixture = format!(
        "//- minicore: {flags}\n//- /std.rs crate:std deps:core\n{STD_PRELUDE}{std_items}\n\
         //- /{name} crate:main edition:{edition} deps:std\n{source}"
    );
    let stderr = crate::tests::checked_with(&fixture, "$DIR/", options)
        .stderr_with(Style { ui_testing: true, ..Style::default() });
    crate::tests::normalized(&stderr)
}

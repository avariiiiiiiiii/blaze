//! rustc's UI tests of the errors of items, names, attributes, impls, macros and syntax.

mod attrs;
mod defs;
mod features;
mod generics;
mod impls;
mod krate;
mod macros;
mod names;
mod parse;
mod resolve;

use crate::Options;

/// [`crate::tests::ui`] of a UI test that needs core's items: `flags` are minicore's
/// (`crates/test-utils/src/minicore.rs`), and core is in the prelude as std is for rustc. The
/// file's lines are the source's, as the `.stderr` counts them.
pub(crate) fn ui_core(name: &str, edition: &str, flags: &str, source: &str) -> String {
    let fixture = format!("//- minicore: {flags}\n//- /{name} crate:main edition:{edition}\n{source}");
    crate::tests::checked_with(&fixture, "$DIR/", &Options::default()).stderr()
}

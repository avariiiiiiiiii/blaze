//! rustc's UI tests of the crate's features: `#![feature]`'s own errors (E0554, E0557, E0635,
//! E0725, incompatible and dependent features), `stable_features`, `duplicate_features`; the
//! feature gates of syntax (`gates`) and of attributes (`attributes`).

mod attributes;
mod gates;

use crate::{Options, emit::Style, lower::mutable_ref_binding::BUILT_ON};

/// rustc writes the date it was built on as `YYYY-MM-DD` under `-Z ui-testing`.
fn dated(stderr: String) -> String {
    stderr.replace(&format!("was built on {BUILT_ON};"), "was built on YYYY-MM-DD;")
}

/// [`crate::tests::ui`], dated as rustc's UI tests are.
fn ui(name: &str, edition: &str, source: &str) -> String {
    dated(crate::tests::ui(name, edition, source))
}

/// [`crate::tests::items::ui_core`], dated as rustc's UI tests are.
fn ui_core(name: &str, edition: &str, flags: &str, source: &str) -> String {
    dated(crate::tests::items::ui_core(name, edition, flags, source))
}

/// The `.stderr` of a fixture whose first crate is the UI test, its other crates what the test
/// builds with (an `aux-build`, a stand-in for std's stability attributes), dated.
fn ui_fixture(fixture: &str) -> String {
    let checked = crate::tests::checked_with(fixture, "$DIR/", &Options::default());
    dated(crate::tests::normalized(&checked.stderr_with(Style { ui_testing: true, ..Style::default() })))
}

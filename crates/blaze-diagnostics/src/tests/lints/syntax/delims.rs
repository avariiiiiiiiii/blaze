//! `unused_parens`, `unused_braces` and `unused_import_braces`, against rustc's UI tests.

mod braces;
mod patterns;
mod types;
mod values;

use crate::{Options, emit::Style, lints::CommandLine};

/// The lint flags compiletest gives every UI test (`runtest.rs`, `make_compile_args`), with
/// `-A unused -W unused_attributes` before them for a test that passes. A test of these lints
/// turns them back on with an attribute.
fn compiletest(passes: bool) -> Options {
    let mut args: Vec<&str> = Vec::new();
    if passes {
        args.extend(["-A", "unused", "-W", "unused_attributes"]);
    }
    args.extend(["-A", "internal_features", "-A", "incomplete_features", "-A", "unused_parens", "-A", "unused_braces"]);
    let args: Vec<String> = args.into_iter().map(str::to_owned).collect();
    Options { lints: CommandLine::parse(&args).unwrap() }
}

/// [`crate::tests::ui_with`] of a UI test that uses items of core: core is minicore with
/// `flags` (`crates/test-utils/src/minicore.rs`). The test's file is the UI test's source byte
/// for byte, so the lines and columns are its `.stderr`'s.
fn ui_core(name: &str, edition: &str, flags: &str, source: &str, options: &Options) -> String {
    let fixture = format!("//- minicore: {flags}\n//- /{name} crate:main edition:{edition}\n{source}");
    let stderr =
        crate::tests::checked_with(&fixture, "$DIR/", options).stderr_with(Style { ui_testing: true, ..Style::default() });
    crate::tests::normalized(&stderr)
}

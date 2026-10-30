//! The rest of the early lint pass and of the lints the lexer, the parser, expansion and
//! resolution buffer, against rustc's UI tests.

mod builtin;
mod labels;
mod lexer;
mod macros;
mod validation;

use crate::{Options, emit::Style, lints::CommandLine};

/// Whether a UI test passes (`//@ check-pass`, `//@ run-pass`): compiletest gives it more flags.
#[derive(Clone, Copy)]
pub(crate) enum Test {
    Passes,
    Fails,
}

/// The lint flags compiletest gives every UI test (`runtest.rs`, `make_compile_args`), with
/// `-A unused -W unused_attributes` before them for a test that passes.
pub(crate) fn compiletest(test: Test) -> Options {
    let mut args: Vec<&str> = Vec::new();
    if let Test::Passes = test {
        args.extend(["-A", "unused", "-W", "unused_attributes"]);
    }
    args.extend(["-A", "internal_features", "-A", "incomplete_features", "-A", "unused_parens", "-A", "unused_braces"]);
    let args: Vec<String> = args.into_iter().map(str::to_owned).collect();
    Options { lints: CommandLine::parse(&args).unwrap(), ..Options::default() }
}

/// What rustc writes to stderr for `fixture`, each file named `$DIR/<path>`: a UI test's
/// `.stderr`.
fn stderr(fixture: &str, test: Test) -> String {
    let stderr = crate::tests::checked_with(fixture, "$DIR/", &compiletest(test))
        .stderr_with(Style { ui_testing: true, ..Style::default() });
    crate::tests::normalized(&stderr)
}

/// [`crate::tests::ui`] under compiletest's flags.
pub(crate) fn ui(name: &str, edition: &str, test: Test, source: &str) -> String {
    stderr(&format!("//- /{name} crate:main edition:{edition}\n{source}"), test)
}

/// [`ui`] of a UI test that needs core's items: core is minicore with `flags`
/// (`crates/test-utils/src/minicore.rs`).
pub(crate) fn ui_core(name: &str, edition: &str, test: Test, flags: &str, source: &str) -> String {
    stderr(&format!("//- minicore: {flags}\n//- /{name} crate:main edition:{edition}\n{source}"), test)
}

/// [`ui_core`] of a UI test that uses std: `std` is a crate of `std_items`, written as std
/// writes them, with a prelude of core's and of `prelude`, what std's prelude adds that the
/// test uses.
pub(crate) fn ui_std(
    name: &str,
    edition: &str,
    test: Test,
    flags: &str,
    std: (&str, &str),
    source: &str,
) -> String {
    let (std_items, prelude) = std;
    let preludes: String = ["rust_2015", "rust_2018", "rust_2021", "rust_2024"]
        .iter()
        .map(|it| format!("    pub mod {it} {{ pub use core::prelude::{it}::*; {prelude} }}\n"))
        .collect();
    let fixture = format!(
        "//- minicore: {flags}\n//- /std.rs crate:std deps:core\npub mod prelude {{\n{preludes}}}\n{std_items}\n\
         //- /{name} crate:main edition:{edition} deps:std\n{source}"
    );
    stderr(&fixture, test)
}

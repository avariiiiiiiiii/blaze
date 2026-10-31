//! The tests' harness: a fixture's crates checked as `blaze build` checks them, and rustc's
//! stderr of a one-file crate, so that an expectation is rustc's `tests/ui/*.stderr` as it is.

mod bodies;
mod check;
mod db;
mod emit;
mod items;
pub(crate) mod lints;

use std::collections::HashMap;

use span::FileId;
use hir_expand::fixture;

use self::db::TestDB;
use crate::{
    Files, Options,
    emit::{Sources, Style},
    model::Diag,
};

/// A fixture checked: its database, the names of its files and what rustc would say.
pub(crate) struct Checked {
    pub(crate) db: TestDB,
    pub(crate) names: HashMap<FileId, String>,
    /// Minicore's file, if the fixture has it: core's, whose source rustc under compiletest
    /// places spans in but doesn't show.
    pub(crate) core: Option<FileId>,
    pub(crate) diags: Vec<Diag>,
}

impl Checked {
    /// The names of the fixture's files, already relative.
    pub(crate) fn files(&self) -> Files<'_> {
        Files::new(&self.names, "")
    }

    /// The sources of the fixture's files, by the names diagnostics give them.
    pub(crate) fn sources(&self) -> Sources {
        let mut sources = Sources::default();
        for (file, name) in &self.names {
            let text = self.db.file_text(*file).text(&self.db).as_str().to_owned();
            match Some(*file) == self.core {
                true => sources.add_unshown(name, text),
                false => sources.add(name, Some(text)),
            }
        }
        sources
    }

    /// What rustc writes to its stderr: each diagnostic, as text, in order.
    pub(crate) fn stderr(&self) -> String {
        self.stderr_with(Style::default())
    }

    pub(crate) fn stderr_with(&self, style: Style) -> String {
        let sources = self.sources();
        self.diags.iter().map(|it| crate::emit::written(it, &sources, style)).collect()
    }

    /// The messages of the diagnostics, with their codes: `E0308: mismatched types`.
    pub(crate) fn messages(&self) -> Vec<String> {
        self.diags
            .iter()
            .map(|it| match &it.code {
                Some(code) => format!("{}: {}", code.as_str(), it.message),
                None => it.message.clone(),
            })
            .collect()
    }
}

/// The paths of a fixture's files, in order, without their leading `/`.
fn paths(fixture: &str) -> Vec<String> {
    fixture
        .lines()
        .filter_map(|line| line.trim_start().strip_prefix("//- "))
        .filter_map(|meta| meta.split_whitespace().next())
        .filter(|path| path.starts_with('/'))
        .map(|path| path.trim_start_matches('/').to_owned())
        .collect()
}

/// The workspace crates of `fixture` checked with `options`, each file named as the fixture
/// names it, with `prefix` before it.
pub(crate) fn checked_with(fixture: &str, prefix: &str, options: &Options) -> Checked {
    let fx = fixture::db(fixture);
    let files = fx.files().to_vec();
    let sysroot = fx.sysroot_files().to_vec();
    let db = fx.into_db();
    let mut names: HashMap<FileId, String> = files
        .iter()
        .zip(paths(fixture))
        .map(|(file, path)| (file.file_id(&db), format!("{prefix}{path}")))
        .collect();
    let (has_core, has_proc_macros) = sysroot_of(fixture);
    let mut sysroot = sysroot.into_iter();
    let core = has_core.then(|| sysroot.next().expect("a fixture with minicore has its file"));
    if let Some(core) = core {
        names.insert(core, CORE_NAME.to_owned());
    }
    if has_proc_macros {
        let proc_macros = sysroot.next().expect("a fixture with proc macros has their crate's file");
        names.insert(proc_macros, format!("{prefix}auxiliary/proc_macros.rs"));
    }
    let diags = hir::attach_db(&db, || {
        hir::check::workspace_crates(&db)
            .into_iter()
            .flat_map(|krate| crate::check::check_crate(&db, krate, Files::new(&names, ""), options))
            .collect()
    });
    Checked { db, names, core, diags }
}

/// The name minicore's file is shown by: core's root as compiletest names it, the library under
/// `$SRC_DIR`.
const CORE_NAME: &str = "$SRC_DIR/core/src/lib.rs";

/// Whether `fixture` has minicore (`//- minicore:`) and the proc macros' crate
/// (`//- proc_macros:`): the files after its own, in that order.
fn sysroot_of(fixture: &str) -> (bool, bool) {
    let says = |meta: &str| fixture.lines().any(|line| line.trim_start().starts_with(meta));
    (says("//- minicore:"), says("//- proc_macros:"))
}

/// A frame for a test that makes its own [`crate::Cx`], as a check's task has one.
pub(crate) fn frame() -> blaze_mem::Frame {
    blaze_mem::Frame::new(blaze_mem::Tag::Diagnostics).expect("a test's frame reserves its region")
}

/// The workspace crates of `fixture`, checked as `blaze build` checks them.
pub(crate) fn checked(fixture: &str) -> Checked {
    checked_with(fixture, "", &Options::default())
}

/// What rustc writes to stderr compiling `source` as the one file of a crate, named
/// `$DIR/<name>` as rustc's UI tests name it, in edition `edition`, as compiletest runs a UI
/// test that is not `//@ run-pass`: a UI test's `.stderr`.
pub(crate) fn ui(name: &str, edition: &str, source: &str) -> String {
    ui_with(name, edition, source, &Options::default())
}

/// [`ui`] with the test's own flags (`//@ compile-flags`) in `options`.
pub(crate) fn ui_with(name: &str, edition: &str, source: &str, options: &Options) -> String {
    ui_as(Mode::Other, name, edition, source, options)
}

/// [`ui_with`] of a `//@ run-pass` test, which compiletest gives no `-A unused`.
pub(crate) fn ui_run_pass(name: &str, edition: &str, source: &str, options: &Options) -> String {
    ui_as(Mode::RunPass, name, edition, source, options)
}

/// The pass mode a UI test declares, as far as compiletest's flags depend on it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Mode {
    RunPass,
    Other,
}

pub(crate) fn ui_as(mode: Mode, name: &str, edition: &str, source: &str, options: &Options) -> String {
    ui_fixture_as(mode, &format!("//- /{name} crate:other:main edition:{edition}\n{source}"), options)
}

/// [`ui_as`] beside core, minicore with `flags` (`crates/test-utils/src/minicore.rs`): what of
/// rustc's core the test reaches, `copy` for a value of a primitive used twice.
pub(crate) fn ui_core_as(
    mode: Mode,
    name: &str,
    edition: &str,
    flags: &str,
    source: &str,
    options: &Options,
) -> String {
    let fixture = format!("//- minicore: {flags}\n//- /{name} crate:other:main edition:{edition}\n{source}");
    ui_fixture_as(mode, &fixture, options)
}

/// [`ui`] beside a core of the builtin macros of core's prelude the UI tests call, as
/// `library/core/src/macros/mod.rs` writes them: `compile_error!` and `#[cfg_accessible]`.
pub(crate) fn ui_builtins(name: &str, edition: &str, source: &str) -> String {
    let fixture = format!("{BUILTINS_CORE}//- /{name} crate:other:main edition:{edition} deps:core\n{source}");
    ui_fixture_as(Mode::Other, &fixture, &Options::default())
}

const BUILTINS_CORE: &str = r#"//- /core.rs crate:core edition:2024
#![feature(decl_macro, rustc_attrs)]
pub mod prelude {
    pub mod v1 {
        pub use crate::compile_error;
        pub use crate::macros::builtin::cfg_accessible;
    }
    pub mod rust_2015 {
        pub use super::v1::*;
    }
    pub mod rust_2018 {
        pub use super::v1::*;
    }
    pub mod rust_2021 {
        pub use super::v1::*;
    }
    pub mod rust_2024 {
        pub use super::v1::*;
    }
}
pub mod macros {
    #[rustc_builtin_macro]
    #[macro_export]
    macro_rules! compile_error {
        ($msg:expr $(,)?) => {{ /* compiler built-in */ }};
    }

    pub mod builtin {
        #[rustc_builtin_macro]
        pub macro cfg_accessible($item:item) {
            /* compiler built-in */
        }
    }
}
"#;

fn ui_fixture_as(mode: Mode, fixture: &str, options: &Options) -> String {
    let stderr = checked_with(fixture, "$DIR/", &compiletest(mode, options))
        .stderr_with(Style { ui_testing: true, ..Style::default() });
    normalized(&stderr)
}

/// The options compiletest runs a UI test with (`runtest.rs`, `make_compile_args`): `-Z
/// ui-testing` (in the style), `-Z deduplicate-diagnostics=no`, and its lint flags before the
/// test's own: `-A unused -W unused_attributes` but for `run-pass`, then `-A
/// internal_features -A incomplete_features -A unused_parens -A unused_braces`.
pub(crate) fn compiletest(mode: Mode, options: &Options) -> Options {
    let mut args: Vec<&str> = Vec::new();
    if mode == Mode::Other {
        args.extend(["-A", "unused", "-W", "unused_attributes"]);
    }
    args.extend(["-A", "internal_features", "-A", "incomplete_features"]);
    args.extend(["-A", "unused_parens", "-A", "unused_braces"]);
    let args: Vec<String> = args.into_iter().map(str::to_owned).collect();
    let mut opts = crate::lints::CommandLine::parse(&args).expect("compiletest's lint flags are rustc's").lints.opts;
    let mut lints = options.lints.clone();
    opts.append(&mut lints.lints.opts);
    lints.lints.opts = opts;
    Options { lints, keep_duplicates: true }
}

/// compiletest's normalization of a UI test's output (`runtest.rs`, `normalize_output`) as far
/// as a one-file crate needs it: tabs shown as `\t`, the test's `//~` annotations removed
/// with the white space before them (its regex `\s*//(\[.*\])?~.*`), and the line and column
/// of a place in the library as `LL:COL`.
pub(crate) fn normalized(output: &str) -> String {
    src_dir_positions(&annotations_removed(output))
}

fn annotations_removed(output: &str) -> String {
    let output = output.replace('\t', "\\t");
    let mut out = String::with_capacity(output.len());
    let mut rest = output.as_str();
    while let Some(at) = rest.find("//") {
        let line_end = rest[at..].find('\n').map_or(rest.len(), |it| at + it);
        let after = &rest[at + 2..line_end];
        let annotation = after.starts_with('~')
            || (after.starts_with('[') && after.match_indices("]~").next().is_some());
        if !annotation {
            out.push_str(&rest[..at + 2]);
            rest = &rest[at + 2..];
            continue;
        }
        out.push_str(rest[..at].trim_end());
        rest = &rest[line_end..];
    }
    out.push_str(rest);
    out
}

/// compiletest's `SRC_DIR(.+):\d+:\d+(: \d+:\d+)?` replaced by `SRC_DIR$1:LL:COL`, on each line:
/// the last line and column after a `SRC_DIR`, and a second pair right after them.
fn src_dir_positions(output: &str) -> String {
    let mut out = String::with_capacity(output.len());
    for line in output.split_inclusive('\n') {
        let Some(from) = line.find("SRC_DIR").map(|it| it + "SRC_DIR".len() + 1) else {
            out.push_str(line);
            continue;
        };
        let digits = |at: usize| {
            let n = line.get(at..)?.bytes().take_while(u8::is_ascii_digit).count();
            (n > 0).then_some(at + n)
        };
        // `\d+:\d+` from `at`: where it ends.
        let pair = |at: usize| {
            let at = digits(at)?;
            line[at..].starts_with(':').then_some(())?;
            digits(at + 1)
        };
        let found = (from..line.len())
            .rev()
            .filter(|&at| line.as_bytes()[at] == b':')
            .find_map(|at| Some((at, pair(at + 1)?)));
        let Some((at, mut end)) = found else {
            out.push_str(line);
            continue;
        };
        if line[end..].starts_with(": ")
            && let Some(second) = pair(end + 2)
        {
            end = second;
        }
        out.push_str(&line[..at]);
        out.push_str(":LL:COL");
        out.push_str(&line[end..]);
    }
    out
}

#[test]
fn library_positions_are_normalized() {
    let output = "  --> $SRC_DIR/core/src/lib.rs:12:4\n  --> $DIR/a.rs:3:1\n";
    assert_eq!(normalized(output), "  --> $SRC_DIR/core/src/lib.rs:LL:COL\n  --> $DIR/a.rs:3:1\n");
}

#[test]
fn annotations_are_removed_with_the_space_before_them() {
    let output = "LL |     f(); //~ ERROR x\nLL |     //~^ ERROR y\n   |     ^\n// not one\tb\n";
    assert_eq!(normalized(output), "LL |     f();\nLL |\n   |     ^\n// not one\\tb\n");
}

#[test]
fn fixture_paths_are_named_without_their_slash() {
    let fixture = "//- /main.rs crate:other:main deps:dep\nfn main() {}\n//- /dep.rs crate:dep\n";
    assert_eq!(paths(fixture), ["main.rs", "dep.rs"]);
}

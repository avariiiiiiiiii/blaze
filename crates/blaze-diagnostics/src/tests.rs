//! The tests' harness: a fixture's crates checked as `blaze build` checks them, and rustc's
//! stderr of a one-file crate, so that an expectation is rustc's `tests/ui/*.stderr` as it is.

mod bodies;
mod check;
mod db;
mod emit;
mod items;
pub(crate) mod lints;

use std::collections::HashMap;

use hir::Semantics;
use span::FileId;
use test_fixture::WithFixture;

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
    pub(crate) diags: Vec<Diag>,
}

impl Checked {
    /// The names of the fixture's files, already relative.
    pub(crate) fn files(&self) -> Files<'_> {
        Files::new(&self.names, "")
    }

    /// The sources of the fixture's files, by the names diagnostics give them.
    pub(crate) fn sources(&self) -> Sources {
        let files = self.names.iter().map(|(file, name)| {
            (name.clone(), self.db.file_text(*file).text(&self.db).to_owned())
        });
        Sources::of(files)
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
    let (db, files) = TestDB::with_many_files(fixture);
    let names: HashMap<FileId, String> = files
        .iter()
        .zip(paths(fixture))
        .map(|(file, path)| (file.file_id(&db), format!("{prefix}{path}")))
        .collect();
    let diags = hir::attach_db(&db, || {
        let sema = Semantics::new(&db);
        hir::check::workspace_crates(&db)
            .into_iter()
            .flat_map(|krate| crate::check::check_crate_with(&sema, krate, Files::new(&names, ""), options))
            .collect()
    });
    Checked { db, names, diags }
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
    let fixture = format!("//- /{name} crate:main edition:{edition}\n{source}");
    let stderr = checked_with(&fixture, "$DIR/", &compiletest(mode, options))
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
    let mut opts = crate::lints::CommandLine::parse(&args).expect("compiletest's lint flags are rustc's").opts;
    let mut lints = options.lints.clone();
    opts.append(&mut lints.opts);
    lints.opts = opts;
    Options { lints, keep_duplicates: true }
}

/// compiletest's normalization of a UI test's output (`runtest.rs`, `normalize_output`) as far
/// as a one-file crate needs it: tabs shown as `\t`, and the test's `//~` annotations removed
/// with the white space before them (its regex `\s*//(\[.*\])?~.*`).
pub(crate) fn normalized(output: &str) -> String {
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

#[test]
fn annotations_are_removed_with_the_space_before_them() {
    let output = "LL |     f(); //~ ERROR x\nLL |     //~^ ERROR y\n   |     ^\n// not one\tb\n";
    assert_eq!(normalized(output), "LL |     f();\nLL |\n   |     ^\n// not one\\tb\n");
}

#[test]
fn fixture_paths_are_named_without_their_slash() {
    let fixture = "//- /main.rs crate:main deps:dep\nfn main() {}\n//- /dep.rs crate:dep\n";
    assert_eq!(paths(fixture), ["main.rs", "dep.rs"]);
}

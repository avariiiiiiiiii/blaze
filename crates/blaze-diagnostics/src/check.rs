//! A crate's diagnostics as rustc gives them compiling it: every error and warning of every
//! item and body, no list of codes to pass over (#1522, #1440, #780), in the order of rustc's
//! passes, stopping where rustc stops, then its closing lines.
//!
//! A file that doesn't parse in a way rustc can't recover from (`FatalError`) ends it there:
//! its errors and the closing lines, nothing of resolution, types or lints.
//!
//! Follows rustc's `compiler/rustc_interface/src/passes.rs` (`run_required_analyses`,
//! `analysis`: no late lint, dead-code or privacy pass after an error that is not a lint's)
//! and `compiler/rustc_errors/src/lib.rs` (`DiagCtxtInner::emit_diagnostic`: a diagnostic
//! emitted again is not written again, nor a `note_once` already written).

use hir::{Semantics, db::HirDatabase};
use rustc_hash::FxHashSet;

use crate::{
    checks,
    cx::{Cx, Files},
    emit, lints,
    lower::{self, Found, Phase},
    model::{Child, Diag},
};

/// What a crate is checked with: the flags of rustc's that change what it says.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// The lint flags rustc is given: cargo's `[lints]` and `RUSTFLAGS`, and `--cap-lints`
    /// for a crate cargo builds as a dependency.
    pub lints: lints::CommandLine,
    /// `-Z deduplicate-diagnostics=no`, as compiletest gives every UI test: a diagnostic
    /// identical to one written is written again, with a note that it is.
    pub keep_duplicates: bool,
}

/// What rustc says compiling `krate`: its diagnostics in the order it writes them, then its
/// closing ones ("aborting due to ...", "For more information about this error, ...").
pub fn check_crate(
    db: &dyn HirDatabase,
    krate: hir::Crate,
    files: &dyn Files,
    options: &Options,
) -> Vec<Diag> {
    let sema = Semantics::new_dyn(db);
    check_crate_with(&sema, krate, files, options)
}

/// [`check_crate`] with semantics the caller holds.
pub fn check_crate_with<'db>(
    sema: &Semantics<'db, dyn HirDatabase>,
    krate: hir::Crate,
    files: &dyn Files,
    options: &Options,
) -> Vec<Diag> {
    let cx = Cx::new(sema, krate, files);
    let parsed = lower::parse::parsed(&cx);
    if parsed.fatal {
        return written(parsed.found.into_iter().map(|it| it.diag).collect(), options);
    }
    let mut found: Vec<Found> = parsed.found;
    for diagnostic in hir::check::check_crate(cx.db, krate, false, None) {
        found.extend(lower::lower(&cx, diagnostic));
    }
    found.extend(checks::check(&cx));
    found.extend(lints::early(&cx, &options.lints));
    found.sort_by_key(|it| it.phase);
    let aborted = found.iter().any(stops_before_late);
    if !aborted {
        let mut late = lints::late(&cx);
        late.iter_mut().for_each(|it| it.phase = Phase::Late);
        found.extend(late);
    }
    written(lints::decide(&cx, &options.lints, found, aborted), options)
}

/// Whether rustc stops before its late passes for `found`: an error that is not a lint's
/// (`has_errors_excluding_lint_errors`). A lint's level is not decided yet, and a lint denied
/// does not stop it either way.
fn stops_before_late(found: &Found) -> bool {
    found.phase < Phase::Late && found.diag.is_error() && !found.diag.is_lint()
}

const DUPLICATE: &str = "duplicate diagnostic emitted due to `-Z deduplicate-diagnostics=no`";

/// What rustc does as it emits each diagnostic: the note of the macro it came from, none
/// written twice (unless `keep_duplicates`: then with a note that it is one), nor a
/// `note_once` already written with another. Gives what is written and, of it, what rustc
/// counts for its closing lines (each diagnostic once: `deduplicated_err_count`).
fn finish(diags: Vec<Diag>, keep_duplicates: bool) -> (Vec<Diag>, Vec<Diag>) {
    let mut seen: FxHashSet<Diag> = FxHashSet::default();
    let mut once: FxHashSet<Child> = FxHashSet::default();
    let mut written = Vec::with_capacity(diags.len());
    let mut counted = Vec::with_capacity(diags.len());
    for mut diag in diags {
        let in_macro = diag.span.primary.iter().any(|it| it.expansion.is_some());
        let noted = diag.children.iter().any(|it| it.message.contains("(in Nightly builds, run with -Z macro-backtrace for more info)"));
        if in_macro && !noted {
            diag.originates_in();
        }
        let already = !seen.insert(diag.clone());
        if already && !keep_duplicates {
            continue;
        }
        diag.children.retain(|child| !child.once || once.insert(child.clone()));
        if already {
            diag.note(DUPLICATE);
        } else {
            counted.push(diag.clone());
        }
        written.push(diag);
    }
    (written, counted)
}

/// `diags` as rustc writes them, then its closing lines.
fn written(diags: Vec<Diag>, options: &Options) -> Vec<Diag> {
    let (mut written, counted) = finish(diags, options.keep_duplicates);
    written.extend(emit::summary(&counted));
    written
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Level, MultiSpan, Span, Sublevel};

    fn at(lo: u32) -> Span {
        Span { file: "src/lib.rs".to_owned(), lo, hi: lo + 1, expansion: None }
    }

    fn found(diag: Diag, phase: Phase) -> Found {
        Found::new(diag, phase)
    }

    #[test]
    fn an_error_stops_rustc_before_its_late_passes() {
        let error = found(Diag::error(Some("E0308"), "mismatched types", at(0)), Phase::Bodies);
        assert!(stops_before_late(&error));
        let lint = found(Diag::lint("unused_must_use", "unused", at(0)), Phase::Mir);
        assert!(!stops_before_late(&lint));
        let mut denied = Diag::lint("non_snake_case", "x", at(0));
        denied.level = Level::Error;
        assert!(!stops_before_late(&found(denied, Phase::Resolve)));
    }

    #[test]
    fn a_diagnostic_is_written_once_and_a_once_note_with_the_first() {
        let note = Child {
            level: Sublevel::Note,
            message: "`#[warn(unused_variables)]` on by default".to_owned(),
            span: MultiSpan::default(),
            once: true,
        };
        let mut a = Diag::lint("unused_variables", "unused variable: `a`", at(0));
        a.children.push(note.clone());
        let mut b = Diag::lint("unused_variables", "unused variable: `b`", at(9));
        b.children.push(note);
        let (diags, counted) = finish(vec![a.clone(), a, b], false);
        assert_eq!(diags.len(), 2);
        assert_eq!(diags[0].children.len(), 1);
        assert!(diags[1].children.is_empty());
        assert_eq!(counted, diags);
    }

    #[test]
    fn a_duplicate_kept_is_noted_and_not_counted() {
        let a = Diag::error(Some("E0425"), "cannot find value `x` in this scope", at(0));
        let (diags, counted) = finish(vec![a.clone(), a.clone()], true);
        assert_eq!(diags.len(), 2);
        assert_eq!(diags[0], a);
        assert_eq!(diags[1].children.last().map(|it| it.message.as_str()), Some(DUPLICATE));
        assert_eq!(counted, [a]);
    }
}

//! A crate's diagnostics as rustc gives them compiling it: every error and warning of every
//! item and body, no list of codes to pass over (#1522, #1440, #780), in the order of rustc's
//! passes, stopping where rustc stops, then its closing lines.
//!
//! A file that doesn't parse in a way rustc can't recover from (`FatalError`) ends it there:
//! its errors and the closing lines, nothing of resolution, types or lints. So does any error
//! marked fatal ([`Diag::stop`]): what rustc would write after it is not written. rustc's
//! stashed warnings ([`Phase::Stashed`]) come last, and only when nothing was an error.
//!
//! Follows rustc's `compiler/rustc_interface/src/passes.rs` (`run_required_analyses`,
//! `analysis`: no late lint, dead-code or privacy pass after an error that is not a lint's)
//! and `compiler/rustc_errors/src/lib.rs` (`DiagCtxtInner::emit_diagnostic`: a diagnostic
//! emitted again is not written again, nor a `note_once` already written).

use std::hash::{Hash, Hasher};

use base_db::Db;
use blaze_ids::radix;
use blaze_mem::{Frame, Tag, frame};
use hir::check::bodies::Plan;
use rustc_hash::FxHashSet;

use crate::{
    checks,
    cx::{Cx, Files},
    emit, lints,
    lower::{self, Found, Phase},
    model::Diag,
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
    db: &Db,
    krate: hir::Crate,
    files: Files<'_>,
    options: &Options,
) -> Vec<Diag> {
    let plan = Plan::new(db, &[krate], &hir::check::every_body);
    frame::task(Tag::Diagnostics, |frame| run(db, krate, files, options, &plan, frame))
}

/// [`check_crate`], its bodies checked by `plan`, a plan of `krate` alone with every body: what
/// a build checks beside its codegen, which tells the plan the bodies it reaches
/// ([`Plan::reached`]) so those are checked first. The same diagnostics as [`check_crate`].
pub fn check_planned(
    db: &Db,
    krate: hir::Crate,
    plan: &Plan<'_, impl Fn(hir::DefWithBody) -> bool + Sync + ?Sized>,
    files: Files<'_>,
    options: &Options,
) -> Vec<Diag> {
    frame::task(Tag::Diagnostics, |frame| run(db, krate, files, options, plan, frame))
}

/// The check of `krate`, its tables in `frame`, its bodies by `plan`.
fn run(
    db: &Db,
    krate: hir::Crate,
    files: Files<'_>,
    options: &Options,
    plan: &Plan<'_, impl Fn(hir::DefWithBody) -> bool + Sync + ?Sized>,
    frame: &Frame,
) -> Vec<Diag> {
    let cx = Cx::new(db, krate, files, frame).with_lints(&options.lints);
    let parsed = lower::parse::parsed(&cx);
    if parsed.fatal {
        // alloc: a file that does not parse, the error path
        return written(parsed.found.into_iter().map(|it| it.diag).collect(), options);
    }
    let mut found: Vec<Found> = parsed.found;
    let mut checked = plan.run(cx.db, false);
    assert_eq!(checked.len(), 1, "a plan of the one crate checked");
    for diagnostic in checked.pop().expect("a plan of one crate") {
        found.extend(lower::lower(&cx, diagnostic));
    }
    found.extend(checks::check(&cx));
    found.extend(lints::early(&cx, &options.lints));
    radix::sort_by_key(&mut found, |it| it.phase as u8);
    if let Some(at) = found.iter().position(|it| it.diag.fatal) {
        found.truncate(at + 1);
        let mut diags = lints::decide(&cx, &options.lints, found, true);
        let stop = diags.iter().position(|it| it.fatal).expect("deciding levels keeps every error");
        diags.truncate(stop + 1);
        return written(diags, options);
    }
    let aborted = found.iter().any(stops_before_late);
    if !aborted {
        let mut late = lints::late(&cx);
        late.iter_mut().filter(|it| it.phase < Phase::Late).for_each(|it| it.phase = Phase::Late);
        found.extend(late);
    }
    let (stashed, found): (Vec<Found>, Vec<Found>) =
        found.into_iter().partition(|it| it.phase == Phase::Stashed);
    let mut diags = lints::decide(&cx, &options.lints, found, aborted);
    diags.extend(unstashed(stashed, &diags));
    written(diags, options)
}

/// rustc's stashed warnings, written at the very end and only when nothing before them was an
/// error, lints denied included.
fn unstashed(stashed: Vec<Found>, written: &[Diag]) -> Vec<Diag> {
    if written.iter().any(Diag::is_error) {
        return Vec::new(); // alloc: empty, allocates nothing
    }
    stashed
        .into_iter()
        .map(|it| {
            assert!(
                it.lint_at.is_none() && !it.diag.is_lint(),
                "a stashed diagnostic is a lint's ({:?}): its level is decided with the others",
                it.diag.code,
            );
            it.diag
        })
        .collect() // alloc: the stashed warnings written; none on a clean crate, which allocates nothing
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
/// `note_once` already written with another. Gives what is written and the indices into it of
/// what rustc counts for its closing lines (each diagnostic once: `deduplicated_err_count`).
fn finish(diags: Vec<Diag>, keep_duplicates: bool) -> (Vec<Diag>, Vec<u32>) {
    let mut seen: FxHashSet<Key> = FxHashSet::default();
    let mut once: FxHashSet<Key> = FxHashSet::default();
    seen.reserve(diags.len());
    let mut written = Vec::with_capacity(diags.len());
    let mut counted = Vec::with_capacity(diags.len());
    for mut diag in diags {
        if diag.span.primary.iter().any(|it| it.expansion.is_some())
            && !diag.children.iter().any(|it| it.message.contains("(in Nightly builds, run with -Z macro-backtrace for more info)"))
        {
            diag.originates_in();
        }
        let already = !seen.insert(key(&diag));
        if already && !keep_duplicates {
            continue;
        }
        diag.children.retain(|child| !child.once || once.insert(key(child)));
        if already {
            diag.note(DUPLICATE);
        } else {
            counted.push(u32::try_from(written.len()).expect("fewer than 2^32 diagnostics"));
        }
        written.push(diag);
    }
    (written, counted)
}

/// rustc's key for a diagnostic already emitted (`DiagCtxtInner::emitted_diagnostics`, a set of
/// `Hash128`): a 128-bit content hash of it, so the set holds 16 bytes per diagnostic rather
/// than a copy of it.
type Key = [u8; 16];

/// BLAKE3 over `it`'s `Hash` form, cut to 128 bits.
fn key(it: &impl Hash) -> Key {
    let mut hasher = Blake3(blaze_ids::b3::Hasher::new());
    it.hash(&mut hasher);
    let mut key = [0; 16];
    key.copy_from_slice(&hasher.0.finalize()[..16]);
    key
}

/// A [`Hasher`] that feeds BLAKE3.
struct Blake3(blaze_ids::b3::Hasher);

impl Hasher for Blake3 {
    fn write(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    fn finish(&self) -> u64 {
        let mut word = [0; 8];
        word.copy_from_slice(&self.0.finalize()[..8]);
        u64::from_le_bytes(word)
    }
}

/// `diags` as rustc writes them, then its closing lines.
fn written(diags: Vec<Diag>, options: &Options) -> Vec<Diag> {
    let (mut written, counted) = finish(diags, options.keep_duplicates);
    let closing = emit::summary(counted.iter().map(|&at| &written[at as usize]));
    written.extend(closing);
    written
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Child, Level, MultiSpan, Span, Sublevel};

    fn at(lo: u32) -> Span {
        Span { file: "src/lib.rs".to_owned(), lo, hi: lo + 1, expansion: None }
    }

    fn found(diag: Diag, phase: Phase) -> Found {
        Found::new(diag, phase)
    }

    fn soft_gate(lo: u32) -> Found {
        let mut diag = Diag::new(Level::Warning, None, "the feature is unstable");
        diag.span = MultiSpan::of(at(lo));
        found(diag, Phase::Stashed)
    }

    #[test]
    fn a_stashed_warning_is_written_only_when_nothing_was_an_error() {
        let warning = Diag::lint("dead_code", "function `f` is never used", at(1));
        let written = unstashed(vec![soft_gate(5)], std::slice::from_ref(&warning));
        assert_eq!(written, [soft_gate(5).diag]);

        let error = Diag::error(Some("E0308"), "mismatched types", at(1));
        assert!(unstashed(vec![soft_gate(5)], &[warning, error]).is_empty());
    }

    #[test]
    fn a_lint_denied_is_an_error_that_drops_the_stashed_warnings() {
        let mut denied = Diag::lint("dead_code", "function `f` is never used", at(1));
        denied.level = Level::Error;
        assert!(unstashed(vec![soft_gate(5)], &[denied]).is_empty());
    }

    #[test]
    #[should_panic(expected = "only an error rustc gives is fatal")]
    fn a_lint_cannot_be_fatal() {
        Diag::lint("dead_code", "function `f` is never used", at(1)).stop();
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
        assert_eq!(counted, [0, 1]);
    }

    #[test]
    fn a_crate_with_nothing_to_say_costs_the_driver_no_allocation() {
        let options = Options::default();
        let (written, used) = blaze_alloc_count::measure(|| written(Vec::new(), &options));
        assert!(written.is_empty());
        assert_eq!(used.allocations, 0, "dedup, once-notes and the closing lines of nothing");
    }

    #[test]
    fn the_dedup_key_is_the_content_and_nothing_else() {
        let a = Diag::error(Some("E0425"), "cannot find value `x` in this scope", at(0));
        assert_eq!(key(&a), key(&a.clone()));
        assert_ne!(key(&a), key(&Diag::error(Some("E0425"), "cannot find value `x` in this scope", at(1))));
        assert_ne!(key(&a), key(&Diag::error(Some("E0425"), "cannot find value `y` in this scope", at(0))));
    }

    #[test]
    fn a_duplicate_kept_is_noted_and_not_counted() {
        let a = Diag::error(Some("E0425"), "cannot find value `x` in this scope", at(0));
        let (diags, counted) = finish(vec![a.clone(), a.clone()], true);
        assert_eq!(diags.len(), 2);
        assert_eq!(diags[0], a);
        assert_eq!(diags[1].children.last().map(|it| it.message.as_str()), Some(DUPLICATE));
        assert_eq!(counted, [0]);
    }
}

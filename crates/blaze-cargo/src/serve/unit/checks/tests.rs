use std::{
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};

use blaze_diagnostics::{Level, Span};

use super::*;

fn at(lo: u32) -> Span {
    Span { file: "src/lib.rs".to_owned(), lo, hi: lo + 1, expansion: None }
}

/// Spins until `flag` is set, or panics naming `what` after a while: two jobs that never run
/// side by side never see each other's flag.
fn wait(flag: &AtomicBool, what: &str) {
    let start = Instant::now();
    while !flag.load(Ordering::Acquire) {
        assert!(start.elapsed() < Duration::from_secs(30), "{what} never ran beside this");
        std::thread::yield_now();
    }
}

#[test]
fn a_check_fails_its_unit_on_an_error_cargo_shows_and_nothing_else() {
    let error = Diag::error(Some("E0308"), "mismatched types", at(0));
    let warning = Diag::lint("dead_code", "function `f` is never used", at(1));
    let mut denied = Diag::lint("non_snake_case", "function `F` should have a snake case name", at(2));
    denied.level = Level::Error;
    let closing = Diag::new(Level::Error, None, "aborting due to 1 previous error");

    assert!(!failed(&[]));
    assert!(!failed(std::slice::from_ref(&warning)));
    assert!(!failed(std::slice::from_ref(&closing)), "a closing line cargo hides is no error of the unit");
    assert!(failed(&[warning.clone(), error]));
    assert!(failed(&[warning, denied]), "a denied lint fails the unit as rustc's error does");
}

#[test]
fn the_check_runs_beside_codegen_and_both_are_waited_for() {
    let (codegen_began, check_began) = (AtomicBool::new(false), AtomicBool::new(false));
    let (diags, made) = beside(
        || {
            wait(&codegen_began, "codegen");
            check_began.store(true, Ordering::Release);
            vec![Diag::error(Some("E0308"), "mismatched types", at(0))]
        },
        || {
            codegen_began.store(true, Ordering::Release);
            wait(&check_began, "the check");
            "made"
        },
    );
    assert_eq!(made, "made");
    assert_eq!(diags.len(), 1);
    assert!(failed(&diags));
}

#[test]
#[should_panic(expected = "a body's check panicked")]
fn a_panic_of_the_check_is_raised_by_its_caller() {
    beside(|| panic!("a body's check panicked"), || ());
}

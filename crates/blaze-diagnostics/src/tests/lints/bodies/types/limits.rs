//! `TypeLimits`: rustc's UI tests of `overflowing_literals` and `unused_comparisons`, in `ui/`
//! byte for byte as `tests/ui/lint/` has them (rust-lang/rust `ad3d0bc141a`, MIT or
//! Apache-2.0).

use super::{Test, Ui, ui};

fn check(name: &str, test: Test, flags: &[&str], source: &str, stderr: &str) {
    let actual = ui(Ui { name, test, flags, source, ..Ui::default() });
    assert_eq!(actual, stderr, "{name}");
}

#[test]
fn lint_type_limits() {
    check(
        "lint-type-limits.rs",
        Test::Fails,
        &["-D unused-comparisons"],
        include_str!("limits/ui/lint-type-limits.rs"),
        include_str!("limits/ui/lint-type-limits.stderr"),
    );
}

#[test]
fn lint_type_limits2() {
    check(
        "lint-type-limits2.rs",
        Test::Fails,
        &["-D unused-comparisons"],
        include_str!("limits/ui/lint-type-limits2.rs"),
        include_str!("limits/ui/lint-type-limits2.stderr"),
    );
}

#[test]
fn lint_type_limits3() {
    check(
        "lint-type-limits3.rs",
        Test::Fails,
        &["-D unused-comparisons"],
        include_str!("limits/ui/lint-type-limits3.rs"),
        include_str!("limits/ui/lint-type-limits3.stderr"),
    );
}

#[test]
fn type_overflow() {
    check(
        "type-overflow.rs",
        Test::Passes,
        &[],
        include_str!("limits/ui/type-overflow.rs"),
        include_str!("limits/ui/type-overflow.stderr"),
    );
}

#[test]
fn lint_type_overflow2() {
    check(
        "lint-type-overflow2.rs",
        Test::Fails,
        &[],
        include_str!("limits/ui/lint-type-overflow2.rs"),
        include_str!("limits/ui/lint-type-overflow2.stderr"),
    );
}

#[test]
fn lint_overflowing_int_136675() {
    check(
        "lint-overflowing-int-136675.rs",
        Test::Fails,
        &[],
        include_str!("limits/ui/lint-overflowing-int-136675.rs"),
        include_str!("limits/ui/lint-overflowing-int-136675.stderr"),
    );
}

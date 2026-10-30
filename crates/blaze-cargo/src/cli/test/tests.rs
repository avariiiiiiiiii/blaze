use std::{ffi::OsString, io, path::Path};

use blaze_api::{Artifact, TestOutcome, TestResult};

use super::*;
use crate::cli::{
    args::{Command, Invocation, command_line},
    run::{Exec, tests::artifact},
    say::Ended,
};

/// Ends each executable as `ends` says by its file name, and the rest with 0.
#[derive(Default)]
pub(crate) struct Fake {
    pub ends: Vec<(&'static str, Ended)>,
    pub ran: Vec<Exec>,
}

impl Programs for Fake {
    fn run(&mut self, exec: &Exec) -> io::Result<Ended> {
        self.ran.push(exec.clone());
        let name = exec.program.file_name().unwrap().to_str().unwrap().to_owned();
        Ok(self.ends.iter().find(|(it, _)| *it == name).map_or(Ended::Code(0), |(_, ended)| *ended))
    }
}

/// A package `foo` with a lib, a bin and a test `x`, each tested, and its doc tests; out of
/// cargo's order.
pub(crate) fn built() -> Vec<Artifact> {
    vec![
        artifact("foo", "test", "x", true, Some("deps/x-4567")),
        artifact("foo", "lib", "foo", false, None),
        Artifact { kind: "doctest".to_owned(), ..artifact("foo", "lib", "foo", true, Some("deps/foo-doctests")) },
        artifact("foo", "bin", "foo", true, Some("deps/foo-89ab")),
        artifact("foo", "lib", "foo", true, Some("deps/foo-0123abcd")),
        artifact("foo", "bin", "foo", false, Some("foo")),
    ]
}

fn asked(command: Command, words: &[&str]) -> Invocation {
    command_line(command, words.iter().map(OsString::from).collect()).unwrap()
}

fn speaking() -> Speaking<'static> {
    Speaking { cwd: Path::new("/w"), color: false, quiet: false, verbose: 0 }
}

fn tested(asked: &Invocation, fake: &mut Fake, speaking: Speaking<'_>) -> (i32, String) {
    let mut err = Vec::new();
    let code = run(asked, &built(), 1, speaking, &mut err, fake).unwrap();
    (code, String::from_utf8(err).unwrap())
}

#[test]
fn rerun_names_the_package_as_cargos_needs_spec_flag_does() {
    assert!(!needs_spec_flag(&asked(Command::Test, &[]), 1));
    assert!(needs_spec_flag(&asked(Command::Test, &[]), 2), "a workspace of two");
    assert!(needs_spec_flag(&asked(Command::Test, &["-p", "foo"]), 1));
    assert!(needs_spec_flag(&asked(Command::Test, &["--workspace", "--exclude", "bar"]), 1));

    let mut fake = Fake { ends: vec![("foo-0123abcd", Ended::Code(101))], ..Fake::default() };
    let mut err = Vec::new();
    let code = run(&asked(Command::Test, &[]), &built(), 3, speaking(), &mut err, &mut fake).unwrap();
    assert_eq!(code, 101);
    assert!(String::from_utf8(err).unwrap().ends_with("error: test failed, to rerun pass `-p foo --lib`\n"));
}

#[test]
fn executables_in_cargos_order() {
    let built = built();
    let (tests, docs) = executables(&built);
    let shown: Vec<String> = tests.iter().map(|it| running(it, Path::new("/w"), Kind::Test)).collect();
    assert_eq!(shown, [
        "unittests src/lib.rs (target/debug/deps/foo-0123abcd)",
        "unittests src/main.rs (target/debug/deps/foo-89ab)",
        "tests/x.rs (target/debug/deps/x-4567)",
    ]);
    assert_eq!(docs.len(), 1);
    assert_eq!(running(tests[0], Path::new("/w"), Kind::Bench), "benches src/lib.rs (target/debug/deps/foo-0123abcd)");
}

#[test]
fn every_one_passes() {
    let mut fake = Fake::default();
    let asked = asked(Command::Test, &["filt", "--", "--exact"]);
    let (code, err) = tested(&asked, &mut fake, speaking());
    assert_eq!(code, 0);
    assert_eq!(
        err,
        "     Running unittests src/lib.rs (target/debug/deps/foo-0123abcd)\n     Running unittests \
         src/main.rs (target/debug/deps/foo-89ab)\n     Running tests/x.rs (target/debug/deps/x-4567)\n   \
         Doc-tests foo\n"
    );
    let first = &fake.ran[0];
    assert_eq!(first.program, Path::new("/w/target/debug/deps/foo-0123abcd"));
    assert_eq!(first.args, [OsString::from("filt"), OsString::from("--exact")]);
    assert_eq!(first.cwd, Path::new("/w/foo"));
    assert_eq!(first.env, [("CARGO_PKG_NAME".to_owned(), "foo".to_owned())]);
    assert_eq!(fake.ran.len(), 4);
}

#[test]
fn the_first_failure_stops_the_run() {
    let mut fake = Fake { ends: vec![("foo-0123abcd", Ended::Code(101))], ..Fake::default() };
    let (code, err) = tested(&asked(Command::Test, &[]), &mut fake, speaking());
    assert_eq!(code, 101);
    assert_eq!(fake.ran.len(), 1);
    assert_eq!(
        err,
        "     Running unittests src/lib.rs (target/debug/deps/foo-0123abcd)\nerror: test failed, to rerun pass `--lib`\n"
    );
}

#[test]
fn no_fail_fast_runs_them_all() {
    let ends = vec![("foo-0123abcd", Ended::Code(101)), ("x-4567", Ended::Code(101))];
    let mut fake = Fake { ends, ..Fake::default() };
    let (code, err) = tested(&asked(Command::Test, &["--no-fail-fast"]), &mut fake, speaking());
    assert_eq!(code, 101);
    assert_eq!(fake.ran.len(), 4);
    assert!(err.contains("error: test failed, to rerun pass `--test x`\n   Doc-tests foo\n"), "{err}");
    assert!(err.ends_with("error: 2 targets failed:\n    `--lib`\n    `--test x`\n"), "{err}");

    let mut fake = Fake { ends: vec![("foo-doctests", Ended::Code(101))], ..Fake::default() };
    let (code, err) = tested(&asked(Command::Test, &["-p", "foo"]), &mut fake, speaking());
    assert_eq!(code, 101);
    assert!(err.ends_with("error: doctest failed, to rerun pass `-p foo --doc`\n"), "{err}");
}

#[cfg(unix)]
#[test]
fn a_test_that_ended_otherwise() {
    let mut fake = Fake { ends: vec![("foo-0123abcd", Ended::Code(3))], ..Fake::default() };
    let (code, err) = tested(&asked(Command::Test, &[]), &mut fake, speaking());
    assert_eq!(code, 3);
    assert!(
        err.ends_with(
            "error: test failed, to rerun pass `--lib`\n\nCaused by:\n  process didn't exit successfully: \
             `/w/target/debug/deps/foo-0123abcd` (exit status: 3)\nnote: test exited abnormally; to see the \
             full output pass --no-capture to the harness.\n"
        ),
        "{err}"
    );

    let mut fake = Fake { ends: vec![("foo-0123abcd", Ended::Signal(libc::SIGSEGV))], ..Fake::default() };
    let (code, err) = tested(&asked(Command::Test, &["--", "--nocapture"]), &mut fake, speaking());
    assert_eq!(code, 101);
    assert!(
        err.ends_with(
            "(signal: 11, SIGSEGV: invalid memory reference)\n"
        ),
        "{err}"
    );
}

#[test]
fn no_run_lists_the_executables() {
    let mut fake = Fake::default();
    let (code, err) = tested(&asked(Command::Test, &["--no-run"]), &mut fake, speaking());
    assert_eq!(code, 0);
    assert!(fake.ran.is_empty());
    assert_eq!(
        err,
        "  Executable unittests src/lib.rs (target/debug/deps/foo-0123abcd)\n  Executable unittests \
         src/main.rs (target/debug/deps/foo-89ab)\n  Executable tests/x.rs (target/debug/deps/x-4567)\n"
    );
}

#[test]
fn quiet_and_bench() {
    let mut fake = Fake::default();
    let (code, err) = tested(&asked(Command::Bench, &["-q"]), &mut fake, Speaking { quiet: true, ..speaking() });
    assert_eq!(code, 0);
    assert_eq!(err, "");
    // No doc tests for bench; libtest's `--bench`, and `--quiet` to a harness.
    assert_eq!(fake.ran.len(), 3);
    assert_eq!(fake.ran[0].args, [OsString::from("--bench"), OsString::from("--quiet")]);
}

fn result(binary: &str, name: &str, outcome: TestOutcome, output: &str) -> TestResult {
    let millis = if outcome == TestOutcome::Ignored { 0 } else { 5 };
    TestResult { binary: binary.to_owned(), name: name.to_owned(), outcome, output: output.as_bytes().to_vec(), millis, cached: None }
}

#[test]
fn reached_results_in_libtests_format() {
    let results = [
        result("/w/target/debug/deps/foo-0123abcd", "a", TestOutcome::Passed, ""),
        result("/w/target/debug/deps/foo-0123abcd", "b", TestOutcome::Failed, "thread 'b' panicked\n"),
        result("/w/target/debug/deps/foo-0123abcd", "c", TestOutcome::Unreached, ""),
        result("/w/target/debug/deps/x-4567", "d", TestOutcome::Ignored, ""),
    ];
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let passed = reached(&results, speaking(), &mut out, &mut err).unwrap();
    assert!(!passed);
    assert_eq!(
        String::from_utf8(err).unwrap(),
        "     Running target/debug/deps/foo-0123abcd\n     Running target/debug/deps/x-4567\n"
    );
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "\nrunning 2 tests\ntest a ... ok\ntest b ... FAILED\n\nfailures:\n\n---- b stdout ----\nthread 'b' \
         panicked\n\n\nfailures:\n    b\n\ntest result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 \
         filtered out; 1 unreached; finished in 0.01s\n\n\nrunning 1 test\ntest d ... ignored\n\ntest result: \
         ok. 0 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s\n\n"
    );
}

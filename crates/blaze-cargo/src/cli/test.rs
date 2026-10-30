//! `test` and `bench` after the build, as cargo's `ops::run_tests` and `run_benches` run them
//! (`ops/cargo_test.rs`): each test executable in cargo's order, then the doc tests, stopping
//! at the first that fails unless `--no-fail-fast`; `--no-run` lists them. And blaze's
//! `--reached` (an extension, opt-in): the daemon's [`TestResult`]s in libtest's format.

use std::{
    ffi::OsString,
    fmt::Write as _,
    io::{self, Write},
    path::{Path, PathBuf},
};

use blaze_api::{Artifact, TestOutcome, TestResult};
use rustc_hash::FxHashMap;

use super::{
    FAILED,
    args::{Command, Invocation},
    run::{Exec, caused},
    say,
};

/// What runs a test executable: the process itself, or the tests' fake.
pub trait Programs {
    fn run(&mut self, exec: &Exec) -> io::Result<say::Ended>;
}

/// Runs each program on the terminal, its output inherited.
pub struct Spawned;

impl Programs for Spawned {
    fn run(&mut self, exec: &Exec) -> io::Result<say::Ended> {
        exec.command().status().map(say::Ended::of)
    }
}

/// cargo's `TestKind`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Test,
    Bench,
    Doctest,
}

impl Kind {
    fn failed(self) -> &'static str {
        match self {
            Kind::Test => "test failed",
            Kind::Bench => "bench failed",
            Kind::Doctest => "doctest failed",
        }
    }
}

/// The order of cargo's `TargetKind`: lib, bin, test, bench, example, build script.
fn rank(artifact: &Artifact) -> u8 {
    match artifact.target.kind.first().map(String::as_str) {
        Some("bin") => 1,
        Some("test") => 2,
        Some("bench") => 3,
        Some("example") => 4,
        Some("custom-build") => 5,
        _ => 0,
    }
}

/// The test executables and the doc tests the build placed, each in cargo's order (its
/// `compilation.tests` sorted: by package, then target).
pub fn executables(artifacts: &[Artifact]) -> (Vec<&Artifact>, Vec<&Artifact>) {
    let key = |it: &Artifact| {
        let id = it.package.as_str();
        (say::package_name(id), say::package_version(id), id, rank(it), it.target.name.as_str())
    };
    let mut tests: Vec<&Artifact> =
        artifacts.iter().filter(|it| it.executable.is_some() && it.profile.test && it.kind != "doctest").collect();
    let mut docs: Vec<&Artifact> = artifacts.iter().filter(|it| it.kind == "doctest" && it.executable.is_some()).collect();
    tests.sort_by(|a, b| key(a).cmp(&key(b)));
    docs.sort_by(|a, b| key(a).cmp(&key(b)));
    (tests, docs)
}

/// The package's directory: cargo runs a test there.
fn package_root(artifact: &Artifact) -> PathBuf {
    Path::new(&artifact.manifest_path).parent().map_or_else(PathBuf::new, Path::to_owned)
}

/// `unittests src/lib.rs (target/debug/deps/foo-0123abcd)`, `tests/x.rs (target/debug/deps/x-4567)`:
/// cargo's `exe_display` of `cmd_builds`.
pub fn running(artifact: &Artifact, cwd: &Path, kind: Kind) -> String {
    let source = Path::new(&artifact.target.src_path);
    let root = package_root(artifact);
    let source = source.strip_prefix(&root).unwrap_or(source).display();
    let exe = say::shown(Path::new(artifact.executable.as_deref().unwrap_or(&artifact.path)), cwd);
    match artifact.target.kind.first().map(String::as_str) {
        Some("test" | "bench") => format!("{source} ({exe})"),
        _ => {
            let unit = if kind == Kind::Bench { "benches" } else { "unittests" };
            format!("{unit} {source} ({exe})")
        }
    }
}

/// What cargo says to pass to rerun one that failed (`UnitTestError::cli_args`).
pub fn rerun(artifact: &Artifact, kind: Kind, spec_flag: bool) -> String {
    let mut args = match spec_flag {
        true => format!("-p {} ", say::package_name(&artifact.package)),
        false => String::new(),
    };
    let name = &artifact.target.name;
    if kind == Kind::Doctest {
        args.push_str("--doc");
        return args;
    }
    match artifact.target.kind.first().map(String::as_str) {
        Some("bin") => args.push_str(&format!("--bin {name}")),
        Some("test") => args.push_str(&format!("--test {name}")),
        Some("bench") => args.push_str(&format!("--bench {name}")),
        Some("example") => args.push_str(&format!("--example {name}")),
        _ => args.push_str("--lib"),
    }
    args
}

/// How the command speaks.
#[derive(Clone, Copy, Debug)]
pub struct Speaking<'a> {
    pub cwd: &'a Path,
    pub color: bool,
    pub quiet: bool,
    pub verbose: u32,
    /// `--message-format` is one of the JSON ones.
    pub json: bool,
}

impl Speaking<'_> {
    fn status(&self, err: &mut dyn Write, word: &str, rest: &str) -> io::Result<()> {
        match self.quiet {
            true => Ok(()),
            false => err.write_all(say::status(word, rest, self.color).as_bytes()),
        }
    }

    /// `Running` or `Executable` and what runs: cargo's `exe_display` (`concise`), or under `-v`
    /// the command itself (`verbose`).
    fn running(&self, err: &mut dyn Write, word: &str, artifact: &Artifact, kind: Kind, shown: &str) -> io::Result<()> {
        match self.verbose {
            0 => self.status(err, word, &running(artifact, self.cwd, kind)),
            _ => self.status(err, word, shown),
        }
    }
}

/// One that failed: what reruns it, and the code it ended with.
struct Failed {
    rerun: String,
    code: Option<i32>,
}

/// cargo's `Packages::needs_spec_flag`: whether "to rerun pass" names the package. `members` is
/// the workspace's member count, which stands for cargo's default members too.
pub fn needs_spec_flag(asked: &Invocation, members: u32) -> bool {
    !asked.flags.packages.is_empty() || !asked.flags.excludes.is_empty() || members > 1
}

/// `test` or `bench` once the build placed `artifacts`, of a workspace of `members`: cargo's
/// exit code.
pub fn run(
    asked: &Invocation,
    artifacts: &[Artifact],
    members: u32,
    speaking: Speaking<'_>,
    err: &mut dyn Write,
    programs: &mut dyn Programs,
) -> io::Result<i32> {
    let bench = asked.command() == Command::Bench;
    let unit_kind = if bench { Kind::Bench } else { Kind::Test };
    let (tests, docs) = executables(artifacts);
    let spec_flag = needs_spec_flag(asked, members);
    // cargo's `display_no_run_information`, which JSON leaves out: its artifacts name them.
    if asked.flags.no_run {
        if !speaking.json {
            for test in &tests {
                let exec = exec_of(test, asked, speaking);
                speaking.running(err, "Executable", test, unit_kind, &exec.shown())?;
            }
        }
        return Ok(0);
    }
    let mut failed = Vec::new();
    for test in &tests {
        let exec = exec_of(test, asked, speaking);
        let shown = exec.shown_at(speaking.verbose);
        speaking.running(err, "Running", test, unit_kind, &shown)?;
        if let Some(failure) = ran(test, unit_kind, &exec, &shown, asked, spec_flag, speaking, err, programs)? {
            failed.push(failure);
            if !asked.flags.no_fail_fast {
                return Ok(failed[0].code.unwrap_or(FAILED));
            }
        }
    }
    if !bench {
        for doc in &docs {
            let exec = exec_of(doc, asked, speaking);
            let shown = exec.shown_at(speaking.verbose);
            speaking.status(err, "Doc-tests", &doc.target.name)?;
            if speaking.verbose > 0 {
                speaking.status(err, "Running", &shown)?;
            }
            if let Some(failure) = ran(doc, Kind::Doctest, &exec, &shown, asked, spec_flag, speaking, err, programs)? {
                failed.push(failure);
                if !asked.flags.no_fail_fast {
                    return Ok(failed[0].code.unwrap_or(FAILED));
                }
            }
        }
    }
    // cargo's `no_fail_fast_err`: under `--no-fail-fast` any failure is its own error, code 101.
    if failed.is_empty() {
        return Ok(0);
    }
    let mut said = String::new();
    let _ = match failed.len() {
        1 => write!(said, "1 target failed:"),
        n => write!(said, "{n} targets failed:"),
    };
    for failure in &failed {
        said.push_str("\n    `");
        said.push_str(&failure.rerun);
        said.push('`');
    }
    err.write_all(say::error(&said, speaking.color).as_bytes())?;
    Ok(FAILED)
}

/// A test executable as cargo runs it: in its package's directory, with the filter and what
/// follows `--`, and libtest's `--quiet` under `-q`.
fn exec_of(artifact: &Artifact, asked: &Invocation, speaking: Speaking<'_>) -> Exec {
    let mut args = asked.test_args();
    if speaking.quiet && artifact.harness {
        args.push(OsString::from("--quiet"));
    }
    let executable = PathBuf::from(artifact.executable.as_deref().unwrap_or(&artifact.path));
    Exec::of(artifact, executable, args, package_root(artifact))
}

/// Runs one, `shown` as the command; what failed, said as cargo's `report_test_error` says it.
#[expect(clippy::too_many_arguments, reason = "one run of a test, and everything cargo's report of it names")]
fn ran(
    artifact: &Artifact,
    kind: Kind,
    exec: &Exec,
    shown: &str,
    asked: &Invocation,
    spec_flag: bool,
    speaking: Speaking<'_>,
    err: &mut dyn Write,
    programs: &mut dyn Programs,
) -> io::Result<Option<Failed>> {
    let rerun = rerun(artifact, kind, spec_flag);
    let what = format!("{}, to rerun pass `{rerun}`", kind.failed());
    let ended = match programs.run(exec) {
        Ok(ended) => ended,
        Err(error) => {
            let never = format!("could not execute process {shown} (never executed)");
            err.write_all(caused(&what, &[never, error.to_string()], speaking.color).as_bytes())?;
            return Ok(Some(Failed { rerun, code: None }));
        }
    };
    let code = match ended {
        say::Ended::Code(0) => return Ok(None),
        say::Ended::Code(code) => Some(code),
        say::Ended::Signal(_) => None,
    };
    if code == Some(FAILED) {
        err.write_all(say::error(&what, speaking.color).as_bytes())?;
        return Ok(Some(Failed { rerun, code }));
    }
    let cause = format!("process didn't exit successfully: {shown} ({})", say::ended(ended));
    err.write_all(caused(&what, &[cause], speaking.color).as_bytes())?;
    let nocapture = asked.program.iter().any(|it| it == "--nocapture" || it == "--no-capture");
    if code.is_some() && artifact.harness && !nocapture && !speaking.quiet {
        let mut said = String::new();
        say::note_into(&mut said, "test exited abnormally; to see the full output pass --no-capture to the harness.", speaking.color);
        err.write_all(said.as_bytes())?;
    }
    Ok(Some(Failed { rerun, code }))
}

/// `blaze test --reached`: the results the daemon sent, by test executable in the order they
/// came, as libtest's pretty format prints them; whether every one that ran passed. A test the
/// edit did not reach is not run, and is named as such (blaze's word, not libtest's).
pub fn reached(results: &[TestResult], speaking: Speaking<'_>, out: &mut dyn Write, err: &mut dyn Write) -> io::Result<bool> {
    let mut at: FxHashMap<&str, u32> = FxHashMap::default();
    let mut binaries: Vec<(&str, Vec<&TestResult>)> = Vec::new();
    for result in results {
        let index = *at.entry(result.binary.as_str()).or_insert_with(|| {
            binaries.push((result.binary.as_str(), Vec::new()));
            (binaries.len() - 1) as u32
        });
        binaries[index as usize].1.push(result);
    }
    let mut passed_all = true;
    for (binary, of) in binaries {
        speaking.status(err, "Running", &say::shown(Path::new(binary), speaking.cwd))?;
        let (text, passed) = libtest(&of, speaking.color);
        out.write_all(text.as_bytes())?;
        passed_all &= passed;
    }
    out.flush()?;
    Ok(passed_all)
}

/// One executable's results as libtest's `PrettyFormatter` writes them.
pub fn libtest(results: &[&TestResult], color: bool) -> (String, bool) {
    let paint = |text: &mut String, style: &str, said: &str| {
        if color {
            text.push_str(style);
        }
        text.push_str(said);
        if color {
            text.push_str("\x1b[0m");
        }
    };
    let (mut passed, mut failed, mut ignored, mut unreached, mut millis) = (0usize, 0usize, 0usize, 0usize, 0u64);
    let mut names = 0;
    for result in results {
        match result.outcome {
            TestOutcome::Passed => passed += 1,
            TestOutcome::Failed => failed += 1,
            TestOutcome::Ignored => ignored += 1,
            TestOutcome::Unreached => unreached += 1,
        }
        if result.outcome != TestOutcome::Unreached {
            millis += result.millis;
            names += result.name.len();
        }
    }
    let ran = results.len() - unreached;
    let noun = if ran == 1 { "test" } else { "tests" };
    let mut text = String::with_capacity(names + ran * 32 + 160);
    // Writing to a String cannot fail.
    let _ = write!(text, "\nrunning {ran} {noun}\n");
    for result in results {
        let (style, said) = match result.outcome {
            TestOutcome::Passed => ("\x1b[32m", "ok"),
            TestOutcome::Failed => ("\x1b[31m", "FAILED"),
            TestOutcome::Ignored => ("\x1b[33m", "ignored"),
            TestOutcome::Unreached => continue,
        };
        text.push_str("test ");
        text.push_str(&result.name);
        text.push_str(" ... ");
        paint(&mut text, style, said);
        text.push('\n');
    }
    if failed > 0 {
        text.push_str("\nfailures:\n");
        let failures = || results.iter().filter(|it| it.outcome == TestOutcome::Failed);
        if failures().any(|it| !it.output.is_empty()) {
            text.push('\n');
            for failure in failures().filter(|it| !it.output.is_empty()) {
                text.push_str("---- ");
                text.push_str(&failure.name);
                text.push_str(" stdout ----\n");
                text.push_str(&String::from_utf8_lossy(&failure.output));
                text.push('\n');
            }
        }
        text.push_str("\nfailures:\n");
        let mut names: Vec<&str> = failures().map(|it| it.name.as_str()).collect();
        names.sort_unstable();
        for name in names {
            text.push_str("    ");
            text.push_str(name);
            text.push('\n');
        }
    }
    let ok = failed == 0;
    text.push_str("\ntest result: ");
    match ok {
        true => paint(&mut text, "\x1b[32m", "ok"),
        false => paint(&mut text, "\x1b[31m", "FAILED"),
    }
    let _ = write!(text, ". {passed} passed; {failed} failed; {ignored} ignored; 0 measured; 0 filtered out");
    if unreached > 0 {
        let _ = write!(text, "; {unreached} unreached");
    }
    let _ = write!(text, "; finished in {:.2}s\n\n", millis as f64 / 1000.0);
    (text, ok)
}

#[cfg(test)]
pub(crate) mod tests;

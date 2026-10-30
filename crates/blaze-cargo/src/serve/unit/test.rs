//! The tests of a build ([`Front::tests`](crate::serve::work::Front::tests)), as
//! `blaze_usercode::test` runs them: each test asked for ([`select::wanted`]) whose pass the
//! store does not keep for the code it reaches and what it read ([`select::select`]) runs in
//! a process of its own, forked from its binary's server, on one queue over every binary: in
//! the sandbox where the workspace opts in, outside it otherwise (docs/cargo.md); the rest
//! are reported unreached, unless every test is asked for. Each result is kept
//! ([`select::record`]; a test run outside never is) and reported as a
//! [`Response::TestResult`].
//!
//! `blaze test --ub` compiles nothing: [`ub_tests`] lists each test unit's tests as its harness
//! would, and [`evaluate`] runs one in blaze's MIR evaluator under the checker of undefined
//! behavior (`hir_ty_ub::run_test`).

#![forbid(unsafe_code)]

mod traced;

#[cfg(test)]
mod tests;

use std::{
    ffi::OsString,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use blaze_api::{ApiError, Artifact, Basis, Cached, Response, TestOutcome, TestResult, Ub};
use blaze_cas::Store;
use blaze_core::{Completion, Effect, FileName, Tag};
use blaze_usercode::{
    Reached,
    test::{
        self, Attrs, Binary, Config, Decision, Filter, Finished, Ignored, Job as TestJob, Mode as TestMode,
        Outcome, Queue, Reported, ShouldPanic, Suite, Test, Wanted, select,
    },
};
use blaze_units::schema::{Mode, Unit};
use load_cargo::v2::Db;

use super::{Job, Real, UnitReader, UnitWriter, compile::TestOf, graph::flag, package_of};
use crate::serve::work::{Failed, Runs, Selection};

/// How long one test may run: libtest sets no limit, so a day, past which it is surely hung.
const TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
/// How long a test's server is given to end a test it was told to kill.
const KILL_GRACE: Duration = Duration::from_secs(5);

/// The unit an artifact is of: the test of its package's target, by the graph's hash of it.
pub(crate) fn unit_of_artifact(job: &Job, artifact: &Artifact) -> Option<usize> {
    let all = &job.units().graph.units;
    let target = &artifact.target;
    job.graph.find(all, &artifact.package, &target.name, &target.kind, Mode::Test).map(|it| it as usize)
}

/// The target as selection names it: `lib`, `test:parse`, `bin:blaze`.
pub(crate) fn target_name(unit: &Unit) -> String {
    let kind = unit.target.kind.first().map(String::as_str).unwrap_or("");
    match kind {
        "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro" => "lib".to_owned(),
        kind => format!("{kind}:{}", unit.target.name),
    }
}

pub(crate) fn attrs(test: &TestOf) -> Attrs {
    Attrs { ignore: test.ignore, should_panic: should_panic(&test.should_panic), bench: test.bench }
}

/// `#[should_panic]`, with its `expected` message if it gives one.
fn should_panic(of: &Option<Option<String>>) -> ShouldPanic {
    match of {
        None => ShouldPanic::No,
        Some(None) => ShouldPanic::Yes,
        Some(Some(message)) => ShouldPanic::WithMessage(message.clone()),
    }
}

/// The tests `selection` asks for, as libtest selects them: its names (those before `--` and
/// the free arguments after it), `--exact`, `--skip NAME`, `--ignored` and `--include-ignored`
/// (libtest's `cli::parse_opts`). Any other argument is refused, naming it: none is dropped.
pub(crate) fn filter(selection: &Selection<'_>) -> Result<Filter, ApiError> {
    let mut filter = Filter { names: selection.filter.to_vec(), ..Filter::default() };
    let (mut ignored, mut include) = (false, false);
    let mut args = selection.args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--exact" => filter.exact = true,
            "--ignored" => ignored = true,
            "--include-ignored" => include = true,
            "--skip" => match args.next() {
                Some(name) => filter.skip.push(name.clone()),
                None => return Err(ApiError::new("argument to option 'skip' missing", None)),
            },
            arg => match arg.strip_prefix("--skip=") {
                Some(name) => filter.skip.push(name.to_owned()),
                None if !arg.starts_with('-') => filter.names.push(arg.to_owned()),
                None => {
                    return Err(ApiError::new(
                        format!(
                            "`{arg}` after `--`: blaze runs the tests itself and takes libtest's \
                             selection only (test names, `--exact`, `--skip NAME`, `--ignored`, \
                             `--include-ignored`)"
                        ),
                        None,
                    ));
                }
            },
        }
    }
    filter.ignored = match (include, ignored) {
        (true, true) => {
            return Err(ApiError::new("the options --include-ignored and --ignored are mutually exclusive", None));
        }
        (true, false) => Ignored::Include,
        (false, true) => Ignored::Only,
        (false, false) => Ignored::Skip,
    };
    Ok(filter)
}

/// `blaze test --ub`'s tests, selected once by [`ub_tests`], each run by [`evaluate`].
pub(crate) struct UbTests {
    tests: Vec<UbTest>,
    /// Each test unit's would-be executable, as `Running` names it, by [`UbTest::binary`].
    binaries: Vec<String>,
    /// `tests` by key, sorted: [`evaluate`] finds its test in `O(log n)` with nothing allocated.
    by_key: Vec<u32>,
}

impl UbTests {
    fn find(&self, key: &str) -> Option<&UbTest> {
        let at = self.by_key.binary_search_by(|it| self.tests[*it as usize].key.as_str().cmp(key)).ok()?;
        Some(&self.tests[self.by_key[at] as usize])
    }
}

struct UbTest {
    /// What the daemon names it by: its package, its target as selection names it and its
    /// name, apart by spaces, which none of the three has.
    key: String,
    /// Its unit's executable, in [`UbTests::binaries`].
    binary: u32,
    name: String,
    function: hir_def::FunctionId,
    should_panic: ShouldPanic,
    bench: bool,
    /// `#[ignore]`d and not asked for: reported ignored, not run, as libtest does.
    ignored: bool,
}

/// The tests of every test unit `selection` asks for, by key ([`UbTest::key`]): unit by unit in
/// the build's order, each unit's by name as libtest sorts them. Its harness's listing
/// ([`super::codegen::test_roots`]) gives the same names and attributes as a compiled test
/// binary's.
pub(crate) fn ub_tests(job: &Job, reader: &UnitReader, selection: Selection<'_>) -> Result<Vec<String>, Failed> {
    if !selection.all {
        return Err(ApiError::new(
            "`blaze test --ub` runs every test it selects: which tests a change reached is known from \
             their compiled code, and `--ub` compiles nothing",
            None,
        )
        .into());
    }
    let filter = filter(&selection)?;
    let units = job.units();
    let (mut tests, mut binaries) = (Vec::new(), Vec::new());
    for at in 0..job.graph.len() {
        if !job.graph.has(at as u32, flag::TEST) {
            continue;
        }
        if job.cancelled() {
            return Err(Failed::Again);
        }
        let unit = job.unit(at)?;
        let package = package_of(units, unit)?;
        let binary = u32::try_from(binaries.len()).expect("under 2^32 units");
        binaries.push(super::artifact::executable(unit, job.plan(at)?).unwrap_or_else(|| unit.target.name.clone()));
        let target = target_name(unit);
        let roots = super::codegen::test_roots(job, reader, at).map_err(|what| ApiError::new(what, Some(unit.target.name.clone())))?;
        let first = tests.len();
        tests.reserve(roots.tests.len());
        for listed in &roots.tests {
            let attrs = Attrs { ignore: listed.ignore, should_panic: should_panic(&listed.should_panic), bench: listed.bench };
            let ignored = match select::asked(&listed.name, &attrs, &filter) {
                Wanted::Filtered => continue,
                Wanted::Ignored => true,
                Wanted::Yes => false,
            };
            tests.push(UbTest {
                key: format!("{} {target} {}", package.name, listed.name),
                binary,
                name: listed.name.clone(),
                function: listed.function,
                should_panic: attrs.should_panic,
                bench: listed.bench,
                ignored,
            });
        }
        blaze_ids::names::sort_by_name(&mut tests[first..], |it| it.name.as_bytes());
    }
    let mut by_key: Vec<u32> = (0..u32::try_from(tests.len()).expect("under 2^32 tests")).collect();
    blaze_ids::names::sort_by_name_in(&mut by_key, |&it| tests[it as usize].key.as_bytes());
    if let Some(twice) = by_key.windows(2).find(|it| tests[it[0] as usize].key == tests[it[1] as usize].key) {
        let test = &tests[twice[0] as usize];
        let binary = &binaries[test.binary as usize];
        return Err(ApiError::new(format!("two test units list test `{}` of `{binary}`", test.name), None).into());
    }
    let keys = tests.iter().map(|it| it.key.clone()).collect();
    if job.ub.set(UbTests { tests, binaries, by_key }).is_err() {
        return Err(ApiError::new("`blaze test --ub` selected its tests twice in one job", None).into());
    }
    Ok(keys)
}

/// Test `key` of [`ub_tests`] run in blaze's MIR evaluator under `ub` on `db`, the task's
/// shared front end, nothing compiled; a bench is refused, naming it (libtest runs one once with a
/// `Bencher`, which the evaluator has no part of).
pub(crate) fn evaluate(job: &Job, db: &Db, key: &str, ub: Ub) -> Result<TestResult, Failed> {
    let selected = job.ub.get();
    let test = selected
        .and_then(|it| it.find(key))
        .ok_or_else(|| ApiError::new(format!("`blaze test --ub` selected no test `{key}`"), None))?;
    let binary = selected.map_or("", |it| it.binaries[test.binary as usize].as_str());
    let result = |outcome, output: String, millis| TestResult {
        name: test.name.clone(),
        binary: binary.to_owned(),
        cached: None,
        millis,
        output: output.into_bytes(),
        outcome,
    };
    if test.ignored {
        return Ok(result(TestOutcome::Ignored, String::new(), 0));
    }
    if test.bench {
        let what = "a `#[bench]` function: blaze's MIR evaluator runs `#[test]` functions, and has no `Bencher`\n";
        return Ok(result(TestOutcome::Failed, what.to_owned(), 0));
    }
    let began = Instant::now();
    let outcome = hir_ty_ub::run_test(db, test.function, &ub_config(ub));
    let millis = u64::try_from(began.elapsed().as_millis()).unwrap_or(u64::MAX);
    let ran = match &outcome {
        hir_ty_ub::Outcome::Passed { .. } => Ran::Returned,
        hir_ty_ub::Outcome::Failed { message, .. } => Ran::Panicked(message),
        hir_ty_ub::Outcome::Ub { report, seed, .. } => Ran::Ub { report, seed: *seed },
        hir_ty_ub::Outcome::Unsupported { what } => Ran::Unsupported(what),
        hir_ty_ub::Outcome::Error { message, seed } => Ran::Stopped { message, seed: *seed },
    };
    let (outcome, output) = judged(ran, &test.should_panic);
    Ok(result(outcome, output, millis))
}

/// `blaze test --ub`'s settings as the checker takes them: what the request leaves out is the
/// checker's default.
pub(crate) fn ub_config(ub: Ub) -> hir_ty_ub::Config {
    let default = hir_ty_ub::Config::default();
    hir_ty_ub::Config {
        seed: ub.seed.unwrap_or(default.seed),
        schedules: ub.schedules.unwrap_or(default.schedules),
        ..default
    }
}

/// How a test ended in the evaluator.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Ran<'a> {
    Returned,
    /// It panicked with this message.
    Panicked(&'a str),
    /// Undefined behavior, as the checker reports it, and the seed that replays it.
    Ub { report: &'a str, seed: u64 },
    /// What the evaluator can't run.
    Unsupported(&'a str),
    /// The evaluator stopped with an error of its own (no layout, a MIR lowering error, a stack
    /// overflow, undefined behavior it found itself), on the schedule of `seed`.
    Stopped { message: &'a str, seed: u64 },
}

/// Whether a test that ran as `ran` passed, as libtest's `calc_result` judges it with its
/// `#[should_panic]`, and its output, in libtest's words. Undefined behavior and what the
/// evaluator can't run fail whatever the test expects: never a pass and never a run of
/// compiled code in its place.
pub(crate) fn judged(ran: Ran<'_>, expects: &ShouldPanic) -> (TestOutcome, String) {
    match (ran, expects) {
        (Ran::Returned, ShouldPanic::No) => (TestOutcome::Passed, String::new()),
        (Ran::Returned, _) => (TestOutcome::Failed, "note: test did not panic as expected\n".to_owned()),
        (Ran::Panicked(message), ShouldPanic::No) => (TestOutcome::Failed, format!("{message}\n")),
        (Ran::Panicked(_), ShouldPanic::Yes) => (TestOutcome::Passed, String::new()),
        (Ran::Panicked(message), ShouldPanic::WithMessage(expected)) if message.contains(expected.as_str()) => {
            (TestOutcome::Passed, String::new())
        }
        (Ran::Panicked(message), ShouldPanic::WithMessage(expected)) => (
            TestOutcome::Failed,
            format!(
                "note: panic did not contain expected string\n      panic message: {message:?}\n expected substring: {expected:?}\n"
            ),
        ),
        (Ran::Ub { report, seed }, _) => (TestOutcome::Failed, format!("{report}\nseed: {seed}\n")),
        (Ran::Unsupported(what), _) => {
            (TestOutcome::Failed, format!("blaze's MIR evaluator can't run this test: {what}\n"))
        }
        (Ran::Stopped { message, seed }, _) => {
            (TestOutcome::Failed, format!("blaze's MIR evaluator stopped: {message}\nseed: {seed}\n"))
        }
    }
}

fn result(report: test::Report) -> TestResult {
    TestResult {
        binary: report.binary,
        name: report.name,
        cached: None,
        outcome: match report.outcome {
            Reported::Passed => TestOutcome::Passed,
            Reported::Failed => TestOutcome::Failed,
            Reported::Ignored => TestOutcome::Ignored,
            Reported::Unreached => TestOutcome::Unreached,
        },
        output: report.output,
        millis: report.millis,
    }
}

/// A test not run, as its pass is kept for its code and what it read: the pass stands, and
/// took `took` when it ran. Passes are kept by what codegen's walk says the test reaches, as
/// no test binary has an execution record blaze keeps a pass by yet.
fn cached(binary: &str, name: &str, took: Option<Duration>) -> TestResult {
    let millis = took.map_or(0, |it| u64::try_from(it.as_millis()).unwrap_or(u64::MAX));
    TestResult {
        binary: binary.to_owned(),
        name: name.to_owned(),
        cached: Some(Cached {
            basis: Basis::Reached { why: REACHED.to_owned() },
            millis,
            outcome: TestOutcome::Passed,
        }),
        outcome: TestOutcome::Unreached,
        output: Vec::new(),
        millis: 0,
    }
}

/// Why a kept pass is by what its test reaches: [`Basis::Reached`]'s reason.
const REACHED: &str = "selected by the code it reaches: blaze keeps no execution record of a test binary yet";

/// One binary's tests as the queue and selection see them.
struct Suited {
    name: String,
    given: blaze_ids::ContentId,
    listed: Listed,
    /// Handed to the queue when it starts: sandboxed when the package's tests are.
    binary: Option<Binary>,
    /// The run of its `unsandboxed-tests`, outside, after every sandboxed test of the queue;
    /// none when `binary` is outside already.
    outside: Option<Binary>,
    /// By `listed.tests`: whether the test runs in `binary` rather than in `outside`.
    inside: Vec<bool>,
    /// The machine's files and the binary's environment, as what a test read is checked
    /// against: made once per binary.
    disk: blaze_usercode::inputs::Disk,
    /// What the tests of its server all read, hashed once per buffer of it.
    common: traced::Common,
}

/// A binary's tests, and each by name.
struct Listed {
    tests: Vec<Test>,
    /// `tests` by name, sorted: a finished test is found in `O(log n)` with nothing allocated.
    by_name: Vec<u32>,
}

impl Listed {
    fn test(&self, name: &str) -> Option<&Test> {
        let at = self.by_name.binary_search_by(|it| self.tests[*it as usize].name().cmp(name)).ok()?;
        Some(&self.tests[self.by_name[at] as usize])
    }
}

/// The tests of `built`, filtered by `filter`, run on the loop.
pub(crate) struct TestRun {
    job: Arc<Job>,
    tags: u64,
    filter: Filter,
    all: bool,
    suites: Vec<Suited>,
    /// By the queue's index of a binary: the suite it is of. A suite gives the queue one
    /// binary, or two when some of its tests run outside the sandbox.
    suite_of: Vec<u32>,
    queue: Option<Queue>,
    ok: bool,
}

pub(crate) fn start(
    job: &Arc<Job>,
    built: &[Artifact],
    selection: Selection<'_>,
    tags: u64,
) -> Result<TestRun, ApiError> {
    if selection.ub.is_some() {
        return Err(ApiError::new(
            "`blaze test --ub` runs its tests in the MIR evaluator (`ub_tests`, `evaluate`), not as compiled binaries",
            None,
        ));
    }
    let filter = filter(&selection)?;
    let all = selection.all;
    let units = job.units();
    let config = &job.input.config;
    let mut suites = Vec::new();
    for artifact in built.iter().filter(|it| it.profile.test) {
        let Some(program) = &artifact.executable else { continue };
        let Some(at) = unit_of_artifact(job, artifact) else {
            return Err(ApiError::new(format!("no test unit of `{}` is in the build", artifact.target.name), None));
        };
        let unit = &units.graph.units[at];
        let package = package_of(units, unit)?;
        let Some(made) = job.made[at].get() else {
            return Err(ApiError::new(format!("the test binary `{program}` was not compiled by this job"), None));
        };
        let Some(context) = made.context else {
            return Err(ApiError::new(format!("the test binary `{program}` was not compiled by this job"), None));
        };
        let suite = Suite { package: package.name.clone(), target: target_name(unit), context };
        let manifest_dir = PathBuf::from(&package.manifest_path).parent().map(PathBuf::from).unwrap_or_default();
        let sandboxed = !job.sandbox_off() && package.tests_sandboxed(units.sandboxed_tests);
        // alloc: one flag per test, once per binary
        let inside: Vec<bool> = made.tests.iter().map(|it| sandboxed && !package.tests.outside(&it.name)).collect();
        let tests: Vec<Test> = made
            .tests
            .iter()
            .zip(&inside)
            .map(|(it, &inside)| {
                let reached = Reached { test: it.name.clone(), binary: artifact.content, reached: it.reached.clone() };
                Test::of(reached, attrs(it), it.outside || !inside)
            })
            .collect();
        if let Some(runner) = artifact.runner.first() {
            return Err(ApiError::new(
                format!(
                    "the test binary `{program}` has the runner `{runner}` (`target.<triple>.runner`), and \
                     blaze's test queue runs a test binary itself, not under a runner"
                ),
                None,
            ));
        }
        let loaded: Option<test::Loaded> = match super::run::launch(job, at, false).map_err(|what| ApiError::new(what, None))? {
            super::run::Launch::File => None,
        };
        let env: Vec<(OsString, OsString)> =
            super::run::process_env(&config.invocation.env, artifact, std::env::consts::OS);
        // `Binary` is not `Clone`: a suite with tests on both sides is given two.
        // clone: the binary's strings, once per binary and once more for one with tests outside
        let make = |sandboxed: bool| Binary {
            id: artifact.content,
            name: program.clone(),
            program: PathBuf::from(program),
            env: env.clone(),
            dir: manifest_dir.clone(),
            read: vec![PathBuf::from(&config.invocation.root), PathBuf::from(&units.target_dir), manifest_dir.clone()],
            write: Vec::new(),
            network: false,
            memory: None,
            mode: TestMode::Process,
            jobs: Vec::new(),
            loaded: loaded.clone(),
            sandboxed,
        };
        let binary = make(sandboxed);
        let outside = (sandboxed && !package.tests.tests.is_empty()).then(|| make(false));
        let mut by_name: Vec<u32> = (0..tests.len() as u32).collect();
        blaze_ids::names::sort_by_name_in(&mut by_name, |&it| tests[it as usize].name().as_bytes());
        let disk = blaze_usercode::inputs::Disk::new(binary.env.clone());
        suites.push(Suited {
            name: program.clone(),
            given: suite.given(),
            listed: Listed { tests, by_name },
            binary: Some(binary),
            outside,
            inside,
            disk,
            common: traced::Common::default(),
        });
    }
    Ok(TestRun { job: job.clone(), tags, filter, all, suites, suite_of: Vec::new(), queue: None, ok: true })
}

impl TestRun {
    fn now(&self) -> Duration {
        self.job.began.elapsed()
    }

    /// What finished: reported, and kept for selection.
    fn finished(&mut self, finished: impl Iterator<Item = Finished>, store: &mut Store, out: &mut Vec<Response>) {
        let color = self.job.input.color;
        let refused = |what: String| Response::Diagnostic(super::say::refused(what, None, color));
        for done in finished {
            let Some(suite) = self.suite_of.get(done.binary).and_then(|&at| self.suites.get_mut(at as usize)) else {
                self.ok = false;
                out.push(refused(format!("the test queue gave a result of its binary {}, which this run did not give it", done.binary)));
                continue;
            };
            if let Outcome::Failed(_) = done.outcome {
                self.ok = false;
            }
            if let Some(report) = test::report(&suite.name, &done) {
                out.push(Response::TestResult(result(report)));
            }
            let passed = match done.outcome {
                Outcome::Passed => true,
                Outcome::Failed(_) => false,
                Outcome::NotRun => continue,
            };
            let Some(name) = &done.test else { continue };
            let Some(test) = suite.listed.test(name) else {
                self.ok = false;
                out.push(refused(format!(
                    "test `{name}` of `{}` ran, and is none of the tests its build listed: how it ran can't be kept",
                    suite.name
                )));
                continue;
            };
            let (given, took) = (suite.given, done.took);
            let kept = suite.common.traced(done.trace, &mut suite.disk, |traced| {
                select::record(store, given, test, &select::Ran { passed, took, traced })
            });
            let kept = match kept {
                Ok(it) => it,
                Err(error) => {
                    out.push(refused(format!("what test `{name}` read can't be told: {error}")));
                    select::record(store, given, test, &select::Ran { passed, took, traced: None })
                }
            };
            if let Err(error) = kept {
                self.ok = false;
                out.push(refused(format!("keeping how test `{name}` ran: {error}")));
            }
        }
    }

    fn end(&self) -> Option<bool> {
        Some(self.ok)
    }
}

impl Runs<Real> for TestRun {
    fn start(
        &mut self,
        store: &Store,
        _writer: Option<&mut UnitWriter>,
        effects: &mut Vec<Effect>,
        out: &mut Vec<Response>,
    ) -> Option<bool> {
        let mut binaries = Vec::with_capacity(self.suites.len());
        self.suite_of.clear();
        for (at, suite) in self.suites.iter_mut().enumerate() {
            let Some(mut binary) = suite.binary.take() else { continue };
            let mut outside = suite.outside.take();
            for (test, &inside) in suite.listed.tests.iter().zip(&suite.inside) {
                let binary = match (inside, &mut outside) {
                    (false, Some(outside)) => outside,
                    _ => &mut binary,
                };
                match select::wanted(test, &self.filter) {
                    Wanted::Filtered => continue,
                    Wanted::Ignored => {
                        let report = test::report_unrun(&suite.name, test.name(), Reported::Ignored, None);
                        out.push(Response::TestResult(result(report)));
                        continue;
                    }
                    Wanted::Yes => {}
                }
                match select::select(store, suite.given, test, self.all, &mut suite.disk) {
                    Ok(Decision::Skip { took }) => {
                        out.push(Response::TestResult(cached(&suite.name, test.name(), took)));
                    }
                    Ok(Decision::Run { expected, .. }) => binary.jobs.push(TestJob {
                        name: test.name().to_owned(),
                        ignore: false,
                        expected: expected.unwrap_or_default(),
                        timeout: TIMEOUT,
                        retries: 0,
                    }),
                    Err(error) => {
                        self.ok = false;
                        out.push(Response::Diagnostic(super::say::refused(
                            format!("whether test `{}` runs: {error}", test.name()),
                            None,
                            self.job.input.color,
                        )));
                    }
                }
            }
            let at = u32::try_from(at).expect("under 2^32 suites");
            binaries.push(binary);
            self.suite_of.push(at);
            if let Some(outside) = outside {
                binaries.push(outside);
                self.suite_of.push(at);
            }
        }
        let Some(dir) = FileName::new(&format!("tests/{:016x}", self.tags)) else { return self.end() };
        let config = Config {
            root: self.job.shared.state.clone(),
            dir,
            jobs: std::thread::available_parallelism().map_or(1, |it| it.get()),
            fail_fast: false,
            kill_grace: KILL_GRACE,
            tags: self.tags,
        };
        let now = self.now();
        let queue = Queue::new(config, binaries, now, effects);
        let finished = queue.finished();
        self.queue = Some(queue);
        match finished {
            true => self.end(),
            false => None,
        }
    }

    fn owns(&self, tag: Tag) -> bool {
        self.queue.as_ref().is_some_and(|it| it.owns(tag))
    }

    fn complete(
        &mut self,
        completion: Completion,
        store: &mut Store,
        _writer: Option<&mut UnitWriter>,
        effects: &mut Vec<Effect>,
        out: &mut Vec<Response>,
    ) -> Option<bool> {
        let now = self.now();
        let mut queue = self.queue.take()?;
        queue.complete(completion, now, effects);
        self.finished(queue.drain(), store, out);
        let done = queue.finished();
        self.queue = Some(queue);
        match done {
            true => self.end(),
            false => None,
        }
    }

    fn cancel(&mut self, effects: &mut Vec<Effect>) -> Option<bool> {
        self.ok = false;
        let now = self.now();
        match &mut self.queue {
            Some(queue) => {
                queue.cancel(now, effects);
                match queue.finished() {
                    true => self.end(),
                    false => None,
                }
            }
            None => self.end(),
        }
    }
}

//! The tests of a build ([`Front::tests`](crate::serve::work::Front::tests)), as
//! `blaze_usercode::test` runs them: each test asked for ([`select::wanted`]) whose pass the
//! store does not keep for the code it reaches and what it read ([`select::select`]) runs in
//! a process of its own, forked from its binary's server in the sandbox, on one queue over
//! every binary; the rest are reported unreached, unless every test is asked for. Each result
//! is kept ([`select::record`]) and reported as a [`Response::TestResult`].

use std::{
    ffi::OsString,
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use blaze_api::{ApiError, Artifact, Response, TestOutcome, TestResult};
use blaze_cas::Store;
use blaze_core::{Completion, Effect, FileName, Tag};
use blaze_usercode::{
    Reached,
    test::{
        self, Attrs, Binary, Config, Decision, Filter, Finished, Job as TestJob, Mode as TestMode, Outcome,
        Queue, Reported, ShouldPanic, Suite, Test, Wanted, select,
    },
};
use blaze_units::schema::{Mode, Unit};

use super::{Job, compile::TestOf, package_of};
use crate::serve::work::Runs;

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
    Attrs {
        ignore: test.ignore,
        should_panic: match &test.should_panic {
            None => ShouldPanic::No,
            Some(None) => ShouldPanic::Yes,
            Some(Some(message)) => ShouldPanic::WithMessage(message.clone()),
        },
        bench: test.bench,
    }
}

fn result(report: test::Report) -> TestResult {
    TestResult {
        binary: report.binary,
        name: report.name,
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

/// One binary's tests as the queue and selection see them.
struct Suited {
    name: String,
    given: blaze_ids::ContentId,
    tests: Vec<Test>,
    /// `tests` by name, sorted: a finished test is found in `O(log n)` with nothing allocated.
    by_name: Vec<u32>,
    /// Handed to the queue when it starts.
    binary: Option<Binary>,
    /// The machine's files and the binary's environment, as what a test read is checked
    /// against: made once per binary.
    disk: blaze_usercode::inputs::Disk,
}

impl Suited {
    fn test(&self, name: &str) -> Option<&Test> {
        let at = self.by_name.binary_search_by(|it| self.tests[*it as usize].name.as_str().cmp(name)).ok()?;
        Some(&self.tests[self.by_name[at] as usize])
    }
}

/// The tests of `built`, filtered by `filter`, run on the loop.
struct TestRun {
    job: Arc<Job>,
    tags: u64,
    filter: Filter,
    all: bool,
    suites: Vec<Suited>,
    queue: Option<Queue>,
    ok: bool,
}

pub(crate) fn start(
    job: &Arc<Job>,
    built: &[Artifact],
    filter: &[String],
    all: bool,
    tags: u64,
) -> Result<Box<dyn Runs>, ApiError> {
    if job.sandbox_off() {
        return Err(ApiError::new(
            "`BLAZE_SANDBOX=off` asks for tests outside the sandbox, and blaze's test queue runs them \
             only in it: run them with the sandbox on",
            None,
        ));
    }
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
        let tests: Vec<Test> = made
            .tests
            .iter()
            .map(|it| {
                let reached = Reached { test: it.name.clone(), binary: artifact.content, reached: it.reached.clone() };
                Test::of(&reached, attrs(it), it.outside)
            })
            .collect();
        let manifest_dir = PathBuf::from(&package.manifest_path).parent().map(PathBuf::from).unwrap_or_default();
        if let Some(runner) = artifact.runner.first() {
            return Err(ApiError::new(
                format!(
                    "the test binary `{program}` has the runner `{runner}` (`target.<triple>.runner`), and \
                     blaze's test queue runs a test binary itself, not under a runner"
                ),
                None,
            ));
        }
        let env: Vec<(OsString, OsString)> =
            super::run::process_env(&config.invocation.env, artifact, std::env::consts::OS);
        let binary = Binary {
            id: artifact.content,
            name: program.clone(),
            program: PathBuf::from(program),
            env,
            dir: manifest_dir.clone(),
            read: vec![PathBuf::from(&config.invocation.root), PathBuf::from(&units.target_dir), manifest_dir],
            write: Vec::new(),
            network: false,
            memory: None,
            mode: TestMode::Process,
            jobs: Vec::new(),
        };
        let mut by_name: Vec<u32> = (0..tests.len() as u32).collect();
        by_name.sort_unstable_by(|a, b| tests[*a as usize].name.cmp(&tests[*b as usize].name));
        let disk = blaze_usercode::inputs::Disk::new(binary.env.clone());
        suites.push(Suited { name: program.clone(), given: suite.given(), tests, by_name, binary: Some(binary), disk });
    }
    let filter = Filter { names: filter.to_vec(), ..Filter::default() };
    Ok(Box::new(TestRun { job: job.clone(), tags, filter, all, suites, queue: None, ok: true }))
}

impl TestRun {
    fn now(&self) -> Duration {
        self.job.began.elapsed()
    }

    /// What finished: reported, and kept for selection.
    fn finished(&mut self, finished: Vec<Finished>, store: &mut Store, out: &mut Vec<Response>) {
        for done in finished {
            let Some(suite) = self.suites.get_mut(done.binary) else { continue };
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
            let traced = match done.trace.traced(&mut suite.disk) {
                Ok(it) => it,
                Err(error) => {
                    out.push(Response::Diagnostic(super::say::refused(
                        format!("what test `{name}` read can't be told: {error}"),
                        None,
                        self.job.input.color,
                    )));
                    None
                }
            };
            let Some(test) = suite.test(name) else { continue };
            let ran = select::Ran { passed, took: done.took, traced: traced.as_ref() };
            if let Err(error) = select::record(store, suite.given, test, &ran) {
                self.ok = false;
                out.push(Response::Diagnostic(super::say::refused(
                    format!("keeping how test `{name}` ran: {error}"),
                    None,
                    self.job.input.color,
                )));
            }
        }
    }

    fn end(&self) -> Option<bool> {
        Some(self.ok)
    }
}

impl Runs for TestRun {
    fn start(&mut self, store: &Store, effects: &mut Vec<Effect>, out: &mut Vec<Response>) -> Option<bool> {
        let mut binaries = Vec::with_capacity(self.suites.len());
        for suite in &mut self.suites {
            let Some(mut binary) = suite.binary.take() else { continue };
            for test in &suite.tests {
                match select::wanted(test, &self.filter) {
                    Wanted::Filtered => continue,
                    Wanted::Ignored => {
                        let report = test::report_unrun(&suite.name, &test.name, Reported::Ignored, None);
                        out.push(Response::TestResult(result(report)));
                        continue;
                    }
                    Wanted::Yes => {}
                }
                match select::select(store, suite.given, test, self.all, &mut suite.disk) {
                    Ok(Decision::Skip { took }) => {
                        let report = test::report_unrun(&suite.name, &test.name, Reported::Unreached, took);
                        out.push(Response::TestResult(result(report)));
                    }
                    Ok(Decision::Run { expected, .. }) => binary.jobs.push(TestJob {
                        name: test.name.clone(),
                        ignore: false,
                        expected: expected.unwrap_or_default(),
                        timeout: TIMEOUT,
                        retries: 0,
                    }),
                    Err(error) => {
                        self.ok = false;
                        out.push(Response::Diagnostic(super::say::refused(
                            format!("whether test `{}` runs: {error}", test.name),
                            None,
                            self.job.input.color,
                        )));
                    }
                }
            }
            binaries.push(binary);
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
        effects: &mut Vec<Effect>,
        out: &mut Vec<Response>,
    ) -> Option<bool> {
        let now = self.now();
        let mut queue = self.queue.take()?;
        queue.complete(completion, now, effects);
        let finished = queue.take();
        let done = queue.finished();
        self.queue = Some(queue);
        self.finished(finished, store, out);
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

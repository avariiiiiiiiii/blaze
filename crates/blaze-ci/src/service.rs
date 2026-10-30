//! What each event makes happen (docs/v2/plan/lanes/ci.md section 10), as a state machine with no IO, no
//! clock and no threads: [`Service::step`] takes an [`Input`] and the time and says what to do
//! in [`Output`]s. [`crate::runtime`] does it.
//!
//! The service is a client of the warm daemon's agent API. It never writes the store (only the
//! daemon does), never starts a build itself and never hands the daemon a secret: each check is
//! one request on a view.
//!
//! - **A checkout is a view.** A commit's tree becomes a root on the daemon by the smallest diff
//!   at hand: the ref's last checkout, else (for a pull request) a checkout of its base commit,
//!   else the default branch's, else the empty tree. The ops of the git diff are applied to that
//!   root as a what-if and the result is held open ([`Output::Pin`]) so the daemon's GC keeps
//!   it until the ref's next checkout. One checkout of a ref runs at a time, and each one that
//!   ends becomes the ref's checkout, so the default branch's head `ci/<branch>` (committed with
//!   the same ops, to survive a daemon restart) always moves from the tree it is at.
//! - **Checks**: each configured check is one check run, posted queued at once, in progress when
//!   its request is sent and completed at its end, on the commit the event named.
//! - **Latest wins**: a newer push to a branch or pull request cancels the older one's requests
//!   and posts its check runs cancelled. Tags go through the [`Train`]: one release at a time,
//!   a newer tag replacing the waiting one.
//! - **Fork pull requests** run as any other: the daemon sandboxes every build script, proc macro
//!   and test, and the service adds no secret to any request (the installation token only
//!   fetches the commit into the VM's clone).
//! - **The oracle** ([`crate::oracle`]): a sampled view is also verified from nothing.
//! - **Snapshots** ([`crate::snapshot`]): after a green run of the default branch or a release.

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};

use blaze_api::{Diagnostic, Level, Mismatch, Request, Response, TestOutcome, TestResult, ViewRef};
use blaze_ids::ContentId;
use blaze_vfs::View;
use serde::{Deserialize, Serialize};

use crate::{
    CheckRun, CheckStatus, Checkout, Event, Train,
    checkout::pull_ref,
    config::{Check, CheckKind, Config},
    oracle,
    snapshot::{self, Kept},
};

/// How many checkouts of branches and pull requests are held open besides the default
/// branch's; past it, the one used longest ago is let go. A pull request's close is no event,
/// so without a bound every root ever checked out would be held.
pub const MOST_CHECKOUTS: usize = 256;

/// The most errors and failed tests a check run's summary lists; the rest are counted.
const MOST_LISTED: usize = 100;

/// The most of a failed test's output a summary shows: its last lines, then its last bytes.
const TAIL_LINES: usize = 30;
const TAIL_BYTES: usize = 4096;

/// What of [`Config`] the service decides with.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Settings {
    pub default_branch: String,
    pub checks: Vec<Check>,
    pub release_checks: Vec<Check>,
    pub oracle_one_in: u64,
    pub oracle_targets: blaze_api::Targets,
    pub snapshot_every_secs: u64,
    pub snapshot_keep: usize,
}

impl Settings {
    pub fn of(config: &Config) -> Settings {
        Settings {
            default_branch: config.default_branch.clone(),
            checks: config.checks.clone(),
            release_checks: config.release_checks.clone(),
            oracle_one_in: config.oracle.one_in,
            oracle_targets: config.oracle.targets.to_targets(),
            snapshot_every_secs: config.snapshot.every_secs,
            snapshot_keep: config.snapshot.keep,
        }
    }
}

/// What the service keeps across restarts, in its state file.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct State {
    /// By ref: `branch/<b>`, `pr/<n>`.
    pub checkouts: BTreeMap<String, Held>,
    pub snapshots: Vec<Kept>,
}

/// A ref's last checkout, held open on the daemon.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct Held {
    pub checkout: Checkout,
    /// When it was last made or used as a base, in seconds since the epoch.
    pub at: u64,
}

/// A job: what one event runs, on one commit.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct JobId(pub u64);

/// A check run, as the service names it before GitHub has given it an id.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct RunId(pub u64);

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Input {
    /// A delivery whose signature checked.
    Event(Event),
    /// The job's commit is a root on the daemon, held open once; or why not.
    CheckedOut { job: JobId, result: Result<ContentId, String> },
    /// A response of the request sent for `run`, before its `Done`.
    Response { job: JobId, run: RunId, response: Response },
    /// The request sent for `run` is over: its `Done`, or `ok` false when the daemon could not
    /// be asked (after a [`Response::Error`] saying why).
    Ended { job: JobId, run: RunId, ok: bool },
    /// GitHub created the check run, with its id; or why not, after retries.
    Created { run: RunId, result: Result<u64, String> },
    /// A root held open was lost (the daemon restarted without it).
    Unpinned { root: ContentId, why: String },
    Snapshotted(Result<Kept, String>),
    Removed { id: String, result: Result<(), String> },
}

#[derive(Clone, PartialEq, Debug)]
pub enum Output {
    /// Hold `root` open once more on the pin connection.
    Pin { root: ContentId },
    /// Let go of one hold of `root`.
    Unpin { root: ContentId },
    /// Fetch `fetch`, diff `from`'s commit (the empty tree for `None`) to `commit`, apply the ops
    /// to `from`'s root (`Head("ci/empty")` for `None`) and hold the result open; with `head`,
    /// commit the same ops to that head and check it reached the same root. Answered with
    /// [`Input::CheckedOut`]. The job's worker starts here.
    Checkout { job: JobId, commit: String, fetch: Vec<String>, from: Option<Checkout>, head: Option<String> },
    /// Send `request` for `run`, on the job's own connection.
    Send { job: JobId, run: RunId, request: Request },
    /// Cancel every request of the job still running; each still ends with [`Input::Ended`].
    Cancel { job: JobId },
    /// The job's worker has nothing more to do.
    EndJob { job: JobId },
    /// Create the check run; answered with [`Input::Created`].
    Create { run: RunId, job: JobId, check: CheckRun },
    Update { run: RunId, id: u64, check: CheckRun },
    /// Write the state file.
    Save(State),
    /// [`Request::Snapshot`], then the snapshot command with `name`; answered with
    /// [`Input::Snapshotted`].
    Snapshot { name: String },
    /// The remove command for `id`; answered with [`Input::Removed`].
    RemoveSnapshot { id: String },
    /// A line for stderr: an error no check run carries.
    Log(String),
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum JobKind {
    Branch { default: bool },
    Pull { number: u64, base: String },
    Tag { name: String },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    /// Waiting for the ref's checkout in flight, or for the train.
    Queued,
    CheckingOut,
    Running,
}

#[derive(Clone, Debug)]
struct Job {
    reference: String,
    commit: String,
    kind: JobKind,
    phase: Phase,
    runs: Vec<RunId>,
    /// A newer event replaced it: its check runs are posted cancelled.
    replaced: bool,
    /// Runs whose request is sent and not ended.
    pending: BTreeSet<RunId>,
    /// [`Output::Checkout`] was given: the runtime has a worker to end.
    worker: bool,
    root: Option<ContentId>,
    /// The root is held for the job alone (a tag's), let go when it ends.
    owns_root: bool,
    /// The base of its checkout, held while the checkout runs.
    base: Option<ContentId>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum Work {
    Check(Check),
    Oracle,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum GitHubId {
    Creating,
    Id(u64),
    /// Creating it failed; the runtime logged it, and nothing more is posted.
    Failed,
}

#[derive(Clone, Debug)]
struct Run {
    job: JobId,
    check: CheckRun,
    work: Work,
    github: GitHubId,
    /// What GitHub was last sent.
    posted: CheckRun,
    report: Report,
    /// Completed: nothing changes it again.
    done: bool,
}

pub struct Service {
    settings: Settings,
    state: State,
    now: u64,
    jobs: BTreeMap<JobId, Job>,
    runs: BTreeMap<RunId, Run>,
    /// Each branch's and pull request's newest job.
    latest: BTreeMap<String, JobId>,
    /// The job whose checkout of a ref is in flight.
    checking_out: BTreeMap<String, JobId>,
    train: Train,
    release_running: Option<JobId>,
    release_waiting: Option<JobId>,
    snapshotting: bool,
    removing: BTreeSet<String>,
    next_job: u64,
    next_run: u64,
}

impl Service {
    /// The service as `state` left it: each checkout it kept is held open again.
    pub fn new(settings: Settings, state: State, out: &mut Vec<Output>) -> Service {
        for held in state.checkouts.values() {
            out.push(Output::Pin { root: held.checkout.root });
        }
        Service {
            settings,
            state,
            now: 0,
            jobs: BTreeMap::new(),
            runs: BTreeMap::new(),
            latest: BTreeMap::new(),
            checking_out: BTreeMap::new(),
            train: Train::default(),
            release_running: None,
            release_waiting: None,
            snapshotting: false,
            removing: BTreeSet::new(),
            next_job: 0,
            next_run: 0,
        }
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    /// Jobs not yet over.
    pub fn jobs(&self) -> usize {
        self.jobs.len()
    }

    /// `input`, arrived at `now` (seconds since the epoch).
    pub fn step(&mut self, now: u64, input: Input, out: &mut Vec<Output>) {
        self.now = now;
        match input {
            Input::Event(event) => self.event(event, out),
            Input::CheckedOut { job, result } => self.checked_out(job, result, out),
            Input::Response { job, run, response } => self.response(job, run, response),
            Input::Ended { job, run, ok } => self.ended(job, run, ok, out),
            Input::Created { run, result } => self.created(run, result, out),
            Input::Unpinned { root, why } => self.unpinned(root, &why, out),
            Input::Snapshotted(result) => self.snapshotted(result, out),
            Input::Removed { id, result } => self.removed(id, result, out),
        }
    }

    fn event(&mut self, event: Event, out: &mut Vec<Output>) {
        match event {
            Event::Push { branch, commit } => {
                let default = branch == self.settings.default_branch;
                self.arrive(format!("branch/{branch}"), commit, JobKind::Branch { default }, out);
            }
            Event::PullRequest { number, head, base, from_fork: _ } => {
                self.arrive(format!("pr/{number}"), head, JobKind::Pull { number, base }, out);
            }
            Event::Tag { name, commit } => self.tag(name, commit, out),
        }
    }

    /// A push to a branch or pull request: the newest wins.
    fn arrive(&mut self, reference: String, commit: String, kind: JobKind, out: &mut Vec<Output>) {
        if let Some(old) = self.latest.get(&reference).copied() {
            self.replace(old, &format!("Cancelled: replaced by {commit}."), out);
        }
        let checks = self.settings.checks.clone();
        let job = self.new_job(reference.clone(), commit, kind, &checks, out);
        self.latest.insert(reference, job);
        self.try_start(job, out);
    }

    fn tag(&mut self, name: String, commit: String, out: &mut Vec<Output>) {
        let checks = self.settings.release_checks.clone();
        let job = self.new_job(format!("tag/{name}"), commit, JobKind::Tag { name: name.clone() }, &checks, out);
        let idle = self.train.running.is_none();
        let replaced = self.train.arrive(name.clone());
        if idle {
            self.release_running = Some(job);
            self.try_start(job, out);
            return;
        }
        let old = self.release_waiting.replace(job);
        if let (Some(old), Some(_)) = (old, replaced) {
            self.replace(old, &format!("Cancelled: replaced by {name}."), out);
        }
        let running = self.train.running.clone().unwrap_or_default();
        let summary = format!("Queued: waiting for release {running} to finish.");
        for run in self.jobs[&job].runs.clone() {
            self.set(run, CheckStatus::Queued, summary.clone(), out);
        }
    }

    fn new_job(
        &mut self,
        reference: String,
        commit: String,
        kind: JobKind,
        checks: &[Check],
        out: &mut Vec<Output>,
    ) -> JobId {
        let job = JobId(self.next_job);
        self.next_job += 1;
        let mut works: Vec<(String, Work)> =
            checks.iter().map(|check| (check.name.clone(), Work::Check(check.clone()))).collect();
        if oracle::sampled(&commit, self.settings.oracle_one_in) {
            works.push((oracle::CHECK.to_owned(), Work::Oracle));
        }
        let mut runs = Vec::with_capacity(works.len());
        for (name, work) in works {
            let run = RunId(self.next_run);
            self.next_run += 1;
            let check = CheckRun {
                name,
                commit: commit.clone(),
                status: CheckStatus::Queued,
                summary: format!("Queued: waiting for the checkout of {commit}."),
            };
            out.push(Output::Create { run, job, check: check.clone() });
            self.runs.insert(
                run,
                Run {
                    job,
                    posted: check.clone(),
                    check,
                    work,
                    github: GitHubId::Creating,
                    report: Report::default(),
                    done: false,
                },
            );
            runs.push(run);
        }
        self.jobs.insert(
            job,
            Job {
                reference,
                commit,
                kind,
                phase: Phase::Queued,
                runs,
                replaced: false,
                pending: BTreeSet::new(),
                worker: false,
                root: None,
                owns_root: false,
                base: None,
            },
        );
        job
    }

    /// A newer event replaced `job`: its check runs are posted cancelled and its requests
    /// cancelled.
    fn replace(&mut self, job: JobId, summary: &str, out: &mut Vec<Output>) {
        let Some(it) = self.jobs.get_mut(&job) else { return };
        it.replaced = true;
        let phase = it.phase;
        let pending = !it.pending.is_empty();
        for run in it.runs.clone() {
            self.conclude(run, CheckStatus::Cancelled, summary.to_owned(), out);
        }
        match phase {
            Phase::Queued => self.finish(job, out),
            Phase::CheckingOut => {}
            Phase::Running if pending => out.push(Output::Cancel { job }),
            Phase::Running => self.finish(job, out),
        }
    }

    /// Where the job's checkout diffs from: the nearest tree already held.
    fn from(&self, job: &Job) -> Option<Checkout> {
        let held = |reference: &str| self.state.checkouts.get(reference).map(|it| it.checkout.clone());
        let default_ref = format!("branch/{}", self.settings.default_branch);
        if job.kind == (JobKind::Branch { default: true }) {
            // The head `ci/<branch>` is at this ref's checkout; its commit moves from there.
            return held(&job.reference);
        }
        let with_commit = |commit: &str| {
            self.state.checkouts.values().find(|it| it.checkout.commit == commit).map(|it| it.checkout.clone())
        };
        if let Some(same) = with_commit(&job.commit) {
            return Some(same);
        }
        if let Some(own) = held(&job.reference) {
            return Some(own);
        }
        if let JobKind::Pull { base, .. } = &job.kind
            && let Some(base) = with_commit(base)
        {
            return Some(base);
        }
        held(&default_ref)
    }

    fn try_start(&mut self, job: JobId, out: &mut Vec<Output>) {
        let Some(it) = self.jobs.get(&job) else { return };
        if it.phase != Phase::Queued || self.checking_out.contains_key(&it.reference) {
            return;
        }
        if matches!(it.kind, JobKind::Tag { .. }) && self.release_running != Some(job) {
            return;
        }
        let from = self.from(it);
        let mut fetch = Vec::new();
        if let JobKind::Pull { number, .. } = it.kind {
            fetch.push(pull_ref(number));
        }
        fetch.push(it.commit.clone());
        let head = match it.kind {
            JobKind::Branch { default: true } => Some(format!("ci/{}", self.settings.default_branch)),
            _ => None,
        };
        let commit = it.commit.clone();
        let reference = it.reference.clone();
        if let Some(from) = &from {
            out.push(Output::Pin { root: from.root });
            let now = self.now;
            if let Some(held) = self.state.checkouts.values_mut().find(|it| it.checkout == *from) {
                held.at = now;
            }
        }
        let it = self.jobs.get_mut(&job).expect("the job is there");
        it.base = from.as_ref().map(|from| from.root);
        it.phase = Phase::CheckingOut;
        it.worker = true;
        self.checking_out.insert(reference, job);
        out.push(Output::Checkout { job, commit, fetch, from, head });
    }

    fn checked_out(&mut self, job: JobId, result: Result<ContentId, String>, out: &mut Vec<Output>) {
        let Some(it) = self.jobs.get_mut(&job) else { return };
        if it.phase != Phase::CheckingOut {
            return;
        }
        let reference = it.reference.clone();
        if self.checking_out.get(&reference) == Some(&job) {
            self.checking_out.remove(&reference);
        }
        if let Some(base) = it.base.take() {
            out.push(Output::Unpin { root: base });
        }
        match result {
            Ok(root) => {
                it.root = Some(root);
                if matches!(it.kind, JobKind::Tag { .. }) {
                    it.owns_root = true;
                } else {
                    let checkout = Checkout { commit: it.commit.clone(), root };
                    self.keep(reference.clone(), checkout, out);
                }
                let it = self.jobs.get_mut(&job).expect("the job is there");
                if it.replaced {
                    self.finish(job, out);
                } else {
                    self.run(job, root, out);
                }
            }
            Err(what) => {
                if !it.replaced {
                    let summary = format!(
                        "The checkout of {} failed, so nothing ran: {what}",
                        it.commit
                    );
                    for run in it.runs.clone() {
                        self.conclude(run, CheckStatus::Failure, summary.clone(), out);
                    }
                }
                self.finish(job, out);
            }
        }
        if let Some(next) = self.latest.get(&reference).copied()
            && next != job
        {
            self.try_start(next, out);
        }
    }

    /// `checkout` becomes `reference`'s, held in place of the one it had.
    fn keep(&mut self, reference: String, checkout: Checkout, out: &mut Vec<Output>) {
        let held = Held { checkout, at: self.now };
        if let Some(old) = self.state.checkouts.insert(reference, held) {
            out.push(Output::Unpin { root: old.checkout.root });
        }
        let default_ref = format!("branch/{}", self.settings.default_branch);
        while self.state.checkouts.keys().filter(|it| **it != default_ref).count() > MOST_CHECKOUTS {
            let oldest = self
                .state
                .checkouts
                .iter()
                .filter(|(reference, _)| **reference != default_ref)
                .min_by_key(|(reference, held)| (held.at, (*reference).clone()))
                .map(|(reference, _)| reference.clone())
                .expect("more than none");
            let held = self.state.checkouts.remove(&oldest).expect("it is there");
            out.push(Output::Unpin { root: held.checkout.root });
        }
        out.push(Output::Save(self.state.clone()));
    }

    /// Sends each check's request on the job's view.
    fn run(&mut self, job: JobId, root: ContentId, out: &mut Vec<Output>) {
        let it = self.jobs.get_mut(&job).expect("the job is there");
        it.phase = Phase::Running;
        let runs = it.runs.clone();
        let commit = it.commit.clone();
        let view = ViewRef::Root(View { root });
        for run in runs {
            let Some(it) = self.runs.get(&run) else { continue };
            if it.done {
                continue;
            }
            let (request, what) = match &it.work {
                Work::Check(check) => (check.request(view.clone()), kind_name(check.kind)),
                Work::Oracle => (
                    Request::Verify { view: view.clone(), targets: self.settings.oracle_targets.clone() },
                    "verify",
                ),
            };
            self.jobs.get_mut(&job).expect("the job is there").pending.insert(run);
            out.push(Output::Send { job, run, request });
            let summary = format!("Running `{what}` on the tree of {commit} (root {root:x}).");
            self.set(run, CheckStatus::InProgress, summary, out);
        }
        if self.jobs[&job].pending.is_empty() {
            self.finish(job, out);
        }
    }

    fn response(&mut self, job: JobId, run: RunId, response: Response) {
        let sent = self.jobs.get(&job).is_some_and(|it| it.pending.contains(&run));
        if let Some(it) = self.runs.get_mut(&run)
            && sent
            && !it.done
        {
            it.report.add(response);
        }
    }

    fn ended(&mut self, job: JobId, run: RunId, ok: bool, out: &mut Vec<Output>) {
        let Some(it) = self.jobs.get_mut(&job) else { return };
        if !it.pending.remove(&run) {
            return;
        }
        let over = it.pending.is_empty() && it.phase == Phase::Running;
        if let Some(it) = self.runs.get(&run)
            && !it.done
        {
            let (status, summary) = it.outcome(ok);
            self.conclude(run, status, summary, out);
        }
        if over {
            self.finish(job, out);
        }
    }

    /// The job is over: its worker ends, its root is let go if it owned it, the train moves, and
    /// a green run of the default branch or a release may snapshot the VM.
    fn finish(&mut self, job: JobId, out: &mut Vec<Output>) {
        let Some(it) = self.jobs.remove(&job) else { return };
        if self.latest.get(&it.reference) == Some(&job) {
            self.latest.remove(&it.reference);
        }
        if self.checking_out.get(&it.reference) == Some(&job) {
            self.checking_out.remove(&it.reference);
        }
        if it.worker {
            out.push(Output::EndJob { job });
        }
        if it.owns_root
            && let Some(root) = it.root
        {
            out.push(Output::Unpin { root });
        }
        for run in &it.runs {
            if self.runs.get(run).is_some_and(|it| !it.done) {
                self.conclude(*run, CheckStatus::Failure, "The job ended without a result.".to_owned(), out);
            }
        }
        let green = !it.replaced
            && it.runs.iter().all(|run| {
                self.runs.get(run).is_some_and(|it| it.check.status == CheckStatus::Success)
            });
        for run in &it.runs {
            self.collect(*run);
        }
        if self.release_waiting == Some(job) {
            self.release_waiting = None;
        }
        if self.release_running == Some(job) {
            self.release_running = None;
            if self.train.finish().is_some()
                && let Some(next) = self.release_waiting.take()
            {
                self.release_running = Some(next);
                self.try_start(next, out);
            }
        }
        let snapshots = matches!(it.kind, JobKind::Branch { default: true } | JobKind::Tag { .. });
        if green && snapshots {
            self.maybe_snapshot(&it.commit, out);
        }
    }

    /// Forgets a run once its job is over and GitHub has answered its creation.
    fn collect(&mut self, run: RunId) {
        let Some(it) = self.runs.get(&run) else { return };
        if it.github != GitHubId::Creating && !self.jobs.contains_key(&it.job) {
            self.runs.remove(&run);
        }
    }

    /// Completes the run, unless it is already.
    fn conclude(&mut self, run: RunId, status: CheckStatus, summary: String, out: &mut Vec<Output>) {
        let Some(it) = self.runs.get_mut(&run) else { return };
        if it.done {
            return;
        }
        it.done = true;
        self.set(run, status, summary, out);
    }

    fn set(&mut self, run: RunId, status: CheckStatus, summary: String, out: &mut Vec<Output>) {
        let Some(it) = self.runs.get_mut(&run) else { return };
        it.check.status = status;
        it.check.summary = summary;
        if let GitHubId::Id(id) = it.github
            && it.posted != it.check
        {
            it.posted = it.check.clone();
            out.push(Output::Update { run, id, check: it.check.clone() });
        }
    }

    fn created(&mut self, run: RunId, result: Result<u64, String>, out: &mut Vec<Output>) {
        let Some(it) = self.runs.get_mut(&run) else { return };
        if it.github != GitHubId::Creating {
            return;
        }
        match result {
            Ok(id) => {
                it.github = GitHubId::Id(id);
                if it.posted != it.check {
                    it.posted = it.check.clone();
                    out.push(Output::Update { run, id, check: it.check.clone() });
                }
            }
            Err(what) => {
                it.github = GitHubId::Failed;
                out.push(Output::Log(format!(
                    "check run {} on {} was never created, so its result is not on GitHub: {what}",
                    it.check.name, it.check.commit
                )));
            }
        }
        self.collect(run);
    }

    fn unpinned(&mut self, root: ContentId, why: &str, out: &mut Vec<Output>) {
        let lost: Vec<String> = self
            .state
            .checkouts
            .iter()
            .filter(|(_, held)| held.checkout.root == root)
            .map(|(reference, _)| reference.clone())
            .collect();
        if lost.is_empty() {
            return;
        }
        for reference in &lost {
            self.state.checkouts.remove(reference);
        }
        out.push(Output::Log(format!(
            "the daemon no longer holds root {root:x}, the checkout of {}: {why}; their next \
             checkout diffs from another tree",
            lost.join(", ")
        )));
        out.push(Output::Save(self.state.clone()));
    }

    fn maybe_snapshot(&mut self, commit: &str, out: &mut Vec<Output>) {
        if self.snapshotting
            || !snapshot::due(&self.state.snapshots, self.now, self.settings.snapshot_every_secs)
        {
            return;
        }
        self.snapshotting = true;
        out.push(Output::Snapshot { name: snapshot::name(commit, self.now) });
    }

    fn snapshotted(&mut self, result: Result<Kept, String>, out: &mut Vec<Output>) {
        self.snapshotting = false;
        match result {
            Ok(kept) => {
                self.state.snapshots.push(kept);
                out.push(Output::Save(self.state.clone()));
                for id in snapshot::beyond(&self.state.snapshots, self.settings.snapshot_keep) {
                    if self.removing.insert(id.clone()) {
                        out.push(Output::RemoveSnapshot { id });
                    }
                }
            }
            Err(what) => out.push(Output::Log(format!(
                "the snapshot failed, and the next green run tries again: {what}"
            ))),
        }
    }

    fn removed(&mut self, id: String, result: Result<(), String>, out: &mut Vec<Output>) {
        self.removing.remove(&id);
        match result {
            Ok(()) => {
                self.state.snapshots.retain(|it| it.id != id);
                out.push(Output::Save(self.state.clone()));
            }
            Err(what) => out.push(Output::Log(format!(
                "removing snapshot {id} failed, and it is tried again after the next snapshot: {what}"
            ))),
        }
    }
}

fn kind_name(kind: CheckKind) -> &'static str {
    match kind {
        CheckKind::Check => "check",
        CheckKind::Build => "build",
        CheckKind::Test => "test",
    }
}

impl Run {
    /// The run's status and summary once its request ended with `ok`.
    fn outcome(&self, ok: bool) -> (CheckStatus, String) {
        let report = &self.report;
        match &self.work {
            Work::Oracle => match &report.verified {
                Some(mismatches) if ok && mismatches.is_empty() => {
                    (CheckStatus::Success, oracle::summary(mismatches))
                }
                Some(mismatches) if !mismatches.is_empty() => {
                    (CheckStatus::Failure, report.with_errors(oracle::summary(mismatches)))
                }
                _ => (
                    CheckStatus::Failure,
                    report.with_errors("The verification ended without a result.".to_owned()),
                ),
            },
            Work::Check(check) => {
                let green = ok
                    && report.errors == 0
                    && report.failed == 0
                    && report.said.is_empty()
                    && report.unexpected.is_empty();
                let status = if green { CheckStatus::Success } else { CheckStatus::Failure };
                (status, report.summary(check.kind, ok))
            }
        }
    }
}

/// What a request's responses said, kept for its check run's summary.
#[derive(Clone, Default, Debug)]
struct Report {
    errors: usize,
    warnings: usize,
    listed: Vec<String>,
    passed: usize,
    failed: usize,
    ignored: usize,
    unreached: usize,
    failures: Vec<String>,
    artifacts: usize,
    verified: Option<Vec<Mismatch>>,
    /// The daemon's errors.
    said: Vec<String>,
    unexpected: Vec<String>,
}

impl Report {
    fn add(&mut self, response: Response) {
        match response {
            Response::Diagnostic(diagnostic) => match diagnostic.level {
                Level::Error => {
                    self.errors += 1;
                    if self.listed.len() < MOST_LISTED {
                        self.listed.push(render_diagnostic(&diagnostic));
                    }
                }
                Level::Warning => self.warnings += 1,
                Level::Note | Level::Help | Level::FailureNote => {}
            },
            Response::TestResult(test) => match test.outcome {
                TestOutcome::Passed => self.passed += 1,
                TestOutcome::Ignored => self.ignored += 1,
                TestOutcome::Unreached => self.unreached += 1,
                TestOutcome::Failed => {
                    self.failed += 1;
                    if self.failures.len() < MOST_LISTED {
                        self.failures.push(render_failure(&test));
                    }
                }
            },
            Response::Built { artifacts, .. } => self.artifacts += artifacts.len(),
            Response::Unit(_) | Response::BuildScript(_) | Response::Work(_) => {}
            Response::Verified { mismatches } => self.verified = Some(mismatches),
            Response::Error(error) => self.said.push(match error.at {
                Some(at) => format!("{} (at {at})", error.what),
                None => error.what,
            }),
            other => {
                if self.unexpected.len() < MOST_LISTED {
                    let mut shown = format!("{other:?}");
                    cut(&mut shown, 200);
                    self.unexpected.push(shown);
                }
            }
        }
    }

    fn summary(&self, kind: CheckKind, ok: bool) -> String {
        let mut text = format!(
            "{} error{}, {} warning{}",
            self.errors,
            plural(self.errors),
            self.warnings,
            plural(self.warnings)
        );
        match kind {
            CheckKind::Test => text.push_str(&format!(
                "; tests: {} passed, {} failed, {} ignored, {} not reached by the change (their \
                 last results stand)",
                self.passed, self.failed, self.ignored, self.unreached
            )),
            CheckKind::Build => text.push_str(&format!("; {} artifacts built", self.artifacts)),
            CheckKind::Check => {}
        }
        text.push('.');
        if !self.listed.is_empty() {
            text.push_str("\n\n## Errors\n");
            for error in &self.listed {
                text.push('\n');
                text.push_str(error);
                text.push('\n');
            }
            if self.errors > self.listed.len() {
                text.push_str(&format!("\nand {} more errors\n", self.errors - self.listed.len()));
            }
        }
        if !self.failures.is_empty() {
            text.push_str("\n\n## Failed tests\n");
            for failure in &self.failures {
                text.push('\n');
                text.push_str(failure);
                text.push('\n');
            }
            if self.failed > self.failures.len() {
                text.push_str(&format!("\nand {} more failed tests\n", self.failed - self.failures.len()));
            }
        }
        let text = self.with_errors(text);
        let quiet = self.errors == 0 && self.failed == 0 && self.said.is_empty();
        if !ok && quiet {
            return format!("{text}\n\nThe daemon ended the request as failed and said nothing more.");
        }
        text
    }

    /// `text`, then what the daemon said went wrong and any response the request should not
    /// have had.
    fn with_errors(&self, mut text: String) -> String {
        if !self.said.is_empty() {
            text.push_str("\n\n## The daemon's errors\n");
            for error in &self.said {
                text.push_str(&format!("\n- {error}"));
            }
            text.push('\n');
        }
        if !self.unexpected.is_empty() {
            text.push_str("\n\n## Responses this request should not have had\n");
            for response in &self.unexpected {
                text.push_str(&format!("\n- {response}"));
            }
            text.push('\n');
        }
        text
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn level_name(level: Level) -> &'static str {
    match level {
        Level::Error => "error",
        Level::Warning => "warning",
        Level::Note => "note",
        Level::Help => "help",
        Level::FailureNote => "failure-note",
    }
}

/// An error as rustc prints its first line, its other labelled places, its notes and the facts
/// its query read.
fn render_diagnostic(diagnostic: &Diagnostic) -> String {
    let code = diagnostic.code.as_deref().map(|code| format!("[{code}]")).unwrap_or_default();
    let place = diagnostic
        .spans
        .first()
        .map_or_else(|| "(no place)".to_owned(), |span| format!("{}:{}:{}", span.file, span.line, span.column));
    let mut text = format!("{}{code} {place}: {}", level_name(diagnostic.level), diagnostic.message);
    for span in &diagnostic.spans {
        if let Some(label) = &span.label {
            text.push_str(&format!("\n  {}:{}:{}: {label}", span.file, span.line, span.column));
        }
    }
    for child in &diagnostic.children {
        text.push_str(&format!("\n  {}: {}", level_name(child.level), child.message));
    }
    for fact in &diagnostic.facts {
        text.push_str(&format!("\n  read: {}", fact.what));
    }
    text
}

fn render_failure(test: &TestResult) -> String {
    format!(
        "test {} `{}` failed after {} ms; the end of its output:\n```\n{}\n```",
        test.binary,
        test.name,
        test.millis,
        tail(&test.output)
    )
}

/// The last [`TAIL_LINES`] lines of `output`, at most [`TAIL_BYTES`] of them.
fn tail(output: &[u8]) -> String {
    let text = String::from_utf8_lossy(output);
    let lines: Vec<&str> = text.trim_end().lines().collect();
    let mut tail = lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n");
    if tail.len() > TAIL_BYTES {
        let mut start = tail.len() - TAIL_BYTES;
        while !tail.is_char_boundary(start) {
            start += 1;
        }
        tail = format!("...{}", &tail[start..]);
    }
    tail.replace("```", "'''")
}

/// `text` cut to at most `most` bytes, at a char boundary.
fn cut(text: &mut String, most: usize) {
    if text.len() > most {
        let mut end = most;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("...");
    }
}

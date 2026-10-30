//! What a request asks of the daemon's build: everything the session can't answer from the VFS
//! and the database alone. The daemon implements [`Builder`] over the plane; the session drives
//! it and turns what it reports into responses.

use blaze_vfs::{Change, View, ViewId};

use crate::{Edit, Targets, Ub};

/// A job of the build, one per request (one per candidate of a best-of-N).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct JobId(pub u64);

/// A view the session opened for a job, in both arenas that name open views. It stays open
/// until the job finishes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Opened {
    pub view: View,
    /// The VFS's name for it ([`blaze_vfs::Vfs::open`]).
    pub files: ViewId,
    /// The database's ([`blaze_query::Database::open`]).
    pub queries: ViewId,
}

/// What a job does, and what it reports. Every job that builds reports, as they come,
/// [`crate::Response::Unit`] for each unit begun, [`crate::Response::BuildScript`] for each
/// build script run and [`crate::Response::Diagnostic`]s (each naming its unit).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Work {
    /// Then [`crate::Response::Built`] with an artifact for each unit that checked (its
    /// metadata).
    Check(Targets),
    /// Then [`crate::Response::Built`] and [`crate::Response::Work`].
    Build(Targets),
    /// Then a [`crate::Response::TestResult`] per test, [`crate::TestOutcome::Unreached`] for
    /// each the change reached none of (all run with `all`), and [`crate::Response::Work`].
    /// `args` is libtest's selection; with `ub` each test runs in the MIR evaluator, checked
    /// for undefined behavior, and nothing is compiled.
    Test { targets: Targets, filter: Vec<String>, all: bool, args: Vec<String>, ub: Option<Ub> },
    /// Then [`crate::Response::Ran`] and [`crate::Response::Work`].
    Run { targets: Targets, bin: Option<String>, args: Vec<String> },
    /// What the job's view's change from `from` reaches ([`crate::Response::Reached`]),
    /// building nothing. `changes` is the VFS's diff of the two.
    Reach { from: Opened, changes: Vec<Change>, targets: Targets },
    /// The view built again in a database of its own from nothing and every result compared
    /// with the daemon's ([`crate::Response::Verified`]): the oracle, on a sampled view.
    Verify(Targets),
    /// The daemon's state made durable (the image, the journal, the store), reported as
    /// [`crate::Response::Snapshotted`] once it is. Has no view.
    Snapshot,
    /// [`crate::Response::CodegenInputs`] of the view's last build.
    CodegenInputs(Targets),
    /// [`crate::Response::Trace`] of the view's last build, from the database's memos.
    Trace(Targets),
    /// [`crate::Response::Tables`]: each table's owner reports its own.
    Tables,
    /// The edits applied to the view in the front end (`blaze_intent::apply`), reported as
    /// [`crate::Response::Edited`] before an `ok` finish; the session commits its ops. The
    /// trees the edits left are the front end's parses of the files they wrote.
    Edit(Vec<Edit>),
    /// Built ahead for [`crate::Request::WillTest`]: the test units holding a test `filter`
    /// selects, and the functions those tests reached when they last ran first
    /// (`blaze_intent::ahead::plan`). Runs no test. Started only through
    /// [`Builder::speculate`].
    Ahead { targets: Targets, filter: Vec<String> },
}

/// The daemon's build, as the session drives it.
///
/// A job reports through [`crate::session::Session::report`] and ends with exactly one
/// [`crate::session::Session::finish`], both called by the daemon after `start` has returned,
/// never inside it. A job reports only the responses its [`Work`] names, never
/// [`crate::Response::Done`] or [`crate::Response::Error`]: a failed build finishes with `ok`
/// false after its diagnostics, and a job that could not run finishes with an
/// [`crate::ApiError`] naming what and where. A build, test or run not cancelled that finishes
/// without having reported [`crate::Response::Work`] fails, naming the job.
pub trait Builder {
    /// Starts `work` on `view` (`None` for [`Work::Snapshot`] only).
    fn start(&mut self, job: JobId, view: Option<Opened>, work: Work);
    /// Ends `job` early. It still finishes, with `ok` false.
    fn cancel(&mut self, job: JobId);
    /// [`Builder::start`] of declared intent: the job's tasks start only when no task of a
    /// job started otherwise is ready and fits (the plane's idle class), so it never delays
    /// one. Its reports are dropped; it finishes as any job does, and its memory is the
    /// plane's to give back when it is cancelled.
    fn speculate(&mut self, job: JobId, view: Opened, work: Work);
    /// A job started by [`Builder::speculate`] runs from now on as one started by
    /// [`Builder::start`]: a request takes its work over. Once per job.
    fn promote(&mut self, job: JobId);
}

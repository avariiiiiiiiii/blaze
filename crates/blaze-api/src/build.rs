//! What a request asks of the daemon's build: everything the session can't answer from the VFS
//! and the database alone. The daemon implements [`Builder`] over the plane; the session drives
//! it and turns what it reports into responses.

use blaze_vfs::{Change, View, ViewId};

use crate::Targets;

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

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Work {
    /// Reports [`crate::Response::Diagnostic`]s.
    Check(Targets),
    /// Reports diagnostics, then [`crate::Response::Built`].
    Build(Targets),
    /// Reports diagnostics, then a [`crate::Response::TestResult`] per test,
    /// [`crate::TestOutcome::Unreached`] for each the change reached none of (all run with
    /// `all`).
    Test { targets: Targets, filter: Vec<String>, all: bool },
    /// Reports diagnostics, then [`crate::Response::Ran`].
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
}

/// The daemon's build, as the session drives it.
///
/// A job reports through [`crate::session::Session::report`] and ends with exactly one
/// [`crate::session::Session::finish`], both called by the daemon after `start` has returned,
/// never inside it. A job reports only the responses its [`Work`] names, never
/// [`crate::Response::Done`] or [`crate::Response::Error`]: a failed build finishes with `ok`
/// false after its diagnostics, and a job that could not run finishes with an
/// [`crate::ApiError`] naming what and where.
pub trait Builder {
    /// Starts `work` on `view` (`None` for [`Work::Snapshot`] only).
    fn start(&mut self, job: JobId, view: Option<Opened>, work: Work);
    /// Ends `job` early. It still finishes, with `ok` false.
    fn cancel(&mut self, job: JobId);
}

//! What each request does (docs/v2/plan/lanes/ci.md, #277): a state machine over the VFS, the database and
//! the [`Builder`], with no IO, clock or threads.
//!
//! Every request ends with exactly one [`Response::Done`], after a [`Response::Error`] naming
//! what and where when it failed. A request that needs the build runs as jobs ([`JobId`], one
//! per candidate of a best-of-N) whose reports are forwarded under the request's id; the views a
//! job builds stay open in both the VFS and the database until it finishes, and the database's
//! view of the last incremental build of each of the latest [`KEPT`] roots stays open after,
//! for [`Request::Why`]. A commit is answered once its ticket is durable. A connection that is
//! gone has its requests cancelled and its views closed, and nothing more is sent to it.
//!
//! The daemon flushes the VFS ([`blaze_vfs::Vfs::flush`]) after each call, so what a call
//! committed is on its way to durability when the call returns.

mod best_of;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, VecDeque};

use blaze_core::Conn;
use rustc_hash::FxHashMap;
use blaze_ids::ContentId;
use blaze_query::QueryKey;
use blaze_vfs::{CommitError, OpError, Ticket, View, ViewId, Vfs};

use crate::{
    ApiError, Arrived, Reply, Request, RequestRef, Response, Status, ViewRef,
    build::{Builder, JobId, Opened, Work},
    session::best_of::BestOf,
};

/// What the session works on, lent for one call.
pub struct Daemon<'a> {
    pub vfs: &'a mut Vfs,
    pub db: &'a blaze_query::Database,
    pub builder: &'a mut dyn Builder,
    /// For [`Response::Status`].
    pub memory_bytes: u64,
    pub budget_bytes: u64,
}

/// The requests under way, the views each connection holds, and the jobs of the build.
///
/// Each call adds to `out` the replies it makes, in order. The daemon flushes the VFS after
/// each call.
pub struct Session {
    next_job: u64,
    next_serial: u64,
    /// Every request not yet over, by the session's own number for it; a gone connection's
    /// stay until their jobs finish.
    pending: FxHashMap<u64, Pending>,
    /// The number of each request of a live connection not yet over, ordered so a gone
    /// connection's are one range.
    live: BTreeMap<RequestRef, u64>,
    /// Each job's request, and its candidate for a best-of-N.
    jobs: FxHashMap<JobId, (u64, Option<u32>)>,
    /// Commits waiting to be durable.
    tickets: FxHashMap<Ticket, u64>,
    /// The views each connection opened with [`Request::Open`], by root, one per open.
    held: BTreeMap<Conn, BTreeMap<ContentId, Vec<ViewId>>>,
    /// The database's view of the last job that ended on each of the latest [`KEPT`] roots,
    /// oldest first: what [`Request::Why`] reads, since a view's reruns are its own.
    kept: VecDeque<(ContentId, ViewId)>,
}

/// How many roots' last builds [`Session`] keeps for [`Request::Why`].
pub const KEPT: usize = 64;

struct Pending {
    sink: Sink,
    what: Kind,
}

/// Where a request's replies go, and whether they go at all.
#[derive(Clone, Copy)]
struct Sink {
    to: RequestRef,
    /// The connection is gone: nothing more is sent.
    gone: bool,
}

impl Sink {
    fn send(self, out: &mut Vec<Reply>, response: Response) {
        if !self.gone {
            out.push(Reply { to: self.to, response });
        }
    }
}

enum Kind {
    Commit { root: ContentId, rebased: bool, ticket: Ticket, head: String },
    /// One job of the build, with the views it holds, and the one whose database view is kept
    /// for [`Request::Why`] when it ends. `owes_work` until a build, test or run has reported
    /// [`Response::Work`]: one that ends without it fails, naming it.
    Job {
        job: JobId,
        cancelled: bool,
        views: Vec<Opened>,
        kept: Option<Opened>,
        owes_work: bool,
    },
    BestOf(BestOf),
}

impl Kind {
    fn cancel(&mut self, builder: &mut dyn Builder) {
        match self {
            Kind::Commit { .. } => {}
            Kind::Job { job, cancelled, .. } => {
                if !*cancelled {
                    *cancelled = true;
                    builder.cancel(*job);
                }
            }
            Kind::BestOf(best) => best.cancel(builder),
        }
    }

    fn views(&self) -> usize {
        match self {
            Kind::Commit { .. } => 0,
            Kind::Job { views, .. } => views.len(),
            Kind::BestOf(best) => best.views(),
        }
    }
}

impl Default for Session {
    fn default() -> Session {
        Session::new()
    }
}

impl Session {
    pub fn new() -> Session {
        Session {
            next_job: 0,
            next_serial: 0,
            pending: FxHashMap::default(),
            live: BTreeMap::new(),
            jobs: FxHashMap::default(),
            tickets: FxHashMap::default(),
            held: BTreeMap::new(),
            kept: VecDeque::new(),
        }
    }

    pub fn arrive(&mut self, cx: &mut Daemon<'_>, arrived: Arrived, out: &mut Vec<Reply>) {
        match arrived {
            Arrived::Request { from, request } => {
                if self.live.contains_key(&from) {
                    let error = ApiError::new(
                        format!("request {} is already under way on this connection", from.id),
                        Some(format!("request {}", from.id)),
                    );
                    fail(out, from, error);
                    return;
                }
                if let Err(error) = self.request(cx, from, request, out) {
                    fail(out, from, error);
                }
            }
            Arrived::Gone { conn } => self.gone(cx, conn),
        }
    }

    /// A job's response ([`Builder`]'s side). One of a job that is unknown or has finished is
    /// dropped: a job can finish before its cancel reaches the build.
    pub fn report(
        &mut self,
        _cx: &mut Daemon<'_>,
        job: JobId,
        response: Response,
        out: &mut Vec<Reply>,
    ) {
        assert!(
            !matches!(response, Response::Done { .. } | Response::Error(_)),
            "job {job:?} reported {response:?}: a job ends through `Session::finish`"
        );
        let Some(&(serial, candidate)) = self.jobs.get(&job) else { return };
        let pending = self.pending.get_mut(&serial).expect("a job's request is pending");
        let sink = pending.sink;
        match (&mut pending.what, candidate) {
            (Kind::BestOf(best), Some(candidate)) => {
                best.count(candidate, &response);
                sink.send(out, of(candidate, response));
            }
            (Kind::Job { owes_work, .. }, _) => {
                if matches!(response, Response::Work(_)) {
                    *owes_work = false;
                }
                sink.send(out, response);
            }
            _ => sink.send(out, response),
        }
    }

    /// A job's end: `ok`, and the error when it could not run. Its views are closed. One of a
    /// job that is unknown or has finished is dropped.
    pub fn finish(
        &mut self,
        cx: &mut Daemon<'_>,
        job: JobId,
        ok: bool,
        error: Option<ApiError>,
        out: &mut Vec<Reply>,
    ) {
        let Some((serial, candidate)) = self.jobs.remove(&job) else { return };
        let pending = self.pending.get_mut(&serial).expect("a job's request is pending");
        let sink = pending.sink;
        let ok = ok && error.is_none();
        let over = match (&mut pending.what, candidate) {
            (Kind::BestOf(best), Some(candidate)) => {
                let ran = error.is_none();
                match best.finish(candidate, ok, ran) {
                    Some(opened) if ran => {
                        cx.vfs.close(opened.files);
                        keep(&mut self.kept, cx.db, opened);
                    }
                    Some(opened) => close(cx, opened),
                    None => {}
                }
                if let Some(error) = error {
                    sink.send(out, of(candidate, Response::Error(error)));
                }
                sink.send(out, of(candidate, Response::Done { ok }));
                if best.running() == 0 {
                    sink.send(out, Response::Ranked(best.ranked()));
                    sink.send(out, Response::Done { ok: best.ok() });
                    true
                } else {
                    false
                }
            }
            (Kind::Job { views, kept, owes_work, cancelled, .. }, None) => {
                let error = match error {
                    None if *owes_work && !*cancelled => Some(ApiError::new(
                        format!(
                            "the build of {:x} ended without reporting what it did \
                             (`Response::Work`)",
                            views.first().expect("a build, test or run has its view").view.root
                        ),
                        Some(format!("job {}", job.0)),
                    )),
                    error => error,
                };
                let ok = ok && error.is_none();
                for opened in views.drain(..) {
                    if error.is_none() && *kept == Some(opened) {
                        cx.vfs.close(opened.files);
                        keep(&mut self.kept, cx.db, opened);
                    } else {
                        close(cx, opened);
                    }
                }
                if let Some(error) = error {
                    sink.send(out, Response::Error(error));
                }
                sink.send(out, Response::Done { ok });
                true
            }
            _ => unreachable!("job {job:?}'s candidate does not match its request"),
        };
        if over {
            self.end(serial);
        }
    }

    /// Commits the VFS now says are durable (what [`Vfs::complete`] returned). Tickets of
    /// commits the session did not make (the watcher's, a gone connection's) are not its own
    /// and are passed over.
    pub fn durable(&mut self, _cx: &mut Daemon<'_>, tickets: &[Ticket], out: &mut Vec<Reply>) {
        for ticket in tickets {
            let Some(serial) = self.tickets.remove(ticket) else { continue };
            let pending = self.end(serial);
            let Kind::Commit { root, rebased, .. } = pending.what else {
                unreachable!("ticket {ticket:?} is of a request that is no commit")
            };
            pending.sink.send(out, Response::Committed { root, rebased });
            pending.sink.send(out, Response::Done { ok: true });
        }
    }

    fn request(
        &mut self,
        cx: &mut Daemon<'_>,
        req: RequestRef,
        request: Request,
        out: &mut Vec<Reply>,
    ) -> Result<(), ApiError> {
        match request {
            Request::Commit { head, base, ops } => {
                let commit = cx
                    .vfs
                    .commit(&head, base, &ops)
                    .map_err(|error| commit_error(&head, base, &error))?;
                let serial = self.begin(
                    req,
                    Kind::Commit {
                        root: commit.root,
                        rebased: commit.rebased,
                        ticket: commit.ticket,
                        head,
                    },
                );
                self.tickets.insert(commit.ticket, serial);
            }
            Request::Check { view, targets } => self.build(cx, req, &view, Work::Check(targets))?,
            Request::Build { view, targets } => self.build(cx, req, &view, Work::Build(targets))?,
            Request::Test { view, targets, filter, all } => {
                self.build(cx, req, &view, Work::Test { targets, filter, all })?
            }
            Request::Run { view, targets, bin, args } => {
                self.build(cx, req, &view, Work::Run { targets, bin, args })?
            }
            Request::Verify { view, targets } => {
                self.build(cx, req, &view, Work::Verify(targets))?
            }
            Request::CodegenInputs { view, targets } => {
                self.build(cx, req, &view, Work::CodegenInputs(targets))?
            }
            Request::Trace { view, targets } => self.build(cx, req, &view, Work::Trace(targets))?,
            Request::Tables { view } => self.build(cx, req, &view, Work::Tables)?,
            Request::Reach { from, to, targets } => {
                let from = resolve(cx.vfs, &from)?;
                let to = resolve(cx.vfs, &to)?;
                let from = open(cx, from)?;
                let to = match open(cx, to) {
                    Ok(to) => to,
                    Err(error) => {
                        close(cx, from);
                        return Err(error);
                    }
                };
                let changes = cx.vfs.diff(from.view.root, to.view.root);
                self.start(cx, req, Some(to), vec![from, to], Work::Reach { from, changes, targets });
            }
            Request::Snapshot => self.start(cx, req, None, Vec::new(), Work::Snapshot),
            Request::BestOf { base, candidates, targets, test, filter } => {
                let base = resolve(cx.vfs, &base)?;
                if candidates.is_empty() {
                    return Err(ApiError::new(
                        format!("a best-of on {base:x} has no candidates"),
                        Some(format!("root {base:x}")),
                    ));
                }
                let sink = Sink { to: req, gone: false };
                let mut best = BestOf::new(base);
                let mut starts = Vec::new();
                for ops in &candidates {
                    let root = cx.vfs.apply(base, ops).map_err(|error| {
                        ApiError::new(
                            format!("the candidate's ops don't apply to {base:x}: {error}"),
                            Some(op_path(&error)),
                        )
                    });
                    match root.and_then(|root| open(cx, root)) {
                        Ok(opened) => {
                            let job = self.job();
                            let candidate = best.started(job, opened);
                            starts.push((job, candidate, opened));
                        }
                        Err(error) => {
                            let candidate = best.refused();
                            sink.send(out, of(candidate, Response::Error(error)));
                            sink.send(out, of(candidate, Response::Done { ok: false }));
                        }
                    }
                }
                if starts.is_empty() {
                    sink.send(out, Response::Ranked(best.ranked()));
                    sink.send(out, Response::Done { ok: false });
                    return Ok(());
                }
                let serial = self.begin(req, Kind::BestOf(best));
                for (job, candidate, opened) in starts {
                    self.jobs.insert(job, (serial, Some(candidate)));
                    let work = if test {
                        Work::Test { targets: targets.clone(), filter: filter.clone(), all: false }
                    } else {
                        Work::Check(targets.clone())
                    };
                    cx.builder.start(job, Some(opened), work);
                }
            }
            Request::Diff { from, to } => {
                let from = resolve(cx.vfs, &from)?;
                let to = resolve(cx.vfs, &to)?;
                let changes = cx.vfs.diff(from, to);
                answer(out, req, Response::Diff { changes });
            }
            Request::Why { view, query } => {
                let root = resolve(cx.vfs, &view)?;
                let Some(&(_, queries)) = self.kept.iter().find(|(kept, _)| *kept == root) else {
                    return Err(ApiError::new(
                        format!(
                            "no build of {root:x} has ended among the latest {KEPT} roots built: \
                             build it, then ask why"
                        ),
                        Some(format!("root {root:x}")),
                    ));
                };
                let Some(why) = cx.db.why(queries, &query) else {
                    return Err(ApiError::new(
                        format!("{} did not rerun in the last build of {root:x}", query_name(&query)),
                        Some(query_name(&query)),
                    ));
                };
                answer(out, req, Response::Why(why));
            }
            Request::Status => {
                let mut requests: Vec<u64> = self.live.keys().map(|it| it.id).collect();
                requests.sort_unstable();
                let status = Status {
                    requests,
                    views: self.views(),
                    memory_bytes: cx.memory_bytes,
                    budget_bytes: cx.budget_bytes,
                };
                answer(out, req, Response::Status(status));
            }
            Request::Open { view } => {
                let root = resolve(cx.vfs, &view)?;
                let files = cx.vfs.open(View { root }).map_err(|error| open_error(root, &error))?;
                self.held.entry(req.conn).or_default().entry(root).or_default().push(files);
                answer(out, req, Response::Opened { root });
            }
            Request::Close { root } => {
                let Some(held) = self.held.get_mut(&req.conn).filter(|it| it.contains_key(&root))
                else {
                    return Err(ApiError::new(
                        format!("this connection holds no open view of {root:x}"),
                        Some(format!("root {root:x}")),
                    ));
                };
                let views = held.get_mut(&root).expect("a held root");
                let files = views.pop().expect("a held root has an open view");
                if views.is_empty() {
                    held.remove(&root);
                }
                if held.is_empty() {
                    self.held.remove(&req.conn);
                }
                cx.vfs.close(files);
                out.push(Reply { to: req, response: Response::Done { ok: true } });
            }
            Request::Cancel { id } => {
                let target = RequestRef { conn: req.conn, id };
                let Some(&serial) = self.live.get(&target) else {
                    return Err(ApiError::new(
                        format!("no request {id} is under way on this connection"),
                        Some(format!("request {id}")),
                    ));
                };
                let pending = self.pending.get_mut(&serial).expect("a live request is pending");
                if let Kind::Commit { head, .. } = &pending.what {
                    return Err(ApiError::new(
                        format!(
                            "request {id} is a commit to head `{head}` that is made and waiting \
                             to be durable: it can't be cancelled"
                        ),
                        Some(format!("request {id}")),
                    ));
                }
                pending.what.cancel(cx.builder);
                out.push(Reply { to: req, response: Response::Done { ok: true } });
            }
        }
        Ok(())
    }

    /// A request on one view, built by one job.
    fn build(
        &mut self,
        cx: &mut Daemon<'_>,
        req: RequestRef,
        view: &ViewRef,
        work: Work,
    ) -> Result<(), ApiError> {
        let root = resolve(cx.vfs, view)?;
        let opened = open(cx, root)?;
        self.start(cx, req, Some(opened), vec![opened], work);
        Ok(())
    }

    fn start(
        &mut self,
        cx: &mut Daemon<'_>,
        req: RequestRef,
        view: Option<Opened>,
        views: Vec<Opened>,
        work: Work,
    ) {
        let job = self.job();
        let kept = view.filter(|_| incremental(&work));
        let owes_work = matches!(work, Work::Build(_) | Work::Test { .. } | Work::Run { .. });
        let serial =
            self.begin(req, Kind::Job { job, cancelled: false, views, kept, owes_work });
        self.jobs.insert(job, (serial, None));
        cx.builder.start(job, view, work);
    }

    fn job(&mut self) -> JobId {
        let job = JobId(self.next_job);
        self.next_job += 1;
        job
    }

    fn begin(&mut self, req: RequestRef, what: Kind) -> u64 {
        let serial = self.next_serial;
        self.next_serial += 1;
        self.pending.insert(serial, Pending { sink: Sink { to: req, gone: false }, what });
        self.live.insert(req, serial);
        serial
    }

    fn end(&mut self, serial: u64) -> Pending {
        let pending = self.pending.remove(&serial).expect("an ended request was pending");
        if !pending.sink.gone {
            self.live.remove(&pending.sink.to);
        }
        pending
    }

    fn gone(&mut self, cx: &mut Daemon<'_>, conn: Conn) {
        let requests: Vec<(RequestRef, u64)> = self
            .live
            .range(RequestRef { conn, id: 0 }..=RequestRef { conn, id: u64::MAX })
            .map(|(req, serial)| (*req, *serial))
            .collect();
        for (req, serial) in requests {
            self.live.remove(&req);
            let pending = self.pending.get_mut(&serial).expect("a live request is pending");
            pending.sink.gone = true;
            if let Kind::Commit { ticket, .. } = pending.what {
                self.pending.remove(&serial);
                self.tickets.remove(&ticket);
            } else {
                pending.what.cancel(cx.builder);
            }
        }
        for files in self.held.remove(&conn).into_iter().flat_map(BTreeMap::into_values).flatten() {
            cx.vfs.close(files);
        }
    }

    /// The views the session holds open: the connections' and the jobs'.
    fn views(&self) -> u32 {
        let held: usize = self.held.values().flat_map(BTreeMap::values).map(Vec::len).sum();
        let jobs: usize = self.pending.values().map(|it| it.what.views()).sum();
        u32::try_from(held + jobs).expect("fewer than 2^32 open views")
    }
}

fn answer(out: &mut Vec<Reply>, to: RequestRef, response: Response) {
    out.push(Reply { to, response });
    out.push(Reply { to, response: Response::Done { ok: true } });
}

fn of(candidate: u32, response: Response) -> Response {
    Response::Of { candidate, response: Box::new(response) }
}

fn fail(out: &mut Vec<Reply>, to: RequestRef, error: ApiError) {
    out.push(Reply { to, response: Response::Error(error) });
    out.push(Reply { to, response: Response::Done { ok: false } });
}

/// The root a request names: a head never written is the empty tree.
fn resolve(vfs: &mut Vfs, view: &ViewRef) -> Result<ContentId, ApiError> {
    match view {
        ViewRef::Head(head) => Ok(vfs.head(head).unwrap_or_else(|| vfs.empty())),
        ViewRef::Root(view) => {
            if vfs.has(view.root) {
                Ok(view.root)
            } else {
                Err(ApiError::new(
                    format!("no root {:x} is known", view.root),
                    Some(format!("root {:x}", view.root)),
                ))
            }
        }
        ViewRef::WhatIf { base, ops } => {
            let base = resolve(vfs, base)?;
            vfs.apply(base, ops).map_err(|error| {
                ApiError::new(
                    format!("the what-if's ops don't apply to {base:x}: {error}"),
                    Some(op_path(&error)),
                )
            })
        }
    }
}

/// `root` opened in both the VFS and the database, for a job.
fn open(cx: &mut Daemon<'_>, root: ContentId) -> Result<Opened, ApiError> {
    let view = View { root };
    let files = cx.vfs.open(view).map_err(|error| open_error(root, &error))?;
    let queries = cx.db.open(view);
    Ok(Opened { view, files, queries })
}

fn close(cx: &mut Daemon<'_>, opened: Opened) {
    cx.vfs.close(opened.files);
    cx.db.close(opened.queries);
}

/// Whether `work` runs the view's queries incrementally, so their reruns answer
/// [`Request::Why`]: a reach builds nothing, a verify builds from nothing, tables only count.
fn incremental(work: &Work) -> bool {
    match work {
        Work::Check(_)
        | Work::Build(_)
        | Work::Test { .. }
        | Work::Run { .. }
        | Work::CodegenInputs(_)
        | Work::Trace(_) => true,
        Work::Reach { .. } | Work::Verify(_) | Work::Snapshot | Work::Tables => false,
    }
}

/// `opened`'s database view kept as its root's last build, closing the one it replaces and,
/// past [`KEPT`] roots, the oldest.
fn keep(kept: &mut VecDeque<(ContentId, ViewId)>, db: &blaze_query::Database, opened: Opened) {
    let root = opened.view.root;
    if let Some(at) = kept.iter().position(|(it, _)| *it == root) {
        let (_, old) = kept.remove(at).expect("a kept root");
        db.close(old);
    }
    if kept.len() == KEPT {
        let (_, oldest) = kept.pop_front().expect("KEPT roots are kept");
        db.close(oldest);
    }
    kept.push_back((root, opened.queries));
}

fn open_error(root: ContentId, error: &CommitError) -> ApiError {
    ApiError::new(format!("opening {root:x}: {error}"), Some(format!("root {root:x}")))
}

fn commit_error(head: &str, base: ContentId, error: &CommitError) -> ApiError {
    match error {
        CommitError::UnknownBase(root) => ApiError::new(
            format!("the base {root:x} of a commit to head `{head}` is no root this daemon has"),
            Some(head.to_owned()),
        ),
        CommitError::Conflict { path } => ApiError::new(
            format!(
                "head `{head}` moved past {base:x} and changed {path} in a way the commit's ops \
                 can't be moved past"
            ),
            Some(path.clone()),
        ),
        CommitError::Op(error) => ApiError::new(
            format!("a commit to head `{head}` on {base:x}: {error}"),
            Some(op_path(error)),
        ),
    }
}

/// Where an op failed: its path, or the root it was given.
fn op_path(error: &OpError) -> String {
    match error {
        OpError::BadPath(path)
        | OpError::NotFound(path)
        | OpError::IsADirectory(path)
        | OpError::NotADirectory(path)
        | OpError::Exists(path)
        | OpError::OutOfRange { path, .. }
        | OpError::BadLink { path, .. } => path.clone(),
        OpError::UnknownRoot(root) => format!("root {root:x}"),
    }
}

fn query_name(query: &QueryKey) -> String {
    format!("the query of kind {:#06x} on {:x}", query.kind.0, query.input)
}

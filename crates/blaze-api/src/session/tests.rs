use std::collections::{BTreeMap, BTreeSet};

use blaze_core::{Conn, FileName};
use blaze_ids::ContentId;
use blaze_query::{Database, QueryKey, QueryKind};
use blaze_vfs::{Change, ChangeKind, Mode, Op, View, Vfs};

use super::{Daemon, Session};
use crate::{
    ApiError, Arrived, Diagnostic, Level, Reach, Reply, Request, RequestRef, Response, Status,
    Targets, TestOutcome, TestResult, ViewRef,
    build::{Builder, JobId, Opened, Work},
};

/// A build that records what it was asked.
#[derive(Default)]
struct Fake {
    started: Vec<(JobId, Option<Opened>, Work)>,
    cancelled: Vec<JobId>,
}

impl Builder for Fake {
    fn start(&mut self, job: JobId, view: Option<Opened>, work: Work) {
        self.started.push((job, view, work));
    }

    fn cancel(&mut self, job: JobId) {
        self.cancelled.push(job);
    }
}

struct World {
    vfs: Vfs,
    db: Database,
    builder: Fake,
    session: Session,
}

impl World {
    fn new() -> World {
        let (vfs, _) = Vfs::recover(FileName::new("journal").unwrap(), 0, &[]);
        World { vfs, db: Database::empty(), builder: Fake::default(), session: Session::new() }
    }

    fn call(&mut self, f: impl FnOnce(&mut Session, &mut Daemon<'_>, &mut Vec<Reply>)) -> Vec<Reply> {
        let mut out = Vec::new();
        let mut cx = Daemon {
            vfs: &mut self.vfs,
            db: &self.db,
            builder: &mut self.builder,
            memory_bytes: 10,
            budget_bytes: 20,
        };
        f(&mut self.session, &mut cx, &mut out);
        out
    }

    fn request(&mut self, conn: u64, id: u64, request: Request) -> Vec<Reply> {
        let from = RequestRef { conn: Conn(conn), id };
        self.call(|session, cx, out| session.arrive(cx, Arrived::Request { from, request }, out))
    }

    fn gone(&mut self, conn: u64) -> Vec<Reply> {
        self.call(|session, cx, out| session.arrive(cx, Arrived::Gone { conn: Conn(conn) }, out))
    }

    fn report(&mut self, job: JobId, response: Response) -> Vec<Reply> {
        self.call(|session, cx, out| session.report(cx, job, response, out))
    }

    fn finish(&mut self, job: JobId, ok: bool, error: Option<ApiError>) -> Vec<Reply> {
        self.call(|session, cx, out| session.finish(cx, job, ok, error, out))
    }

    /// Every commit the session waits on made durable.
    fn durable(&mut self) -> Vec<Reply> {
        let tickets: Vec<_> = self.session.tickets.keys().copied().collect();
        self.call(|session, cx, out| session.durable(cx, &tickets, out))
    }

    fn last_job(&self) -> JobId {
        self.builder.started.last().expect("a job started").0
    }

    /// `main` with `files` written, made durable.
    fn commit(&mut self, files: &[(&str, &str)]) -> ContentId {
        let base = self.vfs.head("main").unwrap_or_else(|| self.vfs.empty());
        let ops = files
            .iter()
            .map(|(path, bytes)| write(path, bytes))
            .collect();
        assert_eq!(self.request(9, 9, Request::Commit { head: "main".to_owned(), base, ops }), []);
        let replies = self.durable();
        assert!(matches!(replies[1].response, Response::Done { ok: true }), "{replies:?}");
        self.vfs.head("main").unwrap()
    }
}

fn targets() -> Targets {
    Targets {
        packages: Vec::new(),
        profile: "dev".to_owned(),
        target: None,
        features: Vec::new(),
        all_features: false,
        no_default_features: false,
    }
}

fn head(name: &str) -> ViewRef {
    ViewRef::Head(name.to_owned())
}

fn to(conn: u64, id: u64) -> RequestRef {
    RequestRef { conn: Conn(conn), id }
}

fn reply(conn: u64, id: u64, response: Response) -> Reply {
    Reply { to: to(conn, id), response }
}

fn done(conn: u64, id: u64, ok: bool) -> Reply {
    reply(conn, id, Response::Done { ok })
}

fn diagnostic(level: Level) -> Response {
    Response::Diagnostic(Diagnostic {
        level,
        code: None,
        message: "a diagnostic".to_owned(),
        spans: Vec::new(),
        children: Vec::new(),
        facts: Vec::new(),
    })
}

fn test_result(outcome: TestOutcome) -> Response {
    Response::TestResult(TestResult {
        binary: "tests".to_owned(),
        name: "a_test".to_owned(),
        outcome,
        output: Vec::new(),
        millis: 1,
    })
}

fn of(candidate: u32, response: Response) -> Response {
    Response::Of { candidate, response: Box::new(response) }
}

fn error_at(replies: &[Reply]) -> Option<String> {
    replies.iter().find_map(|it| match &it.response {
        Response::Error(error) => error.at.clone(),
        _ => None,
    })
}

/// The candidate of an `Of` error, and where the error was.
fn of_error_at(reply: &Reply) -> Option<(u32, Option<String>)> {
    let Response::Of { candidate, response } = &reply.response else { return None };
    let Response::Error(error) = &**response else { return None };
    Some((*candidate, error.at.clone()))
}

fn write(path: &str, bytes: &str) -> Op {
    Op::Write { path: path.to_owned(), bytes: bytes.as_bytes().to_vec(), mode: Mode::Regular }
}

#[test]
fn a_check_forwards_its_reports_then_ends_and_closes_its_view() {
    let mut world = World::new();
    assert_eq!(world.request(1, 1, Request::Check { view: head("main"), targets: targets() }), []);
    let (job, view, work) = world.builder.started[0].clone();
    assert_eq!(view.unwrap().view, View { root: world.vfs.empty() }, "a head never written is empty");
    assert_eq!(work, Work::Check(targets()));
    assert_eq!(world.session.views(), 1);

    let warning = diagnostic(Level::Warning);
    assert_eq!(world.report(job, warning.clone()), [reply(1, 1, warning)]);
    assert_eq!(world.finish(job, false, None), [done(1, 1, false)]);
    assert_eq!(world.session.views(), 0);
}

#[test]
fn a_job_that_could_not_run_ends_with_its_error() {
    let mut world = World::new();
    world.request(1, 1, Request::Build { view: head("main"), targets: targets() });
    let job = world.last_job();
    let error = ApiError::new("no package `x`", Some("x".to_owned()));
    assert_eq!(
        world.finish(job, true, Some(error.clone())),
        [reply(1, 1, Response::Error(error)), done(1, 1, false)]
    );
}

#[test]
fn test_run_and_verify_start_their_work() {
    let mut world = World::new();
    let filter = vec!["parse".to_owned()];
    world.request(1, 1, Request::Test { view: head("main"), targets: targets(), filter: filter.clone(), all: true });
    world.request(1, 2, Request::Run { view: head("main"), targets: targets(), bin: None, args: vec!["-v".to_owned()] });
    world.request(1, 3, Request::Verify { view: head("main"), targets: targets() });
    let works: Vec<Work> = world.builder.started.iter().map(|it| it.2.clone()).collect();
    assert_eq!(
        works,
        [
            Work::Test { targets: targets(), filter, all: true },
            Work::Run { targets: targets(), bin: None, args: vec!["-v".to_owned()] },
            Work::Verify(targets()),
        ]
    );
}

#[test]
fn a_commit_is_answered_only_once_durable() {
    let mut world = World::new();
    let base = world.vfs.empty();
    let commit = Request::Commit { head: "main".to_owned(), base, ops: vec![write("src/lib.rs", "")] };
    assert_eq!(world.request(1, 1, commit), []);
    let root = world.vfs.head("main").unwrap();
    assert_eq!(world.call(|session, cx, out| session.durable(cx, &[], out)), []);
    assert_eq!(
        world.durable(),
        [reply(1, 1, Response::Committed { root, rebased: false }), done(1, 1, true)]
    );
    assert_eq!(world.durable(), [], "answered once");
}

#[test]
fn a_commit_on_an_unknown_base_names_the_head() {
    let mut world = World::new();
    let base = ContentId([7; 32]);
    let replies = world.request(1, 1, Request::Commit { head: "main".to_owned(), base, ops: Vec::new() });
    assert_eq!(error_at(&replies), Some("main".to_owned()));
    assert_eq!(replies.last(), Some(&done(1, 1, false)));
}

#[test]
fn a_commit_whose_op_fails_names_the_path() {
    let mut world = World::new();
    let base = world.vfs.empty();
    let ops = vec![Op::Delete { path: "gone.rs".to_owned() }];
    let replies = world.request(1, 1, Request::Commit { head: "main".to_owned(), base, ops });
    assert_eq!(error_at(&replies), Some("gone.rs".to_owned()));
    assert_eq!(replies.len(), 2);
}

#[test]
fn a_what_if_resolves_inside_out_and_names_a_path_that_fails() {
    let mut world = World::new();
    let inner = ViewRef::WhatIf { base: Box::new(head("main")), ops: vec![write("a.rs", "a")] };
    let outer = ViewRef::WhatIf { base: Box::new(inner.clone()), ops: vec![write("b.rs", "b")] };
    world.request(1, 1, Request::Check { view: outer, targets: targets() });
    let root = world.builder.started[0].1.unwrap().view.root;
    let changes = world.vfs.diff(world.vfs.empty(), root);
    let paths: Vec<&str> = changes.iter().map(|it| it.path.as_str()).collect();
    assert_eq!(paths, ["a.rs", "b.rs"]);
    assert_eq!(world.vfs.head("main"), None, "a what-if moves no head");

    let bad = ViewRef::WhatIf { base: Box::new(inner), ops: vec![Op::Delete { path: "c.rs".to_owned() }] };
    let replies = world.request(1, 2, Request::Check { view: bad, targets: targets() });
    assert_eq!(error_at(&replies), Some("c.rs".to_owned()));
    assert_eq!(replies.last(), Some(&done(1, 2, false)));
    assert_eq!(world.builder.started.len(), 1);
}

#[test]
fn an_unknown_root_is_an_error_naming_it() {
    let mut world = World::new();
    let root = ContentId([3; 32]);
    let replies = world.request(1, 1, Request::Open { view: ViewRef::Root(View { root }) });
    assert_eq!(error_at(&replies), Some(format!("root {root:x}")));
}

#[test]
fn opens_are_counted_and_each_needs_a_close() {
    let mut world = World::new();
    let root = world.vfs.empty();
    assert_eq!(world.request(1, 1, Request::Open { view: head("main") }), [reply(1, 1, Response::Opened { root }), done(1, 1, true)]);
    world.request(1, 2, Request::Open { view: ViewRef::Root(View { root }) });
    assert_eq!(world.session.views(), 2);
    assert_eq!(world.request(2, 1, Request::Close { root }).last(), Some(&done(2, 1, false)), "another connection's");
    assert_eq!(world.request(1, 3, Request::Close { root }), [done(1, 3, true)]);
    assert_eq!(world.request(1, 4, Request::Close { root }), [done(1, 4, true)]);
    let replies = world.request(1, 5, Request::Close { root });
    assert_eq!(error_at(&replies), Some(format!("root {root:x}")));
    assert_eq!(world.session.views(), 0);
}

#[test]
fn a_diff_lists_the_changes() {
    let mut world = World::new();
    world.commit(&[("src/lib.rs", "fn f() {}")]);
    let replies = world.request(1, 1, Request::Diff { from: head("empty"), to: head("main") });
    let changes = vec![Change { path: "src/lib.rs".to_owned(), kind: ChangeKind::Added }];
    assert_eq!(replies, [reply(1, 1, Response::Diff { changes }), done(1, 1, true)]);
}

#[test]
fn why_of_a_query_that_did_not_rerun_names_it() {
    let mut world = World::new();
    let query = QueryKey { kind: QueryKind(0x2001), input: ContentId([1; 32]) };
    let replies = world.request(1, 1, Request::Why { view: head("main"), query });
    assert_eq!(error_at(&replies), Some(super::query_name(&query)));
    let Response::Error(error) = &replies[0].response else { panic!("{replies:?}") };
    assert!(error.what.contains("did not rerun in the last build of"), "{}", error.what);
    assert_eq!(replies[1], done(1, 1, false));
}

#[test]
fn status_lists_every_connections_requests_sorted() {
    let mut world = World::new();
    world.request(2, 8, Request::Check { view: head("main"), targets: targets() });
    world.request(1, 5, Request::Check { view: head("main"), targets: targets() });
    world.request(1, 1, Request::Open { view: head("main") });
    let status = Status { requests: vec![5, 8], views: 3, memory_bytes: 10, budget_bytes: 20 };
    assert_eq!(world.request(3, 1, Request::Status), [reply(3, 1, Response::Status(status)), done(3, 1, true)]);
}

#[test]
fn a_reach_diffs_the_views_and_runs_on_the_later() {
    let mut world = World::new();
    let root = world.commit(&[("src/lib.rs", "")]);
    world.request(1, 1, Request::Reach { from: head("empty"), to: head("main"), targets: targets() });
    let (job, view, work) = world.builder.started[0].clone();
    assert_eq!(view.unwrap().view.root, root);
    let Work::Reach { from, changes, .. } = work else { panic!("{work:?}") };
    assert_eq!(from.view.root, world.vfs.empty());
    assert_eq!(changes, [Change { path: "src/lib.rs".to_owned(), kind: ChangeKind::Added }]);
    assert_eq!(world.session.views(), 2);

    let reached = Response::Reached(Reach::default());
    assert_eq!(world.report(job, reached.clone()), [reply(1, 1, reached)]);
    assert_eq!(world.finish(job, true, None), [done(1, 1, true)]);
    assert_eq!(world.session.views(), 0);
}

#[test]
fn a_snapshot_has_no_view() {
    let mut world = World::new();
    world.request(1, 1, Request::Snapshot);
    let (job, view, work) = world.builder.started[0].clone();
    assert_eq!((view, work), (None, Work::Snapshot));
    let snapshotted = Response::Snapshotted { heads: Vec::new() };
    assert_eq!(world.report(job, snapshotted.clone()), [reply(1, 1, snapshotted)]);
    assert_eq!(world.finish(job, true, None), [done(1, 1, true)]);
}

#[test]
fn a_cancel_cancels_the_jobs_and_the_request_still_ends_once() {
    let mut world = World::new();
    world.request(1, 1, Request::Check { view: head("main"), targets: targets() });
    let job = world.last_job();
    assert_eq!(world.request(1, 2, Request::Cancel { id: 1 }), [done(1, 2, true)]);
    assert_eq!(world.request(1, 3, Request::Cancel { id: 1 }), [done(1, 3, true)]);
    assert_eq!(world.builder.cancelled, [job], "cancelled once");
    assert_eq!(world.finish(job, false, None), [done(1, 1, false)]);

    let replies = world.request(1, 4, Request::Cancel { id: 1 });
    assert_eq!(error_at(&replies), Some("request 1".to_owned()), "it is over");
    let replies = world.request(2, 5, Request::Cancel { id: 1 });
    assert_eq!(error_at(&replies), Some("request 1".to_owned()), "another connection's");
}

#[test]
fn a_commit_waiting_to_be_durable_cant_be_cancelled() {
    let mut world = World::new();
    let base = world.vfs.empty();
    world.request(1, 1, Request::Commit { head: "main".to_owned(), base, ops: vec![write("a", "")] });
    let replies = world.request(1, 2, Request::Cancel { id: 1 });
    assert_eq!(error_at(&replies), Some("request 1".to_owned()));
    assert_eq!(replies.last(), Some(&done(1, 2, false)));
    let replies = world.durable();
    assert_eq!(replies.last(), Some(&done(1, 1, true)));
}

#[test]
fn a_gone_connection_is_cancelled_closed_and_sent_nothing() {
    let mut world = World::new();
    world.request(1, 1, Request::Check { view: head("main"), targets: targets() });
    let job = world.last_job();
    world.request(1, 2, Request::Open { view: head("main") });
    let base = world.vfs.empty();
    world.request(1, 3, Request::Commit { head: "main".to_owned(), base, ops: vec![write("a", "")] });
    world.request(2, 1, Request::Snapshot);
    let other = world.last_job();
    assert_eq!(world.session.views(), 2);

    assert_eq!(world.gone(1), []);
    assert_eq!(world.builder.cancelled, [job]);
    assert_eq!(world.session.views(), 1, "the job's view stays until it finishes");
    assert_eq!(world.report(job, diagnostic(Level::Error)), []);
    assert_eq!(world.finish(job, false, None), []);
    assert_eq!(world.durable(), []);
    assert_eq!(world.session.views(), 0);
    assert_eq!(world.session.pending.len(), 1, "only connection 2's snapshot");

    assert_eq!(world.finish(other, true, None), [done(2, 1, true)]);
    assert!(world.session.pending.is_empty());
}

#[test]
fn a_duplicate_id_is_refused_without_touching_the_first() {
    let mut world = World::new();
    world.request(1, 1, Request::Check { view: head("main"), targets: targets() });
    let job = world.last_job();
    let replies = world.request(1, 1, Request::Snapshot);
    assert_eq!(error_at(&replies), Some("request 1".to_owned()));
    assert_eq!(replies.last(), Some(&done(1, 1, false)));
    assert_eq!(world.builder.started.len(), 1);
    assert_eq!(world.request(2, 1, Request::Snapshot).len(), 0, "another connection may use it");
    assert_eq!(world.finish(job, true, None), [done(1, 1, true)]);
}

#[test]
fn a_report_or_finish_after_the_finish_is_dropped() {
    let mut world = World::new();
    world.request(1, 1, Request::Check { view: head("main"), targets: targets() });
    let job = world.last_job();
    world.finish(job, true, None);
    assert_eq!(world.report(job, diagnostic(Level::Error)), []);
    assert_eq!(world.finish(job, false, None), []);
    assert_eq!(world.report(JobId(99), diagnostic(Level::Error)), []);
}

#[test]
fn job_ids_increase_and_are_never_reused() {
    let mut world = World::new();
    for id in 0..4 {
        world.request(1, id, Request::Snapshot);
        let job = world.last_job();
        world.finish(job, true, None);
    }
    let jobs: Vec<JobId> = world.builder.started.iter().map(|it| it.0).collect();
    assert_eq!(jobs, [JobId(0), JobId(1), JobId(2), JobId(3)]);
}

#[test]
fn best_of_ranks_with_ties_and_an_unappliable_candidate_last() {
    let mut world = World::new();
    let candidates = vec![
        vec![write("a.rs", "a")],
        vec![Op::Delete { path: "missing.rs".to_owned() }],
        vec![write("b.rs", "b")],
        vec![write("c.rs", "c")],
    ];
    let request = Request::BestOf { base: head("main"), candidates, targets: targets(), test: false, filter: Vec::new() };
    let replies = world.request(1, 1, request);
    assert_eq!(replies.len(), 2);
    assert_eq!(of_error_at(&replies[0]), Some((1, Some("missing.rs".to_owned()))));
    assert_eq!(replies[1], reply(1, 1, of(1, Response::Done { ok: false })));
    let jobs: Vec<JobId> = world.builder.started.iter().map(|it| it.0).collect();
    assert_eq!(jobs.len(), 3, "the rest still run");
    assert!(world.builder.started.iter().all(|it| it.2 == Work::Check(targets())));
    assert_eq!(world.session.views(), 3);

    assert_eq!(world.report(jobs[0], diagnostic(Level::Error)), [reply(1, 1, of(0, diagnostic(Level::Error)))]);
    world.report(jobs[1], diagnostic(Level::Warning));
    world.report(jobs[2], diagnostic(Level::Warning));
    world.report(jobs[2], diagnostic(Level::Note));
    assert_eq!(world.finish(jobs[0], false, None), [reply(1, 1, of(0, Response::Done { ok: false }))]);
    world.finish(jobs[2], true, None);
    let replies = world.finish(jobs[1], true, None);
    assert_eq!(replies.len(), 3);
    let Response::Ranked(ranked) = &replies[1].response else { panic!("{replies:?}") };
    let order: Vec<u32> = ranked.iter().map(|it| it.candidate).collect();
    assert_eq!(order, [2, 3, 0, 1], "a tie goes to the earlier candidate");
    assert_eq!((ranked[0].warnings, ranked[2].errors), (1, 1));
    assert_eq!(ranked[3].root, world.vfs.empty(), "the unappliable has the base's root");
    assert!(!ranked[3].ok && ranked[0].ok);
    assert_eq!(replies[2], done(1, 1, true));
    assert_eq!(world.session.views(), 0);
}

#[test]
fn best_of_tests_rank_by_failed_then_passed() {
    let mut world = World::new();
    let candidates = vec![vec![write("a.rs", "a")], vec![write("b.rs", "b")], vec![write("c.rs", "c")]];
    let filter = vec!["it".to_owned()];
    let request = Request::BestOf { base: head("main"), candidates, targets: targets(), test: true, filter: filter.clone() };
    assert_eq!(world.request(1, 1, request), []);
    assert!(world.builder.started.iter().all(|it| it.2 == Work::Test { targets: targets(), filter: filter.clone(), all: false }));
    let jobs: Vec<JobId> = world.builder.started.iter().map(|it| it.0).collect();
    world.report(jobs[0], test_result(TestOutcome::Passed));
    world.report(jobs[0], test_result(TestOutcome::Failed));
    world.report(jobs[1], test_result(TestOutcome::Passed));
    world.report(jobs[2], test_result(TestOutcome::Passed));
    world.report(jobs[2], test_result(TestOutcome::Passed));
    world.report(jobs[2], test_result(TestOutcome::Unreached));
    world.finish(jobs[0], false, None);
    world.finish(jobs[1], true, None);
    let error = ApiError::new("the sandbox could not start", Some("tests".to_owned()));
    let replies = world.finish(jobs[2], false, Some(error.clone()));
    assert_eq!(replies[0], reply(1, 1, of(2, Response::Error(error))));
    let Response::Ranked(ranked) = &replies[2].response else { panic!("{replies:?}") };
    let order: Vec<u32> = ranked.iter().map(|it| it.candidate).collect();
    assert_eq!(order, [1, 0, 2], "one that could not run goes last");
    assert_eq!(replies[3], done(1, 1, true));
}

#[test]
fn best_of_with_no_candidate_that_applies_ends_at_once() {
    let mut world = World::new();
    let candidates = vec![vec![Op::Delete { path: "x".to_owned() }]];
    let request = Request::BestOf { base: head("main"), candidates, targets: targets(), test: false, filter: Vec::new() };
    let replies = world.request(1, 1, request);
    assert!(matches!(replies[2].response, Response::Ranked(ref it) if it.len() == 1));
    assert_eq!(replies[3], done(1, 1, false));
    assert!(world.session.pending.is_empty());
    let replies = world.request(1, 2, Request::BestOf { base: head("main"), candidates: Vec::new(), targets: targets(), test: false, filter: Vec::new() });
    assert_eq!(replies.last(), Some(&done(1, 2, false)));
    assert!(error_at(&replies).is_some());
}

#[test]
fn a_best_of_cancel_cancels_every_running_candidate() {
    let mut world = World::new();
    let candidates = vec![vec![write("a.rs", "a")], vec![write("b.rs", "b")]];
    world.request(1, 1, Request::BestOf { base: head("main"), candidates, targets: targets(), test: false, filter: Vec::new() });
    let jobs: Vec<JobId> = world.builder.started.iter().map(|it| it.0).collect();
    world.finish(jobs[0], true, None);
    world.request(1, 2, Request::Cancel { id: 1 });
    assert_eq!(world.builder.cancelled, [jobs[1]]);
    let replies = world.finish(jobs[1], false, None);
    assert_eq!(replies.last(), Some(&done(1, 1, true)));
}

/// A small xorshift, so the interleavings are the same on every run.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }
}

/// What the replies so far say, checked as they come.
#[derive(Default)]
struct Seen {
    asked: BTreeSet<RequestRef>,
    done: BTreeSet<RequestRef>,
    gone: BTreeSet<Conn>,
}

impl Seen {
    fn take(&mut self, replies: Vec<Reply>) {
        for reply in replies {
            assert!(!self.gone.contains(&reply.to.conn), "a reply to a gone connection: {reply:?}");
            assert!(self.asked.contains(&reply.to), "a reply to no request: {reply:?}");
            assert!(!self.done.contains(&reply.to), "a reply after the Done: {reply:?}");
            if let Response::Done { .. } = reply.response {
                self.done.insert(reply.to);
            }
        }
    }
}

#[test]
fn random_interleavings_end_each_request_once_and_never_answer_a_gone_connection() {
    for seed in 1..=200u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let mut world = World::new();
        let mut seen = Seen::default();
        let mut conns: Vec<u64> = vec![0, 1, 2];
        let mut next_conn = 3;
        let mut ids: BTreeMap<u64, u64> = BTreeMap::new();
        let mut finished: BTreeSet<JobId> = BTreeSet::new();
        let mut written = 0;

        for _ in 0..200 {
            match rng.below(8) {
                0..=2 if !conns.is_empty() => {
                    let conn = conns[rng.below(conns.len())];
                    let next = ids.entry(conn).or_insert(0);
                    let id = *next;
                    *next += 1;
                    let request = match rng.below(7) {
                        0 => Request::Check { view: head("main"), targets: targets() },
                        1 => Request::Snapshot,
                        2 => Request::BestOf {
                            base: head("main"),
                            candidates: vec![
                                vec![write("x.rs", "x")],
                                vec![Op::Delete { path: "never.rs".to_owned() }],
                                vec![write("y.rs", "y")],
                            ],
                            targets: targets(),
                            test: rng.below(2) == 0,
                            filter: Vec::new(),
                        },
                        3 => {
                            written += 1;
                            let base = world.vfs.head("main").unwrap_or_else(|| world.vfs.empty());
                            let ops = vec![write(&format!("f{written}.rs"), "")];
                            Request::Commit { head: "main".to_owned(), base, ops }
                        }
                        4 if id > 0 => Request::Cancel { id: rng.below(id as usize) as u64 },
                        5 => Request::Open { view: head("main") },
                        _ => Request::Status,
                    };
                    seen.asked.insert(to(conn, id));
                    let replies = world.request(conn, id, request);
                    seen.take(replies);
                }
                3 | 4 if !world.builder.started.is_empty() => {
                    let job = world.builder.started[rng.below(world.builder.started.len())].0;
                    let response = if rng.below(2) == 0 {
                        diagnostic(Level::Error)
                    } else {
                        test_result(TestOutcome::Passed)
                    };
                    let replies = world.report(job, response);
                    seen.take(replies);
                }
                5 if !world.builder.started.is_empty() => {
                    let job = world.builder.started[rng.below(world.builder.started.len())].0;
                    finished.insert(job);
                    let replies = world.finish(job, rng.below(2) == 0, None);
                    seen.take(replies);
                }
                6 => {
                    let replies = world.durable();
                    seen.take(replies);
                }
                7 if !conns.is_empty() && rng.below(4) == 0 => {
                    let conn = conns.swap_remove(rng.below(conns.len()));
                    seen.gone.insert(Conn(conn));
                    let replies = world.gone(conn);
                    seen.take(replies);
                    conns.push(next_conn);
                    next_conn += 1;
                }
                _ => {}
            }
        }

        let jobs: Vec<JobId> = world.builder.started.iter().map(|it| it.0).collect();
        for job in jobs.into_iter().filter(|it| !finished.contains(it)) {
            let replies = world.finish(job, true, None);
            seen.take(replies);
        }
        let replies = world.durable();
        seen.take(replies);
        for asked in &seen.asked {
            if !seen.gone.contains(&asked.conn) {
                assert!(seen.done.contains(asked), "seed {seed}: {asked:?} never ended");
            }
        }
        for conn in conns {
            let replies = world.gone(conn);
            seen.take(replies);
        }
        assert!(world.session.pending.is_empty(), "seed {seed}");
        assert!(world.session.live.is_empty(), "seed {seed}");
        assert_eq!(world.session.views(), 0, "seed {seed}");
    }
}

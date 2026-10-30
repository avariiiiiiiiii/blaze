//! Declared intent ([`crate::Request::WillTest`], [`crate::Request::WillSave`]): the jobs the
//! session started through [`Builder::speculate`], each on a view it holds until the job
//! finishes.
//!
//! An intent is taken over (promoted, once) by a request that builds its root or a commit to
//! its head that makes its root. One not taken over is cancelled by a newer intent of its
//! connection, by a commit to its head that makes another root, by its connection's end, or
//! when its deadline passes. A taken-over intent runs to its end.

use blaze_core::Conn;
use blaze_ids::{ContentId, radix};
use rustc_hash::FxHashMap;

use crate::build::{Builder, JobId, Opened};

struct Intent {
    conn: Conn,
    opened: Opened,
    /// The head a [`crate::Request::WillSave`] will move.
    head: Option<String>,
    /// Nanoseconds on the daemon's clock; `None` once taken over.
    deadline_ns: Option<u64>,
    cancelled: bool,
}

impl Intent {
    /// Neither taken over nor cancelled.
    fn open(&self) -> bool {
        self.deadline_ns.is_some() && !self.cancelled
    }
}

#[derive(Default)]
pub(super) struct Intents {
    by_job: FxHashMap<JobId, Intent>,
    /// The daemon's clock as [`super::Session::tick`] last gave it.
    now_ns: u64,
}

impl Intents {
    /// `job`, started on `opened`, after the connection's intents not taken over are cancelled.
    pub(super) fn add(
        &mut self,
        builder: &mut dyn Builder,
        conn: Conn,
        job: JobId,
        opened: Opened,
        head: Option<String>,
        wait_ms: u64,
    ) {
        self.cancel_where(builder, |intent| intent.conn == conn);
        let deadline_ns = self.now_ns.saturating_add(wait_ms.saturating_mul(1_000_000));
        let intent = Intent { conn, opened, head, deadline_ns: Some(deadline_ns), cancelled: false };
        self.by_job.insert(job, intent);
    }

    /// A request builds `root`: every open intent on it is taken over.
    pub(super) fn take_over(&mut self, builder: &mut dyn Builder, root: ContentId) {
        let mut jobs = self.jobs_where(|intent| intent.opened.view.root == root);
        radix::sort_copy_by_key(&mut jobs, |it| it.0);
        for job in jobs {
            self.by_job.get_mut(&job).expect("a job just found").deadline_ns = None;
            builder.promote(job);
        }
    }

    /// `head` moved to `root`: its open intents on `root` are taken over, the rest cancelled.
    pub(super) fn moved(&mut self, builder: &mut dyn Builder, head: &str, root: ContentId) {
        self.cancel_where(builder, |intent| {
            intent.head.as_deref() == Some(head) && intent.opened.view.root != root
        });
        let mut jobs = self.jobs_where(|intent| {
            intent.head.as_deref() == Some(head) && intent.opened.view.root == root
        });
        radix::sort_copy_by_key(&mut jobs, |it| it.0);
        for job in jobs {
            self.by_job.get_mut(&job).expect("a job just found").deadline_ns = None;
            builder.promote(job);
        }
    }

    pub(super) fn gone(&mut self, builder: &mut dyn Builder, conn: Conn) {
        self.cancel_where(builder, |intent| intent.conn == conn);
    }

    /// The clock is `now_ns`: every open intent whose deadline it reached is cancelled.
    pub(super) fn tick(&mut self, builder: &mut dyn Builder, now_ns: u64) {
        self.now_ns = self.now_ns.max(now_ns);
        let now_ns = self.now_ns;
        self.cancel_where(builder, |intent| intent.deadline_ns.is_some_and(|it| it <= now_ns));
    }

    /// The earliest deadline of an open intent.
    /// A scan, not a timing wheel: one intent per waiting client, a few at once.
    pub(super) fn deadline(&self) -> Option<u64> {
        self.by_job.values().filter(|it| it.open()).filter_map(|it| it.deadline_ns).min()
    }

    /// The view of `job` when it is an intent's, the intent removed.
    pub(super) fn finish(&mut self, job: JobId) -> Option<Opened> {
        self.by_job.remove(&job).map(|intent| intent.opened)
    }

    pub(super) fn views(&self) -> usize {
        self.by_job.len()
    }

    fn jobs_where(&self, mut keep: impl FnMut(&Intent) -> bool) -> Vec<JobId> {
        self.by_job.iter().filter(|(_, it)| it.open() && keep(it)).map(|(job, _)| *job).collect()
    }

    fn cancel_where(&mut self, builder: &mut dyn Builder, keep: impl FnMut(&Intent) -> bool) {
        let mut jobs = self.jobs_where(keep);
        radix::sort_copy_by_key(&mut jobs, |it| it.0);
        for job in jobs {
            self.by_job.get_mut(&job).expect("a job just found").cancelled = true;
            builder.cancel(job);
        }
    }
}

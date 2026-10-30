//! A best-of-N: candidates checked (or tested) at once, each counted by what it reported, and
//! ranked when the last one ends.

use std::cmp::Reverse;

use blaze_ids::ContentId;

use crate::{
    Level, Ranked, Response, TestOutcome,
    build::{Builder, JobId, Opened},
};

pub(super) struct BestOf {
    base: ContentId,
    candidates: Vec<Candidate>,
    running: usize,
}

struct Candidate {
    /// `None` for one whose ops did not apply.
    root: Option<ContentId>,
    job: Option<JobId>,
    /// Its view, until its job finishes.
    opened: Option<Opened>,
    cancelled: bool,
    /// Its ops applied and its job finished without an error: its counts are of a build.
    ran: bool,
    ok: bool,
    errors: u32,
    warnings: u32,
    passed: u32,
    failed: u32,
}

impl Candidate {
    fn new(root: Option<ContentId>, job: Option<JobId>, opened: Option<Opened>) -> Candidate {
        Candidate {
            root,
            job,
            opened,
            cancelled: false,
            ran: false,
            ok: false,
            errors: 0,
            warnings: 0,
            passed: 0,
            failed: 0,
        }
    }
}

impl BestOf {
    pub(super) fn new(base: ContentId) -> BestOf {
        BestOf { base, candidates: Vec::new(), running: 0 }
    }

    /// The next candidate, whose job runs on `opened`.
    pub(super) fn started(&mut self, job: JobId, opened: Opened) -> u32 {
        self.running += 1;
        self.push(Candidate::new(Some(opened.view.root), Some(job), Some(opened)))
    }

    /// The next candidate, whose ops did not apply: it ends at once, ranked after every one that
    /// ran.
    pub(super) fn refused(&mut self) -> u32 {
        self.push(Candidate::new(None, None, None))
    }

    fn push(&mut self, candidate: Candidate) -> u32 {
        let at = u32::try_from(self.candidates.len()).expect("fewer than 2^32 candidates");
        self.candidates.push(candidate);
        at
    }

    pub(super) fn count(&mut self, candidate: u32, response: &Response) {
        let candidate = &mut self.candidates[candidate as usize];
        match response {
            Response::Diagnostic(diagnostic) => match diagnostic.level {
                Level::Error => candidate.errors += 1,
                Level::Warning => candidate.warnings += 1,
                Level::Note | Level::Help => {}
            },
            Response::TestResult(result) => match result.outcome {
                TestOutcome::Passed => candidate.passed += 1,
                TestOutcome::Failed => candidate.failed += 1,
                TestOutcome::Ignored | TestOutcome::Unreached => {}
            },
            _ => {}
        }
    }

    /// The candidate's job ended; its view, to close.
    pub(super) fn finish(&mut self, candidate: u32, ok: bool, ran: bool) -> Option<Opened> {
        let candidate = &mut self.candidates[candidate as usize];
        candidate.job = None;
        candidate.ok = ok;
        candidate.ran = ran;
        self.running -= 1;
        candidate.opened.take()
    }

    pub(super) fn running(&self) -> usize {
        self.running
    }

    pub(super) fn cancel(&mut self, builder: &mut dyn Builder) {
        for candidate in &mut self.candidates {
            if let (Some(job), false) = (candidate.job, candidate.cancelled) {
                candidate.cancelled = true;
                builder.cancel(job);
            }
        }
    }

    pub(super) fn views(&self) -> usize {
        self.candidates.iter().filter(|it| it.opened.is_some()).count()
    }

    /// Whether any candidate's request ended with `ok` true.
    pub(super) fn ok(&self) -> bool {
        self.candidates.iter().any(|it| it.ok)
    }

    /// Best first, as [`Ranked`] says, after every candidate that ran: one whose ops did not
    /// apply or whose job could not run has no counts to rank by, so it goes last, in its place
    /// in the request. Such a candidate's root is the base's.
    pub(super) fn ranked(&self) -> Vec<Ranked> {
        let mut ranked: Vec<(bool, Ranked)> = self
            .candidates
            .iter()
            .zip(0..)
            .map(|(it, candidate)| {
                let ranked = Ranked {
                    candidate,
                    root: it.root.unwrap_or(self.base),
                    errors: it.errors,
                    warnings: it.warnings,
                    passed: it.passed,
                    failed: it.failed,
                    ok: it.ok,
                };
                (it.ran, ranked)
            })
            .collect();
        ranked.sort_by_key(|(ran, it)| {
            (!ran, it.errors, it.failed, Reverse(it.passed), it.warnings, it.candidate)
        });
        ranked.into_iter().map(|(_, it)| it).collect()
    }
}

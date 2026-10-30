//! A unit's check beside its codegen (docs/v2/plan/lanes/frontend/fast-bodies-tasks.md).
//!
//! The check's bodies are a [`Plan`] that codegen's walk tells what it reaches
//! ([`BodyChecks::reached`]), so the bodies the build compiles are checked before the others.
//! Which bodies are checked does not change: the check is of every body of the crate, and the
//! unit's outcome waits for all of them, as rustc's does. Once the check fails, codegen stops at
//! its next step ([`BodyChecks::stopped`]) and what it made is dropped: the unit's outcome is
//! the check's errors.

use std::{
    panic::resume_unwind,
    sync::atomic::{AtomicBool, Ordering},
};

use base_db::Db;
use blaze_diagnostics::{Diag, Files, Options};
use hir::{
    DefWithBody,
    check::{bodies::Plan, every_body},
};
use hir_def::DefWithBodyId;
use hir_ty_codegen::v2::BodyChecks;

use super::compile::cargo_hides;

#[cfg(test)]
mod tests;

/// The check of a unit's crate, and whether it failed. See the module.
pub(crate) struct Checking {
    krate: hir::Crate,
    plan: Plan<'static, fn(DefWithBody) -> bool>,
    failed: AtomicBool,
}

impl Checking {
    /// The check of every body of `krate`, listed.
    pub(crate) fn new(db: &Db, krate: hir::Crate) -> Checking {
        static EVERY: fn(DefWithBody) -> bool = every_body;
        Checking { krate, plan: Plan::new(db, &[krate], &EVERY), failed: AtomicBool::new(false) }
    }

    /// The check with nothing beside it: of a unit that compiles no code.
    pub(crate) fn alone(&self, db: &Db, files: Files<'_>, options: &Options) -> Vec<Diag> {
        self.check(db, files, options)
    }

    /// The check, and `codegen` beside it on this thread: the check's diagnostics and what
    /// codegen gave, once both are done. What codegen gave is the caller's to drop when the
    /// check failed.
    pub(crate) fn alongside<T>(
        &self,
        db: &Db,
        files: Files<'_>,
        options: &Options,
        codegen: impl FnOnce() -> T,
    ) -> (Vec<Diag>, T) {
        beside(|| self.check(db, files, options), codegen)
    }

    fn check(&self, db: &Db, files: Files<'_>, options: &Options) -> Vec<Diag> {
        let diags = blaze_diagnostics::check_planned(db, self.krate, &self.plan, files, options);
        if failed(&diags) {
            self.failed.store(true, Ordering::Release);
        }
        diags
    }
}

impl BodyChecks for Checking {
    fn reached(&self, bodies: &[DefWithBodyId]) {
        self.plan.reached(bodies);
    }

    fn stopped(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }
}

/// `check` as a job of the process's pool (its bodies its tasks), and `codegen` on this thread
/// meanwhile; both results once both are done. A panic of either is raised here, after both.
fn beside<T>(check: impl FnOnce() -> Vec<Diag> + Send, codegen: impl FnOnce() -> T) -> (Vec<Diag>, T) {
    let mut checked = None;
    let made = blaze_graph::exec::shared().scope(|scope| {
        scope.submit(None, check, |ran| checked = Some(ran));
        codegen()
    });
    match checked.expect("a scope returns once each of its jobs is done") {
        Ok(diags) => (diags, made),
        Err(panic) => resume_unwind(panic),
    }
}

/// Whether a check's diagnostics fail its unit: an error cargo shows, as the unit's outcome
/// counts them.
fn failed(diags: &[Diag]) -> bool {
    diags.iter().any(|it| it.is_error() && !cargo_hides(&it.message))
}

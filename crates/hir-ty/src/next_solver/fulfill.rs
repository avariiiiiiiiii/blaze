//! Fulfill loop for next-solver.

use rustc_next_trait_solver::solve::{
    GoalEvaluation, GoalStalledOn, HasChanged, SolverDelegateEvalExt, StalledOnCoroutines,
    fast_path::compute_goal_fast_path,
};
use rustc_type_ir::{
    InferCtxtLike, Interner,
    solve::{Certainty, NoSolution},
};

use crate::next_solver::{
    DbInterner, ParamEnv, Predicate, SolverContext, TypingMode,
    infer::{
        InferCtxt,
        traits::{Obligation, ObligationCause, PredicateObligation, PredicateObligations},
    },
};

type StalledOn<'db> = Option<GoalStalledOn<DbInterner<'db>>>;

/// Obligations in the order they are evaluated in, a column for each part of one and one for
/// what it is stalled on. The order is rustc's: it decides which errors are reported and which
/// inference variables are constrained first.
#[derive(Debug, Default)]
struct Pending<'db> {
    cause: Vec<ObligationCause>,
    param_env: Vec<ParamEnv<'db>>,
    predicate: Vec<Predicate<'db>>,
    recursion_depth: Vec<usize>,
    stalled_on: Vec<StalledOn<'db>>,
}

const _: () = assert!(size_of::<Pending<'static>>() == 5 * size_of::<Vec<u8>>());

impl<'db> Pending<'db> {
    fn len(&self) -> usize {
        self.predicate.len()
    }

    fn is_empty(&self) -> bool {
        self.predicate.is_empty()
    }

    fn push(&mut self, obligation: PredicateObligation<'db>, stalled_on: StalledOn<'db>) {
        let Obligation { cause, param_env, predicate, recursion_depth } = obligation;
        self.cause.push(cause);
        self.param_env.push(param_env);
        self.predicate.push(predicate);
        self.recursion_depth.push(recursion_depth);
        self.stalled_on.push(stalled_on);
    }

    fn obligation(&self, at: usize) -> PredicateObligation<'db> {
        Obligation {
            cause: self.cause[at],
            param_env: self.param_env[at],
            predicate: self.predicate[at],
            recursion_depth: self.recursion_depth[at],
        }
    }

    fn obligations(&self) -> impl Iterator<Item = PredicateObligation<'db>> + '_ {
        (0..self.len()).map(|at| self.obligation(at))
    }

    fn clear(&mut self) {
        self.truncate(0);
    }

    fn truncate(&mut self, len: usize) {
        self.cause.truncate(len);
        self.param_env.truncate(len);
        self.predicate.truncate(len);
        self.recursion_depth.truncate(len);
        self.stalled_on.truncate(len);
    }

    /// rustc's `retain_mut`: keeps, in order, the rows `keep` is true of, compacting every
    /// column in place. `keep` may change a row's obligation and what it is stalled on, and
    /// what it leaves is kept.
    fn retain(
        &mut self,
        mut keep: impl FnMut(&mut PredicateObligation<'db>, &mut StalledOn<'db>) -> bool,
    ) {
        let mut kept = 0;
        for at in 0..self.len() {
            let mut obligation = self.obligation(at);
            if !keep(&mut obligation, &mut self.stalled_on[at]) {
                continue;
            }
            let Obligation { cause, param_env, predicate, recursion_depth } = obligation;
            self.cause[kept] = cause;
            self.param_env[kept] = param_env;
            self.predicate[kept] = predicate;
            self.recursion_depth[kept] = recursion_depth;
            // `kept <= at`, and the rows before `at` are all decided.
            self.stalled_on.swap(kept, at);
            kept += 1;
        }
        self.truncate(kept);
    }
}

/// A trait engine using the new trait solver.
///
/// This is mostly identical to how `evaluate_all` works inside of the
/// solver, except that the requirements are slightly different.
///
/// Unlike `evaluate_all` it is possible to add new obligations later on
/// and we also have to track diagnostics information by using `Obligation`
/// instead of `Goal`.
///
/// It is also likely that we want to use slightly different datastructures
/// here as this will have to deal with far more root goals than `evaluate_all`.
#[derive(Debug)]
pub struct FulfillmentCtxt<'db> {
    pending: Pending<'db>,

    /// The snapshot in which this context was created. Using the context
    /// outside of this snapshot leads to subtle bugs if the snapshot
    /// gets rolled back. Because of this we explicitly check that we only
    /// use the context in exactly this snapshot.
    usable_in_snapshot: usize,
}

impl<'db> FulfillmentCtxt<'db> {
    pub fn new(infcx: &InferCtxt<'db>) -> FulfillmentCtxt<'db> {
        FulfillmentCtxt { pending: Default::default(), usable_in_snapshot: infcx.num_open_snapshots() }
    }
}

impl<'db> FulfillmentCtxt<'db> {
    /// A goal the fast path proves is not kept; one it finds stalled is kept with exactly
    /// what it is stalled on, as rustc does.
    #[tracing::instrument(level = "trace", skip(self, infcx))]
    pub(crate) fn register_predicate_obligation(
        &mut self,
        infcx: &InferCtxt<'db>,
        obligation: PredicateObligation<'db>,
    ) {
        assert_eq!(self.usable_in_snapshot, infcx.num_open_snapshots());
        let delegate = <&SolverContext<'db>>::from(infcx);
        match compute_goal_fast_path(delegate, obligation.as_goal(), obligation.cause.span()) {
            Some(GoalEvaluation { certainty: Certainty::Yes, .. }) => {}
            Some(GoalEvaluation { certainty: Certainty::Maybe(_), stalled_on, .. }) => {
                self.pending.push(obligation, stalled_on)
            }
            None => self.pending.push(obligation, None),
        }
    }

    pub(crate) fn register_predicate_obligations(
        &mut self,
        infcx: &InferCtxt<'db>,
        obligations: impl IntoIterator<Item = PredicateObligation<'db>>,
    ) {
        for obligation in obligations {
            self.register_predicate_obligation(infcx, obligation);
        }
    }

    pub(crate) fn collect_remaining_errors(
        &mut self,
        _infcx: &InferCtxt<'db>,
    ) -> Vec<NextSolverError<'db>> {
        if self.pending.is_empty() {
            return Vec::new();
        }
        let errors = self.pending.obligations().map(NextSolverError::Ambiguity).collect();
        self.pending.clear();
        errors
    }

    /// rustc's loop: every pending obligation, until a round changes nothing. An obligation
    /// that made inference progress more often than the recursion limit is rustc's fatal
    /// fulfillment overflow; here it ends the loop with an [`NextSolverError::Overflow`].
    pub(crate) fn try_evaluate_obligations(
        &mut self,
        infcx: &InferCtxt<'db>,
    ) -> Vec<NextSolverError<'db>> {
        assert_eq!(self.usable_in_snapshot, infcx.num_open_snapshots());
        let delegate = <&SolverContext<'db>>::from(infcx);
        let recursion_limit = infcx.interner.recursion_limit();
        let mut errors = Vec::new();
        let mut overflowed = false;
        loop {
            let mut any_changed = false;

            self.pending.retain(|obligation, opt_stalled_on| {
                if overflowed {
                    return true;
                }
                // Common case: still stalled; keep the obligation. This path is extremely hot
                // in some cases; there can be thousands of pending obligations.
                if let Some(stalled_on) = opt_stalled_on
                    && delegate.goal_remains_stalled(stalled_on)
                {
                    return true;
                }

                let goal = obligation.as_goal();
                let span = obligation.cause.span();
                let result = delegate.evaluate_root_goal(goal, span, opt_stalled_on.take());
                infcx.inspect_evaluated_obligation(obligation, &result, || {
                    Some(delegate.evaluate_root_goal_for_proof_tree(goal, span).1)
                });
                let GoalEvaluation { goal, certainty, has_changed, stalled_on } = match result {
                    Ok(result) => result,
                    Err(NoSolution) => {
                        errors.push(NextSolverError::TrueError(obligation.clone()));
                        return false;
                    }
                };

                // We've resolved the goal in `evaluate_root_goal`, avoid redoing this work
                // in the next iteration. This does not resolve the inference variables
                // constrained by evaluating the goal.
                obligation.predicate = goal.predicate;
                if has_changed == HasChanged::Yes {
                    if obligation.recursion_depth > recursion_limit {
                        // We limit the total count of inference progress to avoid hangs, and
                        // don't try to recover from this, as rustc doesn't.
                        errors.push(NextSolverError::Overflow(obligation.clone()));
                        overflowed = true;
                        return false;
                    }
                    // We increment the recursion depth here to track the number of times
                    // this goal has resulted in inference progress. This doesn't precisely
                    // model the way that we track recursion depth in the old solver due
                    // to the fact that we only process root obligations, but it is a good
                    // approximation and should only result in fulfillment overflow in
                    // pathological cases.
                    obligation.recursion_depth += 1;
                    any_changed = true;
                }

                match certainty {
                    Certainty::Yes => false,
                    Certainty::Maybe(_) => {
                        // What it is stalled on for the next round, as we run to a fixpoint.
                        *opt_stalled_on = stalled_on;
                        true
                    }
                }
            });

            if overflowed || !any_changed {
                break;
            }
        }

        errors
    }

    pub(crate) fn evaluate_obligations_error_on_ambiguity(
        &mut self,
        infcx: &InferCtxt<'db>,
    ) -> Vec<NextSolverError<'db>> {
        let errors = self.try_evaluate_obligations(infcx);
        if !errors.is_empty() {
            return errors;
        }

        self.collect_remaining_errors(infcx)
    }

    pub(crate) fn pending_obligations(&self) -> PredicateObligations<'db> {
        self.pending.obligations().collect()
    }

    /// The pending obligations the solver found stalled on a coroutine the typeck root
    /// defines, taken out.
    pub(crate) fn drain_stalled_obligations_for_coroutines(
        &mut self,
        infcx: &InferCtxt<'db>,
    ) -> PredicateObligations<'db> {
        let stalled_coroutines = match infcx.typing_mode_raw().assert_not_erased() {
            TypingMode::Typeck { defining_opaque_types_and_generators } => {
                defining_opaque_types_and_generators
            }
            TypingMode::Coherence
            | TypingMode::PostTypeckThroughBorrowck { defining_opaque_types: _ }
            | TypingMode::PostBorrowck { defined_opaque_types: _ }
            | TypingMode::Reflection
            | TypingMode::PostAnalysis
            | TypingMode::Codegen => return Default::default(),
        };

        if stalled_coroutines.as_slice().is_empty() {
            return Default::default();
        }

        let mut stalled = PredicateObligations::new();
        self.pending.retain(|obligation, stalled_on| {
            let on_coroutines = stalled_on.as_ref().is_some_and(|it| {
                matches!(it.stalled_maybe_info.stalled_on_coroutines, StalledOnCoroutines::Yes)
            });
            if on_coroutines {
                stalled.push(obligation.clone());
            }
            !on_coroutines
        });
        stalled
    }
}

#[derive(Debug, Clone)]
pub enum NextSolverError<'db> {
    TrueError(PredicateObligation<'db>),
    Ambiguity(PredicateObligation<'db>),
    /// An obligation that made inference progress more often than the recursion limit:
    /// rustc's fatal fulfillment overflow.
    Overflow(PredicateObligation<'db>),
}

impl NextSolverError<'_> {
    #[inline]
    pub fn is_true_error(&self) -> bool {
        matches!(self, NextSolverError::TrueError(_))
    }
}

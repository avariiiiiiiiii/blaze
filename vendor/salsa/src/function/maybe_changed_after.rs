#[cfg(feature = "accumulator")]
use crate::accumulator::accumulated_map::InputAccumulatedValues;
use crate::cycle::{CycleHeads, CycleRecoveryStrategy, ProvisionalStatus};
use crate::database::RawDatabase;
use crate::fingerprint::Fingerprint;
use crate::function::memo::{ErasedMemo, MemoHeader, TryClaimCycleHeadsIter, TryClaimHeadsResult};
use crate::function::sync::{ClaimGuard, ClaimResult};
use crate::function::{Configuration, IngredientImpl, Reentrancy, SyncTable};
use crate::table::memo::MemoSlot;
use std::sync::atomic::Ordering;

use crate::graph::{Color, EdgeWords, Graph, OUTPUT};
use crate::key::DatabaseKeyIndex;
use crate::zalsa::{MemoIngredientIndex, Zalsa, ZalsaDatabase};
use crate::zalsa_local::{QueryEdgeKind, QueryEdges, QueryOriginRef, QueryRevisions, ZalsaLocal};
use std::cell::Cell;
use std::mem::ManuallyDrop;
use crate::{Id, Revision};

/// Result of memo validation.
#[derive(Debug, Copy, Clone)]
pub enum VerifyResult {
    /// Memo has changed and needs to be recomputed.
    Changed,

    /// Memo remains valid.
    ///
    /// The inner value tracks whether the memo or any of its dependencies have an
    /// accumulated value.
    Unchanged {
        #[cfg(feature = "accumulator")]
        accumulated: InputAccumulatedValues,
    },
}

impl VerifyResult {
    pub(crate) const fn changed_if(changed: bool) -> Self {
        if changed {
            Self::changed()
        } else {
            Self::unchanged()
        }
    }

    pub(crate) const fn changed() -> Self {
        Self::Changed
    }

    pub(crate) const fn unchanged() -> Self {
        Self::Unchanged {
            #[cfg(feature = "accumulator")]
            accumulated: InputAccumulatedValues::Empty,
        }
    }

    #[inline]
    #[cfg(feature = "accumulator")]
    pub(crate) fn unchanged_with_accumulated(accumulated: InputAccumulatedValues) -> Self {
        Self::Unchanged { accumulated }
    }

    #[inline]
    #[cfg(not(feature = "accumulator"))]
    pub(crate) fn unchanged_with_accumulated() -> Self {
        Self::unchanged()
    }

    /// Returns an unchanged result that propagates accumulated values from both
    /// the memo itself and its inputs.
    #[inline]
    fn unchanged_for_memo(revisions: &QueryRevisions) -> Self {
        #[cfg(not(feature = "accumulator"))]
        let _ = revisions;

        Self::unchanged_with_accumulated(
            #[cfg(feature = "accumulator")]
            match revisions.accumulated() {
                Some(_) => InputAccumulatedValues::Any,
                None => revisions.accumulated_inputs(),
            },
        )
    }

    pub(crate) const fn is_unchanged(&self) -> bool {
        matches!(self, Self::Unchanged { .. })
    }
}

impl<C> IngredientImpl<C>
where
    C: Configuration,
{
    pub(super) fn maybe_changed_after<'db>(
        &'db self,
        db: &'db C::DbView,
        id: Id,
        revision: Revision,
    ) -> VerifyResult {
        let (zalsa, zalsa_local) = db.zalsas();
        let memo_ingredient_index = self.memo_ingredient_index(zalsa, id);
        zalsa.unwind_if_revision_cancelled(zalsa_local);

        loop {
            let database_key_index = self.database_key_index(id);

            crate::tracing::debug!(
                "{database_key_index:?}: maybe_changed_after(revision = {revision:?})"
            );

            // Check if we have a verified version: this is the hot path.
            let memo_guard = self.get_memo_cached(zalsa, zalsa_local, id, memo_ingredient_index);
            let Some(memo) = memo_guard else {
                // No memo? Assume has changed.
                return VerifyResult::changed();
            };

            if let Some(result) = memo.header.maybe_changed_after_hot(
                zalsa,
                database_key_index,
                revision,
                #[cfg(feature = "detailed-trace")]
                memo.value.is_some(),
            ) {
                return result;
            }

            if memo
                .header
                .deep_verify_unclaimed(zalsa, db.into(), database_key_index)
            {
                return memo.header.verify_result_after(revision);
            }

            if let Some(result) = self.maybe_changed_after_cold(
                zalsa,
                zalsa_local,
                db,
                database_key_index,
                revision,
                memo_ingredient_index,
            ) {
                return result;
            }
        }
    }

    fn maybe_changed_after_cold(
        &self,
        zalsa: &Zalsa,
        zalsa_local: &ZalsaLocal,
        db: &C::DbView,
        database_key_index: DatabaseKeyIndex,
        revision: Revision,
        memo_ingredient_index: MemoIngredientIndex,
    ) -> Option<VerifyResult> {
        enum ColdResult<'db> {
            Retry,
            Verified(VerifyResult),
            Reexecute {
                claim_guard: ClaimGuard<'db>,
                old_memo: ErasedMemo<'db>,
            },
        }

        #[allow(clippy::too_many_arguments)]
        fn inner<'db>(
            sync_table: &'db SyncTable,
            zalsa: &'db Zalsa,
            zalsa_local: &'db ZalsaLocal,
            db: RawDatabase<'db>,
            memo_slot: MemoSlot<'db>,
            database_key_index: DatabaseKeyIndex,
            revision: Revision,
            cycle_recovery_strategy: CycleRecoveryStrategy,
        ) -> ColdResult<'db> {
            let claim_guard = match sync_table.try_claim(
                zalsa,
                zalsa_local,
                database_key_index.key_index(),
                Reentrancy::Deny,
            ) {
                ClaimResult::Claimed(guard) => guard,
                ClaimResult::Running(blocked_on) => {
                    let _ = blocked_on.block_on(zalsa);
                    return ColdResult::Retry;
                }
                ClaimResult::Cycle { .. } => {
                    return ColdResult::Verified(maybe_changed_after_cold_cycle(
                        zalsa_local,
                        database_key_index,
                        cycle_recovery_strategy,
                    ));
                }
            };

            // Load the current memo after claiming the query because it may have changed while
            // this query was blocked on another thread.
            let Some(old_memo) = memo_slot.get_erased() else {
                return ColdResult::Verified(VerifyResult::changed());
            };

            let old_header = old_memo.header();

            crate::tracing::debug!(
                "{database_key_index:?}: maybe_changed_after_cold, successful claim, \
                    revision = {revision:?}, old_memo = {old_memo:#?}",
                old_memo = old_header.tracing_debug(old_memo.has_value()),
            );

            if old_header.verify_memo(
                db,
                &claim_guard,
                cycle_recovery_strategy,
                #[cfg(feature = "detailed-trace")]
                old_memo.has_value(),
            ) {
                return ColdResult::Verified(if old_header.revisions.changed_at() > revision {
                    VerifyResult::changed()
                } else {
                    VerifyResult::unchanged_for_memo(&old_header.revisions)
                });
            }

            // If the memo is not provisional, the generic continuation can check whether it has
            // an old value and re-execute. The result may equal the old value and be backdated, in
            // which case the new memo has not logically changed.
            if !old_header.may_be_provisional() {
                ColdResult::Reexecute {
                    claim_guard,
                    old_memo,
                }
            } else {
                ColdResult::Verified(VerifyResult::changed())
            }
        }

        let memo_slot =
            self.memo_slot(zalsa, database_key_index.key_index(), memo_ingredient_index);

        match inner(
            &self.sync_table,
            zalsa,
            zalsa_local,
            db.into(),
            memo_slot,
            database_key_index,
            revision,
            C::CYCLE_STRATEGY,
        ) {
            ColdResult::Retry => None,
            ColdResult::Verified(result) => Some(result),
            ColdResult::Reexecute {
                claim_guard,
                old_memo,
            } => {
                let old_memo = old_memo.downcast::<C>();

                // An evicted value with a value fingerprint is computed again and backdated
                // when its fingerprint is the kept one. Without one it can't be compared.
                if old_memo.value.is_none()
                    && !old_memo.header.revisions.value_fingerprint().is_value()
                {
                    return Some(VerifyResult::changed());
                }

                let memo = self.execute(db, claim_guard, Some(old_memo))?;
                let changed_at = memo.header.revisions.changed_at();

                // Always assume that a provisional value has changed.
                //
                // We don't know if a provisional value has actually changed. To determine whether
                // a provisional value has changed, we need to iterate the outer cycle, which cannot
                // be done here.
                Some(
                    if changed_at > revision || memo.header.may_be_provisional() {
                        VerifyResult::changed()
                    } else {
                        VerifyResult::unchanged_for_memo(&memo.header.revisions)
                    },
                )
            }
        }
    }
}

/// How many edges ahead the claim-free walk prefetches its dependencies' colors: enough to
/// cover a miss to memory at a few nanoseconds an edge.
const COLOR_LOOKAHEAD: usize = 8;

/// A memo on the claim-free walk, and the part of its edges still to check.
struct UnclaimedFrame<'db> {
    header: &'db MemoHeader,
    key: DatabaseKeyIndex,
    edges: EdgeWords<'db>,
    /// The position in `edges` of the next edge.
    position: usize,
    verified_at: Revision,
    #[cfg(feature = "accumulator")]
    inputs: InputAccumulatedValues,
}

impl<'db> UnclaimedFrame<'db> {
    fn new(
        graph: &Graph,
        header: &'db MemoHeader,
        key: DatabaseKeyIndex,
        edges: EdgeWords<'db>,
    ) -> Self {
        for word in edges.iter().take(COLOR_LOOKAHEAD) {
            graph.prefetch_color(word & !OUTPUT);
        }
        UnclaimedFrame {
            header,
            key,
            edges,
            position: 0,
            verified_at: header.verified_at.load(),
            #[cfg(feature = "accumulator")]
            inputs: InputAccumulatedValues::Empty,
        }
    }

    /// Folds in what the input at `position` gave; false if it changed for this memo.
    fn take(
        &mut self,
        zalsa: &Zalsa,
        position: usize,
        dependency: DatabaseKeyIndex,
        mut result: VerifyResult,
    ) -> bool {
        if let VerifyResult::Changed = result
            && let Some(saw) = self.header.revisions.edge_fingerprint(position)
            && let Some(unchanged) =
                unchanged_by_fingerprint(zalsa, dependency, saw, &self.header.revisions)
        {
            result = unchanged;
        }
        match result {
            VerifyResult::Changed => false,
            #[cfg(feature = "accumulator")]
            VerifyResult::Unchanged { accumulated } => {
                self.inputs |= accumulated;
                true
            }
            #[cfg(not(feature = "accumulator"))]
            VerifyResult::Unchanged { .. } => true,
        }
    }

    /// Every input was unchanged: the memo is verified.
    fn finish(&self, zalsa: &Zalsa) {
        #[cfg(feature = "accumulator")]
        self.header.revisions.store_accumulated_inputs(self.inputs);
        self.header.publish_closure(zalsa);
        self.header.mark_as_verified(zalsa, self.key);
    }
}

thread_local! {
    /// The claim-free walk's stack, empty between walks, so a deep chain grows it once per
    /// thread.
    // alloc: `Vec::new` allocates nothing; the thread's walks reuse the one buffer
    static UNCLAIMED_STACK: Cell<Vec<UnclaimedFrame<'static>>> = const { Cell::new(Vec::new()) };
}

/// The thread's walk stack, empty, for frames that borrow for `'db`. A walk inside a walk gets
/// an empty `Vec`, which allocates only if that walk grows it.
fn take_stack<'db>() -> Vec<UnclaimedFrame<'db>> {
    let stack = UNCLAIMED_STACK.take();
    debug_assert!(stack.is_empty());
    let mut stack = ManuallyDrop::new(stack);
    // SAFETY: the vector holds no frame (`give_stack` clears it, `Vec::new` starts empty), so no
    // borrow of another lifetime is carried over; `UnclaimedFrame<'static>` and
    // `UnclaimedFrame<'db>` differ only in a lifetime, so they have one layout, and the
    // allocation is the global allocator's, as it was.
    unsafe { Vec::from_raw_parts(stack.as_mut_ptr().cast(), 0, stack.capacity()) }
}

/// Gives the stack's allocation back to the thread, emptied.
fn give_stack(mut stack: Vec<UnclaimedFrame<'_>>) {
    stack.clear();
    let mut stack = ManuallyDrop::new(stack);
    // SAFETY: as in `take_stack`: empty, the same layout, the same allocation.
    let stack = unsafe { Vec::from_raw_parts(stack.as_mut_ptr().cast(), 0, stack.capacity()) };
    UNCLAIMED_STACK.set(stack);
}

/// Walks `stack` (its bottom the memo checked) until that memo is verified (true) or only a
/// claimed check can decide (false).
///
/// An edge costs one load when its dependency's color proves it unchanged (`graph::colors`), as
/// rustc's `try_mark_green` reads `DepNodeColorMap`; the colors of the next
/// [`COLOR_LOOKAHEAD`] edges are prefetched, so the loads of a memo's edges overlap. Only an
/// edge the color can't decide finds its dependency's memo by key.
fn walk_unclaimed<'db>(
    zalsa: &'db Zalsa,
    db: RawDatabase<'_>,
    stack: &mut Vec<UnclaimedFrame<'db>>,
) -> bool {
    let graph = zalsa.graph();
    let now = zalsa.current_revision();
    while let Some(frame) = stack.last_mut() {
        let position = frame.position;
        let Some(word) = frame.edges.get(position) else {
            let done = stack.pop().expect("salsa: the walk's stack holds the memo it checks");
            done.finish(zalsa);
            let Some(parent) = stack.last_mut() else {
                return true;
            };
            let result = done.header.verify_result_after(parent.verified_at);
            if !parent.take(zalsa, parent.position - 1, done.key, result) {
                return false;
            }
            continue;
        };
        frame.position += 1;
        if let Some(ahead) = frame.edges.get(position + COLOR_LOOKAHEAD) {
            graph.prefetch_color(ahead & !OUTPUT);
        }
        if word & OUTPUT != 0 {
            return false;
        }
        let color = graph.color(word);
        if color.unchanged_since(now, frame.verified_at) {
            #[cfg(debug_assertions)]
            check_color(zalsa, db, word, color, frame.verified_at);
            // Unchanged with no accumulated values: nothing to fold into the frame.
            continue;
        }
        let dependency = graph.decode(word).key();
        let ingredient = zalsa.lookup_ingredient(dependency.ingredient_index());
        let result = match ingredient.as_function() {
            Some(function) => {
                let Some(memo) = function.memo(zalsa, dependency.key_index()) else {
                    return false;
                };
                let header = memo.header();
                match header.maybe_changed_after_hot(
                    zalsa,
                    dependency,
                    frame.verified_at,
                    #[cfg(feature = "detailed-trace")]
                    memo.has_value(),
                ) {
                    Some(result) => result,
                    None => {
                        let Some(edges) = header.unclaimed_edges(zalsa) else {
                            return false;
                        };
                        stack.push(UnclaimedFrame::new(graph, header, dependency, edges));
                        continue;
                    }
                }
            }
            None => {
                if let Some(changed_at) = ingredient.fixed_changed_at(zalsa, dependency.key_index())
                {
                    graph.paint(word, Color::verified(now, changed_at, false));
                }
                // SAFETY: `db` is the database every ingredient of `zalsa` was created with.
                unsafe {
                    ingredient.maybe_changed_after(
                        zalsa,
                        db,
                        dependency.key_index(),
                        frame.verified_at,
                    )
                }
            }
        };
        if !frame.take(zalsa, position, dependency, result) {
            return false;
        }
    }
    unreachable!("salsa: the claim-free walk returns when it pops its first memo")
}

/// Checks a color that said "unchanged" against the memo path, which decides without colors:
/// they agree, or the color changed since it was read (a memo of the key was replaced or removed
/// meanwhile, a race either path may lose).
#[cfg(debug_assertions)]
fn check_color(zalsa: &Zalsa, db: RawDatabase<'_>, word: u32, color: Color, reader: Revision) {
    let graph = zalsa.graph();
    let now = zalsa.current_revision();
    let dependency = graph.decode(word).key();
    let ingredient = zalsa.lookup_ingredient(dependency.ingredient_index());
    let agrees = match ingredient.as_function() {
        Some(function) => function.memo(zalsa, dependency.key_index()).is_some_and(|memo| {
            let header = memo.header();
            header.verified_at.load() == now
                && !header.may_be_provisional()
                && !header.has_accumulated()
                && header.revisions.changed_at() <= reader
        }),
        // SAFETY: `db` is the database every ingredient of `zalsa` was created with.
        None => unsafe {
            ingredient.maybe_changed_after(zalsa, db, dependency.key_index(), reader)
        }
        .is_unchanged(),
    };
    assert!(
        agrees || graph.color(word) != color,
        "salsa: the color of {dependency:?} ({color:?}) says it is unchanged since {reader:?} \
         in {now:?}, but its memo disagrees"
    );
}

impl MemoHeader {
    /// What a reader verified at `revision` learns from this memo, now verified.
    #[inline]
    fn verify_result_after(&self, revision: Revision) -> VerifyResult {
        if self.revisions.changed_at() > revision {
            VerifyResult::changed()
        } else {
            VerifyResult::unchanged_for_memo(&self.revisions)
        }
    }

    /// The edges of a memo the claim-free walk may verify: not (ever) part of a cycle, and
    /// derived from tracked reads only.
    fn unclaimed_edges<'db>(&'db self, zalsa: &'db Zalsa) -> Option<EdgeWords<'db>> {
        if self.may_be_provisional()
            || self.was_cycle_participant()
            || !self.cycle_heads().is_empty()
        {
            return None;
        }
        self.revisions.derived_edge_words(zalsa.graph())
    }

    /// Verifies the memo by its edges with no claim (rustc's `try_mark_green`): true when every
    /// input is unchanged, and the memo (with every function memo found unchanged on the way) is
    /// then marked verified. False when only a claimed check can decide: a memo on the way is
    /// (or was) part of a cycle, has outputs to mark, read untracked state, or has an input
    /// that changed or may have to run. Nothing runs and nothing is claimed; the only writes
    /// are the idempotent stores of a verification (`verified_at`, the accumulated-inputs
    /// flag), so tasks walking the same memos agree, and one running a memo under its claim
    /// sees the same inputs.
    ///
    /// The walk keeps an explicit stack the thread reuses, so a chain of any depth is walked
    /// once, in the order the claimed check walks it, and allocates nothing once the thread has
    /// walked one as deep.
    pub(super) fn deep_verify_unclaimed<'db>(
        &'db self,
        zalsa: &'db Zalsa,
        db: RawDatabase<'_>,
        database_key_index: DatabaseKeyIndex,
    ) -> bool {
        let Some(edges) = self.unclaimed_edges(zalsa) else {
            return false;
        };
        let mut stack = take_stack();
        stack.push(UnclaimedFrame::new(
            zalsa.graph(),
            self,
            database_key_index,
            edges,
        ));
        let verified = walk_unclaimed(zalsa, db, &mut stack);
        give_stack(stack);
        verified
    }

    fn maybe_changed_after_hot(
        &self,
        zalsa: &Zalsa,
        database_key_index: DatabaseKeyIndex,
        revision: Revision,
        #[cfg(feature = "detailed-trace")] has_value: bool,
    ) -> Option<VerifyResult> {
        let can_shallow_update = self.shallow_verify_memo(
            zalsa,
            database_key_index,
            #[cfg(feature = "detailed-trace")]
            has_value,
        );
        if can_shallow_update.yes() && !self.may_be_provisional() {
            self.update_shallow(zalsa, database_key_index, can_shallow_update);

            Some(if self.revisions.changed_at() > revision {
                VerifyResult::changed()
            } else {
                VerifyResult::unchanged_for_memo(&self.revisions)
            })
        } else {
            None
        }
    }

    /// Returns whether this memo is still valid in the current revision.
    pub(super) fn verify_memo(
        &self,
        db: crate::database::RawDatabase<'_>,
        claim_guard: &ClaimGuard<'_>,
        cycle_recovery_strategy: CycleRecoveryStrategy,
        #[cfg(feature = "detailed-trace")] has_value: bool,
    ) -> bool {
        let zalsa = claim_guard.zalsa();
        let zalsa_local = claim_guard.zalsa_local();
        let database_key_index = claim_guard.database_key_index();

        let can_shallow_update = self.shallow_verify_memo(
            zalsa,
            database_key_index,
            #[cfg(feature = "detailed-trace")]
            has_value,
        );
        if can_shallow_update.yes()
            && self.validate_may_be_provisional(
                zalsa,
                zalsa_local,
                database_key_index,
                #[cfg(feature = "detailed-trace")]
                has_value,
            )
        {
            self.update_shallow(zalsa, database_key_index, can_shallow_update);
            true
        } else {
            self.deep_verify_memo(
                db,
                claim_guard,
                cycle_recovery_strategy,
                #[cfg(feature = "detailed-trace")]
                has_value,
            )
            .is_unchanged()
        }
    }

    /// `Some` if the memo's value and `changed_at` time is still valid in this revision.
    /// Does only a shallow O(1) check, doesn't walk the dependencies.
    ///
    /// In general, a provisional memo (from cycle iteration) does not verify. Since we don't
    /// eagerly finalize all provisional memos in cycle iteration, we have to lazily check here
    /// (via `validate_provisional`) whether a may-be-provisional memo should actually be verified
    /// final, because its cycle heads are all now final.
    #[inline]
    pub(super) fn shallow_verify_memo(
        &self,
        zalsa: &Zalsa,
        database_key_index: DatabaseKeyIndex,
        #[cfg(feature = "detailed-trace")] has_value: bool,
    ) -> ShallowUpdate {
        #[cfg(feature = "detailed-trace")]
        crate::tracing::debug!(
            "{database_key_index:?}: shallow_verify_memo(memo = {memo:#?})",
            memo = self.tracing_debug(has_value)
        );
        let verified_at = self.verified_at.load();
        let revision_now = zalsa.current_revision();

        if verified_at == revision_now {
            // Already verified.
            return ShallowUpdate::Verified;
        }

        self.shallow_verify_memo_cold(zalsa, database_key_index, verified_at)
    }

    #[cold]
    #[inline(never)]
    fn shallow_verify_memo_cold(
        &self,
        zalsa: &Zalsa,
        database_key_index: DatabaseKeyIndex,
        verified_at: Revision,
    ) -> ShallowUpdate {
        let last_changed = zalsa.last_changed_revision(self.revisions.durability());
        crate::tracing::trace!(
            "{database_key_index:?}: check_durability({database_key_index:#?}, last_changed={:?} <= verified_at={:?}) = {:?}",
            last_changed,
            verified_at,
            last_changed <= verified_at,
        );
        if last_changed <= verified_at {
            // No input of the suitable durability has changed since last verified.
            ShallowUpdate::HigherDurability
        } else if self.closure_unchanged(zalsa, verified_at) {
            // No input it read, transitively, changed since (`graph::bloom`): what a deep verify
            // would find, without the walk, so it is verified as the durability check verifies.
            ShallowUpdate::HigherDurability
        } else {
            ShallowUpdate::No
        }
    }

    #[inline]
    pub(super) fn update_shallow(
        &self,
        zalsa: &Zalsa,
        database_key_index: DatabaseKeyIndex,
        update: ShallowUpdate,
    ) {
        if let ShallowUpdate::HigherDurability = update {
            self.mark_as_verified(zalsa, database_key_index);
            self.mark_outputs_as_verified(zalsa, database_key_index);
        }
    }

    /// Validates this memo if it is a provisional memo. Returns true for:
    /// * non provisional memos
    /// * provisional memos that have been successfully marked as verified final, that is, its
    ///   cycle heads have all been finalized.
    /// * provisional memos that have been created in the same revision and iteration and are part of the same cycle.
    #[inline]
    fn validate_may_be_provisional(
        &self,
        zalsa: &Zalsa,
        zalsa_local: &ZalsaLocal,
        database_key_index: DatabaseKeyIndex,
        #[cfg(feature = "detailed-trace")] has_value: bool,
    ) -> bool {
        if !self.may_be_provisional() {
            return true;
        }

        let cycle_heads = self.cycle_heads();

        if cycle_heads.is_empty() {
            return true;
        }

        // A pending write can cancel a fixpoint iteration without advancing the revision.
        // Provisional results from the abandoned execution must not be reused.
        if self.revisions.iteration().cancellation_count() != zalsa.runtime().cancellation_count() {
            return false;
        }

        #[cfg(feature = "detailed-trace")]
        crate::tracing::trace!(
            "{database_key_index:?}: validate_may_be_provisional(memo = {memo:#?})",
            memo = self.tracing_debug(has_value),
        );

        let verified_at = self.verified_at.load();
        validate_provisional(
            zalsa,
            database_key_index,
            &self.revisions,
            verified_at,
            cycle_heads,
        ) || validate_same_iteration(
            zalsa,
            zalsa_local,
            database_key_index,
            verified_at,
            cycle_heads,
        )
    }

    /// VerifyResult::Unchanged if the memo's value and `changed_at` time is up-to-date in the
    /// current revision. When this returns Unchanged with no cycle heads, it also updates the
    /// memo's `verified_at` field if needed to make future calls cheaper.
    ///
    /// Takes a [`ClaimGuard`] argument because this function recursively
    /// walks dependencies of `old_memo` and may even execute them to see if their
    /// outputs have changed.
    fn deep_verify_memo(
        &self,
        db: crate::database::RawDatabase<'_>,
        claim_guard: &ClaimGuard<'_>,
        cycle_recovery_strategy: CycleRecoveryStrategy,
        #[cfg(feature = "detailed-trace")] has_value: bool,
    ) -> VerifyResult {
        let zalsa = claim_guard.zalsa();
        let database_key_index = claim_guard.database_key_index();

        match self.origin(zalsa.graph()) {
            QueryOriginRef::Derived(edges) => {
                #[cfg(feature = "detailed-trace")]
                crate::tracing::debug!(
                    "{database_key_index:?}: deep_verify_memo(old_memo = {old_memo:#?})",
                    old_memo = self.tracing_debug(has_value)
                );

                let is_provisional = self.may_be_provisional();

                // If the value is from the same revision but is still provisional, consider it changed
                // because we're now in a new iteration.
                if is_provisional {
                    return VerifyResult::changed();
                }

                // If the old memo participate in a cycle, but the query doesn't have cycle handling,
                // always return changed. The reasoning here is:
                //
                // * cycle heads flatten their dependecies. Therefore, no query with cycle handling
                //   participating in the same cycle should ever call `maybe_changed_after` on any other query.
                //   (we don't get here).
                // * the query can't be reached from any other query without cycle handling because,
                //   executing it would immediately panic because of the cycle.
                // * The only other place where we can reach this code is from `fetch`, this is when
                //   the outer cycle is being re-executed. Given that the cycle re-executes, this
                //   query must always be considered changed.
                //
                // For queries with cycle handling, verify the flattened
                // dependencies of the cycle head instead.
                if cycle_recovery_strategy == CycleRecoveryStrategy::Panic
                    && self.was_cycle_participant()
                {
                    return VerifyResult::changed();
                }

                let verified_at = self.verified_at.load();

                let result = deep_verify_edges(
                    db,
                    zalsa,
                    &self.revisions,
                    verified_at,
                    edges,
                    database_key_index,
                );

                if result.is_unchanged() {
                    self.publish_closure(zalsa);
                    self.mark_as_verified(zalsa, database_key_index);
                }

                result
            }

            QueryOriginRef::Assigned(_) => {
                // If the value was assigned by another query,
                // and that query were up-to-date,
                // then we would have updated the `verified_at` field already.
                // So the fact that we are here means that it was not specified
                // during this revision or is otherwise stale.
                //
                // Example of how this can happen:
                //
                // Conditionally specified queries
                // where the value is specified
                // in rev 1 but not in rev 2.
                VerifyResult::changed()
            }
            QueryOriginRef::DerivedUntracked(_) => {
                // Untracked inputs? Have to assume that it changed.
                VerifyResult::changed()
            }
        }
    }
}

fn maybe_changed_after_cold_cycle(
    zalsa_local: &ZalsaLocal,
    database_key_index: DatabaseKeyIndex,
    cycle_recovery_strategy: CycleRecoveryStrategy,
) -> VerifyResult {
    match cycle_recovery_strategy {
        // SAFETY: We do not access the query stack reentrantly.
        CycleRecoveryStrategy::Panic => unsafe {
            zalsa_local.with_query_stack_unchecked(|stack| {
                panic!(
                    "dependency graph cycle when validating {database_key_index:#?}, \
                    set cycle_fn/cycle_initial to fixpoint iterate.\n\
                    Query stack:\n{stack:#?}",
                );
            })
        },
        // We flatten the dependencies of queries with cycle handling that participate in a query.
        // Verifying those queries should never result in a cycle because all function dependencies were removed.
        // That means, if we hit this path, then some query introduced a new cycle that didn't exist
        // in the previous revision. We have to consider this query changed so that we ultimately
        // insert the fixpoint initial value in `fetch_cold_cycle`.
        CycleRecoveryStrategy::FallbackImmediate | CycleRecoveryStrategy::Fixpoint => {
            crate::tracing::debug!(
                "hit cycle at {database_key_index:?} in `maybe_changed_after`,  returning changed",
            );

            VerifyResult::changed()
        }
    }
}

fn deep_verify_edges(
    db: crate::database::RawDatabase,
    zalsa: &Zalsa,
    old_revisions: &QueryRevisions,
    old_verified_at: Revision,
    edges: QueryEdges<'_>,
    database_key_index: DatabaseKeyIndex,
) -> VerifyResult {
    #[cfg(feature = "accumulator")]
    let mut inputs = InputAccumulatedValues::Empty;

    // Fully tracked inputs? Iterate over the inputs and check them, one by one.
    //
    // NB: It's important here that we are iterating the inputs in the order that
    // they executed. It's possible that if the value of some input I0 is no longer
    // valid, then some later input I1 might never have executed at all, so verifying
    // it is still up to date is meaningless.
    let graph = zalsa.graph();
    let now = zalsa.current_revision();
    for (position, edge) in edges.into_iter().enumerate() {
        match edge.kind() {
            QueryEdgeKind::Input => {
                if let Some(node) = edge.node() {
                    let color = graph.color(node);
                    if color.unchanged_since(now, old_verified_at) {
                        #[cfg(debug_assertions)]
                        check_color(zalsa, db, node, color, old_verified_at);
                        continue;
                    }
                }
                let dependency_index = edge.key();
                let mut input_result =
                    dependency_index.maybe_changed_after(db, zalsa, old_verified_at);

                if let VerifyResult::Changed = input_result
                    && let Some(saw) = old_revisions.edge_fingerprint(position)
                    && let Some(unchanged) =
                        unchanged_by_fingerprint(zalsa, dependency_index, saw, old_revisions)
                {
                    input_result = unchanged;
                }

                match input_result {
                    VerifyResult::Changed => {
                        return VerifyResult::changed();
                    }
                    #[cfg(feature = "accumulator")]
                    VerifyResult::Unchanged { accumulated } => {
                        inputs |= accumulated;
                    }
                    #[cfg(not(feature = "accumulator"))]
                    VerifyResult::Unchanged { .. } => {}
                }
            }
            QueryEdgeKind::Output => {
                let dependency_index = edge.key();
                // Subtle: Mark outputs as validated now, even though we may
                // later find an input that requires us to re-execute the function.
                // Even if it re-execute, the function will wind up writing the same value,
                // since all prior inputs were green. It's important to do this during
                // this loop, because it's possible that one of our input queries will
                // re-execute and may read one of our earlier outputs
                // (e.g., in a scenario where we do something like
                // `e = Entity::new(..); query(e);` and `query` reads a field of `e`).
                //
                // NB. Accumulators are also outputs, but the above logic doesn't
                // quite apply to them. Since multiple values are pushed, the first value
                // may be unchanged, but later values could be different.
                // In that case, however, the data accumulated
                // by this function cannot be read until this function is marked green,
                // so even if we mark them as valid here, the function will re-execute
                // and overwrite the contents.
                dependency_index.mark_validated_output(zalsa, database_key_index);
            }
        }
    }
    let result = VerifyResult::unchanged_with_accumulated(
        #[cfg(feature = "accumulator")]
        inputs,
    );

    // This value is only read once the memo is verified. It's therefore safe
    // to write a non-final value here.
    #[cfg(feature = "accumulator")]
    old_revisions.store_accumulated_inputs(inputs);

    result
}

/// An input that changed since the reader was verified still holds the value the reader saw when
/// its memo is up to date in this revision and has the fingerprint the reader recorded. The
/// input's durability must not have dropped below the reader's: the reader would keep its higher
/// durability and later skip checking the input when only less durable inputs change.
fn unchanged_by_fingerprint(
    zalsa: &Zalsa,
    dependency_index: DatabaseKeyIndex,
    saw: Fingerprint,
    reader: &QueryRevisions,
) -> Option<VerifyResult> {
    let memo = zalsa
        .lookup_ingredient(dependency_index.ingredient_index())
        .as_function()?
        .memo(zalsa, dependency_index.key_index())?;
    let header = memo.header();
    let revisions = &header.revisions;
    (header.verified_at.load() == zalsa.current_revision()
        && !header.may_be_provisional()
        && revisions.durability() >= reader.durability()
        && revisions.value_fingerprint() == saw)
        .then(|| VerifyResult::unchanged_for_memo(revisions))
}

/// Check if this memo's cycle heads have all been finalized. If so, mark it verified final and
/// return true, if not return false.
fn validate_provisional(
    zalsa: &Zalsa,
    database_key_index: DatabaseKeyIndex,
    memo_revisions: &QueryRevisions,
    memo_verified_at: Revision,
    cycle_heads: &CycleHeads,
) -> bool {
    crate::tracing::trace!("{database_key_index:?}: validate_provisional({database_key_index:?})",);

    // Test if our cycle heads (with the same revision) are now finalized.
    for cycle_head in cycle_heads {
        let Some(provisional_status) = zalsa
            .lookup_ingredient(cycle_head.database_key_index.ingredient_index())
            .as_function()
            .and_then(|function| {
                function.provisional_status(zalsa, cycle_head.database_key_index.key_index())
            })
        else {
            return false;
        };

        match provisional_status {
            ProvisionalStatus::Provisional { .. } | ProvisionalStatus::Poisoned { .. } => {
                return false;
            }
            ProvisionalStatus::Final {
                iteration,
                verified_at,
                ..
            } => {
                // Only consider the cycle head if it is from the same revision as the memo
                if verified_at != memo_verified_at {
                    return false;
                }

                // It's important to also account for the iteration for the case where:
                // thread 1: `b` -> `a` (but only in the first iteration)
                //               -> `c` -> `b`
                // thread 2: `a` -> `b`
                //
                // If we don't account for the iteration, then `a` (from iteration 0) will be finalized
                // because its cycle head `b` is now finalized, but `b` never pulled `a` in the last iteration.
                if iteration != cycle_head.iteration.load() {
                    return false;
                }
            }
        }
    }
    // Relaxed is sufficient here because there are no other writes we need to ensure have
    // happened before marking this memo as verified-final.
    memo_revisions.mark_verified_final(Ordering::Relaxed);
    true
}

/// If this is a provisional memo, validate that it was cached in the same iteration of the
/// same cycle(s) that we are still executing. If so, it is valid for reuse. This avoids
/// runaway re-execution of the same queries within a fixpoint iteration.
fn validate_same_iteration(
    zalsa: &Zalsa,
    zalsa_local: &ZalsaLocal,
    memo_database_key_index: DatabaseKeyIndex,
    memo_verified_at: Revision,
    cycle_heads: &CycleHeads,
) -> bool {
    crate::tracing::trace!("validate_same_iteration({memo_database_key_index:?})",);

    // This is an optimization to avoid unnecessary re-execution within the same revision.
    // Don't apply it when verifying memos from past revisions. We want them to re-execute
    // to verify their cycle heads and all participating queries.
    if memo_verified_at != zalsa.current_revision() {
        return false;
    }

    // Always return `false` for cycle initial values "unless" they are running in the same thread.
    if cycle_heads
        .iter_not_eq(memo_database_key_index)
        .next()
        .is_none()
    {
        // SAFETY: We do not access the query stack reentrantly.
        let on_stack = unsafe {
            zalsa_local.with_query_stack_unchecked(|stack| {
                stack
                    .iter()
                    .rev()
                    .any(|query| query.database_key_index == memo_database_key_index)
            })
        };

        return on_stack;
    }

    let cycle_heads_iter = TryClaimCycleHeadsIter::new(zalsa, zalsa_local.task(), cycle_heads);

    for cycle_head in cycle_heads_iter {
        match cycle_head {
            TryClaimHeadsResult::Cycle {
                head_iteration,
                memo_iteration,
                verified_at: head_verified_at,
            } => {
                if head_verified_at != memo_verified_at {
                    return false;
                }

                if head_iteration != memo_iteration {
                    return false;
                }
            }
            _ => {
                return false;
            }
        }
    }

    true
}

#[derive(Copy, Clone, Eq, PartialEq)]
pub(super) enum ShallowUpdate {
    /// The memo is from this revision and has already been verified
    Verified,

    /// The revision for the memo's durability hasn't changed. It can be marked as verified
    /// in this revision.
    HigherDurability,

    /// The memo requires a deep verification.
    No,
}

impl ShallowUpdate {
    pub(super) fn yes(&self) -> bool {
        matches!(
            self,
            ShallowUpdate::Verified | ShallowUpdate::HigherDurability
        )
    }
}

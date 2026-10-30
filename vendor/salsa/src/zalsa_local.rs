use std::cell::{RefCell, UnsafeCell};
use std::fmt;
use std::fmt::Formatter;
use std::panic::UnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use rustc_hash::FxHashMap;
use thin_vec::ThinVec;

#[cfg(feature = "accumulator")]
use crate::accumulator::{
    Accumulator,
    accumulated_map::{AccumulatedMap, InputAccumulatedValues},
};
use crate::active_query::{CompletedQuery, DetachedInputOutputs, QueryCompletion, QueryStack};
use crate::cycle::{AtomicIterationStamp, CycleHeads, IterationStamp, empty_cycle_heads};
use crate::durability::Durability;
use crate::fingerprint::Fingerprint;
use crate::graph::{self, EdgeSpan, Graph, Relocation, TaskGraph};
use crate::key::DatabaseKeyIndex;
use crate::runtime::Stamp;
use crate::sync::atomic::{AtomicU32, AtomicU64};
use crate::table::{PageIndex, Slot};
use crate::tracked_struct::{Disambiguator, Identity, IdentityHash};
use crate::zalsa::{IngredientIndex, Zalsa};
use crate::{Cancelled, Id, Revision};

/// State that is specific to a single task: one per database, thread and task depth
/// ([`crate::task`]).
///
/// Internally, this type uses ref-cells.
///
/// **Note also that all mutations to the local-state must be undone during unwinding.**
pub struct ZalsaLocal {
    /// Vector of active queries.
    ///
    /// Unwinding note: pushes onto this vector must be popped -- even
    /// during unwinding.
    query_stack: RefCell<QueryStack>,

    /// Stores the most recent page for a given ingredient.
    /// This is thread-local to avoid contention.
    most_recent_pages: UnsafeCell<FxHashMap<IngredientIndex, PageIndex>>,

    /// This task's cache of key to node and its reserved edge words ([`Graph::record`]).
    graph: TaskGraph,

    cancelled: CancellationToken,
}

/// A cancellation token that can be used to cancel a query computation for a specific local `Database`.
#[derive(Default, Clone, Debug)]
pub struct CancellationToken(Arc<AtomicU8>);

impl CancellationToken {
    const CANCELLED_MASK: u8 = 0b01;
    const DISABLED_MASK: u8 = 0b10;

    /// Inform the database to cancel the current query computation.
    pub fn cancel(&self) {
        self.0.fetch_or(Self::CANCELLED_MASK, Ordering::Relaxed);
    }

    /// Check if the query computation has been requested to be cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed) & Self::CANCELLED_MASK != 0
    }

    #[inline]
    fn set_cancellation_disabled(&self, disabled: bool) -> bool {
        let previous_disabled_bit = if disabled {
            self.0.fetch_or(Self::DISABLED_MASK, Ordering::Relaxed)
        } else {
            self.0.fetch_and(!Self::DISABLED_MASK, Ordering::Relaxed)
        };
        previous_disabled_bit & Self::DISABLED_MASK != 0
    }

    fn should_trigger_local_cancellation(&self) -> bool {
        self.0.load(Ordering::Relaxed) == Self::CANCELLED_MASK
    }

    fn reset(&self) {
        self.0.store(0, Ordering::Relaxed);
    }
}

impl ZalsaLocal {
    pub(crate) fn new() -> Self {
        ZalsaLocal {
            query_stack: RefCell::new(QueryStack::default()),
            most_recent_pages: UnsafeCell::new(FxHashMap::default()),
            graph: TaskGraph::default(),
            cancelled: CancellationToken::default(),
        }
    }

    /// Writes a finished query's edges into the database's edge array.
    #[inline]
    pub(crate) fn record_edges(
        &self,
        zalsa: &Zalsa,
        edges: impl ExactSizeIterator<Item = QueryEdge>,
    ) -> EdgeSpan {
        zalsa.graph().record(&self.graph, edges)
    }

    /// Writes a capture frame's edges and then its fingerprint words into the edge array
    /// ([`crate::capture`]).
    #[inline]
    pub(crate) fn record_reads(
        &self,
        zalsa: &Zalsa,
        edges: impl ExactSizeIterator<Item = QueryEdge>,
        tail: &[u32],
    ) -> EdgeSpan {
        zalsa.graph().record_with_tail(&self.graph, edges, tail)
    }

    /// The pages this task allocates in, which it gives up.
    pub(crate) fn take_unfilled_pages(
        &mut self,
    ) -> impl Iterator<Item = (IngredientIndex, PageIndex)> + '_ {
        self.most_recent_pages.get_mut().drain()
    }

    /// Allocate a new id in `table` for the given ingredient
    /// storing `value`. Remembers the most recent page from this
    /// thread and attempts to reuse it.
    pub(crate) fn allocate<'db, T: Slot>(
        &self,
        zalsa: &'db Zalsa,
        ingredient: IngredientIndex,
        mut value: impl FnOnce(Id) -> T,
    ) -> (Id, &'db T) {
        // SAFETY: `ZalsaLocal` is `!Sync`, and we never expose a reference to this field,
        // so we have exclusive access.
        let most_recent_pages = unsafe { &mut *self.most_recent_pages.get() };

        // Fast-path, we already have an unfilled page available.
        if let Some(&page) = most_recent_pages.get(&ingredient) {
            let page_ref = zalsa.table().page::<T>(page);

            // SAFETY: `ZalsaLocal` is `!Sync`, and we only insert a page into `most_recent_pages`
            // if it was allocated by our thread, so we are the unique writer.
            match unsafe { page_ref.allocate(page, value) } {
                Ok((id, value)) => return (id, value),
                Err(v) => value = v,
            }
        }

        self.allocate_cold(zalsa, ingredient, value)
    }

    #[cold]
    #[inline(never)]
    pub(crate) fn allocate_cold<'db, T: Slot>(
        &self,
        zalsa: &'db Zalsa,
        ingredient: IngredientIndex,
        mut value: impl FnOnce(Id) -> T,
    ) -> (Id, &'db T) {
        let memo_types = || {
            zalsa
                .lookup_ingredient(ingredient)
                .memo_table_types()
                .clone()
        };

        // SAFETY: `ZalsaLocal` is `!Sync`, and we never expose a reference to this field,
        // so we have exclusive access.
        let most_recent_pages = unsafe { &mut *self.most_recent_pages.get() };

        // Find the most recent page, pushing a page if needed
        let mut page = *most_recent_pages.entry(ingredient).or_insert_with(|| {
            zalsa
                .table()
                .fetch_or_push_page::<T>(ingredient, memo_types)
        });

        loop {
            // Try to allocate an entry on that page
            let page_ref = zalsa.table().page::<T>(page);

            // SAFETY: `ZalsaLocal` is `!Sync`, and we only insert a page into `most_recent_pages`
            // if it was allocated by our thread, so we are the unique writer.
            match unsafe { page_ref.allocate(page, value) } {
                // If successful, return
                Ok((id, value)) => return (id, value),

                // Otherwise, create a new page and try again.
                //
                // Note that we could try fetching a page again, but as we just filled one up
                // it is unlikely that there is a non-full one available.
                Err(v) => {
                    value = v;
                    page = zalsa.table().push_page::<T>(ingredient, memo_types());
                    most_recent_pages.insert(ingredient, page);
                }
            }
        }
    }

    #[inline]
    pub(crate) fn push_query(&self, database_key_index: DatabaseKeyIndex) -> ActiveQueryGuard<'_> {
        // SAFETY: We do not access the query stack reentrantly.
        unsafe {
            self.with_query_stack_unchecked_mut(|stack| {
                stack.push_new_query(database_key_index);

                ActiveQueryGuard {
                    local_state: self,
                    database_key_index,
                    #[cfg(debug_assertions)]
                    push_len: stack.len(),
                }
            })
        }
    }

    /// Executes a closure within the context of the current active query stacks (mutable).
    ///
    /// # Safety
    ///
    /// The closure cannot access the query stack reentrantly, whether mutable or immutable.
    #[inline(always)]
    pub(crate) unsafe fn with_query_stack_unchecked_mut<R>(
        &self,
        f: impl UnwindSafe + FnOnce(&mut QueryStack) -> R,
    ) -> R {
        // SAFETY: The caller guarantees that the query stack will not be accessed reentrantly.
        // Additionally, `ZalsaLocal` is `!Sync`, and we never expose a reference to the query
        // stack except through this method, so we have exclusive access.
        unsafe { f(&mut self.query_stack.try_borrow_mut().unwrap_unchecked()) }
    }

    /// Executes a closure within the context of the current active query stacks.
    ///
    /// # Safety
    ///
    /// No mutable references to the query stack can exist while the closure is executed.
    #[inline(always)]
    pub(crate) unsafe fn with_query_stack_unchecked<R>(
        &self,
        f: impl UnwindSafe + FnOnce(&QueryStack) -> R,
    ) -> R {
        // SAFETY: The caller guarantees that the query stack will not being accessed mutably.
        // Additionally, `ZalsaLocal` is `!Sync`, and we never expose a reference to the query
        // stack except through this method, so we have exclusive access.
        unsafe { f(&self.query_stack.try_borrow().unwrap_unchecked()) }
    }

    #[inline(always)]
    pub(crate) fn try_with_query_stack<R>(
        &self,
        f: impl UnwindSafe + FnOnce(&QueryStack) -> R,
    ) -> Option<R> {
        self.query_stack
            .try_borrow()
            .ok()
            .as_ref()
            .map(|stack| f(stack))
    }

    /// Returns the index of the active query along with its *current* durability/changed-at
    /// information. As the query continues to execute, naturally, that information may change.
    pub(crate) fn active_query(&self) -> Option<(DatabaseKeyIndex, Stamp)> {
        // SAFETY: We do not access the query stack reentrantly.
        unsafe {
            self.with_query_stack_unchecked(|stack| {
                stack
                    .last()
                    .map(|active_query| (active_query.database_key_index, active_query.stamp()))
            })
        }
    }

    /// Returns the active query, its current dependencies, and any provisional cycle results it
    /// depends on.
    pub(crate) fn active_query_with_cycle_heads(
        &self,
    ) -> Option<(DatabaseKeyIndex, Stamp, CycleHeads)> {
        // SAFETY: We do not access the query stack reentrantly.
        unsafe {
            self.with_query_stack_unchecked(|stack| {
                stack.last().map(|active_query| {
                    (
                        active_query.database_key_index,
                        active_query.stamp(),
                        active_query.cycle_heads().clone(),
                    )
                })
            })
        }
    }

    /// Add an output to the current query's list of dependencies
    ///
    /// Returns `Err` if not in a query.
    #[cfg(feature = "accumulator")]
    pub(crate) fn accumulate<A: Accumulator>(
        &self,
        index: IngredientIndex,
        value: A,
    ) -> Result<(), ()> {
        // SAFETY: We do not access the query stack reentrantly.
        unsafe {
            self.with_query_stack_unchecked_mut(|stack| {
                if let Some(top_query) = stack.last_mut() {
                    top_query.accumulate(index, value);
                    Ok(())
                } else {
                    Err(())
                }
            })
        }
    }

    /// Add an output to the current query's list of dependencies, returning whether it was new.
    pub(crate) fn add_output(&self, entity: DatabaseKeyIndex) -> bool {
        // SAFETY: We do not access the query stack reentrantly.
        unsafe {
            self.with_query_stack_unchecked_mut(|stack| {
                stack
                    .last_mut()
                    .is_some_and(|top_query| top_query.add_output(entity))
            })
        }
    }

    /// Check whether `entity` is a tracked struct that was created by the currently active query (if any)
    pub(crate) fn is_tracked_struct_of_active_query(&self, entity: DatabaseKeyIndex) -> bool {
        // SAFETY: We do not access the query stack reentrantly.
        unsafe {
            self.with_query_stack_unchecked_mut(|stack| {
                stack
                    .last_mut()
                    .is_some_and(|top_query| top_query.tracked_struct_ids().is_active(entity))
            })
        }
    }

    /// Register that currently active query reads the given input
    #[inline(always)]
    pub(crate) fn report_tracked_read(
        &self,
        input: DatabaseKeyIndex,
        durability: Durability,
        changed_at: Revision,
        fingerprint: Fingerprint,
        cycle_heads: &CycleHeads,
        #[cfg(feature = "accumulator")] has_accumulated: bool,
        #[cfg(feature = "accumulator")] accumulated_inputs: InputAccumulatedValues,
    ) {
        crate::tracing::debug!(
            "report_tracked_read(input={:?}, durability={:?}, changed_at={:?}, fingerprint={:?})",
            input,
            durability,
            changed_at,
            fingerprint
        );

        // SAFETY: We do not access the query stack reentrantly.
        unsafe {
            self.with_query_stack_unchecked_mut(|stack| {
                if let Some(top_query) = stack.last_mut() {
                    top_query.add_read(
                        input,
                        durability,
                        changed_at,
                        fingerprint,
                        cycle_heads,
                        #[cfg(feature = "accumulator")]
                        has_accumulated,
                        #[cfg(feature = "accumulator")]
                        accumulated_inputs,
                    );
                }
            })
        }
    }

    /// Register that currently active query reads the given input
    #[inline(always)]
    pub(crate) fn report_tracked_read_simple(
        &self,
        input: DatabaseKeyIndex,
        durability: Durability,
        changed_at: Revision,
    ) {
        crate::tracing::debug!(
            "report_tracked_read(input={:?}, durability={:?}, changed_at={:?})",
            input,
            durability,
            changed_at
        );

        // SAFETY: We do not access the query stack reentrantly.
        unsafe {
            self.with_query_stack_unchecked_mut(|stack| {
                if let Some(top_query) = stack.last_mut() {
                    top_query.add_read_simple(input, durability, changed_at);
                }
            })
        }
    }

    /// Update the active query's changed revision without recording a dependency edge.
    #[inline(always)]
    pub(crate) fn report_tracked_read_revision(&self, changed_at: Revision) {
        // SAFETY: We do not access the query stack reentrantly.
        unsafe {
            self.with_query_stack_unchecked_mut(|stack| {
                if let Some(top_query) = stack.last_mut() {
                    top_query.add_changed_at(changed_at);
                }
            })
        }
    }

    /// Register that the current query read an untracked value
    ///
    /// # Parameters
    ///
    /// * `current_revision`, the current revision
    #[inline(always)]
    pub(crate) fn report_untracked_read(&self, current_revision: Revision) {
        // SAFETY: We do not access the query stack reentrantly.
        unsafe {
            self.with_query_stack_unchecked_mut(|stack| {
                if let Some(top_query) = stack.last_mut() {
                    top_query.add_untracked_read(current_revision);
                }
            })
        }
    }

    /// Called when the active queries creates an index from the
    /// entity table with the index `entity_index`. Has the following effects:
    ///
    /// * Add a query read on `DatabaseKeyIndex::for_table(entity_index)`
    /// * Identify a unique disambiguator for the hash within the current query,
    ///   adding the hash to the current query's disambiguator table.
    /// * Returns a tuple of:
    ///   * the id of the current query
    ///   * the current dependencies (durability, changed_at) of current query
    ///   * the disambiguator index
    #[track_caller]
    pub(crate) fn disambiguate(&self, key: IdentityHash) -> (Stamp, Disambiguator) {
        // SAFETY: We do not access the query stack reentrantly.
        unsafe {
            self.with_query_stack_unchecked_mut(|stack| {
                let top_query = stack.last_mut().expect(
                    "cannot create a tracked struct disambiguator outside of a tracked function",
                );
                let disambiguator = top_query.disambiguate(key);
                (top_query.stamp(), disambiguator)
            })
        }
    }

    #[track_caller]
    pub(crate) fn tracked_struct_id(&self, identity: &Identity) -> Option<Id> {
        // SAFETY: We do not access the query stack reentrantly.
        unsafe {
            self.with_query_stack_unchecked_mut(|stack| {
                let top_query = stack
                    .last_mut()
                    .expect("cannot create a tracked struct ID outside of a tracked function");
                top_query.tracked_struct_ids_mut().reuse(identity)
            })
        }
    }

    #[track_caller]
    pub(crate) fn store_tracked_struct_id(&self, identity: Identity, id: Id) {
        // SAFETY: We do not access the query stack reentrantly.
        unsafe {
            self.with_query_stack_unchecked_mut(|stack| {
                let top_query = stack
                    .last_mut()
                    .expect("cannot store a tracked struct ID outside of a tracked function");
                top_query.tracked_struct_ids_mut().insert(identity, id);
            })
        }
    }

    #[inline]
    pub(crate) fn cancellation_token(&self) -> CancellationToken {
        self.cancelled.clone()
    }

    #[inline]
    pub(crate) fn uncancel(&self) {
        self.cancelled.reset();
    }

    #[inline]
    pub fn should_trigger_local_cancellation(&self) -> bool {
        self.cancelled.should_trigger_local_cancellation()
    }

    #[cold]
    pub(crate) fn unwind_cancelled(&self) {
        Cancelled::Local.throw();
    }

    #[inline]
    pub(crate) fn set_cancellation_disabled(&self, was_disabled: bool) -> bool {
        self.cancelled.set_cancellation_disabled(was_disabled)
    }
}

// Okay to implement as `ZalsaLocal`` is !Sync
// - `most_recent_pages` can't observe broken states as we cannot panic such that we enter an
//   inconsistent state
// - neither can `query_stack` as we require the closures accessing it to be `UnwindSafe`
impl std::panic::RefUnwindSafe for ZalsaLocal {}

/// Summarizes "all the inputs that a query used" and "all the outputs it has written to".
///
/// 16 bytes, so a memo header is 24 (data.md: "a memo 24 plus 4 per edge").
// #[derive(Clone)] cloning this is expensive, so we don't derive
pub(crate) struct QueryRevisions {
    /// The revision in which some input last changed in the low 56 bits ([`Self::changed_at`]);
    /// above it the minimum durability of the inputs (2 bits), whether the cycle heads are
    /// verified to not be provisional anymore ([`Self::VERIFIED_FINAL`]), and whether any input
    /// read had direct or indirect accumulated values (`ACCUMULATED_INPUTS`).
    /// `changed_at` and durability are written only through `&mut`; the two flags change on a
    /// shared memo, so the word is atomic.
    stamp: AtomicU64,

    /// The query origin, and optional data for tracked structs, cycles, accumulators and the
    /// value fingerprints the inputs had when this query read them. The fingerprints are those
    /// of the inputs that have one, sorted by the input's position in the origin's edges; an
    /// input without an entry is compared by revision alone. Then, at [`EdgeFingerprint::VALUE`],
    /// the hash of the memo's own value, kept when the value is evicted; there is none when the
    /// function's output has no stable hash or the value is provisional. Most memos have
    /// neither, and then there is no box.
    origin_and_extra: OriginAndExtra,
}

impl std::fmt::Debug for QueryRevisions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut f = f.debug_struct("QueryRevisions");
        f.field("changed_at", &self.changed_at())
            .field("durability", &self.durability())
            .field("origin_and_extra", &self.origin_and_extra)
            .field("verified_final", &self.verified_final(Ordering::Relaxed));
        #[cfg(feature = "accumulator")]
        f.field("accumulated_inputs", &self.accumulated_inputs());
        f.finish()
    }
}

/// The value fingerprint a dependency had when it was read.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct EdgeFingerprint {
    /// Position of the input in [`QueryOriginRef::edges`], or [`EdgeFingerprint::VALUE`].
    pub(crate) edge: u32,
    pub(crate) fingerprint: Fingerprint,
}

impl EdgeFingerprint {
    /// The position of the memo's own value fingerprint. It sorts after every edge, and no
    /// query reads this many inputs (`ActiveQuery::add_read` refuses to record it).
    pub(crate) const VALUE: u32 = u32::MAX;
}

impl QueryRevisions {
    const CHANGED_AT_MASK: u64 = (1 << 56) - 1;
    const DURABILITY_SHIFT: u32 = 56;
    const DURABILITY_MASK: u64 = 0b11 << Self::DURABILITY_SHIFT;
    const VERIFIED_FINAL: u64 = 1 << 58;
    #[cfg(feature = "accumulator")]
    const ACCUMULATED_INPUTS: u64 = 1 << 59;

    pub(crate) fn new(
        changed_at: Revision,
        durability: Durability,
        origin_and_extra: OriginAndExtra,
        verified_final: bool,
        #[cfg(feature = "accumulator")] accumulated_inputs: InputAccumulatedValues,
        fingerprints: ThinVec<EdgeFingerprint>,
    ) -> Self {
        let mut revisions = QueryRevisions {
            stamp: AtomicU64::new(Self::stamp_bits(changed_at, durability)),
            origin_and_extra,
        };
        revisions.set_verified_final(verified_final);
        #[cfg(feature = "accumulator")]
        revisions.store_accumulated_inputs(accumulated_inputs);
        if !fingerprints.is_empty() {
            *revisions.origin_and_extra.fingerprints_mut() = fingerprints;
        }
        revisions
    }

    fn stamp_bits(changed_at: Revision, durability: Durability) -> u64 {
        let changed_at = changed_at.as_usize() as u64;
        assert!(
            changed_at <= Self::CHANGED_AT_MASK,
            "salsa: more than 2^56 revisions"
        );
        changed_at | ((durability.index() as u64) << Self::DURABILITY_SHIFT)
    }

    /// The revision in which some input last changed.
    #[inline]
    pub(crate) fn changed_at(&self) -> Revision {
        // Relaxed: written before the memo was published, which ordered it before our read.
        Revision::from((self.stamp.load(Ordering::Relaxed) & Self::CHANGED_AT_MASK) as usize)
    }

    /// The minimum durability of the inputs.
    #[inline]
    pub(crate) fn durability(&self) -> Durability {
        let bits = (self.stamp.load(Ordering::Relaxed) & Self::DURABILITY_MASK)
            >> Self::DURABILITY_SHIFT;
        Durability::from_index(bits as u8)
    }

    pub(crate) fn set_changed_at(&mut self, changed_at: Revision) {
        let durability = self.durability();
        self.set_stamp(changed_at, durability);
    }

    pub(crate) fn set_durability(&mut self, durability: Durability) {
        let changed_at = self.changed_at();
        self.set_stamp(changed_at, durability);
    }

    fn set_stamp(&mut self, changed_at: Revision, durability: Durability) {
        let flags = self.stamp.load(Ordering::Relaxed)
            & !(Self::CHANGED_AT_MASK | Self::DURABILITY_MASK);
        // Relaxed: `&mut`, so no other task sees the memo.
        self.stamp
            .store(flags | Self::stamp_bits(changed_at, durability), Ordering::Relaxed);
    }

    /// Are the cycle heads verified to not be provisional anymore?
    #[inline]
    pub(crate) fn verified_final(&self, order: Ordering) -> bool {
        self.stamp.load(order) & Self::VERIFIED_FINAL != 0
    }

    /// Marks the cycle heads verified final on a shared memo.
    pub(crate) fn mark_verified_final(&self, order: Ordering) {
        self.stamp.fetch_or(Self::VERIFIED_FINAL, order);
    }

    pub(crate) fn set_verified_final(&mut self, verified_final: bool) {
        // Relaxed: `&mut`, so no other task sees the memo.
        let stamp = self.stamp.load(Ordering::Relaxed);
        let stamp = if verified_final {
            stamp | Self::VERIFIED_FINAL
        } else {
            stamp & !Self::VERIFIED_FINAL
        };
        self.stamp.store(stamp, Ordering::Relaxed);
    }

    /// [`InputAccumulatedValues::Any`] if any input read during the query's execution has any
    /// direct or indirect accumulated values.
    #[cfg(feature = "accumulator")]
    #[inline]
    pub(crate) fn accumulated_inputs(&self) -> InputAccumulatedValues {
        if self.stamp.load(Ordering::Acquire) & Self::ACCUMULATED_INPUTS != 0 {
            InputAccumulatedValues::Any
        } else {
            InputAccumulatedValues::Empty
        }
    }

    #[cfg(feature = "accumulator")]
    pub(crate) fn store_accumulated_inputs(&self, accumulated: InputAccumulatedValues) {
        if accumulated.is_any() {
            self.stamp
                .fetch_or(Self::ACCUMULATED_INPUTS, Ordering::Release);
        } else {
            self.stamp
                .fetch_and(!Self::ACCUMULATED_INPUTS, Ordering::Release);
        }
    }

    /// The hash of the memo's value, or [`Fingerprint::NONE`] if it has none.
    #[inline]
    pub(crate) fn value_fingerprint(&self) -> Fingerprint {
        match self.origin_and_extra.fingerprints().last() {
            Some(last) if last.edge == EdgeFingerprint::VALUE => last.fingerprint,
            _ => Fingerprint::NONE,
        }
    }

    /// Sets the hash of the memo's value; [`Fingerprint::NONE`] removes it.
    pub(crate) fn set_value_fingerprint(&mut self, fingerprint: Fingerprint) {
        let has_value = self
            .origin_and_extra
            .fingerprints()
            .last()
            .is_some_and(|last| last.edge == EdgeFingerprint::VALUE);
        match (has_value, fingerprint.is_value()) {
            (true, true) => {
                let fingerprints = self.origin_and_extra.fingerprints_mut();
                let last = fingerprints.len() - 1;
                fingerprints[last].fingerprint = fingerprint;
            }
            (true, false) => {
                self.origin_and_extra.fingerprints_mut().pop();
            }
            (false, true) => self.origin_and_extra.fingerprints_mut().push(EdgeFingerprint {
                edge: EdgeFingerprint::VALUE,
                fingerprint,
            }),
            (false, false) => {}
        }
    }

    /// The fingerprint recorded for the input at `edge` in [`QueryOriginRef::edges`], if it was
    /// read with a value fingerprint.
    pub(crate) fn edge_fingerprint(&self, edge: usize) -> Option<Fingerprint> {
        let edge = u32::try_from(edge)
            .ok()
            .filter(|&edge| edge != EdgeFingerprint::VALUE)?;
        let fingerprints = self.origin_and_extra.fingerprints();
        let position = fingerprints
            .binary_search_by_key(&edge, |entry| entry.edge)
            .ok()?;
        Some(fingerprints[position].fingerprint)
    }

    /// Returns the semantic origin of this query, its edges read from `graph`.
    #[inline]
    pub(crate) fn origin<'a>(&'a self, graph: &'a Graph) -> QueryOriginRef<'a> {
        self.origin_and_extra.origin(graph)
    }

    /// Whether the value was derived by running the function (tracked or not), so it has edges.
    #[inline]
    pub(crate) const fn is_derived(&self) -> bool {
        self.origin_and_extra.is_derived()
    }

    #[inline]
    pub(crate) const fn is_derived_untracked(&self) -> bool {
        self.origin_and_extra.is_derived_untracked()
    }

    /// Moves the edges into the array a compaction makes.
    pub(crate) fn relocate_edges(&mut self, relocation: &mut Relocation<'_>) {
        self.origin_and_extra.relocate(relocation);
    }

    /// Discard dependency and output edges that a never-changing query will
    /// never need again.
    ///
    /// This is called after output reconciliation so that a query that becomes
    /// never-changing during re-execution can still preserve outputs recreated
    /// by that execution.
    #[cfg(not(feature = "persistence"))]
    pub(crate) fn discard_edges_if_never_change(&mut self) {
        if self.durability() != Durability::NEVER_CHANGE
            || !self.is_derived()
            || self.is_derived_untracked()
            || !self.cycle_heads().is_empty()
        {
            return;
        }

        #[cfg(feature = "accumulator")]
        if self.accumulated_inputs().is_any() {
            return;
        }

        self.origin_and_extra.clear_edges();
        let value = self.value_fingerprint();
        if !self.origin_and_extra.fingerprints().is_empty() {
            *self.origin_and_extra.fingerprints_mut() = ThinVec::new();
        }
        self.set_value_fingerprint(value);
    }

    #[cfg(feature = "salsa_unstable")]
    pub(crate) fn allocation_size(&self) -> usize {
        self.origin_and_extra.allocation_size()
    }
}

/// Builder and serialization form for optional [`QueryRevisions`] data.
///
/// In particular, not all queries create tracked structs, participate
/// in cycles, or create accumulators. Stored revisions move this state
/// into the allocation owned by [`OriginAndExtra`].
#[derive(Debug, Default)]
#[cfg_attr(feature = "persistence", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "persistence", serde(transparent))]
pub(crate) struct QueryRevisionsExtra(Option<QueryRevisionsExtraInner>);

impl QueryRevisionsExtra {
    pub fn new(
        #[cfg(feature = "accumulator")] accumulated: AccumulatedMap,
        mut tracked_struct_ids: ThinVec<(Identity, Id)>,
        cycle_heads: CycleHeads,
        iteration: IterationStamp,
        force_extra: bool,
    ) -> Self {
        #[cfg(feature = "accumulator")]
        let accumulated_is_empty = accumulated.is_empty();
        #[cfg(not(feature = "accumulator"))]
        let accumulated_is_empty = true;
        // `cycle_heads` may be empty because cycle completion moved its entries out before
        // constructing the revisions. Preserve extra storage so those heads can be restored
        // without rebuilding the origin allocation.
        let inner = if !force_extra
            && accumulated_is_empty
            && tracked_struct_ids.is_empty()
            && cycle_heads.is_empty()
            && iteration.is_default()
        {
            None
        } else {
            tracked_struct_ids.shrink_to_fit();

            Some(QueryRevisionsExtraInner {
                #[cfg(feature = "accumulator")]
                accumulated,
                cycle_heads,
                tracked_struct_ids,
                iteration: iteration.into(),
                cycle_converged: false,
            })
        };

        Self(inner)
    }
}

#[cfg_attr(feature = "persistence", derive(serde::Serialize, serde::Deserialize))]
struct QueryRevisionsExtraInner {
    #[cfg(feature = "accumulator")]
    #[cfg_attr(feature = "persistence", serde(skip))] // TODO: Support serializing accumulators
    accumulated: AccumulatedMap,

    /// The ids of tracked structs created by this query.
    ///
    /// This table plays an important role when queries are
    /// re-executed:
    /// * A clone of this field is used as the initial set of
    ///   `TrackedStructId`s for the query on the next execution.
    /// * The query will thus re-use the same ids if it creates
    ///   tracked structs with the same `KeyStruct` as before.
    ///   It may also create new tracked structs.
    /// * One tricky case involves deleted structs. If
    ///   the old revision created a struct S but the new
    ///   revision did not, there will still be a map entry
    ///   for S. This is because queries only ever grow the map
    ///   and they start with the same entries as from the
    ///   previous revision. To handle this, `diff_outputs` compares
    ///   the structs from the old/new revision and retains
    ///   only entries that appeared in the new revision.
    //
    // TODO: We only need to serialize the IDs of tracked structs that
    // are actually going to be serialized. Those that are not will
    // be created with new IDs anyways.
    tracked_struct_ids: ThinVec<(Identity, Id)>,

    /// This result was computed based on provisional values from
    /// these cycle heads. The "cycle head" is the query responsible
    /// for managing a fixpoint iteration. In a cycle like
    /// `--> A --> B --> C --> A`, the cycle head is query `A`: it is
    /// the query whose value is requested while it is executing,
    /// which must provide the initial provisional value and decide,
    /// after each iteration, whether the cycle has converged or must
    /// iterate again.
    cycle_heads: CycleHeads,

    #[cfg_attr(feature = "persistence", serde(skip))]
    iteration: AtomicIterationStamp,

    /// Stores for nested cycle heads whether they've converged in the last iteration.
    /// This value is always `false` for other queries.
    #[cfg_attr(feature = "persistence", serde(skip))]
    cycle_converged: bool,
}

impl QueryRevisionsExtraInner {
    fn empty() -> Self {
        QueryRevisionsExtraInner {
            #[cfg(feature = "accumulator")]
            accumulated: AccumulatedMap::default(),
            tracked_struct_ids: ThinVec::default(),
            cycle_heads: empty_cycle_heads().clone(),
            iteration: IterationStamp::default().into(),
            cycle_converged: false,
        }
    }

    #[cfg(feature = "salsa_unstable")]
    fn allocation_size(&self) -> usize {
        let QueryRevisionsExtraInner {
            #[cfg(feature = "accumulator")]
            accumulated,
            tracked_struct_ids,
            cycle_heads,
            iteration: _,
            cycle_converged: _,
        } = self;

        #[cfg(feature = "accumulator")]
        let b = accumulated.allocation_size();
        #[cfg(not(feature = "accumulator"))]
        let b = 0;
        b + cycle_heads.allocation_size() + std::mem::size_of_val(tracked_struct_ids.as_slice())
    }
}

impl fmt::Debug for QueryRevisionsExtraInner {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        struct FmtTrackedStructIds<'a>(&'a ThinVec<(Identity, Id)>);

        impl fmt::Debug for FmtTrackedStructIds<'_> {
            fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
                let mut f = f.debug_list();

                if self.0.len() > 5 {
                    f.entries(&self.0[..5]);
                    f.finish_non_exhaustive()
                } else {
                    f.entries(self.0);
                    f.finish()
                }
            }
        }

        let mut f = f.debug_struct("QueryRevisionsExtraInner");

        f.field("cycle_heads", &self.cycle_heads)
            .field("iteration", &self.iteration)
            .field("cycle_converged", &self.cycle_converged);

        #[cfg(feature = "accumulator")]
        {
            f.field("accumulated", &self.accumulated);
        }

        f.field(
            "tracked_struct_ids",
            &FmtTrackedStructIds(&self.tracked_struct_ids),
        );

        f.finish()
    }
}

#[cfg(not(feature = "shuttle"))]
const _: [(); std::mem::size_of::<QueryRevisions>()] = [(); 16];

const _: [(); std::mem::size_of::<EdgeFingerprint>()] = [(); 20];

#[cfg(not(feature = "shuttle"))]
#[cfg(target_pointer_width = "64")]
const _: [(); std::mem::size_of::<QueryRevisionsExtraInner>()] =
    [(); std::mem::size_of::<[usize; if cfg!(feature = "accumulator") { 7 } else { 3 }]>()];

impl QueryRevisions {
    pub(crate) fn fixpoint_initial(query: DatabaseKeyIndex, iteration: IterationStamp) -> Self {
        Self::new(
            Revision::start(),
            Durability::MAX,
            OriginAndExtra::derived(
                EdgeSpan::EMPTY,
                QueryRevisionsExtra::new(
                    #[cfg(feature = "accumulator")]
                    AccumulatedMap::default(),
                    ThinVec::default(),
                    CycleHeads::initial(query, iteration),
                    iteration,
                    false,
                ),
            ),
            false,
            #[cfg(feature = "accumulator")]
            InputAccumulatedValues::Empty,
            ThinVec::new(),
        )
    }

    /// Returns a reference to the `AccumulatedMap` for this query, or `None` if the map is empty.
    #[cfg(feature = "accumulator")]
    pub(crate) fn accumulated(&self) -> Option<&AccumulatedMap> {
        self.origin_and_extra
            .extra()
            .map(|extra| &extra.accumulated)
            .filter(|map| !map.is_empty())
    }

    /// Returns a reference to the `CycleHeads` for this query.
    pub(crate) fn cycle_heads(&self) -> &CycleHeads {
        match self.origin_and_extra.extra() {
            Some(extra) => &extra.cycle_heads,
            None => empty_cycle_heads(),
        }
    }

    /// Sets the `CycleHeads` for this query.
    pub(crate) fn set_cycle_heads(&mut self, cycle_heads: CycleHeads, iteration: IterationStamp) {
        let extra = self.origin_and_extra.get_or_insert_extra();
        extra.cycle_heads = cycle_heads;
        extra.iteration = iteration.into();
    }

    pub(crate) fn cycle_converged(&self) -> bool {
        match self.origin_and_extra.extra() {
            Some(extra) => extra.cycle_converged,
            None => false,
        }
    }

    pub(crate) fn set_cycle_converged(&mut self, cycle_converged: bool) {
        if let Some(extra) = self.origin_and_extra.extra_mut() {
            extra.cycle_converged = cycle_converged
        }
    }

    pub(crate) fn iteration(&self) -> IterationStamp {
        match self.origin_and_extra.extra() {
            Some(extra) => extra.iteration.load(),
            None => IterationStamp::default(),
        }
    }

    pub(crate) fn set_iteration_count(
        &self,
        database_key_index: DatabaseKeyIndex,
        iteration: IterationStamp,
    ) {
        let Some(extra) = self.origin_and_extra.extra() else {
            return;
        };
        debug_assert!(extra.iteration.load() <= iteration);

        extra.iteration.store_iteration(iteration);

        extra
            .cycle_heads
            .update_iteration_count(database_key_index, iteration);
    }

    fn extra(&self) -> Option<&QueryRevisionsExtraInner> {
        self.origin_and_extra.extra()
    }

    /// Returns the ids of the tracked structs created when running this query.
    pub fn tracked_struct_ids(&self) -> &[(Identity, Id)] {
        self.extra()
            .map(|extra| &*extra.tracked_struct_ids)
            .unwrap_or_default()
    }

    /// Returns a mutable reference to the `IdentityMap` for this query, or `None` if the map is empty.
    pub fn tracked_struct_ids_mut(&mut self) -> Option<&mut ThinVec<(Identity, Id)>> {
        self.origin_and_extra
            .extra_mut()
            .map(|extra| &mut extra.tracked_struct_ids)
            .filter(|tracked_struct_ids| !tracked_struct_ids.is_empty())
    }
}

/// Tracks the way that a memoized value for a query was created.
///
/// This is a borrowed view of a query origin stored in [`OriginAndExtra`].
#[derive(Debug, Clone, Copy)]
pub enum QueryOriginRef<'a> {
    /// The value was assigned as the output of another query (e.g., using `specify`).
    /// The `DatabaseKeyIndex` is the identity of the assigning query.
    Assigned(DatabaseKeyIndex),

    /// The value was derived by executing a function
    /// and we were able to track ALL of that function's inputs.
    /// Those inputs are described in [`QueryEdges`].
    Derived(QueryEdges<'a>),

    /// The value was derived by executing a function
    /// but that function also reported that it read untracked inputs.
    /// The [`QueryEdges`] argument contains a listing of all the inputs we saw
    /// (but we know there were more).
    DerivedUntracked(QueryEdges<'a>),
}

impl<'a> QueryOriginRef<'a> {
    /// Indices for queries *read* by this query
    #[inline]
    pub(crate) fn inputs(self) -> impl DoubleEndedIterator<Item = DatabaseKeyIndex> + use<'a> {
        self.edges().iter().filter_map(|edge| match edge.kind() {
            QueryEdgeKind::Input => Some(edge.key()),
            QueryEdgeKind::Output => None,
        })
    }

    /// Indices for queries *written* by this query (if any)
    pub(crate) fn outputs(self) -> impl DoubleEndedIterator<Item = DatabaseKeyIndex> + use<'a> {
        let opt_edges = match self {
            QueryOriginRef::Derived(edges) | QueryOriginRef::DerivedUntracked(edges) => Some(edges),
            QueryOriginRef::Assigned(_) => None,
        };
        opt_edges.into_iter().flat_map(output_edges)
    }

    #[inline]
    pub(crate) const fn edges(self) -> QueryEdges<'a> {
        match self {
            QueryOriginRef::Derived(edges) | QueryOriginRef::DerivedUntracked(edges) => edges,
            QueryOriginRef::Assigned(_) => QueryEdges::wide(&[]),
        }
    }
}

// The discriminants reserve bit 1 for derived origins, allowing `Derived` and
// `DerivedUntracked` to be recognized together.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum QueryOriginKind {
    /// The value was assigned as the output of another query.
    ///
    /// This can, for example, can occur when `specify` is used.
    Assigned = 0b01,

    /// The value was derived by executing a function
    /// _and_ Salsa was able to track all of said function's inputs.
    Derived = 0b11,

    /// The value was derived by executing a function
    /// but that function also reported that it read untracked inputs.
    DerivedUntracked = 0b10,
}

/// A query origin and its optional revision data in one 8-byte word.
///
/// The low three bits are the tag: the [`QueryOriginKind`] in bits 0 and 1, and in bit 2 whether
/// the rest is a box. The word is either:
///
/// | form   | bits 3 and up                                                    | used for                                     |
/// |--------|------------------------------------------------------------------|----------------------------------------------|
/// | inline | the edges' [`EdgeSpan`]: `len` in bits 3 to 31, `start` in 32 to 63 | a derived origin with no extra data          |
/// | boxed  | the address of a `WithExtra<T>` (aligned to 8, so its low bits are free) | everything else: extra data, fingerprints, an assigned origin's key (`T` = [`DatabaseKeyIndex`]), or a span too long to inline (`T` = [`EdgeSpan`]) |
///
/// A derived origin's edges are a range of the database's edge array ([`Graph`]), 4 bytes an
/// edge, so a memo without extra data owns no allocation at all.
///
/// The tag describes the word's form; all accesses rely on it. The box's address is stored with
/// its provenance exposed and read back through it.
pub(crate) struct OriginAndExtra {
    word: u64,
}

const _: [(); std::mem::size_of::<OriginAndExtra>()] = [(); 8];

/// SAFETY: [`OriginAndExtra`] uses its tag to know its form. Its only owned allocation is a
/// `Box<WithExtra<_>>` of `Send` data.
unsafe impl Send for OriginAndExtra
where
    QueryRevisionsExtraInner: Send,
    EdgeFingerprint: Send,
{
}

/// SAFETY: Same as above, and shared access to the boxed data is `Sync`.
unsafe impl Sync for OriginAndExtra
where
    QueryRevisionsExtraInner: Sync,
    EdgeFingerprint: Sync,
{
}

impl OriginAndExtra {
    const KIND_MASK: u64 = 0b011;
    const BOXED: u64 = 0b100;
    const TAG_MASK: u64 = 0b111;
    /// The longest span the inline form holds; a longer one is boxed.
    const INLINE_LEN: u32 = (1 << 29) - 1;

    #[inline]
    pub(crate) fn derived(edges: EdgeSpan, extra: QueryRevisionsExtra) -> Self {
        Self::new_derived(edges, QueryOriginKind::Derived, extra)
    }

    #[inline]
    pub(crate) fn derived_untracked(edges: EdgeSpan, extra: QueryRevisionsExtra) -> Self {
        Self::new_derived(edges, QueryOriginKind::DerivedUntracked, extra)
    }

    pub(crate) fn assigned(key: DatabaseKeyIndex) -> Self {
        Self::boxed(QueryOriginKind::Assigned, WithExtra::new(None, key))
    }

    fn assigned_with_extra(key: DatabaseKeyIndex, extra: QueryRevisionsExtraInner) -> Self {
        Self::boxed(QueryOriginKind::Assigned, WithExtra::new(Some(extra), key))
    }

    #[inline]
    fn new_derived(edges: EdgeSpan, kind: QueryOriginKind, extra: QueryRevisionsExtra) -> Self {
        match extra.0 {
            None if edges.len <= Self::INLINE_LEN => Self {
                word: (u64::from(edges.start) << 32) | (u64::from(edges.len) << 3) | kind as u64,
            },
            extra => Self::boxed(kind, WithExtra::new(extra, edges)),
        }
    }

    fn boxed<T>(kind: QueryOriginKind, with_extra: WithExtra<T>) -> Self {
        let address = Box::into_raw(Box::new(with_extra)).expose_provenance() as u64;
        debug_assert_eq!(address & Self::TAG_MASK, 0);
        Self {
            word: address | Self::BOXED | kind as u64,
        }
    }

    #[inline]
    const fn kind(&self) -> QueryOriginKind {
        match self.word & Self::KIND_MASK {
            0b01 => QueryOriginKind::Assigned,
            0b11 => QueryOriginKind::Derived,
            0b10 => QueryOriginKind::DerivedUntracked,
            _ => panic!("invalid query origin kind"),
        }
    }

    #[inline]
    const fn is_boxed(&self) -> bool {
        self.word & Self::BOXED != 0
    }

    /// The box's address with its provenance; only meaningful when [`Self::is_boxed`].
    #[inline]
    fn allocation<T>(&self) -> *mut WithExtra<T> {
        std::ptr::with_exposed_provenance_mut((self.word & !Self::TAG_MASK) as usize)
    }

    /// The box's parts common to every `T`.
    #[inline]
    fn head(&self) -> Option<&WithExtraHead> {
        if !self.is_boxed() {
            return None;
        }
        // `WithExtra` is `repr(C)` with its head first.
        // SAFETY: A boxed origin owns a live `WithExtra`, borrowed here through `self`.
        Some(unsafe { &*self.allocation::<()>().cast::<WithExtraHead>() })
    }

    #[inline]
    fn head_mut(&mut self) -> Option<&mut WithExtraHead> {
        if !self.is_boxed() {
            return None;
        }
        // SAFETY: As in `head`, and the box is uniquely borrowed through `self`.
        Some(unsafe { &mut *self.allocation::<()>().cast::<WithExtraHead>() })
    }

    fn extra(&self) -> Option<&QueryRevisionsExtraInner> {
        self.head()?.extra.as_ref()
    }

    fn extra_mut(&mut self) -> Option<&mut QueryRevisionsExtraInner> {
        self.head_mut()?.extra.as_mut()
    }

    /// The box, made now for an inline origin (which is always derived).
    fn head_or_box(&mut self) -> &mut WithExtraHead {
        if !self.is_boxed() {
            let span = self.span().expect("salsa: an inline origin is derived");
            let kind = self.kind();
            // The inline form owns nothing, so dropping it here frees nothing.
            *self = Self::boxed(kind, WithExtra::new(None, span));
        }
        self.head_mut().expect("salsa: the origin was just boxed")
    }

    fn get_or_insert_extra(&mut self) -> &mut QueryRevisionsExtraInner {
        self.head_or_box()
            .extra
            .get_or_insert_with(QueryRevisionsExtraInner::empty)
    }

    /// The value fingerprints the inputs had when read; see [`QueryRevisions::edge_fingerprint`].
    #[inline]
    fn fingerprints(&self) -> &[EdgeFingerprint] {
        self.head()
            .map_or(&[][..], |head| head.fingerprints.as_slice())
    }

    /// The fingerprints, boxing the origin if they are written.
    fn fingerprints_mut(&mut self) -> &mut ThinVec<EdgeFingerprint> {
        &mut self.head_or_box().fingerprints
    }

    /// The edges of a derived origin.
    #[inline]
    pub(crate) fn span(&self) -> Option<EdgeSpan> {
        match (self.kind(), self.is_boxed()) {
            (QueryOriginKind::Assigned, _) => None,
            // SAFETY: A boxed derived origin owns a `WithExtra<EdgeSpan>`.
            (_, true) => Some(unsafe { (*self.allocation::<EdgeSpan>()).origin }),
            (_, false) => Some(EdgeSpan {
                start: (self.word >> 32) as u32,
                len: ((self.word >> 3) & u64::from(Self::INLINE_LEN)) as u32,
            }),
        }
    }

    /// Replaces a derived origin's edges; an assigned origin has none.
    fn set_span(&mut self, edges: EdgeSpan) {
        match (self.kind(), self.is_boxed()) {
            (QueryOriginKind::Assigned, _) => {}
            // SAFETY: A boxed derived origin owns a `WithExtra<EdgeSpan>`, uniquely borrowed
            // through `self`.
            (_, true) => unsafe { (*self.allocation::<EdgeSpan>()).origin = edges },
            (kind, false) => *self = Self::new_derived(edges, kind, QueryRevisionsExtra(None)),
        }
    }

    /// Moves the edges into the array a compaction makes.
    pub(crate) fn relocate(&mut self, relocation: &mut Relocation<'_>) {
        if let Some(span) = self.span() {
            self.set_span(relocation.relocate(span));
        }
    }

    /// Drops the edges; their words are dead until the next compaction.
    #[cfg(not(feature = "persistence"))]
    fn clear_edges(&mut self) {
        self.set_span(EdgeSpan::EMPTY);
    }

    #[inline]
    const fn is_derived(&self) -> bool {
        !matches!(self.kind(), QueryOriginKind::Assigned)
    }

    const fn is_derived_untracked(&self) -> bool {
        matches!(self.kind(), QueryOriginKind::DerivedUntracked)
    }

    #[inline]
    fn origin<'a>(&'a self, graph: &'a Graph) -> QueryOriginRef<'a> {
        match self.kind() {
            QueryOriginKind::Assigned => QueryOriginRef::Assigned(self.assigned_key()),
            kind => {
                let span = self
                    .span()
                    .expect("salsa: a derived origin has an edge span");
                let edges = QueryEdges::in_graph(graph, span);
                if kind == QueryOriginKind::Derived {
                    QueryOriginRef::Derived(edges)
                } else {
                    QueryOriginRef::DerivedUntracked(edges)
                }
            }
        }
    }

    fn assigned_key(&self) -> DatabaseKeyIndex {
        assert!(
            matches!(self.kind(), QueryOriginKind::Assigned) && self.is_boxed(),
            "salsa: only an assigned origin has an assigning key"
        );
        // SAFETY: An assigned origin is always boxed, as a `WithExtra<DatabaseKeyIndex>`.
        unsafe { (*self.allocation::<DatabaseKeyIndex>()).origin }
    }

    /// The bytes this origin owns or uses: its box and what hangs off it, and its share of the
    /// edge array.
    #[cfg(any(test, feature = "salsa_unstable"))]
    fn allocation_size(&self) -> usize {
        let edges = self
            .span()
            .map_or(0, |span| span.len as usize * size_of::<u32>());
        let Some(head) = self.head() else {
            return edges;
        };
        let boxed = match self.kind() {
            QueryOriginKind::Assigned => size_of::<WithExtra<DatabaseKeyIndex>>(),
            _ => size_of::<WithExtra<EdgeSpan>>(),
        };
        let fingerprints = if head.fingerprints.is_empty() {
            0
        } else {
            2 * size_of::<usize>() + size_of_val(head.fingerprints.as_slice())
        };

        #[cfg(feature = "salsa_unstable")]
        if let Some(extra) = &head.extra {
            return edges + boxed + fingerprints + extra.allocation_size();
        }

        edges + boxed + fingerprints
    }
}

impl Drop for OriginAndExtra {
    fn drop(&mut self) {
        if !self.is_boxed() {
            return;
        }
        // SAFETY: A boxed origin owns the `WithExtra` that `OriginAndExtra::boxed` made, of the
        // type its kind names; `Drop` runs once.
        unsafe {
            match self.kind() {
                QueryOriginKind::Assigned => {
                    drop(Box::from_raw(self.allocation::<DatabaseKeyIndex>()))
                }
                QueryOriginKind::Derived | QueryOriginKind::DerivedUntracked => {
                    drop(Box::from_raw(self.allocation::<EdgeSpan>()))
                }
            }
        }
    }
}

impl std::fmt::Debug for OriginAndExtra {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut f = f.debug_struct("OriginAndExtra");
        match self.kind() {
            QueryOriginKind::Assigned => f.field("assigned", &self.assigned_key()),
            QueryOriginKind::Derived => f.field("derived", &self.span()),
            QueryOriginKind::DerivedUntracked => f.field("derived_untracked", &self.span()),
        };
        if let Some(extra) = self.extra() {
            f.field("extra", extra);
        }
        if !self.fingerprints().is_empty() {
            f.field("fingerprints", &self.fingerprints());
        }
        f.finish()
    }
}

/// What a boxed origin holds besides its own payload: the extra data most memos don't have
/// (`None` when the box holds only fingerprints, a key or a long span) and the input
/// fingerprints.
#[repr(C)]
struct WithExtraHead {
    extra: Option<QueryRevisionsExtraInner>,
    fingerprints: ThinVec<EdgeFingerprint>,
}

/// A boxed origin: the head, then the origin's own payload. Aligned to 8 so its address leaves
/// the tag's three bits free.
#[repr(C, align(8))]
struct WithExtra<T> {
    head: WithExtraHead,
    origin: T,
}

impl<T> WithExtra<T> {
    fn new(extra: Option<QueryRevisionsExtraInner>, origin: T) -> Self {
        WithExtra {
            head: WithExtraHead {
                extra,
                fingerprints: ThinVec::new(),
            },
            origin,
        }
    }
}

/// An input or output query edge.
///
/// This type stores the [`QueryEdgeKind`] as a tag on the `IngredientIndex` without
/// increasing the size of the type. Its 12-byte size is meaningful because inputs and
/// outputs are stored contiguously.
#[derive(Copy, Clone, PartialEq, Eq)]
pub struct QueryEdge {
    // Store a normalized zero-based index rather than nesting an `Id`, whose index uses a
    // `NonZeroU32(index + 1)` representation. Packed origins unpack many transient
    // `QueryEdge`s while flattening cycles; keeping the fields split lets that conversion,
    // `kind`, `Hash`, and `Eq` operate on the decoded words directly.
    index: u32,
    generation: u32,
    ingredient: IngredientIndex,
}

const _: [(); std::mem::size_of::<QueryEdge>()] = [(); 12];

impl QueryEdge {
    /// Create an input query edge with the given index.
    pub const fn input(key: DatabaseKeyIndex) -> QueryEdge {
        let id = key.key_index();

        QueryEdge {
            index: id.index(),
            generation: id.generation(),
            ingredient: key.ingredient_index(),
        }
    }

    /// Create an output query edge with the given index.
    pub const fn output(key: DatabaseKeyIndex) -> QueryEdge {
        let id = key.key_index();

        QueryEdge {
            index: id.index(),
            generation: id.generation(),
            ingredient: key.ingredient_index().with_tag(true),
        }
    }

    /// Return the key of this query edge.
    pub const fn key(self) -> DatabaseKeyIndex {
        // Clear the tag to restore the original index.
        DatabaseKeyIndex::new(self.ingredient.with_tag(false), self.id())
    }

    /// Returns the kind of this query edge.
    pub const fn kind(self) -> QueryEdgeKind {
        if self.ingredient.tag() {
            QueryEdgeKind::Output
        } else {
            QueryEdgeKind::Input
        }
    }

    const fn id(self) -> Id {
        // SAFETY: `index` came from a valid `Id` in `QueryEdge::input`.
        unsafe { Id::from_index(self.index) }.with_generation(self.generation)
    }
}

impl std::hash::Hash for QueryEdge {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        let &Self {
            index,
            ingredient,
            generation: _,
        } = self;
        // A query should never depend on the same ingredient and index at different generations:
        // advancing the generation replaces the previous identity. If this assumption is ever
        // violated, `Eq` still compares the generation, so omitting it here only causes a hash
        // collision. The ingredient's tag bit includes the edge kind in the hash.
        state.write_u32(index);
        state.write_u32(ingredient.as_u32());
    }
}

impl std::fmt::Debug for QueryEdge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut tuple = match self.kind() {
            QueryEdgeKind::Input => f.debug_tuple("Input"),
            QueryEdgeKind::Output => f.debug_tuple("Output"),
        };
        tuple.field(&self.key()).finish()
    }
}

/// A read-only view over the retained edges for a query origin: a range of the database's edge
/// array, or (persisted origins, before they are recorded) a slice of full edges.
#[derive(Copy, Clone)]
pub struct QueryEdges<'a> {
    data: QueryEdgesData<'a>,
}

#[derive(Copy, Clone)]
enum QueryEdgesData<'a> {
    Graph {
        graph: &'a Graph,
        words: &'a [AtomicU32],
    },
    Wide(&'a [QueryEdge]),
}

impl<'a> QueryEdges<'a> {
    #[inline]
    pub(crate) fn in_graph(graph: &'a Graph, span: EdgeSpan) -> Self {
        QueryEdges {
            data: QueryEdgesData::Graph {
                graph,
                words: graph.words(span),
            },
        }
    }

    pub(crate) const fn wide(edges: &'a [QueryEdge]) -> Self {
        QueryEdges {
            data: QueryEdgesData::Wide(edges),
        }
    }

    #[cfg(any(test, feature = "accumulator"))]
    pub(crate) const fn len(self) -> usize {
        match self.data {
            QueryEdgesData::Graph { words, .. } => words.len(),
            QueryEdgesData::Wide(edges) => edges.len(),
        }
    }

    #[inline]
    pub(crate) fn iter(self) -> QueryEdgeIter<'a> {
        let data = match self.data {
            QueryEdgesData::Graph { graph, words } => QueryEdgeIterData::Graph {
                graph,
                words: words.iter(),
            },
            QueryEdgesData::Wide(edges) => QueryEdgeIterData::Wide(edges.iter()),
        };

        QueryEdgeIter { data }
    }

    /// The output edges, in order. In the graph an output is told by its word's top bit, so the
    /// inputs are skipped without looking up their keys.
    pub(crate) fn iter_outputs(self) -> OutputEdgeIter<'a> {
        OutputEdgeIter(self.iter())
    }
}

pub struct QueryEdgeIter<'a> {
    data: QueryEdgeIterData<'a>,
}

enum QueryEdgeIterData<'a> {
    Graph {
        graph: &'a Graph,
        words: std::slice::Iter<'a, AtomicU32>,
    },
    Wide(std::slice::Iter<'a, QueryEdge>),
}

impl Iterator for QueryEdgeIter<'_> {
    type Item = QueryEdge;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.data {
            // Relaxed: the memo holding these edges was loaded with `Acquire`, and its edges
            // were written before it was published.
            QueryEdgeIterData::Graph { graph, words } => words
                .next()
                .map(|word| graph.decode(word.load(Ordering::Relaxed))),
            QueryEdgeIterData::Wide(edges) => edges.next().copied(),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.len();
        (len, Some(len))
    }
}

impl DoubleEndedIterator for QueryEdgeIter<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        match &mut self.data {
            QueryEdgeIterData::Graph { graph, words } => words
                .next_back()
                .map(|word| graph.decode(word.load(Ordering::Relaxed))),
            QueryEdgeIterData::Wide(edges) => edges.next_back().copied(),
        }
    }
}

impl ExactSizeIterator for QueryEdgeIter<'_> {
    fn len(&self) -> usize {
        match &self.data {
            QueryEdgeIterData::Graph { words, .. } => words.len(),
            QueryEdgeIterData::Wide(edges) => edges.len(),
        }
    }
}

impl std::iter::FusedIterator for QueryEdgeIter<'_> {}

/// The output edges of a [`QueryEdges`].
pub struct OutputEdgeIter<'a>(QueryEdgeIter<'a>);

impl OutputEdgeIter<'_> {
    fn next_word(
        words: &mut std::slice::Iter<'_, AtomicU32>,
        back: bool,
    ) -> Option<u32> {
        loop {
            let word = if back { words.next_back() } else { words.next() }?;
            let word = word.load(Ordering::Relaxed);
            if word & graph::OUTPUT != 0 {
                return Some(word);
            }
        }
    }

    fn step(&mut self, back: bool) -> Option<QueryEdge> {
        match &mut self.0.data {
            QueryEdgeIterData::Graph { graph, words } => {
                Self::next_word(words, back).map(|word| graph.decode(word))
            }
            QueryEdgeIterData::Wide(edges) => loop {
                let edge = if back { edges.next_back() } else { edges.next() }?;
                if edge.kind() == QueryEdgeKind::Output {
                    return Some(*edge);
                }
            },
        }
    }
}

impl Iterator for OutputEdgeIter<'_> {
    type Item = QueryEdge;

    fn next(&mut self) -> Option<Self::Item> {
        self.step(false)
    }
}

impl DoubleEndedIterator for OutputEdgeIter<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.step(true)
    }
}

impl<'a> IntoIterator for QueryEdges<'a> {
    type Item = QueryEdge;
    type IntoIter = QueryEdgeIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl std::fmt::Debug for QueryEdges<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum QueryEdgeKind {
    Input,
    Output,
}

/// Returns the (tracked) outputs that were executed in computing this memoized value.
///
/// These will always be in execution order.
pub(crate) fn output_edges(
    input_outputs: QueryEdges<'_>,
) -> impl DoubleEndedIterator<Item = DatabaseKeyIndex> + use<'_> {
    input_outputs.iter_outputs().map(QueryEdge::key)
}

/// When a query is pushed onto the `active_query` stack, this guard
/// is returned to represent its slot. The guard can be used to pop
/// the query from the stack -- in the case of unwinding, the guard's
/// destructor will also remove the query.
pub(crate) struct ActiveQueryGuard<'me> {
    local_state: &'me ZalsaLocal,
    #[cfg(debug_assertions)]
    push_len: usize,
    pub(crate) database_key_index: DatabaseKeyIndex,
}

impl<'me> ActiveQueryGuard<'me> {
    /// Initialize the tracked struct ids with the values from the prior execution.
    pub(crate) fn seed_tracked_struct_ids(&self, tracked_struct_ids: &[(Identity, Id)]) {
        // SAFETY: We do not access the query stack reentrantly.
        unsafe {
            self.local_state.with_query_stack_unchecked_mut(|stack| {
                #[cfg(debug_assertions)]
                assert_eq!(stack.len(), self.push_len, "mismatched push and pop");
                let frame = stack.last_mut().unwrap();
                frame.tracked_struct_ids_mut().seed(tracked_struct_ids);
            })
        }
    }

    /// Append the given `outputs` to the query's output list.
    pub(crate) fn seed_iteration(&self, zalsa: &Zalsa, previous: &QueryRevisions) {
        let durability = previous.durability();
        let changed_at = previous.changed_at();
        let edges = previous.origin(zalsa.graph()).edges();
        let untracked_read = previous.is_derived_untracked();

        let tracked_ids = previous.tracked_struct_ids();

        // SAFETY: We do not access the query stack reentrantly.
        unsafe {
            self.local_state.with_query_stack_unchecked_mut(|stack| {
                #[cfg(debug_assertions)]
                assert_eq!(stack.len(), self.push_len, "mismatched push and pop");
                let frame = stack.last_mut().unwrap();
                frame.seed_iteration(durability, changed_at, edges, untracked_read, tracked_ids);
            })
        }
    }

    pub(crate) fn take_cycle_heads(&mut self) -> CycleHeads {
        // SAFETY: We do not access the query stack reentrantly.
        unsafe {
            self.local_state.with_query_stack_unchecked_mut(|stack| {
                #[cfg(debug_assertions)]
                assert_eq!(stack.len(), self.push_len);
                let frame = stack.last_mut().unwrap();
                frame.take_cycle_heads()
            })
        }
    }

    pub(crate) fn detach(self) -> DetachedQuery<'me> {
        // SAFETY: We do not access the query stack reentrantly.
        let input_outputs = unsafe {
            self.local_state.with_query_stack_unchecked_mut(|stack| {
                #[cfg(debug_assertions)]
                assert_eq!(stack.len(), self.push_len);
                stack.last_mut().unwrap().detach_input_outputs()
            })
        };
        DetachedQuery {
            guard: self,
            input_outputs,
        }
    }

    /// Invoked when the query has successfully completed execution.
    fn complete(self, zalsa: &Zalsa, iteration: IterationStamp) -> CompletedQuery {
        let local_state = self.local_state;
        // SAFETY: We do not access the query stack reentrantly.
        let query = unsafe {
            local_state.with_query_stack_unchecked_mut(|stack| {
                stack.pop_into_revisions(
                    zalsa,
                    local_state,
                    self.database_key_index,
                    iteration,
                    #[cfg(debug_assertions)]
                    self.push_len,
                )
            })
        };
        std::mem::forget(self);
        query
    }

    /// Pops an active query from the stack. Returns the [`CompletedQuery`]
    /// which summarizes the other queries that were accessed during this
    /// query's execution.
    #[inline]
    pub(crate) fn pop(self, zalsa: &Zalsa, iteration: IterationStamp) -> CompletedQuery {
        self.complete(zalsa, iteration)
    }

    fn pop_detached_completion(
        self,
        iteration: IterationStamp,
        detached_input_outputs: DetachedInputOutputs,
        force_extra: bool,
    ) -> QueryCompletion {
        // SAFETY: We do not access the query stack reentrantly.
        let completion = unsafe {
            self.local_state
                .with_query_stack_unchecked_mut(move |stack| {
                    stack.pop_detached_completion(
                        self.database_key_index,
                        iteration,
                        detached_input_outputs,
                        force_extra,
                        #[cfg(debug_assertions)]
                        self.push_len,
                    )
                })
        };
        std::mem::forget(self);
        completion
    }
}

pub(crate) struct DetachedQuery<'me> {
    guard: ActiveQueryGuard<'me>,
    input_outputs: DetachedInputOutputs,
}

impl<'me> DetachedQuery<'me> {
    /// The task the query ran on, whose graph records its edges.
    pub(crate) fn zalsa_local(&self) -> &'me ZalsaLocal {
        self.guard.local_state
    }

    pub(crate) fn input_outputs(&self) -> &crate::hash::FxIndexSet<QueryEdge> {
        self.input_outputs.input_outputs()
    }

    pub(crate) fn pop_completion(
        self,
        iteration: IterationStamp,
        force_extra: bool,
    ) -> QueryCompletion {
        let Self {
            guard,
            input_outputs,
        } = self;
        guard.pop_detached_completion(iteration, input_outputs, force_extra)
    }
}

impl Drop for ActiveQueryGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: We do not access the query stack reentrantly.
        unsafe {
            self.local_state.with_query_stack_unchecked_mut(|stack| {
                stack.pop(
                    self.database_key_index,
                    #[cfg(debug_assertions)]
                    self.push_len,
                );
            })
        };
    }
}

#[cfg(feature = "persistence")]
pub(crate) mod persistence {
    #[cfg(test)]
    use super::QueryOriginRef;
    use thin_vec::ThinVec;

    #[cfg(feature = "accumulator")]
    use super::InputAccumulatedValues;
    use super::{OriginAndExtra, QueryEdge, QueryEdges, QueryRevisions, QueryRevisionsExtra};
    use crate::DatabaseKeyIndex;
    use crate::graph::{Graph, TaskGraph};
    use crate::{Durability, Revision};
    use std::sync::atomic::Ordering;

    impl OriginAndExtra {
        /// A persisted origin, its edges written into `graph`.
        pub(crate) fn recorded(
            origin: PersistentQueryOrigin,
            extra: QueryRevisionsExtra,
            graph: &Graph,
            task: &TaskGraph,
        ) -> Self {
            match origin {
                PersistentQueryOrigin::Assigned(key) => match extra.0 {
                    Some(extra) => Self::assigned_with_extra(key, extra),
                    None => Self::assigned(key),
                },
                PersistentQueryOrigin::Derived(edges) => {
                    Self::derived(graph.record(task, edges.iter().copied()), extra)
                }
                PersistentQueryOrigin::DerivedUntracked(edges) => {
                    Self::derived_untracked(graph.record(task, edges.iter().copied()), extra)
                }
            }
        }
    }

    impl QueryEdge {
        const fn raw_key(self) -> DatabaseKeyIndex {
            DatabaseKeyIndex::new(self.ingredient, self.id())
        }
    }

    impl serde::Serialize for QueryEdge {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            serde::Serialize::serialize(&self.raw_key(), serializer)
        }
    }

    impl<'de> serde::Deserialize<'de> for QueryEdge {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            let key: DatabaseKeyIndex = serde::Deserialize::deserialize(deserializer)?;
            let id = key.key_index();

            Ok(QueryEdge {
                index: id.index(),
                generation: id.generation(),
                ingredient: key.ingredient_index(),
            })
        }
    }

    impl serde::Serialize for QueryEdges<'_> {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            serializer.collect_seq(self.iter())
        }
    }

    /// Safe owning representation of a query origin used for persistence.
    #[derive(Debug, serde::Serialize, serde::Deserialize)]
    #[serde(rename = "QueryOrigin")]
    pub(crate) enum PersistentQueryOrigin {
        Assigned(DatabaseKeyIndex),
        Derived(Box<[QueryEdge]>),
        DerivedUntracked(Box<[QueryEdge]>),
    }

    impl PersistentQueryOrigin {
        pub(crate) fn derived<I>(input_outputs: I) -> Self
        where
            I: IntoIterator<Item = QueryEdge>,
            I::IntoIter: ExactSizeIterator,
        {
            Self::Derived(input_outputs.into_iter().collect())
        }

        pub(crate) fn derived_untracked<I>(input_outputs: I) -> Self
        where
            I: IntoIterator<Item = QueryEdge>,
            I::IntoIter: ExactSizeIterator,
        {
            Self::DerivedUntracked(input_outputs.into_iter().collect())
        }

        pub(crate) const fn assigned(key: DatabaseKeyIndex) -> Self {
            Self::Assigned(key)
        }

        #[cfg(test)]
        pub(crate) const fn as_ref(&self) -> QueryOriginRef<'_> {
            match self {
                Self::Assigned(key) => QueryOriginRef::Assigned(*key),
                Self::Derived(edges) => QueryOriginRef::Derived(QueryEdges::wide(edges)),
                Self::DerivedUntracked(edges) => {
                    QueryOriginRef::DerivedUntracked(QueryEdges::wide(edges))
                }
            }
        }
    }

    /// A serialization view that separates the packed origin from optional revision data.
    #[derive(serde::Serialize)]
    pub(crate) struct MappedQueryRevisions<'a> {
        changed_at: Revision,
        durability: Durability,
        #[serde(rename = "origin")]
        origin: PersistentQueryOrigin,
        verified_final: bool,
        extra: Option<&'a super::QueryRevisionsExtraInner>,
    }

    impl QueryRevisions {
        // Accumulated inputs are not serialized (accumulators aren't yet). The value fingerprint
        // is computed again from the deserialized value, and the serialized origin's edges are
        // not the recorded ones, so no fingerprint is persisted either.
        pub(crate) fn with_origin(
            &self,
            serialized_origin: PersistentQueryOrigin,
        ) -> MappedQueryRevisions<'_> {
            MappedQueryRevisions {
                changed_at: self.changed_at(),
                durability: self.durability(),
                extra: self.origin_and_extra.extra(),
                origin: serialized_origin,
                verified_final: self.verified_final(Ordering::Relaxed),
            }
        }
    }

    /// Deserialized revisions, whose edges are not yet in the database's graph: the deserializer
    /// has no database, so [`PersistedRevisions::record`] writes them once it does.
    #[derive(serde::Deserialize)]
    #[serde(rename = "QueryRevisions")]
    pub(crate) struct PersistedRevisions {
        changed_at: Revision,
        durability: Durability,
        #[serde(rename = "origin")]
        origin: PersistentQueryOrigin,
        verified_final: bool,
        extra: QueryRevisionsExtra,
    }

    impl PersistedRevisions {
        pub(crate) fn record(self, graph: &Graph, task: &TaskGraph) -> QueryRevisions {
            QueryRevisions::new(
                self.changed_at,
                self.durability,
                OriginAndExtra::recorded(self.origin, self.extra, graph, task),
                self.verified_final,
                #[cfg(feature = "accumulator")]
                InputAccumulatedValues::Empty,
                ThinVec::new(),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "persistence")]
    use super::QueryEdgeKind;
    #[cfg(feature = "persistence")]
    use super::persistence::PersistentQueryOrigin;
    #[cfg(not(feature = "shuttle"))]
    use super::{QueryRevisionsExtra, QueryRevisionsExtraInner, WithExtra};
    use std::sync::atomic::Ordering;

    use thin_vec::ThinVec;

    #[cfg(feature = "accumulator")]
    use super::InputAccumulatedValues;
    use super::{OriginAndExtra, QueryEdge, QueryOriginRef, QueryRevisions};
    use crate::durability::Durability;
    use crate::fingerprint::Fingerprint;
    use crate::graph::{EdgeSpan, Graph, TaskGraph};
    use crate::{DatabaseKeyIndex, Id, IngredientIndex, Revision};

    fn record(graph: &Graph, edges: &[QueryEdge]) -> EdgeSpan {
        graph.record(&TaskGraph::default(), edges.iter().copied())
    }

    #[test]
    fn derived_origin_reads_its_edges_from_the_graph() {
        let graph = Graph::default();
        let input = QueryEdge::input(key(231, 10_842_122, 41));
        let output = QueryEdge::output(key(232, 10_842_123, 1 << 25));
        let wide = QueryEdge::input(key(5000, 7, 3));
        let span = record(&graph, &[input, output, wide]);
        let origin = OriginAndExtra::derived(span, Default::default());
        let QueryOriginRef::Derived(edges) = origin.origin(&graph) else {
            panic!("expected derived origin");
        };

        assert_eq!(edges.len(), 3);
        assert_eq!(edges.iter().collect::<Vec<_>>(), vec![input, output, wide]);
        assert_eq!(
            edges.iter().rev().collect::<Vec<_>>(),
            vec![wide, output, input]
        );
        assert_eq!(
            origin.origin(&graph).inputs().collect::<Vec<_>>(),
            vec![input.key(), wide.key()]
        );
        assert_eq!(
            origin.origin(&graph).outputs().collect::<Vec<_>>(),
            vec![output.key()]
        );
        assert_eq!(
            edges.iter_outputs().rev().collect::<Vec<_>>(),
            vec![output]
        );
        assert_eq!(origin.allocation_size(), 3 * size_of::<u32>());
    }

    #[test]
    fn untracked_origin_keeps_its_kind() {
        let graph = Graph::default();
        let input = QueryEdge::input(key(1, 2, 0));
        let origin = OriginAndExtra::derived_untracked(record(&graph, &[input]), Default::default());
        assert!(origin.is_derived_untracked());
        let QueryOriginRef::DerivedUntracked(edges) = origin.origin(&graph) else {
            panic!("expected untracked derived origin");
        };
        assert_eq!(edges.iter().collect::<Vec<_>>(), vec![input]);
    }

    #[cfg(all(not(feature = "persistence"), not(feature = "shuttle")))]
    #[test]
    fn clearing_edges_preserves_extra() {
        let graph = Graph::default();
        let edge = QueryEdge::input(key(231, 10_842_122, 41));
        let mut extra = QueryRevisionsExtraInner::empty();
        extra.cycle_converged = true;
        let mut origin = OriginAndExtra::derived_untracked(
            record(&graph, &[edge]),
            QueryRevisionsExtra(Some(extra)),
        );

        origin.clear_edges();

        assert!(origin.extra().unwrap().cycle_converged);
        let QueryOriginRef::DerivedUntracked(edges) = origin.origin(&graph) else {
            panic!("expected untracked derived origin");
        };
        assert!(edges.iter().next().is_none());
        assert_eq!(origin.allocation_size(), size_of::<WithExtra<EdgeSpan>>());
    }

    #[cfg(not(feature = "shuttle"))]
    #[test]
    fn inserting_extra_keeps_the_edges() {
        let graph = Graph::default();
        let input = QueryEdge::input(key(231, 10_842_122, 41));
        let other_input = QueryEdge::input(key(232, 10_842_123, 42));
        let mut origin =
            OriginAndExtra::derived(record(&graph, &[input, other_input]), Default::default());

        origin.get_or_insert_extra().cycle_converged = true;

        assert!(origin.extra().unwrap().cycle_converged);
        assert_eq!(
            origin.origin(&graph).edges().iter().collect::<Vec<_>>(),
            vec![input, other_input]
        );
        assert_eq!(
            origin.allocation_size(),
            size_of::<WithExtra<EdgeSpan>>() + 2 * size_of::<u32>()
        );
    }

    #[cfg(not(feature = "shuttle"))]
    #[test]
    fn assigned_origin_with_extra_keeps_the_key() {
        let graph = Graph::default();
        let key = key(231, 10_842_122, 41);
        let mut origin = OriginAndExtra::assigned(key);
        origin.get_or_insert_extra().cycle_converged = true;

        assert_eq!(origin.allocation_size(), size_of::<WithExtra<DatabaseKeyIndex>>());
        assert!(origin.extra().unwrap().cycle_converged);
        let QueryOriginRef::Assigned(stored_key) = origin.origin(&graph) else {
            panic!("expected assigned origin");
        };
        assert_eq!(stored_key, key);
        assert_eq!(origin.span(), None);
    }

    #[test]
    fn inline_and_boxed_origins_read_the_same() {
        let span = EdgeSpan {
            start: 0xdead_beef,
            len: OriginAndExtra::INLINE_LEN,
        };
        let inline = OriginAndExtra::derived(span, Default::default());
        assert!(!inline.is_boxed());
        assert_eq!(inline.span(), Some(span));
        assert!(inline.extra().is_none());

        let long = EdgeSpan {
            start: 7,
            len: OriginAndExtra::INLINE_LEN + 1,
        };
        let boxed = OriginAndExtra::derived_untracked(long, Default::default());
        assert!(boxed.is_boxed() && boxed.is_derived_untracked());
        assert_eq!(boxed.span(), Some(long));
        assert!(boxed.extra().is_none());

        let mut relocated = OriginAndExtra::derived(span, Default::default());
        relocated.set_span(long);
        assert_eq!(relocated.span(), Some(long));
        relocated.set_span(EdgeSpan::EMPTY);
        assert_eq!(relocated.span(), Some(EdgeSpan::EMPTY));
        assert!(relocated.is_derived() && !relocated.is_derived_untracked());
    }

    #[test]
    fn fingerprints_box_the_origin_without_extra() {
        let span = EdgeSpan { start: 3, len: 2 };
        let mut revisions = QueryRevisions::new(
            Revision::from(5),
            Durability::HIGH,
            OriginAndExtra::derived(span, Default::default()),
            false,
            #[cfg(feature = "accumulator")]
            InputAccumulatedValues::Empty,
            ThinVec::new(),
        );
        assert!(!revisions.origin_and_extra.is_boxed());
        let fingerprint = Fingerprint::of_value(&1u32);
        revisions.set_value_fingerprint(fingerprint);
        assert_eq!(revisions.value_fingerprint(), fingerprint);
        assert!(revisions.origin_and_extra.is_boxed());
        assert!(revisions.origin_and_extra.extra().is_none());
        assert_eq!(revisions.origin_and_extra.span(), Some(span));
        revisions.set_value_fingerprint(Fingerprint::NONE);
        assert_eq!(revisions.value_fingerprint(), Fingerprint::NONE);
    }

    #[test]
    fn the_stamp_keeps_changed_at_durability_and_flags_apart() {
        let revisions = QueryRevisions::new(
            Revision::from((1 << 56) - 1),
            Durability::NEVER_CHANGE,
            OriginAndExtra::derived(EdgeSpan::EMPTY, Default::default()),
            true,
            #[cfg(feature = "accumulator")]
            InputAccumulatedValues::Any,
            ThinVec::new(),
        );
        assert_eq!(revisions.changed_at(), Revision::from((1 << 56) - 1));
        assert_eq!(revisions.durability(), Durability::NEVER_CHANGE);
        assert!(revisions.verified_final(Ordering::Relaxed));
        #[cfg(feature = "accumulator")]
        assert!(revisions.accumulated_inputs().is_any());

        let mut revisions = QueryRevisions::new(
            Revision::from(9),
            Durability::LOW,
            OriginAndExtra::derived(EdgeSpan::EMPTY, Default::default()),
            false,
            #[cfg(feature = "accumulator")]
            InputAccumulatedValues::Empty,
            ThinVec::new(),
        );
        assert!(!revisions.verified_final(Ordering::Relaxed));
        revisions.mark_verified_final(Ordering::Release);
        #[cfg(feature = "accumulator")]
        {
            assert!(revisions.accumulated_inputs().is_empty());
            revisions.store_accumulated_inputs(InputAccumulatedValues::Any);
            assert!(revisions.accumulated_inputs().is_any());
            revisions.store_accumulated_inputs(InputAccumulatedValues::Empty);
            assert!(revisions.accumulated_inputs().is_empty());
        }
        revisions.set_changed_at(Revision::from(12));
        revisions.set_durability(Durability::MEDIUM);
        assert_eq!(revisions.changed_at(), Revision::from(12));
        assert_eq!(revisions.durability(), Durability::MEDIUM);
        assert!(revisions.verified_final(Ordering::Relaxed));
        revisions.set_verified_final(false);
        assert!(!revisions.verified_final(Ordering::Relaxed));
        assert_eq!(revisions.changed_at(), Revision::from(12));
    }

    /// A model of memos over compactions: the origins keep reading the edges they were given.
    #[cfg(not(feature = "shuttle"))]
    #[test]
    fn relocated_origins_read_the_same_edges() {
        let mut graph = Graph::default();
        let task = TaskGraph::default();
        let mut memos: Vec<(Vec<QueryEdge>, OriginAndExtra)> = (0..300u32)
            .map(|i| {
                let edges: Vec<QueryEdge> = (0..i % 23)
                    .map(|j| {
                        let key = key(j % 4, i * 5 + j, j % 2);
                        if j % 7 == 3 {
                            QueryEdge::output(key)
                        } else {
                            QueryEdge::input(key)
                        }
                    })
                    .collect();
                let span = graph.record(&task, edges.iter().copied());
                let extra = if i % 5 == 0 {
                    QueryRevisionsExtra(Some(QueryRevisionsExtraInner::empty()))
                } else {
                    QueryRevisionsExtra::default()
                };
                (edges, OriginAndExtra::derived(span, extra))
            })
            .collect();
        memos.retain(|(edges, _)| edges.len() % 2 == 1);
        for _ in 0..2 {
            graph.compact(|relocation| {
                for (_, origin) in &mut memos {
                    origin.relocate(relocation);
                }
            });
            for (edges, origin) in &memos {
                assert_eq!(
                    &origin.origin(&graph).edges().iter().collect::<Vec<_>>(),
                    edges
                );
            }
        }
    }

    #[cfg(feature = "persistence")]
    #[test]
    fn query_origin_serde_round_trip_preserves_edges() {
        let input = QueryEdge::input(key(231, 10_842_122, 41));
        let output = QueryEdge::output(key(232, 10_842_123, 1 << 25));
        let origin = PersistentQueryOrigin::derived_untracked([input, output]);
        let serialized = serde_json::to_string(&origin).unwrap();
        let deserialized_origin: PersistentQueryOrigin = serde_json::from_str(&serialized).unwrap();
        let QueryOriginRef::DerivedUntracked(edges) = deserialized_origin.as_ref() else {
            panic!("expected untracked derived origin");
        };

        assert_eq!(edges.iter().collect::<Vec<_>>(), vec![input, output]);
        assert_eq!(
            edges.iter().map(QueryEdge::kind).collect::<Vec<_>>(),
            vec![QueryEdgeKind::Input, QueryEdgeKind::Output]
        );
    }

    fn key(ingredient: u32, index: u32, generation: u32) -> DatabaseKeyIndex {
        // SAFETY: Test inputs are valid `Id` indices.
        let id = unsafe { Id::from_index(index) }.with_generation(generation);

        DatabaseKeyIndex::new(IngredientIndex::new(ingredient), id)
    }
}

//! Input-closure filters: a memo green with no walk (docs/v2/plan/lanes/db/graph.md, "Bloom").
//!
//! Every node keeps a 62-bit Bloom filter of the input fields its memo read, directly or through
//! the memos it read, transitively: the OR of what each of its edges contributes, made when the
//! memo is published and again whenever a deep verify finds it unchanged. Every setter logs the
//! field it changed in a [`ChangeLog`] by durability. A memo whose filter shares no bit with the
//! fields changed since it was verified (of its durability or higher) read nothing that changed,
//! so it is unchanged and is marked verified without walking its edges, as the durability check
//! marks one. A shared bit proves nothing and the normal deep verify runs. Either way the answer
//! is the walk's: a filter is a proof, never a guess.
//!
//! What an edge contributes ([`ClosureClass`]):
//! - an input field: its own bit, which only a setter changes, and every setter logs it;
//! - a memo: its filter, valid while it is verified, since the memo's inputs are what can make it
//!   run again;
//! - a tracked struct's field: the filter of the query that created the struct, which alone
//!   writes its fields, if that query is verified in the revision the reader is (a struct read
//!   without its creator being verified first may be older than the creator's inputs);
//! - anything else (an interned value that may be reused, an accumulator, a provisional or
//!   cycle memo, an untracked read): every bit, so the memo is never green by its filter.
//!
//! Why the filter stays exact: when a memo `M` is verified (run or walked) in `V`, every memo it
//! read is verified in `V`, so its filter covers what can change them from `V` on. `M` is green
//! by filter in `V'` only if none of those changed in `(V, V']`, so none of them ran again and
//! their filters still hold; by induction over `M`'s verifications, its filter covers every
//! input that could change any of its reads. A memo read through a durability shortcut is the
//! same argument with salsa's durability invariant. The change log keeps the last
//! [`WINDOW`] revisions; a memo verified before them is walked.
//!
//! The column holds one word a node: 0 for nothing known (every bit), [`FILTER`] and the
//! filter, or [`CREATOR`] and the node of the query that created a tracked struct (on the
//! struct's node and its fields'). Writes are idempotent within a revision: every writer of a
//! node computes the same word from the same verified memos.

use crate::durability::Durability;
use crate::key::DatabaseKeyIndex;
use crate::sync::OnceLock;
use crate::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use crate::zalsa::{IngredientIndex, Zalsa};
use crate::{Id, Revision};

use super::{Graph, OUTPUT};

/// The bits of a filter: a column word keeps two for its tag.
const PAYLOAD: u64 = (1 << 62) - 1;
/// The word holds a filter in its low 62 bits.
const FILTER: u64 = 1 << 63;
/// The word holds, in its low 31 bits, the node of the query that created a tracked struct.
const CREATOR: u64 = 1 << 62;

/// Revisions the change log remembers; a memo verified longer ago is walked.
pub(crate) const WINDOW: usize = 64;

/// Node page `p` holds `1 << (FIRST_PAGE_BITS + p)` words, as `nodes.rs` pages its keys.
const FIRST_PAGE_BITS: u32 = 10;
const PAGES: usize = 22;

/// A Bloom filter of input fields, one bit each by the hash of its key.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) struct Filter(u64);

impl Filter {
    pub(crate) const EMPTY: Filter = Filter(0);
    /// Every bit: meets every non-empty filter.
    pub(crate) const FULL: Filter = Filter(PAYLOAD);

    /// The filter of one input field.
    #[inline]
    pub(crate) fn of_key(key: DatabaseKeyIndex) -> Filter {
        Filter(1 << bit_of(key.key_index().as_bits(), key.ingredient_index().as_u32()))
    }

    #[inline]
    pub(crate) fn union(self, other: Filter) -> Filter {
        Filter(self.0 | other.0)
    }

    /// Whether the two share a bit: something in one may be in the other.
    #[inline]
    pub(crate) fn meets(self, other: Filter) -> bool {
        self.0 & other.0 != 0
    }

    #[inline]
    pub(crate) fn is_full(self) -> bool {
        self.0 == PAYLOAD
    }

    #[cfg(test)]
    pub(crate) fn bits(self) -> u64 {
        self.0
    }
}

/// The bit of a key: splitmix64's finalizer (every bit of the key reaches it), scaled to 62.
#[inline]
fn bit_of(id: u64, ingredient: u32) -> u32 {
    let x = id ^ u64::from(ingredient).rotate_right(20) ^ 0x2545_f491_4f6c_dd1d;
    let x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    let x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    let x = x ^ (x >> 31);
    (((x >> 32) * 62) >> 32) as u32
}

/// What a read of an ingredient's key adds to the reader's filter.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ClosureClass {
    /// It may change other than by a logged input edit (an interned slot reused, an
    /// accumulator): every bit.
    Opaque,
    /// An input field: its own bit.
    Input,
    /// A tracked function's memo: its filter.
    Query,
    /// A field of the tracked structs of `struct_ingredient`: its creator's filter.
    TrackedField { struct_ingredient: IngredientIndex },
}

impl ClosureClass {
    const UNKNOWN: u32 = 0;
    const OPAQUE: u32 = 1;
    const INPUT: u32 = 2;
    const QUERY: u32 = 3;
    const FIELD: u32 = 4;

    fn encode(self) -> u32 {
        match self {
            ClosureClass::Opaque => Self::OPAQUE,
            ClosureClass::Input => Self::INPUT,
            ClosureClass::Query => Self::QUERY,
            ClosureClass::TrackedField { struct_ingredient } => {
                let index = struct_ingredient.as_u32();
                // Past 2^29 ingredients the class can't be kept; every bit is always right.
                if index >= 1 << 29 {
                    Self::OPAQUE
                } else {
                    Self::FIELD | (index << 3)
                }
            }
        }
    }

    fn decode(word: u32) -> ClosureClass {
        match word & 7 {
            Self::INPUT => ClosureClass::Input,
            Self::QUERY => ClosureClass::Query,
            Self::FIELD => ClosureClass::TrackedField {
                struct_ingredient: IngredientIndex::new(word >> 3),
            },
            _ => ClosureClass::Opaque,
        }
    }
}

/// What a node's column word says.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Entry {
    Unknown,
    Filter(Filter),
    Creator(u32),
}

impl Entry {
    #[inline]
    fn decode(word: u64) -> Entry {
        if word & FILTER != 0 {
            Entry::Filter(Filter(word & PAYLOAD))
        } else if word & CREATOR != 0 {
            Entry::Creator(word as u32 & !OUTPUT)
        } else {
            Entry::Unknown
        }
    }
}

/// A column word with the node it names (a creator) renumbered by `new`.
pub(crate) fn renumber_word(word: u64, new: impl Fn(u32) -> u32) -> u64 {
    match Entry::decode(word) {
        Entry::Creator(node) => CREATOR | u64::from(new(node)),
        Entry::Filter(_) | Entry::Unknown => word,
    }
}

/// The page of `index` and its offset there.
#[inline]
fn locate(index: u32) -> (usize, usize) {
    let x = index as usize + (1 << FIRST_PAGE_BITS);
    let bits = usize::BITS - 1 - x.leading_zeros();
    ((bits - FIRST_PAGE_BITS) as usize, x - (1 << bits))
}

/// The filter column by node, and the class of each ingredient, learned on first use.
pub(crate) struct Closures {
    words: Box<[OnceLock<super::Words<AtomicU64>>]>,
    classes: Box<[OnceLock<super::Words<AtomicU32>>]>,
}

impl Default for Closures {
    fn default() -> Self {
        Closures {
            // alloc: setup, once per database: the filter column's page directory
            words: (0..PAGES).map(|_| OnceLock::new()).collect(),
            // alloc: setup, once per database: the class table's page directory
            classes: (0..PAGES).map(|_| OnceLock::new()).collect(),
        }
    }
}

impl Closures {
    #[inline]
    fn word(&self, node: u32) -> Option<&AtomicU64> {
        let (page, offset) = locate(node);
        self.words.get(page)?.get().map(|page| &page[offset])
    }

    #[inline]
    fn entry(&self, node: u32) -> Entry {
        // Relaxed: a filter is stored before the memo it belongs to is published (`Release`),
        // and a reader reaches it through that memo or a verification in this revision; a word
        // read early is an older filter of the same node, which the reader's own check covers.
        self.word(node)
            .map_or(Entry::Unknown, |word| Entry::decode(word.load(Ordering::Relaxed)))
    }

    #[inline]
    fn store(&self, node: u32, word: u64) {
        let (page, offset) = locate(node);
        let slot = &self.words[page]
            .get_or_init(|| super::zeroed_u64s(1 << (FIRST_PAGE_BITS as usize + page)))[offset];
        // A word already right is not written: hot leaves are read by every task.
        if slot.load(Ordering::Relaxed) != word {
            slot.store(word, Ordering::Relaxed);
        }
    }

    fn class(&self, zalsa: &Zalsa, ingredient: IngredientIndex) -> ClosureClass {
        let (page, offset) = locate(ingredient.as_u32());
        let slot = &self.classes[page]
            .get_or_init(|| super::zeroed_u32s(1 << (FIRST_PAGE_BITS as usize + page)))[offset];
        let known = slot.load(Ordering::Relaxed);
        if known != ClosureClass::UNKNOWN {
            return ClosureClass::decode(known);
        }
        let class = zalsa.lookup_ingredient(ingredient).closure_class();
        slot.store(class.encode(), Ordering::Relaxed);
        ClosureClass::decode(class.encode())
    }

    /// The column word of `node`, 0 if none was stored: what the frozen graph keeps.
    #[inline]
    pub(crate) fn word_of(&self, node: u32) -> u64 {
        // Relaxed: read with `&mut Zalsa` (a compaction or an image write), or in a test.
        self.word(node).map_or(0, |word| word.load(Ordering::Relaxed))
    }

    /// Moves every node's word to its new number (`new_of_old`, `old_of_new` its inverse) and
    /// renames the creator a word names, as `Graph::compact_renumbered` renumbers the nodes.
    /// Written in new order into fresh pages, a page made only where a word is set.
    pub(crate) fn renumber(&mut self, new_of_old: &[u32], old_of_new: &[u32]) {
        let words: Box<[OnceLock<super::Words<AtomicU64>>]> =
            // alloc: once per renumbering: the page directory of the new column
            (0..PAGES).map(|_| OnceLock::new()).collect();
        for (new, &old) in old_of_new.iter().enumerate() {
            let word = self.word_of(old);
            if word == 0 {
                continue;
            }
            let (page, offset) = locate(new as u32);
            words[page]
                .get_or_init(|| super::zeroed_u64s(1 << (FIRST_PAGE_BITS as usize + page)))[offset]
                .store(renumber_word(word, |node| new_of_old[node as usize]), Ordering::Relaxed);
        }
        self.words = words;
    }
}

impl Graph {
    /// The filter of a memo with these edge words, verified (run or walked) in `now`: what each
    /// input edge contributes, ORed; outputs add nothing. Not `eligible` (provisional, a cycle's,
    /// untracked, assigned): every bit.
    pub(crate) fn closure_of(
        &self,
        zalsa: &Zalsa,
        eligible: bool,
        words: impl Iterator<Item = u32>,
        now: Revision,
    ) -> Filter {
        if !eligible {
            return Filter::FULL;
        }
        let mut filter = Filter::EMPTY;
        for word in words {
            if word & OUTPUT != 0 {
                continue;
            }
            filter = filter.union(self.contribution(zalsa, word, now));
            if filter.is_full() {
                break;
            }
        }
        filter
    }

    /// What a read of `node` adds to a filter made in `now`.
    #[inline]
    fn contribution(&self, zalsa: &Zalsa, node: u32, now: Revision) -> Filter {
        match self.closures.entry(node) {
            Entry::Filter(filter) => filter,
            Entry::Creator(creator) => self.creator_filter(creator, now),
            Entry::Unknown => self.classify(zalsa, node, now),
        }
    }

    /// The filter of the query `creator`, if it is verified in `now`; else every bit.
    #[inline]
    fn creator_filter(&self, creator: u32, now: Revision) -> Filter {
        if !self.color(creator).verified_in(now) {
            return Filter::FULL;
        }
        match self.closures.entry(creator) {
            Entry::Filter(filter) => filter,
            Entry::Creator(_) | Entry::Unknown => Filter::FULL,
        }
    }

    /// A node's first contribution: from its ingredient's class, remembered where it can't change.
    #[cold]
    #[inline(never)]
    fn classify(&self, zalsa: &Zalsa, node: u32, now: Revision) -> Filter {
        let key = self.nodes.key(node);
        match self.closures.class(zalsa, key.ingredient_index()) {
            ClosureClass::Input => {
                let filter = Filter::of_key(key);
                self.closures.store(node, FILTER | filter.0);
                filter
            }
            ClosureClass::Opaque => {
                self.closures.store(node, FILTER | Filter::FULL.0);
                Filter::FULL
            }
            // Published without a filter (removed, or not yet published): every bit, and nothing
            // kept, since its next memo stores its own.
            ClosureClass::Query => Filter::FULL,
            ClosureClass::TrackedField { struct_ingredient } => {
                let owner = DatabaseKeyIndex::new(struct_ingredient, key.key_index());
                let Some(Entry::Creator(creator)) =
                    self.find_node(owner).map(|owner| self.closures.entry(owner))
                else {
                    return Filter::FULL;
                };
                // A struct's slot, generation included, is made by one query for its life.
                self.closures.store(node, CREATOR | u64::from(creator));
                self.creator_filter(creator, now)
            }
        }
    }

    /// Stores a memo's filter at its key's `node`, before the memo is published or right after
    /// it is verified; and names `node` the creator of each tracked struct the memo made.
    pub(crate) fn publish_closure(
        &self,
        node: u32,
        filter: Filter,
        created: impl Iterator<Item = (IngredientIndex, Id)>,
    ) {
        self.closures.store(node, FILTER | filter.0);
        for (ingredient, id) in created {
            let owner = self.node(DatabaseKeyIndex::new(ingredient, id));
            self.closures.store(owner, CREATOR | u64::from(node));
        }
    }

    /// Whether the memo at `node`, verified in `verified_at` with `durability`, read no input
    /// that `changes` logged since: then it is unchanged, with no walk.
    #[inline]
    pub(crate) fn closure_unchanged(
        &self,
        node: u32,
        verified_at: Revision,
        durability: Durability,
        changes: &ChangeLog,
    ) -> bool {
        if !changes.enabled {
            return false;
        }
        match self.closures.entry(node) {
            Entry::Filter(filter) if !filter.is_full() => {
                !filter.meets(changes.since(verified_at, durability))
            }
            _ => false,
        }
    }
}

/// The input fields set in the last [`WINDOW`] revisions, by durability, as filters.
pub(crate) struct ChangeLog {
    /// `since[d][r % WINDOW]`: the fields of durability `d` or higher set in the finished
    /// revisions from `r` up to `now - 1`, for `r` in `(now - WINDOW, now]`.
    since: [[u64; WINDOW]; Durability::LEN],
    /// The fields of durability `d` or higher set in `now`.
    pending: [u64; Durability::LEN],
    now: u32,
    /// The first revision whose changes are all logged: what changed before it is unknown.
    start: u32,
    enabled: bool,
}

impl Default for ChangeLog {
    fn default() -> Self {
        ChangeLog::new(Revision::start())
    }
}

impl ChangeLog {
    /// A log that knows nothing up to and including `now`: writes made in `now` before the log
    /// existed (a database deserialized mid-revision) are not in it.
    pub(crate) fn new(now: Revision) -> ChangeLog {
        ChangeLog {
            since: [[0; WINDOW]; Durability::LEN],
            pending: [0; Durability::LEN],
            now: now.as_u32(),
            start: now.as_u32().saturating_add(1),
            enabled: true,
        }
    }

    /// Turns filters off (`false`: every memo is walked, as without them) or back on, for
    /// comparing the two.
    pub(crate) fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    /// The field `key`, of `durability` before the write, was set in the current revision.
    pub(crate) fn changed(&mut self, key: DatabaseKeyIndex, durability: Durability) {
        let filter = Filter::of_key(key).0;
        let levels = (durability.index() + 1).min(Durability::LEN);
        for pending in &mut self.pending[..levels] {
            *pending |= filter;
        }
    }

    /// Everything of `durability` or lower may have changed in the current revision, with no
    /// field named (`Database::synthetic_write`): every bit at those levels.
    pub(crate) fn changed_everything(&mut self, durability: Durability) {
        let levels = (durability.index() + 1).min(Durability::LEN);
        for pending in &mut self.pending[..levels] {
            *pending |= PAYLOAD;
        }
    }

    /// The revision is now `now`: the last one's changes are finished.
    pub(crate) fn advance(&mut self, now: Revision) {
        let now = now.as_u32();
        debug_assert!(now > self.now, "salsa: revisions only advance");
        let steps = (now - self.now) as usize;
        for (since, pending) in self.since.iter_mut().zip(&mut self.pending) {
            for slot in since.iter_mut() {
                *slot |= *pending;
            }
            *pending = 0;
            // Revisions `old now + 1 ..= now` start with nothing finished from them.
            for r in 1..=steps.min(WINDOW) {
                since[(self.now as usize + r) % WINDOW] = 0;
            }
        }
        self.now = now;
    }

    /// The fields of durability `durability` or higher set after `verified_at`, up to now; every
    /// bit when that is not known.
    #[inline]
    pub(crate) fn since(&self, verified_at: Revision, durability: Durability) -> Filter {
        let d = durability.index();
        if d >= Durability::LEN {
            return Filter::EMPTY;
        }
        if !self.enabled {
            return Filter::FULL;
        }
        let from = verified_at.as_u32().saturating_add(1);
        if from > self.now {
            return Filter::EMPTY;
        }
        if from < self.start || (self.now - from) as usize >= WINDOW {
            return Filter::FULL;
        }
        Filter(self.since[d][from as usize % WINDOW] | self.pending[d])
    }
}

/// A model of filters `W` words wide over a synthetic graph, for measuring 64 against 128 bits:
/// how many memos an edit of one input leaves green by filter, against how many it truly leaves
/// unchanged (no input in their closure changed).
pub mod model {
    /// Of `nodes` queries over `inputs` inputs (each query reads up to 4 inputs and up to 6
    /// earlier queries), for `edits` random single-input edits, the fraction of queries truly
    /// unaffected and the fraction a `W`-word filter proves unaffected. Panics if the filter ever
    /// proves a query unaffected that is not (it is exact by construction).
    pub fn green_rates<const W: usize>(seed: u64, inputs: u32, nodes: u32, edits: u32) -> (f64, f64) {
        let mut rng = blaze_ids::rng::SplitMix64::new(seed);
        let bit = |input: u32| -> (usize, u64) {
            let x = u64::from(input).wrapping_mul(0x9e37_79b9_7f4a_7c15);
            let x = (x ^ (x >> 29)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            let at = ((x >> 32) * (62 * W as u64)) >> 32;
            ((at / 62) as usize, 1 << (at % 62))
        };
        // alloc: the model's graph, test and bench only
        let mut filters: Vec<[u64; W]> = Vec::with_capacity(nodes as usize);
        // alloc: the model's graph, test and bench only
        let mut closures: Vec<Vec<u32>> = Vec::with_capacity(nodes as usize);
        for q in 0..nodes {
            let mut filter = [0u64; W];
            // alloc: the model's graph, test and bench only
            let mut closure = Vec::new();
            for _ in 0..rng.below(5) {
                let input = rng.below(u64::from(inputs)) as u32;
                let (w, b) = bit(input);
                filter[w] |= b;
                closure.push(input);
            }
            if q > 0 {
                for _ in 0..rng.below(7) {
                    // Mostly near (a module's queries), sometimes a hub.
                    let dep = if rng.one_in(4) {
                        rng.below(u64::from(q.min(64))) as u32
                    } else {
                        q - 1 - rng.below(u64::from(q.min(32))) as u32
                    };
                    for (f, d) in filter.iter_mut().zip(&filters[dep as usize]) {
                        *f |= d;
                    }
                    // clone: the model's graph, test and bench only
                    let dep_closure = closures[dep as usize].clone();
                    closure.extend(dep_closure);
                }
            }
            closure.sort_unstable();
            closure.dedup();
            filters.push(filter);
            closures.push(closure);
        }
        let (mut truly, mut proved) = (0u64, 0u64);
        for _ in 0..edits {
            let input = rng.below(u64::from(inputs)) as u32;
            let (w, b) = bit(input);
            for (filter, closure) in filters.iter().zip(&closures) {
                let unaffected = closure.binary_search(&input).is_err();
                let green = filter[w] & b == 0;
                assert!(!green || unaffected, "a filter proved a reader of the edited input green");
                truly += u64::from(unaffected);
                proved += u64::from(green);
            }
        }
        let total = (u64::from(nodes) * u64::from(edits)).max(1) as f64;
        (truly as f64 / total, proved as f64 / total)
    }
}

#[cfg(test)]
#[cfg(not(feature = "shuttle"))]
mod tests {
    use super::{ChangeLog, ClosureClass, Entry, Filter, WINDOW, model};
    use crate::durability::Durability;
    use crate::graph::Graph;
    use crate::key::DatabaseKeyIndex;
    use crate::zalsa::IngredientIndex;
    use crate::{Id, Revision};

    fn key(ingredient: u32, index: u32) -> DatabaseKeyIndex {
        // SAFETY: Test indices are below `Id::MAX_U32`.
        DatabaseKeyIndex::new(IngredientIndex::new(ingredient), unsafe { Id::from_index(index) })
    }

    fn r(n: usize) -> Revision {
        Revision::from(n)
    }

    #[test]
    fn a_key_has_one_bit_and_full_meets_everything() {
        for i in 0..10_000 {
            let filter = Filter::of_key(key(i % 7, i));
            assert_eq!(filter.bits().count_ones(), 1);
            assert!(filter.bits() < 1 << 62);
            assert!(Filter::FULL.meets(filter));
            assert!(!Filter::EMPTY.meets(filter));
        }
        assert_eq!(Filter::of_key(key(3, 9)), Filter::of_key(key(3, 9)));
    }

    #[test]
    fn classes_round_trip() {
        for class in [
            ClosureClass::Opaque,
            ClosureClass::Input,
            ClosureClass::Query,
            ClosureClass::TrackedField { struct_ingredient: IngredientIndex::new(12_345) },
        ] {
            assert_eq!(ClosureClass::decode(class.encode()), class);
        }
        assert_eq!(
            ClosureClass::TrackedField { struct_ingredient: IngredientIndex::new(1 << 29) }.encode(),
            ClosureClass::OPAQUE
        );
    }

    /// The safe reference: a list of (revision, durability, key) set, searched for each query.
    #[test]
    fn the_log_agrees_with_a_list_of_writes() {
        let mut log = ChangeLog::new(r(1));
        let mut writes: Vec<(u32, usize, DatabaseKeyIndex)> = Vec::new();
        let durabilities = [Durability::LOW, Durability::MEDIUM, Durability::HIGH];
        let mut rng = blaze_ids::rng::SplitMix64::new(11);
        let mut now = 1u32;
        for _ in 0..400 {
            if rng.one_in(3) {
                now += 1 + rng.below(3) as u32;
                log.advance(r(now as usize));
            }
            for _ in 0..rng.below(3) {
                let k = key(rng.below(5) as u32, rng.below(200) as u32);
                let d = durabilities[rng.below(3) as usize];
                log.changed(k, d);
                writes.push((now, d.index(), k));
            }
            let verified_at = now.saturating_sub(rng.below(80) as u32).max(1);
            for d in durabilities {
                let got = log.since(r(verified_at as usize), d);
                if now - verified_at > WINDOW as u32 {
                    assert_eq!(got, Filter::FULL);
                    continue;
                }
                let want = writes
                    .iter()
                    .filter(|&&(at, level, _)| at > verified_at && level >= d.index())
                    .fold(Filter::EMPTY, |f, &(_, _, k)| f.union(Filter::of_key(k)));
                assert_eq!(got, want, "since {verified_at} at {now}, {d:?}");
            }
            assert_eq!(log.since(r(now as usize), Durability::LOW), Filter::EMPTY);
            assert_eq!(log.since(r(1), Durability::NEVER_CHANGE), Filter::EMPTY);
        }
        log.set_enabled(false);
        assert!(now > 1);
        assert_eq!(log.since(r(now as usize - 1), Durability::HIGH), Filter::FULL);
    }

    #[test]
    fn before_the_log_began_is_unknown() {
        let mut log = ChangeLog::new(r(5));
        log.advance(r(6));
        assert_eq!(log.since(r(5), Durability::LOW), Filter::EMPTY);
        assert_eq!(log.since(r(4), Durability::LOW), Filter::FULL);
        // Made mid-revision (deserialized): writes already made in 5 are unknown.
        let fresh = ChangeLog::new(r(5));
        assert_eq!(fresh.since(r(4), Durability::LOW), Filter::FULL);
        assert_eq!(fresh.since(r(5), Durability::LOW), Filter::EMPTY);
    }

    #[test]
    fn a_synthetic_write_changes_everything_at_and_below_its_durability() {
        let mut log = ChangeLog::new(r(1));
        log.advance(r(2));
        log.changed_everything(Durability::MEDIUM);
        assert_eq!(log.since(r(1), Durability::LOW), Filter::FULL);
        assert_eq!(log.since(r(1), Durability::MEDIUM), Filter::FULL);
        assert_eq!(log.since(r(1), Durability::HIGH), Filter::EMPTY);
        log.advance(r(3));
        assert_eq!(log.since(r(1), Durability::MEDIUM), Filter::FULL);
        assert_eq!(log.since(r(2), Durability::MEDIUM), Filter::EMPTY);
    }

    #[test]
    fn creators_and_filters_live_in_the_column() {
        let graph = Graph::default();
        let creator = graph.node(key(1, 1));
        let filter = Filter::of_key(key(9, 3)).union(Filter::of_key(key(9, 4)));
        graph.publish_closure(creator, filter, [(IngredientIndex::new(5), key(5, 7).key_index())].into_iter());
        assert_eq!(graph.closures.entry(creator), Entry::Filter(filter));
        let owner = graph.find_node(key(5, 7)).expect("a created struct gets a node");
        assert_eq!(graph.closures.entry(owner), Entry::Creator(creator));
        assert_eq!(graph.closures.entry(graph.node(key(2, 2))), Entry::Unknown);

        let mut log = ChangeLog::new(r(1));
        log.advance(r(2));
        assert!(graph.closure_unchanged(creator, r(1), Durability::LOW, &log));
        log.changed(key(9, 4), Durability::LOW);
        assert!(!graph.closure_unchanged(creator, r(1), Durability::LOW, &log));
        // A memo of higher durability read no LOW field.
        assert!(graph.closure_unchanged(creator, r(1), Durability::HIGH, &log));
        // Unknown and full filters never prove anything.
        let unknown = graph.node(key(2, 2));
        log.advance(r(3));
        assert!(!graph.closure_unchanged(unknown, r(2), Durability::LOW, &log));
        graph.publish_closure(unknown, Filter::FULL, std::iter::empty());
        assert!(!graph.closure_unchanged(unknown, r(2), Durability::LOW, &log));
        // An empty closure (no input read at all) is always unchanged.
        graph.publish_closure(unknown, Filter::EMPTY, std::iter::empty());
        assert!(graph.closure_unchanged(unknown, r(1), Durability::LOW, &log));
        // A disabled log proves nothing, not even an empty closure.
        log.set_enabled(false);
        assert!(!graph.closure_unchanged(unknown, r(1), Durability::LOW, &log));
    }

    /// The safe reference is the column before: after a renumbering each node's entry is its
    /// old node's, a creator renamed, an unknown still unknown, and no page is made for nodes
    /// with only unknowns.
    #[test]
    fn renumbering_moves_filters_creators_and_unknowns() {
        use super::{CREATOR, Closures, FILTER};

        const NODES: u32 = 3500;
        let mut closures = Closures::default();
        let mut rng = blaze_ids::rng::SplitMix64::new(23);
        for node in 0..NODES {
            match rng.below(3) {
                0 => closures.store(node, FILTER | Filter::of_key(key(4, node)).bits()),
                1 => closures.store(node, CREATOR | rng.below(u64::from(NODES))),
                _ => {}
            }
        }
        let before: Vec<Entry> = (0..NODES).map(|node| closures.entry(node)).collect();
        let mut new_of_old: Vec<u32> = (0..NODES).collect();
        rng.shuffle(&mut new_of_old);
        let old_of_new = crate::graph::order::invert(&new_of_old);
        closures.renumber(&new_of_old, &old_of_new);
        for (old, entry) in before.iter().enumerate() {
            let want = match *entry {
                Entry::Creator(creator) => Entry::Creator(new_of_old[creator as usize]),
                other => other,
            };
            assert_eq!(closures.entry(new_of_old[old]), want, "node {old}");
            if *entry == Entry::Unknown {
                assert_eq!(closures.word_of(new_of_old[old]), 0);
            }
        }
        assert!(closures.words[3].get().is_none(), "no page past the nodes");

        // Only unknowns: no page at all.
        let mut empty = Closures::default();
        let identity: Vec<u32> = (0..NODES).collect();
        empty.renumber(&identity, &identity);
        assert!(empty.words.iter().all(|page| page.get().is_none()));
    }

    /// 64 against 128 bits on a synthetic graph: both exact, 128 proves at least as much.
    #[test]
    fn wider_filters_prove_more_and_never_wrong() {
        let (truly_64, proved_64) = model::green_rates::<1>(3, 2_000, 4_000, 40);
        let (truly_128, proved_128) = model::green_rates::<2>(3, 2_000, 4_000, 40);
        assert_eq!(truly_64, truly_128);
        assert!(proved_64 <= truly_64 && proved_128 <= truly_128);
        assert!(proved_128 >= proved_64 * 0.99, "64: {proved_64}, 128: {proved_128}");
        eprintln!(
            "unaffected {truly_64:.3}; proved green: 64-bit {proved_64:.3}, 128-bit {proved_128:.3}"
        );
    }
}

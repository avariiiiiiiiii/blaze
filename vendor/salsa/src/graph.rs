//! The dependency graph of a database: dense nodes for the keys edges name, and one array of
//! edges that every memo's edges are a range of (docs/v2/plan/lanes/db/speed-salsa.md, "S5").
//!
//! An edge is one `u32` word: its node in the low 31 bits and, in the top bit, whether it is an
//! output. A memo holds its edges as an [`EdgeSpan`] of the array: 8 bytes and 4 an edge, where a
//! memo held a 12-byte (or packed 8-byte) edge in an allocation of its own. A compaction settles
//! the live edges bit-packed, ⌈log2 nodes⌉ + 1 bits each ([`edges::Settled`]); readers see the
//! same words either way ([`EdgeWords`]).

pub mod bloom;
mod colors;
mod edges;
pub mod frozen;
mod nodes;
pub mod order;
pub mod varint;

use std::cell::Cell;

pub(crate) use bloom::{ChangeLog, ClosureClass, Filter};
pub(crate) use colors::Color;
pub(crate) use edges::{EdgeSpan, Edges};
use edges::{Packed, PackedIter, Settled, SettledWriter};
pub(crate) use nodes::NODE_LIMIT;
use nodes::{NodeCache, Nodes};

use crate::key::DatabaseKeyIndex;
use crate::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use crate::zalsa_local::{QueryEdge, QueryEdgeKind};

/// The top bit of an edge word: set for an output.
pub(crate) const OUTPUT: u32 = NODE_LIMIT;

/// Compact the edges at a new revision only once they are more than this many, settled and
/// fresh, and more than twice what was live at the last compaction.
const COMPACT_AT_LEAST: u32 = 1 << 20;

pub(crate) struct Graph {
    nodes: Nodes,
    /// The edges the last compaction settled: the positions below `settled.len()`.
    settled: Settled,
    /// The words recorded since, from position `settled.len()`.
    edges: Edges,
    /// The edges live after the last compaction.
    live: u32,
    /// Each node's input-closure filter or tracked-struct creator ([`bloom`]).
    closures: bloom::Closures,
    /// Whether a compaction renumbers the nodes in walk order ([`Graph::compact_renumbered`]):
    /// on, unless a bench or test compares the two.
    walk_order: bool,
    /// Whether every new revision compacts, however few edges are dead (tests and benches).
    compact_always: bool,
}

/// A task's part of the graph: its cache of key to node and the edge words it reserved.
#[derive(Default)]
pub(crate) struct TaskGraph {
    nodes: NodeCache,
    chunk: Cell<edges::Chunk>,
}

impl Default for Graph {
    fn default() -> Self {
        Graph {
            nodes: Nodes::new(),
            settled: Settled::empty(),
            edges: Edges::new(1, 0),
            live: 0,
            closures: bloom::Closures::default(),
            walk_order: true,
            compact_always: false,
        }
    }
}

impl Graph {
    /// Writes `edges` into the array and returns their range.
    pub(crate) fn record(
        &self,
        task: &TaskGraph,
        edges: impl ExactSizeIterator<Item = QueryEdge>,
    ) -> EdgeSpan {
        self.record_with_tail(task, edges, &[])
    }

    /// Writes `edges` and then the raw words `tail` into one range and returns it: a memo's edges
    /// (no tail), or a capture frame's reads, whose tail holds their fingerprints
    /// ([`crate::capture`]).
    pub(crate) fn record_with_tail(
        &self,
        task: &TaskGraph,
        edges: impl ExactSizeIterator<Item = QueryEdge>,
        tail: &[u32],
    ) -> EdgeSpan {
        let edge_count = edges.len();
        let len = u32::try_from(edge_count + tail.len())
            .ok()
            .filter(|&len| len < OUTPUT)
            .expect("salsa: a query recorded 2^31 edges or more");
        if len == 0 {
            return EdgeSpan::EMPTY;
        }
        let start = self.edges.reserve_for_task(&task.chunk, len);
        let span = EdgeSpan { start, len };
        let (edge_words, tail_words) = self.edges.slice(span).split_at(edge_count);
        let mut written = 0;
        // Relaxed: the memo (or the `Reads`) these words belong to is published with `Release`
        // after them.
        for (word, edge) in edge_words.iter().zip(edges) {
            word.store(self.encode(task, edge), Ordering::Relaxed);
            written += 1;
        }
        assert_eq!(
            written, edge_count,
            "salsa: an `ExactSizeIterator` of edges gave a wrong length"
        );
        for (word, &value) in tail_words.iter().zip(tail) {
            word.store(value, Ordering::Relaxed);
        }
        span
    }

    /// An edge's word: the node it carries (a read of a memo, which knows its key's node), else
    /// the task's cache or the shared table's.
    #[inline]
    fn encode(&self, task: &TaskGraph, edge: QueryEdge) -> u32 {
        let node = match edge.node() {
            Some(node) => {
                debug_assert_eq!(self.nodes.key(node), edge.key(), "an edge's node names its key");
                node
            }
            None => self.nodes.node_cached(&task.nodes, edge.key()),
        };
        match edge.kind() {
            QueryEdgeKind::Input => node,
            QueryEdgeKind::Output => node | OUTPUT,
        }
    }

    /// The node of `key`, given one if it has none: what a memo of `key` keeps
    /// ([`crate::function::memo::MemoHeader`]), so reads of it and its color need no lookup.
    #[inline]
    pub(crate) fn node(&self, key: DatabaseKeyIndex) -> u32 {
        self.nodes.node(key)
    }

    /// The node of `key` if some edge or memo gave it one, with no lock and no write.
    #[inline]
    pub(crate) fn find_node(&self, key: DatabaseKeyIndex) -> Option<u32> {
        self.nodes.find(key)
    }

    /// The edge an edge word holds, carrying its node.
    #[inline]
    pub(crate) fn decode(&self, word: u32) -> QueryEdge {
        let node = word & !OUTPUT;
        let key = self.nodes.key(node);
        if word & OUTPUT == 0 {
            QueryEdge::input_at(key, node)
        } else {
            QueryEdge::output_at(key, node)
        }
    }

    /// A memo's edge words, read in place: every reader of a memo's edges goes through this, so
    /// how the array stores them is the array's business.
    #[inline]
    pub(crate) fn edge_words(&self, span: EdgeSpan) -> EdgeWords<'_> {
        edge_words_in(&self.settled, &self.edges, span)
    }

    /// The raw words of `span`, a capture frame's edges and fingerprint tail
    /// ([`crate::capture`]): fresh words of the current revision, never compacted, since a
    /// compaction starts a revision and a frame is valid only in its own. A settled span panics.
    #[inline]
    pub(crate) fn raw_words(&self, span: EdgeSpan) -> &[AtomicU32] {
        self.edges.slice(span)
    }

    /// Whether the dead ranges are worth a compaction now.
    pub(crate) fn wants_compaction(&self) -> bool {
        self.compact_always
            || u64::from(self.settled.len()) + u64::from(self.edges.len())
                > u64::from(self.live.saturating_mul(2).max(COMPACT_AT_LEAST))
    }

    /// Whether a compaction renumbers the nodes in walk order.
    #[inline]
    pub(crate) fn walk_order(&self) -> bool {
        self.walk_order
    }

    /// Turns the walk-order renumbering at compaction off or back on (benches and tests).
    pub(crate) fn set_walk_order(&mut self, on: bool) {
        self.walk_order = on;
    }

    /// Makes every new revision compact, or only those whose dead edges are worth it.
    pub(crate) fn set_compact_always(&mut self, on: bool) {
        self.compact_always = on;
    }

    /// An empty gathering of the live memos' edges, for [`Ingredient::memo_edges`] to fill.
    ///
    /// [`Ingredient::memo_edges`]: crate::ingredient::Ingredient::memo_edges
    pub(crate) fn live_edges(&self) -> LiveEdges<'_> {
        LiveEdges {
            graph: self,
            // alloc: once per compaction or image write: a bit a node, which nodes hold a memo
            has_memo: vec![0; (self.nodes.len() as usize).div_ceil(64)],
            // alloc: once per compaction or image write: each memo's node and words
            memos: Vec::new(),
            // alloc: once per compaction or image write: the live edge words
            words: Vec::new(),
        }
    }

    /// Frees the node tables that shards outgrew, which no reader probes once we have `&mut`.
    pub(crate) fn release_outgrown_nodes(&mut self) {
        self.nodes.release_outgrown();
    }

    /// Settles the live ranges: `relocate_all` hands every live range to the [`Relocation`] and
    /// stores the range it returns in place of the old. The ranges are written bit-packed at the
    /// width the largest node needs, and the next words are recorded from past them.
    pub(crate) fn compact(&mut self, relocate_all: impl FnOnce(&mut Relocation<'_>)) {
        self.compact_with(None, relocate_all);
    }

    /// [`Graph::compact`] and, in the same pass, renumbers every node by `renumbering`: the key
    /// columns, the key table and the colors ([`Nodes::renumber`]), the filter column
    /// ([`bloom`]), and every edge word the [`Relocation`] copies. `relocate_all` must also give
    /// every memo's node to [`Relocation::node`]. A task's cached nodes are dropped on its next
    /// use. The node count doesn't change, so the settled width is the same.
    pub(crate) fn compact_renumbered(
        &mut self,
        renumbering: &Renumbering,
        relocate_all: impl FnOnce(&mut Relocation<'_>),
    ) {
        self.compact_with(Some(renumbering), relocate_all);
    }

    fn compact_with(
        &mut self,
        renumbering: Option<&Renumbering>,
        relocate_all: impl FnOnce(&mut Relocation<'_>),
    ) {
        let nodes = self.nodes.len();
        if let Some(renumbering) = renumbering {
            assert_eq!(
                renumbering.new_of_old.len(),
                nodes as usize,
                "salsa: a renumbering made for another node count"
            );
        }
        let width = edges::width_for(nodes);
        let new_of_old = renumbering.map(|renumbering| &renumbering.new_of_old[..]);
        let (settled, live) = settle(&self.settled, &self.edges, width, new_of_old, relocate_all);
        if let Some(renumbering) = renumbering {
            self.nodes
                .renumber(&renumbering.new_of_old, &renumbering.old_of_new);
            self.closures
                .renumber(&renumbering.new_of_old, &renumbering.old_of_new);
        }
        self.edges = Edges::new(self.edges.generation() + 1, settled.len());
        self.settled = settled;
        self.live = live;
    }

    /// How many keys have a node, and how many edges are stored, settled and fresh.
    #[cfg(test)]
    pub(crate) fn sizes(&self) -> (u32, u32) {
        (self.nodes.len(), self.settled.len() + self.edges.len())
    }
}

/// A span's words in whichever form they are stored.
#[inline]
fn edge_words_in<'a>(settled: &'a Settled, edges: &'a Edges, span: EdgeSpan) -> EdgeWords<'a> {
    EdgeWords {
        form: if settled.holds(span) {
            Form::Packed(settled.view(span))
        } else {
            Form::Words(edges.slice(span))
        },
    }
}

/// Copies every range `relocate_all` hands over out of `settled` and `edges` into a new settled
/// region of `width` bits an edge, each word's node renumbered by `new_of_old` when given,
/// returning the region and how many edges it holds.
fn settle(
    settled: &Settled,
    edges: &Edges,
    width: u32,
    new_of_old: Option<&[u32]>,
    relocate_all: impl FnOnce(&mut Relocation<'_>),
) -> (Settled, u32) {
    // The positions held, live or dead, bound the live edges unless two memos share a range;
    // the writer grows if they do.
    let bound = settled.len().saturating_add(edges.len());
    let mut relocation = Relocation {
        settled,
        edges,
        new_of_old,
        to: SettledWriter::new(width, bound),
    };
    relocate_all(&mut relocation);
    let live = relocation.to.len();
    (relocation.to.finish(), live)
}

/// Moves live edge ranges into the settled region a compaction writes, and renames nodes when
/// the compaction renumbers them ([`Graph::compact_renumbered`]).
pub(crate) struct Relocation<'a> {
    settled: &'a Settled,
    edges: &'a Edges,
    new_of_old: Option<&'a [u32]>,
    to: SettledWriter,
}

impl Relocation<'_> {
    pub(crate) fn relocate(&mut self, span: EdgeSpan) -> EdgeSpan {
        if span.len == 0 {
            return EdgeSpan::EMPTY;
        }
        self.to.reserve(span.len);
        let start = self.to.len();
        let words = edge_words_in(self.settled, self.edges, span).iter();
        match self.new_of_old {
            None => {
                for word in words {
                    self.to.push(word);
                }
            }
            Some(new_of_old) => {
                for word in words {
                    self.to
                        .push(new_of_old[(word & !OUTPUT) as usize] | (word & OUTPUT));
                }
            }
        }
        EdgeSpan {
            start,
            len: span.len,
        }
    }

    /// The number `node` has after this compaction: a memo's own node
    /// ([`crate::function::memo::MemoHeader`]). [`QueryEdge::NO_NODE`] stays.
    #[inline]
    pub(crate) fn node(&self, node: u32) -> u32 {
        match self.new_of_old {
            Some(new_of_old) if node != QueryEdge::NO_NODE => new_of_old[node as usize],
            _ => node,
        }
    }
}

/// A permutation of the nodes, made by [`LiveEdges::walk_order`] for
/// [`Graph::compact_renumbered`]: node `old` becomes `new_of_old[old]`.
pub(crate) struct Renumbering {
    new_of_old: Vec<u32>,
    old_of_new: Vec<u32>,
}

impl Renumbering {
    /// From `new_of_old`, which must be a permutation of `0..len`.
    pub(crate) fn new(new_of_old: Vec<u32>) -> Renumbering {
        debug_assert!(order::is_permutation(&new_of_old));
        let old_of_new = order::invert(&new_of_old);
        Renumbering {
            new_of_old,
            old_of_new,
        }
    }

    /// The new number of node `old`.
    #[cfg(test)]
    pub(crate) fn new_of(&self, old: u32) -> u32 {
        self.new_of_old[old as usize]
    }

    pub(crate) fn new_of_old(&self) -> &[u32] {
        &self.new_of_old
    }
}

/// Every live memo's node and edge words, gathered with `&mut` (at a compaction, or to write the
/// image) by each ingredient's [`Ingredient::memo_edges`], then grouped by node: what the walk
/// order is computed from ([`LiveEdges::walk_order`]), and the live graph as a
/// [`frozen::Source`] once [`LiveEdges::by_node`] has grouped it.
///
/// [`Ingredient::memo_edges`]: crate::ingredient::Ingredient::memo_edges
pub(crate) struct LiveEdges<'g> {
    graph: &'g Graph,
    /// A bit a node: set once a memo of it was pushed.
    has_memo: Vec<u64>,
    /// Each memo's node and the end of its words in `words`, in the order pushed.
    memos: Vec<(u32, u32)>,
    words: Vec<u32>,
}

impl<'g> LiveEdges<'g> {
    /// The graph the edges are of.
    #[inline]
    pub(crate) fn graph(&self) -> &'g Graph {
        self.graph
    }

    /// The memo of `node` has the edge words `words`. A node has at most one memo: a memo is of
    /// its key, and a key of one function's slot.
    pub(crate) fn push(&mut self, node: u32, words: EdgeWords<'_>) {
        assert!(
            node < self.graph.nodes.len(),
            "salsa: a memo names node {node}, past the graph's {}",
            self.graph.nodes.len()
        );
        let (word, bit) = ((node / 64) as usize, 1u64 << (node % 64));
        assert!(
            self.has_memo[word] & bit == 0,
            "salsa: two live memos name node {node} ({:?})",
            self.graph.nodes.key(node)
        );
        self.has_memo[word] |= bit;
        self.words.extend(words.iter());
        let end = u32::try_from(self.words.len())
            .expect("salsa: the live edges fit a u32, as the edge array does");
        self.memos.push((node, end));
    }

    /// How many memos were pushed.
    #[cfg(test)]
    pub(crate) fn memos(&self) -> usize {
        self.memos.len()
    }

    /// The order a deep verify walks the live graph in: `order::dfs_preorder` from the roots
    /// over the memos' input edges, in read order. The nodes the walk never loads a color of (no
    /// memo, and no input edge names them) come after, in their old order.
    pub(crate) fn walk_order(&self) -> Renumbering {
        let nodes = self.graph.nodes.len();
        let csr = self.input_csr(nodes);
        let preorder = order::dfs_preorder(&csr);
        // alloc: once per compaction: a bit a node, which nodes the walk loads
        let mut walked = vec![0u64; self.has_memo.len()];
        for (walked, &has_memo) in walked.iter_mut().zip(&self.has_memo) {
            *walked = has_memo;
        }
        for &target in &csr.targets {
            walked[(target / 64) as usize] |= 1u64 << (target % 64);
        }
        drop(csr);
        let is_walked = |node: u32| walked[(node / 64) as usize] >> (node % 64) & 1 == 1;
        // alloc: once per compaction: the renumbering's result
        let mut new_of_old = vec![0u32; nodes as usize];
        let mut next = 0u32;
        for &old in &order::invert(&preorder) {
            if is_walked(old) {
                new_of_old[old as usize] = next;
                next += 1;
            }
        }
        for old in 0..nodes {
            if !is_walked(old) {
                new_of_old[old as usize] = next;
                next += 1;
            }
        }
        debug_assert_eq!(next, nodes);
        Renumbering::new(new_of_old)
    }

    /// The memos' input edges as a CSR over all `nodes` nodes, a node with no memo having none.
    fn input_csr(&self, nodes: u32) -> order::Csr {
        // alloc: once per compaction: the CSR's offsets
        let mut offsets = vec![0u32; nodes as usize + 1];
        let mut start = 0u32;
        for &(node, end) in &self.memos {
            let inputs = self.words[start as usize..end as usize]
                .iter()
                .filter(|&&word| word & OUTPUT == 0)
                .count() as u32;
            offsets[node as usize + 1] = inputs;
            start = end;
        }
        for at in 0..nodes as usize {
            offsets[at + 1] += offsets[at];
        }
        // alloc: once per compaction: the CSR's targets
        let mut targets = vec![0u32; offsets[nodes as usize] as usize];
        let mut start = 0u32;
        for &(node, end) in &self.memos {
            let mut at = offsets[node as usize] as usize;
            for &word in &self.words[start as usize..end as usize] {
                if word & OUTPUT == 0 {
                    targets[at] = word;
                    at += 1;
                }
            }
            start = end;
        }
        order::Csr { offsets, targets }
    }

    /// The live graph grouped by node, as [`frozen::write`] reads it.
    pub(crate) fn by_node(&self) -> LiveGraph<'g> {
        let nodes = self.graph.nodes.len();
        // alloc: once per image write: each node's words' start
        let mut offsets = vec![0u32; nodes as usize + 1];
        let mut start = 0u32;
        for &(node, end) in &self.memos {
            offsets[node as usize + 1] = end - start;
            start = end;
        }
        for at in 0..nodes as usize {
            offsets[at + 1] += offsets[at];
        }
        // alloc: once per image write: the words by node
        let mut words = vec![0u32; self.words.len()];
        let mut start = 0u32;
        for &(node, end) in &self.memos {
            let at = offsets[node as usize] as usize;
            let len = (end - start) as usize;
            words[at..at + len].copy_from_slice(&self.words[start as usize..end as usize]);
            start = end;
        }
        LiveGraph {
            graph: self.graph,
            offsets,
            words,
        }
    }
}

/// The live graph by node ([`LiveEdges::by_node`]): node `v`'s memo's edge words are
/// `words[offsets[v]..offsets[v + 1]]`, none if it has no memo.
pub(crate) struct LiveGraph<'g> {
    graph: &'g Graph,
    offsets: Vec<u32>,
    words: Vec<u32>,
}

impl frozen::Source for LiveGraph<'_> {
    fn nodes(&self) -> u32 {
        self.graph.nodes.len()
    }

    fn key(&self, node: u32) -> (u64, u32) {
        let key = self.graph.nodes.key(node);
        (key.key_index().as_bits(), key.ingredient_index().as_u32())
    }

    fn filter_word(&self, node: u32) -> u64 {
        self.graph.closures.word_of(node)
    }

    fn edges(&self, node: u32, out: &mut Vec<u32>) {
        out.clear();
        let (start, end) = (self.offsets[node as usize], self.offsets[node as usize + 1]);
        out.extend_from_slice(&self.words[start as usize..end as usize]);
    }
}

/// A memo's edges as words (a node in the low 31 bits, [`OUTPUT`] on top), from
/// [`Graph::edge_words`], whether they are fresh words or settled bit-packed.
#[derive(Copy, Clone)]
pub(crate) struct EdgeWords<'a> {
    form: Form<'a>,
}

#[derive(Copy, Clone)]
enum Form<'a> {
    /// Recorded since the last compaction.
    Words(&'a [AtomicU32]),
    /// Settled by the last compaction.
    Packed(Packed<'a>),
}

impl<'a> EdgeWords<'a> {
    pub(crate) const EMPTY: EdgeWords<'static> = EdgeWords {
        form: Form::Words(&[]),
    };

    #[inline]
    pub(crate) fn len(self) -> usize {
        match self.form {
            Form::Words(words) => words.len(),
            Form::Packed(packed) => packed.len() as usize,
        }
    }

    #[inline]
    pub(crate) fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// The word at `position`, if the memo has that many edges.
    #[inline]
    pub(crate) fn get(self, position: usize) -> Option<u32> {
        match self.form {
            // Relaxed: the memo holding these edges was loaded with `Acquire`, and its edges
            // were written before it was published.
            Form::Words(words) => words.get(position).map(|word| word.load(Ordering::Relaxed)),
            // Plain loads: settled bytes were written under the compaction's `&mut Graph` and
            // never change while `&Graph` is shared.
            Form::Packed(packed) => {
                (position < packed.len() as usize).then(|| packed.word(position as u32))
            }
        }
    }

    #[inline]
    pub(crate) fn iter(self) -> EdgeWordIter<'a> {
        EdgeWordIter {
            form: match self.form {
                Form::Words(words) => IterForm::Words(words.iter()),
                Form::Packed(packed) => IterForm::Packed(packed.iter()),
            },
        }
    }
}

/// The words of an [`EdgeWords`], in order.
#[derive(Clone)]
pub(crate) struct EdgeWordIter<'a> {
    form: IterForm<'a>,
}

#[derive(Clone)]
enum IterForm<'a> {
    Words(std::slice::Iter<'a, AtomicU32>),
    Packed(PackedIter<'a>),
}

impl Iterator for EdgeWordIter<'_> {
    type Item = u32;

    #[inline]
    fn next(&mut self) -> Option<u32> {
        match &mut self.form {
            // Relaxed: as in `EdgeWords::get`.
            IterForm::Words(words) => words.next().map(|word| word.load(Ordering::Relaxed)),
            IterForm::Packed(packed) => packed.next(),
        }
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        match &self.form {
            IterForm::Words(words) => words.size_hint(),
            IterForm::Packed(packed) => packed.size_hint(),
        }
    }
}

impl DoubleEndedIterator for EdgeWordIter<'_> {
    #[inline]
    fn next_back(&mut self) -> Option<u32> {
        match &mut self.form {
            // Relaxed: as in `EdgeWords::get`.
            IterForm::Words(words) => words.next_back().map(|word| word.load(Ordering::Relaxed)),
            IterForm::Packed(packed) => packed.next_back(),
        }
    }
}

impl ExactSizeIterator for EdgeWordIter<'_> {}

impl std::iter::FusedIterator for EdgeWordIter<'_> {}

/// The words of a page of the node or edge columns.
#[cfg(not(feature = "shuttle"))]
pub(crate) type Words<T> = Zeroed<T>;
/// The words of a page of the node or edge columns; shuttle's atomics are objects of their own.
#[cfg(feature = "shuttle")]
pub(crate) type Words<T> = Box<[T]>;

/// `len` zeroed words for a page of the node or edge columns.
#[cfg(not(feature = "shuttle"))]
pub(crate) fn zeroed_u32s(len: usize) -> Words<AtomicU32> {
    // SAFETY: all-zero bits are an `AtomicU32` of 0, which needs no drop.
    unsafe { Zeroed::new(len) }
}

/// `len` zeroed words, as [`zeroed_u32s`].
#[cfg(not(feature = "shuttle"))]
fn zeroed_u64s(len: usize) -> Words<AtomicU64> {
    // SAFETY: all-zero bits are an `AtomicU64` of 0, which needs no drop.
    unsafe { Zeroed::new(len) }
}

/// `len` `T`s on a region of their own, tagged `Query`: committed whole and zeroed by the
/// kernel, so the part of a page never written is never touched and stays out of the resident
/// set.
#[cfg(not(feature = "shuttle"))]
pub(crate) struct Zeroed<T> {
    region: blaze_mem::Region,
    len: usize,
    _owns: std::marker::PhantomData<T>,
}

#[cfg(not(feature = "shuttle"))]
impl<T> Zeroed<T> {
    /// # Safety
    ///
    /// All-zero bits must be a valid `T`, and `T` must need no drop.
    unsafe fn new(len: usize) -> Zeroed<T> {
        debug_assert!(!std::mem::needs_drop::<T>());
        assert!(std::mem::align_of::<T>() <= blaze_mem::page_size());
        let bytes = std::mem::size_of::<T>()
            .checked_mul(len)
            .expect("salsa: a graph page fits in memory");
        let region = blaze_mem::Region::reserve_huge(bytes.max(1), blaze_mem::Tag::Query)
            .unwrap_or_else(|err| panic!("salsa: reserving a graph page: {err}"));
        region
            .ensure(bytes)
            .unwrap_or_else(|err| panic!("salsa: committing a graph page: {err}"));
        Zeroed {
            region,
            len,
            _owns: std::marker::PhantomData,
        }
    }
}

#[cfg(not(feature = "shuttle"))]
impl<T> std::ops::Deref for Zeroed<T> {
    type Target = [T];

    #[inline]
    fn deref(&self) -> &[T] {
        // SAFETY: `new` committed `len * size_of::<T>()` bytes from the region's base, which is
        // page-aligned (so aligned for `T`, checked in `new`); the kernel zeroed them, zero bits
        // are a valid `T` (`new`'s contract), they change only through `T`'s interior
        // mutability, and they live as long as `self.region`.
        unsafe { std::slice::from_raw_parts(self.region.base().cast::<T>().as_ptr(), self.len) }
    }
}

#[cfg(not(feature = "shuttle"))]
impl<T> std::ops::DerefMut for Zeroed<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut [T] {
        // SAFETY: as in `deref`; `&mut self` borrows the region, which `self` owns alone, so
        // no other reference to its bytes exists.
        unsafe {
            std::slice::from_raw_parts_mut(self.region.base().cast::<T>().as_ptr(), self.len)
        }
    }
}

/// `len` zeroed words; shuttle's atomics are not plain words, so they are made one by one.
#[cfg(feature = "shuttle")]
pub(crate) fn zeroed_u32s(len: usize) -> Words<AtomicU32> {
    // alloc: one page of the node or edge columns, made when the previous page fills
    (0..len).map(|_| AtomicU32::new(0)).collect()
}

/// `len` zeroed words, as [`zeroed_u32s`].
#[cfg(feature = "shuttle")]
fn zeroed_u64s(len: usize) -> Words<AtomicU64> {
    // alloc: one page of the node or edge columns, made when the previous page fills
    (0..len).map(|_| AtomicU64::new(0)).collect()
}

/// How many edges a database's graph holds settled, and how many fresh words were reserved
/// since the last compaction: `(settled, fresh)`. A compaction leaves `fresh` at 0.
#[doc(hidden)]
pub fn edges_at_rest(zalsa: &crate::zalsa::Zalsa) -> (u32, u32) {
    let graph = zalsa.graph();
    (graph.settled.len(), graph.edges.len())
}

/// What the graph benches reach (`benches/graph_*.rs`): not an API.
#[doc(hidden)]
pub mod bench {
    pub use super::bloom::model as bloom;
    pub use super::frozen::model as frozen;
    pub use super::order;
    pub use super::varint;
}

/// A benchmark's edge store (benches/edges_at_rest.rs): spans of random words naming `nodes`
/// nodes, recorded as fresh words and settled by the compaction's own code. It has no node
/// table; the width comes from the node count it is given.
#[doc(hidden)]
pub struct EdgesBench {
    settled: Settled,
    edges: Edges,
    spans: Vec<EdgeSpan>,
    nodes: u32,
}

impl EdgesBench {
    /// `edges` words in spans of 1 to `2 * mean - 1` (`mean` on average), each a node below
    /// `nodes`, an output one time in 16.
    pub fn new(nodes: u32, edges: u32, mean: u32) -> EdgesBench {
        assert!(nodes > 0 && nodes <= NODE_LIMIT && mean > 0);
        let store = Edges::new(1, 0);
        // alloc: bench setup
        let mut spans = Vec::with_capacity((edges / mean) as usize + 1);
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut left = edges;
        while left > 0 {
            let len = (1 + (next() % u64::from(2 * mean - 1)) as u32).min(left);
            let span = EdgeSpan {
                start: store.reserve(len),
                len,
            };
            for word in store.slice(span) {
                let random = next();
                let node = (random % u64::from(nodes)) as u32;
                let output = if random >> 60 == 0 { OUTPUT } else { 0 };
                word.store(node | output, Ordering::Relaxed);
            }
            spans.push(span);
            left -= len;
        }
        EdgesBench {
            settled: Settled::empty(),
            edges: store,
            spans,
            nodes,
        }
    }

    /// Settles every span, as a compaction does.
    pub fn compact(&mut self) {
        let width = edges::width_for(self.nodes);
        let spans = &mut self.spans;
        let (settled, _) = settle(&self.settled, &self.edges, width, None, |relocation| {
            for span in spans.iter_mut() {
                *span = relocation.relocate(*span);
            }
        });
        self.edges = Edges::new(self.edges.generation() + 1, settled.len());
        self.settled = settled;
    }

    pub fn edges(&self) -> u64 {
        self.spans.iter().map(|span| u64::from(span.len)).sum()
    }

    /// The bits a settled edge takes, 32 before a compaction.
    pub fn width(&self) -> u32 {
        if self.settled.len() == 0 {
            32
        } else {
            self.settled.width()
        }
    }

    /// The bytes an edge takes at rest: the settled bits and 4 for each fresh word reserved.
    pub fn bytes_per_edge(&self) -> f64 {
        let bytes = self.settled.packed_bytes() + 4 * u64::from(self.edges.len());
        bytes as f64 / self.edges() as f64
    }

    /// Sums every span's words through [`EdgeWords::iter`], in order.
    pub fn sum_iter(&self) -> u64 {
        let mut sum = 0u64;
        for &span in &self.spans {
            for word in edge_words_in(&self.settled, &self.edges, span).iter() {
                sum = sum.wrapping_add(u64::from(word));
            }
        }
        sum
    }

    /// Sums every span's words through [`EdgeWords::get`], position by position, as deep
    /// verify reads them.
    pub fn sum_get(&self) -> u64 {
        let mut sum = 0u64;
        for &span in &self.spans {
            let words = edge_words_in(&self.settled, &self.edges, span);
            for position in 0..words.len() {
                sum = sum.wrapping_add(u64::from(words.get(position).unwrap_or(0)));
            }
        }
        sum
    }
}

#[cfg(test)]
#[cfg(not(feature = "shuttle"))]
mod tests {
    use super::{EdgeSpan, EdgeWords, Form, Graph, OUTPUT, TaskGraph};
    use crate::Id;
    use crate::key::DatabaseKeyIndex;
    use crate::sync::atomic::Ordering;
    use crate::zalsa::IngredientIndex;
    use crate::zalsa_local::{QueryEdge, QueryEdgeKind};

    fn key(ingredient: u32, index: u32) -> DatabaseKeyIndex {
        // SAFETY: Test indices are below `Id::MAX_U32`.
        DatabaseKeyIndex::new(IngredientIndex::new(ingredient), unsafe {
            Id::from_index(index)
        })
    }

    fn read(graph: &Graph, span: EdgeSpan) -> Vec<QueryEdge> {
        graph
            .edge_words(span)
            .iter()
            .map(|word| graph.decode(word))
            .collect()
    }

    #[test]
    fn zeroed_pages_are_zero_and_writable() {
        let words = super::zeroed_u32s(5000);
        assert_eq!(words.len(), 5000);
        assert!(words.iter().all(|word| word.load(Ordering::Relaxed) == 0));
        words[4999].store(7, Ordering::Relaxed);
        assert_eq!(words[4999].load(Ordering::Relaxed), 7);
    }

    #[test]
    fn recorded_edges_read_back_in_order() {
        let graph = Graph::default();
        let task = TaskGraph::default();
        let edges = vec![
            QueryEdge::input(key(3, 9)),
            QueryEdge::output(key(4, 1)),
            QueryEdge::input(key(3, 10)),
            QueryEdge::input(key(3, 9)),
        ];
        let span = graph.record(&task, edges.iter().copied());
        assert_eq!(read(&graph, span), edges);
        assert_eq!(graph.record(&task, std::iter::empty()), EdgeSpan::EMPTY);
        assert_eq!(graph.sizes().0, 3);
    }

    /// A model of memos holding spans: after a compaction that keeps some, each kept memo reads
    /// the same edges, and only the kept words are live.
    #[test]
    fn compaction_keeps_live_edges() {
        let mut graph = Graph::default();
        let task = TaskGraph::default();
        let mut memos: Vec<(Vec<QueryEdge>, EdgeSpan)> = (0..500u32)
            .map(|i| {
                let edges: Vec<QueryEdge> = (0..i % 37)
                    .map(|j| QueryEdge::input(key(j % 5, i * 3 + j)))
                    .collect();
                let span = graph.record(&task, edges.iter().copied());
                (edges, span)
            })
            .collect();
        memos.retain(|(edges, _)| edges.len() % 3 != 0);
        let live: u32 = memos.iter().map(|(edges, _)| edges.len() as u32).sum();
        graph.compact(|relocation| {
            for (_, span) in &mut memos {
                *span = relocation.relocate(*span);
            }
        });
        assert_eq!(graph.live, live);
        // Every live edge is settled, packed with no gaps, and no fresh word is reserved.
        assert_eq!(graph.settled.len(), live);
        assert_eq!(graph.edges.len(), 0);
        for (edges, span) in &memos {
            assert!(span.len == 0 || span.start + span.len <= live);
            check(&graph, *span, edges);
        }
        // The task's chunk was of the old array, so the next record reserves from the new one,
        // whose words start past the settled edges.
        let fresh = [QueryEdge::output(key(1, 1))];
        let span = graph.record(&task, fresh.iter().copied());
        assert_eq!(span.start, live);
        check(&graph, span, &fresh);
    }

    /// Every way to read a span gives `edges`: `get` at each position and past the end, `iter`
    /// from the front, from the back, and from both ends at once.
    fn check(graph: &Graph, span: EdgeSpan, edges: &[QueryEdge]) {
        let expected: Vec<u32> = edges
            .iter()
            .map(|edge| {
                let node = graph
                    .find_node(edge.key())
                    .expect("a recorded edge's key has a node");
                match edge.kind() {
                    QueryEdgeKind::Input => node,
                    QueryEdgeKind::Output => node | OUTPUT,
                }
            })
            .collect();
        check_words(graph.edge_words(span), &expected);
    }

    fn check_words(words: EdgeWords<'_>, expected: &[u32]) {
        assert_eq!(words.len(), expected.len());
        assert_eq!(words.is_empty(), expected.is_empty());
        for (position, &word) in expected.iter().enumerate() {
            assert_eq!(words.get(position), Some(word), "position {position}");
        }
        assert_eq!(words.get(expected.len()), None);
        let iter = words.iter();
        assert_eq!(iter.len(), expected.len());
        assert_eq!(iter.collect::<Vec<_>>(), expected);
        let mut backward: Vec<u32> = words.iter().rev().collect();
        backward.reverse();
        assert_eq!(backward, expected);
        let mut both = words.iter();
        let (mut front, mut back) = (Vec::new(), Vec::new());
        let mut turn = 0u32;
        loop {
            turn += 1;
            let next = if turn % 3 == 0 { both.next_back() } else { both.next() };
            match next {
                Some(word) if turn % 3 == 0 => back.push(word),
                Some(word) => front.push(word),
                None => break,
            }
            assert_eq!(both.len(), expected.len() - front.len() - back.len());
        }
        back.reverse();
        front.extend(back);
        assert_eq!(front, expected);
    }

    /// A small PRNG (xorshift), so a failure replays.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    #[test]
    fn width_is_the_largest_node_and_the_output_bit() {
        for (nodes, width) in [
            (0, 1),
            (1, 1),
            (2, 2),
            (3, 3),
            (4, 3),
            (5, 4),
            (1 << 20, 21),
            ((1 << 20) + 1, 22),
            (10_000_000, 25),
            (1 << 30, 31),
            ((1 << 30) + 1, 32),
            (super::NODE_LIMIT, 32),
        ] {
            assert_eq!(super::edges::width_for(nodes), width, "{nodes} nodes");
        }
    }

    /// The safe reference is each span's list of words: settled at every width from 1 to 32,
    /// spans of every length class (empty, under a group, partial groups, past the SIMD
    /// threshold) read back the same in every way, with the writer growing past its first room.
    #[test]
    fn settled_spans_read_as_their_words() {
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        for width in 1..=32u32 {
            let node_bits = width - 1;
            let mut writer = super::SettledWriter::new(width, 16);
            let mut spans: Vec<(EdgeSpan, Vec<u32>)> = Vec::new();
            for _ in 0..120 {
                let len = match rng.below(8) {
                    0 => 0,
                    1 => 300 + rng.below(200) as u32,
                    2 | 3 => 32 + rng.below(64) as u32,
                    _ => 1 + rng.below(20) as u32,
                };
                let words: Vec<u32> = (0..len)
                    .map(|_| {
                        let random = rng.next();
                        let largest = (1u32 << node_bits) - 1;
                        // Half the edges name the largest node, so every bit is used.
                        let node = if random >> 63 == 0 {
                            (random as u32) & largest
                        } else {
                            largest
                        };
                        if (random >> 62) & 1 == 1 { node | super::OUTPUT } else { node }
                    })
                    .collect();
                writer.reserve(len);
                let start = writer.len();
                for &word in &words {
                    writer.push(word);
                }
                spans.push((EdgeSpan { start, len }, words));
            }
            let settled = writer.finish();
            assert_eq!(
                settled.len(),
                spans.iter().map(|(span, _)| span.len).sum::<u32>()
            );
            assert_eq!(settled.width(), width);
            for (span, words) in &spans {
                let view = if span.len == 0 && settled.len() == 0 {
                    EdgeWords::EMPTY
                } else {
                    EdgeWords {
                        form: Form::Packed(settled.view(*span)),
                    }
                };
                check_words(view, words);
            }
        }
    }

    #[test]
    #[should_panic(expected = "wider than the 3 bits")]
    fn a_node_past_the_width_is_refused() {
        let mut writer = super::SettledWriter::new(3, 4);
        writer.push(4);
    }

    /// Compactions of a real graph at node counts giving widths 1 to 18, each a width change
    /// from the last: every kept memo, settled at the old width or fresh, reads the same after,
    /// and the region is exactly the live edges at the width the node count needs.
    #[test]
    fn compaction_settles_at_the_width_of_the_node_count() {
        let mut graph = Graph::default();
        let task = TaskGraph::default();
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let mut memos: Vec<(Vec<QueryEdge>, EdgeSpan)> = Vec::new();
        let edge = |k: u32, output: bool| {
            let key = key(k % 3, k);
            if output { QueryEdge::output(key) } else { QueryEdge::input(key) }
        };
        for nodes in [1u32, 2, 3, 4, 200, 5000, 70_000] {
            // One memo names every key below `nodes`, so the graph has exactly that many nodes.
            let all: Vec<QueryEdge> = (0..nodes).map(|k| edge(k, k % 5 == 0)).collect();
            memos.push((all.clone(), graph.record(&task, all.iter().copied())));
            for _ in 0..200 {
                let len = match rng.below(6) {
                    0 => 0,
                    1 => 40 + rng.below(100) as u32,
                    _ => 1 + rng.below(12) as u32,
                };
                let edges: Vec<QueryEdge> = (0..len)
                    .map(|_| {
                        let random = rng.next();
                        edge((random % u64::from(nodes)) as u32, random >> 61 == 0)
                    })
                    .collect();
                memos.push((edges.clone(), graph.record(&task, edges.iter().copied())));
            }
            assert_eq!(graph.sizes().0, nodes);
            let mut kept = 0usize;
            memos.retain(|_| {
                kept += 1;
                kept % 4 != 0
            });
            graph.compact(|relocation| {
                for (_, span) in &mut memos {
                    *span = relocation.relocate(*span);
                }
            });
            let live: u32 = memos.iter().map(|(edges, _)| edges.len() as u32).sum();
            assert_eq!(graph.settled.len(), live);
            assert_eq!(graph.settled.width(), super::edges::width_for(nodes));
            assert_eq!(graph.edges.len(), 0);
            for (edges, span) in &memos {
                check(&graph, *span, edges);
            }
        }
        // A compaction at the same node count keeps the width and the words.
        let width = graph.settled.width();
        graph.compact(|relocation| {
            for (_, span) in &mut memos {
                *span = relocation.relocate(*span);
            }
        });
        assert_eq!(graph.settled.width(), width);
        for (edges, span) in &memos {
            check(&graph, *span, edges);
        }
    }

    /// The safe reference for a renumbering is the graph before it, read by key: after a
    /// compaction in walk order every key keeps a node naming it, with its color and its filter
    /// column word (creators renamed); every memo reads the same edges and names its own key's
    /// node; the walked nodes come first and the rest keep their order; the live graph freezes
    /// in the same order; and a task whose cache was filled before records no old node after.
    #[test]
    fn renumbered_compaction_keeps_keys_colors_and_edges() {
        use super::bloom::{self, Filter};
        use super::colors::Color;
        use super::frozen::{self, FrozenGraph};
        use super::order;
        use crate::Revision;

        const MEMOS: u32 = 3000;
        const KEYS: u32 = 5000;
        let mut graph = Graph::default();
        let (early, late) = (TaskGraph::default(), TaskGraph::default());
        let mut rng = Rng(0x5851_f42d_4c95_7f2d);
        // Memo `i` is of `key(i % 3, i)` and names keys of higher index only, so the memos form a
        // DAG; one edge in eight is an output, and keys from `MEMOS` on are inputs.
        let memo_key = |i: u32| key(i % 3, i);
        let mut memos: Vec<(DatabaseKeyIndex, Vec<QueryEdge>, EdgeSpan, u32)> = Vec::new();
        for i in 0..MEMOS {
            let len = rng.below(12) as u32;
            let edges: Vec<QueryEdge> = (0..len)
                .map(|_| {
                    let target = i + 1 + rng.below(u64::from(KEYS - i - 1)) as u32;
                    if rng.below(8) == 0 {
                        QueryEdge::output(memo_key(target))
                    } else {
                        QueryEdge::input(memo_key(target))
                    }
                })
                .collect();
            let task = if i % 2 == 0 { &early } else { &late };
            let span = graph.record(task, edges.iter().copied());
            let node = graph.node(memo_key(i));
            memos.push((memo_key(i), edges, span, node));
        }
        let now = Revision::from(4);
        for (i, (k, _, _, node)) in memos.iter().enumerate() {
            graph.paint(*node, Color::verified(now, Revision::from(1 + i % 3), i % 5 == 0));
            if i % 7 == 0 {
                let made = key(9, i as u32).key_index();
                graph.publish_closure(
                    *node,
                    Filter::of_key(*k),
                    [(IngredientIndex::new(9), made)].into_iter(),
                );
            }
        }
        let nodes = graph.nodes.len();
        let before: Vec<(DatabaseKeyIndex, Color, u64)> = (0..nodes)
            .map(|v| (graph.nodes.key(v), graph.color(v), graph.closures.word_of(v)))
            .collect();

        let renumbering = {
            let mut live = graph.live_edges();
            for (_, _, span, node) in &memos {
                live.push(*node, graph.edge_words(*span));
            }
            assert_eq!(live.memos(), memos.len());
            let renumbering = live.walk_order();
            assert!(order::is_permutation(renumbering.new_of_old()));

            let section = frozen::write(&live.by_node(), Some(renumbering.new_of_old()))
                .expect("the live graph freezes");
            let frozen = FrozenGraph::open(&section).expect("the frozen live graph opens");
            assert_eq!(frozen.nodes(), nodes);
            let mut out = Vec::new();
            for (old, (k, _, word)) in before.iter().enumerate() {
                let new = renumbering.new_of(old as u32);
                assert_eq!(
                    frozen.key(new),
                    (k.key_index().as_bits(), k.ingredient_index().as_u32())
                );
                assert_eq!(
                    frozen.filter_word(new),
                    bloom::renumber_word(*word, |n| renumbering.new_of(n))
                );
            }
            for (_, _, span, node) in &memos {
                frozen
                    .edges(renumbering.new_of(*node), &mut out)
                    .expect("a frozen node's edges read");
                let want: Vec<u32> = graph
                    .edge_words(*span)
                    .iter()
                    .map(|word| renumbering.new_of(word & !OUTPUT) | (word & OUTPUT))
                    .collect();
                assert_eq!(out, want);
            }
            renumbering
        };

        // The nodes the walk loads (memos and the inputs they read) come first, the rest after in
        // their old order.
        let mut walked = vec![false; nodes as usize];
        for (_, edges, _, node) in &memos {
            walked[*node as usize] = true;
            for edge in edges {
                if matches!(edge.kind(), QueryEdgeKind::Input) {
                    walked[graph.find_node(edge.key()).unwrap() as usize] = true;
                }
            }
        }
        let first_unwalked = walked.iter().filter(|&&walked| walked).count() as u32;
        let mut last = None;
        for old in 0..nodes {
            let new = renumbering.new_of(old);
            if walked[old as usize] {
                assert!(new < first_unwalked, "walked node {old} numbered {new}");
            } else {
                assert!(new >= first_unwalked && last < Some(new), "unwalked node {old}");
                last = Some(new);
            }
        }

        graph.compact_renumbered(&renumbering, |relocation| {
            for (_, _, span, node) in &mut memos {
                *span = relocation.relocate(*span);
                *node = relocation.node(*node);
            }
            assert_eq!(relocation.node(QueryEdge::NO_NODE), QueryEdge::NO_NODE);
        });
        assert_eq!(graph.nodes.len(), nodes);
        assert_eq!(graph.edges.len(), 0);
        for (old, (k, color, word)) in before.iter().enumerate() {
            let new = renumbering.new_of(old as u32);
            assert_eq!(graph.find_node(*k), Some(new), "{k:?}");
            assert_eq!(graph.nodes.key(new), *k);
            assert_eq!(graph.color(new), *color, "{k:?}");
            assert_eq!(
                graph.closures.word_of(new),
                bloom::renumber_word(*word, |n| renumbering.new_of(n))
            );
        }
        for (k, edges, span, node) in &memos {
            assert_eq!(graph.find_node(*k), Some(*node));
            check(&graph, *span, edges);
        }
        // Both tasks cached old numbers of these keys; what they record now names the keys.
        for (i, (_, edges, _, _)) in memos.iter().enumerate().take(400) {
            let task = if i % 2 == 0 { &late } else { &early };
            let span = graph.record(task, edges.iter().copied());
            check(&graph, span, edges);
        }
        // A plain compaction after keeps them too.
        graph.compact(|relocation| {
            for (_, _, span, _) in &mut memos {
                *span = relocation.relocate(*span);
            }
        });
        for (k, edges, span, node) in &memos {
            assert_eq!(graph.find_node(*k), Some(*node));
            check(&graph, *span, edges);
        }
    }
}

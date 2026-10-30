//! The dependency graph of a database: dense nodes for the keys edges name, and one array of
//! edges that every memo's edges are a range of (docs/v2/plan/lanes/db/speed-salsa.md, "S5").
//!
//! An edge is one `u32` word: its node in the low 31 bits and, in the top bit, whether it is an
//! output. A memo holds its edges as an [`EdgeSpan`] of the array: 8 bytes and 4 an edge, where a
//! memo held a 12-byte (or packed 8-byte) edge in an allocation of its own.

mod edges;
mod nodes;

use std::cell::Cell;

pub(crate) use edges::{EdgeSpan, Edges};
pub(crate) use nodes::NODE_LIMIT;
use nodes::{NodeCache, Nodes};

use crate::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use crate::zalsa_local::{QueryEdge, QueryEdgeKind};

/// The top bit of an edge word: set for an output.
pub(crate) const OUTPUT: u32 = NODE_LIMIT;

/// Compact the edge array at a new revision only once it holds more than this many words, and
/// more than twice what was live at the last compaction.
const COMPACT_AT_LEAST: u32 = 1 << 20;

pub(crate) struct Graph {
    nodes: Nodes,
    edges: Edges,
    /// The words live after the last compaction.
    live: u32,
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
            edges: Edges::new(1),
            live: 0,
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
        let len = u32::try_from(edges.len())
            .ok()
            .filter(|&len| len < OUTPUT)
            .expect("salsa: a query recorded 2^31 edges or more");
        if len == 0 {
            return EdgeSpan::EMPTY;
        }
        let start = self.edges.reserve_for_task(&task.chunk, len);
        let span = EdgeSpan { start, len };
        let words = self.edges.slice(span);
        let mut written = 0;
        for (word, edge) in words.iter().zip(edges) {
            // Relaxed: the memo these edges belong to is published with `Release` after them.
            word.store(self.encode(task, edge), Ordering::Relaxed);
            written += 1;
        }
        assert_eq!(written, len, "salsa: an `ExactSizeIterator` of edges gave a wrong length");
        span
    }

    #[inline]
    fn encode(&self, task: &TaskGraph, edge: QueryEdge) -> u32 {
        let node = self.nodes.node_cached(&task.nodes, edge.key());
        match edge.kind() {
            QueryEdgeKind::Input => node,
            QueryEdgeKind::Output => node | OUTPUT,
        }
    }

    /// The edge an edge word holds.
    #[inline]
    pub(crate) fn decode(&self, word: u32) -> QueryEdge {
        let key = self.nodes.key(word & !OUTPUT);
        if word & OUTPUT == 0 {
            QueryEdge::input(key)
        } else {
            QueryEdge::output(key)
        }
    }

    /// The words of `span`.
    #[inline]
    pub(crate) fn words(&self, span: EdgeSpan) -> &[AtomicU32] {
        self.edges.slice(span)
    }

    /// Whether the dead ranges are worth a compaction now.
    pub(crate) fn wants_compaction(&self) -> bool {
        self.edges.len() > self.live.saturating_mul(2).max(COMPACT_AT_LEAST)
    }

    /// Frees the node tables that shards outgrew, which no reader probes once we have `&mut`.
    pub(crate) fn release_outgrown_nodes(&mut self) {
        self.nodes.release_outgrown();
    }

    /// Copies the live ranges into a new array: `relocate_all` hands every live range to the
    /// [`Relocation`] and stores the range it returns in place of the old.
    pub(crate) fn compact(&mut self, relocate_all: impl FnOnce(&mut Relocation<'_>)) {
        let to = Edges::new(self.edges.generation() + 1);
        let mut relocation = Relocation {
            from: &self.edges,
            to: &to,
            live: 0,
        };
        relocate_all(&mut relocation);
        self.live = relocation.live;
        self.edges = to;
    }

    /// How many keys have a node, and how many edge words are reserved.
    #[cfg(test)]
    pub(crate) fn sizes(&self) -> (u32, u32) {
        (self.nodes.len(), self.edges.len())
    }
}

/// Moves live edge ranges into the array a compaction makes.
pub(crate) struct Relocation<'a> {
    from: &'a Edges,
    to: &'a Edges,
    live: u32,
}

impl Relocation<'_> {
    pub(crate) fn relocate(&mut self, span: EdgeSpan) -> EdgeSpan {
        if span.len == 0 {
            return EdgeSpan::EMPTY;
        }
        let start = self.to.reserve(span.len);
        let moved = EdgeSpan {
            start,
            len: span.len,
        };
        for (to, from) in self.to.slice(moved).iter().zip(self.from.slice(span)) {
            to.store(from.load(Ordering::Relaxed), Ordering::Relaxed);
        }
        self.live += span.len;
        moved
    }
}

/// `len` zeroed words for a page of the node or edge columns, from a zeroed allocation: the part
/// of a page that is never written is never touched, so it stays out of the resident set.
#[cfg(not(feature = "shuttle"))]
fn zeroed_u32s(len: usize) -> Box<[AtomicU32]> {
    // SAFETY: all-zero bits are an `AtomicU32` of 0.
    unsafe { zeroed(len) }
}

/// `len` zeroed words, as [`zeroed_u32s`].
#[cfg(not(feature = "shuttle"))]
fn zeroed_u64s(len: usize) -> Box<[AtomicU64]> {
    // SAFETY: all-zero bits are an `AtomicU64` of 0.
    unsafe { zeroed(len) }
}

/// A boxed slice of `len` `T`s from `alloc_zeroed`.
///
/// # Safety
///
/// All-zero bits must be a valid `T`.
#[cfg(not(feature = "shuttle"))]
unsafe fn zeroed<T>(len: usize) -> Box<[T]> {
    let layout = std::alloc::Layout::array::<T>(len).expect("salsa: a graph page fits in memory");
    if layout.size() == 0 {
        return Vec::new().into_boxed_slice();
    }
    // SAFETY: `layout` has a nonzero size.
    let words = unsafe { std::alloc::alloc_zeroed(layout) }.cast::<T>();
    if words.is_null() {
        std::alloc::handle_alloc_error(layout);
    }
    // SAFETY: `words` is a live allocation of `layout`, `len` `T`s with `T`'s alignment, made by
    // the global allocator the box frees it with; its zeroed bytes are valid `T`s (the caller's
    // contract), and nothing else owns it.
    unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(words, len)) }
}

/// `len` zeroed words; shuttle's atomics are not plain words, so they are made one by one.
#[cfg(feature = "shuttle")]
fn zeroed_u32s(len: usize) -> Box<[AtomicU32]> {
    (0..len).map(|_| AtomicU32::new(0)).collect()
}

/// `len` zeroed words, as [`zeroed_u32s`].
#[cfg(feature = "shuttle")]
fn zeroed_u64s(len: usize) -> Box<[AtomicU64]> {
    (0..len).map(|_| AtomicU64::new(0)).collect()
}

#[cfg(all(test, not(feature = "shuttle")))]
mod tests {
    use super::{EdgeSpan, Graph, TaskGraph};
    use crate::Id;
    use crate::key::DatabaseKeyIndex;
    use crate::sync::atomic::Ordering;
    use crate::zalsa::IngredientIndex;
    use crate::zalsa_local::QueryEdge;

    fn key(ingredient: u32, index: u32) -> DatabaseKeyIndex {
        // SAFETY: Test indices are below `Id::MAX_U32`.
        DatabaseKeyIndex::new(IngredientIndex::new(ingredient), unsafe {
            Id::from_index(index)
        })
    }

    fn read(graph: &Graph, span: EdgeSpan) -> Vec<QueryEdge> {
        graph
            .words(span)
            .iter()
            .map(|word| graph.decode(word.load(Ordering::Relaxed)))
            .collect()
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
        // Only a range that would cross a page skips the page's tail.
        assert!(graph.edges.len() - live < 2 * 37);
        for (edges, span) in &memos {
            assert_eq!(&read(&graph, *span), edges);
        }
        // The task's chunk was of the old array, so the next record reserves from the new one.
        let span = graph.record(&task, [QueryEdge::input(key(1, 1))].into_iter());
        assert!(span.start >= live);
    }
}

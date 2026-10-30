//! The graph at rest: every node's key, filter and edges in one section of the daemon image,
//! read in place (docs/v2/plan/lanes/db/graph.md, "Packed, mapped graph at rest").
//!
//! A node's edges stay in read order (deep verify must check them in the order the query read
//! them: a later read may only make sense once an earlier one is unchanged), so they are not
//! sorted; each is a zigzag delta from the edge before it (the first from the node itself), and
//! with nodes numbered in depth-first preorder (`order::dfs_preorder`) a query's first reads
//! follow it, so the deltas are small, as a sorted list's would be. The deltas are StreamVByte
//! (`varint`) after a LEB128 count: about 1.3 to 1.6 bytes an edge.
//! Offsets are two-level: a `u64` per block of [`BLOCK`] nodes and a `u16` per node inside it, or
//! a `u32` per node for a block whose lists pass 64 KiB; about 2.1 bytes a node.
//!
//! The section, all little-endian, every column at a multiple of its width:
//!
//! | part | bytes |
//! |---|---|
//! | header: magic `u64`, nodes `u32`, wide blocks `u32`, stream bytes `u64` | 24 |
//! | key ids `[u64; nodes]` | 8 a node |
//! | filter words `[u64; nodes]` (`bloom`'s column words, creators renumbered) | 8 a node |
//! | blocks `[u64; ceil(nodes / BLOCK)]`: stream offset (40 bits), wide index (23), wide (1) | 8 a block |
//! | key ingredients `[u32; nodes]` | 4 a node |
//! | wide offsets `[u32; BLOCK * wide blocks]` | |
//! | narrow offsets `[u16; nodes]` (0 in a wide block) | 2 a node |
//! | stream: per node, `varint::encode_edges` of its words from the node | 1.3 to 1.6 an edge |
//!
//! Nothing is decoded when the section is opened: [`FrozenGraph::open`] checks the header and
//! the lengths, the image has checked every group's CRC32C (the whole section, at open), and a
//! read checks its own bounds. A section that is not a graph of this layout, or whose bytes don't
//! hash to what the image's spine names, is an error naming why; nothing is rebuilt in its place.

#![cfg_attr(
    not(test),
    allow(dead_code, reason = "the restore path that reads it in place is not wired yet (graph.md, section 3)")
)]

use blaze_format::{
    SchemaHash,
    cursor::{self, Reader},
};
use blaze_image::SectionId;

use super::{OUTPUT, varint};

/// Where the image keeps the graph.
pub const SECTION: SectionId = SectionId::SALSA_GRAPH;

/// The section's layout: a change is a refused image.
pub const LAYOUT: SchemaHash = blaze_format::schema(
    "blaze v2 salsa graph 1: header { magic u64, nodes u32, wide u32, stream u64 }; \
     ids [u64; nodes]; filters [u64; nodes]; blocks [u64; ceil(nodes / 64)] \
     (offset 40 bits, wide index 23 bits, wide flag 1 bit); ingredients [u32; nodes]; \
     wide offsets [u32; 64 * wide]; narrow offsets [u16; nodes]; stream (per node: LEB128 count, \
     StreamVByte of zigzag31 deltas from the node, shifted left by one, output flag in bit 0)",
);

const MAGIC: u64 = u64::from_le_bytes(*b"blzgrph1");
const HEADER: usize = 24;
/// Nodes a block of offsets covers.
pub(crate) const BLOCK: usize = 64;
const WIDE: u64 = 1 << 63;
const OFFSET_BITS: u32 = 40;
const WIDE_INDEX_BITS: u32 = 23;

/// Why a section is not a graph this blaze reads.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FrozenError {
    /// The section's bytes are not a graph of this layout: what is wrong.
    Invalid(&'static str),
    /// Node `node`'s edges run past the stream or name a node the graph doesn't have.
    Edges { node: u32 },
}

impl std::fmt::Display for FrozenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrozenError::Invalid(what) => write!(f, "the image's salsa graph is invalid: {what}"),
            FrozenError::Edges { node } => {
                write!(f, "the image's salsa graph is invalid: the edges of node {node}")
            }
        }
    }
}

impl std::error::Error for FrozenError {}

/// What the graph to freeze holds, node by node.
pub(crate) trait Source {
    fn nodes(&self) -> u32;
    /// The key of `node`: its id's bits and its ingredient.
    fn key(&self, node: u32) -> (u64, u32);
    /// `bloom`'s column word of `node`.
    fn filter_word(&self, node: u32) -> u64;
    /// Fills `out` (cleared first) with the edge words of `node`'s memo in read order: none if
    /// it has no memo.
    fn edges(&self, node: u32, out: &mut Vec<u32>);
}

/// Writes `source` as the section, nodes renumbered by `new_of_old` (a permutation of
/// `0..nodes`, or `None` to keep them). Returns the bytes for `Builder::add(SECTION, LAYOUT, ..)`.
pub(crate) fn write(source: &impl Source, new_of_old: Option<&[u32]>) -> Result<Vec<u8>, FrozenError> {
    let n = source.nodes();
    let nodes = n as usize;
    if let Some(p) = new_of_old {
        assert_eq!(p.len(), nodes, "salsa: a graph renumbered by a permutation of another size");
    }
    let new = |old: u32| new_of_old.map_or(old, |p| p[old as usize]);
    // alloc: the edge: once per image written
    let mut old_of_new = vec![0u32; nodes];
    for old in 0..n {
        old_of_new[new(old) as usize] = old;
    }

    // The stream and each node's start in it, in the new order.
    // alloc: the edge: once per image written
    let mut stream = Vec::new();
    // alloc: the edge: once per image written
    let mut starts = Vec::with_capacity(nodes);
    // alloc: the edge: once per image written, reused per node
    let (mut words, mut renumbered, mut scratch) = (Vec::new(), Vec::new(), Vec::new());
    for at in 0..n {
        let old = old_of_new[at as usize];
        source.edges(old, &mut words);
        renumbered.clear();
        renumbered.extend(words.iter().map(|&word| new(word & !OUTPUT) | (word & OUTPUT)));
        starts.push(stream.len() as u64);
        varint::encode_edges(&renumbered, at, &mut scratch, &mut stream);
    }
    if stream.len() as u64 >= 1 << OFFSET_BITS {
        return Err(FrozenError::Invalid("its edges take 2^40 bytes or more"));
    }

    // Offsets: a block is wide when a node in it starts 64 KiB or more past the block's first.
    let blocks = nodes.div_ceil(BLOCK);
    // alloc: the edge: once per image written
    let mut block_words = Vec::with_capacity(blocks);
    // alloc: the edge: once per image written
    let mut narrow = vec![0u16; nodes];
    // alloc: the edge: once per image written
    let mut wide = Vec::new();
    for b in 0..blocks {
        let range = b * BLOCK..((b + 1) * BLOCK).min(nodes);
        let base = starts[range.start];
        let last = starts[range.end - 1];
        if last - base < 1 << 16 {
            for i in range {
                narrow[i] = (starts[i] - base) as u16;
            }
            block_words.push(base);
        } else {
            let index = (wide.len() / BLOCK) as u64;
            if index >= 1 << WIDE_INDEX_BITS {
                return Err(FrozenError::Invalid("more than 2^23 blocks of long edge lists"));
            }
            for i in 0..BLOCK {
                let start = starts.get(range.start + i).copied().unwrap_or(last);
                wide.push(u32::try_from(start - base).map_err(|_| {
                    FrozenError::Invalid("a block of 64 nodes with 4 GiB of edges")
                })?);
            }
            block_words.push(base | (index << OFFSET_BITS) | WIDE);
        }
    }

    let len = HEADER
        + nodes * 8 * 2
        + blocks * 8
        + nodes * 4
        + wide.len() * 4
        + nodes * 2
        + stream.len();
    // alloc: the edge: once per image written
    let mut out = Vec::with_capacity(len);
    out.extend_from_slice(&MAGIC.to_le_bytes());
    out.extend_from_slice(&n.to_le_bytes());
    out.extend_from_slice(&((wide.len() / BLOCK) as u32).to_le_bytes());
    out.extend_from_slice(&(stream.len() as u64).to_le_bytes());
    for &old in &old_of_new {
        out.extend_from_slice(&source.key(old).0.to_le_bytes());
    }
    for &old in &old_of_new {
        let word = super::bloom::renumber_word(source.filter_word(old), &new);
        out.extend_from_slice(&word.to_le_bytes());
    }
    for word in &block_words {
        out.extend_from_slice(&word.to_le_bytes());
    }
    for &old in &old_of_new {
        out.extend_from_slice(&source.key(old).1.to_le_bytes());
    }
    for offset in &wide {
        out.extend_from_slice(&offset.to_le_bytes());
    }
    for offset in &narrow {
        out.extend_from_slice(&offset.to_le_bytes());
    }
    out.extend_from_slice(&stream);
    debug_assert_eq!(out.len(), len);
    Ok(out)
}

/// A frozen graph read in place from the bytes of its section.
#[derive(Copy, Clone)]
pub(crate) struct FrozenGraph<'a> {
    nodes: u32,
    ids: &'a [u8],
    filters: &'a [u8],
    blocks: &'a [u8],
    ingredients: &'a [u8],
    wide: &'a [u8],
    narrow: &'a [u8],
    stream: &'a [u8],
}

#[inline]
fn u64_at(bytes: &[u8], index: usize) -> u64 {
    let at = index * 8;
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("an 8-byte slice"))
}

#[inline]
fn u32_at(bytes: &[u8], index: usize) -> u32 {
    let at = index * 4;
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("a 4-byte slice"))
}

#[inline]
fn u16_at(bytes: &[u8], index: usize) -> u16 {
    let at = index * 2;
    u16::from_le_bytes(bytes[at..at + 2].try_into().expect("a 2-byte slice"))
}

impl<'a> FrozenGraph<'a> {
    /// The graph in `bytes`, the whole section, which the image has checked against its CRCs.
    /// Checks the header and that the columns fill the bytes exactly; decodes nothing.
    pub(crate) fn open(bytes: &'a [u8]) -> Result<FrozenGraph<'a>, FrozenError> {
        let invalid = FrozenError::Invalid;
        let mut read = Reader::new(bytes);
        let header = |_: cursor::Error| invalid("shorter than its header");
        if read.left() < HEADER {
            return Err(invalid("shorter than its header"));
        }
        if read.u64().map_err(header)? != MAGIC {
            return Err(invalid("not a graph section (its magic)"));
        }
        let n = read.u32().map_err(header)?;
        let wide_blocks = read.u32().map_err(header)? as usize;
        let stream_len = read.u64().map_err(header)?;
        if n >= OUTPUT {
            return Err(invalid("2^31 nodes or more"));
        }
        let nodes = n as usize;
        let blocks = nodes.div_ceil(BLOCK);
        let mut take = |len: usize| read.take(len).map_err(|_| invalid("shorter than its columns"));
        let graph = FrozenGraph {
            nodes: n,
            ids: take(nodes * 8)?,
            filters: take(nodes * 8)?,
            blocks: take(blocks * 8)?,
            ingredients: take(nodes * 4)?,
            wide: take(wide_blocks * BLOCK * 4)?,
            narrow: take(nodes * 2)?,
            stream: take(usize::try_from(stream_len).map_err(|_| invalid("a stream past memory"))?)?,
        };
        read.finish().map_err(|_| invalid("longer than its columns"))?;
        for b in 0..blocks {
            let word = u64_at(graph.blocks, b);
            if word & ((1 << OFFSET_BITS) - 1) > stream_len {
                return Err(invalid("a block starts past the stream"));
            }
            if word & WIDE != 0 && ((word & !WIDE) >> OFFSET_BITS) as usize >= wide_blocks {
                return Err(invalid("a wide block past the wide offsets"));
            }
        }
        Ok(graph)
    }

    pub(crate) fn nodes(&self) -> u32 {
        self.nodes
    }

    /// The key of `node`: its id's bits and its ingredient.
    #[inline]
    pub(crate) fn key(&self, node: u32) -> (u64, u32) {
        (u64_at(self.ids, node as usize), u32_at(self.ingredients, node as usize))
    }

    /// `bloom`'s column word of `node`.
    #[inline]
    pub(crate) fn filter_word(&self, node: u32) -> u64 {
        u64_at(self.filters, node as usize)
    }

    /// Where `node`'s list starts in the stream.
    #[inline]
    fn start(&self, node: u32) -> usize {
        let i = node as usize;
        let word = u64_at(self.blocks, i / BLOCK);
        let base = (word & ((1 << OFFSET_BITS) - 1)) as usize;
        if word & WIDE == 0 {
            base + usize::from(u16_at(self.narrow, i))
        } else {
            let index = ((word & !WIDE) >> OFFSET_BITS) as usize;
            base + u32_at(self.wide, index * BLOCK + i % BLOCK) as usize
        }
    }

    /// Fills `out` (cleared first) with `node`'s edge words, in read order. No allocation once
    /// `out` is as long as the longest list.
    #[inline]
    pub(crate) fn edges(&self, node: u32, out: &mut Vec<u32>) -> Result<(), FrozenError> {
        assert!(node < self.nodes, "salsa: node {node} of a frozen graph of {}", self.nodes);
        let error = FrozenError::Edges { node };
        let bytes = self.stream.get(self.start(node)..).ok_or(error)?;
        varint::decode_edges(bytes, node, out).map_err(|_| error)?;
        if out.iter().any(|&word| word & !OUTPUT >= self.nodes) {
            return Err(error);
        }
        Ok(())
    }

    /// The bytes of the stream, for the bench's bytes-an-edge.
    pub(crate) fn stream_len(&self) -> usize {
        self.stream.len()
    }
}

/// A frozen graph with the revision's changes over it: the nodes whose memo was published or
/// replaced since it was frozen take their edges from the live graph, the rest from the frozen
/// one, so a restart maps the image and runs without decoding it into the live array.
pub(crate) struct Overlay<'a> {
    frozen: FrozenGraph<'a>,
    /// One bit a frozen node: set once its memo was replaced live.
    replaced: Vec<u64>,
}

impl<'a> Overlay<'a> {
    pub(crate) fn new(frozen: FrozenGraph<'a>) -> Overlay<'a> {
        Overlay {
            // alloc: the edge: once per restore, a bit a node
            replaced: vec![0; (frozen.nodes() as usize).div_ceil(64)],
            frozen,
        }
    }

    pub(crate) fn frozen(&self) -> &FrozenGraph<'a> {
        &self.frozen
    }

    /// `node`'s memo was published live: its frozen edges are no longer its own.
    pub(crate) fn replace(&mut self, node: u32) {
        if node < self.frozen.nodes() {
            self.replaced[node as usize / 64] |= 1 << (node % 64);
        }
    }

    /// Whether `node`'s edges are the frozen graph's.
    #[inline]
    pub(crate) fn is_frozen(&self, node: u32) -> bool {
        node < self.frozen.nodes() && self.replaced[node as usize / 64] & (1 << (node % 64)) == 0
    }

    /// `node`'s frozen edges, if they are still its own; `None` sends the reader to the live
    /// graph.
    #[inline]
    pub(crate) fn edges(&self, node: u32, out: &mut Vec<u32>) -> Option<Result<(), FrozenError>> {
        self.is_frozen(node).then(|| self.frozen.edges(node, out))
    }
}

/// What `benches/graph_frozen.rs` measures, on an `order::Csr`: not an API.
#[doc(hidden)]
pub mod model {
    use super::{FrozenGraph, Source};
    use crate::graph::order::Csr;

    struct Plain<'c>(&'c Csr);

    impl Source for Plain<'_> {
        fn nodes(&self) -> u32 {
            self.0.nodes()
        }

        fn key(&self, node: u32) -> (u64, u32) {
            (u64::from(node), 0)
        }

        fn filter_word(&self, _: u32) -> u64 {
            0
        }

        fn edges(&self, node: u32, out: &mut Vec<u32>) {
            out.clear();
            out.extend_from_slice(self.0.edges(node));
        }
    }

    /// The section for `csr`, renumbered by `new_of_old` when given.
    pub fn write(csr: &Csr, new_of_old: Option<&[u32]>) -> Vec<u8> {
        super::write(&Plain(csr), new_of_old)
            .unwrap_or_else(|error| panic!("salsa bench: freezing the graph: {error}"))
    }

    /// The stream's bytes an edge, and the whole section's.
    pub fn bytes_per_edge(section: &[u8], edges: usize) -> (f64, f64) {
        let frozen = open(section);
        let edges = edges.max(1) as f64;
        (frozen.stream_len() as f64 / edges, section.len() as f64 / edges)
    }

    /// Reads every node's list in node order, as a verify of everything would, into `out`;
    /// returns the sum of the words so nothing is optimized away.
    pub fn read_all(section: &[u8], out: &mut Vec<u32>) -> u64 {
        let frozen = open(section);
        let mut sum = 0u64;
        for node in 0..frozen.nodes() {
            frozen
                .edges(node, out)
                .unwrap_or_else(|error| panic!("salsa bench: {error}"));
            sum += out.iter().map(|&word| u64::from(word)).sum::<u64>();
        }
        sum
    }

    /// Opening the section: the header and length checks, nothing decoded.
    pub fn open_nodes(section: &[u8]) -> u32 {
        open(section).nodes()
    }

    fn open(section: &[u8]) -> FrozenGraph<'_> {
        FrozenGraph::open(section).unwrap_or_else(|error| panic!("salsa bench: {error}"))
    }
}

#[cfg(test)]
#[cfg(not(feature = "shuttle"))]
mod tests {
    use super::{BLOCK, FrozenError, FrozenGraph, Overlay, Source, write};
    use crate::graph::OUTPUT;
    use crate::graph::order::{Csr, dfs_preorder, invert, synthetic};

    /// The safe reference: plain vectors.
    struct Lists {
        keys: Vec<(u64, u32)>,
        filters: Vec<u64>,
        edges: Vec<Vec<u32>>,
    }

    impl Source for Lists {
        fn nodes(&self) -> u32 {
            self.edges.len() as u32
        }
        fn key(&self, node: u32) -> (u64, u32) {
            self.keys[node as usize]
        }
        fn filter_word(&self, node: u32) -> u64 {
            self.filters[node as usize]
        }
        fn edges(&self, node: u32, out: &mut Vec<u32>) {
            out.clear();
            out.extend_from_slice(&self.edges[node as usize]);
        }
    }

    fn lists(csr: &Csr, seed: u64) -> Lists {
        let mut rng = blaze_ids::rng::SplitMix64::new(seed);
        let n = csr.nodes();
        Lists {
            keys: (0..n).map(|v| (u64::from(v) * 3 + 1, v % 17)).collect(),
            filters: (0..n).map(|_| (1 << 63) | (rng.next_u64() >> 2)).collect(),
            edges: (0..n)
                .map(|v| {
                    csr.edges(v)
                        .iter()
                        .map(|&t| if rng.one_in(50) { t | OUTPUT } else { t })
                        .collect()
                })
                .collect(),
        }
    }

    fn check_same(source: &Lists, frozen: &FrozenGraph<'_>, new_of_old: Option<&[u32]>) {
        let new = |v: u32| new_of_old.map_or(v, |p| p[v as usize]);
        let mut out = Vec::new();
        for old in 0..source.nodes() {
            let at = new(old);
            assert_eq!(frozen.key(at), source.key(old));
            assert_eq!(frozen.filter_word(at), source.filter_word(old));
            frozen.edges(at, &mut out).expect("a written graph reads back");
            let want: Vec<u32> = source.edges[old as usize]
                .iter()
                .map(|&w| new(w & !OUTPUT) | (w & OUTPUT))
                .collect();
            assert_eq!(out, want, "node {old}");
        }
    }

    #[test]
    fn a_graph_reads_back_as_written_and_renumbered() {
        let csr = synthetic(5, 20_000, 10);
        let source = lists(&csr, 1);
        let bytes = write(&source, None).unwrap();
        let frozen = FrozenGraph::open(&bytes).unwrap();
        check_same(&source, &frozen, None);
        let plain_bytes = frozen.stream_len();

        let p = dfs_preorder(&csr);
        let bytes = write(&source, Some(&p)).unwrap();
        let frozen = FrozenGraph::open(&bytes).unwrap();
        check_same(&source, &frozen, Some(&p));
        let edges = csr.targets.len().max(1) as f64;
        eprintln!(
            "bytes an edge: as numbered {:.2}, preorder {:.2}",
            plain_bytes as f64 / edges,
            frozen.stream_len() as f64 / edges
        );
        assert!(frozen.stream_len() <= plain_bytes, "preorder deltas are no larger");
        assert_eq!(invert(&invert(&p)), p);
    }

    /// A node with a list long enough that its block's offsets don't fit 16 bits.
    #[test]
    fn wide_blocks_and_empty_graphs() {
        let mut edges = vec![Vec::new(); 3 * BLOCK + 5];
        edges[BLOCK + 3] = (0..40_000u32).map(|i| (i * 7919) % (3 * BLOCK as u32 + 5)).collect();
        edges[BLOCK + 9] = vec![1, 2, 3];
        let n = edges.len() as u32;
        let source = Lists {
            keys: (0..n).map(|v| (u64::from(v), 1)).collect(),
            filters: vec![0; n as usize],
            edges,
        };
        let bytes = write(&source, None).unwrap();
        let frozen = FrozenGraph::open(&bytes).unwrap();
        check_same(&source, &frozen, None);

        let empty = Lists { keys: vec![], filters: vec![], edges: vec![] };
        let bytes = write(&empty, None).unwrap();
        assert_eq!(FrozenGraph::open(&bytes).unwrap().nodes(), 0);
    }

    #[test]
    fn bad_bytes_are_errors_never_panics() {
        let csr = synthetic(9, 2_000, 6);
        let source = lists(&csr, 2);
        let bytes = write(&source, None).unwrap();
        for cut in [0, 7, 23, 24, 100, bytes.len() - 1] {
            assert!(FrozenGraph::open(&bytes[..cut]).is_err(), "cut at {cut}");
        }
        let mut longer = bytes.clone();
        longer.push(0);
        assert_eq!(
            FrozenGraph::open(&longer).err(),
            Some(FrozenError::Invalid("longer than its columns"))
        );
        let mut magic = bytes.clone();
        magic[0] ^= 1;
        assert!(FrozenGraph::open(&magic).is_err());
        // A stream byte changed: every read is an error or a list of real nodes, never a panic.
        let mut flipped = bytes.clone();
        let last = flipped.len() - 3;
        flipped[last] ^= 0xff;
        let frozen = FrozenGraph::open(&flipped).unwrap();
        let mut out = Vec::new();
        for node in 0..frozen.nodes() {
            if frozen.edges(node, &mut out).is_ok() {
                assert!(out.iter().all(|&w| w & !OUTPUT < frozen.nodes()));
            }
        }
    }

    #[test]
    fn the_overlay_sends_replaced_nodes_to_the_live_graph() {
        let csr = synthetic(4, 1_000, 5);
        let source = lists(&csr, 3);
        let bytes = write(&source, None).unwrap();
        let mut overlay = Overlay::new(FrozenGraph::open(&bytes).unwrap());
        let mut out = Vec::new();
        assert!(overlay.edges(7, &mut out).is_some());
        overlay.replace(7);
        overlay.replace(5_000);
        assert!(overlay.edges(7, &mut out).is_none());
        assert!(!overlay.is_frozen(5_000));
        assert!(overlay.is_frozen(8));
        assert_eq!(overlay.frozen().nodes(), 1_000);
    }
}

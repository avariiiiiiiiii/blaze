//! Walk-order numbering for the dependency graph. Deep verify walks from a memo through its
//! edges depth first, in read order, loading each dependency's color; with nodes numbered as
//! keys are first read, those loads land anywhere in the color column. When the graph is frozen
//! (`&mut` at a new revision, and when it is persisted) its nodes are renumbered so a walk reads
//! colors and edges mostly in sequence: [`dfs_preorder`] numbers nodes in the order the walk
//! first reaches them, [`reverse_cuthill_mckee`] is the classic bandwidth-reducing order to
//! compare it with, and [`renumber`] applies either to a graph in [`Csr`] form.
//!
//! Everything here is pure code over a CSR: the graph applies the permutation to its own
//! columns. [`synthetic`], [`verify_walk`] and [`line_changes`] are the benchmark's graph, walk
//! and locality measure (`benches/graph_walk.rs`).

#![deny(unsafe_code)]

use blaze_ids::rng::SplitMix64;

/// Colors are `u64`s: 8 to a 64-byte line.
const COLORS_PER_LINE: u64 = 8;

/// How many edges ahead [`verify_walk`] prefetches colors.
const PREFETCH_AHEAD: usize = 8;

/// A graph in compressed sparse rows: node `v`'s edges are
/// `targets[offsets[v] as usize..offsets[v + 1] as usize]`, in read order, node numbers only
/// (no output bit). `offsets.len() == nodes + 1`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Csr {
    pub offsets: Vec<u32>,
    pub targets: Vec<u32>,
}

impl Csr {
    #[inline]
    pub fn nodes(&self) -> u32 {
        self.offsets.len().saturating_sub(1) as u32
    }

    #[inline]
    pub fn edges(&self, node: u32) -> &[u32] {
        let (start, end) = self.span(node);
        &self.targets[start as usize..end as usize]
    }

    /// From one edge list per node.
    pub fn from_lists(lists: &[Vec<u32>]) -> Csr {
        let total = lists.iter().map(Vec::len).sum();
        let mut offsets = Vec::with_capacity(lists.len() + 1);
        let mut targets = Vec::with_capacity(total);
        offsets.push(0);
        for list in lists {
            targets.extend_from_slice(list);
            offsets.push(u32::try_from(targets.len()).expect("a graph's edges fit a u32"));
        }
        Csr { offsets, targets }
    }

    /// `node`'s edges as a range of `targets`.
    #[inline(always)]
    fn span(&self, node: u32) -> (u32, u32) {
        let node = node as usize;
        (self.offsets[node], self.offsets[node + 1])
    }
}

/// The nodes no edge names, ascending: where verify walks start (the queries the driver asks).
pub fn roots(csr: &Csr) -> Vec<u32> {
    let nodes = csr.nodes();
    // alloc: once per numbering: a bit a node, which nodes some edge names
    let mut named = vec![0u64; words(nodes)];
    mark_named(csr, &mut named);
    // alloc: once per numbering: the roots, the result
    let mut roots = Vec::new();
    for_each_unset(&named, nodes, |root| roots.push(root));
    roots
}

/// A permutation `new_of_old`: depth-first preorder from each root in ascending order, children
/// in read order, then a depth-first preorder from every node not yet numbered, in ascending
/// order (cycles, unreachable nodes). Iterative (an explicit stack of each open node's next edge
/// and end), a bitset of visited nodes, O(nodes + edges), allocation only for the result, the
/// stack and the bitset (one allocation for the visited and named halves). Deterministic.
pub fn dfs_preorder(csr: &Csr) -> Vec<u32> {
    let nodes = csr.nodes();
    let words = words(nodes);
    // alloc: once per numbering: the seen bits, a bit a node
    let mut bits = vec![0u64; 2 * words];
    let (visited, named) = bits.split_at_mut(words);
    mark_named(csr, named);
    let mut numbering = Preorder {
        // alloc: once per numbering: the result, one number a node
        new_of_old: vec![0u32; nodes as usize],
        next: 0,
        // alloc: once per numbering: the DFS stack, grown to the graph's depth
        stack: Vec::new(),
    };
    for_each_unset(named, nodes, |root| numbering.number_from(csr, root, visited));
    for word in 0..words {
        loop {
            let free = !visited[word] & valid_bits(word, nodes);
            if free == 0 {
                break;
            }
            let node = ((word as u32) << 6) | free.trailing_zeros();
            numbering.number_from(csr, node, visited);
        }
    }
    debug_assert_eq!(numbering.next, nodes);
    numbering.new_of_old
}

struct Preorder {
    new_of_old: Vec<u32>,
    next: u32,
    /// Each open node's next edge and end, as indices into `targets`.
    stack: Vec<(u32, u32)>,
}

impl Preorder {
    #[inline]
    fn number(&mut self, csr: &Csr, node: u32) {
        self.new_of_old[node as usize] = self.next;
        self.next += 1;
        self.stack.push(csr.span(node));
    }

    fn number_from(&mut self, csr: &Csr, root: u32, visited: &mut [u64]) {
        if !mark(visited, root) {
            return;
        }
        self.number(csr, root);
        while let Some(top) = self.stack.last_mut() {
            let (start, end) = *top;
            let list = &csr.targets[start as usize..end as usize];
            match list.iter().position(|&target| mark(visited, target)) {
                Some(at) => {
                    top.0 = start + at as u32 + 1;
                    self.number(csr, list[at]);
                }
                None => {
                    self.stack.pop();
                }
            }
        }
    }
}

/// Reverse Cuthill-McKee over the graph made undirected: BFS from a pseudo-peripheral node of
/// each component (lowest degree, ties by node), neighbors in ascending degree (ties by node),
/// then the order reversed. A node's degree counts its edges both ways, repeats included.
/// Deterministic.
pub fn reverse_cuthill_mckee(csr: &Csr) -> Vec<u32> {
    let nodes = csr.nodes();
    let incoming = transpose(csr);
    let degree: Vec<u32> = (0..nodes)
        .map(|node| {
            let out = csr.span(node);
            let inc = incoming.span(node);
            (out.1 - out.0).saturating_add(inc.1 - inc.0)
        })
        // alloc: once per RCM numbering (a comparison layout, never on a verify path)
        .collect();
    let key = |node: u32| (u64::from(degree[node as usize]) << 32) | u64::from(node);
    // alloc: once per RCM numbering: each node's sort key
    let mut starts: Vec<u64> = (0..nodes).map(key).collect();
    starts.sort_unstable();

    // alloc: once per RCM numbering: a bit a node
    let mut visited = vec![0u64; words(nodes)];
    let mut order: Vec<u32> = Vec::with_capacity(nodes as usize);
    // alloc: once per RCM numbering: one BFS level, reused across levels
    let mut batch: Vec<u64> = Vec::new();
    for start in starts {
        let start = start as u32;
        if !mark(&mut visited, start) {
            continue;
        }
        let mut head = order.len();
        order.push(start);
        while let Some(&node) = order.get(head) {
            head += 1;
            batch.clear();
            for &neighbor in csr.edges(node).iter().chain(incoming.edges(node)) {
                if mark(&mut visited, neighbor) {
                    batch.push(key(neighbor));
                }
            }
            batch.sort_unstable();
            order.extend(batch.iter().map(|&key| key as u32));
        }
    }
    debug_assert_eq!(order.len(), nodes as usize);

    // alloc: once per RCM numbering: the result, one number a node
    let mut new_of_old = vec![0u32; nodes as usize];
    for (at, &node) in order.iter().enumerate() {
        new_of_old[node as usize] = nodes - 1 - at as u32;
    }
    new_of_old
}

/// The graph with every edge turned around; each node's incoming edges in ascending source order.
fn transpose(csr: &Csr) -> Csr {
    let nodes = csr.nodes() as usize;
    // alloc: once per RCM numbering: the transpose's offsets
    let mut offsets = vec![0u32; nodes + 1];
    for &target in &csr.targets {
        offsets[target as usize] += 1;
    }
    let mut sum = 0u32;
    for offset in &mut offsets {
        let count = *offset;
        *offset = sum;
        sum += count;
    }
    // alloc: once per RCM numbering: the transpose's targets
    let mut targets = vec![0u32; csr.targets.len()];
    for source in 0..nodes as u32 {
        for &target in csr.edges(source) {
            let cursor = &mut offsets[target as usize];
            targets[*cursor as usize] = source;
            *cursor += 1;
        }
    }
    // Each cursor ended at the next node's start.
    offsets.copy_within(0..nodes, 1);
    offsets[0] = 0;
    Csr { offsets, targets }
}

/// `old_of_new` from `new_of_old`.
pub fn invert(new_of_old: &[u32]) -> Vec<u32> {
    // alloc: once per numbering: the inverse permutation, the result
    let mut old_of_new = vec![0u32; new_of_old.len()];
    for (old, &new) in new_of_old.iter().enumerate() {
        old_of_new[new as usize] = old as u32;
    }
    old_of_new
}

/// Whether `p` is a permutation of `0..p.len()`.
pub fn is_permutation(p: &[u32]) -> bool {
    let Ok(len) = u32::try_from(p.len()) else {
        return false;
    };
    // alloc: once per check: a bit a node (tests and debug asserts)
    let mut seen = vec![0u64; words(len)];
    p.iter().all(|&value| value < len && mark(&mut seen, value))
}

/// The graph under the numbering: node `new_of_old[v]` has edges
/// `[new_of_old[t] for t in csr.edges(v)]`, in the same order. Reads the old graph in sequence
/// and writes each list to its new place, so the only random reads are the `new_of_old` lookups,
/// all independent of each other.
pub fn renumber(csr: &Csr, new_of_old: &[u32]) -> Csr {
    let nodes = csr.nodes() as usize;
    assert_eq!(new_of_old.len(), nodes, "a new number for every node");
    // alloc: once per renumbering: the new graph's offsets, the result
    let mut offsets = vec![0u32; nodes + 1];
    for (old, &new) in new_of_old.iter().enumerate() {
        offsets[new as usize + 1] = csr.offsets[old + 1] - csr.offsets[old];
    }
    for at in 0..nodes {
        offsets[at + 1] += offsets[at];
    }
    // alloc: once per renumbering: the new graph's targets, the result
    let mut targets = vec![0u32; csr.targets.len()];
    for (old, &new) in new_of_old.iter().enumerate() {
        let list = csr.edges(old as u32);
        let start = offsets[new as usize] as usize;
        for (slot, &target) in targets[start..start + list.len()].iter_mut().zip(list) {
            *slot = new_of_old[target as usize];
        }
    }
    Csr { offsets, targets }
}

/// Fixed-point scale of the Zipf weights, so every platform draws the same graph.
const ZIPF_SCALE: u64 = 1 << 40;

/// Creation positions a window of first reads shuffles.
const READ_WINDOW: usize = 4096;

/// A synthetic query graph shaped like nushell's (#734: about 10M keys, 100M edges; pass smaller
/// sizes for tests): `nodes` keys, about `nodes * mean_edges` edges, a DAG (every edge names a
/// node created earlier), about 15% leaves (inputs: no edges), out-degree geometric with mean
/// `mean_edges`, targets 70% near the reader in creation order (its "module", within 64), 25% to
/// a power-law hub set (crate graph, def maps: Zipf over the first 1% of nodes), 5% uniform.
/// Then numbered as salsa numbers keys today: in first-read order under parallel execution,
/// modelled as creation order shuffled within windows of 4096. Deterministic per seed, on every
/// platform (integer draws only).
pub fn synthetic(seed: u64, nodes: u32, mean_edges: u32) -> Csr {
    let mut rng = SplitMix64::new(seed);
    let created = created(&mut rng, nodes, mean_edges);
    let first_read = first_read_order(&mut rng, nodes);
    renumber(&created, &first_read)
}

/// [`synthetic`]'s graph in creation order: every target below its reader.
fn created(rng: &mut SplitMix64, nodes: u32, mean_edges: u32) -> Csr {
    let hubs = (nodes / 100).max(1);
    let mut zipf = Vec::with_capacity(hubs as usize);
    let mut total = 0u64;
    for rank in 0..u64::from(hubs) {
        total += ZIPF_SCALE / (rank + 1);
        zipf.push(total);
    }

    let estimate = nodes as usize * mean_edges as usize;
    let mut offsets = Vec::with_capacity(nodes as usize + 1);
    let mut targets = Vec::with_capacity(estimate + estimate / 8);
    offsets.push(0u32);
    // A non-leaf's degree is geometric on 1.. with mean `mean_edges / 0.85`, so that with 15%
    // leaves the mean over all nodes is `mean_edges`: each further edge with odds
    // `1 - 85 / (100 * mean_edges)`.
    let stop_in = 100 * u64::from(mean_edges);
    for reader in 0..nodes {
        if reader > 0 && mean_edges > 0 && rng.below(100) >= 15 {
            let mut degree = 1u32;
            while rng.below(stop_in) >= 85 {
                degree += 1;
            }
            for _ in 0..degree {
                let draw = rng.below(100);
                let target = if draw < 70 {
                    reader - 1 - rng.below(u64::from(reader.min(64))) as u32
                } else if draw < 95 {
                    let reach = &zipf[..reader.min(hubs) as usize];
                    let at = rng.below(reach[reach.len() - 1]);
                    reach.partition_point(|&cumulative| cumulative <= at) as u32
                } else {
                    rng.below(u64::from(reader)) as u32
                };
                targets.push(target);
            }
        }
        offsets.push(u32::try_from(targets.len()).expect("the synthetic graph's edges fit a u32"));
    }
    Csr { offsets, targets }
}

/// Creation order shuffled within windows of [`READ_WINDOW`], as `new_of_old`.
fn first_read_order(rng: &mut SplitMix64, nodes: u32) -> Vec<u32> {
    // alloc: synthetic graphs only (benches and tests): the first-read order
    let mut order: Vec<u32> = (0..nodes).collect();
    for window in order.chunks_mut(READ_WINDOW) {
        rng.shuffle(window);
    }
    order
}

/// What a verify walk does, as a benchmark: from every root, depth first in read order, every
/// node once (a bitset `seen`, cleared by the caller), each edge loading `colors[target]`; with
/// `prefetch`, the colors of the next 8 edges of the current list are prefetched before the
/// current one is checked. Returns the wrapping sum of the colors loaded (so nothing is
/// optimized away). No allocation once `stack` and `seen` are big enough.
///
/// Finding the roots is a pass over every edge, which deep verify doesn't make (the driver names
/// its queries): `seen` holds a second bitset for it, and [`verify_walk_from`] takes the roots
/// instead.
pub fn verify_walk(
    csr: &Csr,
    colors: &[u64],
    prefetch: bool,
    stack: &mut Vec<(u32, u32)>,
    seen: &mut Vec<u64>,
) -> u64 {
    let nodes = csr.nodes();
    let words = words(nodes);
    if seen.len() < 2 * words {
        seen.resize(2 * words, 0);
    }
    let (visited, named) = seen.split_at_mut(words);
    let named = &mut named[..words];
    named.fill(0);
    mark_named(csr, named);
    stack.clear();
    let mut sum = 0u64;
    let mut load = |target: u32| {
        sum = sum.wrapping_add(colors[target as usize]);
    };
    if prefetch {
        for_each_unset(named, nodes, |root| {
            walk_from::<true, _>(csr, root, colors, stack, visited, &mut load)
        });
    } else {
        for_each_unset(named, nodes, |root| {
            walk_from::<false, _>(csr, root, colors, stack, visited, &mut load)
        });
    }
    sum
}

/// [`verify_walk`] from the given `roots`, in their order: the walk alone.
pub fn verify_walk_from(
    csr: &Csr,
    roots: &[u32],
    colors: &[u64],
    prefetch: bool,
    stack: &mut Vec<(u32, u32)>,
    seen: &mut Vec<u64>,
) -> u64 {
    let words = words(csr.nodes());
    if seen.len() < words {
        seen.resize(words, 0);
    }
    stack.clear();
    let mut sum = 0u64;
    let mut load = |target: u32| {
        sum = sum.wrapping_add(colors[target as usize]);
    };
    if prefetch {
        for &root in roots {
            walk_from::<true, _>(csr, root, colors, stack, seen, &mut load);
        }
    } else {
        for &root in roots {
            walk_from::<false, _>(csr, root, colors, stack, seen, &mut load);
        }
    }
    sum
}

/// The cache lines the walk's color loads touch that are not the line of the previous load: a
/// layout's locality as one number, for tests and the report.
pub fn line_changes(csr: &Csr) -> u64 {
    // alloc: tests and the report only: the walk's stack
    let mut stack = Vec::new();
    // alloc: tests and the report only: a bit a node
    let mut visited = vec![0u64; words(csr.nodes())];
    let mut changes = 0u64;
    let mut last = u64::MAX;
    let mut load = |target: u32| {
        let line = u64::from(target) / COLORS_PER_LINE;
        if line != last {
            changes += 1;
            last = line;
        }
    };
    for root in roots(csr) {
        walk_from::<false, _>(csr, root, &[], &mut stack, &mut visited, &mut load);
    }
    changes
}

/// The walk from `root` unless it was seen: each node's edges in read order, `load` for every
/// edge, descending into each target not seen before. The stack holds each open node's next
/// edge and end, as indices into `targets`.
#[inline(always)]
fn walk_from<const PREFETCH: bool, F: FnMut(u32)>(
    csr: &Csr,
    root: u32,
    colors: &[u64],
    stack: &mut Vec<(u32, u32)>,
    seen: &mut [u64],
    load: &mut F,
) {
    if !mark(seen, root) {
        return;
    }
    enter::<PREFETCH>(csr, root, colors, stack);
    while let Some(top) = stack.last_mut() {
        let (start, end) = *top;
        let list = &csr.targets[start as usize..end as usize];
        let mut child = None;
        for (at, &target) in list.iter().enumerate() {
            if PREFETCH && let Some(&ahead) = list.get(at + PREFETCH_AHEAD) {
                prefetch_color(colors, ahead);
            }
            load(target);
            if mark(seen, target) {
                child = Some((at, target));
                break;
            }
        }
        match child {
            Some((at, target)) => {
                top.0 = start + at as u32 + 1;
                enter::<PREFETCH>(csr, target, colors, stack);
            }
            None => {
                stack.pop();
            }
        }
    }
}

#[inline(always)]
fn enter<const PREFETCH: bool>(csr: &Csr, node: u32, colors: &[u64], stack: &mut Vec<(u32, u32)>) {
    let (start, end) = csr.span(node);
    if PREFETCH {
        for &target in csr.targets[start as usize..end as usize].iter().take(PREFETCH_AHEAD) {
            prefetch_color(colors, target);
        }
    }
    stack.push((start, end));
}

#[inline(always)]
fn prefetch_color(colors: &[u64], node: u32) {
    if let Some(color) = colors.get(node as usize) {
        prefetch::line(color);
    }
}

#[inline]
fn words(nodes: u32) -> usize {
    (nodes as usize).div_ceil(64)
}

/// Sets `node`'s bit; whether it was clear.
#[inline(always)]
fn mark(bits: &mut [u64], node: u32) -> bool {
    let word = &mut bits[(node >> 6) as usize];
    let bit = 1u64 << (node & 63);
    let fresh = *word & bit == 0;
    *word |= bit;
    fresh
}

/// The bits of word `word` that stand for nodes below `nodes`.
#[inline]
fn valid_bits(word: usize, nodes: u32) -> u64 {
    let base = (word as u64) << 6;
    let left = u64::from(nodes).saturating_sub(base);
    if left >= 64 { u64::MAX } else { (1u64 << left) - 1 }
}

/// Sets the bit of every node an edge names.
fn mark_named(csr: &Csr, named: &mut [u64]) {
    for &target in &csr.targets {
        named[(target >> 6) as usize] |= 1u64 << (target & 63);
    }
}

/// `f` of every node below `nodes` whose bit is clear, ascending.
#[inline]
fn for_each_unset(bits: &[u64], nodes: u32, mut f: impl FnMut(u32)) {
    for (word, &set) in bits[..words(nodes)].iter().enumerate() {
        let mut free = !set & valid_bits(word, nodes);
        while free != 0 {
            f(((word as u32) << 6) | free.trailing_zeros());
            free &= free - 1;
        }
    }
}

/// The file's one unsafe part: a prefetch hint by inline assembly (the stable intrinsics don't
/// cover aarch64), as in `colors.rs`.
#[allow(unsafe_code)]
mod prefetch {
    #[cfg(all(not(feature = "shuttle"), target_arch = "x86_64"))]
    #[inline(always)]
    pub(super) fn line(word: &u64) {
        // SAFETY: `prefetcht0` is a hint: it reads nothing into a register, changes no memory or
        // flags, and never faults, whatever the address; `word` is a live reference anyway.
        unsafe {
            std::arch::asm!(
                "prefetcht0 [{0}]",
                in(reg) std::ptr::from_ref(word),
                options(nostack, preserves_flags, readonly)
            );
        }
    }

    #[cfg(all(not(feature = "shuttle"), target_arch = "aarch64"))]
    #[inline(always)]
    pub(super) fn line(word: &u64) {
        // SAFETY: `prfm` is a hint: it changes no register, memory or flags and never faults,
        // whatever the address; `word` is a live reference anyway.
        unsafe {
            std::arch::asm!(
                "prfm pldl1keep, [{0}]",
                in(reg) std::ptr::from_ref(word),
                options(nostack, preserves_flags, readonly)
            );
        }
    }

    #[cfg(any(feature = "shuttle", not(any(target_arch = "x86_64", target_arch = "aarch64"))))]
    #[inline(always)]
    pub(super) fn line(word: &u64) {
        let _ = word;
    }
}

#[cfg(test)]
mod tests {
    use blaze_ids::rng::SplitMix64;

    use super::*;

    /// Random edge lists: `nodes` nodes of up to `max_degree` edges each, repeats allowed, every
    /// target below its reader when `dag`, else anywhere (self-loops and cycles included).
    fn random_lists(seed: u64, nodes: u32, max_degree: u64, dag: bool) -> Vec<Vec<u32>> {
        let mut rng = SplitMix64::new(seed);
        (0..nodes)
            .map(|reader| {
                let reach = if dag { reader } else { nodes };
                if reach == 0 {
                    return Vec::new();
                }
                let degree = rng.below(max_degree + 1);
                (0..degree).map(|_| rng.below(u64::from(reach)) as u32).collect()
            })
            .collect()
    }

    fn graphs() -> Vec<Csr> {
        let mut graphs = vec![
            Csr::from_lists(&[]),
            Csr::from_lists(&[vec![]]),
            Csr::from_lists(&[vec![0]]),
            Csr::from_lists(&[vec![1], vec![2], vec![0]]),
            Csr::from_lists(&[vec![], vec![0, 0, 0], vec![1, 0], vec![2, 1, 0]]),
        ];
        for seed in 0..12 {
            let nodes = [1, 2, 7, 64, 65, 130, 500, 2000][seed as usize % 8];
            graphs.push(Csr::from_lists(&random_lists(seed, nodes, 6, seed % 2 == 0)));
        }
        graphs.push(synthetic(3, 20_000, 6));
        graphs
    }

    /// The reference: recursive depth-first preorder from the roots, then from every node left.
    fn reference_preorder(csr: &Csr) -> Vec<u32> {
        fn visit(csr: &Csr, node: u32, new_of_old: &mut [u32], next: &mut u32) {
            if new_of_old[node as usize] != u32::MAX {
                return;
            }
            new_of_old[node as usize] = *next;
            *next += 1;
            for &target in csr.edges(node) {
                visit(csr, target, new_of_old, next);
            }
        }
        let nodes = csr.nodes();
        let mut named = vec![false; nodes as usize];
        for &target in &csr.targets {
            named[target as usize] = true;
        }
        let mut new_of_old = vec![u32::MAX; nodes as usize];
        let mut next = 0;
        for node in (0..nodes).filter(|&node| !named[node as usize]) {
            visit(csr, node, &mut new_of_old, &mut next);
        }
        for node in 0..nodes {
            visit(csr, node, &mut new_of_old, &mut next);
        }
        new_of_old
    }

    fn shuffled(seed: u64, nodes: u32) -> Vec<u32> {
        let mut p: Vec<u32> = (0..nodes).collect();
        SplitMix64::new(seed).shuffle(&mut p);
        p
    }

    fn colors(seed: u64, nodes: u32) -> Vec<u64> {
        let mut rng = SplitMix64::new(seed);
        (0..nodes).map(|_| rng.next_u64()).collect()
    }

    fn bit(bits: &[u64], node: u32) -> bool {
        (bits[(node >> 6) as usize] >> (node & 63)) & 1 == 1
    }

    #[test]
    fn csr_reads_back_its_lists() {
        let lists = random_lists(9, 300, 5, false);
        let csr = Csr::from_lists(&lists);
        assert_eq!(csr.nodes(), 300);
        assert_eq!(csr.offsets.len(), 301);
        for (node, list) in lists.iter().enumerate() {
            assert_eq!(csr.edges(node as u32), &list[..]);
        }
        assert_eq!(Csr::default().nodes(), 0);
    }

    #[test]
    fn roots_are_the_nodes_no_edge_names() {
        let csr = Csr::from_lists(&[vec![], vec![0], vec![1], vec![], vec![2, 3]]);
        assert_eq!(roots(&csr), vec![4]);
        for csr in graphs() {
            let expected: Vec<u32> = (0..csr.nodes())
                .filter(|node| !csr.targets.contains(node))
                .collect();
            assert_eq!(roots(&csr), expected);
        }
    }

    #[test]
    fn permutations_are_recognised() {
        assert!(is_permutation(&[]));
        assert!(is_permutation(&[0]));
        assert!(is_permutation(&[2, 0, 1]));
        assert!(!is_permutation(&[1]));
        assert!(!is_permutation(&[0, 0]));
        assert!(!is_permutation(&[0, 2]));
        assert!(!is_permutation(&[3, 1, 0]));
    }

    #[test]
    fn numberings_are_deterministic_permutations() {
        for csr in graphs() {
            for p in [dfs_preorder(&csr), reverse_cuthill_mckee(&csr)] {
                assert!(is_permutation(&p), "{} nodes", csr.nodes());
                assert!(is_permutation(&invert(&p)));
                assert_eq!(invert(&invert(&p)), p);
            }
            assert_eq!(dfs_preorder(&csr), dfs_preorder(&csr));
            assert_eq!(reverse_cuthill_mckee(&csr), reverse_cuthill_mckee(&csr));
        }
    }

    #[test]
    fn preorder_matches_the_recursive_reference() {
        for csr in graphs() {
            assert_eq!(dfs_preorder(&csr), reference_preorder(&csr), "{} nodes", csr.nodes());
        }
        for seed in 100..160 {
            let nodes = 1 + (seed as u32 % 50) * 7;
            let csr = Csr::from_lists(&random_lists(seed, nodes, 4, seed % 3 == 0));
            assert_eq!(dfs_preorder(&csr), reference_preorder(&csr), "seed {seed}");
        }
    }

    #[test]
    fn preorder_numbers_a_chain_and_a_diamond_in_walk_order() {
        // 3 -> 1 -> 0, 3 -> 2 -> 0: the walk is 3, 1, 0, 2.
        let csr = Csr::from_lists(&[vec![], vec![0], vec![0], vec![1, 2]]);
        assert_eq!(dfs_preorder(&csr), vec![2, 1, 3, 0]);
        // A cycle with no root starts at its lowest node.
        let csr = Csr::from_lists(&[vec![2], vec![0], vec![1]]);
        assert_eq!(dfs_preorder(&csr), vec![0, 2, 1]);
    }

    #[test]
    fn cuthill_mckee_on_a_path_runs_end_to_end() {
        // The path 0 - 2 - 1 - 3 made undirected: start at the lowest-degree end, 0.
        let csr = Csr::from_lists(&[vec![2], vec![2], vec![], vec![1]]);
        // BFS order 0, 2, 1, 3; reversed.
        assert_eq!(reverse_cuthill_mckee(&csr), vec![3, 1, 2, 0]);
    }

    #[test]
    fn renumbering_keeps_the_graph_and_the_walk() {
        for (seed, csr) in graphs().into_iter().enumerate() {
            let nodes = csr.nodes();
            let identity: Vec<u32> = (0..nodes).collect();
            assert_eq!(renumber(&csr, &identity), csr);
            let old_colors = colors(seed as u64, nodes);
            let (mut stack, mut seen) = (Vec::new(), Vec::new());
            let old_sum = verify_walk(&csr, &old_colors, false, &mut stack, &mut seen);
            let old_seen = seen.clone();
            seen.fill(0);
            assert_eq!(verify_walk(&csr, &old_colors, true, &mut stack, &mut seen), old_sum);
            seen.fill(0);
            let old_roots = roots(&csr);
            let from_roots =
                verify_walk_from(&csr, &old_roots, &old_colors, true, &mut stack, &mut seen);
            assert_eq!(from_roots, old_sum);
            let reached: Vec<bool> = (0..nodes).map(|node| bit(&old_seen, node)).collect();
            let expected_sum = (0..nodes)
                .filter(|&node| reached[node as usize])
                .flat_map(|node| csr.edges(node))
                .fold(0u64, |sum, &target| sum.wrapping_add(old_colors[target as usize]));
            assert_eq!(old_sum, expected_sum);

            let numberings =
                [dfs_preorder(&csr), reverse_cuthill_mckee(&csr), shuffled(seed as u64, nodes)];
            for p in numberings {
                let new = renumber(&csr, &p);
                assert_eq!(new.nodes(), nodes);
                assert_eq!(new.targets.len(), csr.targets.len());
                for old in 0..nodes {
                    let mapped: Vec<u32> = csr.edges(old).iter().map(|&t| p[t as usize]).collect();
                    assert_eq!(new.edges(p[old as usize]), &mapped[..]);
                }
                assert_eq!(renumber(&new, &invert(&p)), csr);

                let mut new_roots = roots(&new);
                new_roots.sort_unstable();
                let mut mapped_roots: Vec<u32> = old_roots.iter().map(|&r| p[r as usize]).collect();
                mapped_roots.sort_unstable();
                assert_eq!(new_roots, mapped_roots);

                let mut new_colors = vec![0u64; nodes as usize];
                for old in 0..nodes {
                    new_colors[p[old as usize] as usize] = old_colors[old as usize];
                }
                for prefetch in [false, true] {
                    seen.fill(0);
                    let sum = verify_walk(&new, &new_colors, prefetch, &mut stack, &mut seen);
                    assert_eq!(sum, old_sum);
                    for old in 0..nodes {
                        assert_eq!(bit(&seen, p[old as usize]), reached[old as usize]);
                    }
                }
            }
        }
    }

    #[test]
    fn a_dag_walk_visits_every_node() {
        let csr = synthetic(4, 50_000, 8);
        let (mut stack, mut seen) = (Vec::new(), Vec::new());
        verify_walk(&csr, &colors(4, csr.nodes()), true, &mut stack, &mut seen);
        assert!((0..csr.nodes()).all(|node| bit(&seen, node)));
    }

    #[test]
    fn synthetic_is_a_dag_of_the_asked_shape() {
        let shapes = [(1u64, 200_000u32, 10u32), (2, 100_000, 4), (3, 50_000, 1)];
        for (seed, nodes, mean_edges) in shapes {
            let mut rng = SplitMix64::new(seed);
            let made = created(&mut rng, nodes, mean_edges);
            assert_eq!(made.nodes(), nodes);
            for reader in 0..nodes {
                assert!(made.edges(reader).iter().all(|&target| target < reader), "seed {seed}");
            }
            let mean = made.targets.len() as f64 / f64::from(nodes);
            let want = f64::from(mean_edges);
            assert!((mean - want).abs() <= want * 0.1, "mean out-degree {mean}, asked {want}");
            let leaves = (0..nodes).filter(|&node| made.edges(node).is_empty()).count();
            let leaves = leaves as f64 / f64::from(nodes);
            assert!((0.13..0.17).contains(&leaves), "{leaves} leaves");

            let first_read = first_read_order(&mut rng, nodes);
            assert!(is_permutation(&first_read));
            for (creation, &read) in first_read.iter().enumerate() {
                assert_eq!(creation / READ_WINDOW, read as usize / READ_WINDOW);
            }
            assert_eq!(synthetic(seed, nodes, mean_edges), renumber(&made, &first_read));
        }
        assert_eq!(synthetic(7, 10_000, 10), synthetic(7, 10_000, 10));
        assert_ne!(synthetic(7, 10_000, 10), synthetic(8, 10_000, 10));
    }

    #[test]
    fn preorder_cuts_line_changes() {
        let csr = synthetic(1, 200_000, 10);
        let as_made = line_changes(&csr);
        let preorder = line_changes(&renumber(&csr, &dfs_preorder(&csr)));
        let rcm = line_changes(&renumber(&csr, &reverse_cuthill_mckee(&csr)));
        eprintln!(
            "line changes over {} edges: as made {as_made}, dfs preorder {preorder}, rcm {rcm}",
            csr.targets.len()
        );
        assert!(as_made <= csr.targets.len() as u64);
        assert!(preorder < as_made, "dfs preorder {preorder}, as made {as_made}");
    }

    #[test]
    fn walks_allocate_nothing_once_warm() {
        let (_, used) =
            blaze_alloc_count::measure(|| std::hint::black_box(Vec::<u8>::with_capacity(1)));
        assert_eq!(used.allocations, 1, "the counting allocator is installed for salsa's tests");

        let csr = synthetic(5, 20_000, 10);
        let colors = colors(5, csr.nodes());
        let roots = roots(&csr);
        let (mut stack, mut seen) = (Vec::new(), Vec::new());
        let first = verify_walk(&csr, &colors, true, &mut stack, &mut seen);
        blaze_alloc_count::assert_at_most(0, "warm verify walks", || {
            for prefetch in [false, true] {
                seen.fill(0);
                assert_eq!(verify_walk(&csr, &colors, prefetch, &mut stack, &mut seen), first);
                seen.fill(0);
                let sum = verify_walk_from(&csr, &roots, &colors, prefetch, &mut stack, &mut seen);
                assert_eq!(sum, first);
            }
        });
    }
}

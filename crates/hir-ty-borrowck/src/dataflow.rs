//! Gen/kill dataflow over the blocks of a body, as rustc's `rustc_mir_dataflow` solves it:
//! each block's effect is one gen set and one kill set, `out = (in & !kill) | gen`, found once
//! from its statements, and a work queue (forwards in reverse postorder, backwards in
//! postorder) joins by union until nothing changes. Effects on one edge only, a call's return
//! or what may be initialized leaving a block, are a callback on the edge.

use std::collections::VecDeque;

use hir_ty_mir::mir::BasicBlockId;

use super::{
    bitset::{BitMatrix, BitSet, union_words, words_for},
    cfg::{Cfg, index},
};

/// The effect of each block (or of anything else with a row) as a gen and a kill set.
pub(super) struct GenKill {
    words: usize,
    gen_: Vec<u64>,
    kill: Vec<u64>,
}

impl GenKill {
    /// The identity effect for `rows` rows over sets of `domain` bits.
    pub(super) fn new(rows: usize, domain: usize) -> Self {
        let words = words_for(domain);
        GenKill { words, gen_: vec![0; rows * words], kill: vec![0; rows * words] }
    }

    fn range(&self, row: usize) -> std::ops::Range<usize> {
        row * self.words..(row + 1) * self.words
    }

    /// Adds `bit` after what the row does so far.
    pub(super) fn gen_bit(&mut self, row: usize, bit: usize) {
        let at = row * self.words + bit / 64;
        self.gen_[at] |= 1 << (bit % 64);
        self.kill[at] &= !(1 << (bit % 64));
    }

    /// Removes `bit` after what the row does so far.
    pub(super) fn kill_bit(&mut self, row: usize, bit: usize) {
        let at = row * self.words + bit / 64;
        self.kill[at] |= 1 << (bit % 64);
        self.gen_[at] &= !(1 << (bit % 64));
    }

    /// The row's effect from what it does to the empty set (`made`) and to the full one
    /// (`kept`): for an effect that only adds and removes bits whatever is in the set, that is
    /// all of it.
    pub(super) fn set_from(&mut self, row: usize, made: &BitSet, kept: &BitSet) {
        let range = self.range(row);
        for (at, word) in range.clone().zip(made.words()) {
            self.gen_[at] = *word;
        }
        for (at, word) in range.zip(kept.words()) {
            self.kill[at] = !*word;
        }
    }

    pub(super) fn apply(&self, row: usize, state: &mut [u64]) {
        let range = self.range(row);
        let (gen_, kill) = (&self.gen_[range.clone()], &self.kill[range]);
        for ((word, gen_), kill) in state.iter_mut().zip(gen_).zip(kill) {
            *word = (*word & !kill) | gen_;
        }
    }
}

/// rustc's `WorkQueue`: each block at most once in the queue.
struct WorkQueue {
    queue: VecDeque<BasicBlockId>,
    queued: BitSet,
}

impl WorkQueue {
    fn new(blocks: usize, initial: impl Iterator<Item = BasicBlockId>) -> Self {
        let mut out = WorkQueue { queue: VecDeque::new(), queued: BitSet::new(blocks) };
        for it in initial {
            out.push(it);
        }
        out
    }

    fn push(&mut self, it: BasicBlockId) {
        if self.queued.insert(index(it)) {
            self.queue.push_back(it);
        }
    }

    fn pop(&mut self) -> Option<BasicBlockId> {
        let it = self.queue.pop_front()?;
        self.queued.remove(index(it));
        Some(it)
    }
}

/// The state at the start of each block of a forward analysis whose start block begins with
/// `boundary`: the least fixed point, unreachable blocks empty. `edge(from, to, state)`
/// changes what leaves `from` for `to`.
pub(super) fn forward(
    cfg: &Cfg,
    blocks: &GenKill,
    start: BasicBlockId,
    boundary: &BitSet,
    mut edge: impl FnMut(BasicBlockId, BasicBlockId, &mut [u64]),
) -> BitMatrix {
    let count = cfg.reachable.len();
    let mut entry = BitMatrix::new(count, blocks.words * 64);
    if count == 0 {
        return entry;
    }
    for (word, from) in entry.row_mut(index(start)).iter_mut().zip(boundary.words()) {
        *word = *from;
    }
    let mut out = vec![0u64; blocks.words];
    let mut along = vec![0u64; blocks.words];
    let mut queue = WorkQueue::new(count, cfg.order.iter().copied());
    while let Some(at) = queue.pop() {
        out.copy_from_slice(entry.row(index(at)));
        blocks.apply(index(at), &mut out);
        for &to in &cfg.successors[index(at)] {
            along.copy_from_slice(&out);
            edge(at, to, &mut along);
            if union_words(entry.row_mut(index(to)), &along) {
                queue.push(to);
            }
        }
    }
    entry
}

/// The state at the start of each block of a backward analysis, nothing live past the end:
/// the least fixed point, unreachable blocks empty. `edge(from, to, state)` changes what
/// comes back from `to` to the end of `from`.
pub(super) fn backward(
    cfg: &Cfg,
    blocks: &GenKill,
    mut edge: impl FnMut(BasicBlockId, BasicBlockId, &mut [u64]),
) -> BitMatrix {
    let count = cfg.reachable.len();
    let mut entry = BitMatrix::new(count, blocks.words * 64);
    let mut exit = vec![0u64; blocks.words];
    let mut along = vec![0u64; blocks.words];
    let mut queue = WorkQueue::new(count, cfg.order.iter().rev().copied());
    while let Some(at) = queue.pop() {
        exit.fill(0);
        for &to in &cfg.successors[index(at)] {
            along.copy_from_slice(entry.row(index(to)));
            edge(at, to, &mut along);
            union_words(&mut exit, &along);
        }
        blocks.apply(index(at), &mut exit);
        if entry.row(index(at)) != exit.as_slice() {
            entry.row_mut(index(at)).copy_from_slice(&exit);
            for &from in &cfg.predecessors[index(at)] {
                if cfg.reachable[index(from)] {
                    queue.push(from);
                }
            }
        }
    }
    entry
}

#[cfg(test)]
mod tests {
    use super::{GenKill, backward, forward};
    use crate::{
        bitset::{BitSet, iter_words},
        cfg::{Cfg, block, index},
    };

    struct Rng(u64);

    impl Rng {
        fn next(&mut self, bound: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % bound.max(1) as u64) as usize
        }
    }

    /// What one point does: bits it removes, then bits it adds.
    #[derive(Clone, Default)]
    struct Effect {
        kill: Vec<usize>,
        gen_: Vec<usize>,
    }

    impl Effect {
        fn apply(&self, state: &mut Vec<bool>) {
            for &it in &self.kill {
                state[it] = false;
            }
            for &it in &self.gen_ {
                state[it] = true;
            }
        }
    }

    fn effect(rng: &mut Rng, domain: usize) -> Effect {
        let mut out = Effect::default();
        for _ in 0..rng.next(3) {
            out.kill.push(rng.next(domain));
        }
        for _ in 0..rng.next(3) {
            out.gen_.push(rng.next(domain));
        }
        out
    }

    struct Case {
        cfg: Cfg,
        domain: usize,
        /// Each block's points, in the order the block runs them.
        points: Vec<Vec<Effect>>,
        /// An edge's own effect, applied to what crosses it.
        edges: Vec<((usize, usize), Effect)>,
        boundary: Vec<usize>,
    }

    fn case(rng: &mut Rng) -> Case {
        let blocks = 1 + rng.next(16);
        let domain = 1 + rng.next(150);
        let mut edges = Vec::new();
        for b in 1..blocks {
            if rng.next(5) != 0 {
                edges.push((rng.next(b), b));
            }
        }
        for _ in 0..rng.next(blocks * 2 + 1) {
            edges.push((rng.next(blocks), rng.next(blocks)));
        }
        let lens: Vec<usize> = (0..blocks).map(|_| 1 + rng.next(5)).collect();
        let cfg = Cfg::from_edges(&lens, &edges);
        let points =
            lens.iter().map(|&len| (0..len).map(|_| effect(rng, domain)).collect()).collect();
        let mut edge_effects = Vec::new();
        for (from, to) in edges {
            if rng.next(3) == 0 && !edge_effects.iter().any(|(it, _)| *it == (from, to)) {
                edge_effects.push(((from, to), effect(rng, domain)));
            }
        }
        let boundary = (0..rng.next(4)).map(|_| rng.next(domain)).collect();
        Case { cfg, domain, points, edges: edge_effects, boundary }
    }

    fn edge_effect(case: &Case, from: usize, to: usize) -> Option<&Effect> {
        case.edges.iter().find(|(it, _)| *it == (from, to)).map(|(_, it)| it)
    }

    fn apply_words(effect: &Effect, domain: usize, words: &mut [u64]) {
        let mut state: Vec<bool> =
            (0..domain).map(|it| words[it / 64] & (1 << (it % 64)) != 0).collect();
        effect.apply(&mut state);
        words.fill(0);
        for (it, &set) in state.iter().enumerate() {
            if set {
                words[it / 64] |= 1 << (it % 64);
            }
        }
    }

    fn as_set(state: &[bool]) -> Vec<usize> {
        state.iter().enumerate().filter(|(_, it)| **it).map(|(at, _)| at).collect()
    }

    fn reachable(case: &Case) -> Vec<usize> {
        case.cfg.order.iter().map(|&it| index(it)).collect()
    }

    /// The forward fixed point taken a point at a time, every reachable point again until
    /// nothing changes.
    fn naive_forward(case: &Case) -> Vec<Vec<usize>> {
        let blocks = case.points.len();
        let mut entry = vec![vec![false; case.domain]; blocks];
        for &it in &case.boundary {
            entry[0][it] = true;
        }
        let order = reachable(case);
        loop {
            let mut changed = false;
            for &b in &order {
                let mut state = entry[b].clone();
                for point in &case.points[b] {
                    point.apply(&mut state);
                }
                for &to in &case.cfg.successors[b] {
                    let mut along = state.clone();
                    if let Some(it) = edge_effect(case, b, index(to)) {
                        it.apply(&mut along);
                    }
                    for (into, from) in entry[index(to)].iter_mut().zip(&along) {
                        changed |= *from && !*into;
                        *into |= from;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        entry.iter().map(|it| as_set(it)).collect()
    }

    fn naive_backward(case: &Case) -> Vec<Vec<usize>> {
        let blocks = case.points.len();
        let mut entry = vec![vec![false; case.domain]; blocks];
        let order = reachable(case);
        loop {
            let mut changed = false;
            for &b in order.iter().rev() {
                let mut state = vec![false; case.domain];
                for &to in &case.cfg.successors[b] {
                    let mut along = entry[index(to)].clone();
                    if let Some(it) = edge_effect(case, b, index(to)) {
                        it.apply(&mut along);
                    }
                    for (into, from) in state.iter_mut().zip(&along) {
                        *into |= from;
                    }
                }
                for point in case.points[b].iter().rev() {
                    point.apply(&mut state);
                }
                if state != entry[b] {
                    entry[b] = state;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        entry.iter().map(|it| as_set(it)).collect()
    }

    /// Each block's effect composed point by point, as the analyses build theirs.
    fn composed(case: &Case, backward: bool) -> GenKill {
        let mut out = GenKill::new(case.points.len(), case.domain);
        for (b, points) in case.points.iter().enumerate() {
            let mut run = |point: &Effect| {
                for &it in &point.kill {
                    out.kill_bit(b, it);
                }
                for &it in &point.gen_ {
                    out.gen_bit(b, it);
                }
            };
            if backward {
                points.iter().rev().for_each(&mut run);
            } else {
                points.iter().for_each(&mut run);
            }
        }
        out
    }

    /// Each block's effect from running it on the empty and the full set, as the analyses
    /// whose effects are written on a set build theirs.
    fn from_runs(case: &Case) -> GenKill {
        let mut out = GenKill::new(case.points.len(), case.domain);
        for (b, points) in case.points.iter().enumerate() {
            let mut made = vec![false; case.domain];
            let mut kept = vec![true; case.domain];
            for point in points {
                point.apply(&mut made);
                point.apply(&mut kept);
            }
            let to_set = |it: &[bool]| {
                let mut set = BitSet::new(case.domain);
                for at in as_set(it) {
                    set.insert(at);
                }
                set
            };
            out.set_from(b, &to_set(&made), &to_set(&kept));
        }
        out
    }

    fn rows(entry: &crate::bitset::BitMatrix, blocks: usize) -> Vec<Vec<usize>> {
        (0..blocks).map(|b| iter_words(entry.row(b)).collect()).collect()
    }

    #[test]
    fn forward_matches_a_point_at_a_time() {
        let mut rng = Rng(0x2545f4914f6cdd1d);
        for round in 0..400 {
            let case = case(&mut rng);
            let want = naive_forward(&case);
            let mut boundary = BitSet::new(case.domain);
            for &it in &case.boundary {
                boundary.insert(it);
            }
            for blocks in [composed(&case, false), from_runs(&case)] {
                let entry = forward(&case.cfg, &blocks, block(0), &boundary, |from, to, state| {
                    if let Some(it) = edge_effect(&case, index(from), index(to)) {
                        apply_words(it, case.domain, state);
                    }
                });
                assert_eq!(rows(&entry, case.points.len()), want, "round {round}");
            }
        }
    }

    #[test]
    fn backward_matches_a_point_at_a_time() {
        let mut rng = Rng(0x853c49e6748fea9b);
        for round in 0..400 {
            let case = case(&mut rng);
            let want = naive_backward(&case);
            let entry = backward(&case.cfg, &composed(&case, true), |from, to, state| {
                if let Some(it) = edge_effect(&case, index(from), index(to)) {
                    apply_words(it, case.domain, state);
                }
            });
            assert_eq!(rows(&entry, case.points.len()), want, "round {round}");
        }
    }
}

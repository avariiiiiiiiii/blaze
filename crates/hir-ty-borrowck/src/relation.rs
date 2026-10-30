//! Facts as columnar relations: rows `(kind, a, b)` gathered as they are found, then frozen
//! into a CSR over `(kind, a)`, so the `b`s of one `a` are one range: the moves at a point, the
//! loans of a local, the uses at a point. Freezing is a counting sort in place (count, prefix
//! sum, scatter with the start array as the cursor): two passes, and no allocation but the
//! arrays it returns, whatever the number of kinds. A lookup is three loads.

/// That `a` relates to `b` by `kind`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Fact {
    pub(super) kind: u8,
    pub(super) a: u32,
    pub(super) b: u32,
}

const _: () = assert!(size_of::<Fact>() == 12);

/// Adjacency lists in compressed sparse row form: the items of node `n` are
/// `items[start[n]..start[n + 1]]`. 4 bytes per node and one `T` per item.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct Csr<T = u32> {
    start: Vec<u32>,
    items: Vec<T>,
}

impl<T> Default for Csr<T> {
    fn default() -> Self {
        Csr { start: vec![0], items: Vec::new() }
    }
}

/// `start` (one longer than the nodes) from the node of each item: `start[n]` where node `n`'s
/// items begin. Returns the number of items.
fn starts(start: &mut [u32], nodes: impl Iterator<Item = usize>) -> usize {
    for node in nodes {
        start[node + 1] += 1;
    }
    for at in 1..start.len() {
        start[at] += start[at - 1];
    }
    start.last().map_or(0, |&it| it as usize)
}

/// After a scatter that used `start[n]` as node `n`'s cursor, which leaves it where node `n`
/// ends, puts each start back.
fn rewind(start: &mut [u32]) {
    for at in (1..start.len()).rev() {
        start[at] = start[at - 1];
    }
    start[0] = 0;
}

impl<T: Copy> Csr<T> {
    /// The lists of `nodes` nodes from `(node, item)` pairs, each list in the order its pairs
    /// come in (a counting sort, which is stable). The pairs are walked twice and never
    /// copied. A node at or past `nodes` is a bug.
    pub(super) fn build<I>(nodes: usize, pairs: I) -> Self
    where
        I: IntoIterator<Item = (u32, T)>,
        I::IntoIter: Clone,
    {
        let pairs = pairs.into_iter();
        let mut start = vec![0u32; nodes + 1];
        let total = starts(&mut start, pairs.clone().map(|(node, _)| node as usize));
        let Some((_, filler)) = pairs.clone().next() else {
            return Csr { start, items: Vec::new() };
        };
        let mut items = vec![filler; total];
        for (node, item) in pairs {
            let at = &mut start[node as usize];
            items[*at as usize] = item;
            *at += 1;
        }
        rewind(&mut start);
        Csr { start, items }
    }

    /// An empty CSR to append nodes to in order, with room for `nodes` nodes and `items`
    /// items.
    pub(super) fn with_capacity(nodes: usize, items: usize) -> Self {
        let mut start = Vec::with_capacity(nodes + 1);
        start.push(0);
        Csr { start, items: Vec::with_capacity(items) }
    }

    /// Adds an item to the node being appended.
    pub(super) fn push(&mut self, item: T) {
        self.items.push(item);
    }

    /// Adds the items of the ended node `node` to the node being appended.
    pub(super) fn push_from(&mut self, node: usize) {
        let (from, to) = (self.start[node] as usize, self.start[node + 1] as usize);
        self.items.extend_from_within(from..to);
    }

    /// The items of the node being appended, so far.
    pub(super) fn open(&self) -> &[T] {
        let from = *self.start.last().expect("a CSR has a start") as usize;
        &self.items[from..]
    }

    /// The items of the node being appended, so far, to change in place.
    pub(super) fn open_mut(&mut self) -> &mut [T] {
        let from = *self.start.last().expect("a CSR has a start") as usize;
        &mut self.items[from..]
    }

    /// Drops the open node's items past `len`.
    pub(super) fn truncate_open(&mut self, len: usize) {
        let from = *self.start.last().expect("a CSR has a start") as usize;
        self.items.truncate(from + len);
    }

    /// Ends the node being appended; the next item starts the next node.
    pub(super) fn end_node(&mut self) {
        self.start.push(self.items.len() as u32);
    }

    /// The items of `node`, none for a node past the last.
    pub(super) fn of(&self, node: usize) -> &[T] {
        match (self.start.get(node), self.start.get(node + 1)) {
            (Some(&from), Some(&to)) => &self.items[from as usize..to as usize],
            _ => &[],
        }
    }

    pub(super) fn nodes(&self) -> usize {
        self.start.len() - 1
    }
}

/// Rows of [`Fact`]s, as they are found.
#[derive(Default)]
pub(super) struct Facts {
    rows: Vec<Fact>,
}

impl Facts {
    pub(super) fn with_capacity(rows: usize) -> Self {
        Facts { rows: Vec::with_capacity(rows) }
    }

    pub(super) fn push(&mut self, kind: u8, a: usize, b: usize) {
        self.rows.push(Fact { kind, a: a as u32, b: b as u32 });
    }

    /// One CSR over every kind, kind `k` taking `domains[k]` values of `a`: the `b`s of an `a`
    /// in the order their facts were pushed.
    pub(super) fn freeze(self, domains: &[usize]) -> Relation {
        let mut base = Vec::with_capacity(domains.len() + 1);
        let mut nodes = 0u32;
        for &domain in domains {
            base.push(nodes);
            nodes += domain as u32;
        }
        base.push(nodes);
        let node = |fact: &Fact| (base[fact.kind as usize] + fact.a) as usize;
        let mut start = vec![0u32; nodes as usize + 1];
        let total = starts(&mut start, self.rows.iter().map(node));
        let mut items = vec![0u32; total];
        for fact in &self.rows {
            let at = &mut start[node(fact)];
            items[*at as usize] = fact.b;
            *at += 1;
        }
        rewind(&mut start);
        Relation { base, start, items }
    }
}

/// [`Facts`] frozen: the `b`s of each `(kind, a)`.
#[derive(Clone, Debug)]
pub(super) struct Relation {
    /// Where each kind's nodes begin, and past the last, the number of nodes.
    base: Vec<u32>,
    start: Vec<u32>,
    items: Vec<u32>,
}

impl Default for Relation {
    fn default() -> Self {
        Relation { base: vec![0], start: vec![0], items: Vec::new() }
    }
}

impl Relation {
    pub(super) fn get(&self, kind: u8, a: usize) -> &[u32] {
        let kind = kind as usize;
        let (Some(&from), Some(&to)) = (self.base.get(kind), self.base.get(kind + 1)) else {
            return &[];
        };
        if a >= (to - from) as usize {
            return &[];
        }
        let node = from as usize + a;
        &self.items[self.start[node] as usize..self.start[node + 1] as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::{Csr, Facts};

    #[test]
    fn a_relation_is_its_facts_grouped_in_order() {
        let mut seed = 0x9e3779b97f4a7c15u64;
        let mut next = |bound: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % bound
        };
        for _ in 0..200 {
            let domains = [1 + next(20) as usize, 1 + next(5) as usize, 1 + next(40) as usize];
            let mut rows = Vec::new();
            let mut facts = Facts::default();
            for _ in 0..next(100) {
                let kind = next(3) as u8;
                let a = next(domains[kind as usize] as u64) as usize;
                let b = next(1000) as usize;
                rows.push((kind, a, b));
                facts.push(kind, a, b);
            }
            let relation = facts.freeze(&domains);
            for (kind, &domain) in domains.iter().enumerate() {
                for a in 0..domain + 2 {
                    let want: Vec<u32> = rows
                        .iter()
                        .filter(|&&(k, x, _)| k as usize == kind && x == a)
                        .map(|&(_, _, b)| b as u32)
                        .collect();
                    assert_eq!(relation.get(kind as u8, a), want.as_slice());
                }
            }
            assert!(relation.get(7, 0).is_empty());
        }
    }

    #[test]
    fn a_csr_keeps_each_lists_order() {
        let pairs = [(2, 'a'), (0, 'b'), (2, 'c'), (3, 'd'), (2, 'e')];
        let csr = Csr::build(4, pairs.iter().copied());
        assert_eq!(csr.of(0), &['b']);
        assert_eq!(csr.of(1), &[] as &[char]);
        assert_eq!(csr.of(2), &['a', 'c', 'e']);
        assert_eq!(csr.of(3), &['d']);
        assert_eq!(csr.of(4), &[] as &[char]);
        assert_eq!((csr.nodes(), csr.items.len()), (4, 5));
        assert_eq!(Csr::<u32>::default().of(0), &[] as &[u32]);
        let empty = Csr::<u32>::build(3, std::iter::empty());
        assert_eq!((empty.nodes(), empty.of(2)), (3, &[] as &[u32]));
    }
}

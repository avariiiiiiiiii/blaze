//! Facts as columnar relations: rows `(kind, a, b)` gathered as they are found, then frozen,
//! for each kind, into a CSR over `a`, so the `b`s of one `a` are one range: the moves at a
//! point, the loans of a local, the uses at a point.

/// That `a` relates to `b` by `kind`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Fact {
    pub(super) kind: u8,
    pub(super) a: u32,
    pub(super) b: u32,
}

const _: () = assert!(size_of::<Fact>() == 12);

/// Adjacency lists in compressed sparse row form: the items of node `n` are
/// `items[start[n]..start[n + 1]]`.
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

impl<T: Copy> Csr<T> {
    /// The lists of `nodes` nodes from `(node, item)` pairs, each list in the order its pairs
    /// come in: a counting sort, which is stable. A node at or past `nodes` is a bug.
    pub(super) fn build(nodes: usize, pairs: impl IntoIterator<Item = (u32, T)>) -> Self {
        let pairs: Vec<(u32, T)> = pairs.into_iter().collect();
        let mut start = vec![0u32; nodes + 1];
        for &(node, _) in &pairs {
            start[node as usize + 1] += 1;
        }
        for at in 1..start.len() {
            start[at] += start[at - 1];
        }
        let mut next: Vec<u32> = start[..nodes].to_vec();
        let mut items: Vec<Option<T>> = vec![None; pairs.len()];
        for (node, item) in pairs {
            let at = &mut next[node as usize];
            items[*at as usize] = Some(item);
            *at += 1;
        }
        let items = items.into_iter().map(|it| it.expect("every slot is filled")).collect();
        Csr { start, items }
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
    pub(super) fn push(&mut self, kind: u8, a: usize, b: usize) {
        self.rows.push(Fact { kind, a: a as u32, b: b as u32 });
    }

    /// One CSR per kind, kind `k` over `domains[k]` values of `a`: the `b`s of an `a` in the
    /// order their facts were pushed.
    pub(super) fn freeze(self, domains: &[usize]) -> Relation {
        let mut by_kind: Vec<Vec<(u32, u32)>> = vec![Vec::new(); domains.len()];
        for Fact { kind, a, b } in self.rows {
            by_kind[kind as usize].push((a, b));
        }
        Relation {
            kinds: by_kind
                .into_iter()
                .zip(domains)
                .map(|(pairs, &domain)| Csr::build(domain, pairs))
                .collect(),
        }
    }
}

/// [`Facts`] frozen: the `b`s of each `a`, per kind.
#[derive(Clone, Default, Debug)]
pub(super) struct Relation {
    kinds: Vec<Csr>,
}

impl Relation {
    pub(super) fn get(&self, kind: u8, a: usize) -> &[u32] {
        self.kinds.get(kind as usize).map_or(&[], |it| it.of(a))
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
        let csr = Csr::build(4, [(2, 'a'), (0, 'b'), (2, 'c'), (3, 'd'), (2, 'e')]);
        assert_eq!(csr.of(0), &['b']);
        assert_eq!(csr.of(1), &[] as &[char]);
        assert_eq!(csr.of(2), &['a', 'c', 'e']);
        assert_eq!(csr.of(3), &['d']);
        assert_eq!(csr.of(4), &[] as &[char]);
        assert_eq!((csr.nodes(), csr.items.len()), (4, 5));
        assert_eq!(Csr::<u32>::default().of(0), &[] as &[u32]);
    }
}

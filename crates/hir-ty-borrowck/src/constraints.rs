//! The outlives constraints of a body as columns: `(category, sup, sub)` read by region
//! inference and Polonius on every constraint, the location, span and whether a closure asked
//! for it read for errors and localized edges. The outlives graph is a CSR of constraint
//! indices by `sup`, built once and kept until a constraint is added.

use std::cell::OnceCell;

use hir_ty::mir::MirSpan;

use super::{
    cfg::{Location, block, index},
    regions::{Category, Outlives, Rv},
    relation::Csr,
};

const _: () = assert!(size_of::<Category>() == 1);
const _: () = assert!(size_of::<MirSpan>() <= 8);

/// No location: the constraint holds at every point.
const EVERYWHERE: u32 = u32::MAX;

#[derive(Default)]
pub(super) struct Constraints {
    category: Vec<Category>,
    sup: Vec<u32>,
    sub: Vec<u32>,
    /// The block and statement, the block [`EVERYWHERE`] for none.
    location: Vec<[u32; 2]>,
    span: Vec<MirSpan>,
    from_closure: Vec<u64>,
    by_sup: OnceCell<Csr>,
}

impl Constraints {
    pub(super) fn push(&mut self, it: Outlives) {
        let at = self.category.len();
        self.category.push(it.category);
        self.sup.push(it.sup as u32);
        self.sub.push(it.sub as u32);
        self.location.push(match it.location {
            Some(location) => [index(location.block) as u32, location.statement as u32],
            None => [EVERYWHERE, 0],
        });
        self.span.push(it.span);
        if at.is_multiple_of(64) {
            self.from_closure.push(0);
        }
        if it.from_closure {
            self.from_closure[at / 64] |= 1 << (at % 64);
        }
        self.by_sup.take();
    }

    pub(super) fn len(&self) -> usize {
        self.category.len()
    }

    pub(super) fn sup(&self, at: usize) -> Rv {
        self.sup[at] as Rv
    }

    pub(super) fn sub(&self, at: usize) -> Rv {
        self.sub[at] as Rv
    }

    pub(super) fn category(&self, at: usize) -> Category {
        self.category[at]
    }

    pub(super) fn set_category(&mut self, at: usize, category: Category) {
        self.category[at] = category;
    }

    pub(super) fn location(&self, at: usize) -> Option<Location> {
        let [block_at, statement] = self.location[at];
        (block_at != EVERYWHERE)
            .then(|| Location { block: block(block_at as usize), statement: statement as usize })
    }

    pub(super) fn get(&self, at: usize) -> Outlives {
        Outlives {
            sup: self.sup(at),
            sub: self.sub(at),
            location: self.location(at),
            category: self.category[at],
            span: self.span[at],
            from_closure: self.from_closure[at / 64] & (1 << (at % 64)) != 0,
        }
    }

    pub(super) fn iter(&self) -> Iter<'_> {
        Iter { constraints: self, at: 0..self.len() }
    }

    /// The indices of the constraints whose `sup` is `sup`, in the order they were added.
    pub(super) fn of_sup(&self, sup: Rv) -> &[u32] {
        self.by_sup
            .get_or_init(|| {
                let regions = self.sup.iter().map(|&it| it as usize + 1).max().unwrap_or(0);
                Csr::build(regions, (0..self.len()).map(|at| (self.sup[at], at as u32)))
            })
            .of(sup)
    }

    /// The subs below `count` of the constraints whose `sup` is `sup`, one per constraint.
    pub(super) fn subs_of(&self, count: usize, sup: Rv) -> impl Iterator<Item = Rv> + '_ {
        let ids = if sup < count { self.of_sup(sup) } else { &[] };
        ids.iter().map(|&at| self.sub[at as usize] as Rv).filter(move |&it| it < count)
    }
}

impl Extend<Outlives> for Constraints {
    fn extend<I: IntoIterator<Item = Outlives>>(&mut self, iter: I) {
        for it in iter {
            self.push(it);
        }
    }
}

pub(super) struct Iter<'a> {
    constraints: &'a Constraints,
    at: std::ops::Range<usize>,
}

impl Iterator for Iter<'_> {
    type Item = Outlives;

    fn next(&mut self) -> Option<Outlives> {
        self.at.next().map(|at| self.constraints.get(at))
    }
}

impl<'a> IntoIterator for &'a Constraints {
    type Item = Outlives;
    type IntoIter = Iter<'a>;

    fn into_iter(self) -> Iter<'a> {
        self.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::Constraints;
    use crate::{
        cfg::{Location, block},
        regions::{Category, Outlives},
    };
    use hir_ty::mir::MirSpan;

    #[test]
    fn a_constraint_reads_back_as_it_was_added() {
        let mut constraints = Constraints::default();
        let mut added = Vec::new();
        for at in 0..150usize {
            let it = Outlives {
                sup: at % 7,
                sub: (at * 3) % 11,
                location: (at % 3 != 0).then(|| Location { block: block(at % 5), statement: at }),
                category: if at % 2 == 0 { Category::Assignment } else { Category::Boring },
                span: MirSpan::Unknown,
                from_closure: at % 5 == 1,
            };
            constraints.push(it);
            added.push(it);
        }
        for (at, want) in added.iter().enumerate() {
            let got = constraints.get(at);
            assert_eq!(
                (got.sup, got.sub, got.location, got.category, got.span, got.from_closure),
                (want.sup, want.sub, want.location, want.category, want.span, want.from_closure)
            );
        }
        constraints.set_category(4, Category::TypeAnnotation);
        assert_eq!(constraints.category(4), Category::TypeAnnotation);
        for sup in 0..9 {
            let want: Vec<u32> =
                (0..added.len()).filter(|&at| added[at].sup == sup).map(|at| at as u32).collect();
            assert_eq!(constraints.of_sup(sup), want.as_slice());
            // Below 6 regions, those of region 6 and the subs past 5 are left out.
            let subs: Vec<usize> = constraints.subs_of(6, sup).collect();
            let want: Vec<usize> = want
                .iter()
                .map(|&at| added[at as usize].sub)
                .filter(|&it| it < 6 && sup < 6)
                .collect();
            assert_eq!(subs, want);
        }
        // Adding a constraint drops the graph built before.
        constraints.push(Outlives { sup: 0, sub: 1, ..added[0] });
        assert_eq!(constraints.of_sup(0).last(), Some(&150));
    }
}

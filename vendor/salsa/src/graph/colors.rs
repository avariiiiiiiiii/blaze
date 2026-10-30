//! Node colors: rustc's `DepNodeColorMap` (docs/v2/plan/lanes/db/speed-of-light.md, "Deep
//! verify"). Each node has one `u64` beside its key columns saying in which revision its key was
//! last verified and when its value last changed, so deep verify checks an edge with one load of
//! the dependency's color instead of finding its memo through its key (about 8 dependent lines).
//!
//! A color is only a shortcut that may say "unchanged": a reader trusts it only when it was
//! written in the current revision, by a final (not provisional) memo, without accumulated
//! values, and its changed-at is at or before the reader's verified-at. Anything else (an old
//! revision, a provisional or removed memo, a change after the reader) sends the reader to the
//! memo, which decides as it did before colors existed. So a color written late, missed or raced
//! costs a slower check, never a wrong answer, provided it never says "unchanged" when the memo
//! path would not:
//!
//! - Every memo published or verified in revision `R` raises its key's color to at least
//!   `(R, its changed-at, its accumulated bit)`; a provisional memo or a removed one raises it to
//!   `(R, unknown)`. Colors only rise, by `fetch_max` on the whole word (verified-at in the high
//!   half, so a later revision always wins), so of two racing writers the higher stays, whatever
//!   their order: within a revision the color has the accumulated bit if any memo of the key had
//!   it, else the largest changed-at any memo had, which is conservative, and "unknown" sorts
//!   above both.
//! - Verified-at only ever moves to the current revision, so a color of revision `R` means some
//!   memo of the key was final and verified in `R`, and every memo published after it in `R`
//!   wrote its own color too.
//!
//! An input field's color is written by deep verify when it checks the field (its changed-at
//! can't move within a revision, since setting it needs `&mut`), and is good for that revision.
//! Debug builds check every color that said "unchanged" against the memo path
//! (`function::maybe_changed_after`).

use super::Graph;
use crate::Revision;
use crate::key::DatabaseKeyIndex;
use crate::sync::atomic::Ordering;

/// A node's color word: the revision it was verified in (high 32 bits), then whether it or its
/// inputs had accumulated values (bit 31), then its changed-at (31 bits). The accumulated bit
/// sits above changed-at so that `fetch_max` never drops it for a later changed-at. All ones in
/// the low half is "unknown": ask the memo.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) struct Color(u64);

impl Color {
    const ACCUMULATED: u32 = 1 << 31;
    const UNKNOWN: u32 = u32::MAX;
    /// The first changed-at a color can't hold; a memo past it is "unknown".
    const CHANGED_LIMIT: u32 = Self::ACCUMULATED - 1;

    /// The color of a final memo verified in `verified_at`.
    #[inline]
    pub(crate) fn verified(verified_at: Revision, changed_at: Revision, accumulated: bool) -> Color {
        let changed = changed_at.as_u32();
        let low = if changed >= Self::CHANGED_LIMIT {
            Self::UNKNOWN
        } else if accumulated {
            changed | Self::ACCUMULATED
        } else {
            changed
        };
        Color((u64::from(verified_at.as_u32()) << 32) | u64::from(low))
    }

    /// The color of a key whose memo in `revision` is provisional or gone: the memo decides.
    #[inline]
    pub(crate) fn unknown(revision: Revision) -> Color {
        Color((u64::from(revision.as_u32()) << 32) | u64::from(Self::UNKNOWN))
    }

    /// Whether this color proves its key unchanged for a reader verified at `reader_verified_at`
    /// in the current revision `now`, with no accumulated values to fold in. "Unknown" and the
    /// accumulated bit both fail the last comparison, since a reader's revision is below 2^31
    /// whenever a changed-at at or before it fits.
    #[inline]
    pub(crate) fn unchanged_since(self, now: Revision, reader_verified_at: Revision) -> bool {
        let low = self.0 as u32;
        (self.0 >> 32) as u32 == now.as_u32()
            && low <= reader_verified_at.as_u32().min(Self::CHANGED_LIMIT - 1)
    }

    /// Whether a final memo of the key was published or verified in `revision` and none was
    /// provisional or removed in it: then its input-closure filter (`graph::bloom`) is current.
    #[inline]
    pub(crate) fn verified_in(self, revision: Revision) -> bool {
        (self.0 >> 32) as u32 == revision.as_u32() && self.0 as u32 != Self::UNKNOWN
    }
}

impl Graph {
    /// The color of `node`.
    #[inline]
    pub(crate) fn color(&self, node: u32) -> Color {
        // Acquire: pairs with `paint`, so a reader that trusts the color sees what the painter
        // saw of the memo.
        Color(self.nodes.color(node).load(Ordering::Acquire))
    }

    /// Raises the color of `node` to at least `color`.
    #[inline]
    pub(crate) fn paint(&self, node: u32, color: Color) {
        self.nodes.color(node).fetch_max(color.0, Ordering::AcqRel);
    }

    /// Makes the color of `key`, if it has a node, "unknown" in `revision`: its memo was removed.
    pub(crate) fn forget(&self, key: DatabaseKeyIndex, revision: Revision) {
        if let Some(node) = self.nodes.find(key) {
            self.paint(node, Color::unknown(revision));
        }
    }

    /// Starts loading the color of `node` into the cache, for deep verify a few edges ahead.
    #[inline(always)]
    pub(crate) fn prefetch_color(&self, node: u32) {
        prefetch::line(self.nodes.color(node));
    }
}

/// The one unsafe part: a prefetch hint by inline assembly (the stable intrinsics don't cover
/// aarch64).
mod prefetch {
    use crate::sync::atomic::AtomicU64;

    #[cfg(all(not(feature = "shuttle"), target_arch = "x86_64"))]
    #[inline(always)]
    pub(super) fn line(word: &AtomicU64) {
        // SAFETY: `prefetcht0` is a hint: it reads nothing into a register, changes no memory or
        // flags, and never faults, whatever the address; `word` is a live reference anyway.
        unsafe {
            std::arch::asm!(
                "prefetcht0 [{0}]",
                in(reg) word.as_ptr(),
                options(nostack, preserves_flags, readonly)
            );
        }
    }

    #[cfg(all(not(feature = "shuttle"), target_arch = "aarch64"))]
    #[inline(always)]
    pub(super) fn line(word: &AtomicU64) {
        // SAFETY: `prfm` is a hint: it changes no register, memory or flags and never faults,
        // whatever the address; `word` is a live reference anyway.
        unsafe {
            std::arch::asm!(
                "prfm pldl1keep, [{0}]",
                in(reg) word.as_ptr(),
                options(nostack, preserves_flags, readonly)
            );
        }
    }

    #[cfg(any(feature = "shuttle", not(any(target_arch = "x86_64", target_arch = "aarch64"))))]
    #[inline(always)]
    pub(super) fn line(word: &AtomicU64) {
        let _ = word;
    }
}

#[cfg(test)]
#[cfg(not(feature = "shuttle"))]
mod tests {
    use super::Color;
    use crate::Revision;
    use crate::graph::Graph;
    use crate::key::DatabaseKeyIndex;
    use crate::zalsa::IngredientIndex;
    use crate::Id;

    fn r(n: usize) -> Revision {
        Revision::from(n)
    }

    #[test]
    fn a_color_proves_unchanged_only_in_its_revision_and_after_its_change() {
        let color = Color::verified(r(5), r(3), false);
        assert!(color.unchanged_since(r(5), r(3)));
        assert!(color.unchanged_since(r(5), r(4)));
        assert!(!color.unchanged_since(r(5), r(2)), "changed after the reader");
        assert!(!color.unchanged_since(r(6), r(4)), "verified in an older revision");
        assert!(!Color::verified(r(5), r(1), true).unchanged_since(r(5), r(4)), "accumulated");
        assert!(!Color::unknown(r(5)).unchanged_since(r(5), r(4)));
        assert!(!Color(0).unchanged_since(r(1), r(1)), "never painted");
        let far = Color::verified(r(5), Revision::from(Color::CHANGED_LIMIT as usize), false);
        assert_eq!(far, Color::unknown(r(5)));
    }

    /// The order `fetch_max` keeps: a later revision, then accumulated values, then a later
    /// change, then "unknown".
    #[test]
    fn colors_rise_conservatively() {
        let order = [
            Color(0),
            Color::verified(r(4), r(4), true),
            Color::verified(r(5), r(1), false),
            Color::verified(r(5), r(3), false),
            Color::verified(r(5), r(1), true),
            Color::verified(r(5), r(3), true),
            Color::unknown(r(5)),
            Color::verified(r(6), r(1), false),
        ];
        for pair in order.windows(2) {
            assert!(pair[0].0 < pair[1].0, "{:?} < {:?}", pair[0], pair[1]);
        }
    }

    #[test]
    fn painting_keeps_the_highest_color_and_forgetting_makes_it_unknown() {
        let graph = Graph::default();
        // SAFETY: Test indices are below `Id::MAX_U32`.
        let key = DatabaseKeyIndex::new(IngredientIndex::new(3), unsafe { Id::from_index(9) });
        assert_eq!(graph.find_node(key), None);
        graph.forget(key, r(2));
        let node = graph.node(key);
        assert_eq!(graph.find_node(key), Some(node));
        assert_eq!(graph.color(node), Color(0));
        graph.paint(node, Color::verified(r(2), r(2), false));
        graph.paint(node, Color::verified(r(2), r(1), false));
        assert_eq!(graph.color(node), Color::verified(r(2), r(2), false));
        graph.prefetch_color(node);
        graph.forget(key, r(2));
        assert!(!graph.color(node).unchanged_since(r(2), r(2)));
        graph.paint(node, Color::verified(r(3), r(1), false));
        assert!(graph.color(node).unchanged_since(r(3), r(1)));
    }

    #[test]
    fn racing_painters_end_at_the_maximum() {
        let graph = Graph::default();
        // SAFETY: Test indices are below `Id::MAX_U32`.
        let key = DatabaseKeyIndex::new(IngredientIndex::new(1), unsafe { Id::from_index(1) });
        let node = graph.node(key);
        std::thread::scope(|scope| {
            for t in 0..8usize {
                let graph = &graph;
                scope.spawn(move || {
                    for i in 0..1000usize {
                        graph.paint(node, Color::verified(r(7), r(1 + (i * 7 + t) % 6), false));
                    }
                });
            }
        });
        assert_eq!(graph.color(node), Color::verified(r(7), r(6), false));
    }
}

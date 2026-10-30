//! Capturing what a piece of a query read, and replaying it into another query: rustc's
//! `with_cached_task` and `get_tracked`, which let one query reuse a result another computed (a
//! solved trait goal) and still rerun when what that result read changes.
//!
//! A capture frame is not a query. It belongs to the query active when it opens, whose identity,
//! cycle detection and tracked-struct disambiguation it uses, and every read made in it is a read
//! of that query as it happens. The frame notes which of the query's edges it read, as positions
//! in the query's edge set, so it hashes nothing the query did not already hash: a position at or
//! past the frame's `base` (the query's edge count when it opened) was first read while it was
//! open and is pushed once, when the query inserts it; a position below `base` is pushed the first
//! time the frame reads it again, told by a bitset over the positions below `base`.
//!
//! Frames nest. A closing frame's positions, stamp and verdict are merged into the frame around
//! it, so an inner frame's reads, and what a [`replay_reads`] appended in it, are reads of every
//! enclosing frame. A frame that may not be shared (see [`capture_reads`]) makes every enclosing
//! one unshareable too.
//!
//! A [`Reads`] is a range of the database's one edge array (`crate::graph`): the frame's edges in
//! read order, then, for each edge read with a value fingerprint, its index among them and the
//! fingerprint's four words. The words are written by the capturing task, as a query's edges are
//! (docs/v2/plan/design/concurrency.md, "Dependencies are recorded per task"), and live until the
//! next revision's compaction, which takes `&mut`; a `Reads` names its revision and is refused in
//! any other.

use std::fmt;

use crate::Revision;
use crate::database::Database;
use crate::durability::Durability;
use crate::fingerprint::Fingerprint;
use crate::graph::{EdgeSpan, Graph};
use crate::hash::FxIndexSet;
use crate::sync::atomic::Ordering;
use crate::zalsa::{Zalsa, ZalsaDatabase};
use crate::zalsa_local::{EdgeFingerprint, QueryEdge, ZalsaLocal};

/// Words a fingerprinted edge adds after the edges: its index among them, then the fingerprint.
const FINGERPRINT_WORDS: usize = 5;

/// What a [`capture_reads`] frame read: its edges and their fingerprints, the minimum durability
/// and maximum `changed_at` of what it read, and the revision it was captured in.
///
/// [`replay_reads`] makes another query depend on all of it. A `Reads` is valid only in the
/// revision it was captured in; keep shared results keyed by revision.
///
/// Send it to another thread only through something that synchronizes (a lock, a channel, or a
/// `Release` store read with `Acquire`), as any shared cache does: the edge words it names are
/// written with relaxed stores before it is returned.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Reads {
    span: EdgeSpan,
    /// How many words of `span` are edges; the rest are fingerprints.
    edges: u32,
    durability: Durability,
    accumulated: bool,
    revision: Revision,
    changed_at: Revision,
}

#[cfg(target_pointer_width = "64")]
const _: () = assert!(std::mem::size_of::<Reads>() == 32);
#[cfg(target_pointer_width = "64")]
const _: () = assert!(std::mem::size_of::<Option<Reads>>() == 32);

impl Reads {
    #[inline]
    pub(crate) fn durability(&self) -> Durability {
        self.durability
    }

    #[inline]
    pub(crate) fn changed_at(&self) -> Revision {
        self.changed_at
    }

    /// Whether an input read has accumulated values, so an accumulator walk must visit the
    /// reader.
    #[inline]
    pub(crate) fn accumulated(&self) -> bool {
        self.accumulated
    }

    /// The edges in read order, each with the fingerprint it was read with
    /// ([`Fingerprint::NONE`] for none).
    pub(crate) fn edges<'g>(
        &self,
        graph: &'g Graph,
    ) -> impl ExactSizeIterator<Item = (QueryEdge, Fingerprint)> + 'g {
        let (edges, tail) = graph.words(self.span).split_at(self.edges as usize);
        let mut fingerprints = tail.chunks_exact(FINGERPRINT_WORDS).peekable();
        edges.iter().enumerate().map(move |(index, word)| {
            // Relaxed: the capturing task wrote these before the `Reads` reached this one by a
            // synchronizing hand-off (see the type's docs).
            let edge = graph.decode(word.load(Ordering::Relaxed));
            let fingerprint = match fingerprints
                .next_if(|entry| entry[0].load(Ordering::Relaxed) == index as u32)
            {
                Some(entry) => Fingerprint::from_words([
                    entry[1].load(Ordering::Relaxed),
                    entry[2].load(Ordering::Relaxed),
                    entry[3].load(Ordering::Relaxed),
                    entry[4].load(Ordering::Relaxed),
                ]),
                None => Fingerprint::NONE,
            };
            (edge, fingerprint)
        })
    }
}

/// Runs `f` in a capture frame of the active query and returns what the frame read.
///
/// Every read in `f` is a read of the active query, exactly as if `f` ran without the frame. The
/// result is `None`, so what `f` computed must not be shared with another query, when `f`:
///
/// - read a provisional value or one with cycle heads (a fixpoint iteration's),
/// - created a tracked struct, specified a value, wrote an output or accumulated a value,
/// - made an untracked read,
/// - called [`unshareable_reads`] (directly or in a frame nested in this one), or
/// - ran with no active query.
///
/// Frames nest: the reads of a frame inside `f` are reads of this one too.
pub fn capture_reads<R>(db: &dyn Database, f: impl FnOnce() -> R) -> (R, Option<Reads>) {
    let (zalsa, zalsa_local) = db.zalsas();
    // SAFETY: The closure does not access the query stack.
    let depth = unsafe {
        zalsa_local.with_query_stack_unchecked_mut(|stack| {
            let depth = stack.len();
            stack.last_mut().map(|query| {
                query.open_capture();
                depth
            })
        })
    };
    let Some(depth) = depth else {
        return (f(), None);
    };
    let mut frame = OpenFrame {
        zalsa_local,
        depth,
        open: true,
    };
    let value = f();
    let reads = frame.close(zalsa);
    (value, reads)
}

/// Makes the active query depend on `reads` as if it had made them: their edges are added to the
/// query's (each once), with the fingerprints they were read with, and their durability and
/// `changed_at` are folded into the query's. Inside a [`capture_reads`] frame they are reads of
/// the frame too. With no active query it does nothing.
///
/// # Panics
///
/// If `reads` was captured in another revision.
pub fn replay_reads(db: &dyn Database, reads: Reads) {
    let (zalsa, zalsa_local) = db.zalsas();
    let current = zalsa.current_revision();
    assert!(
        reads.revision == current,
        "salsa: `replay_reads` of reads captured in revision {:?}, in revision {:?}: key a cache of \
         shared results by revision",
        reads.revision,
        current,
    );
    // SAFETY: The closure does not access the query stack.
    unsafe {
        zalsa_local.with_query_stack_unchecked_mut(|stack| {
            if let Some(query) = stack.last_mut() {
                query.replay(zalsa.graph(), &reads);
            }
        })
    }
}

/// Makes every [`capture_reads`] frame open in the active query return `None`: the caller used
/// something the query computed earlier, outside those frames, whose reads they did not make.
/// With no frame open it does nothing.
///
/// Frames of the queries below the active one on the task's stack are untouched: they depend on
/// the active query through its memo's edge, which carries every read it made.
pub fn unshareable_reads(db: &dyn Database) {
    let zalsa_local = db.zalsa_local();
    // SAFETY: The closure does not access the query stack.
    unsafe {
        zalsa_local.with_query_stack_unchecked_mut(|stack| {
            if let Some(query) = stack.last_mut() {
                query.captures_mut().unshareable();
            }
        })
    }
}

/// A frame [`capture_reads`] opened; dropping it unclosed (an unwind out of `f`) discards it.
struct OpenFrame<'a> {
    zalsa_local: &'a ZalsaLocal,
    /// The query stack's length when the frame opened: its query is the last there.
    depth: usize,
    open: bool,
}

impl OpenFrame<'_> {
    fn close(&mut self, zalsa: &Zalsa) -> Option<Reads> {
        self.open = false;
        let (zalsa_local, depth) = (self.zalsa_local, self.depth);
        // SAFETY: The closure does not access the query stack; `record_reads` uses the task's
        // graph, which is not part of it.
        unsafe {
            zalsa_local.with_query_stack_unchecked_mut(|stack| {
                assert_eq!(
                    stack.len(),
                    depth,
                    "salsa: a capture frame closed while a query it ran is still on the stack"
                );
                let query = stack
                    .last_mut()
                    .expect("salsa: a capture frame outlived its query");
                query.close_capture(zalsa, zalsa_local)
            })
        }
    }
}

impl Drop for OpenFrame<'_> {
    fn drop(&mut self) {
        if !self.open {
            return;
        }
        let depth = self.depth;
        // SAFETY: The closure does not access the query stack.
        unsafe {
            self.zalsa_local.with_query_stack_unchecked_mut(|stack| {
                if stack.len() == depth
                    && let Some(query) = stack.last_mut()
                {
                    query.captures_mut().discard();
                }
            })
        }
    }
}

/// The capture frames open in one query, innermost last, and buffers kept for the next ones.
#[derive(Default)]
pub(crate) struct Captures {
    open: Vec<Frame>,
    spare: Vec<Frame>,
    /// The fingerprint words of the frame being closed.
    tail: Vec<u32>,
}

struct Frame {
    /// The query's edge count when the frame opened.
    base: u32,
    /// Positions in the query's edge set that the frame read, in first-read order.
    read: Vec<u32>,
    /// Bits of the positions below `base` that `read` holds; all clear in a spare frame.
    old: Vec<u64>,
    durability: Durability,
    changed_at: Revision,
    shareable: bool,
    accumulated: bool,
}

impl fmt::Debug for Captures {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Captures")
            .field("open", &self.open.len())
            .finish()
    }
}

impl Frame {
    /// Pushes `position`, below `base`, unless the frame holds it.
    #[inline]
    fn read_old(&mut self, position: u32) {
        let (word, bit) = ((position / 64) as usize, 1u64 << (position % 64));
        if self.old.len() <= word {
            self.old.resize(self.base.div_ceil(64) as usize, 0);
        }
        if self.old[word] & bit == 0 {
            self.old[word] |= bit;
            self.read.push(position);
        }
    }

    /// Makes this frame's reads, stamp and verdict the enclosing frame's too.
    fn merge_into(&self, outer: &mut Frame) {
        outer.durability = outer.durability.min(self.durability);
        outer.changed_at = outer.changed_at.max(self.changed_at);
        outer.shareable &= self.shareable;
        outer.accumulated |= self.accumulated;
        for &position in &self.read {
            if position >= self.base {
                outer.read.push(position);
            } else if position < outer.base {
                outer.read_old(position);
            }
            // Otherwise the outer frame read it, directly or in a frame closed before.
        }
    }

    fn recycle(&mut self) {
        for &position in &self.read {
            if position < self.base {
                self.old[(position / 64) as usize] = 0;
            }
        }
        self.read.clear();
    }
}

impl Captures {
    #[inline(always)]
    pub(crate) fn is_open(&self) -> bool {
        !self.open.is_empty()
    }

    /// Opens a frame in a query with `base` edges so far.
    pub(crate) fn open(&mut self, base: usize) {
        let base = u32::try_from(base).expect("salsa: a query recorded 2^32 edges or more");
        let frame = match self.spare.pop() {
            Some(mut frame) => {
                frame.base = base;
                frame.durability = Durability::MAX;
                frame.changed_at = Revision::start();
                frame.shareable = true;
                frame.accumulated = false;
                frame
            }
            None => Frame {
                base,
                read: Vec::new(),
                old: Vec::new(),
                durability: Durability::MAX,
                changed_at: Revision::start(),
                shareable: true,
                accumulated: false,
            },
        };
        self.open.push(frame);
    }

    /// A read in the innermost frame: its durability and `changed_at`, and the position of its
    /// edge in the query's edge set with whether the query inserted it now, if it recorded one.
    #[inline]
    pub(crate) fn read(
        &mut self,
        durability: Durability,
        changed_at: Revision,
        edge: Option<(usize, bool)>,
    ) {
        let Some(frame) = self.open.last_mut() else {
            return;
        };
        frame.durability = frame.durability.min(durability);
        frame.changed_at = frame.changed_at.max(changed_at);
        if let Some((position, inserted)) = edge {
            // A query's edges are fewer than 2^31 (`Graph::record` refuses more).
            let position = position as u32;
            if inserted {
                frame.read.push(position);
            } else if position < frame.base {
                frame.read_old(position);
            }
        }
    }

    /// A `changed_at` folded in without an edge.
    #[inline]
    pub(crate) fn changed_at(&mut self, changed_at: Revision) {
        if let Some(frame) = self.open.last_mut() {
            frame.changed_at = frame.changed_at.max(changed_at);
        }
    }

    /// An input read in the innermost frame has accumulated values.
    #[inline]
    pub(crate) fn accumulated_inputs(&mut self) {
        if let Some(frame) = self.open.last_mut() {
            frame.accumulated = true;
        }
    }

    /// Every open frame must not be shared.
    #[inline]
    pub(crate) fn unshareable(&mut self) {
        for frame in &mut self.open {
            frame.shareable = false;
        }
    }

    /// Closes the innermost frame, merging it into the one around it, and returns its reads if
    /// it may be shared. `edges` and `fingerprints` are the query's edge set and the value
    /// fingerprints of its edges, sorted by position.
    pub(crate) fn close(
        &mut self,
        zalsa: &Zalsa,
        zalsa_local: &ZalsaLocal,
        edges: &FxIndexSet<QueryEdge>,
        fingerprints: &[EdgeFingerprint],
    ) -> Option<Reads> {
        let mut inner = self
            .open
            .pop()
            .expect("salsa: closed a capture frame that is not open");
        if let Some(outer) = self.open.last_mut() {
            inner.merge_into(outer);
        }
        let reads = inner.shareable.then(|| {
            self.tail.clear();
            if !fingerprints.is_empty() {
                for (index, &position) in inner.read.iter().enumerate() {
                    if let Ok(at) = fingerprints.binary_search_by_key(&position, |entry| entry.edge)
                    {
                        let [a, b, c, d] = fingerprints[at].fingerprint.words();
                        self.tail.extend_from_slice(&[index as u32, a, b, c, d]);
                    }
                }
            }
            let span = zalsa_local.record_reads(
                zalsa,
                inner.read.iter().map(|&position| {
                    *edges
                        .get_index(position as usize)
                        .expect("salsa: a capture frame read a position past the query's edges")
                }),
                &self.tail,
            );
            Reads {
                span,
                edges: inner.read.len() as u32,
                durability: inner.durability,
                accumulated: inner.accumulated,
                revision: zalsa.current_revision(),
                changed_at: inner.changed_at,
            }
        });
        inner.recycle();
        self.spare.push(inner);
        reads
    }

    /// Drops the innermost frame, which `f` unwound out of. Its reads stay the query's, but the
    /// frames around it no longer know them, so none of them may be shared.
    pub(crate) fn discard(&mut self) {
        if let Some(mut frame) = self.open.pop() {
            frame.recycle();
            self.spare.push(frame);
        }
        self.unshareable();
    }

    /// Drops every open frame, as the query they belong to ends.
    pub(crate) fn clear(&mut self) {
        while let Some(mut frame) = self.open.pop() {
            frame.recycle();
            self.spare.push(frame);
        }
    }
}

#[cfg(all(test, not(feature = "shuttle")))]
mod tests {
    use super::Captures;
    use crate::Revision;
    use crate::durability::Durability;

    fn read(captures: &mut Captures, position: usize, inserted: bool) {
        captures.read(Durability::LOW, Revision::start(), Some((position, inserted)));
    }

    fn positions(captures: &Captures) -> Vec<u32> {
        captures.open.last().unwrap().read.clone()
    }

    /// The safe reference: a frame's positions are the distinct positions read while it was open,
    /// in first-read order, whatever the query had read before.
    #[test]
    fn a_frame_holds_each_position_it_read_once_in_order() {
        let mut captures = Captures::default();
        captures.open(3);
        read(&mut captures, 3, true);
        read(&mut captures, 1, false);
        read(&mut captures, 3, false);
        read(&mut captures, 1, false);
        read(&mut captures, 4, true);
        read(&mut captures, 0, false);
        assert_eq!(positions(&captures), [3, 1, 4, 0]);
    }

    #[test]
    fn an_inner_frame_merges_into_the_outer_in_order() {
        let mut captures = Captures::default();
        captures.open(2);
        read(&mut captures, 2, true);
        read(&mut captures, 0, false);
        captures.open(3);
        read(&mut captures, 0, false);
        read(&mut captures, 2, false);
        read(&mut captures, 1, false);
        read(&mut captures, 3, true);
        captures.changed_at(Revision::from(7));
        assert_eq!(positions(&captures), [0, 2, 1, 3]);
        let mut inner = captures.open.pop().unwrap();
        inner.merge_into(captures.open.last_mut().unwrap());
        inner.recycle();
        assert!(inner.old.iter().all(|&word| word == 0));
        assert_eq!(positions(&captures), [2, 0, 1, 3]);
        assert_eq!(captures.open[0].changed_at, Revision::from(7));
    }

    #[test]
    fn unshareable_and_discard_reach_every_open_frame() {
        let mut captures = Captures::default();
        captures.open(0);
        captures.open(0);
        captures.discard();
        assert!(!captures.open[0].shareable);
        captures.clear();
        assert!(!captures.is_open());
        captures.open(0);
        assert!(captures.open[0].shareable);
        assert_eq!(captures.spare.len(), 1);
    }
}

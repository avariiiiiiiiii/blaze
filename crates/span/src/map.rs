//! A map that maps a span to every position in a file. Usually maps a span to some range of positions.
//! Allows bidirectional lookup.

use std::{fmt, hash::Hash};

use blaze_format::Spans;
use blaze_ids::ContentId;
use stdx::{always, itertools::Itertools};

use crate::{
    EditionedFileId, ErasedFileAstId, ROOT_ERASED_FILE_AST_ID, Span, SyntaxContext, TextRange,
    TextSize,
};

/// Maps absolute text ranges for the corresponding file to the relevant span data.
#[derive(Debug, PartialEq, Eq, Clone, Hash)]
pub struct SpanMap {
    /// The offset stored here is the *end* of the node.
    spans: Vec<(TextSize, Span)>,
    /// Index of the matched macro arm on successful expansion for declarative macros.
    // FIXME: Does it make sense to have this here?
    pub matched_arm: Option<u32>,
    /// Where the macro fragments start and where they end, each in order. A fragment is one
    /// token tree to the macro it is passed on to, which the text does not show.
    fragment_starts: Vec<TextSize>,
    fragment_ends: Vec<TextSize>,
}

/// How many of the sorted `offsets` are `offset`.
fn count_of(offsets: &[TextSize], offset: TextSize) -> usize {
    let first = offsets.partition_point(|&it| it < offset);
    offsets[first..].partition_point(|&it| it == offset)
}

impl SpanMap {
    /// Creates a new empty [`SpanMap`].
    pub fn empty() -> Self {
        Self {
            spans: Vec::new(),
            matched_arm: None,
            fragment_starts: Vec::new(),
            fragment_ends: Vec::new(),
        }
    }

    /// Finalizes the [`SpanMap`], shrinking its backing storage and validating that the offsets are
    /// in order.
    pub fn finish(&mut self) {
        always!(
            self.spans.iter().array_windows().all(|[a, b]| a.0 < b.0),
            "spans are not in order"
        );
        self.spans.shrink_to_fit();
        self.fragment_starts.sort_unstable();
        self.fragment_ends.sort_unstable();
    }

    /// Pushes a new span onto the [`SpanMap`].
    pub fn push(&mut self, offset: TextSize, span: Span) {
        if cfg!(debug_assertions)
            && let Some(&(last_offset, _)) = self.spans.last()
        {
            assert!(
                last_offset < offset,
                "last_offset({last_offset:?}) must be smaller than offset({offset:?})"
            );
        }
        self.spans.push((offset, span));
    }

    /// Records that the tokens of `range` are one macro fragment.
    pub fn push_fragment(&mut self, range: TextRange) {
        self.fragment_starts.push(range.start());
        self.fragment_ends.push(range.end());
    }

    /// How many macro fragments start at `offset`.
    pub fn fragments_starting_at(&self, offset: TextSize) -> usize {
        count_of(&self.fragment_starts, offset)
    }

    /// How many macro fragments end at `offset`.
    pub fn fragments_ending_at(&self, offset: TextSize) -> usize {
        count_of(&self.fragment_ends, offset)
    }

    /// Returns all [`TextRange`]s that correspond to the given span.
    ///
    /// Note this does a linear search through the entire backing vector.
    pub fn ranges_with_span_exact(
        &self,
        span: Span,
    ) -> impl Iterator<Item = (TextRange, SyntaxContext)> + '_ {
        self.spans.iter().enumerate().filter_map(move |(idx, &(end, s))| {
            if !s.eq_ignoring_ctx(span) {
                return None;
            }
            let start = idx.checked_sub(1).map_or(TextSize::new(0), |prev| self.spans[prev].0);
            Some((TextRange::new(start, end), s.ctx))
        })
    }

    /// Returns all [`TextRange`]s whose spans contain the given span.
    ///
    /// Note this does a linear search through the entire backing vector.
    pub fn ranges_with_span(
        &self,
        span: Span,
    ) -> impl Iterator<Item = (TextRange, SyntaxContext)> + '_ {
        self.spans.iter().enumerate().filter_map(move |(idx, &(end, s))| {
            if s.anchor != span.anchor {
                return None;
            }
            if !s.range.contains_range(span.range) {
                return None;
            }
            let start = idx.checked_sub(1).map_or(TextSize::new(0), |prev| self.spans[prev].0);
            Some((TextRange::new(start, end), s.ctx))
        })
    }

    /// Returns the span at the given position.
    pub fn span_at(&self, offset: TextSize) -> Span {
        let entry = self.spans.partition_point(|&(it, _)| it <= offset);
        self.spans[entry].1
    }

    /// Returns the spans associated with the given range.
    /// In other words, this will return all spans that correspond to all offsets within the given range.
    pub fn spans_for_range(&self, range: TextRange) -> impl Iterator<Item = Span> + '_ {
        let (start, end) = (range.start(), range.end());
        let start_entry = self.spans.partition_point(|&(it, _)| it <= start);
        let end_entry = self.spans[start_entry..].partition_point(|&(it, _)| it <= end); // FIXME: this might be wrong?
        self.spans[start_entry..][..end_entry].iter().map(|&(_, s)| s)
    }

    pub fn iter(&self) -> impl Iterator<Item = (TextSize, Span)> + '_ {
        self.spans.iter().copied()
    }

    /// Merges this span map with another span map, where `other` is inserted at (and replaces) `other_range`.
    ///
    /// The length of the replacement node needs to be `other_size`.
    pub fn merge(&mut self, other_range: TextRange, other_size: TextSize, other: &SpanMap) {
        // I find the following diagram helpful to illustrate the bounds and why we use `<` or `<=`:
        // --------------------------------------------------------------------
        //   1   3   5   6   7   10    11          <-- offsets we store
        // 0-1 1-3 3-5 5-6 6-7 7-10 10-11          <-- ranges these offsets refer to
        //       3   ..      7                     <-- other_range
        //         3-5 5-6 6-7                     <-- ranges we replace (len = 7-3 = 4)
        //         ^^^^^^^^^^^ ^^^^^^^^^^
        //           remove       shift
        //   2   3   5   9                         <-- offsets we insert
        // 0-2 2-3 3-5 5-9                         <-- ranges we insert (other_size = 9-0 = 9)
        // ------------------------------------
        //   1   3
        // 0-1 1-3                                 <-- these remain intact
        //           5   6   8   12
        //         3-5 5-6 6-8 8-12                <-- we shift these by other_range.start() and insert them
        //                             15    16
        //                          12-15 15-16    <-- we shift these by other_size-other_range.len() = 9-4 = 5
        // ------------------------------------
        //   1   3   5   6   8   12    15    16    <-- final offsets we store
        // 0-1 1-3 3-5 5-6 6-8 8-12 12-15 15-16    <-- final ranges

        self.spans.retain_mut(|(offset, _)| {
            if other_range.start() < *offset && *offset <= other_range.end() {
                false
            } else {
                if *offset > other_range.end() {
                    *offset += other_size;
                    *offset -= other_range.len();
                }
                true
            }
        });

        self.spans
            .extend(other.spans.iter().map(|&(offset, span)| (offset + other_range.start(), span)));

        self.spans.sort_unstable_by_key(|&(offset, _)| offset);

        // A fragment that starts where the replaced text ends comes after it, one that ends
        // where it starts comes before it.
        let shift = |offset: &mut TextSize| *offset = *offset + other_size - other_range.len();
        self.fragment_starts.retain_mut(|start| {
            if other_range.contains(*start) {
                return false;
            }
            if *start >= other_range.end() {
                shift(start);
            }
            true
        });
        self.fragment_ends.retain_mut(|end| {
            if other_range.start() < *end && *end <= other_range.end() {
                return false;
            }
            if *end > other_range.end() {
                shift(end);
            }
            true
        });
        let moved = |offsets: &[TextSize]| {
            offsets.iter().map(|&offset| offset + other_range.start()).collect::<Vec<_>>()
        };
        self.fragment_starts.extend(moved(&other.fragment_starts));
        self.fragment_ends.extend(moved(&other.fragment_ends));
        self.fragment_starts.sort_unstable();
        self.fragment_ends.sort_unstable();

        // Matched arm info is no longer correct once we have multiple macros.
        self.matched_arm = None;
    }
}

#[cfg(not(no_salsa_async_drops))]
impl Drop for SpanMap {
    fn drop(&mut self) {
        let spans = std::mem::take(&mut self.spans);
        static SPAN_MAP_DROP_THREAD: std::sync::OnceLock<
            std::sync::mpsc::Sender<Vec<(TextSize, Span)>>,
        > = std::sync::OnceLock::new();

        SPAN_MAP_DROP_THREAD
            .get_or_init(|| {
                let (sender, receiver) = std::sync::mpsc::channel::<Vec<(TextSize, Span)>>();
                std::thread::Builder::new()
                    .name("SpanMapDropper".to_owned())
                    .spawn(move || {
                        loop {
                            // block on a receive
                            _ = receiver.recv();
                            // then drain the entire channel
                            while receiver.try_recv().is_ok() {}
                            // and sleep for a bit
                            std::thread::sleep(std::time::Duration::from_millis(100));
                        }
                        // why do this over just a `receiver.iter().for_each(drop)`? To reduce contention on the channel lock.
                        // otherwise this thread will constantly wake up and sleep again.
                    })
                    .unwrap();
                sender
            })
            .send(spans)
            .unwrap();
    }
}

/// The anchors of a real file's spans: the items whose ast ids the spans of the text from where
/// each begins up to the next one are relative to.
///
/// What it is (what is compared and hashed) is each anchor with the length of its text, not
/// where that text is: no position in the file. The positions lookups take are summed from
/// the lengths when the map is made.
#[derive(Debug)]
pub struct RealSpanMap {
    file_id: EditionedFileId,
    /// Each anchor and the length of its text, in the order of the text; the first begins at 0.
    anchors: Box<[(ErasedFileAstId, TextSize)]>,
    /// Where each anchor's text begins: the lengths before it, summed.
    starts: Box<[TextSize]>,
}

impl PartialEq for RealSpanMap {
    fn eq(&self, other: &RealSpanMap) -> bool {
        self.file_id == other.file_id && self.anchors == other.anchors
    }
}

impl Eq for RealSpanMap {}

impl Hash for RealSpanMap {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.file_id.hash(state);
        self.anchors.hash(state);
    }
}

impl fmt::Display for RealSpanMap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "RealSpanMap({:?}):", self.file_id)?;
        for (start, (ast_id, _)) in self.starts.iter().zip(&self.anchors) {
            writeln!(f, "{}: {:#?}", u32::from(*start), ast_id)?;
        }
        Ok(())
    }
}

impl RealSpanMap {
    /// Creates a real file span map that returns absolute ranges (relative ranges to the root ast id).
    pub fn absolute(file_id: EditionedFileId) -> Self {
        RealSpanMap {
            file_id,
            anchors: Box::from([(ROOT_ERASED_FILE_AST_ID, TextSize::new(!0))]),
            starts: Box::from([TextSize::new(0)]),
        }
    }

    /// The map of a file of `end` bytes whose anchors begin at `pairs`' offsets, in order,
    /// the first at 0.
    pub fn from_file(
        file_id: EditionedFileId,
        pairs: Box<[(TextSize, ErasedFileAstId)]>,
        end: TextSize,
    ) -> Self {
        always!(
            pairs.first().is_some_and(|it| it.0 == TextSize::new(0))
                && pairs.windows(2).all(|it| it[0].0 <= it[1].0)
                && pairs.last().is_some_and(|it| it.0 <= end),
            "the anchors of a file begin at 0, in order, within it"
        );
        let anchors = pairs
            .iter()
            .enumerate()
            .map(|(i, &(start, ast_id))| {
                let next = pairs.get(i + 1).map_or(end, |it| it.0);
                (ast_id, next.checked_sub(start).unwrap_or_default())
            })
            .collect();
        let starts = pairs.iter().map(|&(start, _)| start).collect();
        RealSpanMap { file_id, anchors, starts }
    }

    /// Where the file ends: past the last anchor's text.
    fn end(&self) -> TextSize {
        let last = self.anchors.len() - 1;
        self.starts[last].checked_add(self.anchors[last].1).unwrap_or(TextSize::new(!0))
    }

    /// The number of the anchor of what begins at `offset`: the last to begin at or before it.
    fn index_at(&self, offset: TextSize) -> usize {
        let after = self.starts.partition_point(|&it| it <= offset);
        after.checked_sub(1).unwrap_or_else(|| panic!("no anchor begins at or before {offset:?}"))
    }

    /// The anchor that the spans of what begins at `offset` are relative to, and where it begins.
    pub fn anchor_at(&self, offset: TextSize) -> (TextSize, ErasedFileAstId) {
        let idx = self.index_at(offset);
        (self.starts[idx], self.anchors[idx].0)
    }

    /// The map of `range` alone, as a file of its own: the spans it gives what is in `range`,
    /// moved to begin at 0, are those this map gives it where it is. `range` has to begin with
    /// an anchor ([`RealSpanMap::anchor_at`]).
    pub fn within(&self, range: TextRange) -> RealSpanMap {
        let pairs = self
            .starts
            .iter()
            .zip(&self.anchors)
            .filter(|(offset, _)| range.contains(**offset))
            .map(|(&offset, &(ast_id, _))| (offset - range.start(), ast_id))
            .collect::<Box<[_]>>();
        debug_assert_eq!(pairs.first().map(|it| it.0), Some(TextSize::new(0)));
        RealSpanMap::from_file(self.file_id, pairs, range.len())
    }

    #[cfg(feature = "salsa")]
    pub fn span_for_range(&self, range: TextRange) -> Span {
        let end = self.end();
        assert!(range.end() <= end, "range {range:?} goes beyond the end of the file {end:?}");
        let (offset, ast_id) = self.anchor_at(range.start());
        Span {
            range: range - offset,
            anchor: crate::SpanAnchor { file_id: self.file_id, ast_id },
            ctx: SyntaxContext::root(self.file_id.edition()),
        }
    }

    /// `ranges`, of this map's text at or after where anchor `ast_id` begins, as the side table
    /// of the anchor's spans (docs/v2/format.md section 5, "Spans"), each relative to where the
    /// anchor begins. `anchor` is the content id of the anchor's item, which names the table;
    /// the span of the `i`th range is the table's `i`th, `SpanRef { anchor, index: i }`.
    ///
    /// # Panics
    ///
    /// When `ast_id` is not an anchor that begins at or before the first range, or a range
    /// begins before it or ends past the end of the text.
    pub fn write_spans(
        &self,
        anchor: ContentId,
        ast_id: ErasedFileAstId,
        ranges: impl IntoIterator<Item = TextRange>,
    ) -> Spans {
        let mut spans = Spans::new(anchor);
        let mut found = None;
        let end = self.end();
        for range in ranges {
            let begins = *found.get_or_insert_with(|| self.start_of(ast_id, range.start()));
            assert!(
                begins <= range.start() && range.end() <= end,
                "{range:?} is not in the text after {ast_id:?} begins at {begins:?} in {:?}",
                self.file_id
            );
            spans.push(u32::from(range.start() - begins), u32::from(range.len()));
        }
        spans
    }

    /// Where anchor `ast_id` begins, at or before `offset`.
    fn start_of(&self, ast_id: ErasedFileAstId, offset: TextSize) -> TextSize {
        let last = self.index_at(offset);
        let at = (0..=last).rev().find(|&it| self.anchors[it].0 == ast_id);
        let at = at.unwrap_or_else(|| {
            panic!("{ast_id:?} is no anchor at or before {offset:?} in {:?}", self.file_id)
        });
        self.starts[at]
    }
}

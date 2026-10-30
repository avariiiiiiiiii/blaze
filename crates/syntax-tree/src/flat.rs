//! A syntax tree as arrays, for trees nobody edits.
//!
//! Every node and token is an element, numbered in preorder. Nothing is allocated per element,
//! and walking is stepping through the arrays. The cursor ([`crate::cursor`]) serves these
//! trees through the same API as green ones, and makes a green tree of a part only when asked
//! for one; a walk that reads many elements reads them here, by number, and makes a cursor
//! only of what it keeps ([`crate::cursor::SyntaxNode::flat`]).
//!
//! What an element costs, about 8.5 bytes:
//!
//! - one `u32` word: its kind in the low 16 bits, whether it is a node, a token or a lazy node
//!   in the next 2, and in the top 14 a token's or lazy node's length in bytes or a node's
//!   subtree size in elements. A value of 2^14 - 1 or more is kept in a side table sorted by
//!   element (8 bytes an entry), which a file has few entries in: tokens of 16 KiB or more, and
//!   nodes of more than 16 thousand elements, which are the few ancestors of large parts;
//! - its parent's number, a `u32`: ancestors, `index` and the sibling moves step up, and
//!   finding a parent from the words alone would be a scan back over its earlier children;
//! - an eighth of a `u32`: the start of every eighth element, from which an element's start
//!   is that plus the lengths of up to seven elements before it, so [`FlatTree::range`] reads
//!   at most fifteen words. The binary searches of the child at a range search these starts
//!   and then step through at most eight elements, so they need no column of their own.
//!
//! That is against 14 bytes for a kind, a start, an end and a parent each, and no copy of the
//! text: a tree's ranges index a source it shares ([`FlatTree::source`]), the whole file's for
//! a file's tree and its parts.
//!
//! A lazy node is a node whose children are not in the arrays (a body an outline parse left
//! unparsed): its text is parsed by the tree's [`Expand`] the first time its part is asked for
//! ([`FlatTree::part`]), once, whichever thread asks. The arrays show a lazy node as a node
//! without children, so array walks never parse; the cursor walks into parts as if they were
//! in the arrays.

use std::{
    fmt,
    hash::{Hash, Hasher},
    sync::{Arc, OnceLock},
};

use crate::{GreenNode, GreenNodeBuilder, NodeCache, SyntaxKind, TextRange, TextSize};

/// Every how many elements a start is kept.
const MARK: u32 = 8;
const NO_PARENT: u32 = u32::MAX;
const TAG: u32 = 0b11 << 16;
const NODE: u32 = 0;
const TOKEN: u32 = 1 << 16;
const LAZY: u32 = 2 << 16;
const SHIFT: u32 = 18;
/// The payload of an element whose length or size is in the side table.
const BIG: u32 = u32::MAX >> SHIFT;

/// What parses a lazy node: called with the node's kind, its text and the tree's `data`, it
/// gives a tree whose root is a node of that kind over exactly that text.
pub type Expand = fn(kind: SyntaxKind, text: &str, data: u32) -> FlatTree;

#[derive(Clone)]
pub struct FlatTree {
    source: Arc<str>,
    words: Box<[u32]>,
    /// The parent's number, [`NO_PARENT`] for the root.
    parents: Box<[u32]>,
    /// The start of every [`MARK`]th element, and of the end when the elements are a multiple
    /// of it: `len / MARK + 1` of them.
    marks: Box<[u32]>,
    /// The lengths and sizes that do not fit a word, by element.
    big: Box<[(u32, u32)]>,
    lazy: Option<Box<Lazy>>,
}

/// A tree's lazy nodes, in preorder, and their parts.
#[derive(Clone)]
struct Lazy {
    nodes: Box<[u32]>,
    parts: Box<[OnceLock<Arc<FlatTree>>]>,
    expand: Expand,
    data: u32,
}

impl fmt::Debug for FlatTree {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FlatTree")
            .field("elements", &self.words.len())
            .field("range", &self.range(0))
            .field("lazies", &self.lazy.as_ref().map_or(0, |it| it.nodes.len()))
            .finish()
    }
}

/// Two trees are equal when they are the same elements over the same text with the same
/// expander's data; which parts were parsed does not matter.
impl PartialEq for FlatTree {
    fn eq(&self, other: &FlatTree) -> bool {
        self.words == other.words
            && self.big == other.big
            && self.marks.first() == other.marks.first()
            && self.lazy.as_ref().map(|it| it.data) == other.lazy.as_ref().map(|it| it.data)
            && (Arc::ptr_eq(&self.source, &other.source) || self.source == other.source)
    }
}

impl Eq for FlatTree {}

impl Hash for FlatTree {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.source.hash(state);
        self.words.hash(state);
        self.big.hash(state);
        self.marks.first().hash(state);
        self.lazy.as_ref().map(|it| it.data).hash(state);
    }
}

#[inline]
fn pack(kind: SyntaxKind, tag: u32, value: u32, at: u32, big: &mut Vec<(u32, u32)>) -> u32 {
    let payload = match value < BIG {
        true => value,
        false => {
            big.push((at, value));
            BIG
        }
    };
    kind.0 as u32 | tag | payload << SHIFT
}

/// The starts [`FlatTree::marks`] keeps of `words`, the first element starting at `base`.
fn marks(words: &[u32], big: &[(u32, u32)], base: u32) -> Box<[u32]> {
    let mut marks = Vec::with_capacity(words.len() / MARK as usize + 1);
    let mut big = big.iter();
    let mut offset = base;
    for (at, &word) in words.iter().enumerate() {
        if at % MARK as usize == 0 {
            marks.push(offset);
        }
        if word & TAG != NODE {
            offset += match word >> SHIFT {
                BIG => big.find(|it| it.0 == at as u32).expect("a length in the side table").1,
                len => len,
            };
        }
    }
    if words.len() % MARK as usize == 0 {
        marks.push(offset);
    }
    marks.into_boxed_slice()
}

/// `part`, parsed or not, as the part of a lazy node whose text moved from `from` to `to`,
/// in `source`.
fn moved_part(
    part: &OnceLock<Arc<FlatTree>>,
    source: &Arc<str>,
    from: u32,
    to: u32,
) -> OnceLock<Arc<FlatTree>> {
    let moved = OnceLock::new();
    if let Some(part) = part.get() {
        let _ = moved.set(Arc::new(FlatTree::clone(part).moved(source.clone(), from, to)));
    }
    moved
}

impl FlatTree {
    /// How many nodes and tokens the tree has; the root is number 0.
    #[inline]
    pub fn len(&self) -> u32 {
        self.words.len() as u32
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    /// The text the tree's ranges index: [`Self::source`].
    #[inline]
    pub fn text(&self) -> &str {
        &self.source
    }

    /// The text the tree's ranges index: the whole file's source for a file's tree and its
    /// parts, the tree's own text for one built by appending text.
    #[inline]
    pub fn source(&self) -> &Arc<str> {
        &self.source
    }

    /// The bytes the tree holds besides itself: its arrays and the parts parsed so far, not
    /// the source it shares.
    pub fn heap_size(&self) -> usize {
        let lazy = self.lazy.as_ref().map_or(0, |lazy| {
            lazy.nodes.len() * 4
                + lazy.parts.len() * std::mem::size_of::<OnceLock<Arc<FlatTree>>>()
                + lazy.parts.iter().filter_map(|it| it.get()).map(|it| it.heap_size()).sum::<usize>()
        });
        (self.words.len() + self.parents.len() + self.marks.len()) * 4 + self.big.len() * 8 + lazy
    }

    #[inline]
    pub fn kind(&self, at: u32) -> SyntaxKind {
        SyntaxKind(self.words[at as usize] as u16)
    }

    #[inline]
    pub fn is_token(&self, at: u32) -> bool {
        self.words[at as usize] & TAG == TOKEN
    }

    /// Whether `at` is a lazy node: a node whose children are in its part, not the arrays.
    #[inline]
    pub fn is_lazy(&self, at: u32) -> bool {
        self.words[at as usize] & TAG == LAZY
    }

    #[inline]
    pub fn parent(&self, at: u32) -> Option<u32> {
        let parent = self.parents[at as usize];
        (parent != NO_PARENT).then_some(parent)
    }

    /// A token's or lazy node's length, a node's subtree size.
    #[inline]
    fn value(&self, at: u32) -> u32 {
        match self.words[at as usize] >> SHIFT {
            BIG => self.big_value(at),
            value => value,
        }
    }

    #[cold]
    fn big_value(&self, at: u32) -> u32 {
        match self.big.binary_search_by_key(&at, |it| it.0) {
            Ok(it) => self.big[it].1,
            Err(_) => unreachable!("element {at}'s length or size is not in the side table"),
        }
    }

    /// The bytes of text the element holds itself: a token's or lazy node's, none for a node.
    #[inline]
    fn text_len(&self, at: u32) -> u32 {
        match self.words[at as usize] & TAG == NODE {
            true => 0,
            false => self.value(at),
        }
    }

    /// The number after the element's subtree.
    #[inline]
    pub fn end(&self, at: u32) -> u32 {
        match self.words[at as usize] & TAG == NODE {
            true => at + self.value(at),
            false => at + 1,
        }
    }

    /// The children and tokens of `at`, in order.
    #[inline]
    pub fn children_with_tokens(&self, at: u32) -> impl Iterator<Item = u32> + '_ {
        let end = self.end(at);
        let mut next = at + 1;
        std::iter::from_fn(move || {
            let it = next;
            (it < end).then(|| {
                next = self.end(it);
                it
            })
        })
    }

    /// The children of `at` that are nodes, in order.
    #[inline]
    pub fn children(&self, at: u32) -> impl Iterator<Item = u32> + '_ {
        self.children_with_tokens(at).filter(|&it| !self.is_token(it))
    }

    /// `at` and the nodes under it, in preorder.
    #[inline]
    pub fn descendants(&self, at: u32) -> impl Iterator<Item = u32> + '_ {
        (at..self.end(at)).filter(|&it| !self.is_token(it))
    }

    /// Hashes what the subtree of `at` is made of, as the same subtree is hashed anywhere in
    /// any tree: its text, and its elements' kinds and shape, numbered from `at`. A lazy node
    /// is hashed as its kind and text, so an outline's hash is not a full tree's.
    pub fn hash_subtree<H: Hasher>(&self, at: u32, state: &mut H) {
        let end = self.end(at);
        self.source[self.range(at)].hash(state);
        self.words[at as usize..end as usize].hash(state);
        for &(it, value) in self.big_in(at, end) {
            (it - at, value).hash(state);
        }
    }

    /// The side table's entries of the elements from `lo` to `hi`.
    fn big_in(&self, lo: u32, hi: u32) -> &[(u32, u32)] {
        let from = self.big.partition_point(|it| it.0 < lo);
        let to = self.big.partition_point(|it| it.0 < hi);
        &self.big[from..to]
    }

    /// Where element `at` starts; `len()` for where the last ends.
    #[inline]
    fn offset(&self, at: u32) -> u32 {
        let mark = at / MARK;
        let mut offset = self.marks[mark as usize];
        for it in mark * MARK..at {
            offset += self.text_len(it);
        }
        offset
    }

    #[inline]
    pub fn range(&self, at: u32) -> TextRange {
        let start = self.offset(at);
        let end = match self.words[at as usize] & TAG == NODE {
            false => start + self.value(at),
            true => {
                let end = self.end(at);
                match end / MARK == at / MARK {
                    true => (at..end).fold(start, |offset, it| offset + self.text_len(it)),
                    false => self.offset(end),
                }
            }
        };
        TextRange::new(TextSize::from(start), TextSize::from(end))
    }

    #[inline]
    pub fn token_text(&self, at: u32) -> &str {
        &self.source[self.range(at)]
    }

    /// The first element from `lo` to `hi` whose start `pred` refuses, `pred` accepting the
    /// starts up to some offset and none after: a binary search of the kept starts, then at
    /// most [`MARK`] steps.
    fn partition_point(&self, lo: u32, hi: u32, pred: impl Fn(u32) -> bool) -> u32 {
        let first = lo.div_ceil(MARK);
        let last = hi.div_ceil(MARK).max(first);
        let n = self.marks[first as usize..last as usize].partition_point(|&it| pred(it)) as u32;
        let (mut it, mut offset) = match n {
            0 => (lo, self.offset(lo)),
            n => ((first + n - 1) * MARK, self.marks[(first + n - 1) as usize]),
        };
        while it < hi && pred(offset) {
            offset += self.text_len(it);
            it += 1;
        }
        it
    }

    /// The last of `at` and the elements under it to start at or before `offset`.
    #[inline]
    pub(crate) fn last_starting(&self, at: u32, offset: TextSize) -> Option<u32> {
        let offset = u32::from(offset);
        let n = self.partition_point(at, self.end(at), |it| it <= offset);
        (n > at).then(|| n - 1)
    }

    #[inline]
    pub(crate) fn first_child_or_token(&self, at: u32) -> Option<u32> {
        (self.end(at) > at + 1).then_some(at + 1)
    }

    #[inline]
    pub(crate) fn next_sibling_or_token(&self, at: u32) -> Option<u32> {
        let parent = self.parent(at)?;
        let next = self.end(at);
        (next < self.end(parent)).then_some(next)
    }

    /// The child of `parent` that `at` is in. Up from `at` is as many steps as `at` is deep
    /// below `parent`, which an `else if` chain makes as many as it has arms, and across
    /// `parent`'s children as many as come before it, which a long block makes many: a step
    /// of each at a time takes the fewer.
    #[inline]
    fn child_containing(&self, parent: u32, at: u32) -> u32 {
        let (mut up, mut across) = (at, parent + 1);
        loop {
            match self.parent(up) {
                Some(it) if it != parent => up = it,
                _ => return up,
            }
            if at < self.end(across) {
                return across;
            }
            across = self.end(across);
        }
    }

    #[inline]
    pub(crate) fn prev_sibling_or_token(&self, at: u32) -> Option<u32> {
        let parent = self.parent(at)?;
        (at > parent + 1).then(|| self.child_containing(parent, at - 1))
    }

    #[inline]
    pub(crate) fn last_child_or_token(&self, at: u32) -> Option<u32> {
        self.first_child_or_token(at).map(|_| self.child_containing(at, self.end(at) - 1))
    }

    /// The element's position among its parent's children and tokens.
    pub(crate) fn index(&self, at: u32) -> usize {
        let Some(parent) = self.parent(at) else { return 0 };
        let mut child = parent + 1;
        let mut index = 0;
        while child != at {
            child = self.end(child);
            index += 1;
        }
        index
    }

    /// The first child node of `at` from `from` on, `from` being `at`'s child or its end.
    #[inline]
    fn node_from(&self, at: u32, mut from: u32) -> Option<u32> {
        let end = self.end(at);
        // A token's subtree is itself.
        while from < end {
            if !self.is_token(from) {
                return Some(from);
            }
            from += 1;
        }
        None
    }

    #[inline]
    pub(crate) fn first_child(&self, at: u32) -> Option<u32> {
        self.node_from(at, at + 1)
    }

    #[inline]
    pub(crate) fn next_sibling(&self, at: u32) -> Option<u32> {
        self.node_from(self.parent(at)?, self.end(at))
    }

    /// The child or token of `at` that holds `range`, as syntax-tree's green nodes find it with a
    /// binary search of the children: the one that holds `range` and overlaps it, else the
    /// last that ends at or before its start, else the first.
    pub(crate) fn child_at_range(&self, at: u32, range: TextRange) -> Option<u32> {
        let first = self.first_child_or_token(at)?;
        let (lo, hi) = (at + 1, self.end(at));
        let start = u32::from(range.start());
        let contains = |it: u32| self.range(it).contains_range(range);
        if !range.is_empty() {
            // Elements are in preorder, so the last to start at or before `start` is in the
            // only child that can hold a range that is not empty.
            let n = self.partition_point(lo, hi, |it| it <= start);
            if n == lo {
                return None;
            }
            let child = self.child_containing(at, n - 1);
            return contains(child).then_some(child);
        }
        let n = self.partition_point(lo, hi, |it| it < start);
        let mut last_less = None;
        if n > lo {
            let child = self.child_containing(at, n - 1);
            if u32::from(self.range(child).end()) > start {
                return Some(child);
            }
            last_less = Some(child);
        }
        // Empty children at `start` end at or before it too.
        let mut next = last_less.map_or(Some(first), |it| self.next_sibling_or_token(it));
        while let Some(it) = next.filter(|&it| self.range(it) == range) {
            last_less = Some(it);
            next = self.next_sibling_or_token(it);
        }
        last_less.or(Some(first)).filter(|&it| contains(it))
    }

    /// The first node of `kind` and `range` that `child_at_range` steps down to from `at`, for
    /// a range that is not empty: those steps go through the ancestors of the last element to
    /// start at or before the range, and stop at the first that does not hold it or is a
    /// token. Found from the bottom up, up to the first ancestor larger than the range, above
    /// which every one is larger too. A lazy node is a node without children here.
    pub(crate) fn node_at_range(&self, at: u32, range: TextRange, kind: SyntaxKind) -> Option<u32> {
        debug_assert!(!range.is_empty());
        let mut it = self.last_starting(at, range.start())?;
        let mut found = None;
        loop {
            let of_it = self.range(it);
            if of_it == range {
                if !self.is_token(it) && self.kind(it) == kind {
                    found = Some(it);
                }
            } else if of_it.contains_range(range) {
                return found;
            }
            if it == at {
                return found;
            }
            it = self.parent(it)?;
        }
    }

    /// The subtree of node `at`, as a tree of its own over a copy of its text, starting at 0.
    /// Its lazy nodes stay lazy, and the parts already parsed are kept, moved to the copy's
    /// text: copying a part's arrays is cheaper than parsing it again.
    pub(crate) fn subtree(&self, at: u32) -> FlatTree {
        let (lo, hi) = (at, self.end(at));
        let range = self.range(at);
        let source: Arc<str> = Arc::from(&self.source[range]);
        let words: Box<[u32]> = self.words[lo as usize..hi as usize].into();
        let big: Box<[(u32, u32)]> =
            self.big_in(lo, hi).iter().map(|&(it, value)| (it - at, value)).collect();
        let marks = marks(&words, &big, 0);
        let parents = std::iter::once(NO_PARENT)
            .chain(self.parents[lo as usize + 1..hi as usize].iter().map(|it| it - at))
            .collect();
        let lazy = self.lazy.as_ref().and_then(|lazy| {
            let from = lazy.nodes.partition_point(|&it| it < lo);
            let to = lazy.nodes.partition_point(|&it| it < hi);
            (from < to).then(|| {
                Box::new(Lazy {
                    nodes: lazy.nodes[from..to].iter().map(|it| it - at).collect(),
                    parts: lazy.parts[from..to]
                        .iter()
                        .map(|part| moved_part(part, &source, range.start().into(), 0))
                        .collect(),
                    expand: lazy.expand,
                    data: lazy.data,
                })
            })
        });
        FlatTree { source, words, parents, marks, big, lazy }
    }

    /// This tree, which has no lazy nodes, with its text at `to` in `source` instead of at
    /// `from` in its own.
    fn moved(mut self, source: Arc<str>, from: u32, to: u32) -> FlatTree {
        debug_assert!(self.lazy.is_none());
        for mark in self.marks.iter_mut() {
            *mark = *mark - from + to;
        }
        self.source = source;
        self
    }

    /// The tree with node `at`'s subtree replaced by `with`, whose root is a node of the same
    /// kind, over `source`, the new whole text: the text of `with` is `source` at `at`'s
    /// start, and the rest of the root's text is as it was around it. Lazy nodes outside `at`
    /// stay lazy and those of `with` come along; the parts already parsed of either are kept,
    /// moved to `source` (their text did not change, only where it is). A tree has one
    /// expander, so when both keep lazy nodes they must have the same `data`.
    pub fn replace_subtree(&self, at: u32, with: &FlatTree, source: Arc<str>) -> FlatTree {
        let kind = self.kind(at);
        assert!(
            !self.is_token(at) && !with.is_token(0) && with.kind(0) == kind,
            "element {at} is replaced by a {:?} that is not a node of its kind, {kind:?}",
            with.kind(0),
        );
        let (old, root, new) = (self.range(at), self.range(0), with.range(0));
        let (base, start) = (u32::from(root.start()), u32::from(old.start()));
        let (old_end, old_root_end) = (u32::from(old.end()), u32::from(root.end()));
        let new_end = start as u64 + u32::from(new.len()) as u64;
        let root_end = new_end + (old_root_end - old_end) as u64;
        fn bytes(text: &str, from: u64, to: u64) -> Option<&[u8]> {
            text.as_bytes().get(from as usize..to as usize)
        }
        let same = |from: u64, to: u64, old_from: u32, old_to: u32| {
            let it = bytes(&source, from, to);
            it.is_some() && it == bytes(&self.source, old_from as u64, old_to as u64)
        };
        assert!(
            root_end <= u32::MAX as u64
                && (old_root_end as usize != self.source.len() || root_end == source.len() as u64)
                && bytes(&source, start as u64, new_end) == Some(with.source[new].as_bytes())
                && same(base as u64, start as u64, base, start)
                && same(new_end, root_end, old_end, old_root_end),
            "the new text is not the old one with {kind:?} at {old:?} replaced by the {new:?} given",
        );
        let new_end = new_end as u32;
        let (lo, hi) = (at, self.end(at));
        let (removed, added) = (hi - lo, with.len());
        let len = self.len() as u64 - removed as u64 + added as u64;
        assert!(len < NO_PARENT as u64, "a tree of 2^32 - 1 elements or more");
        let after = |it: u32| it - removed + added;

        let ancestors: Vec<u32> = std::iter::successors(self.parent(at), |&it| self.parent(it))
            .collect();
        let mut words = Vec::with_capacity(len as usize);
        let mut big: Vec<(u32, u32)> = self
            .big_in(0, lo)
            .iter()
            .filter(|it| !ancestors.contains(&it.0))
            .copied()
            .collect();
        words.extend_from_slice(&self.words[..lo as usize]);
        for &it in &ancestors {
            let size = self.end(it) - it - removed + added;
            words[it as usize] = pack(self.kind(it), NODE, size, it, &mut big);
        }
        words.extend_from_slice(&with.words);
        big.extend(with.big.iter().map(|&(it, value)| (it + lo, value)));
        words.extend_from_slice(&self.words[hi as usize..]);
        big.extend(self.big_in(hi, self.len()).iter().map(|&(it, value)| (after(it), value)));
        big.sort_unstable_by_key(|it| it.0);

        let mut parents = Vec::with_capacity(len as usize);
        parents.extend_from_slice(&self.parents[..=lo as usize]);
        parents.extend(with.parents[1..].iter().map(|it| it + lo));
        parents.extend(self.parents[hi as usize..].iter().map(|&it| match it < lo {
            true => it,
            false => after(it),
        }));

        let ours = self.lazy.as_ref().map(|lazy| {
            let before = lazy.nodes.partition_point(|&it| it < lo);
            let past = lazy.nodes.partition_point(|&it| it < hi);
            (lazy, before, past)
        });
        let kept = ours.map_or(0, |(lazy, before, past)| before + lazy.nodes.len() - past);
        let theirs = with.lazy.as_ref().filter(|it| !it.nodes.is_empty());
        let expander = match (ours.filter(|_| kept > 0), theirs) {
            (Some((ours, ..)), Some(theirs)) => {
                assert_eq!(
                    ours.data, theirs.data,
                    "a tree's lazy nodes and those replacing {kind:?} at {old:?} have different expanders",
                );
                Some((ours.expand, ours.data))
            }
            (Some((ours, ..)), None) => Some((ours.expand, ours.data)),
            (None, Some(theirs)) => Some((theirs.expand, theirs.data)),
            (None, None) => None,
        };
        let lazy = expander.map(|(expand, data)| {
            let (mut nodes, mut parts) = (Vec::new(), Vec::new());
            if let Some((ours, before, _)) = ours {
                for (&it, part) in ours.nodes[..before].iter().zip(&ours.parts[..before]) {
                    nodes.push(it);
                    parts.push(moved_part(part, &source, 0, 0));
                }
            }
            if let Some(theirs) = theirs {
                for (&it, part) in theirs.nodes.iter().zip(theirs.parts.iter()) {
                    nodes.push(it + lo);
                    parts.push(moved_part(part, &source, new.start().into(), start));
                }
            }
            if let Some((ours, _, past)) = ours {
                for (&it, part) in ours.nodes[past..].iter().zip(&ours.parts[past..]) {
                    nodes.push(after(it));
                    parts.push(moved_part(part, &source, old_end, new_end));
                }
            }
            Box::new(Lazy { nodes: nodes.into(), parts: parts.into(), expand, data })
        });

        let marks = marks(&words, &big, base);
        FlatTree {
            source,
            words: words.into_boxed_slice(),
            parents: parents.into_boxed_slice(),
            marks,
            big: big.into_boxed_slice(),
            lazy,
        }
    }

    /// The lazy nodes, in preorder.
    pub fn lazies(&self) -> impl Iterator<Item = u32> + '_ {
        self.lazy.iter().flat_map(|it| it.nodes.iter().copied())
    }

    fn slot(&self, at: u32) -> &OnceLock<Arc<FlatTree>> {
        let lazy = match (self.is_lazy(at), &self.lazy) {
            (true, Some(lazy)) => lazy,
            _ => panic!("element {at} ({:?}) is not a lazy node", self.kind(at)),
        };
        let it = lazy.nodes.binary_search(&at).expect("a lazy node is among the lazy nodes");
        &lazy.parts[it]
    }

    /// The parsed part of lazy node `at`, parsed on the first call (on one thread, the others
    /// waiting for it): a tree whose root is `at`'s kind and range, its ranges in the same
    /// source (so absolute in the file), without lazy nodes of its own.
    pub fn part(&self, at: u32) -> &Arc<FlatTree> {
        self.slot(at).get_or_init(|| Arc::new(self.expand(at)))
    }

    /// Whether `at` is a lazy node whose part was parsed already.
    pub fn is_parsed(&self, at: u32) -> bool {
        self.is_lazy(at) && self.slot(at).get().is_some()
    }

    fn expand(&self, at: u32) -> FlatTree {
        let lazy = self.lazy.as_deref().expect("a lazy node's tree has an expander");
        let (kind, range) = (self.kind(at), self.range(at));
        let text = &self.source[range];
        let part = (lazy.expand)(kind, text, lazy.data);
        let root = part.range(0);
        assert!(
            !part.is_token(0)
                && part.kind(0) == kind
                && part.lazy.is_none()
                && part.source.get(usize::from(root.start())..usize::from(root.end()))
                    == Some(text),
            "the part of the lazy {kind:?} at {range:?} is not one {kind:?} over its text \
             without lazy nodes: its root is a {:?} at {root:?}",
            part.kind(0),
        );
        part.moved(self.source.clone(), root.start().into(), range.start().into())
    }

    /// A green tree of the element's subtree, made with `cache`, with the parts of its lazy
    /// nodes in it.
    pub(crate) fn green(&self, at: u32, cache: &mut NodeCache) -> GreenNode {
        let mut builder = GreenNodeBuilder::with_cache(cache);
        self.build(at, self.end(at), &mut builder);
        builder.finish()
    }

    /// Gives `builder` the elements from `from` to `to`, whole subtrees, lazy nodes with their
    /// parts.
    fn build(&self, from: u32, to: u32, builder: &mut GreenNodeBuilder<'_>) {
        let mut open = Vec::new();
        for it in from..to {
            while open.last().is_some_and(|&end| end <= it) {
                open.pop();
                builder.finish_node();
            }
            if self.is_token(it) {
                builder.token(self.kind(it), self.token_text(it));
            } else if self.is_lazy(it) {
                let part = self.part(it);
                builder.start_node(self.kind(it));
                part.build(1, part.len(), builder);
                builder.finish_node();
            } else {
                builder.start_node(self.kind(it));
                open.push(self.end(it));
            }
        }
        for _ in open {
            builder.finish_node();
        }
    }
}

/// The vector's elements in an allocation of their size: an allocator may keep a block that
/// is shrunk to more than half of it whole (mimalloc does), so a vector grown by doubling
/// and boxed holds up to twice what it needs.
fn exact<T: Copy>(vec: Vec<T>) -> Box<[T]> {
    match vec.capacity() == vec.len() {
        true => vec.into_boxed_slice(),
        false => vec.as_slice().into(),
    }
}

/// Makes a [`FlatTree`] from the same calls as [`GreenNodeBuilder`].
#[derive(Default, Debug)]
pub struct FlatTreeBuilder {
    /// The text appended so far, when the builder has no source.
    text: String,
    source: Option<Arc<str>>,
    /// The next byte of `source`.
    next: usize,
    words: Vec<u32>,
    parents: Vec<u32>,
    marks: Vec<u32>,
    big: Vec<(u32, u32)>,
    open: Vec<u32>,
    lazies: Vec<u32>,
    expand: Option<(Expand, u32)>,
}

impl FlatTreeBuilder {
    pub fn new() -> FlatTreeBuilder {
        FlatTreeBuilder::default()
    }

    /// A builder for a text of about `len` bytes.
    pub fn with_capacity(len: usize) -> FlatTreeBuilder {
        // About one element for every two bytes of Rust.
        let elements = len / 2;
        FlatTreeBuilder {
            text: String::with_capacity(len),
            words: Vec::with_capacity(elements),
            parents: Vec::with_capacity(elements),
            marks: Vec::with_capacity(elements / MARK as usize + 1),
            ..FlatTreeBuilder::default()
        }
    }

    /// A builder whose tokens are consecutive slices of `source`, which the tree keeps instead
    /// of a copy: `token` checks its text is the next bytes of `source` and moves past them,
    /// and `finish` checks every byte was reached.
    pub fn sourced(source: Arc<str>) -> FlatTreeBuilder {
        let elements = source.len() / 2;
        FlatTreeBuilder {
            words: Vec::with_capacity(elements),
            parents: Vec::with_capacity(elements),
            marks: Vec::with_capacity(elements / MARK as usize + 1),
            source: Some(source),
            ..FlatTreeBuilder::default()
        }
    }

    /// Where the next element starts.
    #[inline]
    fn offset(&self) -> usize {
        match self.source {
            Some(_) => self.next,
            None => self.text.len(),
        }
    }

    fn push(&mut self, kind: SyntaxKind, tag: u32, value: u32) -> u32 {
        assert!(self.words.len() < NO_PARENT as usize, "a tree of 2^32 - 1 elements or more");
        let at = self.words.len() as u32;
        if at % MARK == 0 {
            self.marks.push(self.offset() as u32);
        }
        let word = pack(kind, tag, value, at, &mut self.big);
        self.words.push(word);
        self.parents.push(self.open.last().copied().unwrap_or(NO_PARENT));
        at
    }

    pub fn start_node(&mut self, kind: SyntaxKind) {
        assert!(!self.open.is_empty() || self.words.is_empty(), "a tree has one root");
        let at = self.push(kind, NODE, 0);
        self.open.push(at);
    }

    pub fn token(&mut self, kind: SyntaxKind, text: &str) {
        assert!(!self.open.is_empty(), "a token outside every node");
        let end = self.offset() + text.len();
        assert!(end <= u32::MAX as usize, "a text of 4 GiB or more");
        if let Some(source) = &self.source {
            assert!(
                source.as_bytes().get(self.next..end) == Some(text.as_bytes()),
                "the {kind:?} {text:?} is not the source's next bytes, at {}",
                self.next,
            );
        }
        self.push(kind, TOKEN, text.len() as u32);
        match self.source {
            Some(_) => self.next = end,
            None => self.text.push_str(text),
        }
    }

    /// The source's next `len` bytes as one lazy node of `kind` (a body not parsed), with no
    /// children in the arrays: inside a node, or as the whole tree. Only for a builder with a
    /// source, and the tree must be given its expander before `finish`.
    pub fn lazy(&mut self, kind: SyntaxKind, len: u32) {
        assert!(!self.open.is_empty() || self.words.is_empty(), "a tree has one root");
        let end = self.next + len as usize;
        match &self.source {
            None => panic!("a lazy {kind:?} in a tree built without a source"),
            Some(source) => assert!(
                end <= source.len() && source.is_char_boundary(end),
                "a lazy {kind:?} of {len} bytes at {} does not end in the source",
                self.next,
            ),
        }
        let at = self.push(kind, LAZY, len);
        self.lazies.push(at);
        self.next = end;
    }

    /// What parses the tree's lazy nodes, and the `data` it is called with.
    pub fn expand_with(&mut self, expand: Expand, data: u32) {
        self.expand = Some((expand, data));
    }

    pub fn finish_node(&mut self) {
        let at = self.open.pop().expect("a node finished that was not started");
        let size = self.words.len() as u32 - at;
        let kind = SyntaxKind(self.words[at as usize] as u16);
        self.words[at as usize] = pack(kind, NODE, size, at, &mut self.big);
    }

    pub fn finish(mut self) -> FlatTree {
        assert!(self.open.is_empty() && !self.words.is_empty(), "the tree is not finished");
        if self.words.len() % MARK as usize == 0 {
            self.marks.push(self.offset() as u32);
        }
        let source: Arc<str> = match self.source {
            Some(source) => {
                assert!(
                    self.next == source.len(),
                    "the tree ends at byte {} of a source of {}",
                    self.next,
                    source.len(),
                );
                source
            }
            None => Arc::from(self.text),
        };
        self.big.sort_unstable_by_key(|it| it.0);
        let lazy = match self.lazies.is_empty() {
            true => None,
            false => {
                let (expand, data) = self.expand.expect("a tree with lazy nodes has no expander");
                Some(Box::new(Lazy {
                    parts: self.lazies.iter().map(|_| OnceLock::new()).collect(),
                    nodes: exact(self.lazies),
                    expand,
                    data,
                }))
            }
        };
        FlatTree {
            source,
            words: exact(self.words),
            parents: exact(self.parents),
            marks: exact(self.marks),
            big: exact(self.big),
            lazy,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{hash_map::DefaultHasher, BTreeMap},
        hash::{Hash, Hasher},
        sync::{
            atomic::{AtomicU32, AtomicUsize, Ordering},
            Arc, Barrier, Mutex,
        },
    };

    use super::{Expand, FlatTree, BIG};
    use crate::{
        cursor::{SyntaxElement, SyntaxNode},
        FlatTreeBuilder, GreenNodeBuilder, NodeOrToken, SyntaxKind, TextRange, TextSize,
        TokenAtOffset, WalkEvent,
    };

    struct Rng(u64);

    impl Rng {
        fn new(seed: u64) -> Rng {
            Rng(seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407))
        }

        fn below(&mut self, below: u64) -> u64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (self.0 >> 33) % below
        }
    }

    /// A tree to build each way: a node is its kind, its children and, when it is lazy in an
    /// outline, the number of its part in [`PARTS`].
    #[derive(Clone, Debug)]
    enum Shape {
        Node(SyntaxKind, Vec<Shape>, Option<usize>),
        Token(SyntaxKind, String),
    }

    /// A tree from a seed: nodes up to five deep, some empty, tokens of zero to three bytes.
    fn random_shape(seed: u64) -> Shape {
        fn close(open: &mut Vec<(SyntaxKind, Vec<Shape>)>) {
            let (kind, children) = open.pop().unwrap();
            open.last_mut().unwrap().1.push(Shape::Node(kind, children, None));
        }
        let mut rng = Rng::new(seed);
        let mut open = vec![(SyntaxKind(100), Vec::new())];
        for _ in 0..rng.below(60) {
            match rng.below(4) {
                0 if open.len() < 5 => {
                    open.push((SyntaxKind(100 + rng.below(5) as u16), Vec::new()))
                }
                1 if open.len() > 1 => close(&mut open),
                _ => {
                    let kind = SyntaxKind(rng.below(5) as u16);
                    let text = "abc"[..rng.below(4) as usize].to_owned();
                    open.last_mut().unwrap().1.push(Shape::Token(kind, text));
                }
            }
        }
        while open.len() > 1 {
            close(&mut open);
        }
        let (kind, children) = open.pop().unwrap();
        Shape::Node(kind, children, None)
    }

    /// A tree past what an element's word holds: rows of 170 tokens, in nodes of more than
    /// 2^14 elements, and a few tokens of more than 2^14 bytes.
    fn big_shape(seed: u64) -> Shape {
        let mut rng = Rng::new(seed);
        let mut rows = Vec::new();
        for row in 0..100u64 {
            let mut tokens = Vec::new();
            for i in 0..170u16 {
                let text = "abc"[..rng.below(4) as usize].to_owned();
                tokens.push(Shape::Token(SyntaxKind(i % 5), text));
            }
            if row % 37 == seed % 37 {
                let long = "x".repeat(BIG as usize + rng.below(3000) as usize);
                tokens.insert(rng.below(170) as usize, Shape::Token(SyntaxKind(3), long));
            }
            rows.push(Shape::Node(SyntaxKind(101 + (row % 4) as u16), tokens, None));
        }
        let rows = Shape::Node(SyntaxKind(102), rows, None);
        Shape::Node(SyntaxKind(100), vec![rows, Shape::Token(SyntaxKind(1), "ab".into())], None)
    }

    /// The parts of the lazy nodes of every outline the tests build, by the number in their
    /// first token.
    static PARTS: Mutex<BTreeMap<usize, FlatTree>> = Mutex::new(BTreeMap::new());
    static NEXT_PART: AtomicUsize = AtomicUsize::new(0);
    const DATA: u32 = 0x5eed;

    /// A lazy node: its first token is `<n>`, `n` the number of its part.
    fn lazy_node(kind: SyntaxKind, mut children: Vec<Shape>) -> Shape {
        let n = NEXT_PART.fetch_add(1, Ordering::Relaxed);
        children.insert(0, Shape::Token(SyntaxKind(4), format!("<{n}>")));
        Shape::Node(kind, children, Some(n))
    }

    /// Makes some nodes below the root of `shape` lazy, none inside another.
    fn make_lazy(shape: &mut Shape, rng: &mut Rng) {
        let Shape::Node(_, children, _) = shape else { return };
        for child in children.iter_mut() {
            if let Shape::Node(kind, grandchildren, None) = child {
                if rng.below(3) == 0 {
                    *child = lazy_node(*kind, std::mem::take(grandchildren));
                    continue;
                }
            }
            make_lazy(child, rng);
        }
    }

    /// The expander of the tests' outlines.
    fn expand(kind: SyntaxKind, text: &str, data: u32) -> FlatTree {
        assert_eq!(data, DATA);
        let n: usize = text[1..text.find('>').expect("a part's number")].parse().unwrap();
        let part = PARTS.lock().unwrap()[&n].clone();
        assert_eq!(part.kind(0), kind);
        part
    }

    fn text_of(shape: &Shape, text: &mut String) {
        match shape {
            Shape::Node(_, children, _) => children.iter().for_each(|it| text_of(it, text)),
            Shape::Token(_, it) => text.push_str(it),
        }
    }

    fn to_green(shape: &Shape, builder: &mut GreenNodeBuilder<'_>) {
        match shape {
            Shape::Node(kind, children, _) => {
                builder.start_node(*kind);
                children.iter().for_each(|it| to_green(it, builder));
                builder.finish_node();
            }
            Shape::Token(kind, text) => builder.token(*kind, text),
        }
    }

    /// Builds `shape`, its lazy nodes lazy when `outline`, keeping their parts in [`PARTS`].
    fn to_flat(shape: &Shape, builder: &mut FlatTreeBuilder, outline: bool) {
        fn node(kind: SyntaxKind, children: &[Shape], builder: &mut FlatTreeBuilder, outline: bool) {
            builder.start_node(kind);
            children.iter().for_each(|it| to_flat(it, builder, outline));
            builder.finish_node();
        }
        match shape {
            Shape::Node(kind, children, Some(n)) if outline => {
                let mut text = String::new();
                text_of(shape, &mut text);
                builder.lazy(*kind, text.len() as u32);
                let mut part = FlatTreeBuilder::new();
                node(*kind, children, &mut part, false);
                PARTS.lock().unwrap().insert(*n, part.finish());
            }
            Shape::Node(kind, children, _) => node(*kind, children, builder, outline),
            Shape::Token(kind, text) => builder.token(*kind, text),
        }
    }

    fn build_green(shape: &Shape) -> SyntaxNode {
        let mut builder = GreenNodeBuilder::new();
        to_green(shape, &mut builder);
        SyntaxNode::new_root(builder.finish())
    }

    /// `shape` as a flat tree in full, appending its text or over its source.
    fn build_flat(shape: &Shape, sourced: bool) -> FlatTree {
        let mut builder = match sourced {
            true => {
                let mut text = String::new();
                text_of(shape, &mut text);
                FlatTreeBuilder::sourced(text.into())
            }
            false => FlatTreeBuilder::new(),
        };
        to_flat(shape, &mut builder, false);
        builder.finish()
    }

    /// `shape` as an outline over its source.
    fn build_outline(shape: &Shape, expand: Expand) -> Arc<FlatTree> {
        let mut text = String::new();
        text_of(shape, &mut text);
        let mut builder = FlatTreeBuilder::sourced(text.into());
        to_flat(shape, &mut builder, true);
        builder.expand_with(expand, DATA);
        Arc::new(builder.finish())
    }

    fn describe(element: Option<SyntaxElement>) -> Option<(SyntaxKind, TextRange, bool)> {
        element.map(|it| (it.kind(), it.text_range(), it.as_token().is_some()))
    }

    fn elements(root: &SyntaxNode) -> Vec<SyntaxElement> {
        root.preorder_with_tokens()
            .filter_map(|it| match it {
                WalkEvent::Enter(it) => Some(it),
                WalkEvent::Leave(_) => None,
            })
            .collect()
    }

    fn hash_of(node: &SyntaxNode) -> u64 {
        let mut state = DefaultHasher::new();
        node.hash(&mut state);
        state.finish()
    }

    /// The ranges a node's checks look at when not every range: its own, its children's,
    /// those one byte shorter at either end, and the empty ones at their starts.
    fn around(node: &SyntaxNode) -> Vec<TextRange> {
        let one = TextSize::from(1);
        let mut ranges = vec![node.text_range(), TextRange::empty(node.text_range().end())];
        for child in node.children_with_tokens() {
            let range = child.text_range();
            ranges.extend([range, TextRange::empty(range.start())]);
            if range.len() > one {
                ranges.extend([
                    TextRange::new(range.start() + one, range.end()),
                    TextRange::new(range.start(), range.end() - one),
                ]);
            }
        }
        ranges
    }

    /// Every function of the cursor on every element of `flat` against `green`, the same tree:
    /// with every range in every node when `exhaustive`, else the ranges [`around`] it.
    fn compare(green: &SyntaxNode, flat: &SyntaxNode, exhaustive: bool, seed: u64) {
        assert_eq!(green.to_string(), flat.to_string());
        let (greens, flats) = (elements(green), elements(flat));
        assert_eq!(greens.len(), flats.len());
        for (g, f) in greens.iter().zip(&flats) {
            assert_eq!(describe(Some(g.clone())), describe(Some(f.clone())), "seed {seed}");
            assert_eq!(g.index(), f.index());
            assert_eq!(describe(g.parent().map(Into::into)), describe(f.parent().map(Into::into)));
            assert_eq!(describe(g.next_sibling_or_token()), describe(f.next_sibling_or_token()));
            assert_eq!(describe(g.prev_sibling_or_token()), describe(f.prev_sibling_or_token()));
            assert_eq!(
                describe(g.first_token().map(Into::into)),
                describe(f.first_token().map(Into::into))
            );
            assert_eq!(
                describe(g.last_token().map(Into::into)),
                describe(f.last_token().map(Into::into))
            );
            match (g, f) {
                (NodeOrToken::Node(g), NodeOrToken::Node(f)) => {
                    let node = |it: Option<SyntaxNode>| describe(it.map(Into::into));
                    assert_eq!(node(g.first_child()), node(f.first_child()));
                    assert_eq!(node(g.last_child()), node(f.last_child()));
                    assert_eq!(node(g.next_sibling()), node(f.next_sibling()));
                    assert_eq!(node(g.prev_sibling()), node(f.prev_sibling()));
                    assert_eq!(describe(g.first_child_or_token()), describe(f.first_child_or_token()));
                    assert_eq!(describe(g.last_child_or_token()), describe(f.last_child_or_token()));
                    assert_eq!(g.green(), f.green());
                    assert_eq!(g.text().to_string(), f.text().to_string());
                    assert_eq!(g.first_child_token_text(), f.first_child_token_text());
                    let range = g.text_range();
                    let (start, end) = (u32::from(range.start()), u32::from(range.end()));
                    let ranges: Vec<TextRange> = match exhaustive {
                        true => (start..=end)
                            .flat_map(|from| {
                                (from..=end).map(move |to| TextRange::new(from.into(), to.into()))
                            })
                            .collect(),
                        false => around(g),
                    };
                    for &at in &ranges {
                        assert_eq!(
                            describe(g.child_or_token_at_range(at)),
                            describe(f.child_or_token_at_range(at)),
                            "seed {seed}, {at:?} in {range:?}"
                        );
                        assert_eq!(
                            describe(Some(g.covering_element(at))),
                            describe(Some(f.covering_element(at)))
                        );
                        for kind in (0..5).chain(100..105).map(SyntaxKind) {
                            let stepped = |from: &SyntaxNode| {
                                std::iter::successors(Some(from.clone()), |it| {
                                    it.child_or_token_at_range(at)?.into_node()
                                })
                                .find(|it| it.text_range() == at && it.kind() == kind)
                            };
                            let found =
                                f.flat_node_at_range(at, kind).unwrap_or_else(|| stepped(f));
                            assert_eq!(
                                stepped(g).map(|it| describe(Some(it.into()))),
                                found.map(|it| describe(Some(it.into()))),
                                "seed {seed}, {kind:?} at {at:?}"
                            );
                        }
                    }
                    let offsets: Vec<u32> = match exhaustive {
                        true => (start..=end).collect(),
                        false => ranges.iter().map(|it| u32::from(it.start())).collect(),
                    };
                    for offset in offsets {
                        let offset = TextSize::from(offset);
                        let tokens = |it: TokenAtOffset<_>| {
                            it.map(|it| describe(Some(NodeOrToken::Token(it)))).collect::<Vec<_>>()
                        };
                        assert_eq!(tokens(g.token_at_offset(offset)), tokens(f.token_at_offset(offset)));
                    }
                }
                (NodeOrToken::Token(g), NodeOrToken::Token(f)) => {
                    assert_eq!(g.text(), f.text());
                    assert_eq!(g.green(), f.green());
                    let token = |it: Option<_>| describe(it.map(NodeOrToken::Token));
                    assert_eq!(token(g.next_token()), token(f.next_token()));
                    assert_eq!(token(g.prev_token()), token(f.prev_token()));
                }
                _ => unreachable!(),
            }
        }
        let hash = |it: &SyntaxNode| {
            let (tree, at) = it.flat().unwrap();
            let mut state = DefaultHasher::new();
            tree.hash_subtree(at, &mut state);
            state.finish()
        };
        // Whether the arrays hold all of a node's subtree, no lazy node in it.
        let full = |it: &SyntaxNode| {
            let (tree, at) = it.flat().unwrap();
            (at..tree.end(at)).all(|it| !tree.is_lazy(it))
        };
        for (g, f) in greens.iter().zip(&flats).filter(|(g, _)| g.as_node().is_some()) {
            let (g, f) = (g.as_node().unwrap(), f.as_node().unwrap());
            let replacement = g.green().to_owned();
            assert_eq!(g.replace_with(replacement.clone()), f.replace_with(replacement));
            assert_eq!(g.clone_subtree().to_string(), f.clone_subtree().to_string());
            let alone = f.clone_subtree();
            assert_eq!(alone.green(), g.green());
            assert_eq!(hash(f), hash(&alone), "seed {seed}");
            if full(f) {
                let mut again = FlatTreeBuilder::new();
                for event in g.preorder_with_tokens() {
                    match event {
                        WalkEvent::Enter(NodeOrToken::Node(it)) => again.start_node(it.kind()),
                        WalkEvent::Enter(NodeOrToken::Token(it)) => again.token(it.kind(), it.text()),
                        WalkEvent::Leave(NodeOrToken::Node(_)) => again.finish_node(),
                        WalkEvent::Leave(NodeOrToken::Token(_)) => {}
                    }
                }
                assert_eq!(alone.flat_tree().map(|it| &**it), Some(&again.finish()));
            }
            if exhaustive {
                let twin = g.to_flat();
                assert_eq!(describe(Some(twin.clone().into())), describe(Some(f.clone().into())));
                assert_eq!(twin.parent().is_none(), g.parent().is_none());
                if full(f) {
                    assert_eq!(hash(&twin), hash(f));
                }
            }
            for node in [g, f] {
                for kind in (0..5).chain(100..105).map(SyntaxKind) {
                    let ancestor = node.ancestors().find(|it| it.kind() == kind);
                    assert_eq!(node.ancestor_where(|it| it == kind), ancestor);
                    let child = node.children().find(|it| it.kind() == kind);
                    assert_eq!(node.child_where(|it| it == kind), child);
                    let token = node
                        .children_with_tokens()
                        .filter_map(NodeOrToken::into_token)
                        .find(|it| it.kind() == kind);
                    assert_eq!(node.child_token(kind), token);
                }
                let mut odd = node.children_where();
                let by_kind = std::iter::from_fn(|| odd.next_where(|it| it.0 % 2 == 1));
                let children = node.children().filter(|it| it.kind().0 % 2 == 1);
                assert!(by_kind.eq(children));
            }
            let (tree, at) = f.flat().unwrap();
            if !tree.is_lazy(at) {
                let children: Vec<_> = f.children().collect();
                let read: Vec<_> = tree.children(at).map(|it| f.flat_element(it)).collect();
                assert_eq!(read, children.into_iter().map(NodeOrToken::Node).collect::<Vec<_>>());
            }
            if full(f) {
                let descendants = f.descendants().map(|it| (it.kind(), it.text_range()));
                assert!(
                    descendants.eq(tree.descendants(at).map(|it| (tree.kind(it), tree.range(it))))
                );
            }
        }
    }

    #[test]
    fn a_flat_tree_answers_as_a_green_one() {
        for seed in 0..2000 {
            let shape = random_shape(seed);
            let flat = SyntaxNode::new_flat_root(Arc::new(build_flat(&shape, seed % 2 == 1)));
            compare(&build_green(&shape), &flat, true, seed);
        }
        for seed in 0..4 {
            let shape = big_shape(seed);
            let tree = build_flat(&shape, seed % 2 == 1);
            assert!(tree.big.iter().any(|&(it, _)| tree.is_token(it)), "a token past the word");
            assert!(tree.big.iter().any(|&(it, _)| !tree.is_token(it)), "a node past the word");
            compare(&build_green(&shape), &SyntaxNode::new_flat_root(Arc::new(tree)), false, seed);
        }
    }

    #[test]
    fn a_tree_of_millions_of_elements_and_a_token_of_megabytes() {
        const ROWS: u32 = 30_000;
        let long = "y".repeat(5 << 20);
        let mut builder = FlatTreeBuilder::new();
        builder.start_node(SyntaxKind(100));
        for _ in 0..ROWS {
            builder.start_node(SyntaxKind(101));
            for _ in 0..99 {
                builder.token(SyntaxKind(1), "ab");
            }
            builder.finish_node();
        }
        builder.token(SyntaxKind(2), &long);
        builder.finish_node();
        let tree = builder.finish();
        assert_eq!(tree.len(), 1 + ROWS * 100 + 1);
        assert_eq!(tree.end(0), tree.len());
        for row in [0, 1, 7, 12_345, ROWS - 1] {
            let at = 1 + row * 100;
            assert_eq!((tree.kind(at), tree.end(at), tree.parent(at)), (SyntaxKind(101), at + 100, Some(0)));
            assert_eq!(tree.range(at), TextRange::at((row * 198).into(), 198.into()));
            assert_eq!(tree.parent(at + 99), Some(at));
            assert_eq!(tree.range(at + 99), TextRange::at((row * 198 + 196).into(), 2.into()));
        }
        let last = tree.len() - 1;
        assert_eq!(tree.range(last), TextRange::at((ROWS * 198).into(), (5u32 << 20).into()));
        assert_eq!(tree.token_text(last), long);
        assert_eq!(tree.range(0).end(), TextSize::of(tree.text()));
        let root = SyntaxNode::new_flat_root(Arc::new(tree));
        let found = root.covering_element(TextRange::at((12_345 * 198 + 3).into(), 1.into()));
        assert_eq!(found.text_range(), TextRange::at((12_345 * 198 + 2).into(), 2.into()));
        let row = found.parent().unwrap();
        assert_eq!(row.text_range(), TextRange::at((12_345 * 198).into(), 198.into()));
        assert_eq!(row.index(), 12_345);
        let in_long = TextRange::at((ROWS * 198 + 1_000_000).into(), 5.into());
        assert_eq!(root.covering_element(in_long).kind(), SyntaxKind(2));
        let alone = row.clone_subtree();
        assert_eq!(alone.text_range(), TextRange::up_to(198.into()));
        assert_eq!(alone.to_string(), "ab".repeat(99));
    }

    #[test]
    fn an_outline_answers_as_the_tree_in_full() {
        for seed in 0..2004 {
            let big = seed >= 2000;
            let mut shape = match big {
                true => big_shape(seed),
                false => random_shape(seed),
            };
            make_lazy(&mut shape, &mut Rng::new(seed ^ 0x1a2b));
            let tree = build_outline(&shape, expand);
            let lazies: Vec<u32> = tree.lazies().collect();
            let outline = SyntaxNode::new_flat_root(tree.clone());
            for &at in &lazies {
                let node = outline.flat_element(at).into_node().unwrap();
                assert!(tree.is_lazy(at) && node.is_lazy() && !tree.is_parsed(at));
                assert_eq!(tree.end(at), at + 1);
                let (of, n) = node.flat().unwrap();
                assert!(Arc::ptr_eq(of, &tree) && n == at);
            }
            // Reading the arrays parses nothing.
            tree.hash_subtree(0, &mut DefaultHasher::new());
            assert_eq!(tree.descendants(0).filter(|&it| tree.is_lazy(it)).count(), lazies.len());
            let _ = tree.subtree(0);
            assert!(lazies.iter().all(|&at| !tree.is_parsed(at)));
            compare(&build_green(&shape), &outline, !big, seed);
            for &at in &lazies {
                let node = outline.flat_element(at).into_node().unwrap();
                assert!(!node.is_lazy() && tree.is_parsed(at));
                let part = tree.part(at);
                assert!(Arc::ptr_eq(part.source(), tree.source()));
                assert_eq!((part.kind(0), part.range(0)), (tree.kind(at), tree.range(at)));
                assert_eq!(part.lazies().count(), 0);
                for child in node.children_with_tokens() {
                    let parent = child.parent().unwrap();
                    assert_eq!(parent, node);
                    assert_eq!(hash_of(&parent), hash_of(&node));
                    assert_eq!(parent.index(), node.index());
                    let (of, n) = parent.flat().unwrap();
                    assert!(Arc::ptr_eq(of, &tree) && n == at);
                    if let NodeOrToken::Node(child) = &child {
                        let (of, n) = child.flat().unwrap();
                        assert!(Arc::ptr_eq(of, part));
                        assert_eq!(part.parent(n), Some(0));
                        assert_eq!(child.flat_element(0), NodeOrToken::Node(node.clone()));
                        assert!(child.ancestors().any(|it| it == node));
                        assert_eq!(child.tree_top(), outline);
                    }
                }
            }
        }
    }

    #[test]
    fn a_part_is_parsed_once_whichever_thread_asks() {
        static CALLS: AtomicU32 = AtomicU32::new(0);
        fn counting(kind: SyntaxKind, text: &str, data: u32) -> FlatTree {
            CALLS.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(20));
            expand(kind, text, data)
        }
        let body = vec![
            Shape::Token(SyntaxKind(2), "{".into()),
            Shape::Node(SyntaxKind(102), vec![Shape::Token(SyntaxKind(3), "x".into())], None),
            Shape::Token(SyntaxKind(2), "}".into()),
        ];
        let children = vec![Shape::Token(SyntaxKind(1), "fn".into()), lazy_node(SyntaxKind(101), body)];
        let tree = build_outline(&Shape::Node(SyntaxKind(100), children, None), counting);
        let at = tree.lazies().next().unwrap();
        let barrier = Barrier::new(4);
        let parts: Vec<Arc<FlatTree>> = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        tree.part(at).clone()
                    })
                })
                .collect();
            threads.into_iter().map(|it| it.join().unwrap()).collect()
        });
        assert!(parts.iter().all(|it| Arc::ptr_eq(it, &parts[0])));
        assert!(Arc::ptr_eq(&parts[0], tree.part(at)));
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);
        assert!(tree.is_parsed(at));
    }

    #[test]
    fn a_sourced_tree_and_its_parts_share_the_source() {
        let body = vec![
            Shape::Token(SyntaxKind(2), "{ ".into()),
            Shape::Token(SyntaxKind(3), "body".into()),
            Shape::Token(SyntaxKind(2), " }".into()),
        ];
        let children = vec![Shape::Token(SyntaxKind(1), "fn f() ".into()), lazy_node(SyntaxKind(101), body)];
        let shape = Shape::Node(SyntaxKind(100), children, None);
        let mut text = String::new();
        text_of(&shape, &mut text);
        let source: Arc<str> = text.into();
        let mut builder = FlatTreeBuilder::sourced(source.clone());
        to_flat(&shape, &mut builder, true);
        builder.expand_with(expand, DATA);
        let tree = Arc::new(builder.finish());
        assert!(Arc::ptr_eq(tree.source(), &source));
        let at = tree.lazies().next().unwrap();
        assert!(Arc::ptr_eq(tree.part(at).source(), &source));
        let mut builder = FlatTreeBuilder::sourced(source.clone());
        to_flat(&shape, &mut builder, false);
        let full = builder.finish();
        assert!(Arc::ptr_eq(full.source(), &source));
        assert_eq!(full, build_flat(&shape, false));
        assert!(full.heap_size() <= full.len() as usize * 9 + 8, "arrays only, no text");
        let root = SyntaxNode::new_flat_root(tree.clone());
        let body = root.last_child().unwrap();
        assert!(body.is_lazy());
        let token = body.last_token().unwrap();
        assert!(!body.is_lazy());
        assert_eq!(token.text(), " }");
        assert_eq!(token.text_range().end(), TextSize::of(&*source));
        assert_eq!(token.parent(), Some(body.clone()));
        let (of, n) = token.parent().unwrap().flat().map(|(it, n)| (it.clone(), n)).unwrap();
        assert!(Arc::ptr_eq(&of, &tree) && n == at);
    }

    #[test]
    #[should_panic(expected = "is not the source's next bytes")]
    fn a_sourced_builder_takes_only_its_source() {
        let mut builder = FlatTreeBuilder::sourced("ab".into());
        builder.start_node(SyntaxKind(100));
        builder.token(SyntaxKind(1), "b");
    }

    #[test]
    #[should_panic(expected = "the part of the lazy")]
    fn a_part_must_be_its_lazy_node_over_its_text() {
        fn short(kind: SyntaxKind, text: &str, _: u32) -> FlatTree {
            let mut builder = FlatTreeBuilder::new();
            builder.start_node(kind);
            builder.token(SyntaxKind(1), &text[1..]);
            builder.finish_node();
            builder.finish()
        }
        let mut builder = FlatTreeBuilder::sourced("{x}".into());
        builder.start_node(SyntaxKind(100));
        builder.lazy(SyntaxKind(101), 3);
        builder.finish_node();
        builder.expand_with(short, 0);
        let _ = builder.finish().part(1);
    }

    /// The elements of `shape`'s outline: a lazy node is one.
    fn count(shape: &Shape) -> u32 {
        match shape {
            Shape::Node(_, children, None) => 1 + children.iter().map(count).sum::<u32>(),
            _ => 1,
        }
    }

    /// `shape` with the node that is element `at` of its outline replaced by `with`, `next`
    /// the number of `shape` in it.
    fn replaced(shape: &Shape, at: u32, with: &Shape, next: &mut u32) -> Shape {
        if *next == at {
            *next += count(shape);
            return with.clone();
        }
        *next += 1;
        match shape {
            Shape::Node(kind, children, None) => {
                let children = children.iter().map(|it| replaced(it, at, with, next)).collect();
                Shape::Node(*kind, children, None)
            }
            _ => shape.clone(),
        }
    }

    #[test]
    fn replacing_a_subtree_is_building_the_edited_tree() {
        for seed in 0..1000 {
            let mut rng = Rng::new(seed ^ 0x9e37);
            let mut shape = random_shape(seed);
            make_lazy(&mut shape, &mut rng);
            let tree = build_outline(&shape, expand);
            for (i, at) in tree.lazies().enumerate() {
                if i % 2 == 0 {
                    let _ = tree.part(at);
                }
            }
            let nodes: Vec<u32> = (0..tree.len()).filter(|&it| !tree.is_token(it)).collect();
            let at = nodes[rng.below(nodes.len() as u64) as usize];
            let mut with = random_shape(seed + 10_000);
            if let Shape::Node(kind, ..) = &mut with {
                *kind = tree.kind(at);
            }
            if rng.below(2) == 0 {
                make_lazy(&mut with, &mut rng);
            }
            let edited = replaced(&shape, at, &with, &mut 0);
            let mut text = String::new();
            text_of(&edited, &mut text);
            let (lo, hi) = (at, tree.end(at));
            let carried =
                tree.lazies().filter(|&it| (it < lo || it >= hi) && tree.is_parsed(it)).count();
            let spliced = tree.replace_subtree(at, &build_outline(&with, expand), text.into());
            let built = build_outline(&edited, expand);
            assert_eq!(spliced, *built, "seed {seed}");
            for it in 0..built.len() {
                assert_eq!(
                    (spliced.parent(it), spliced.range(it), spliced.end(it), spliced.is_lazy(it)),
                    (built.parent(it), built.range(it), built.end(it), built.is_lazy(it)),
                    "seed {seed}, element {it}"
                );
            }
            assert!(spliced.lazies().eq(built.lazies()));
            assert_eq!(spliced.lazies().filter(|&it| spliced.is_parsed(it)).count(), carried);
            for it in spliced.lazies() {
                assert_eq!(spliced.part(it), built.part(it), "seed {seed}, lazy {it}");
            }
            compare(&build_green(&edited), &SyntaxNode::new_flat_root(Arc::new(spliced)), true, seed);
        }
    }

    #[test]
    fn replacing_in_a_big_tree_moves_what_is_past_the_word() {
        for seed in 0..3 {
            let shape = big_shape(seed);
            let tree = build_flat(&shape, true);
            let row = tree.children(tree.first_child(0).unwrap()).nth(seed as usize * 40).unwrap();
            let with = Shape::Node(tree.kind(row), vec![Shape::Token(SyntaxKind(3), "z".repeat(BIG as usize + 5))], None);
            let edited = replaced(&shape, row, &with, &mut 0);
            let mut text = String::new();
            text_of(&edited, &mut text);
            let spliced = tree.replace_subtree(row, &build_flat(&with, false), text.into());
            let built = build_flat(&edited, true);
            assert_eq!(spliced, built);
            assert!((0..built.len()).all(|it| spliced.parent(it) == built.parent(it)));
            compare(&build_green(&edited), &SyntaxNode::new_flat_root(Arc::new(spliced)), false, seed);
        }
    }

    #[test]
    fn equal_cursors_are_the_same_element() {
        let flat = SyntaxNode::new_flat_root(Arc::new(build_flat(&random_shape(7), false)));
        let all = elements(&flat);
        let again = elements(&SyntaxNode::new_flat_root(flat.flat_tree().unwrap().clone()));
        for (i, a) in all.iter().enumerate() {
            for (j, b) in again.iter().enumerate() {
                assert_eq!(a == b, i == j);
            }
        }
    }
}

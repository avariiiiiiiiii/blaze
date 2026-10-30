//! A token tree's storage: columns over `u32` indices, the top subtree first and every token
//! tree after it in preorder.
//!
//! - `nodes`: one [`Node`] (8 bytes) per token tree: a tag with small fields in one word, and a
//!   symbol index, a character or a subtree's length in the other. A subtree's children are the
//!   `len` nodes after it, so skipping one is one add, and any token tree is one index away.
//! - `spans`: one [`PackedSpan`] (8 bytes) per node, a leaf's span or a subtree's opening one
//!   (the top subtree's slot is unused), then one per subtree but the top for its closing span,
//!   in the subtrees' order. A packed span is a range and an index into `parts`, the (anchor,
//!   context) pairs the tree uses; a range or an index too large to pack goes to `wide`.
//! - `symbols`: the symbols of idents and literals.
//! - the top subtree's two spans, whole, so that they can be changed in place.
//!
//! A leaf costs 16 bytes and a subtree 24, with no allocation of its own: a tree is four boxed
//! slices whatever its shape. The tables list entries in the order the tree first uses them, so
//! the columns are a function of the token trees alone: two trees are equal, and hash equal,
//! exactly when their token trees are, whichever builder calls made them.

use std::{cell::Cell, sync::LazyLock};

use intern::Symbol;
use rustc_hash::FxHashMap;
use span::{
    Edition, EditionedFileId, FileId, ROOT_ERASED_FILE_AST_ID, Span, SpanAnchor, SyntaxContext,
    TextRange, TextSize,
};

use crate::{
    DelimSpan, Delimiter, DelimiterKind, Ident, IdentIsRaw, Leaf, LitKind, Literal, Punct, Spacing,
    Subtree, SubtreeView, TokenTree, TokenTreesView, TtIter,
};

#[cfg(test)]
mod tests;
mod wire;

pub use wire::{WireError, WirePart};
#[cfg(test)]
mod varint;

/// The parts of a span that tokens near each other share: all but the range. 12 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct CompressedSpanPart {
    anchor: SpanAnchor,
    ctx: SyntaxContext,
}

const _: () = assert!(size_of::<CompressedSpanPart>() == 12);

impl CompressedSpanPart {
    #[inline]
    fn from_span(span: &Span) -> Self {
        Self { anchor: span.anchor, ctx: span.ctx }
    }

    #[inline]
    fn recombine(&self, range: TextRange) -> Span {
        Span { range, anchor: self.anchor, ctx: self.ctx }
    }
}

/// One token tree, 8 bytes. `head` holds the tag in its low two bits and above it:
/// - a subtree: its delimiter kind (2 bits), then its ordinal (the number of subtrees before it,
///   the top one being 0); `data` is its length, the number of token trees in it;
/// - a punct: its spacing (2 bits); `data` is its character;
/// - an ident: whether it is raw (1 bit); `data` is its symbol's index;
/// - a literal: its kind (4 bits), the hashes of a raw one (8 bits) and its suffix's length
///   (the 18 bits left, below [`MAX_SUFFIX`]); `data` is its symbol's index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Node {
    head: u32,
    data: u32,
}

const _: () = assert!(size_of::<Node>() == 8);

const TAG: u32 = 0b11;
const SUBTREE: u32 = 0;
const PUNCT: u32 = 1;
const IDENT: u32 = 2;
const LITERAL: u32 = 3;
const ORDINAL_SHIFT: u32 = 4;
const MAX_ORDINAL: u32 = u32::MAX >> ORDINAL_SHIFT;
const SUFFIX_SHIFT: u32 = 14;
/// The longest suffix a literal node holds: 256 KiB, against rustc's none.
const MAX_SUFFIX: u32 = u32::MAX >> SUFFIX_SHIFT;

impl Node {
    #[inline]
    fn tag(self) -> u32 {
        self.head & TAG
    }

    #[inline]
    fn subtree(kind: DelimiterKind, ordinal: u32, len: u32) -> Node {
        assert!(ordinal <= MAX_ORDINAL, "a token tree of more than {MAX_ORDINAL} subtrees");
        Node { head: SUBTREE | (kind as u32) << 2 | ordinal << ORDINAL_SHIFT, data: len }
    }

    #[inline]
    fn ordinal(self) -> u32 {
        self.head >> ORDINAL_SHIFT
    }

    #[inline]
    fn with_ordinal(self, ordinal: u32) -> Node {
        Node::subtree(self.delimiter_kind(), ordinal, self.data)
    }

    #[inline]
    fn delimiter_kind(self) -> DelimiterKind {
        match (self.head >> 2) & 0b11 {
            0 => DelimiterKind::Parenthesis,
            1 => DelimiterKind::Brace,
            2 => DelimiterKind::Bracket,
            _ => DelimiterKind::Invisible,
        }
    }

    #[inline]
    fn punct(char: char, spacing: Spacing) -> Node {
        Node { head: PUNCT | (spacing as u32) << 2, data: char as u32 }
    }

    #[inline]
    fn spacing(self) -> Spacing {
        match (self.head >> 2) & 0b11 {
            0 => Spacing::Alone,
            1 => Spacing::Joint,
            _ => Spacing::JointHidden,
        }
    }

    #[inline]
    fn ident(is_raw: IdentIsRaw, symbol: u32) -> Node {
        Node { head: IDENT | (is_raw as u32) << 2, data: symbol }
    }

    #[inline]
    fn is_raw(self) -> IdentIsRaw {
        match (self.head >> 2) & 1 {
            0 => IdentIsRaw::No,
            _ => IdentIsRaw::Yes,
        }
    }

    #[inline]
    fn literal(kind: LitKind, suffix_len: u32, symbol: u32) -> Node {
        assert!(
            suffix_len <= MAX_SUFFIX,
            "a literal with a suffix of {suffix_len} bytes: a token tree holds at most {MAX_SUFFIX}"
        );
        let (code, hashes) = match kind {
            LitKind::Byte => (0, 0),
            LitKind::Char => (1, 0),
            LitKind::Integer => (2, 0),
            LitKind::Float => (3, 0),
            LitKind::Str => (4, 0),
            LitKind::StrRaw(hashes) => (5, hashes),
            LitKind::ByteStr => (6, 0),
            LitKind::ByteStrRaw(hashes) => (7, hashes),
            LitKind::CStr => (8, 0),
            LitKind::CStrRaw(hashes) => (9, hashes),
            LitKind::Err(()) => (10, 0),
        };
        Node {
            head: LITERAL | code << 2 | u32::from(hashes) << 6 | suffix_len << SUFFIX_SHIFT,
            data: symbol,
        }
    }

    #[inline]
    fn lit_kind(self) -> LitKind {
        let hashes = (self.head >> 6) as u8;
        match (self.head >> 2) & 0xF {
            0 => LitKind::Byte,
            1 => LitKind::Char,
            2 => LitKind::Integer,
            3 => LitKind::Float,
            4 => LitKind::Str,
            5 => LitKind::StrRaw(hashes),
            6 => LitKind::ByteStr,
            7 => LitKind::ByteStrRaw(hashes),
            8 => LitKind::CStr,
            9 => LitKind::CStrRaw(hashes),
            _ => LitKind::Err(()),
        }
    }

    #[inline]
    fn suffix_len(self) -> u32 {
        self.head >> SUFFIX_SHIFT
    }
}

/// A span, 8 bytes: where it starts, and its length in the low 16 bits of `len_part` with its
/// part's index in the high 16. A length or index of `0xFFFF` or more makes it wide: then
/// `len_part` is [`WIDE`] and `start` the index of a [`WideSpan`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct PackedSpan {
    start: u32,
    len_part: u32,
}

const _: () = assert!(size_of::<PackedSpan>() == 8);

const WIDE: u32 = u32::MAX;
const NARROW_LIMIT: u32 = 0xFFFF;

impl PackedSpan {
    const UNUSED: PackedSpan = PackedSpan { start: 0, len_part: 0 };
}

/// A span too large to pack, 12 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct WideSpan {
    start: u32,
    len: u32,
    part: u32,
}

/// Distinct entries in the order they are first asked for, each found by a scan while there are
/// few and through a map after.
struct Table<T> {
    list: Vec<T>,
    map: FxHashMap<T, u32>,
}

const SCAN: usize = 8;

impl<T: Copy + Eq + std::hash::Hash> Table<T> {
    fn new() -> Self {
        // alloc: the tree's parts column, allocated at its first part.
        Table { list: Vec::new(), map: FxHashMap::default() }
    }

    #[inline]
    fn index(&mut self, value: T) -> u32 {
        if self.list.len() <= SCAN {
            if let Some(at) = self.list.iter().position(|it| *it == value) {
                return at as u32;
            }
            let at = self.list.len() as u32;
            self.list.push(value);
            if self.list.len() > SCAN {
                self.map.reserve(4 * SCAN);
                self.map.extend(self.list.iter().enumerate().map(|(at, &it)| (it, at as u32)));
            }
            return at;
        }
        let next = self.list.len() as u32;
        let list = &mut self.list;
        *self.map.entry(value).or_insert_with(|| {
            list.push(value);
            next
        })
    }
}

/// How many of the latest symbols a new one is looked for among, once a tree has more than
/// that: symbols repeat close together (`self.a`, `self.b`), and a repeat further back costs 8
/// bytes where finding it would cost a hash per ident.
const SYMBOL_WINDOW: usize = 16;

/// The symbols of a tree, an index each: the same symbol has one while the tree has at most
/// [`SYMBOL_WINDOW`], and after that when it was among the last [`SYMBOL_WINDOW`] added.
struct Symbols {
    list: Vec<Symbol>,
}

impl Symbols {
    #[inline]
    fn index(&mut self, symbol: Symbol) -> u32 {
        let from = self.list.len().saturating_sub(SYMBOL_WINDOW);
        if let Some(at) = self.list[from..].iter().rposition(|&it| it == symbol) {
            return (from + at) as u32;
        }
        self.list.push(symbol);
        (self.list.len() - 1) as u32
    }
}

/// What a tree's columns are built with: its tables, filled in the order spans and symbols are
/// encoded.
struct Encoder {
    parts: Table<CompressedSpanPart>,
    last_part: Option<(CompressedSpanPart, u32)>,
    wide: Vec<WideSpan>,
    symbols: Symbols,
}

impl Encoder {
    fn new(symbols: usize) -> Self {
        Encoder {
            parts: Table::new(),
            last_part: None,
            // alloc: the tree's wide spans, allocated only for one too wide to pack.
            wide: Vec::new(),
            symbols: Symbols { list: Vec::with_capacity(symbols) },
        }
    }

    #[inline]
    fn span(&mut self, span: &Span) -> PackedSpan {
        let part = CompressedSpanPart::from_span(span);
        let index = match self.last_part {
            Some((last, index)) if last == part => index,
            _ => {
                let index = self.parts.index(part);
                self.last_part = Some((part, index));
                index
            }
        };
        let (start, len) = (u32::from(span.range.start()), u32::from(span.range.len()));
        if len < NARROW_LIMIT && index < NARROW_LIMIT {
            PackedSpan { start, len_part: len | index << 16 }
        } else {
            self.wide_span(WideSpan { start, len, part: index })
        }
    }

    #[cold]
    fn wide_span(&mut self, span: WideSpan) -> PackedSpan {
        self.wide.push(span);
        PackedSpan { start: (self.wide.len() - 1) as u32, len_part: WIDE }
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct TopSubtree {
    nodes: Box<[Node]>,
    spans: Box<[PackedSpan]>,
    wide: Box<[WideSpan]>,
    parts: Box<[CompressedSpanPart]>,
    symbols: Box<[Symbol]>,
    top_open: Span,
    top_close: Span,
}

/// The tree [`TokenTreesView::empty`] views none of.
static EMPTY: LazyLock<TopSubtree> = LazyLock::new(|| {
    let span = Span {
        range: TextRange::empty(TextSize::new(0)),
        anchor: SpanAnchor {
            file_id: EditionedFileId::new(FileId::from_raw(0), Edition::CURRENT),
            ast_id: ROOT_ERASED_FILE_AST_ID,
        },
        ctx: SyntaxContext::root(Edition::CURRENT),
    };
    TopSubtree::empty(DelimSpan::from_single(span))
});

#[inline]
pub(crate) fn empty_tree() -> &'static TopSubtree {
    &EMPTY
}

impl TopSubtree {
    pub fn empty(span: DelimSpan) -> Self {
        TopSubtreeBuilder::new(Delimiter::invisible_delim_spanned(span)).build()
    }

    pub fn invisible_from_leaves<const N: usize>(delim_span: Span, leaves: [Leaf; N]) -> Self {
        let mut builder =
            TopSubtreeBuilder::with_capacity(Delimiter::invisible_spanned(delim_span), N);
        builder.extend(leaves);
        builder.build()
    }

    pub fn from_token_trees(delimiter: Delimiter, token_trees: TokenTreesView<'_>) -> Self {
        let mut builder = TopSubtreeBuilder::with_capacity(delimiter, token_trees.len());
        builder.extend_with_tt(token_trees);
        builder.build()
    }

    /// The tree of `tts`, the top subtree first and every token tree after it in preorder.
    pub fn from_serialized(tts: Vec<TokenTree>) -> Self {
        let mut tts = tts.into_iter();
        let Some(TokenTree::Subtree(top_subtree)) = tts.next() else {
            panic!("must always have a top subtree");
        };
        let mut builder = TopSubtreeBuilder::with_capacity(top_subtree.delimiter, tts.len());
        tts.for_each(|tt| builder.push_token_tree(tt));
        builder.build()
    }

    pub fn from_subtree(subtree: SubtreeView<'_>) -> Self {
        let token_trees = subtree.token_trees();
        let mut builder =
            TopSubtreeBuilder::with_capacity(subtree.top_subtree().delimiter, token_trees.len());
        builder.extend_with_tt(token_trees);
        builder.build()
    }

    pub fn view(&self) -> SubtreeView<'_> {
        SubtreeView(TokenTreesView { tree: self, start: 0, end: self.nodes.len() as u32 })
    }

    pub fn iter(&self) -> TtIter<'_> {
        self.view().iter()
    }

    pub fn top_subtree(&self) -> Subtree {
        self.subtree_at(0)
    }

    pub fn set_top_subtree_delimiter_kind(&mut self, kind: DelimiterKind) {
        self.nodes[0] = Node::subtree(kind, 0, self.nodes[0].data);
    }

    pub fn set_top_subtree_delimiter_span(&mut self, span: DelimSpan) {
        self.top_open = span.open;
        self.top_close = span.close;
    }

    /// Puts `leaf` in place of the leaf at `idx` (the top subtree being 0). Rebuilds the tree,
    /// in one pass with no buffer but its own: avoid it in a loop.
    pub fn set_token(&mut self, idx: usize, leaf: Leaf) {
        assert!(idx > 0 && !self.is_subtree(idx as u32), "cannot change a subtree to a leaf");
        let mut builder =
            TopSubtreeBuilder::with_capacity(self.top_subtree().delimiter, self.nodes.len() - 1);
        let mut leaf = Some(leaf);
        for (at, tt) in (1..).zip(self.token_trees().iter_flat_tokens()) {
            match at == idx {
                true => builder.push(leaf.take().expect("one token tree is at `idx`")),
                false => builder.push_token_tree(tt),
            }
        }
        *self = builder.build();
    }

    pub fn token_trees(&self) -> TokenTreesView<'_> {
        self.view().token_trees()
    }

    pub fn as_token_trees(&self) -> TokenTreesView<'_> {
        self.view().as_token_trees()
    }

    pub fn change_every_ast_id(&mut self, mut callback: impl FnMut(&mut span::ErasedFileAstId)) {
        for part in &mut self.parts {
            callback(&mut part.anchor.ast_id);
        }
        callback(&mut self.top_open.anchor.ast_id);
        callback(&mut self.top_close.anchor.ast_id);
        // Two parts made one would leave the table with a part twice, which a tree built from
        // the same token trees doesn't have.
        if has_duplicates(&self.parts) {
            *self = TopSubtree::from_token_trees(self.top_subtree().delimiter, self.token_trees());
        }
    }

    #[inline]
    fn unpack(&self, span: PackedSpan) -> Span {
        if span.len_part == WIDE {
            return self.unpack_wide(span.start);
        }
        let len = TextSize::new(span.len_part & NARROW_LIMIT);
        self.parts[(span.len_part >> 16) as usize]
            .recombine(TextRange::at(TextSize::new(span.start), len))
    }

    #[cold]
    fn unpack_wide(&self, at: u32) -> Span {
        let WideSpan { start, len, part } = self.wide[at as usize];
        self.parts[part as usize].recombine(TextRange::at(TextSize::new(start), TextSize::new(len)))
    }

    #[inline]
    pub(crate) fn is_subtree(&self, at: u32) -> bool {
        self.nodes[at as usize].tag() == SUBTREE
    }

    /// The length of the subtree at `at`, which is one.
    #[inline]
    pub(crate) fn subtree_len(&self, at: u32) -> u32 {
        self.nodes[at as usize].data
    }

    /// The delimiter kind of the subtree at `at`, which is one.
    #[inline]
    pub(crate) fn delimiter_kind_at(&self, at: u32) -> DelimiterKind {
        self.nodes[at as usize].delimiter_kind()
    }

    /// The subtree at `at`, which is one.
    #[inline]
    pub(crate) fn subtree_at(&self, at: u32) -> Subtree {
        let node = self.nodes[at as usize];
        debug_assert_eq!(node.tag(), SUBTREE);
        let (open, close) = match at {
            0 => (self.top_open, self.top_close),
            _ => (self.unpack(self.spans[at as usize]), self.close_span(node)),
        };
        Subtree { delimiter: Delimiter { open, close, kind: node.delimiter_kind() }, len: node.data }
    }

    #[inline]
    fn close_span(&self, node: Node) -> Span {
        self.unpack(self.spans[self.nodes.len() + node.ordinal() as usize - 1])
    }

    /// The leaf at `at`, which is one.
    #[inline]
    pub(crate) fn leaf_at(&self, at: u32) -> Leaf {
        let node = self.nodes[at as usize];
        let span = self.unpack(self.spans[at as usize]);
        match node.tag() {
            PUNCT => Leaf::Punct(Punct {
                char: char::from_u32(node.data).expect("a punct's node holds a char"),
                spacing: node.spacing(),
                span,
            }),
            IDENT => Leaf::Ident(Ident {
                sym: self.symbols[node.data as usize],
                span,
                is_raw: node.is_raw(),
            }),
            LITERAL => Leaf::Literal(Literal {
                text_and_suffix: self.symbols[node.data as usize],
                span,
                kind: node.lit_kind(),
                suffix_len: node.suffix_len(),
            }),
            _ => unreachable!("token tree {at} is a subtree"),
        }
    }

    #[inline]
    pub(crate) fn token_tree_at(&self, at: u32) -> TokenTree {
        match self.is_subtree(at) {
            true => TokenTree::Subtree(self.subtree_at(at)),
            false => TokenTree::Leaf(self.leaf_at(at)),
        }
    }

    /// A leaf's span, or a subtree's opening one.
    #[inline]
    pub(crate) fn first_span_at(&self, at: u32) -> Span {
        match at {
            0 => self.top_open,
            _ => self.unpack(self.spans[at as usize]),
        }
    }

    /// A leaf's span, or a subtree's closing one.
    #[inline]
    pub(crate) fn last_span_at(&self, at: u32) -> Span {
        let node = self.nodes[at as usize];
        match (node.tag(), at) {
            (SUBTREE, 0) => self.top_close,
            (SUBTREE, _) => self.close_span(node),
            _ => self.unpack(self.spans[at as usize]),
        }
    }
}

thread_local! {
    /// The subtrees a [`TopSubtree::for_each_token`] walk is in, each as the index of the node
    /// after it and its own, kept for the next walk.
    // alloc: one buffer per thread, grown to the deepest nesting walked.
    static OPEN: Cell<Vec<(u32, u32)>> = const { Cell::new(Vec::new()) };
}

impl TopSubtree {
    /// The context of a packed span: its part's, with no range unpacked.
    #[inline]
    fn ctx_of(&self, span: PackedSpan) -> SyntaxContext {
        let part = match span.len_part {
            WIDE => self.wide[span.start as usize].part,
            len_part => len_part >> 16,
        };
        self.parts[part as usize].ctx
    }

    /// The context of the closing span of the subtree at `at`.
    #[inline]
    fn close_ctx_at(&self, at: u32) -> SyntaxContext {
        match at {
            0 => self.top_close.ctx,
            _ => {
                let ordinal = self.nodes[at as usize].ordinal() as usize;
                self.ctx_of(self.spans[self.nodes.len() + ordinal - 1])
            }
        }
    }

    #[inline]
    fn close_token(&self, subtree: u32) -> (crate::FlatToken, SyntaxContext) {
        let kind = self.nodes[subtree as usize].delimiter_kind();
        (crate::FlatToken::Close(kind), self.close_ctx_at(subtree))
    }

    /// Calls `f` with each token of the token trees `start..end` in order, a subtree being its
    /// opening delimiter, its token trees and its closing delimiter, with the context of each.
    /// Reads the columns: no span's range and no leaf is unpacked, and nothing is allocated
    /// but the stack of open subtrees, kept per thread.
    pub(crate) fn for_each_token(
        &self,
        start: u32,
        end: u32,
        mut f: impl FnMut(crate::FlatToken, SyntaxContext),
    ) {
        use crate::FlatToken;
        let mut open = OPEN.take();
        open.clear();
        let mut from = start as usize;
        if start == 0 && end > 0 {
            let top = self.nodes[0];
            open.push((1 + top.data, 0));
            f(FlatToken::Open(top.delimiter_kind()), self.top_open.ctx);
            from = 1;
        }
        let end = end as usize;
        let columns = self.nodes[from..end].iter().zip(&self.spans[from..end]);
        for (at, (&node, &span)) in (from as u32..).zip(columns) {
            while let Some(&(after, subtree)) = open.last()
                && after == at
            {
                open.pop();
                let (close, ctx) = self.close_token(subtree);
                f(close, ctx);
            }
            let token = match node.tag() {
                SUBTREE => {
                    open.push((at + 1 + node.data, at));
                    FlatToken::Open(node.delimiter_kind())
                }
                PUNCT => FlatToken::Punct {
                    char: char::from_u32(node.data).expect("a punct's node holds a char"),
                    spacing: node.spacing(),
                },
                IDENT => {
                    FlatToken::Ident { sym: self.symbols[node.data as usize], is_raw: node.is_raw() }
                }
                _ => FlatToken::Literal {
                    sym: self.symbols[node.data as usize],
                    kind: node.lit_kind(),
                    suffix_len: node.suffix_len(),
                },
            };
            f(token, self.ctx_of(span));
        }
        while let Some((_, subtree)) = open.pop() {
            let (close, ctx) = self.close_token(subtree);
            f(close, ctx);
        }
        OPEN.set(open);
    }
}

fn has_duplicates(parts: &[CompressedSpanPart]) -> bool {
    if parts.len() <= SCAN {
        return parts.iter().enumerate().any(|(at, part)| parts[..at].contains(part));
    }
    let mut seen = rustc_hash::FxHashSet::default();
    seen.reserve(parts.len());
    !parts.iter().all(|part| seen.insert(*part))
}

/// A builder's buffers, kept per thread between builds, so that building allocates only the
/// tree.
#[derive(Default)]
struct Scratch {
    nodes: Vec<Node>,
    spans: Vec<Span>,
    closes: Vec<Span>,
    symbols: Vec<Symbol>,
    unclosed: Vec<u32>,
}

/// The most token trees whose buffers are kept for the next build.
const KEEP: usize = 1 << 16;

thread_local! {
    static SCRATCH: Cell<Option<Scratch>> = const { Cell::new(None) };
}

impl Scratch {
    fn take() -> Scratch {
        SCRATCH.with(Cell::take).unwrap_or_default()
    }

    fn give_back(mut self) {
        if self.nodes.capacity() > KEEP || self.closes.capacity() > KEEP {
            return;
        }
        self.nodes.clear();
        self.spans.clear();
        self.closes.clear();
        self.symbols.clear();
        self.unclosed.clear();
        SCRATCH.with(|it| it.set(Some(self)));
    }
}

/// Records token trees as they come; [`Self::build`] packs them into a [`TopSubtree`] in one pass.
#[derive(Debug)]
pub struct TopSubtreeBuilder {
    /// The token trees, the top subtree first. An ident's or literal's `data` is its symbol's
    /// index in `symbols`; a subtree's ordinal is its closing span's index in `closes`.
    nodes: Vec<Node>,
    /// Per node, its span or (for a subtree) its opening span.
    spans: Vec<Span>,
    closes: Vec<Span>,
    symbols: Vec<Symbol>,
    unclosed_subtree_indices: Vec<u32>,
    last_closed_subtree: Option<u32>,
}

impl TopSubtreeBuilder {
    pub fn new(top_delimiter: Delimiter) -> Self {
        Self::with_capacity(top_delimiter, 0)
    }

    /// A builder with room for `len` token trees besides the top subtree.
    pub fn with_capacity(top_delimiter: Delimiter, len: usize) -> Self {
        let Scratch { mut nodes, mut spans, mut closes, symbols, unclosed } = Scratch::take();
        nodes.reserve(len + 1);
        spans.reserve(len + 1);
        nodes.push(Node::subtree(top_delimiter.kind, 0, 0));
        spans.push(top_delimiter.open);
        closes.push(top_delimiter.close);
        Self {
            nodes,
            spans,
            closes,
            symbols,
            unclosed_subtree_indices: unclosed,
            last_closed_subtree: None,
        }
    }

    /// Not to be exposed, this assumes the subtree's children will be filled in immediately.
    fn push_subtree(&mut self, subtree: Subtree) {
        let ordinal = self.closes.len() as u32;
        self.nodes.push(Node::subtree(subtree.delimiter.kind, ordinal, subtree.len));
        self.spans.push(subtree.delimiter.open);
        self.closes.push(subtree.delimiter.close);
    }

    pub fn open(&mut self, delimiter_kind: DelimiterKind, open_span: Span) {
        let subtree_idx = self.nodes.len() as u32;
        self.push_subtree(Subtree {
            delimiter: Delimiter { open: open_span, close: open_span, kind: delimiter_kind },
            len: 0, // Will be overwritten on close.
        });
        self.unclosed_subtree_indices.push(subtree_idx);
    }

    pub fn close(&mut self, close_span: Span) {
        let last_unclosed_index = self
            .unclosed_subtree_indices
            .pop()
            .expect("attempt to close a `tt::Subtree` when none is open");
        let len = self.nodes.len() as u32 - last_unclosed_index - 1;
        let node = &mut self.nodes[last_unclosed_index as usize];
        node.data = len;
        self.closes[node.ordinal() as usize] = close_span;
        self.last_closed_subtree = Some(last_unclosed_index);
    }

    /// You cannot call this consecutively, it will only work once after close.
    pub fn remove_last_subtree_if_invisible(&mut self) {
        let Some(last_subtree_idx) = self.last_closed_subtree.take() else { return };
        if self.nodes[last_subtree_idx as usize].delimiter_kind() == DelimiterKind::Invisible {
            self.nodes.remove(last_subtree_idx as usize);
            self.spans.remove(last_subtree_idx as usize);
        }
    }

    pub fn push(&mut self, leaf: Leaf) {
        let (node, span) = match leaf {
            Leaf::Punct(Punct { char, spacing, span }) => (Node::punct(char, spacing), span),
            Leaf::Ident(Ident { sym, span, is_raw }) => {
                self.symbols.push(sym);
                (Node::ident(is_raw, (self.symbols.len() - 1) as u32), span)
            }
            Leaf::Literal(Literal { text_and_suffix, span, kind, suffix_len }) => {
                self.symbols.push(text_and_suffix);
                (Node::literal(kind, suffix_len, (self.symbols.len() - 1) as u32), span)
            }
        };
        self.nodes.push(node);
        self.spans.push(span);
    }

    fn push_token_tree(&mut self, tt: TokenTree) {
        match tt {
            TokenTree::Leaf(leaf) => self.push(leaf),
            TokenTree::Subtree(subtree) => self.push_subtree(subtree),
        }
    }

    pub fn extend(&mut self, leaves: impl IntoIterator<Item = Leaf>) {
        leaves.into_iter().for_each(|leaf| self.push(leaf));
    }

    pub fn extend_with_tt(&mut self, tt: TokenTreesView<'_>) {
        self.nodes.reserve(tt.len());
        self.spans.reserve(tt.len());
        tt.iter_flat_tokens().for_each(|tt| self.push_token_tree(tt));
    }

    /// Like [`Self::extend_with_tt()`], but makes sure the new tokens will never be
    /// joint with whatever comes after them.
    pub fn extend_with_tt_alone(&mut self, tt: TokenTreesView<'_>) {
        self.extend_with_tt(tt);
        if !tt.is_empty()
            && let Some(last) = self.nodes.last_mut()
            && last.tag() == PUNCT
        {
            last.head = PUNCT | (Spacing::Alone as u32) << 2;
        }
    }

    pub fn expected_delimiters(&self) -> impl Iterator<Item = DelimiterKind> {
        self.unclosed_subtree_indices
            .iter()
            .rev()
            .map(|&subtree_idx| self.nodes[subtree_idx as usize].delimiter_kind())
    }

    /// Builds, and remove the top subtree if it has only one subtree child.
    pub fn build_skip_top_subtree(self) -> TopSubtree {
        assert!(
            self.unclosed_subtree_indices.is_empty(),
            "attempt to build an unbalanced `TopSubtreeBuilder`"
        );
        let tt_len = self.nodes.len();
        if let Some(&first) = self.nodes.get(1)
            && first.tag() == SUBTREE
            && first.data as usize == tt_len - 2
        {
            let top = Delimiter {
                open: self.spans[1],
                close: self.closes[first.ordinal() as usize],
                kind: first.delimiter_kind(),
            };
            self.finish(2, top)
        } else {
            self.build()
        }
    }

    pub fn build(self) -> TopSubtree {
        assert!(
            self.unclosed_subtree_indices.is_empty(),
            "attempt to build an unbalanced `TopSubtreeBuilder`"
        );
        let top = Delimiter {
            open: self.spans[0],
            close: self.closes[0],
            kind: self.nodes[0].delimiter_kind(),
        };
        self.finish(1, top)
    }

    /// The tree of `top` holding the token trees from `first` on, packed in one pass.
    fn finish(self, first: usize, top: Delimiter) -> TopSubtree {
        let TopSubtreeBuilder { nodes, spans, closes, symbols, unclosed_subtree_indices, .. } =
            self;
        let len = nodes.len() - first + 1;
        let subtrees = nodes[first..].iter().filter(|it| it.tag() == SUBTREE).count();
        let mut out_nodes = Vec::with_capacity(len);
        // alloc: the tree's span column, its exact size.
        let mut out_spans = vec![PackedSpan::UNUSED; len + subtrees];
        let mut encoder = Encoder::new(symbols.len().min(len));
        out_nodes.push(Node::subtree(top.kind, 0, (len - 1) as u32));
        let mut ordinal = 0;
        for (at, (&node, span)) in nodes[first..].iter().zip(&spans[first..]).enumerate() {
            out_spans[at + 1] = encoder.span(span);
            out_nodes.push(match node.tag() {
                SUBTREE => {
                    ordinal += 1;
                    out_spans[len + ordinal - 1] = encoder.span(&closes[node.ordinal() as usize]);
                    node.with_ordinal(ordinal as u32)
                }
                PUNCT => node,
                _ => Node {
                    head: node.head,
                    data: encoder.symbols.index(symbols[node.data as usize]),
                },
            });
        }
        let tree = TopSubtree {
            nodes: out_nodes.into_boxed_slice(),
            spans: out_spans.into_boxed_slice(),
            wide: encoder.wide.into_boxed_slice(),
            parts: encoder.parts.list.into_boxed_slice(),
            symbols: encoder.symbols.list.into_boxed_slice(),
            top_open: top.open,
            top_close: top.close,
        };
        Scratch { nodes, spans, closes, symbols, unclosed: unclosed_subtree_indices }.give_back();
        tree
    }

    pub fn restore_point(&mut self) -> SubtreeBuilderRestorePoint {
        // We reset the `last_closed_subtree`, since restoring from a restore point doesn't play well with removing the last subtree.
        self.last_closed_subtree = None;
        SubtreeBuilderRestorePoint {
            unclosed_subtree_indices_len: self.unclosed_subtree_indices.len(),
            token_trees_len: self.nodes.len(),
        }
    }

    /// Drops the token trees pushed since `restore_point`. Their symbols and closing spans stay
    /// in the builder's buffers, unused: no node refers to them, so the tree doesn't hold them.
    pub fn restore(&mut self, restore_point: SubtreeBuilderRestorePoint) {
        if restore_point.token_trees_len >= self.nodes.len() {
            // This means we restored twice, potentially with an earlier restore point first.
            return;
        }
        self.nodes.truncate(restore_point.token_trees_len);
        self.spans.truncate(restore_point.token_trees_len);
        self.unclosed_subtree_indices.truncate(restore_point.unclosed_subtree_indices_len);
        self.last_closed_subtree = None;
    }
}

#[derive(Clone, Copy)]
pub struct SubtreeBuilderRestorePoint {
    unclosed_subtree_indices_len: usize,
    token_trees_len: usize,
}

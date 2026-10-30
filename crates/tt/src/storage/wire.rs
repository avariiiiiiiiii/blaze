//! A token tree's columns as bytes, for the proc-macro wire: the columns themselves, written as
//! they are and read back checked, with no token tree built on either side.
//!
//! The bytes are little-endian `u32` words: a header (`MAGIC`, then the lengths of `nodes`,
//! `wide`, `parts` and `symbols` and the symbols' bytes), the nodes (two words each), the packed
//! spans (two each), the wide spans (three), the parts (three: file, AST id and context, as
//! [`WirePart`]), each symbol's end offset, the top subtree's two spans (five words each: start,
//! length and a [`WirePart`]), then the symbols' UTF-8, padded to a word.
//!
//! Reading checks everything a tree's methods rely on (tags, lengths nesting, ordinals, indices,
//! chars, UTF-8, ranges) and that the columns are the ones the builder makes for those token
//! trees, so that a tree read compares and hashes as the same tree built. It allocates at most a
//! constant times the bytes it is given, so their length bounds it.

use intern::Symbol;
use span::{SpanAnchor, SyntaxContext, TextRange, TextSize};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use super::{
    CompressedSpanPart, IDENT, LITERAL, NARROW_LIMIT, Node, PUNCT, PackedSpan, SUBTREE,
    SYMBOL_WINDOW, TopSubtree, WIDE, WideSpan, has_duplicates,
};

/// The first word, naming the layout; a change to it changes this.
const MAGIC: u32 = u32::from_le_bytes(*b"tt\x01\x00");
const HEADER_WORDS: usize = 6;

const _: () = assert!(
    cfg!(target_endian = "little"),
    "the wire carries the columns as they are in memory, its little-endian words on a \
     little-endian host only"
);

/// A span's anchor and context as the wire carries them: raw ids, which the side reading them
/// turns back into its own (see [`TopSubtree::read_wire`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WirePart {
    pub file_id: u32,
    pub ast_id: u32,
    pub ctx: u32,
}

impl WirePart {
    fn words(self) -> [u32; 3] {
        [self.file_id, self.ast_id, self.ctx]
    }

    fn of(part: CompressedSpanPart) -> WirePart {
        WirePart {
            file_id: part.anchor.file_id.as_u32(),
            ast_id: part.anchor.ast_id.into_raw(),
            ctx: part.ctx.into_u32(),
        }
    }
}

/// Why bytes are not a token tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireError {
    /// The bytes end at `at` where `needed` more were to come.
    Truncated { at: usize, needed: usize },
    /// The first word is not [`MAGIC`].
    Magic(u32),
    /// A column breaks a rule of the layout: what, at which index of its column.
    Malformed { what: &'static str, at: usize },
    /// The side reading refused a part.
    Part { at: usize, part: WirePart, why: String },
    /// Well formed, but not the columns the builder makes for these token trees.
    NotCanonical { what: &'static str, at: usize },
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireError::Truncated { at, needed } => {
                write!(f, "token tree bytes end at {at}, {needed} more expected")
            }
            WireError::Magic(word) => write!(f, "token tree bytes start with {word:#x}"),
            WireError::Malformed { what, at } => write!(f, "token tree bytes: {what} at {at}"),
            WireError::Part { at, part, why } => {
                write!(f, "token tree bytes: span part {at} ({part:?}) refused: {why}")
            }
            WireError::NotCanonical { what, at } => {
                write!(f, "token tree bytes: {what} at {at} is not what the builder makes")
            }
        }
    }
}

impl std::error::Error for WireError {}

fn malformed<T>(what: &'static str, at: usize) -> Result<T, WireError> {
    Err(WireError::Malformed { what, at })
}

impl TopSubtree {
    /// Appends the tree's columns to `out`, as [`TopSubtree::read_wire`] reads them: at most one
    /// allocation, the nodes and spans copied as their bytes and the rest written as rows.
    pub fn write_wire(&self, out: &mut Vec<u8>) {
        let len = |it: usize| u32::try_from(it).expect("a token tree column fits in u32");
        let symbol_bytes: usize = self.symbols.iter().map(|it| it.as_str().len()).sum();
        let words = HEADER_WORDS
            + 2 * self.nodes.len()
            + 2 * self.spans.len()
            + 3 * self.wide.len()
            + 3 * self.parts.len()
            + self.symbols.len()
            + 10;
        let base = out.len();
        let end_of_tree = base + 4 * words + symbol_bytes.next_multiple_of(4);
        out.reserve(end_of_tree - base);
        put_rows(
            out,
            [[
                MAGIC,
                len(self.nodes.len()),
                len(self.wide.len()),
                len(self.parts.len()),
                len(self.symbols.len()),
                len(symbol_bytes),
            ]],
        );
        out.extend_from_slice(self.nodes.as_bytes());
        out.extend_from_slice(self.spans.as_bytes());
        out.extend_from_slice(self.wide.as_bytes());
        put_rows(out, self.parts.iter().map(|&it| WirePart::of(it).words()));
        let mut end = 0;
        put_rows(
            out,
            self.symbols.iter().map(|it| {
                end += it.as_str().len();
                [end as u32]
            }),
        );
        put_rows(
            out,
            [self.top_open, self.top_close].iter().map(|span| {
                let [file_id, ast_id, ctx] =
                    WirePart::of(CompressedSpanPart::from_span(span)).words();
                [span.range.start().into(), span.range.len().into(), file_id, ast_id, ctx]
            }),
        );
        for symbol in &self.symbols {
            out.extend_from_slice(symbol.as_str().as_bytes());
        }
        debug_assert_eq!(out.len(), base + 4 * words + symbol_bytes, "the rows are the words counted");
        out.resize(end_of_tree, 0);
    }

    /// Reads a tree [`TopSubtree::write_wire`] wrote at the start of `bytes`, and how many bytes
    /// it took. `part` turns each [`WirePart`] into the anchor and context it stands for here,
    /// or refuses it: the ids are the writer's, and only the reader knows which it may use.
    /// `bytes` start at a 4-byte boundary, as a heap buffer or a tree after another one does.
    pub fn read_wire(
        bytes: &[u8],
        part: &mut impl FnMut(WirePart) -> Result<(SpanAnchor, SyntaxContext), String>,
    ) -> Result<(TopSubtree, usize), WireError> {
        let mut words = Words { bytes, at: 0 };
        let [magic, nodes_len, wide_len, parts_len, symbols_len, symbol_bytes] =
            words.rows::<6>(1)?.next().expect("one header row is read");
        if magic != MAGIC {
            return Err(WireError::Magic(magic));
        }
        let [nodes_len, wide_len, parts_len, symbols_len, symbol_bytes] =
            [nodes_len, wide_len, parts_len, symbols_len, symbol_bytes].map(|it| it as usize);
        if nodes_len == 0 {
            return malformed("a tree without its top subtree", 0);
        }
        // The node and span columns are checked where they lie in the bytes, and copied only
        // once the whole tree is, so the allocations are at most the bytes given.
        let nodes: &[Node] = words.column(nodes_len)?;
        let subtrees = check_nodes(nodes, symbols_len)?;
        let spans: &[PackedSpan] = words.column(nodes_len + subtrees)?;
        let wide: &[WideSpan] = words.column(wide_len)?;
        for (at, it) in wide.iter().enumerate() {
            if it.part as usize >= parts_len {
                return malformed("a wide span's part past the parts", at);
            }
            if it.start.checked_add(it.len).is_none() {
                return malformed("a wide span ending past 4 GiB", at);
            }
        }
        check_spans(spans, wide_len, parts_len)?;
        let mut parts = Vec::with_capacity(parts_len);
        for (at, [file_id, ast_id, ctx]) in words.rows::<3>(parts_len)?.enumerate() {
            parts.push(resolve(part, WirePart { file_id, ast_id, ctx }, at)?);
        }
        // Read twice from the bytes, checked now and used once the text is read.
        let mut ends = Words { bytes, at: words.at };
        let mut last = 0;
        for (at, [end]) in words.rows::<1>(symbols_len)?.enumerate() {
            let end = end as usize;
            if end < last || end > symbol_bytes {
                return malformed("a symbol ending before the last or past the symbols' bytes", at);
            }
            last = end;
        }
        if last != symbol_bytes {
            return malformed("symbols' bytes past the last symbol", symbols_len);
        }
        let mut top = [None; 2];
        for (at, [start, len, file_id, ast_id, ctx]) in words.rows::<5>(2)?.enumerate() {
            let Some(end) = start.checked_add(len) else {
                return malformed("a top span ending past 4 GiB", at);
            };
            let resolved = resolve(part, WirePart { file_id, ast_id, ctx }, parts_len + at)?;
            let range = TextRange::new(TextSize::new(start), TextSize::new(end));
            top[at] = Some(resolved.recombine(range));
        }
        let [Some(top_open), Some(top_close)] = top else { unreachable!("two top spans are read") };
        let text = words.bytes(symbol_bytes)?;
        let text = std::str::from_utf8(text).map_err(|e| WireError::Malformed {
            what: "symbols that are not UTF-8",
            at: e.valid_up_to(),
        })?;
        let mut symbols = Vec::with_capacity(symbols_len);
        let mut from = 0;
        for (at, [end]) in ends.rows::<1>(symbols_len)?.enumerate() {
            let end = end as usize;
            let Some(symbol) = text.get(from..end) else {
                return malformed("a symbol ending inside a char", at);
            };
            symbols.push(Symbol::intern(symbol));
            from = end;
        }
        words.pad()?;
        let tree = TopSubtree {
            // alloc: the tree's columns, each its exact size.
            nodes: Box::from(nodes),
            // alloc: as above.
            spans: Box::from(spans),
            // alloc: as above.
            wide: Box::from(wide),
            parts: parts.into_boxed_slice(),
            symbols: symbols.into_boxed_slice(),
            top_open,
            top_close,
            words: Default::default(),
        };
        check_literals(&tree)?;
        check_canonical(&tree)?;
        Ok((tree, words.at))
    }
}

fn resolve(
    part: &mut impl FnMut(WirePart) -> Result<(SpanAnchor, SyntaxContext), String>,
    wire: WirePart,
    at: usize,
) -> Result<CompressedSpanPart, WireError> {
    match part(wire) {
        Ok((anchor, ctx)) => Ok(CompressedSpanPart { anchor, ctx }),
        Err(why) => Err(WireError::Part { at, part: wire, why }),
    }
}

/// Checks each node is one a constructor makes, every subtree's children end inside its parent,
/// and ordinals count the subtrees in order. Returns how many subtrees there are, the top's
/// aside.
fn check_nodes(nodes: &[Node], symbols: usize) -> Result<usize, WireError> {
    let top = nodes[0];
    if top.tag() != SUBTREE
        || top != Node::subtree(top.delimiter_kind(), 0, top.data)
        || top.data as usize != nodes.len() - 1
    {
        return malformed("a top subtree not holding the whole tree", 0);
    }
    // Where each open subtree ends, the innermost last.
    // alloc: one buffer per tree read, as deep as its nesting.
    let mut ends: Vec<usize> = vec![nodes.len()];
    let mut ordinal = 0;
    for (at, &node) in nodes.iter().enumerate().skip(1) {
        while ends.last() == Some(&at) {
            ends.pop();
        }
        let same = match node.tag() {
            SUBTREE => {
                ordinal += 1;
                if node.ordinal() != ordinal {
                    return malformed("a subtree out of order", at);
                }
                let end = at + 1 + node.data as usize;
                if ends.last().is_none_or(|&parent| end > parent) {
                    return malformed("a subtree ending past its parent", at);
                }
                ends.push(end);
                Node::subtree(node.delimiter_kind(), ordinal, node.data)
            }
            PUNCT => {
                let Some(char) = char::from_u32(node.data) else {
                    return malformed("a punct that is not a char", at);
                };
                Node::punct(char, node.spacing())
            }
            IDENT => {
                if node.data as usize >= symbols {
                    return malformed("an ident's symbol past the symbols", at);
                }
                Node::ident(node.is_raw(), node.data)
            }
            LITERAL => {
                if node.data as usize >= symbols {
                    return malformed("a literal's symbol past the symbols", at);
                }
                Node::literal(node.lit_kind(), node.suffix_len(), node.data)
            }
            _ => unreachable!("a tag is two bits"),
        };
        if same != node {
            return malformed("a node with bits no constructor sets", at);
        }
    }
    Ok(ordinal as usize)
}

/// Checks each packed span indexes what is there and ends below 4 GiB.
fn check_spans(spans: &[PackedSpan], wide: usize, parts: usize) -> Result<(), WireError> {
    if spans[0] != PackedSpan::UNUSED {
        return malformed("the top subtree's unused span set", 0);
    }
    for (at, span) in spans.iter().enumerate().skip(1) {
        if span.len_part == WIDE {
            if span.start as usize >= wide {
                return malformed("a wide span past the wide spans", at);
            }
            continue;
        }
        let (len, part) = (span.len_part & NARROW_LIMIT, span.len_part >> 16);
        if len == NARROW_LIMIT {
            return malformed("a packed span as long as only a wide one is", at);
        }
        if part as usize >= parts {
            return malformed("a span's part past the parts", at);
        }
        if span.start.checked_add(len).is_none() {
            return malformed("a span ending past 4 GiB", at);
        }
    }
    Ok(())
}

/// Checks each literal's suffix is a tail of its text, as a literal's methods slice it.
fn check_literals(tree: &TopSubtree) -> Result<(), WireError> {
    for (at, node) in tree.nodes.iter().enumerate() {
        if node.tag() != LITERAL {
            continue;
        }
        let text = tree.symbols[node.data as usize].as_str();
        let suffix = node.suffix_len() as usize;
        if suffix > text.len() || !text.is_char_boundary(text.len() - suffix) {
            return malformed("a literal's suffix longer than its text or inside a char", at);
        }
    }
    Ok(())
}

/// Checks the columns are the builder's for these token trees, from indices alone, in the
/// order the builder encodes (each node's span, then a subtree's closing one): each span uses a
/// part already used or the next one, the parts are distinct, a span is wide exactly when it
/// doesn't pack and the wide ones are in order, and each ident or literal has the index the
/// builder's window gives its symbol. No span is unpacked and no part compared but once.
fn check_canonical(tree: &TopSubtree) -> Result<(), WireError> {
    let not = |what, at| Err(WireError::NotCanonical { what, at });
    let len = tree.nodes.len();
    let mut order = Order { parts: 0, wide: 0 };
    let mut symbols = 0;
    for (at, &node) in tree.nodes.iter().enumerate().skip(1) {
        order.span(tree, tree.spans[at], at)?;
        match node.tag() {
            SUBTREE => {
                let at = len + node.ordinal() as usize - 1;
                order.span(tree, tree.spans[at], at)?;
            }
            PUNCT => {}
            _ => {
                let index = node.data as usize;
                let from = symbols.saturating_sub(SYMBOL_WINDOW);
                let symbol = tree.symbols[index];
                if index == symbols {
                    if tree.symbols[from..symbols].contains(&symbol) {
                        return not("a symbol again in its window", at);
                    }
                    symbols += 1;
                } else if !(from..symbols).contains(&index)
                    || tree.symbols[index + 1..symbols].contains(&symbol)
                {
                    return not("a symbol's index other than its latest in the window", at);
                }
            }
        }
    }
    if order.parts != tree.parts.len() {
        return not("a part no span uses", order.parts);
    }
    if order.wide != tree.wide.len() {
        return not("a wide span no span uses", order.wide);
    }
    if symbols != tree.symbols.len() {
        return not("a symbol no node uses", symbols);
    }
    if has_duplicates(&tree.parts) {
        return not("a part twice", 0);
    }
    Ok(())
}

/// How many parts and wide spans the spans checked so far use.
struct Order {
    parts: usize,
    wide: usize,
}

impl Order {
    #[inline]
    fn span(&mut self, tree: &TopSubtree, span: PackedSpan, at: usize) -> Result<(), WireError> {
        let not = |what| Err(WireError::NotCanonical { what, at });
        let part = if span.len_part == WIDE {
            if span.start as usize != self.wide {
                return not("a wide span out of order");
            }
            self.wide += 1;
            let wide = tree.wide[span.start as usize];
            if wide.len < NARROW_LIMIT && wide.part < NARROW_LIMIT {
                return not("a wide span that packs");
            }
            wide.part
        } else {
            span.len_part >> 16
        };
        match part as usize {
            part if part < self.parts => Ok(()),
            part if part == self.parts => {
                self.parts += 1;
                Ok(())
            }
            _ => not("a part used before the ones before it"),
        }
    }
}

/// Where [`TopSubtree::write_wire`] writes next: the rest of its buffer.
/// Appends `rows` of `N` words to `out`, which has room for them.
#[inline]
fn put_rows<const N: usize>(out: &mut Vec<u8>, rows: impl IntoIterator<Item = [u32; N]>) {
    for row in rows {
        for word in row {
            out.extend_from_slice(&word.to_le_bytes());
        }
    }
}

/// A cursor over the bytes' words.
struct Words<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Words<'a> {
    fn bytes(&mut self, len: usize) -> Result<&'a [u8], WireError> {
        match self.bytes.get(self.at..).and_then(|it| it.get(..len)) {
            Some(it) => {
                self.at += len;
                Ok(it)
            }
            None => Err(WireError::Truncated { at: self.at, needed: len }),
        }
    }

    /// The next `count` rows of `N` words, taken whole first.
    fn rows<const N: usize>(
        &mut self,
        count: usize,
    ) -> Result<impl ExactSizeIterator<Item = [u32; N]> + use<'a, N>, WireError> {
        let bytes = self.bytes(count.saturating_mul(4 * N))?;
        Ok(bytes.chunks_exact(4 * N).map(|row| {
            std::array::from_fn(|i| {
                u32::from_le_bytes(row[4 * i..4 * i + 4].try_into().expect("four bytes"))
            })
        }))
    }

    /// The next `count` items of a column, viewed where they lie: any words are a `T`, which the
    /// checks then hold to what a tree's methods rely on.
    fn column<T: FromBytes + Immutable + KnownLayout>(
        &mut self,
        count: usize,
    ) -> Result<&'a [T], WireError> {
        let at = self.at;
        let bytes = self.bytes(count.saturating_mul(size_of::<T>()))?;
        if !bytes.as_ptr().addr().is_multiple_of(align_of::<T>()) {
            return Err(WireError::Malformed { what: "bytes not starting at a 4-byte boundary", at });
        }
        Ok(<[T]>::ref_from_bytes(bytes).expect("whole rows at their alignment"))
    }

    /// Skips the zeros up to the next word.
    fn pad(&mut self) -> Result<(), WireError> {
        let pad = self.at.next_multiple_of(4) - self.at;
        if self.bytes(pad)?.iter().any(|&it| it != 0) {
            return malformed("padding that is not zero", self.at);
        }
        Ok(())
    }
}

#[cfg(test)]
pub(super) fn parts_of(tree: &TopSubtree) -> Vec<(WirePart, (SpanAnchor, SyntaxContext))> {
    tree.parts
        .iter()
        .chain([tree.top_open, tree.top_close].map(|it| CompressedSpanPart::from_span(&it)).iter())
        .map(|&it| (WirePart::of(it), (it.anchor, it.ctx)))
        .collect()
}

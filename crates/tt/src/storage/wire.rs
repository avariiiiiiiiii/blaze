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
use span::{EditionedFileId, ErasedFileAstId, Span, SpanAnchor, SyntaxContext, TextRange, TextSize};

use super::{
    CompressedSpanPart, Encoder, IDENT, LITERAL, NARROW_LIMIT, Node, PUNCT, PackedSpan, SUBTREE,
    TopSubtree, WIDE, WideSpan,
};

/// The first word, naming the layout; a change to it changes this.
const MAGIC: u32 = u32::from_le_bytes(*b"tt\x01\x00");
const HEADER_WORDS: usize = 6;

/// A span's anchor and context as the wire carries them: raw ids, which the side reading them
/// turns back into its own (see [`TopSubtree::read_wire`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WirePart {
    pub file_id: u32,
    pub ast_id: u32,
    pub ctx: u32,
}

impl WirePart {
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
    /// Appends the tree's columns to `out`, as [`TopSubtree::read_wire`] reads them.
    pub fn write_wire(&self, out: &mut Vec<u8>) {
        let symbol_bytes: usize = self.symbols.iter().map(|it| it.as_str().len()).sum();
        let words = HEADER_WORDS
            + 2 * self.nodes.len()
            + 2 * self.spans.len()
            + 3 * self.wide.len()
            + 3 * self.parts.len()
            + self.symbols.len()
            + 10;
        out.reserve(4 * words + symbol_bytes.next_multiple_of(4));
        let mut word = |it: u32| out.extend_from_slice(&it.to_le_bytes());
        let len = |it: usize| u32::try_from(it).expect("a token tree column fits in u32");
        word(MAGIC);
        word(len(self.nodes.len()));
        word(len(self.wide.len()));
        word(len(self.parts.len()));
        word(len(self.symbols.len()));
        word(len(symbol_bytes));
        for node in &self.nodes {
            word(node.head);
            word(node.data);
        }
        for span in &self.spans {
            word(span.start);
            word(span.len_part);
        }
        for wide in &self.wide {
            word(wide.start);
            word(wide.len);
            word(wide.part);
        }
        let mut part = |part: WirePart| {
            word(part.file_id);
            word(part.ast_id);
            word(part.ctx);
        };
        for &it in &self.parts {
            part(WirePart::of(it));
        }
        let mut end = 0;
        for symbol in &self.symbols {
            end += symbol.as_str().len();
            word(len(end));
        }
        for span in [self.top_open, self.top_close] {
            word(span.range.start().into());
            word(span.range.len().into());
            word(WirePart::of(CompressedSpanPart::from_span(&span)).file_id);
            word(span.anchor.ast_id.into_raw());
            word(span.ctx.into_u32());
        }
        for symbol in &self.symbols {
            out.extend_from_slice(symbol.as_str().as_bytes());
        }
        out.resize(out.len().next_multiple_of(4), 0);
    }

    /// Reads a tree [`TopSubtree::write_wire`] wrote at the start of `bytes`, and how many bytes
    /// it took. `part` turns each [`WirePart`] into the anchor and context it stands for here,
    /// or refuses it: the ids are the writer's, and only the reader knows which it may use.
    pub fn read_wire(
        bytes: &[u8],
        part: &mut dyn FnMut(WirePart) -> Result<(SpanAnchor, SyntaxContext), String>,
    ) -> Result<(TopSubtree, usize), WireError> {
        let mut words = Words { bytes, at: 0 };
        let magic = words.one()?;
        if magic != MAGIC {
            return Err(WireError::Magic(magic));
        }
        let [nodes_len, wide_len, parts_len, symbols_len, symbol_bytes] =
            words.take::<5>()?.map(|it| it as usize);
        if nodes_len == 0 {
            return malformed("a tree without its top subtree", 0);
        }
        words.ensure(2 * nodes_len)?;
        let mut nodes = Vec::with_capacity(nodes_len);
        for _ in 0..nodes_len {
            let [head, data] = words.take()?;
            nodes.push(Node { head, data });
        }
        let subtrees = check_nodes(&nodes, symbols_len)?;
        let spans_len = nodes_len + subtrees;
        words.ensure(2 * spans_len)?;
        let mut spans = Vec::with_capacity(spans_len);
        for _ in 0..spans_len {
            let [start, len_part] = words.take()?;
            spans.push(PackedSpan { start, len_part });
        }
        words.ensure(3 * wide_len)?;
        let mut wide = Vec::with_capacity(wide_len);
        for at in 0..wide_len {
            let [start, len, part] = words.take()?;
            if part as usize >= parts_len {
                return malformed("a wide span's part past the parts", at);
            }
            if start.checked_add(len).is_none() {
                return malformed("a wide span ending past 4 GiB", at);
            }
            wide.push(WideSpan { start, len, part });
        }
        check_spans(&spans, wide_len, parts_len)?;
        words.ensure(3 * parts_len)?;
        let mut parts = Vec::with_capacity(parts_len);
        for at in 0..parts_len {
            let [file_id, ast_id, ctx] = words.take()?;
            parts.push(resolve(part, WirePart { file_id, ast_id, ctx }, at)?);
        }
        words.ensure(symbols_len)?;
        let mut ends = Vec::with_capacity(symbols_len);
        let mut last = 0;
        for at in 0..symbols_len {
            let end = words.one()? as usize;
            if end < last || end > symbol_bytes {
                return malformed("a symbol ending before the last or past the symbols' bytes", at);
            }
            ends.push(end);
            last = end;
        }
        if last != symbol_bytes {
            return malformed("symbols' bytes past the last symbol", symbols_len);
        }
        let mut top = [None; 2];
        for (at, slot) in top.iter_mut().enumerate() {
            let [start, len, file_id, ast_id, ctx] = words.take()?;
            let Some(end) = start.checked_add(len) else {
                return malformed("a top span ending past 4 GiB", at);
            };
            let part = resolve(part, WirePart { file_id, ast_id, ctx }, parts_len + at)?;
            *slot = Some(part.recombine(TextRange::new(TextSize::new(start), TextSize::new(end))));
        }
        let [Some(top_open), Some(top_close)] = top else {
            unreachable!("both top spans are read above")
        };
        let text = words.bytes(symbol_bytes)?;
        let text = std::str::from_utf8(text).map_err(|e| WireError::Malformed {
            what: "symbols that are not UTF-8",
            at: e.valid_up_to(),
        })?;
        let mut symbols = Vec::with_capacity(symbols_len);
        let mut from = 0;
        for (at, &end) in ends.iter().enumerate() {
            let Some(symbol) = text.get(from..end) else {
                return malformed("a symbol ending inside a char", at);
            };
            symbols.push(Symbol::intern(symbol));
            from = end;
        }
        words.pad()?;
        let tree = TopSubtree {
            nodes: nodes.into_boxed_slice(),
            spans: spans.into_boxed_slice(),
            wide: wide.into_boxed_slice(),
            parts: parts.into_boxed_slice(),
            symbols: symbols.into_boxed_slice(),
            top_open,
            top_close,
        };
        check_canonical(&tree)?;
        Ok((tree, words.at))
    }
}

fn resolve(
    part: &mut dyn FnMut(WirePart) -> Result<(SpanAnchor, SyntaxContext), String>,
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
    if top.tag() != SUBTREE || top.ordinal() != 0 || top.data as usize != nodes.len() - 1 {
        return malformed("a top subtree not holding the whole tree", 0);
    }
    // Where each open subtree ends, the innermost last.
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
        if len == NARROW_LIMIT || part as usize >= parts {
            return malformed("a span's part past the parts", at);
        }
        if span.start.checked_add(len).is_none() {
            return malformed("a span ending past 4 GiB", at);
        }
    }
    Ok(())
}

/// Encodes the tree's spans and symbols again in the order the builder does and checks each
/// lands where it is, and that no table holds more: then the columns are the builder's.
fn check_canonical(tree: &TopSubtree) -> Result<(), WireError> {
    let len = tree.nodes.len();
    let mut encoder = Encoder::new(tree.symbols.len());
    for (at, &node) in tree.nodes.iter().enumerate().skip(1) {
        let span = tree.spans[at];
        if encoder.span(&tree.unpack(span)) != span {
            return Err(WireError::NotCanonical { what: "a span", at });
        }
        match node.tag() {
            SUBTREE => {
                let at = len + node.ordinal() as usize - 1;
                let close = tree.spans[at];
                if encoder.span(&tree.unpack(close)) != close {
                    return Err(WireError::NotCanonical { what: "a closing span", at });
                }
            }
            PUNCT => {}
            _ => {
                if encoder.symbols.index(tree.symbols[node.data as usize]) != node.data {
                    return Err(WireError::NotCanonical { what: "a symbol", at });
                }
            }
        }
    }
    if encoder.parts.list.len() != tree.parts.len() {
        return Err(WireError::NotCanonical { what: "a part no span uses", at: tree.parts.len() });
    }
    if encoder.wide != *tree.wide {
        return Err(WireError::NotCanonical { what: "the wide spans", at: 0 });
    }
    if encoder.symbols.list.len() != tree.symbols.len() {
        return Err(WireError::NotCanonical { what: "a symbol no node uses", at: tree.symbols.len() });
    }
    Ok(())
}

/// A cursor over the bytes' words.
struct Words<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Words<'a> {
    /// Fails unless `words` more words follow, before a column that long is allocated.
    fn ensure(&self, words: usize) -> Result<(), WireError> {
        let needed = words.saturating_mul(4);
        match self.bytes.len() - self.at >= needed {
            true => Ok(()),
            false => Err(WireError::Truncated { at: self.bytes.len(), needed }),
        }
    }

    fn bytes(&mut self, len: usize) -> Result<&'a [u8], WireError> {
        match self.bytes.get(self.at..).and_then(|it| it.get(..len)) {
            Some(it) => {
                self.at += len;
                Ok(it)
            }
            None => Err(WireError::Truncated { at: self.bytes.len(), needed: len }),
        }
    }

    fn take<const N: usize>(&mut self) -> Result<[u32; N], WireError> {
        let bytes = self.bytes(4 * N)?;
        Ok(std::array::from_fn(|i| {
            u32::from_le_bytes(bytes[4 * i..4 * i + 4].try_into().expect("four bytes"))
        }))
    }

    fn one(&mut self) -> Result<u32, WireError> {
        self.take::<1>().map(|[it]| it)
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

/// Keeps the unused imports of the header honest: the anchor's parts are what [`WirePart`]
/// carries.
#[cfg(test)]
#[allow(dead_code)]
fn anchor_of(part: WirePart) -> (EditionedFileId, ErasedFileAstId) {
    (EditionedFileId::from_raw(part.file_id), ErasedFileAstId::from_raw(part.ast_id))
}

#[allow(dead_code)]
fn span_of(part: CompressedSpanPart, range: TextRange) -> Span {
    part.recombine(range)
}

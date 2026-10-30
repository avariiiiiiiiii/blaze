//! The hosts' side of the encoding a stream crosses into and out of a macro's `proc_macro` in
//! (`crates/blaze-expand/src/native/proc_macro/api.rs`, `wire`): a token tree's columns as
//! `tt::TopSubtree::write_wire` lays them out, each span part named by its index in the
//! [`Parts`] of the expansion, and each span the macro holds a word of those parts. The wasm
//! host writes and reads the bytes of every stream; the in-process bridge passes trees as they
//! are, and writes bytes only of text a macro asks to be tokens of.

use rustc_hash::FxHashMap;
use span::{
    EditionedFileId, ErasedFileAstId, Span, SpanAnchor, SyntaxContext, TextRange, TextSize,
};
use tt::{
    DelimiterKind, Leaf, LitKind, Spacing, Subtree, TokenTree, TopSubtree, TopSubtreeBuilder,
    WirePart,
};

/// The word of no span: a wide index never given out.
pub const NONE: u64 = u64::MAX;
/// A packed span's `len_part` that makes its `start` an index of the wide spans.
const WIDE: u32 = u32::MAX;
/// What a packed span's length and part each stay under.
const NARROW: u32 = 0xFFFF;
/// How many parts are found by a scan before a map is built.
const SCAN: usize = 8;
/// The words of a tree's header.
const HEADER: usize = 6;

/// A span's anchor and context: what the spans of an expansion share.
pub type Part = (SpanAnchor, SyntaxContext);

/// The span parts of an expansion, in the order first given out (the def, call and mixed
/// sites, the input's, the attribute's, then what span operations make), and its spans too
/// wide to pack. A span is a word: `start | len_part << 32`, `len_part` being `len | part << 16`
/// when both are under [`NARROW`], else [`WIDE`] with `start` an index of the wide spans, as
/// tt packs a span. Both tables hold each entry once and a span packs whenever it can, so
/// equal spans have equal words.
#[derive(Default)]
pub struct Parts {
    list: Vec<Part>,
    /// Each part's index, once there are more than [`SCAN`].
    map: FxHashMap<Part, u32>,
    /// The part asked for last and its index: tokens near each other share one.
    last: Option<(Part, u32)>,
    wide: Vec<[u32; 3]>,
    wides: FxHashMap<[u32; 3], u32>,
}

impl Parts {
    /// Forgets every part and wide span, keeping the room, for the next expansion.
    pub fn clear(&mut self) {
        self.list.clear();
        self.map.clear();
        self.last = None;
        self.wide.clear();
        self.wides.clear();
    }

    pub fn len(&self) -> usize {
        self.list.len()
    }

    pub fn all(&self) -> &[Part] {
        &self.list
    }

    /// The index of the part `(anchor, ctx)`, the next one when it is new.
    #[inline]
    pub fn part(&mut self, anchor: SpanAnchor, ctx: SyntaxContext) -> u32 {
        let part = (anchor, ctx);
        if let Some((last, index)) = self.last
            && last == part
        {
            return index;
        }
        let index = self.index(part);
        self.last = Some((part, index));
        index
    }

    fn index(&mut self, part: Part) -> u32 {
        if self.list.len() <= SCAN {
            if let Some(at) = self.list.iter().position(|it| *it == part) {
                return at as u32;
            }
            let at = self.list.len() as u32;
            self.list.push(part);
            if self.list.len() > SCAN {
                self.map.reserve(4 * SCAN);
                self.map.extend(self.list.iter().enumerate().map(|(at, &it)| (it, at as u32)));
            }
            return at;
        }
        let next = self.list.len() as u32;
        let list = &mut self.list;
        *self.map.entry(part).or_insert_with(|| {
            list.push(part);
            next
        })
    }

    /// The word of `span`.
    #[inline]
    pub fn pack(&mut self, span: Span) -> u64 {
        let part = self.part(span.anchor, span.ctx);
        self.word(span.range.start().into(), span.range.len().into(), part)
    }

    /// The word of the span `start..start + len` of the part at `part`, which is one.
    #[inline]
    pub fn word(&mut self, start: u32, len: u32, part: u32) -> u64 {
        if len < NARROW && part < NARROW {
            return u64::from(start) | u64::from(len | part << 16) << 32;
        }
        self.wide_word([start, len, part])
    }

    #[cold]
    fn wide_word(&mut self, span: [u32; 3]) -> u64 {
        let next = self.wide.len() as u32;
        let wide = &mut self.wide;
        let at = *self.wides.entry(span).or_insert_with(|| {
            wide.push(span);
            next
        });
        u64::from(at) | u64::from(WIDE) << 32
    }

    /// The word the module asks for of `start`, `len` and `part` (`span_pack`): [`NONE`] for a
    /// part it was never given or a span ending past 4 GiB.
    pub fn pack_fields(&mut self, start: u32, len: u32, part: u32) -> u64 {
        if part as usize >= self.list.len() || start.checked_add(len).is_none() {
            return NONE;
        }
        self.word(start, len, part)
    }

    /// The span of `word`: `None` for a word no span of this expansion has, which a packed
    /// span as long as only a wide one is, or of a part only a wide one has, is not either.
    #[inline]
    pub fn get(&self, word: u64) -> Option<Span> {
        let [start, len, part] = self.fields_of(word)?;
        let (anchor, ctx) = *self.list.get(part as usize)?;
        let end = start.checked_add(len)?;
        Some(Span { range: TextRange::new(TextSize::new(start), TextSize::new(end)), anchor, ctx })
    }

    fn fields_of(&self, word: u64) -> Option<[u32; 3]> {
        let (start, len_part) = (word as u32, (word >> 32) as u32);
        if len_part == WIDE {
            return self.wide.get(start as usize).copied();
        }
        let (len, part) = (len_part & NARROW, len_part >> 16);
        (len != NARROW && part != NARROW).then_some([start, len, part])
    }

    /// `[start, len, part]` of `word` (`span_unpack`), `[0, 0, u32::MAX]` for a word no span of
    /// this expansion has: its part is then refused wherever the module names it.
    pub fn fields(&self, word: u64) -> [u32; 3] {
        match self.get(word) {
            Some(_) => self.fields_of(word).expect("a span's word has its fields"),
            None => [0, 0, u32::MAX],
        }
    }

    /// The part a tree the module wrote names: `{ file_id: index, ast_id: 0, ctx: 0 }` with an
    /// index of this expansion's parts, and nothing else.
    pub fn named(&self, part: WirePart) -> Result<Part, String> {
        match part {
            WirePart { file_id, ast_id: 0, ctx: 0 } if (file_id as usize) < self.list.len() => {
                Ok(self.list[file_id as usize])
            }
            // alloc: the message of a tree refused.
            _ => Err(format!(
                "the expansion has {} span parts, each named by its index alone",
                self.list.len()
            )),
        }
    }

    /// The index of the part of the raw ids `write_wire` wrote of it.
    fn part_written(&mut self, file_id: u32, ast_id: u32, ctx: u32) -> u32 {
        let anchor = SpanAnchor {
            file_id: EditionedFileId::from_raw(file_id),
            ast_id: ErasedFileAstId::from_raw(ast_id),
        };
        // SAFETY: `from_u32` asks for a value `into_u32` gave of this process's contexts:
        // `ctx` is what `write_wire` wrote of a part of a tree of this process the moment before
        // (`relative`). Memory safety doesn't rest on it: a wrong value panics in `from_u32` or
        // in salsa's checked lookup, or names another context's hygiene.
        let ctx = unsafe { SyntaxContext::from_u32(ctx) };
        self.part(anchor, ctx)
    }
}

/// Where a tree's bytes hold its spans and what names its span parts: byte offsets, and counts.
pub struct Sections {
    /// How many nodes there are, the top subtree's included.
    pub nodes: usize,
    pub spans: usize,
    pub wide: usize,
    pub wides: usize,
    pub parts: usize,
    pub count: usize,
    /// The top subtree's two spans, five words each.
    pub top: usize,
}

fn get(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

fn put(bytes: &mut [u8], at: usize, words: [u32; 3]) {
    for (i, word) in words.into_iter().enumerate() {
        bytes[at + 4 * i..at + 4 * i + 4].copy_from_slice(&word.to_le_bytes());
    }
}

/// The sections of `bytes`, exactly one tree `write_wire` wrote. The header doesn't count the
/// subtrees, whose closing spans the span column has after the nodes': the length gives them.
pub fn sections(bytes: &[u8]) -> Option<Sections> {
    let word = |at: usize| {
        Some(u32::from_le_bytes(bytes.get(4 * at..4 * at + 4)?.try_into().ok()?) as usize)
    };
    let (nodes, wides, count, symbols, text) = (word(1)?, word(2)?, word(3)?, word(4)?, word(5)?);
    let fixed = HEADER + 4 * nodes + 3 * wides + 3 * count + symbols + 10;
    let closes = (bytes.len() / 4).checked_sub(text.div_ceil(4))?.checked_sub(fixed)? / 2;
    let spans = 4 * (HEADER + 2 * nodes);
    let wide = spans + 8 * (nodes + closes);
    let parts = wide + 12 * wides;
    let top = parts + 12 * count + 4 * symbols;
    Some(Sections { nodes, spans, wide, wides, parts, count, top })
}

/// Appends `tree`'s columns to `out` with each span part named by its index in `parts`
/// (`{ file_id: index, ast_id: 0, ctx: 0 }`), putting its parts and the spans of it that don't
/// pack in `parts` in the order the tree uses them, then its top subtree's: bytes free of file
/// ids and contexts, which the module reads and an expansion's key hashes.
pub fn relative(tree: &TopSubtree, parts: &mut Parts, out: &mut Vec<u8>) {
    let base = out.len();
    tree.write_wire(out);
    let bytes = &mut out[base..];
    let at = sections(bytes).expect("`write_wire` wrote one whole tree");
    for row in 0..at.count {
        let it = at.parts + 12 * row;
        let index = parts.part_written(get(bytes, it), get(bytes, it + 4), get(bytes, it + 8));
        put(bytes, it, [index, 0, 0]);
    }
    let read: &[u8] = bytes;
    let part = |local: u32| get(read, at.parts + 12 * local as usize);
    if parts.len() < NARROW as usize {
        // No span the tree packs fails to pack among the expansion's parts.
        for row in 0..at.wides {
            let it = at.wide + 12 * row;
            parts.word(get(read, it), get(read, it + 4), part(get(read, it + 8)));
        }
    } else {
        // Each span in the order the tree uses it: a node's, then a subtree's closing one.
        let mut register = |span: usize| {
            let (start, len_part) = (get(read, span), get(read, span + 4));
            let [start, len, local] = match len_part {
                WIDE => {
                    let it = at.wide + 12 * start as usize;
                    [get(read, it), get(read, it + 4), get(read, it + 8)]
                }
                _ => [start, len_part & NARROW, len_part >> 16],
            };
            parts.word(start, len, part(local));
        };
        let mut ordinal = 0;
        for node in 1..at.nodes {
            register(at.spans + 8 * node);
            if get(read, 4 * HEADER + 8 * node) & 0b11 == 0 {
                ordinal += 1;
                register(at.spans + 8 * (at.nodes + ordinal - 1));
            }
        }
    }
    for row in 0..2 {
        let it = at.top + 20 * row;
        let index =
            parts.part_written(get(bytes, it + 8), get(bytes, it + 12), get(bytes, it + 16));
        parts.word(get(bytes, it), get(bytes, it + 4), index);
        put(bytes, it + 8, [index, 0, 0]);
    }
}

/// `bytes` at a 4-byte boundary, as `read_wire` takes them: themselves when they are, else a
/// copy in `room`.
fn aligned<'a>(bytes: &'a [u8], room: &'a mut Vec<u8>) -> &'a [u8] {
    if bytes.as_ptr().addr().is_multiple_of(4) {
        return bytes;
    }
    room.clear();
    // alloc: a copy only of bytes off a 4-byte boundary, as a kept output in the store can be.
    room.resize(bytes.len() + 3, 0);
    let skip = room.as_ptr().addr().wrapping_neg() % 4;
    room[skip..skip + bytes.len()].copy_from_slice(bytes);
    let room: &'a [u8] = room;
    &room[skip..skip + bytes.len()]
}

/// The tree of `bytes`, all of them, each span part it names one of `parts`: an error saying
/// what is wrong with them otherwise.
pub fn read(bytes: &[u8], parts: &Parts) -> Result<TopSubtree, String> {
    // alloc: none: `room` takes memory only for bytes `aligned` copies.
    let mut room = Vec::new();
    let bytes = aligned(bytes, &mut room);
    let (tree, used) = TopSubtree::read_wire(bytes, &mut |part| parts.named(part))
        // alloc: the message of a tree refused.
        .map_err(|e| e.to_string())?;
    match bytes.len() - used {
        0 => Ok(tree),
        // alloc: the message of a tree refused.
        left => Err(format!("{left} bytes after the token tree")),
    }
}

/// `trees` in a top subtree without delimiters at `at`, as they are: what text the module asks
/// to be tokens of is given back as.
pub fn invisible(trees: impl IntoIterator<Item = Tree>, at: Span) -> TopSubtree {
    let mut builder = TopSubtreeBuilder::new(tt::Delimiter::invisible_spanned(at));
    push(&mut builder, trees);
    builder.build()
}

fn push(builder: &mut TopSubtreeBuilder, trees: impl IntoIterator<Item = Tree>) {
    for tree in trees {
        match tree {
            Tree::Leaf(leaf) => builder.push(leaf),
            Tree::Group { kind, open, close, entire: _, trees } => {
                builder.open(kind, open);
                push(builder, trees);
                builder.close(close);
            }
        }
    }
}

/// A stream the host made: what the lexer makes of text (`lex.rs`).
pub enum Tree {
    Group { kind: DelimiterKind, open: Span, close: Span, entire: Span, trees: Vec<Tree> },
    Leaf(Leaf),
}

/// rust-analyzer's tree of a stream: its one group, or the stream in a group without
/// delimiters at `call_site`, as its server writes a macro's output.
pub fn top_subtree(trees: Vec<Tree>, call_site: Span) -> TopSubtree {
    // alloc: the tree's own token column.
    let mut flat = Vec::new();
    match <[Tree; 1]>::try_from(trees) {
        Ok([group @ Tree::Group { .. }]) => flatten(group, &mut flat),
        // alloc: the one leaf's top subtree, of text lexed outside an expansion.
        Ok([leaf]) => flatten(top_of(vec![leaf], call_site), &mut flat),
        Err(trees) => flatten(top_of(trees, call_site), &mut flat),
    }
    TopSubtree::from_serialized(flat)
}

fn top_of(trees: Vec<Tree>, call_site: Span) -> Tree {
    let (open, close, entire) = (call_site, call_site, call_site);
    Tree::Group { kind: DelimiterKind::Invisible, open, close, entire, trees }
}

fn flatten(tree: Tree, flat: &mut Vec<TokenTree>) {
    match tree {
        // `Literal::i32_suffixed(-1)` is one token to the macro and a minus sign and a literal
        // to the compiler, as the native server's reader splits it (`legacy_protocol`, `flat`).
        Tree::Leaf(Leaf::Literal(literal))
            if matches!(literal.kind, LitKind::Integer | LitKind::Float)
                && literal.text().starts_with('-') =>
        {
            let (text, suffix) = literal.text_and_suffix();
            let span = literal.span;
            let minus = tt::Punct { char: '-', spacing: Spacing::Alone, span };
            let magnitude = tt::Literal::new(&text[1..], span, literal.kind, suffix);
            flat.push(TokenTree::Leaf(Leaf::Punct(minus)));
            flat.push(TokenTree::Leaf(Leaf::Literal(magnitude)));
        }
        Tree::Leaf(leaf) => flat.push(TokenTree::Leaf(leaf)),
        Tree::Group { kind, open, close, entire: _, trees } => {
            let at = flat.len();
            flat.push(TokenTree::Subtree(Subtree {
                delimiter: tt::Delimiter { open, close, kind },
                len: 0,
            }));
            for tree in trees {
                flatten(tree, flat);
            }
            let len = (flat.len() - at - 1) as u32;
            if let TokenTree::Subtree(subtree) = &mut flat[at] {
                subtree.len = len;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        super::{at, tokens},
        *,
    };

    #[test]
    fn a_negative_number_comes_out_as_a_minus_and_a_number() {
        let number = |text: &str, kind: tt::LitKind, suffix: &str| {
            Tree::Leaf(tt::Leaf::Literal(tt::Literal::new(text, at(0, 0), kind, suffix)))
        };
        let trees = vec![
            number("-1", tt::LitKind::Integer, "i8"),
            number("-1.5", tt::LitKind::Float, ""),
            number("2", tt::LitKind::Integer, ""),
        ];
        let want = tokens("-1i8 -1.5 2", at(0, 0)).unwrap();
        assert_eq!(top_subtree(trees, at(0, 0)).to_string(), want.to_string());
    }
}

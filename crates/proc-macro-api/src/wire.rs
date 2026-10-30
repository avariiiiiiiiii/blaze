//! Token trees as their columns ([`version::WIRE_TREES`]): a request carries the macro's input
//! and attribute as [`tt::TopSubtree::write_wire`] writes them, and the answer its output the
//! same way, read back checked by [`tt::TopSubtree::read_wire`]. Nothing is converted per token
//! but what a proc macro must not see (the parentheses of a fragment passed on, as
//! [`FlatTree`](crate::legacy_protocol::msg::FlatTree) leaves them out).
//!
//! A span part is an anchor and a context, as raw ids. The client takes back only an anchor and
//! a context it sent ([`Sent`]), in the trees or as the def, call and mixed sites: a server's
//! spans are the client's, moved (`Span::resolved_at` puts one's context on another's anchor).
//! Any other id is an error naming the part, never a span made up of it.
//!
//! [`version::WIRE_TREES`]: crate::version::WIRE_TREES

use paths::Utf8PathBuf;
use rustc_hash::FxHashMap;
use serde_derive::{Deserialize, Serialize};
use span::{
    Edition, EditionedFileId, ErasedFileAstId, Span, SpanAnchor, SyntaxContext, TextRange,
    TextSize,
};
use tt::{TtElement, WirePart};

use crate::{
    bidirectional_protocol::msg::{ParentSpan, SpanJoin, SubResponse},
    legacy_protocol::msg::{
        FlatTree, SpanDataIndexMap, deserialize_span_data_index_map, flat::fragment_contents,
    },
};

/// A span carried whole, as the three sites are: its range and its part's raw ids.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireSpan {
    pub start: u32,
    pub len: u32,
    pub file_id: u32,
    pub ast_id: u32,
    pub ctx: u32,
}

impl WireSpan {
    pub fn of(span: Span) -> WireSpan {
        WireSpan {
            start: span.range.start().into(),
            len: span.range.len().into(),
            file_id: span.anchor.file_id.as_u32(),
            ast_id: span.anchor.ast_id.into_raw(),
            ctx: span.ctx.into_u32(),
        }
    }

    pub fn part(self) -> WirePart {
        WirePart { file_id: self.file_id, ast_id: self.ast_id, ctx: self.ctx }
    }

    /// The span, its part turned into an anchor and a context by `part`, as `read_wire`'s
    /// callback turns a tree's.
    pub fn resolve(
        self,
        part: &mut impl FnMut(WirePart) -> Result<(SpanAnchor, SyntaxContext), String>,
    ) -> Result<Span, String> {
        let (anchor, ctx) = part(self.part())?;
        let Some(end) = self.start.checked_add(self.len) else {
            // alloc: error path, naming the site.
            return Err(format!("a span at {} of {} bytes, ending past 4 GiB", self.start, self.len));
        };
        Ok(Span { range: TextRange::new(TextSize::new(self.start), TextSize::new(end)), anchor, ctx })
    }
}

/// An expansion asked of a server of [`version::WIRE_TREES`](crate::version::WIRE_TREES), in
/// either protocol.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpandMacroWire {
    /// The library the macro is in.
    pub lib: Utf8PathBuf,
    pub env: Vec<(String, String)>,
    pub current_dir: Option<String>,
    pub macro_name: String,
    /// The macro's input, then for an attribute macro its attribute: each tree as `write_wire`
    /// writes it, so the second starts at the word after the first.
    pub trees: Vec<u8>,
    pub has_attributes: bool,
    pub def_site: WireSpan,
    pub call_site: WireSpan,
    pub mixed_site: WireSpan,
}

/// How many anchors or contexts are looked for by a scan before they go in a map.
const SCAN: usize = 8;

/// Values by their raw ids: a list while there are few, then a map.
struct Ids<K, V> {
    list: Vec<(K, V)>,
    map: FxHashMap<K, V>,
}

impl<K: Copy + Eq + std::hash::Hash, V: Copy> Ids<K, V> {
    fn new() -> Self {
        // alloc: none until the first insert; the ids of one request to another process (the edge).
        Ids { list: Vec::new(), map: FxHashMap::default() }
    }

    fn insert(&mut self, key: K, value: V) {
        if !self.map.is_empty() {
            self.map.entry(key).or_insert(value);
            return;
        }
        if self.list.iter().any(|&(it, _)| it == key) {
            return;
        }
        self.list.push((key, value));
        if self.list.len() > SCAN {
            self.map.reserve(4 * SCAN);
            self.map.extend(self.list.iter().copied());
        }
    }

    fn get(&self, key: K) -> Option<V> {
        match self.map.is_empty() {
            true => self.list.iter().find(|&&(it, _)| it == key).map(|&(_, value)| value),
            false => self.map.get(&key).copied(),
        }
    }
}

/// The anchors and contexts a request sent, by their raw ids: what its answer may name.
pub struct Sent {
    anchors: Ids<(u32, u32), SpanAnchor>,
    contexts: Ids<u32, SyntaxContext>,
    last: Option<(SpanAnchor, SyntaxContext)>,
}

impl Default for Sent {
    fn default() -> Sent {
        Sent::new()
    }
}

impl Sent {
    pub fn new() -> Sent {
        Sent { anchors: Ids::new(), contexts: Ids::new(), last: None }
    }

    /// Notes the anchor and context of `span` as sent.
    #[inline]
    pub fn span(&mut self, span: &Span) {
        if self.last == Some((span.anchor, span.ctx)) {
            return;
        }
        self.last = Some((span.anchor, span.ctx));
        let anchor = (span.anchor.file_id.as_u32(), span.anchor.ast_id.into_raw());
        self.anchors.insert(anchor, span.anchor);
        self.contexts.insert(span.ctx.into_u32(), span.ctx);
    }

    /// The anchor and context of `part`, which must both have been sent.
    pub(crate) fn part(&self, part: WirePart) -> Result<(SpanAnchor, SyntaxContext), String> {
        let Some(anchor) = self.anchors.get((part.file_id, part.ast_id)) else {
            // alloc: error path, naming the part.
            return Err(format!(
                "file {:#x} and AST id {} are no anchor the request sent",
                part.file_id, part.ast_id
            ));
        };
        let Some(ctx) = self.contexts.get(part.ctx) else {
            // alloc: error path, naming the part.
            return Err(format!("context {} is none the request sent", part.ctx));
        };
        Ok((anchor, ctx))
    }

    /// Whether `span`'s anchor and context were both sent.
    pub fn has(&self, span: &Span) -> bool {
        let anchor = (span.anchor.file_id.as_u32(), span.anchor.ast_id.into_raw());
        self.anchors.get(anchor) == Some(span.anchor)
            && self.contexts.get(span.ctx.into_u32()) == Some(span.ctx)
    }

    /// Notes the span the client gave in `response` to a server's sub-request, the file of the
    /// two spans it joins being `joined_in` for a join: the server may put the span in its output
    /// (a span's source, parent or the join of two).
    pub(crate) fn sub_response(&mut self, joined_in: Option<u32>, response: &SubResponse) {
        let part = match response {
            SubResponse::SpanSourceResult { file_id, ast_id, ctx, .. }
            | SubResponse::SpanParentResult {
                parent_span: Some(ParentSpan { file_id, ast_id, ctx, .. }),
            } => WirePart { file_id: *file_id, ast_id: *ast_id, ctx: *ctx },
            SubResponse::SpanJoinResult { span: Some(SpanJoin { ast_id, ctx, .. }) } => {
                let Some(file_id) = joined_in else { return };
                WirePart { file_id, ast_id: *ast_id, ctx: *ctx }
            }
            _ => return,
        };
        // A part that is no ids at all names nothing the server could be holding.
        if let Ok((anchor, ctx)) = raw_part(part) {
            self.anchors.insert((part.file_id, part.ast_id), anchor);
            self.contexts.insert(part.ctx, ctx);
        }
    }

    /// Of every span of a request's span table.
    pub fn of_table(table: &SpanDataIndexMap) -> Sent {
        let mut sent = Sent::new();
        table.iter().for_each(|span| sent.span(span));
        sent
    }
}

/// The tree an older server answered with a [`FlatTree`]: its ids index `answered`, the span
/// table it sent back, or the request's own `table` when it sent none. Each span of a table sent
/// back is held to `sent` as a tree of [`version::WIRE_TREES`](crate::version::WIRE_TREES) is.
pub(crate) fn flat_answer(
    tree: FlatTree,
    version: u32,
    table: &SpanDataIndexMap,
    answered: Option<&[u32]>,
    sent: &Sent,
) -> Result<tt::TopSubtree, String> {
    let Some(answered) = answered else {
        return tree.to_subtree_resolved(version, table);
    };
    let answered = deserialize_span_data_index_map(answered)?;
    if let Some((at, span)) = answered.iter().enumerate().find(|(_, span)| !sent.has(span)) {
        // alloc: error path, naming the span.
        return Err(format!(
            "span {at} of the answer's span table, {span:?}, has an anchor or a context the \
             request did not send"
        ));
    }
    tree.to_subtree_resolved(version, &answered)
}

/// Appends `subtree` to `out` as `write_wire` writes a tree, without the parentheses of the
/// fragments it passes on, and notes each of its spans in `sent`.
pub(crate) fn write_tree(subtree: tt::SubtreeView<'_>, sent: &mut Sent, out: &mut Vec<u8>) {
    let top = subtree.top_subtree();
    sent.span(&top.delimiter.open);
    sent.span(&top.delimiter.close);
    let mut builder =
        tt::TopSubtreeBuilder::with_capacity(top.delimiter, subtree.token_trees().len());
    // alloc: one stack per tree sent, as deep as its nesting.
    let mut open: Vec<(tt::TtIter<'_>, Span)> = Vec::new();
    let mut trees = fragment_contents(&top, subtree.iter()).unwrap_or_else(|| subtree.iter());
    loop {
        let Some(element) = trees.next() else {
            let Some((outer, close)) = open.pop() else { break };
            builder.close(close);
            trees = outer;
            continue;
        };
        match element {
            TtElement::Leaf(leaf) => {
                sent.span(leaf.span());
                builder.push(leaf);
            }
            TtElement::Subtree(inner, contents) => {
                sent.span(&inner.delimiter.open);
                sent.span(&inner.delimiter.close);
                builder.open(inner.delimiter.kind, inner.delimiter.open);
                // clone: a slice iterator, no allocation.
                let contents = fragment_contents(&inner, contents.clone()).unwrap_or(contents);
                open.push((std::mem::replace(&mut trees, contents), inner.delimiter.close));
            }
        }
    }
    builder.build().write_wire(out);
}

/// The one tree of a server's answer `bytes`, each part one `sent` sent. `scratch` holds the
/// bytes when they don't start at a word, which `read_wire` asks for.
pub fn read_tree(
    bytes: &[u8],
    sent: &Sent,
    scratch: &mut Vec<u8>,
) -> Result<tt::TopSubtree, String> {
    let bytes = aligned(bytes, scratch);
    let (tree, used) = tt::TopSubtree::read_wire(bytes, &mut |part| sent.part(part))
        // alloc: error path.
        .map_err(|error| format!("the server's answer: {error}"))?;
    if used != bytes.len() {
        // alloc: error path.
        return Err(format!(
            "the server's answer has {} bytes after its token tree",
            bytes.len() - used
        ));
    }
    Ok(tree)
}

/// `bytes`, or a copy of them in `scratch` starting at a 4-byte boundary when they don't.
pub fn aligned<'a>(bytes: &'a [u8], scratch: &'a mut Vec<u8>) -> &'a [u8] {
    if bytes.as_ptr().addr().is_multiple_of(4) {
        return bytes;
    }
    scratch.clear();
    // alloc: only for bytes a deserializer left off a word, which a heap buffer is not.
    scratch.resize(bytes.len() + 3, 0);
    let at = scratch.as_ptr().align_offset(4);
    scratch[at..at + bytes.len()].copy_from_slice(bytes);
    &scratch[at..at + bytes.len()]
}

/// The anchor and context of raw ids, refused when they are no ids [`EditionedFileId::as_u32`]
/// and [`SyntaxContext::into_u32`] give: for a side that takes parts as they come (a server, or a
/// table an older server sends back, before it is held to what was sent).
pub fn raw_part(part: WirePart) -> Result<(SpanAnchor, SyntaxContext), String> {
    let anchor = SpanAnchor {
        file_id: raw_file_id(part.file_id)?,
        ast_id: ErasedFileAstId::from_raw(part.ast_id),
    };
    Ok((anchor, raw_context(part.ctx)?))
}

pub(crate) fn raw_file_id(raw: u32) -> Result<EditionedFileId, String> {
    if raw & EditionedFileId::RESERVED_MASK != 0 {
        // alloc: error path, naming the id.
        return Err(format!("file id {raw:#x} has the reserved bit set"));
    }
    let edition = (raw & EditionedFileId::EDITION_MASK) >> EditionedFileId::FILE_ID_BITS;
    if edition > Edition::LATEST as u32 {
        // alloc: error path, naming the id.
        return Err(format!("file id {raw:#x} has edition {edition}, past the latest"));
    }
    Ok(EditionedFileId::from_raw(raw))
}

pub(crate) fn raw_context(raw: u32) -> Result<SyntaxContext, String> {
    if raw == u32::MAX {
        // alloc: error path, naming the id.
        return Err(format!("context {raw:#x} is none `into_u32` gives"));
    }
    // SAFETY: `from_u32` asks for a value `into_u32` gave of this database's contexts. A server
    // has no database and only carries contexts back; a client holds what it takes back to the
    // contexts it sent (`Sent`) before using any. Memory safety doesn't rest on it either way:
    // `from_u32` refuses only `u32::MAX`, checked above, and salsa's table checks every lookup.
    Ok(unsafe { SyntaxContext::from_u32(raw) })
}

#[cfg(test)]
mod tests {
    use intern::Symbol;
    use span::{FileId, ROOT_ERASED_FILE_AST_ID};
    use tt::{Delimiter, DelimiterKind, Leaf, Punct, Spacing, TopSubtree, TopSubtreeBuilder};

    use super::*;
    use crate::bidirectional_protocol::msg::{BidirectionalMessage, Request};
    use crate::legacy_protocol::msg::Message;

    fn span(file: u32, start: u32, ctx: u32) -> Span {
        Span {
            range: TextRange::at(TextSize::new(start), TextSize::new(1)),
            anchor: SpanAnchor {
                file_id: EditionedFileId::new(FileId::from_raw(file), Edition::CURRENT),
                ast_id: ROOT_ERASED_FILE_AST_ID,
            },
            // SAFETY: no database; the context is compared, never looked up.
            ctx: unsafe { SyntaxContext::from_u32(ctx) },
        }
    }

    fn punct(char: char, span: Span) -> Leaf {
        Leaf::Punct(Punct { char, spacing: Spacing::Alone, span })
    }

    /// `#[a]` with each token in a file of its own, and as many contexts.
    fn tree(files: u32) -> TopSubtree {
        let top = span(0, 0, 0);
        let mut builder = TopSubtreeBuilder::new(Delimiter::invisible_spanned(top));
        for file in 1..=files {
            builder.push(punct('#', span(file, file, file)));
            builder.open(DelimiterKind::Bracket, span(file, 1, file));
            builder.push(Leaf::Ident(tt::Ident {
                sym: Symbol::intern("a"),
                span: span(file, 2, file),
                is_raw: tt::IdentIsRaw::No,
            }));
            builder.close(span(file, 3, file));
        }
        builder.build()
    }

    #[test]
    fn a_tree_sent_reads_back_whole_against_its_own_parts() {
        for files in [0, 3, 40] {
            let tree = tree(files);
            let mut sent = Sent::new();
            let mut bytes = Vec::new();
            write_tree(tree.view(), &mut sent, &mut bytes);
            assert_eq!(read_tree(&bytes, &sent, &mut Vec::new()), Ok(tree.clone()), "{files}");
        }
    }

    #[test]
    fn a_part_the_request_did_not_send_is_refused_by_name() {
        let mut sent = Sent::new();
        write_tree(tree(2).view(), &mut sent, &mut Vec::new());
        let mut bytes = Vec::new();
        tree(3).write_wire(&mut bytes);
        let why = read_tree(&bytes, &sent, &mut Vec::new()).unwrap_err();
        assert!(why.contains("no anchor the request sent"), "{why}");

        let mut other_context = TopSubtreeBuilder::new(Delimiter::invisible_spanned(span(1, 0, 9)));
        other_context.push(punct(';', span(1, 0, 1)));
        let mut bytes = Vec::new();
        other_context.build().write_wire(&mut bytes);
        let why = read_tree(&bytes, &sent, &mut Vec::new()).unwrap_err();
        assert!(why.contains("context 9"), "{why}");
    }

    #[test]
    fn a_span_given_in_a_sub_response_may_be_answered() {
        let mut sent = Sent::new();
        write_tree(tree(1).view(), &mut sent, &mut Vec::new());
        let given = span(5, 0, 7);
        assert!(!sent.has(&given));
        let source = SubResponse::SpanSourceResult {
            file_id: given.anchor.file_id.as_u32(),
            ast_id: given.anchor.ast_id.into_raw(),
            start: 0,
            end: 1,
            ctx: 7,
        };
        sent.sub_response(None, &source);
        assert!(sent.has(&given));
        let joined = SubResponse::SpanJoinResult {
            span: Some(SpanJoin { ast_id: 4, start: 0, end: 1, ctx: 7 }),
        };
        sent.sub_response(Some(given.anchor.file_id.as_u32()), &joined);
        let mut at_4 = given;
        at_4.anchor.ast_id = ErasedFileAstId::from_raw(4);
        assert!(sent.has(&at_4));
    }

    #[test]
    fn bytes_after_the_tree_and_a_cut_tree_are_refused() {
        let tree = tree(2);
        let mut sent = Sent::new();
        let mut bytes = Vec::new();
        write_tree(tree.view(), &mut sent, &mut bytes);
        let mut longer = bytes.clone();
        longer.extend_from_slice(&[0; 4]);
        assert!(read_tree(&longer, &sent, &mut Vec::new()).unwrap_err().contains("after its"));
        for cut in [0, 4, bytes.len() / 2, bytes.len() - 4] {
            assert!(read_tree(&bytes[..cut], &sent, &mut Vec::new()).is_err(), "{cut}");
        }
    }

    #[test]
    fn bytes_off_a_word_are_read_from_a_copy() {
        let tree = tree(2);
        let mut sent = Sent::new();
        let mut bytes = vec![0];
        write_tree(tree.view(), &mut sent, &mut bytes);
        let off = &bytes[1..];
        assert_eq!(read_tree(off, &sent, &mut Vec::new()), Ok(tree));
    }

    #[test]
    fn the_parentheses_of_a_fragment_passed_on_are_left_out() {
        let at = span(1, 0, 1);
        let mut builder = TopSubtreeBuilder::new(Delimiter::invisible_spanned(at));
        builder.open(DelimiterKind::Invisible, at);
        builder.open(DelimiterKind::Parenthesis, at);
        builder.push(punct('!', span(1, 1, 1)));
        builder.close(at);
        builder.close(at);
        let mut sent = Sent::new();
        let mut bytes = Vec::new();
        write_tree(builder.build().view(), &mut sent, &mut bytes);
        let read = read_tree(&bytes, &sent, &mut Vec::new()).unwrap();
        let mut expected = TopSubtreeBuilder::new(Delimiter::invisible_spanned(at));
        expected.open(DelimiterKind::Invisible, at);
        expected.push(punct('!', span(1, 1, 1)));
        expected.close(at);
        assert_eq!(read, expected.build());
    }

    #[test]
    fn a_wire_request_reads_back_equal_in_both_formats() {
        let mut trees = Vec::new();
        tree(2).write_wire(&mut trees);
        let request = ExpandMacroWire {
            lib: Utf8PathBuf::from("/lib.so"),
            env: vec![("A".into(), "1".into())],
            current_dir: Some("/".into()),
            macro_name: "m".into(),
            trees,
            has_attributes: false,
            def_site: WireSpan::of(span(1, 0, 1)),
            call_site: WireSpan::of(span(1, 1, 1)),
            mixed_site: WireSpan::of(span(1, 2, 1)),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(serde_json::from_str::<ExpandMacroWire>(&json).unwrap(), request);

        let message = |request: ExpandMacroWire| {
            BidirectionalMessage::Request(Request::ExpandMacroWire(Box::new(request)))
        };
        let mut frame = Vec::new();
        message(serde_json::from_str(&json).unwrap()).write(&mut frame).unwrap();
        let read = BidirectionalMessage::read(&mut &frame[..], &mut Vec::new()).unwrap();
        assert_eq!(read, Some(message(request)));
    }

    #[test]
    fn a_site_ending_past_4_gib_is_refused() {
        let site = WireSpan { start: u32::MAX, len: 2, ..WireSpan::of(span(1, 0, 1)) };
        assert!(site.resolve(&mut raw_part).unwrap_err().contains("past 4 GiB"));
        assert_eq!(WireSpan::of(span(1, 3, 1)).resolve(&mut raw_part), Ok(span(1, 3, 1)));
    }

    #[test]
    fn raw_ids_that_no_constructor_gives_are_refused() {
        let ok = WirePart { file_id: 1, ast_id: 0, ctx: 0 };
        assert!(raw_part(ok).is_ok());
        assert!(raw_part(WirePart { file_id: 1 << 31, ..ok }).is_err());
        assert!(raw_part(WirePart { file_id: 0x7F80_0000, ..ok }).is_err());
        assert!(raw_part(WirePart { ctx: u32::MAX, ..ok }).is_err());
    }
}

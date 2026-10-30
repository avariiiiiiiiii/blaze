//! Token trees as their columns ([`version::WIRE_TREES`]): a request carries the macro's input
//! and attribute as [`tt::Tree::write_wire`] writes them, and the answer its output the
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

use std::fmt;

use blaze_mem::{Frame, Tag, frame};
use paths::Utf8PathBuf;
use rustc_hash::FxHashMap;
use serde::{
    Deserializer, Serializer,
    de::{MapAccess, SeqAccess, Visitor},
    ser::{SerializeSeq, SerializeStruct},
};
use serde_derive::{Deserialize, Serialize};
use span::{
    Edition, EditionedFileId, ErasedFileAstId, Span, SpanAnchor, SyntaxContext, TextRange,
    TextSize,
};
use tt::{TtElement, WirePart};

use crate::{
    ProcMacroKind,
    bidirectional_protocol::msg::{
        ExpandMacroResponse, ParentSpan, Request, SpanJoin, SubRequest, SubResponse,
    },
    legacy_protocol::msg::{
        FlatTree, PanicMessage, ServerConfig, SpanDataIndexMap, deserialize_span_data_index_map,
        flat::fragment_contents,
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

/// An [`ExpandMacroWire`] borrowed from its caller, written as the same bytes: a conversation
/// encodes its request without copying a string, a variable or a tree. Its environment is
/// `passed`, then `call`.
pub struct WireAsk<'a> {
    pub lib: &'a str,
    pub passed: &'a [(String, String)],
    pub call: &'a [(String, String)],
    pub current_dir: &'a str,
    pub macro_name: &'a str,
    pub trees: &'a [u8],
    pub has_attributes: bool,
    pub sites: [WireSpan; 3],
}

/// The index of `Request` in `BidirectionalMessage`, and of `ExpandMacroWire` in `Request`, as
/// their derives number them.
const REQUEST: u32 = 0;
const EXPAND_MACRO_WIRE: u32 = 4;

impl WireAsk<'_> {
    /// The request as the message `BidirectionalMessage::Request(Request::ExpandMacroWire(..))`
    /// is written.
    pub fn message(&self) -> impl serde::Serialize + '_ {
        Variant { of: "BidirectionalMessage", index: REQUEST, name: "Request", value: Variant {
            of: "Request",
            index: EXPAND_MACRO_WIRE,
            name: "ExpandMacroWire",
            value: self,
        } }
    }
}

/// One variant of an enum, holding `value`, as a derive writes it.
struct Variant<T> {
    of: &'static str,
    index: u32,
    name: &'static str,
    value: T,
}

impl<T: serde::Serialize> serde::Serialize for Variant<T> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_newtype_variant(self.of, self.index, self.name, &self.value)
    }
}

impl serde::Serialize for WireAsk<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let [def_site, call_site, mixed_site] = &self.sites;
        let mut it = s.serialize_struct("ExpandMacroWire", 9)?;
        it.serialize_field("lib", self.lib)?;
        it.serialize_field("env", &Env(self.passed, self.call))?;
        it.serialize_field("current_dir", &Some(self.current_dir))?;
        it.serialize_field("macro_name", self.macro_name)?;
        it.serialize_field("trees", &Bytes(self.trees))?;
        it.serialize_field("has_attributes", &self.has_attributes)?;
        it.serialize_field("def_site", def_site)?;
        it.serialize_field("call_site", call_site)?;
        it.serialize_field("mixed_site", mixed_site)?;
        it.end()
    }
}

/// Two lists of variables written as one `Vec<(String, String)>`.
struct Env<'a>(&'a [(String, String)], &'a [(String, String)]);

impl serde::Serialize for Env<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(self.0.len() + self.1.len()))?;
        for (name, value) in self.0.iter().chain(self.1) {
            seq.serialize_element(&(name.as_str(), value.as_str()))?;
        }
        seq.end()
    }
}

/// Bytes written as a `Vec<u8>` is, in one piece: postcard's length and bytes, JSON's array.
struct Bytes<'a>(&'a [u8]);

impl serde::Serialize for Bytes<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(self.0)
    }
}

/// A server's message to a conversation of [`WireAsk`], read in place: an answer's tree is
/// borrowed from the frame it came in, and what the client does not use of it is read past.
#[derive(Debug, Deserialize)]
#[serde(rename = "BidirectionalMessage")]
pub(crate) enum Heard<'a> {
    Request(Request),
    #[serde(borrow)]
    Response(HeardResponse<'a>),
    SubRequest(SubRequest),
    SubResponse(SubResponse),
}

/// [`Response`](crate::bidirectional_protocol::msg::Response), the answer to a wire request
/// borrowed.
#[derive(Debug, Deserialize)]
#[serde(rename = "Response")]
pub(crate) enum HeardResponse<'a> {
    ListMacros(Result<Vec<(String, ProcMacroKind)>, String>),
    ApiVersionCheck(u32),
    SetConfig(ServerConfig),
    ExpandMacro(Result<ExpandMacroResponse, PanicMessage>),
    #[serde(borrow)]
    ExpandMacroWire(Result<WireAnswer<'a>, PanicMessage>),
}

/// [`ExpandMacroWireResponse`](crate::bidirectional_protocol::msg::ExpandMacroWireResponse),
/// its tree borrowed. What the macro tracked is not used: a warm server's sandbox traces what
/// an expansion reads.
#[derive(Debug, Deserialize)]
pub(crate) struct WireAnswer<'a> {
    #[serde(borrow)]
    pub(crate) tree: &'a [u8],
    #[serde(rename = "tracked_env_vars")]
    _tracked_env_vars: UnusedVars,
    #[serde(rename = "tracked_paths")]
    _tracked_paths: UnusedNames,
}

/// A map of names to optional values, read past without keeping any.
#[derive(Debug)]
struct UnusedVars;

/// A list of names, read past without keeping any.
#[derive(Debug)]
struct UnusedNames;

impl<'de> serde::Deserialize<'de> for UnusedVars {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<UnusedVars, D::Error> {
        d.deserialize_map(Past).map(|()| UnusedVars)
    }
}

impl<'de> serde::Deserialize<'de> for UnusedNames {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<UnusedNames, D::Error> {
        d.deserialize_seq(Past).map(|()| UnusedNames)
    }
}

/// Reads names past, borrowed where they lie.
struct Past;

impl<'de> Visitor<'de> for Past {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a map or a list of names")
    }

    fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<(), M::Error> {
        while map.next_entry::<&'de str, Option<&'de str>>()?.is_some() {}
        Ok(())
    }

    fn visit_seq<L: SeqAccess<'de>>(self, mut seq: L) -> Result<(), L::Error> {
        while seq.next_element::<&'de str>()?.is_some() {}
        Ok(())
    }
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

    /// Empties it, keeping what it has room for.
    fn clear(&mut self) {
        self.list.clear();
        self.map.clear();
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

    /// Forgets what was sent, keeping the room it took: a connection notes each request's
    /// parts in the one `Sent`.
    pub fn clear(&mut self) {
        self.anchors.clear();
        self.contexts.clear();
        self.last = None;
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
    // The stack and the rebuilt tree are the request's temporaries, in its task's frame; the
    // builder's own buffers are kept per thread.
    frame::task(Tag::Expand, |frame| {
        let mut open: Vec<(tt::TtIter<'_>, Span), &Frame> = Vec::new_in(frame);
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
        builder.build_in(frame).write_wire(out);
    });
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
            assert_eq!(read_tree(&bytes, &sent, &mut Vec::new()), Ok(tree.copy()), "{files}");
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

    fn vars(names: &[&str]) -> Vec<(String, String)> {
        names.iter().map(|name| (name.to_string(), format!("{name}="))).collect()
    }

    #[test]
    fn a_borrowed_wire_request_is_written_as_the_owned_one_in_both_formats() {
        let mut trees = Vec::new();
        tree(2).write_wire(&mut trees);
        let (passed, call) = (vars(&["PASSED"]), vars(&["CALL", "OTHER"]));
        let sites = [span(1, 0, 1), span(1, 1, 1), span(1, 2, 1)].map(WireSpan::of);
        let ask = WireAsk {
            lib: "/lib.so",
            passed: &passed,
            call: &call,
            current_dir: "/",
            macro_name: "m",
            trees: &trees,
            has_attributes: true,
            sites,
        };
        let [def_site, call_site, mixed_site] = sites;
        let owned = BidirectionalMessage::Request(Request::ExpandMacroWire(Box::new(
            ExpandMacroWire {
                lib: Utf8PathBuf::from("/lib.so"),
                env: [passed.clone(), call.clone()].concat(),
                current_dir: Some("/".into()),
                macro_name: "m".into(),
                trees: trees.clone(),
                has_attributes: true,
                def_site,
                call_site,
                mixed_site,
            },
        )));
        assert_eq!(
            crate::transport::postcard::encode(&ask.message()).unwrap(),
            crate::transport::postcard::encode(&owned).unwrap()
        );
        assert_eq!(
            serde_json::to_string(&ask.message()).unwrap(),
            serde_json::to_string(&owned).unwrap()
        );
    }

    #[test]
    fn an_answer_is_heard_where_it_lies_in_its_frame() {
        use crate::bidirectional_protocol::msg::{ExpandMacroWireResponse, Response};
        let mut tree_bytes = Vec::new();
        tree(2).write_wire(&mut tree_bytes);
        let answer = ExpandMacroWireResponse {
            tree: tree_bytes.clone(),
            tracked_env_vars: [("A".into(), Some("1".into())), ("B".into(), None)]
                .into_iter()
                .collect(),
            tracked_paths: ["/p".into()].into_iter().collect(),
        };
        let message = BidirectionalMessage::Response(Response::ExpandMacroWire(Ok(answer)));
        let mut frame = crate::transport::postcard::encode(&message).unwrap();
        let len = crate::transport::postcard::unframe(&mut frame).unwrap();
        let heard = crate::transport::postcard::parse::<Heard<'_>>(&frame[..len]).unwrap();
        let Heard::Response(HeardResponse::ExpandMacroWire(Ok(heard))) = heard else {
            panic!("an answer heard as another message");
        };
        assert_eq!(heard.tree, &tree_bytes[..]);
        let range = frame.as_ptr_range();
        assert!(range.contains(&heard.tree.as_ptr()), "the tree is read in place");
    }

    #[test]
    fn a_wire_request_is_written_into_its_buffers_without_allocating() {
        let (subtree, attr) = (tree(2), tree(1));
        let (passed, call) = (vars(&["PASSED"]), vars(&["CALL"]));
        let (mut sent, mut trees, mut frame) = (Sent::new(), Vec::new(), Vec::new());
        let mut request = || {
            sent.clear();
            trees.clear();
            write_tree(subtree.view(), &mut sent, &mut trees);
            write_tree(attr.view(), &mut sent, &mut trees);
            let ask = WireAsk {
                lib: "/lib.so",
                passed: &passed,
                call: &call,
                current_dir: "/",
                macro_name: "m",
                trees: &trees,
                has_attributes: true,
                sites: [span(1, 0, 1); 3].map(WireSpan::of),
            };
            crate::transport::postcard::encode_into(&ask.message(), &mut frame).unwrap();
        };
        // The first fills the buffers and the builder's scratch.
        request();
        blaze_alloc_count::assert_at_most(0, "a wire request into its buffers", request);
    }

    #[test]
    fn a_wire_answer_read_allocates_only_its_tree() {
        let tree = tree(2);
        let mut sent = Sent::new();
        let mut bytes = Vec::new();
        write_tree(tree.view(), &mut sent, &mut bytes);
        let mut aligned = Vec::new();
        assert_eq!(read_tree(&bytes, &sent, &mut aligned).as_ref(), Ok(&tree));
        // The tree's block: the first read kept `read_wire`'s check buffers for this thread.
        let read = blaze_alloc_count::assert_at_most(1, "a wire answer's tree", || {
            read_tree(&bytes, &sent, &mut aligned)
        });
        assert_eq!(read, Ok(tree));
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

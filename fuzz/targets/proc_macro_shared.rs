//! What the `proc_macro_*` targets share. It is a module of each (`#[path =
//! "proc_macro_shared.rs"] mod shared;`), not a target of its own.
//!
//! - The client reads what a proc-macro server (user code, in its sandbox) sends back through
//!   proc-macro-api's `Message::read`: postcard messages in COBS frames, each ended by a 0, from
//!   the warm and the native servers (`BidirectionalMessage`), and lines of JSON from a legacy
//!   one (`legacy_protocol::msg::Response`). The crate keeps its framing (`transport`) to
//!   itself, so `cobs` and `uncobs` are this module's own, checked against the crate's by the
//!   seeds: every frame the crate writes is `cobs` of what `uncobs` gives back of it.
//! - `Pieces` gives a stream to a reader a few bytes at a time, as a socket gives what has come.
//! - Two messages are told the same by `==`: the crate's messages derive `PartialEq, Eq`, whose
//!   hash tables compare in any order.

#![allow(dead_code)]

use std::io::{self, BufRead, Read};

use paths::Utf8PathBuf;
use proc_macro_api::{
    ProcMacroKind,
    bidirectional_protocol::msg::{
        ApiVersionCheck, BidirectionalMessage, ExpandMacroResponse, ExpandMacroWireResponse,
        ParentSpan, Request, Response, SpanJoin, SubRequest, SubResponse,
    },
    legacy_protocol::msg::{
        FlatTree, Message, PanicMessage, ServerConfig, SpanDataIndexMap, SpanMode,
        serialize_span_data_index_map,
    },
    version,
    wire::{ExpandMacroWire, WireSpan},
};
use span::{
    Edition, EditionedFileId, FileId, ROOT_ERASED_FILE_AST_ID, Span, SpanAnchor, SyntaxContext,
    TextRange, TextSize,
};
use tt::{Delimiter, DelimiterKind, Ident, LitKind, Literal, Punct, Spacing, TopSubtreeBuilder};

/// The most one hash table of an expansion's answer (`tracked_env_vars`, `tracked_paths`)
/// reserves for a length it is promised before its entries come: serde's cautious megabyte of
/// entries, which hashbrown rounds up to a power of two of buckets at 7/8 load, plus a control
/// byte each: 2,228,240 bytes for `tracked_paths`' 16-byte entries.
pub(crate) const TABLE: usize = 9 << 18;

/// The most a read of a `len`-byte message may ask for at once: what `blaze_fuzz::allowed`
/// lets a read ask for, with a hash table's reservation in place of its megabyte.
pub(crate) fn bound(len: usize) -> usize {
    blaze_fuzz::allowed(len) - (1 << 20) + TABLE
}

/// `raw` COBS-encoded and ended with a 0: one frame, as `postcard::to_allocvec_cobs` writes it.
pub(crate) fn cobs(raw: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(raw.len() + raw.len() / 254 + 2);
    let mut code_at = 0;
    frame.push(0);
    for &byte in raw {
        if byte != 0 {
            frame.push(byte);
        }
        if byte == 0 || frame.len() - code_at == 0xff {
            frame[code_at] = (frame.len() - code_at) as u8;
            code_at = frame.len();
            frame.push(0);
        }
    }
    frame[code_at] = (frame.len() - code_at) as u8;
    frame.push(0);
    frame
}

/// The bytes of `frame`, one frame the crate wrote (ended with its 0), as `cobs` was given them.
pub(crate) fn uncobs(frame: &[u8]) -> Vec<u8> {
    let (&end, body) = frame.split_last().expect("a frame ends with a 0");
    assert_eq!(end, 0, "a frame ends with a 0");
    let mut raw = Vec::with_capacity(body.len());
    let mut at = 0;
    while at < body.len() {
        let code = usize::from(body[at]);
        assert!(code > 0 && at + code <= body.len(), "a frame has no 0 before its end: {frame:?}");
        raw.extend_from_slice(&body[at + 1..at + code]);
        at += code;
        if code < 0xff && at < body.len() {
            raw.push(0);
        }
    }
    raw
}

/// The next message of `stream`, as the client reads one.
pub(crate) fn read_one(stream: &[u8]) -> io::Result<Option<BidirectionalMessage>> {
    let mut reader = stream;
    // alloc: a fresh buffer is what reading a frame on its own means; it grows only on a read.
    <BidirectionalMessage as Message>::read(&mut reader, &mut Vec::new())
}

/// The frame the crate writes for `message`.
pub(crate) fn written(message: BidirectionalMessage) -> Vec<u8> {
    // alloc: the frame `Message::write` makes, which a check reads back or compares.
    let mut frame = Vec::new();
    message.write(&mut frame).expect("a message is written to memory");
    frame
}

/// The postcard bytes of `message` in the frame the crate writes for it: `cobs` of them reads
/// back to what that frame reads as.
pub(crate) fn raw(message: BidirectionalMessage) -> Vec<u8> {
    let frame = written(message);
    let raw = uncobs(&frame);
    match (read_one(&cobs(&raw)), read_one(&frame)) {
        (Ok(Some(again)), Ok(Some(said))) => {
            assert_eq!(again, said, "`cobs` frames as the crate does")
        }
        other => panic!("`cobs` of the bytes of {frame:?} and the frame read as {other:?}"),
    }
    raw
}

/// How the pieces targets cut a stream: a byte at a time, two, uneven, and large then small.
pub(crate) const CUTS: [&[usize]; 4] = [&[1], &[2], &[3, 1, 4, 1, 5, 9, 2, 6], &[255, 1, 4096]];

/// The one way a pieces target cuts `data` this run: one of `CUTS`, or pieces of the sizes its
/// first bytes say, picked by its length and last byte. One cut a run, not all of them: a byte
/// at a time costs about 15 reads of the whole stream, and the engine's mutations reach every
/// cut of every input across runs.
pub(crate) fn sizes<'a>(data: &[u8], own: &'a mut [usize; 8]) -> &'a [usize] {
    let pick = data.len() + data.last().map_or(0, |it| usize::from(*it));
    if let Some(sizes) = CUTS.get(pick % (CUTS.len() + 1)) {
        return sizes;
    }
    for (size, byte) in own.iter_mut().zip(data) {
        *size = usize::from(byte % 16) + 1;
    }
    &own[..data.len().min(own.len())]
}

/// `stream` given out in pieces of `sizes` in turn.
pub(crate) struct Pieces<'a> {
    stream: &'a [u8],
    sizes: &'a [usize],
    piece: usize,
    at: usize,
    end: usize,
}

impl<'a> Pieces<'a> {
    pub(crate) fn new(stream: &'a [u8], sizes: &'a [usize]) -> Pieces<'a> {
        Pieces { stream, sizes, piece: 0, at: 0, end: 0 }
    }
}

impl Read for Pieces<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let available = self.fill_buf()?;
        let n = available.len().min(out.len());
        out[..n].copy_from_slice(&available[..n]);
        self.consume(n);
        Ok(n)
    }
}

impl BufRead for Pieces<'_> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.at == self.end {
            let size = self.sizes[self.piece % self.sizes.len()];
            self.end = (self.at + size).min(self.stream.len());
            self.piece += 1;
        }
        Ok(&self.stream[self.at..self.end])
    }

    fn consume(&mut self, n: usize) {
        self.at = (self.at + n).min(self.end);
    }
}

/// A span of the fixtures' one file: `len` bytes at `start`.
pub(crate) fn at(start: u32, len: u32) -> Span {
    Span {
        range: TextRange::at(TextSize::new(start), TextSize::new(len)),
        anchor: SpanAnchor {
            file_id: EditionedFileId::new(FileId::from_raw(0xe4e4e), Edition::CURRENT),
            ast_id: ROOT_ERASED_FILE_AST_ID,
        },
        ctx: SyntaxContext::root(Edition::CURRENT),
    }
}

/// Token trees a macro is given and gives back: every kind of leaf and delimiter, a raw
/// identifier, a negative literal (sent as one token, read as a minus and a literal), nothing,
/// and the parentheses of a passed-on expression, which the writer leaves out.
pub(crate) fn trees() -> Vec<tt::TopSubtree> {
    let mut many = TopSubtreeBuilder::new(Delimiter {
        open: at(0, 0),
        close: at(40, 0),
        kind: DelimiterKind::Invisible,
    });
    many.push(Ident::new("struct", at(0, 6)).into());
    many.push(Ident::new("r#Foo", at(7, 5)).into());
    many.push(Literal::new_no_suffix("Foo", at(13, 5), LitKind::Str).into());
    many.push(Punct { char: '@', spacing: Spacing::Joint, span: at(18, 1) }.into());
    many.open(DelimiterKind::Brace, at(19, 1));
    many.open(DelimiterKind::Bracket, at(20, 1));
    many.push(Literal::new("0", at(21, 4), LitKind::Integer, "u32").into());
    many.close(at(25, 1));
    many.open(DelimiterKind::Parenthesis, at(26, 1));
    many.push(Literal::new_no_suffix("raw", at(27, 8), LitKind::StrRaw(1)).into());
    many.push(Punct { char: ',', spacing: Spacing::Alone, span: at(35, 1) }.into());
    many.close(at(36, 1));
    many.close(at(37, 1));
    many.push(Literal::new("-1.5", at(38, 6), LitKind::Float, "f32").into());

    let empty = TopSubtreeBuilder::new(Delimiter::invisible_spanned(at(0, 0)));
    let brace = TopSubtreeBuilder::new(Delimiter {
        open: at(0, 1),
        close: at(1, 1),
        kind: DelimiterKind::Brace,
    });

    let mut passed_on = TopSubtreeBuilder::new(Delimiter::invisible_spanned(at(4, 3)));
    passed_on.open(DelimiterKind::Parenthesis, at(4, 3));
    passed_on.push(Ident::new("x", at(5, 1)).into());
    passed_on.close(at(4, 3));

    vec![many.build(), empty.build(), brace.build(), passed_on.build()]
}

fn answer(response: Response) -> BidirectionalMessage {
    BidirectionalMessage::Response(response)
}

fn asked(request: SubRequest) -> BidirectionalMessage {
    BidirectionalMessage::SubRequest(request)
}

fn told(response: SubResponse) -> BidirectionalMessage {
    BidirectionalMessage::SubResponse(response)
}

/// Messages of every kind: what a server sends (answers and sub-requests) and, read the same
/// way, what the client sends.
pub(crate) fn messages() -> Vec<BidirectionalMessage> {
    let mut messages = vec![
        answer(Response::ApiVersionCheck(version::CURRENT_API_VERSION)),
        answer(Response::SetConfig(ServerConfig { span_mode: SpanMode::RustAnalyzer })),
        answer(Response::ListMacros(Ok(vec![
            ("Derive".to_owned(), ProcMacroKind::CustomDerive),
            ("attr".to_owned(), ProcMacroKind::Attr),
            ("bang".to_owned(), ProcMacroKind::Bang),
        ]))),
        answer(Response::ListMacros(Err("the library can't be loaded".to_owned()))),
        answer(Response::ExpandMacro(Err(PanicMessage("the macro panicked: ünï".to_owned())))),
        asked(SubRequest::FilePath { file_id: 3 }),
        asked(SubRequest::SourceText { file_id: 3, ast_id: 1, start: 4, end: 9 }),
        asked(SubRequest::LocalFilePath { file_id: 3 }),
        asked(SubRequest::LineColumn { file_id: 3, ast_id: 1, offset: 4 }),
        asked(SubRequest::ByteRange { file_id: 3, ast_id: 1, start: 4, end: 9 }),
        asked(SubRequest::SpanSource { file_id: 3, ast_id: 1, start: 4, end: 9, ctx: 0 }),
        asked(SubRequest::SpanParent { file_id: 3, ast_id: 1, start: 4, end: 9, ctx: u32::MAX }),
        asked(SubRequest::SpanJoin {
            file_id: 3,
            ast_id_first: 1,
            start_first: 0,
            end_first: 2,
            ctx_first: 0,
            ast_id_second: 1,
            start_second: 3,
            end_second: 5,
            ctx_second: 0,
        }),
        told(SubResponse::FilePathResult { name: "/src/lib.rs".to_owned() }),
        told(SubResponse::SourceTextResult { text: None }),
        told(SubResponse::LocalFilePathResult { name: Some("src/lib.rs".to_owned()) }),
        told(SubResponse::LineColumnResult { line: 1, column: u32::MAX }),
        told(SubResponse::ByteRangeResult { range: 4..9 }),
        told(SubResponse::SpanSourceResult { file_id: 3, ast_id: 1, start: 0, end: 1, ctx: 2 }),
        told(SubResponse::SpanParentResult {
            parent_span: Some(ParentSpan { file_id: 3, ast_id: 1, start: 0, end: 1, ctx: 2 }),
        }),
        told(SubResponse::SpanJoinResult { span: Some(SpanJoin { ast_id: 1, start: 0, end: 5, ctx: 0 }) }),
        told(SubResponse::Cancel { reason: "callback panicked or was cancelled".to_owned() }),
        BidirectionalMessage::Request(Request::ApiVersionCheck(ApiVersionCheck {})),
        BidirectionalMessage::Request(Request::SetConfig(ServerConfig { span_mode: SpanMode::Id })),
    ];
    for tree in trees() {
        let mut table = SpanDataIndexMap::default();
        let flat = FlatTree::from_subtree(tree.view(), version::CURRENT_API_VERSION, &mut table);
        messages.push(answer(Response::ExpandMacro(Ok(ExpandMacroResponse {
            tree: flat,
            span_data_table: serialize_span_data_index_map(&table),
            tracked_env_vars: tracked_env_vars(),
            tracked_paths: tracked_paths(),
        }))));
    }

    // `WIRE_TREES`: an attribute macro's input and attribute, one tree after the other, and the
    // answers, a tree or a panic.
    let mut fixtures = trees().into_iter();
    let mut output = Vec::new();
    fixtures.next().expect("a fixture tree").write_wire(&mut output);
    let mut call = Vec::new();
    fixtures.next().expect("a fixture tree").write_wire(&mut call);
    fixtures.next().expect("a fixture tree").write_wire(&mut call);
    messages.push(BidirectionalMessage::Request(Request::ExpandMacroWire(Box::new(
        ExpandMacroWire {
            lib: Utf8PathBuf::from("/target/debug/deps/libderive.so"),
            env: vec![("CARGO_PKG_NAME".to_owned(), "x".to_owned())],
            current_dir: Some("/src".to_owned()),
            macro_name: "attr".to_owned(),
            trees: call,
            has_attributes: true,
            def_site: WireSpan::of(at(0, 0)),
            call_site: WireSpan::of(at(0, 6)),
            mixed_site: WireSpan::of(at(7, 5)),
        },
    ))));
    messages.push(answer(Response::ExpandMacroWire(Ok(ExpandMacroWireResponse {
        tree: output,
        tracked_env_vars: tracked_env_vars(),
        tracked_paths: tracked_paths(),
    }))));
    messages.push(answer(Response::ExpandMacroWire(Err(PanicMessage("the macro panicked".to_owned())))));
    messages
}

/// What an answer's macro tracked: a variable set, one unset, and two paths.
fn tracked_env_vars<T: FromIterator<(Box<str>, Option<Box<str>>)>>() -> T {
    [("CARGO_PKG_NAME", Some("x")), ("UNSET", None)]
        .into_iter()
        .map(|(name, value)| (Box::<str>::from(name), value.map(Box::<str>::from)))
        .collect()
}

fn tracked_paths<T: FromIterator<Box<str>>>() -> T {
    ["/src/lib.rs", "/a b"].into_iter().map(Box::<str>::from).collect()
}

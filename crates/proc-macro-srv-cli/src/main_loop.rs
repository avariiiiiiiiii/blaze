//! The main loop of the proc-macro server: each request blaze sends answered in turn, with an
//! expansion's sub-requests asked of blaze in between in the bidirectional protocol.
//!
//! The server implements every request of [`CURRENT_API_VERSION`]: the version check, the
//! span mode, listing a library's macros, expanding a `FlatTree` in either span mode, and
//! expanding wire trees (`version::WIRE_TREES`). `proc-macro-srv` marks each expansion's scope
//! (`version::SCOPED_EXPANSIONS`), counting every expansion asked for, read or not.
//!
//! A connection reads every frame into one buffer and writes every answer from another, and its
//! wire expansions keep their scratch and output buffers, so a request allocates only what its
//! message decodes to and what the macro makes.

use std::{
    io::{self, BufRead, Write},
    mem,
    ops::Range,
    panic::{panic_any, resume_unwind},
};

use paths::Utf8Path;
use proc_macro_api::{
    ProtocolFormat,
    bidirectional_protocol::msg::{
        self as bidirectional, ApiVersionCheck, BidirectionalMessage, ExpandMacroResponse,
        ExpandMacroWireResponse, ListMacros,
    },
    legacy_protocol::{
        self,
        msg::{self as legacy, Message},
    },
    version::CURRENT_API_VERSION,
    wire::{ExpandMacroWire, WireSpan, raw_part},
};
use proc_macro_srv::{
    EnvSnapshot, ExpandError, ProcMacroClientError, ProcMacroClientHandle,
    ProcMacroClientInterface, ProcMacroPanicMarker, ProcMacroSrv, SpanId, TokenStream, TrackedEnv,
    span::{EditionedFileId, FileId, Span, TextRange, TextSize},
};
use tt::WirePart;

/// The spans of [`legacy::SpanMode::Id`]: a number each, carried through as it came.
struct SpanTrans;

impl legacy::SpanTransformer for SpanTrans {
    type Table = ();
    type Span = SpanId;

    fn token_id_of(_: &mut (), span: SpanId) -> legacy_protocol::SpanId {
        legacy_protocol::SpanId(span.0)
    }

    fn span_for_token_id(_: &(), id: legacy_protocol::SpanId) -> Result<SpanId, String> {
        Ok(SpanId(id.0))
    }
}

/// Answers the requests read from `input` on `output`, in `format`, until `input` ends.
pub fn run<R: BufRead + Send + Sync, W: Write + Send + Sync>(
    input: &mut R,
    output: &mut W,
    format: ProtocolFormat,
) -> io::Result<()> {
    match format {
        ProtocolFormat::JsonLegacy => run_legacy(input, output),
        ProtocolFormat::BidirectionalPostcardPrototype => run_bidirectional(input, output),
    }
}

/// Connects to blaze's socket at `socket`, as a warm server its pool started does: the pool
/// names the socket in `blaze_usercode::pool::SOCKET`, as a spawn in a sandbox has no pipes.
#[cfg(unix)]
pub fn connect(socket: &std::path::Path) -> io::Result<std::os::unix::net::UnixStream> {
    std::os::unix::net::UnixStream::connect(socket).map_err(|error| {
        // alloc: error path, naming the socket.
        io::Error::new(error.kind(), format!("connecting to {}: {error}", socket.display()))
    })
}

/// Answers the requests that come on `stream` in the bidirectional protocol, as a warm server.
#[cfg(unix)]
pub fn run_warm(stream: std::os::unix::net::UnixStream) -> io::Result<()> {
    let mut output = stream.try_clone()?;
    let mut input = io::BufReader::new(stream);
    run_bidirectional(&mut input, &mut output)
}

fn run_bidirectional<R: BufRead + Send + Sync, W: Write + Send + Sync>(
    input: &mut R,
    output: &mut W,
) -> io::Result<()> {
    let env = EnvSnapshot::default();
    let srv = ProcMacroSrv::new(&env);
    let mut span_mode = legacy::SpanMode::Id;
    // alloc: once per connection; each keeps its room across the connection's requests.
    let (mut read, mut written, mut wire) = (Vec::new(), Vec::new(), WireBuffers::default());

    while let Some(message) = BidirectionalMessage::read(input, &mut read)? {
        let BidirectionalMessage::Request(request) = message else {
            return Err(out_of_turn(&message));
        };
        let response = match request {
            bidirectional::Request::ListMacros(ListMacros { dylib_path }) => {
                bidirectional::Response::ListMacros(list(&srv, &dylib_path))
            }
            bidirectional::Request::ApiVersionCheck(ApiVersionCheck {}) => {
                bidirectional::Response::ApiVersionCheck(CURRENT_API_VERSION)
            }
            bidirectional::Request::SetConfig(config) => {
                span_mode = config.span_mode;
                bidirectional::Response::SetConfig(config)
            }
            bidirectional::Request::ExpandMacro(task) => {
                let bidirectional::ExpandMacro { lib, env, current_dir, data } = *task;
                let bidirectional::ExpandMacroData {
                    macro_body,
                    macro_name,
                    attributes,
                    has_global_spans: bidirectional::ExpnGlobals { def_site, call_site, mixed_site },
                    span_data_table,
                } = data;
                let task = FlatTask {
                    lib: &lib,
                    env: &env,
                    current_dir: current_dir.as_deref(),
                    macro_name: &macro_name,
                    macro_body,
                    attributes,
                    sites: [def_site, call_site, mixed_site],
                    span_data_table: &span_data_table,
                };
                let mut tracked = TrackedEnv::default();
                let mut client = Client { input: &mut *input, output: &mut *output, buf: &mut read };
                // Only spans of `SpanMode::RustAnalyzer` are blaze's to ask about.
                let callback: Option<ProcMacroClientHandle<'_>> = match span_mode {
                    legacy::SpanMode::Id => None,
                    legacy::SpanMode::RustAnalyzer => Some(&mut client),
                };
                let expanded = expand_flat(&srv, span_mode, task, &mut tracked, callback);
                bidirectional::Response::ExpandMacro(
                    expanded
                        .map(|(tree, span_data_table)| ExpandMacroResponse {
                            tree,
                            span_data_table,
                            tracked_env_vars: tracked.env_vars,
                            tracked_paths: tracked.paths,
                        })
                        .map_err(legacy::PanicMessage),
                )
            }
            bidirectional::Request::ExpandMacroWire(task) => {
                let mut tracked = TrackedEnv::default();
                let mut client = Client { input: &mut *input, output: &mut *output, buf: &mut read };
                let expanded = expand_wire(&srv, &task, &mut wire, &mut tracked, Some(&mut client));
                bidirectional::Response::ExpandMacroWire(
                    expanded
                        .map(|()| ExpandMacroWireResponse {
                            tree: mem::take(&mut wire.out),
                            tracked_env_vars: tracked.env_vars,
                            tracked_paths: tracked.paths,
                        })
                        .map_err(legacy::PanicMessage),
                )
            }
        };
        let response = BidirectionalMessage::Response(response);
        response.write_in(output, &mut written)?;
        if let BidirectionalMessage::Response(bidirectional::Response::ExpandMacroWire(Ok(answer))) =
            response
        {
            wire.out = answer.tree;
        }
    }
    Ok(())
}

/// The newline-delimited JSON protocol: no sub-requests, so a macro asks blaze nothing.
fn run_legacy<R: BufRead, W: Write>(input: &mut R, output: &mut W) -> io::Result<()> {
    let env = EnvSnapshot::default();
    let srv = ProcMacroSrv::new(&env);
    let mut span_mode = legacy::SpanMode::Id;
    // alloc: once per connection; each keeps its room across the connection's requests.
    let (mut read, mut wire) = (String::new(), WireBuffers::default());

    while let Some(request) = legacy::Request::read(input, &mut read)? {
        let response = match request {
            legacy::Request::ApiVersionCheck {} => {
                legacy::Response::ApiVersionCheck(CURRENT_API_VERSION)
            }
            legacy::Request::ListMacros { dylib_path } => {
                legacy::Response::ListMacros(list(&srv, &dylib_path))
            }
            legacy::Request::SetConfig(config) => {
                span_mode = config.span_mode;
                legacy::Response::SetConfig(config)
            }
            legacy::Request::ExpandMacro(task) => {
                let legacy::ExpandMacro { lib, env, current_dir, data } = *task;
                let legacy::ExpandMacroData {
                    macro_body,
                    macro_name,
                    attributes,
                    has_global_spans:
                        legacy::ExpnGlobals { serialize: _, def_site, call_site, mixed_site },
                    span_data_table,
                } = data;
                let task = FlatTask {
                    lib: &lib,
                    env: &env,
                    current_dir: current_dir.as_deref(),
                    macro_name: &macro_name,
                    macro_body,
                    attributes,
                    sites: [def_site, call_site, mixed_site],
                    span_data_table: &span_data_table,
                };
                let expanded =
                    expand_flat(&srv, span_mode, task, &mut TrackedEnv::default(), None);
                match span_mode {
                    legacy::SpanMode::Id => legacy::Response::ExpandMacro(
                        expanded.map(|(tree, _)| tree).map_err(legacy::PanicMessage),
                    ),
                    legacy::SpanMode::RustAnalyzer => legacy::Response::ExpandMacroExtended(
                        expanded
                            .map(|(tree, span_data_table)| legacy::ExpandMacroExtended {
                                tree,
                                span_data_table,
                            })
                            .map_err(legacy::PanicMessage),
                    ),
                }
            }
            legacy::Request::ExpandMacroWire(task) => {
                let expanded =
                    expand_wire(&srv, &task, &mut wire, &mut TrackedEnv::default(), None);
                legacy::Response::ExpandMacroWire(
                    expanded.map(|()| mem::take(&mut wire.out)).map_err(legacy::PanicMessage),
                )
            }
        };
        response.write(output)?;
        if let legacy::Response::ExpandMacroWire(Ok(tree)) = response {
            wire.out = tree;
        }
    }
    Ok(())
}

/// A message blaze sent where only a request may come: the connection is broken.
fn out_of_turn(message: &BidirectionalMessage) -> io::Error {
    let what = match message {
        BidirectionalMessage::Request(_) => "a request",
        BidirectionalMessage::Response(_) => "a response",
        BidirectionalMessage::SubRequest(_) => "a sub-request",
        BidirectionalMessage::SubResponse(_) => "a sub-response",
    };
    // alloc: error path, naming the message.
    io::Error::new(io::ErrorKind::InvalidData, format!("blaze sent {what} where a request goes"))
}

fn list(
    srv: &ProcMacroSrv<'_>,
    dylib_path: &Utf8Path,
) -> Result<Vec<(String, proc_macro_api::ProcMacroKind)>, String> {
    let macros = srv.list_macros(dylib_path)?;
    let kind = |kind: proc_macro_srv::ProcMacroKind| match kind {
        proc_macro_srv::ProcMacroKind::CustomDerive => proc_macro_api::ProcMacroKind::CustomDerive,
        proc_macro_srv::ProcMacroKind::Bang => proc_macro_api::ProcMacroKind::Bang,
        proc_macro_srv::ProcMacroKind::Attr => proc_macro_api::ProcMacroKind::Attr,
    };
    // alloc: none of its own: the list is mapped in place, once per library listed.
    Ok(macros.into_iter().map(|(name, it)| (name, kind(it))).collect())
}

fn expand_error(error: ExpandError) -> String {
    error.into_string().unwrap_or_default()
}

/// An expansion of [`legacy::FlatTree`]s, as either protocol asks for one.
struct FlatTask<'t> {
    lib: &'t Utf8Path,
    env: &'t [(String, String)],
    current_dir: Option<&'t str>,
    macro_name: &'t str,
    macro_body: legacy::FlatTree,
    attributes: Option<legacy::FlatTree>,
    /// The def, call and mixed sites: span ids in [`legacy::SpanMode::Id`], else indices of
    /// the span table.
    sites: [usize; 3],
    span_data_table: &'t [u32],
}

/// The expansion's output tree, and in [`legacy::SpanMode::RustAnalyzer`] its span table.
fn expand_flat<'a>(
    srv: &ProcMacroSrv<'_>,
    span_mode: legacy::SpanMode,
    task: FlatTask<'_>,
    tracked_env: &'a mut TrackedEnv,
    callback: Option<ProcMacroClientHandle<'a>>,
) -> Result<(legacy::FlatTree, Vec<u32>), String> {
    let FlatTask { lib, env, current_dir, macro_name, macro_body, attributes, sites, span_data_table } =
        task;
    let unread = |why: String| {
        srv.count_unexpanded();
        why
    };
    match span_mode {
        legacy::SpanMode::Id => {
            let [def_site, call_site, mixed_site] = sites.map(|site| SpanId(site as u32));
            let body = macro_body
                .to_tokenstream_unresolved::<SpanTrans>(CURRENT_API_VERSION, |_, b| b)
                .map_err(unread)?;
            let attributes = attributes
                .map(|it| it.to_tokenstream_unresolved::<SpanTrans>(CURRENT_API_VERSION, |_, b| b))
                .transpose()
                .map_err(unread)?;
            let output = srv
                .expand(
                    lib,
                    env,
                    current_dir,
                    macro_name,
                    body,
                    attributes,
                    def_site,
                    call_site,
                    mixed_site,
                    tracked_env,
                    callback,
                )
                .map_err(expand_error)?;
            let tree = legacy::FlatTree::from_tokenstream_raw::<SpanTrans>(
                output,
                call_site,
                CURRENT_API_VERSION,
            );
            // alloc: none, empty: spans of `SpanMode::Id` have no table.
            Ok((tree, Vec::new()))
        }
        legacy::SpanMode::RustAnalyzer => {
            let join = |a, b| srv.join_spans(a, b).unwrap_or(b);
            let read = || {
                let table = legacy::deserialize_span_data_index_map(span_data_table)?;
                let site = |at: usize| match table.get_index(at) {
                    Some(&span) => Ok(span),
                    // alloc: error path, naming the site.
                    None => Err(format!("site span {at} past the span table of {}", table.len())),
                };
                let sites = [site(sites[0])?, site(sites[1])?, site(sites[2])?];
                let body =
                    macro_body.to_tokenstream_resolved(CURRENT_API_VERSION, &table, join)?;
                let attributes = attributes
                    .map(|it| it.to_tokenstream_resolved(CURRENT_API_VERSION, &table, join))
                    .transpose()?;
                Ok::<_, String>((table, sites, body, attributes))
            };
            let (mut table, [def_site, call_site, mixed_site], body, attributes) =
                read().map_err(unread)?;
            let output = srv
                .expand(
                    lib,
                    env,
                    current_dir,
                    macro_name,
                    body,
                    attributes,
                    def_site,
                    call_site,
                    mixed_site,
                    tracked_env,
                    callback,
                )
                .map_err(expand_error)?;
            let tree =
                legacy::FlatTree::from_tokenstream(output, CURRENT_API_VERSION, call_site, &mut table);
            Ok((tree, legacy::serialize_span_data_index_map(&table)))
        }
    }
}

/// The buffers a connection's wire expansions are read and written in, kept across them.
#[derive(Default)]
struct WireBuffers {
    /// A copy of trees that don't start at a 4-byte boundary.
    scratch: Vec<u8>,
    /// The output tree last written, given back once its answer has been.
    out: Vec<u8>,
}

/// Expands `task`, its output tree written to `buffers.out`.
fn expand_wire<'a>(
    srv: &ProcMacroSrv<'_>,
    task: &ExpandMacroWire,
    buffers: &mut WireBuffers,
    tracked_env: &'a mut TrackedEnv,
    callback: Option<ProcMacroClientHandle<'a>>,
) -> Result<(), String> {
    let (body, attributes, [def_site, call_site, mixed_site]) =
        match wire_inputs(srv, task, &mut buffers.scratch) {
            Ok(inputs) => inputs,
            Err(why) => {
                srv.count_unexpanded();
                return Err(why);
            }
        };
    let output = srv
        .expand(
            &task.lib,
            &task.env,
            task.current_dir.as_deref(),
            &task.macro_name,
            body,
            attributes,
            def_site,
            call_site,
            mixed_site,
            tracked_env,
            callback,
        )
        .map_err(expand_error)?;
    buffers.out.clear();
    output.write_wire(call_site, &mut buffers.out)
}

/// The macro's input, its attribute and the three sites of a wire request, every id refused
/// that no span has (`raw_part`).
fn wire_inputs(
    srv: &ProcMacroSrv<'_>,
    task: &ExpandMacroWire,
    scratch: &mut Vec<u8>,
) -> Result<(TokenStream<Span>, Option<TokenStream<Span>>, [Span; 3]), String> {
    let join = |open: Span, close: Span| srv.join_spans(open, close).unwrap_or(close);
    let site = |site: WireSpan, what: &str| {
        // alloc: error path, naming the site.
        site.resolve(&mut raw_part).map_err(|why| format!("the request's {what}: {why}"))
    };
    let sites = [
        site(task.def_site, "def site")?,
        site(task.call_site, "call site")?,
        site(task.mixed_site, "mixed site")?,
    ];
    let (body, used) = TokenStream::<Span>::read_wire(&task.trees, scratch, &mut raw_part, join)
        // alloc: error path, naming the tree.
        .map_err(|why| format!("the request's input: {why}"))?;
    let rest = task.trees.get(used..).unwrap_or_default();
    let (attributes, left) = match task.has_attributes {
        true => {
            let (attribute, used) =
                TokenStream::<Span>::read_wire(rest, scratch, &mut raw_part, join)
                // alloc: error path, naming the tree.
                .map_err(|why| format!("the request's attribute: {why}"))?;
            (Some(attribute), rest.len() - used)
        }
        false => (None, rest.len()),
    };
    if left != 0 {
        // alloc: error path, naming the length.
        return Err(format!("the request's trees have {left} bytes after the last"));
    }
    Ok((body, attributes, sites))
}

/// An expansion's way to ask blaze its sub-requests: the connection the expansion came on,
/// whose read buffer the expansion's request is done with.
struct Client<'c, R, W> {
    input: &'c mut R,
    output: &'c mut W,
    buf: &'c mut Vec<u8>,
}

impl<R: BufRead, W: Write> Client<'_, R, W> {
    fn roundtrip(
        &mut self,
        request: bidirectional::SubRequest,
    ) -> Result<bidirectional::SubResponse, ProcMacroClientError> {
        BidirectionalMessage::SubRequest(request)
            .write_in(&mut *self.output, self.buf)
            .map_err(ProcMacroClientError::Io)?;
        let message = BidirectionalMessage::read(&mut *self.input, self.buf)
            .map_err(ProcMacroClientError::Io)?
            .ok_or(ProcMacroClientError::Eof)?;
        match message {
            BidirectionalMessage::SubResponse(bidirectional::SubResponse::Cancel { reason }) => {
                Err(ProcMacroClientError::Cancelled { reason })
            }
            BidirectionalMessage::SubResponse(response) => Ok(response),
            // alloc: error path, naming the message.
            other => Err(ProcMacroClientError::Protocol(format!(
                "expected a sub-response, got {other:?}"
            ))),
        }
    }
}

/// Ends the expansion that asked: blaze cancelled it, the connection broke or blaze answered
/// something else than what was asked.
fn handle_failure(failure: Result<bidirectional::SubResponse, ProcMacroClientError>) -> ! {
    match failure {
        Err(ProcMacroClientError::Cancelled { reason }) => {
            // alloc: error path, the marker the expansion ends with.
            resume_unwind(Box::new(ProcMacroPanicMarker::Cancelled { reason }));
        }
        Err(err) => panic_any(ProcMacroPanicMarker::Internal {
            // alloc: error path, naming the failure.
            reason: format!("proc-macro IPC error: {err:?}"),
        }),
        Ok(other) => panic_any(ProcMacroPanicMarker::Internal {
            // alloc: error path, naming the answer.
            reason: format!("unexpected SubResponse {other:?}"),
        }),
    }
}

/// The span blaze answered with, its ids checked as a request's are ([`raw_part`]): an answer
/// that is no span ends the expansion that asked.
fn span_of(file_id: u32, ast_id: u32, start: u32, end: u32, ctx: u32) -> Span {
    let (anchor, ctx) = match raw_part(WirePart { file_id, ast_id, ctx }) {
        Ok(part) => part,
        Err(why) => panic_any(ProcMacroPanicMarker::Internal {
            // alloc: error path, naming why.
            reason: format!("blaze answered with no span: {why}"),
        }),
    };
    if start > end {
        panic_any(ProcMacroPanicMarker::Internal {
            // alloc: error path, naming the range.
            reason: format!("blaze answered with a span from {start} back to {end}"),
        });
    }
    Span { range: TextRange::new(TextSize::new(start), TextSize::new(end)), anchor, ctx }
}

fn raw(span: Span) -> (u32, u32, u32, u32, u32) {
    let Span { range, anchor, ctx } = span;
    (
        anchor.file_id.as_u32(),
        anchor.ast_id.into_raw(),
        range.start().into(),
        range.end().into(),
        ctx.into_u32(),
    )
}

impl<R: BufRead, W: Write> ProcMacroClientInterface for Client<'_, R, W> {
    fn file(&mut self, file_id: FileId) -> String {
        match self.roundtrip(bidirectional::SubRequest::FilePath { file_id: file_id.index() }) {
            Ok(bidirectional::SubResponse::FilePathResult { name }) => name,
            other => handle_failure(other),
        }
    }

    fn source_text(&mut self, span: Span) -> Option<String> {
        let (file_id, ast_id, start, end, _) = raw(span);
        match self.roundtrip(bidirectional::SubRequest::SourceText { file_id, ast_id, start, end }) {
            Ok(bidirectional::SubResponse::SourceTextResult { text }) => text,
            other => handle_failure(other),
        }
    }

    fn local_file(&mut self, file_id: FileId) -> Option<String> {
        match self.roundtrip(bidirectional::SubRequest::LocalFilePath { file_id: file_id.index() })
        {
            Ok(bidirectional::SubResponse::LocalFilePathResult { name }) => name,
            other => handle_failure(other),
        }
    }

    fn line_column(&mut self, span: Span) -> Option<(u32, u32)> {
        let (file_id, ast_id, offset, _, _) = raw(span);
        match self.roundtrip(bidirectional::SubRequest::LineColumn { file_id, ast_id, offset }) {
            Ok(bidirectional::SubResponse::LineColumnResult { line, column }) => {
                Some((line, column))
            }
            other => handle_failure(other),
        }
    }

    fn byte_range(&mut self, span: Span) -> Range<usize> {
        let (file_id, ast_id, start, end, _) = raw(span);
        match self.roundtrip(bidirectional::SubRequest::ByteRange { file_id, ast_id, start, end }) {
            Ok(bidirectional::SubResponse::ByteRangeResult { range }) => range,
            other => handle_failure(other),
        }
    }

    fn span_source(&mut self, span: Span) -> Span {
        let (file_id, ast_id, start, end, ctx) = raw(span);
        let request = bidirectional::SubRequest::SpanSource { file_id, ast_id, start, end, ctx };
        match self.roundtrip(request) {
            Ok(bidirectional::SubResponse::SpanSourceResult { file_id, ast_id, start, end, ctx }) => {
                span_of(file_id, ast_id, start, end, ctx)
            }
            other => handle_failure(other),
        }
    }

    fn span_parent(&mut self, span: Span) -> Option<Span> {
        let (file_id, ast_id, start, end, ctx) = raw(span);
        let request = bidirectional::SubRequest::SpanParent { file_id, ast_id, start, end, ctx };
        match self.roundtrip(request) {
            Ok(bidirectional::SubResponse::SpanParentResult { parent_span }) => {
                parent_span.map(|bidirectional::ParentSpan { file_id, ast_id, start, end, ctx }| {
                    span_of(file_id, ast_id, start, end, ctx)
                })
            }
            other => handle_failure(other),
        }
    }

    fn span_join(&mut self, first: Span, second: Span) -> Option<Span> {
        // Spans of two files join to none, as rustc's `Span::join` has it.
        if first.anchor.file_id != second.anchor.file_id {
            return None;
        }
        let file: EditionedFileId = first.anchor.file_id;
        let (file_id, ast_id_first, start_first, end_first, ctx_first) = raw(first);
        let (_, ast_id_second, start_second, end_second, ctx_second) = raw(second);
        let request = bidirectional::SubRequest::SpanJoin {
            file_id,
            ast_id_first,
            start_first,
            end_first,
            ctx_first,
            ast_id_second,
            start_second,
            end_second,
            ctx_second,
        };
        match self.roundtrip(request) {
            Ok(bidirectional::SubResponse::SpanJoinResult { span }) => {
                span.map(|bidirectional::SpanJoin { ast_id, start, end, ctx }| {
                    span_of(file.as_u32(), ast_id, start, end, ctx)
                })
            }
            other => handle_failure(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use intern::Symbol;
    use proc_macro_api::{
        bidirectional_protocol::msg::{Request, Response, SubResponse},
        legacy_protocol::msg::ServerConfig,
        version,
    };
    use proc_macro_srv::span::{Edition, ROOT_ERASED_FILE_AST_ID, SpanAnchor, SyntaxContext};
    use tt::{TopSubtree, TopSubtreeBuilder};

    use super::*;

    /// The library of `proc-macro-test-impl` blaze linked for these tests, by
    /// `proc-macro-libraries` in this package's manifest.
    fn library() -> paths::Utf8PathBuf {
        env!("BLAZE_PROC_MACRO_LIBRARY_PROC_MACRO_TEST_IMPL").into()
    }

    fn span(start: u32) -> Span {
        Span {
            range: TextRange::at(TextSize::new(start), TextSize::new(1)),
            anchor: SpanAnchor {
                file_id: EditionedFileId::new(FileId::from_raw(1), Edition::CURRENT),
                ast_id: ROOT_ERASED_FILE_AST_ID,
            },
            // SAFETY: no database; the context is carried and compared, never looked up.
            ctx: unsafe { SyntaxContext::from_u32(0) },
        }
    }

    /// `foo + 1u8`.
    fn tree() -> TopSubtree {
        let mut builder = TopSubtreeBuilder::new(tt::Delimiter::invisible_spanned(span(0)));
        builder.push(tt::Leaf::Ident(tt::Ident {
            sym: Symbol::intern("foo"),
            span: span(1),
            is_raw: tt::IdentIsRaw::No,
        }));
        builder.push(tt::Leaf::Punct(tt::Punct {
            char: '+',
            spacing: tt::Spacing::Alone,
            span: span(2),
        }));
        builder.push(tt::Leaf::Literal(tt::Literal::new("1", span(3), tt::LitKind::Integer, "u8")));
        builder.build()
    }

    fn tokens(tree: &TopSubtree) -> Vec<tt::TokenTree> {
        tree.token_trees().iter_flat_tokens().collect()
    }

    fn wire_task(macro_name: &str, trees: &[&TopSubtree]) -> ExpandMacroWire {
        let mut bytes = Vec::new();
        for tree in trees {
            tree.write_wire(&mut bytes);
        }
        ExpandMacroWire {
            lib: library(),
            env: Vec::new(),
            current_dir: None,
            macro_name: macro_name.into(),
            trees: bytes,
            has_attributes: trees.len() == 2,
            def_site: WireSpan::of(span(0)),
            call_site: WireSpan::of(span(0)),
            mixed_site: WireSpan::of(span(0)),
        }
    }

    fn wire(task: ExpandMacroWire) -> BidirectionalMessage {
        BidirectionalMessage::Request(Request::ExpandMacroWire(Box::new(task)))
    }

    /// What `run` answers `messages` with in the bidirectional protocol, and how it ended.
    fn converse(messages: &[BidirectionalMessage]) -> (io::Result<()>, Vec<BidirectionalMessage>) {
        let (mut input, mut frame) = (Vec::new(), Vec::new());
        for message in messages {
            message.write_in(&mut input, &mut frame).unwrap();
        }
        let mut output = Vec::new();
        let ran = run(&mut &input[..], &mut output, ProtocolFormat::BidirectionalPostcardPrototype);
        let (mut answers, mut reader) = (Vec::new(), &output[..]);
        while let Some(answer) = BidirectionalMessage::read(&mut reader, &mut frame).unwrap() {
            answers.push(answer);
        }
        (ran, answers)
    }

    fn answered_tree(answer: &BidirectionalMessage) -> TopSubtree {
        let BidirectionalMessage::Response(Response::ExpandMacroWire(Ok(answer))) = answer else {
            panic!("not an expansion's tree: {answer:?}");
        };
        let (tree, used) = TopSubtree::read_wire(&answer.tree, &mut raw_part).unwrap();
        assert_eq!(used, answer.tree.len());
        tree
    }

    fn refusal(answer: &BidirectionalMessage) -> &str {
        let BidirectionalMessage::Response(Response::ExpandMacroWire(Err(why))) = answer else {
            panic!("not a refused expansion: {answer:?}");
        };
        &why.0
    }

    #[test]
    fn the_server_reports_the_newest_version_and_echoes_its_span_mode() {
        let config = ServerConfig { span_mode: legacy::SpanMode::RustAnalyzer };
        let (ran, answers) = converse(&[
            BidirectionalMessage::Request(Request::ApiVersionCheck(ApiVersionCheck {})),
            BidirectionalMessage::Request(Request::SetConfig(config)),
        ]);
        ran.unwrap();
        assert_eq!(CURRENT_API_VERSION, version::WIRE_TREES);
        assert_eq!(
            answers,
            [
                BidirectionalMessage::Response(Response::ApiVersionCheck(version::WIRE_TREES)),
                BidirectionalMessage::Response(Response::SetConfig(ServerConfig {
                    span_mode: legacy::SpanMode::RustAnalyzer
                })),
            ]
        );
    }

    #[test]
    fn a_library_blaze_linked_lists_its_macros() {
        let (ran, answers) = converse(&[BidirectionalMessage::Request(Request::ListMacros(
            ListMacros { dylib_path: library() },
        ))]);
        ran.unwrap();
        let [BidirectionalMessage::Response(Response::ListMacros(Ok(macros)))] = &answers[..] else {
            panic!("{answers:?}");
        };
        assert!(macros.contains(&("fn_like_noop".into(), proc_macro_api::ProcMacroKind::Bang)));
        assert!(macros.contains(&("attr_noop".into(), proc_macro_api::ProcMacroKind::Attr)));
        assert!(
            macros.contains(&("DeriveEmpty".into(), proc_macro_api::ProcMacroKind::CustomDerive))
        );
    }

    #[test]
    fn a_wire_expansion_gives_back_what_the_macro_made() {
        let (ran, answers) = converse(&[wire(wire_task("fn_like_noop", &[&tree()]))]);
        ran.unwrap();
        assert_eq!(tokens(&answered_tree(&answers[0])), tokens(&tree()));
    }

    #[test]
    fn an_attribute_is_read_after_the_input() {
        let mut attribute = TopSubtreeBuilder::new(tt::Delimiter::invisible_spanned(span(9)));
        attribute.push(tt::Leaf::Punct(tt::Punct {
            char: '!',
            spacing: tt::Spacing::Alone,
            span: span(9),
        }));
        let (ran, answers) =
            converse(&[wire(wire_task("attr_noop", &[&tree(), &attribute.build()]))]);
        ran.unwrap();
        assert_eq!(tokens(&answered_tree(&answers[0])), tokens(&tree()));
    }

    #[test]
    fn a_request_that_cannot_be_read_is_refused_and_the_next_still_expands() {
        let mut longer = wire_task("fn_like_noop", &[&tree()]);
        longer.trees.extend_from_slice(&[0; 4]);
        let mut bad_site = wire_task("fn_like_noop", &[&tree()]);
        bad_site.call_site.ctx = u32::MAX;
        let (ran, answers) = converse(&[
            wire(longer),
            wire(bad_site),
            wire(wire_task("fn_like_noop", &[&tree()])),
        ]);
        ran.unwrap();
        assert!(refusal(&answers[0]).contains("bytes after the last"), "{answers:?}");
        assert!(refusal(&answers[1]).contains("call site"), "{answers:?}");
        assert_eq!(tokens(&answered_tree(&answers[2])), tokens(&tree()));
    }

    #[test]
    fn a_macro_that_panics_is_answered_with_its_message() {
        let (ran, answers) = converse(&[wire(wire_task("fn_like_panic", &[&tree()]))]);
        ran.unwrap();
        assert!(refusal(&answers[0]).contains("fn_like_panic!"), "{answers:?}");
    }

    #[test]
    fn a_macro_asks_blaze_on_the_connection_it_came_on() {
        let line_column = || {
            BidirectionalMessage::SubResponse(SubResponse::LineColumnResult { line: 3, column: 7 })
        };
        let (ran, answers) = converse(&[
            wire(wire_task("fn_like_span_line_column", &[&tree()])),
            line_column(),
            line_column(),
        ]);
        ran.unwrap();
        let asked = bidirectional::SubRequest::LineColumn {
            file_id: span(1).anchor.file_id.as_u32(),
            ast_id: ROOT_ERASED_FILE_AST_ID.into_raw(),
            offset: 1,
        };
        assert_eq!(answers[0], BidirectionalMessage::SubRequest(asked));
        let literals: Vec<_> = tokens(&answered_tree(&answers[2]))
            .into_iter()
            .map(|token| match token {
                tt::TokenTree::Leaf(tt::Leaf::Literal(literal)) => {
                    literal.text_and_suffix().0.to_owned()
                }
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(literals, ["3", "7"]);
    }

    #[test]
    fn a_cancelled_sub_request_ends_only_its_expansion() {
        let cancel = BidirectionalMessage::SubResponse(SubResponse::Cancel { reason: "gone".into() });
        let (ran, answers) = converse(&[
            wire(wire_task("fn_like_span_line_column", &[&tree()])),
            cancel,
            wire(wire_task("fn_like_noop", &[&tree()])),
        ]);
        ran.unwrap();
        // Refused, whether the bridge hands the cancel back as one or as the macro's panic.
        refusal(&answers[1]);
        assert_eq!(tokens(&answered_tree(&answers[2])), tokens(&tree()));
    }

    #[test]
    fn a_message_out_of_turn_stops_the_server() {
        let (ran, answers) =
            converse(&[BidirectionalMessage::SubResponse(SubResponse::Cancel { reason: "x".into() })]);
        assert!(ran.unwrap_err().to_string().contains("a sub-response where a request goes"));
        assert!(answers.is_empty());
    }

    #[test]
    fn the_json_protocol_checks_its_version_and_expands_wire_trees() {
        let mut input = Vec::new();
        legacy::Request::ApiVersionCheck {}.write(&mut input).unwrap();
        legacy::Request::ExpandMacroWire(Box::new(wire_task("fn_like_noop", &[&tree()])))
            .write(&mut input)
            .unwrap();
        let mut output = Vec::new();
        run(&mut &input[..], &mut output, ProtocolFormat::JsonLegacy).unwrap();
        let (mut reader, mut line) = (&output[..], String::new());
        let version = legacy::Response::read(&mut reader, &mut line).unwrap();
        assert_eq!(version, Some(legacy::Response::ApiVersionCheck(version::WIRE_TREES)));
        let Some(legacy::Response::ExpandMacroWire(Ok(bytes))) =
            legacy::Response::read(&mut reader, &mut line).unwrap()
        else {
            panic!("no wire answer");
        };
        let (answer, _) = TopSubtree::read_wire(&bytes, &mut raw_part).unwrap();
        assert_eq!(tokens(&answer), tokens(&tree()));
    }
}

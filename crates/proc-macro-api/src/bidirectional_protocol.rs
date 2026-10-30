//! Bidirectional protocol methods

use std::{
    cell::RefCell,
    io::{self, BufRead, Write},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::Arc,
};

use paths::AbsPath;
use span::Span;

use crate::{
    ProcMacro, ProcMacroKind, ServerError,
    bidirectional_protocol::msg::{
        ApiVersionCheck, BidirectionalMessage, ExpandMacro, ExpandMacroData, ExpnGlobals,
        ListMacros, Request, Response, SubRequest, SubResponse,
    },
    legacy_protocol::{
        SpanMode, answer_error,
        msg::{FlatTree, ServerConfig, SpanDataIndexMap, serialize_span_data_index_map},
    },
    process::Claimed,
    transport::postcard,
    wire::{Sent, flat_answer},
};

pub mod msg;

/// What answers a server's sub-requests during a request: the database's answer
/// (`load_cargo::answer`), or a refusal while none can be asked.
// load-cargo's answer from the database, which this crate can't name (load-cargo depends on it).
// dyn: called once per sub-request, each a round trip to the server.
pub type SubCallback<'a> = &'a dyn Fn(SubRequest) -> Result<SubResponse, ServerError>;

/// One request and its answer, the server's sub-requests answered by `callback` in between,
/// every frame written and read in `buf`: a handshake's, or an older server's expansion. A
/// server of [`crate::version::WIRE_TREES`] expands over its connection's buffers instead
/// (`Claimed::expand_wire`).
pub fn run_conversation<W: Write + ?Sized, R: BufRead + ?Sized>(
    writer: &mut W,
    reader: &mut R,
    buf: &mut Vec<u8>,
    msg: BidirectionalMessage,
    callback: SubCallback<'_>,
) -> Result<BidirectionalMessage, ServerError> {
    postcard::encode_into(&msg, buf).map_err(wrap_encode)?;
    postcard::write(writer, buf).map_err(wrap_io("failed to write initial request"))?;

    loop {
        let maybe_buf = postcard::read(reader, buf).map_err(wrap_io("failed to read message"))?;
        let Some(b) = maybe_buf else {
            return Err(closed());
        };

        let msg: BidirectionalMessage = postcard::decode(b).map_err(wrap_decode)?;

        match msg {
            BidirectionalMessage::Response(response) => {
                return Ok(BidirectionalMessage::Response(response));
            }
            BidirectionalMessage::SubRequest(sr) => {
                let resp = BidirectionalMessage::SubResponse(answer(callback, sr));
                postcard::encode_into(&resp, buf).map_err(wrap_encode)?;
                postcard::write(writer, buf).map_err(wrap_io("failed to write sub-response"))?;
            }
            other => return Err(unexpected(&other)),
        }
    }
}

/// `callback`'s answer to the server's sub-request `request`, or a refusal saying why it gave
/// none: it failed, panicked or was cancelled.
pub(crate) fn answer(callback: SubCallback<'_>, request: SubRequest) -> SubResponse {
    // TODO: Avoid `AssertUnwindSafe` by making the callback `UnwindSafe` once `SourceDatabase`
    // becomes unwind-safe (currently blocked by `parking_lot::RwLock` in the VFS).
    match catch_unwind(AssertUnwindSafe(|| callback(request))) {
        Ok(Ok(response)) => response,
        // alloc: error path, the refusal's reason.
        Ok(Err(err)) => SubResponse::Cancel { reason: err.to_string() },
        // alloc: error path, as above.
        Err(_) => SubResponse::Cancel { reason: "callback panicked or was cancelled".into() },
    }
}

/// The server closed the stream before it answered.
pub(crate) fn closed() -> ServerError {
    ServerError {
        message: "proc-macro server closed the stream".into(),
        // alloc: error path.
        io: Some(Arc::new(io::Error::new(io::ErrorKind::UnexpectedEof, "closed"))),
    }
}

/// A message the server should not have sent then.
pub(crate) fn unexpected(message: &impl std::fmt::Debug) -> ServerError {
    // alloc: error path, naming the message.
    ServerError { message: format!("unexpected message {message:?}"), io: None }
}

pub(crate) fn wrap_io(msg: &'static str) -> impl Fn(io::Error) -> ServerError {
    // alloc: error path.
    move |err| ServerError { message: msg.into(), io: Some(Arc::new(err)) }
}

pub(crate) fn wrap_encode(err: io::Error) -> ServerError {
    // alloc: error path.
    ServerError { message: "failed to encode message".into(), io: Some(Arc::new(err)) }
}

pub(crate) fn wrap_decode(err: io::Error) -> ServerError {
    // alloc: error path.
    ServerError { message: "failed to decode message".into(), io: Some(Arc::new(err)) }
}

pub(crate) fn version_check(
    srv: &Claimed<'_>,
    callback: SubCallback<'_>,
) -> Result<u32, ServerError> {
    let request = BidirectionalMessage::Request(Request::ApiVersionCheck(ApiVersionCheck {}));

    let response_payload = run_request(srv, request, callback)?;

    match response_payload {
        BidirectionalMessage::Response(Response::ApiVersionCheck(version)) => Ok(version),
        other => Err(unexpected(&other)),
    }
}

/// Enable support for rust-analyzer span mode if the server supports it.
pub(crate) fn enable_rust_analyzer_spans(
    srv: &Claimed<'_>,
    callback: SubCallback<'_>,
) -> Result<SpanMode, ServerError> {
    let request = BidirectionalMessage::Request(Request::SetConfig(ServerConfig {
        span_mode: SpanMode::RustAnalyzer,
    }));

    let response_payload = run_request(srv, request, callback)?;

    match response_payload {
        BidirectionalMessage::Response(Response::SetConfig(ServerConfig { span_mode })) => {
            Ok(span_mode)
        }
        other => Err(unexpected(&other)),
    }
}

/// Finds proc-macros in a given dynamic library.
pub(crate) fn find_proc_macros(
    srv: &Claimed<'_>,
    dylib_path: &AbsPath,
    callback: SubCallback<'_>,
) -> Result<Result<Vec<(String, ProcMacroKind)>, String>, ServerError> {
    let request = BidirectionalMessage::Request(Request::ListMacros(ListMacros {
        // alloc: once per library.
        dylib_path: dylib_path.to_path_buf().into(),
    }));

    let response_payload = run_request(srv, request, callback)?;

    match response_payload {
        BidirectionalMessage::Response(Response::ListMacros(it)) => Ok(it),
        other => Err(unexpected(&other)),
    }
}

/// An expansion by a server older than [`crate::version::WIRE_TREES`], which takes and gives a
/// [`FlatTree`]: the request owns what it sends (the edge, to an older server). A newer server
/// expands over its connection's buffers (`Claimed::expand_wire`). The expansion saw the
/// variables `seen`, what the shell passed and then the crate's, for its trace.
pub(crate) fn expand(
    proc_macro: &ProcMacro,
    process: &Claimed<'_>,
    subtree: tt::SubtreeView<'_>,
    attr: Option<tt::SubtreeView<'_>>,
    env: Vec<(String, String)>,
    def_site: Span,
    call_site: Span,
    mixed_site: Span,
    current_dir: String,
    callback: SubCallback<'_>,
    seen: [&[(String, String)]; 2],
) -> Result<Result<tt::TopSubtree, String>, crate::ServerError> {
    let version = process.version();
    if let Some(err) = process.exited() {
        // clone: error path, the server's exit told to each caller.
        return Err(err.clone());
    }
    let mut span_data_table = SpanDataIndexMap::default();
    let def_site = span_data_table.insert_full(def_site).0;
    let call_site = span_data_table.insert_full(call_site).0;
    let mixed_site = span_data_table.insert_full(mixed_site).0;
    // alloc: an older server's request, which owns what it sends (the edge).
    let task = BidirectionalMessage::Request(Request::ExpandMacro(Box::new(ExpandMacro {
        data: ExpandMacroData {
            macro_body: FlatTree::from_subtree(subtree, version, &mut span_data_table),
            macro_name: proc_macro.name.to_string(), // alloc: as above.
            attributes: attr
                .map(|subtree| FlatTree::from_subtree(subtree, version, &mut span_data_table)),
            has_global_spans: ExpnGlobals { def_site, call_site, mixed_site },
            span_data_table: if process.rust_analyzer_spans() {
                serialize_span_data_index_map(&span_data_table)
            } else {
                Vec::new() // alloc: none, empty.
            },
        },
        lib: proc_macro.dylib_path.to_path_buf().into(),
        env,
        current_dir: Some(current_dir),
    })));

    let sent = RefCell::new(Sent::of_table(&span_data_table));
    let response_payload = process.run_expansion(task, &noting(&sent, callback), seen)?;

    match response_payload {
        BidirectionalMessage::Response(Response::ExpandMacro(Ok(resp))) => {
            let answered = Some(&resp.span_data_table[..]);
            let tree = flat_answer(resp.tree, version, &span_data_table, answered, &sent.borrow());
            Ok(Ok(tree.map_err(answer_error)?))
        }
        BidirectionalMessage::Response(Response::ExpandMacro(Err(msg))) => Ok(Err(msg.0)),
        other => Err(unexpected(&other)),
    }
}

/// `callback`, noting in `sent` each span it answers a sub-request with: the server may put it
/// in its output.
fn noting<'a>(
    sent: &'a RefCell<Sent>,
    callback: SubCallback<'a>,
) -> impl Fn(SubRequest) -> Result<SubResponse, ServerError> + 'a {
    move |request| {
        let joined_in = match &request {
            SubRequest::SpanJoin { file_id, .. } => Some(*file_id),
            _ => None,
        };
        let response = callback(request)?;
        sent.borrow_mut().sub_response(joined_in, &response);
        Ok(response)
    }
}

fn run_request(
    srv: &Claimed<'_>,
    msg: BidirectionalMessage,
    callback: SubCallback<'_>,
) -> Result<BidirectionalMessage, ServerError> {
    if let Some(err) = srv.exited() {
        // clone: error path, the server's exit told to each caller.
        return Err(err.clone());
    }
    srv.run_bidirectional(msg, callback)
}

pub fn reject_subrequests(req: SubRequest) -> Result<SubResponse, ServerError> {
    // alloc: error path, naming the sub-request.
    Err(ServerError { message: format!("{req:?} sub-request not supported here"), io: None })
}

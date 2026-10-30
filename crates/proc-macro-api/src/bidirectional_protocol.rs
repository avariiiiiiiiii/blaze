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
    version,
    wire::{ExpandMacroWire, Sent, flat_answer, read_tree},
};

pub mod msg;

pub type SubCallback<'a> = &'a dyn Fn(SubRequest) -> Result<SubResponse, ServerError>;

pub fn run_conversation(
    writer: &mut dyn Write,
    reader: &mut dyn BufRead,
    buf: &mut Vec<u8>,
    msg: BidirectionalMessage,
    callback: SubCallback<'_>,
) -> Result<BidirectionalMessage, ServerError> {
    let encoded = postcard::encode(&msg).map_err(wrap_encode)?;
    postcard::write(writer, &encoded).map_err(wrap_io("failed to write initial request"))?;

    loop {
        let maybe_buf = postcard::read(reader, buf).map_err(wrap_io("failed to read message"))?;
        let Some(b) = maybe_buf else {
            return Err(ServerError {
                message: "proc-macro server closed the stream".into(),
                io: Some(Arc::new(io::Error::new(io::ErrorKind::UnexpectedEof, "closed"))),
            });
        };

        let msg: BidirectionalMessage = postcard::decode(b).map_err(wrap_decode)?;

        match msg {
            BidirectionalMessage::Response(response) => {
                return Ok(BidirectionalMessage::Response(response));
            }
            BidirectionalMessage::SubRequest(sr) => {
                // TODO: Avoid `AssertUnwindSafe` by making the callback `UnwindSafe` once `SourceDatabase`
                // becomes unwind-safe (currently blocked by `parking_lot::RwLock` in the VFS).
                let resp = match catch_unwind(AssertUnwindSafe(|| callback(sr))) {
                    Ok(Ok(resp)) => BidirectionalMessage::SubResponse(resp),
                    Ok(Err(err)) => BidirectionalMessage::SubResponse(SubResponse::Cancel {
                        reason: err.to_string(),
                    }),
                    Err(_) => BidirectionalMessage::SubResponse(SubResponse::Cancel {
                        reason: "callback panicked or was cancelled".into(),
                    }),
                };

                let encoded = postcard::encode(&resp).map_err(wrap_encode)?;
                postcard::write(writer, &encoded)
                    .map_err(wrap_io("failed to write sub-response"))?;
            }
            _ => {
                return Err(ServerError {
                    message: format!("unexpected message {:?}", msg),
                    io: None,
                });
            }
        }
    }
}

fn wrap_io(msg: &'static str) -> impl Fn(io::Error) -> ServerError {
    move |err| ServerError { message: msg.into(), io: Some(Arc::new(err)) }
}

fn wrap_encode(err: io::Error) -> ServerError {
    ServerError { message: "failed to encode message".into(), io: Some(Arc::new(err)) }
}

fn wrap_decode(err: io::Error) -> ServerError {
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
        other => {
            Err(ServerError { message: format!("unexpected response: {:?}", other), io: None })
        }
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
        _ => Err(ServerError { message: "unexpected response".to_owned(), io: None }),
    }
}

/// Finds proc-macros in a given dynamic library.
pub(crate) fn find_proc_macros(
    srv: &Claimed<'_>,
    dylib_path: &AbsPath,
    callback: SubCallback<'_>,
) -> Result<Result<Vec<(String, ProcMacroKind)>, String>, ServerError> {
    let request = BidirectionalMessage::Request(Request::ListMacros(ListMacros {
        dylib_path: dylib_path.to_path_buf().into(),
    }));

    let response_payload = run_request(srv, request, callback)?;

    match response_payload {
        BidirectionalMessage::Response(Response::ListMacros(it)) => Ok(it),
        _ => Err(ServerError { message: "unexpected response".to_owned(), io: None }),
    }
}

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
    seen: Option<Vec<(std::ffi::OsString, std::ffi::OsString)>>,
) -> Result<Result<tt::TopSubtree, String>, crate::ServerError> {
    let version = process.version();
    if let Some(err) = process.exited() {
        // clone: error path, the server's exit told to each caller.
        return Err(err.clone());
    }
    if version >= version::WIRE_TREES {
        let mut sent = Sent::new();
        let task = ExpandMacroWire::of(
            proc_macro,
            subtree,
            attr,
            env,
            [def_site, call_site, mixed_site],
            current_dir,
            &mut sent,
        );
        let sent = RefCell::new(sent);
        // alloc: the request, sent to another process (the edge).
        let task = BidirectionalMessage::Request(Request::ExpandMacroWire(Box::new(task)));
        let answer = process.run_expansion(task, &noting(&sent, callback), seen)?;
        return match answer {
            BidirectionalMessage::Response(Response::ExpandMacroWire(Ok(resp))) => {
                // alloc: none unless the answer's bytes are off a word.
                let tree = read_tree(&resp.tree, &sent.borrow(), &mut Vec::new());
                Ok(Ok(tree.map_err(answer_error)?))
            }
            BidirectionalMessage::Response(Response::ExpandMacroWire(Err(msg))) => Ok(Err(msg.0)),
            // alloc: error path.
            _ => Err(ServerError { message: "unexpected response".to_owned(), io: None }),
        };
    }
    let mut span_data_table = SpanDataIndexMap::default();
    let def_site = span_data_table.insert_full(def_site).0;
    let call_site = span_data_table.insert_full(call_site).0;
    let mixed_site = span_data_table.insert_full(mixed_site).0;
    let task = BidirectionalMessage::Request(Request::ExpandMacro(Box::new(ExpandMacro {
        data: ExpandMacroData {
            macro_body: FlatTree::from_subtree(subtree, version, &mut span_data_table),
            macro_name: proc_macro.name.to_string(),
            attributes: attr
                .map(|subtree| FlatTree::from_subtree(subtree, version, &mut span_data_table)),
            has_global_spans: ExpnGlobals { def_site, call_site, mixed_site },
            span_data_table: if process.rust_analyzer_spans() {
                serialize_span_data_index_map(&span_data_table)
            } else {
                Vec::new()
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
        _ => Err(ServerError { message: "unexpected response".to_owned(), io: None }),
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
        return Err(err.clone());
    }
    srv.run_bidirectional(msg, callback)
}

pub fn reject_subrequests(req: SubRequest) -> Result<SubResponse, ServerError> {
    Err(ServerError { message: format!("{req:?} sub-request not supported here"), io: None })
}

//! The initial proc-macro-srv protocol, soon to be deprecated.

pub mod msg;

use std::{
    io::{BufRead, Write},
    sync::Arc,
};

use paths::AbsPath;
use span::Span;

use crate::{
    ProcMacro, ProcMacroKind, ServerError,
    legacy_protocol::msg::{
        ExpandMacro, ExpandMacroData, ExpnGlobals, FlatTree, Message, Request, Response,
        ServerConfig, SpanDataIndexMap, flat::serialize_span_data_index_map,
    },
    process::Claimed,
    version,
    wire::{ExpandMacroWire, Sent, WireSpan, flat_answer, read_tree, write_tree},
};

pub(crate) use crate::legacy_protocol::msg::SpanMode;

/// Legacy span type, only defined here as it is still used by the proc-macro server.
/// While rust-analyzer doesn't use this anymore at all, RustRover relies on the legacy type for
/// proc-macro expansion.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SpanId(pub u32);

impl std::fmt::Debug for SpanId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

pub(crate) fn version_check(srv: &Claimed<'_>) -> Result<u32, ServerError> {
    let request = Request::ApiVersionCheck {};
    let response = send_task(srv, request)?;

    match response {
        Response::ApiVersionCheck(version) => Ok(version),
        _ => Err(ServerError { message: "unexpected response".to_owned(), io: None }),
    }
}

/// Enable support for rust-analyzer span mode if the server supports it.
pub(crate) fn enable_rust_analyzer_spans(
    srv: &Claimed<'_>,
) -> Result<SpanMode, ServerError> {
    let request = Request::SetConfig(ServerConfig { span_mode: SpanMode::RustAnalyzer });
    let response = send_task(srv, request)?;

    match response {
        Response::SetConfig(ServerConfig { span_mode }) => Ok(span_mode),
        _ => Err(ServerError { message: "unexpected response".to_owned(), io: None }),
    }
}

/// Finds proc-macros in a given dynamic library.
pub(crate) fn find_proc_macros(
    srv: &Claimed<'_>,
    dylib_path: &AbsPath,
) -> Result<Result<Vec<(String, ProcMacroKind)>, String>, ServerError> {
    let request = Request::ListMacros { dylib_path: dylib_path.to_path_buf().into() };

    let response = send_task(srv, request)?;

    match response {
        Response::ListMacros(it) => Ok(it),
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
) -> Result<Result<tt::TopSubtree, String>, crate::ServerError> {
    let version = process.version();
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
        return match send_task(process, Request::ExpandMacroWire(Box::new(task)))? {
            Response::ExpandMacroWire(Ok(tree)) => {
                Ok(Ok(read_tree(&tree, &sent, &mut Vec::new()).map_err(answer_error)?))
            }
            Response::ExpandMacroWire(Err(msg)) => Ok(Err(msg.0)),
            _ => Err(ServerError { message: "unexpected response".to_owned(), io: None }),
        };
    }
    let mut span_data_table = SpanDataIndexMap::default();
    let def_site = span_data_table.insert_full(def_site).0;
    let call_site = span_data_table.insert_full(call_site).0;
    let mixed_site = span_data_table.insert_full(mixed_site).0;
    let task = ExpandMacro {
        data: ExpandMacroData {
            macro_body: FlatTree::from_subtree(subtree, version, &mut span_data_table),
            macro_name: proc_macro.name.to_string(),
            attributes: attr
                .map(|subtree| FlatTree::from_subtree(subtree, version, &mut span_data_table)),
            has_global_spans: ExpnGlobals {
                serialize: version >= version::HAS_GLOBAL_SPANS,
                def_site,
                call_site,
                mixed_site,
            },
            span_data_table: if process.rust_analyzer_spans() {
                serialize_span_data_index_map(&span_data_table)
            } else {
                Vec::new()
            },
        },
        lib: proc_macro.dylib_path.to_path_buf().into(),
        env,
        current_dir: Some(current_dir),
    };

    let response = send_task(process, Request::ExpandMacro(Box::new(task)))?;

    let (tree, answered) = match response {
        Response::ExpandMacro(Ok(tree)) => (tree, None),
        Response::ExpandMacroExtended(Ok(resp)) => (resp.tree, Some(resp.span_data_table)),
        Response::ExpandMacro(Err(msg)) | Response::ExpandMacroExtended(Err(msg)) => {
            return Ok(Err(msg.0));
        }
        _ => return Err(ServerError { message: "unexpected response".to_owned(), io: None }),
    };
    let sent = Sent::of_table(&span_data_table);
    let mut expanded = flat_answer(tree, version, &span_data_table, answered.as_deref(), &sent)
        .map_err(answer_error)?;
    if proc_macro.needs_fixup_change() {
        proc_macro.change_fixup_to_match_old_server(&mut expanded);
    }
    Ok(Ok(expanded))
}

/// A server's answer that is no tree, or names what the request did not send: the server's
/// fault, never the macro's.
pub(crate) fn answer_error(why: String) -> ServerError {
    ServerError { message: format!("the proc-macro server's answer: {why}"), io: None }
}

impl ExpandMacroWire {
    /// The request of an expansion of `proc_macro`, noting in `sent` each span part it sends.
    pub(crate) fn of(
        proc_macro: &ProcMacro,
        subtree: tt::SubtreeView<'_>,
        attr: Option<tt::SubtreeView<'_>>,
        env: Vec<(String, String)>,
        [def_site, call_site, mixed_site]: [Span; 3],
        current_dir: String,
        sent: &mut Sent,
    ) -> ExpandMacroWire {
        for site in [def_site, call_site, mixed_site] {
            sent.span(&site);
        }
        let has_attributes = attr.is_some();
        let mut trees = Vec::new();
        write_tree(subtree, sent, &mut trees);
        if let Some(attr) = attr {
            write_tree(attr, sent, &mut trees);
        }
        ExpandMacroWire {
            lib: proc_macro.dylib_path.to_path_buf().into(),
            env,
            current_dir: Some(current_dir),
            macro_name: proc_macro.name.to_string(),
            trees,
            has_attributes,
            def_site: WireSpan::of(def_site),
            call_site: WireSpan::of(call_site),
            mixed_site: WireSpan::of(mixed_site),
        }
    }
}

/// Sends a request to the proc-macro server and waits for a response.
fn send_task(srv: &Claimed<'_>, req: Request) -> Result<Response, ServerError> {
    if let Some(server_error) = srv.exited() {
        return Err(server_error.clone());
    }

    srv.send_task_legacy::<_, _>(send_request, req)
}

/// Sends a request to the server and reads the response.
fn send_request(
    mut writer: &mut dyn Write,
    mut reader: &mut dyn BufRead,
    req: Request,
    buf: &mut String,
) -> Result<Option<Response>, ServerError> {
    req.write(&mut writer).map_err(|err| ServerError {
        message: "failed to write request".into(),
        io: Some(Arc::new(err)),
    })?;
    let res = Response::read(&mut reader, buf).map_err(|err| ServerError {
        message: "failed to read response".into(),
        io: Some(Arc::new(err)),
    })?;
    Ok(res)
}

//! Handle process life-time and message passing for proc-macro client

use std::{
    cell::RefCell,
    fmt::Debug,
    io::{self, BufRead, BufReader, Read, Write},
    panic::AssertUnwindSafe,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{Arc, Mutex, OnceLock},
};

use paths::AbsPath;
use semver::Version;
use span::Span;
use stdx::JodChild;

use crate::{
    ProcMacro, ProcMacroKind, ProtocolFormat, ServerError,
    bidirectional_protocol::{
        self, SubCallback, answer, closed,
        msg::{BidirectionalMessage, SubRequest, SubResponse},
        reject_subrequests, unexpected, wrap_decode, wrap_encode, wrap_io,
    },
    legacy_protocol::{self, SpanMode, answer_error},
    transport::postcard,
    version,
    warm::{self, Environment, LinkExit, LinkReader, LinkWriter, Tracer, Warm},
    wire::{Heard, HeardResponse, Sent, WireAsk, WireSpan, read_tree, write_tree},
};

/// Represents a process handling proc-macro communication.
pub(crate) struct ProcMacroServerProcess {
    /// The connection's state while no request runs on it. Its protocol is one request at a
    /// time, so a request takes the state for itself ([`ProcMacroServerProcess::claim`]) and
    /// no lock is held while it runs: a sub-request it answers from the database may expand
    /// another macro, which takes another connection ([`crate::pool`]).
    state: Parked<ProcessSrvState>,
    version: u32,
    protocol: Protocol,
    /// Populated when the server exits.
    exited: OnceLock<AssertUnwindSafe<ServerError>>,
    /// Of a warm server: what the shell passes through, sent with each expansion.
    environment: Option<Arc<Environment>>,
}

/// A value its takers use one at a time, held by none of them while they use it: the lock is
/// held only to move the value out or back, never across a call, so it is never waited on for
/// longer than a move and a thread can't wait on itself.
struct Parked<T>(Mutex<Option<T>>);

impl<T> Parked<T> {
    fn new(value: T) -> Parked<T> {
        Parked(Mutex::new(Some(value)))
    }

    fn take(&self) -> Option<T> {
        self.0.lock().unwrap_or_else(|it| it.into_inner()).take()
    }

    fn give_back(&self, value: T) {
        *self.0.lock().unwrap_or_else(|it| it.into_inner()) = Some(value);
    }
}

/// A server's connection, the caller's for its requests until dropped, when it is given back.
/// Not `Sync`: it is the one caller's.
pub(crate) struct Claimed<'a> {
    process: &'a ProcMacroServerProcess,
    state: RefCell<Option<ProcessSrvState>>,
}

impl Drop for Claimed<'_> {
    fn drop(&mut self) {
        if let Some(state) = self.state.get_mut().take() {
            self.process.state.give_back(state);
        }
    }
}

impl std::fmt::Debug for ProcMacroServerProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcMacroServerProcess")
            .field("version", &self.version)
            .field("protocol", &self.protocol)
            .field("exited", &self.exited)
            .finish()
    }
}

#[derive(Debug, Clone)]
pub(crate) enum Protocol {
    LegacyJson { mode: SpanMode },
    BidirectionalPostcardPrototype { mode: SpanMode },
}

impl From<ProtocolFormat> for Protocol {
    fn from(format: ProtocolFormat) -> Protocol {
        match format {
            ProtocolFormat::JsonLegacy => Protocol::LegacyJson { mode: SpanMode::Id },
            ProtocolFormat::BidirectionalPostcardPrototype => {
                Protocol::BidirectionalPostcardPrototype { mode: SpanMode::Id }
            }
        }
    }
}

/// What tells a connection its server ended, by the way blaze reaches it.
pub(crate) enum Exit {
    /// A server process blaze started: its exit status.
    Child(Process),
    /// A warm server: its link, which the driver told.
    Warm(LinkExit),
}

impl Exit {
    fn exit_err(&mut self) -> Option<ServerError> {
        match self {
            Exit::Child(process) => process.exit_err(),
            Exit::Warm(link) => link.exit_err(),
        }
    }
}

/// The end of a connection a request is written to.
pub(crate) enum Writer {
    /// A server process's stdin.
    Child(ChildStdin),
    /// A warm server's link.
    Warm(LinkWriter),
}

impl Write for Writer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Writer::Child(it) => it.write(bytes),
            Writer::Warm(it) => it.write(bytes),
        }
    }

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        match self {
            Writer::Child(it) => it.write_all(bytes),
            Writer::Warm(it) => it.write_all(bytes),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Writer::Child(it) => it.flush(),
            Writer::Warm(it) => it.flush(),
        }
    }
}

/// The end of a connection an answer is read from.
pub(crate) enum Reader {
    /// A server process's stdout.
    Child(BufReader<ChildStdout>),
    /// A warm server's link.
    Warm(LinkReader),
}

impl Read for Reader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        match self {
            Reader::Child(it) => it.read(out),
            Reader::Warm(it) => it.read(out),
        }
    }
}

impl BufRead for Reader {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        match self {
            Reader::Child(it) => it.fill_buf(),
            Reader::Warm(it) => it.fill_buf(),
        }
    }

    fn consume(&mut self, n: usize) {
        match self {
            Reader::Child(it) => it.consume(n),
            Reader::Warm(it) => it.consume(n),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        bidirectional_protocol::msg::Response,
        legacy_protocol::msg::{Message as _, ServerConfig},
    };
    use crate::pool::ProcMacroServerPool;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A connection to a server that plays `answers` and takes what it is sent.
    fn scripted(answers: Vec<u8>) -> (Exit, Writer, Reader) {
        let link = Arc::new(warm::Link::Script(warm::Script {
            sent: Mutex::default(),
            answers: Mutex::new([answers].into()),
            ended: Mutex::default(),
        }));
        let (exit, writer) = (LinkExit(link.clone()), LinkWriter::new(link.clone()));
        (Exit::Warm(exit), Writer::Warm(writer), Reader::Warm(LinkReader::new(link)))
    }

    fn connect(responses: Vec<Response>) -> io::Result<ProcMacroServerProcess> {
        let mut bytes = Vec::new();
        for response in responses {
            BidirectionalMessage::Response(response).write(&mut bytes).unwrap();
        }
        let attempts = AtomicUsize::new(0);
        let result = ProcMacroServerProcess::run(
            |format| {
                assert_eq!(attempts.fetch_add(1, Ordering::Relaxed), 0, "no protocol retry");
                assert_eq!(format, Some(ProtocolFormat::BidirectionalPostcardPrototype));
                Ok(scripted(bytes.clone()))
            },
            None,
            || "test server".to_owned(),
        );
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
        result
    }

    /// Writes a frame on `writer` and reads one back on `reader`: the server's answer.
    fn round_trip(writer: &mut Writer, reader: &mut Reader) -> Option<BidirectionalMessage> {
        BidirectionalMessage::Response(Response::ApiVersionCheck(7)).write(writer).unwrap();
        writer.flush().unwrap();
        BidirectionalMessage::read(reader, &mut Vec::new()).unwrap()
    }

    /// Each way a connection reaches its server writes, reads and tells when it ended: a
    /// process's pipes (`cat`, which answers what it is sent) and a warm server's link.
    #[cfg(unix)]
    #[test]
    fn each_way_to_a_server_writes_reads_and_tells_its_end() {
        let answered = |it: Option<BidirectionalMessage>| {
            matches!(it, Some(BidirectionalMessage::Response(Response::ApiVersionCheck(7))))
        };

        #[allow(clippy::disallowed_methods)]
        let mut child = Command::new("cat").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
        let mut writer = Writer::Child(child.stdin.take().unwrap());
        let mut reader = Reader::Child(BufReader::new(child.stdout.take().unwrap()));
        let mut exit = Exit::Child(Process { child: JodChild(child) });
        assert!(answered(round_trip(&mut writer, &mut reader)));
        assert!(exit.exit_err().is_none());
        if let Exit::Child(process) = &mut exit {
            process.child.kill().unwrap();
            process.child.wait().unwrap();
        }
        assert!(exit.exit_err().unwrap().message.contains("exited"));

        let mut frame = Vec::new();
        BidirectionalMessage::Response(Response::ApiVersionCheck(7)).write(&mut frame).unwrap();
        let (mut exit, mut writer, mut reader) = scripted(frame.clone());
        assert!(answered(round_trip(&mut writer, &mut reader)));
        assert!(exit.exit_err().is_none());
        let link = match &exit {
            Exit::Warm(LinkExit(link)) => link.clone(),
            Exit::Child(_) => panic!("a warm server's exit"),
        };
        let warm::Link::Script(script) = &*link else { panic!("a script") };
        assert_eq!(*script.sent.lock().unwrap(), frame);
        *script.ended.lock().unwrap() = Some("it exited with 3".to_owned());
        assert!(exit.exit_err().unwrap().message.contains("exited with 3"));
    }

    #[test]
    fn a_native_server_uses_source_spans_without_a_version_hint_or_an_environment_opt_in() {
        let srv = connect(vec![
            Response::ApiVersionCheck(version::CURRENT_API_VERSION),
            Response::SetConfig(ServerConfig { span_mode: SpanMode::RustAnalyzer }),
        ])
        .unwrap();
        assert!(srv.rust_analyzer_spans());
        assert!(matches!(srv.protocol, Protocol::BidirectionalPostcardPrototype { .. }));
    }

    #[test]
    fn failed_handshakes_are_not_retried_in_legacy_json() {
        assert!(connect(vec![]).unwrap_err().to_string().contains("version check failed"));
        assert!(
            connect(vec![Response::ApiVersionCheck(version::CURRENT_API_VERSION + 1)])
                .unwrap_err()
                .to_string()
                .contains("too new")
        );
    }

    fn warm_server(version: u32) -> io::Result<ProcMacroServerProcess> {
        let mut bytes = Vec::new();
        for response in [
            Response::ApiVersionCheck(version),
            Response::SetConfig(ServerConfig { span_mode: SpanMode::RustAnalyzer }),
        ] {
            BidirectionalMessage::Response(response).write(&mut bytes).unwrap();
        }
        let link = Arc::new(warm::Link::Script(warm::Script {
            sent: Mutex::default(),
            answers: Mutex::new([bytes].into()),
            ended: Mutex::default(),
        }));
        ProcMacroServerProcess::warm(Warm { link, traced: None }, Environment::new())
    }

    #[test]
    fn a_warm_server_must_mark_the_scope_of_each_expansion() {
        let refused = warm_server(version::HASHED_AST_ID).unwrap_err().to_string();
        assert!(refused.contains("scope"), "{refused}");
        assert!(warm_server(version::SCOPED_EXPANSIONS).unwrap().rust_analyzer_spans());
    }

    fn handshake() -> io::Result<ProcMacroServerProcess> {
        connect(vec![
            Response::ApiVersionCheck(version::CURRENT_API_VERSION),
            Response::SetConfig(ServerConfig { span_mode: SpanMode::RustAnalyzer }),
        ])
    }

    #[test]
    fn a_claim_while_every_connection_is_claimed_opens_another_and_a_given_back_one_is_reused() {
        let opened = Arc::new(AtomicUsize::new(0));
        let open = {
            let opened = opened.clone();
            move || {
                opened.fetch_add(1, Ordering::Relaxed);
                handshake()
            }
        };
        let pool = ProcMacroServerPool::new(vec![open().unwrap()], Box::new(open)).unwrap();
        let first = pool.claim().unwrap();
        assert!(first.rust_analyzer_spans());
        // As an expansion asked for while this thread answers the first's sub-request would.
        let second = pool.claim().unwrap();
        assert_eq!(opened.load(Ordering::Relaxed), 2);
        drop(second);
        let again = pool.claim().unwrap();
        assert_eq!(opened.load(Ordering::Relaxed), 2);
        drop((first, again));
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| drop(pool.claim().unwrap()));
            }
        });
        // Every claim was given back: the next one opens nothing.
        let before = opened.load(Ordering::Relaxed);
        drop(pool.claim().unwrap());
        assert_eq!(opened.load(Ordering::Relaxed), before);
        assert!(pool.exited().is_none());
    }

    #[test]
    fn a_server_that_cannot_enable_source_spans_is_refused() {
        for config in [None, Some(ServerConfig { span_mode: SpanMode::Id })] {
            let mut responses = vec![Response::ApiVersionCheck(version::CURRENT_API_VERSION)];
            responses.extend(config.map(Response::SetConfig));
            assert!(connect(responses).is_err());
        }
    }
}

impl Process {
    fn exit_err(&mut self) -> Option<ServerError> {
        match self.child.try_wait() {
            Ok(None) | Err(_) => None,
            Ok(Some(status)) => {
                // alloc: error path, what the server said as it exited.
                let mut msg = String::new();
                if !status.success()
                    && let Some(stderr) = self.child.stderr.as_mut()
                {
                    _ = stderr.read_to_string(&mut msg);
                }
                Some(ServerError {
                    // alloc: error path, as above.
                    message: format!(
                        "proc-macro server exited with {status}{}{msg}",
                        if msg.is_empty() { "" } else { ": " }
                    ),
                    io: None,
                })
            }
        }
    }
}

/// Maintains the state of the proc-macro server process.
pub(crate) struct ProcessSrvState {
    process: Exit,
    stdin: Writer,
    stdout: Reader,
    /// Of a warm server in a sandbox that traces: its live record, read by scope.
    tracer: Option<Tracer>,
    scratch: Scratch,
}

/// What each request of a connection reuses, each as long as the longest it held: once they
/// are, a request allocates none of them.
#[derive(Default)]
struct Scratch {
    /// A frame written or read whole, the one in flight.
    frame: Vec<u8>,
    /// A request's trees, as `write_wire` writes them.
    trees: Vec<u8>,
    /// An answer's tree when it doesn't start at a word, which `read_wire` asks for.
    aligned: Vec<u8>,
    /// What a request sent of span parts, which its answer may use.
    sent: Sent,
}

impl ProcMacroServerProcess {
    /// Starts the proc-macro server and performs a version check
    pub(crate) fn spawn<'a>(
        process_path: &AbsPath,
        env: impl IntoIterator<
            Item = (impl AsRef<std::ffi::OsStr>, &'a Option<impl 'a + AsRef<std::ffi::OsStr>>),
        > + Clone,
        version: Option<&Version>,
    ) -> io::Result<ProcMacroServerProcess> {
        Self::run(
            |format| {
                let mut process = Process::run(
                    process_path,
                    env.clone(), // clone: the variables' iterator, once per server started.
                    // alloc: the format's name, once per server started.
                    format.map(|format| format.to_string()).as_deref(),
                )?;
                let (stdin, stdout) = process.stdio().expect("couldn't access child stdio");

                Ok((Exit::Child(process), Writer::Child(stdin), Reader::Child(stdout)))
            },
            version,
            || {
                #[expect(clippy::disallowed_methods)]
                Command::new(process_path)
                    .arg("--version")
                    .output()
                    // alloc: error path, the server's version named in a refusal.
                    .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
                    // alloc: as above.
                    .unwrap_or_else(|_| "unknown version".to_owned())
            },
        )
    }

    /// Invokes `spawn` and performs a version check.
    pub(crate) fn run(
        spawn: impl Fn(Option<ProtocolFormat>) -> io::Result<(Exit, Writer, Reader)>,
        _version: Option<&Version>,
        binary_server_version: impl Fn() -> String,
    ) -> io::Result<ProcMacroServerProcess> {
        // Blaze owns the pinned server. Source queries need its bidirectional protocol:
        // JSON cannot ask for a span's text, file or position and would return placeholders.
        let format = ProtocolFormat::BidirectionalPostcardPrototype;
        let (process, stdin, stdout) = spawn(Some(format))?;
        let state =
            ProcessSrvState { process, stdin, stdout, tracer: None, scratch: Scratch::default() };
        Self::start(state, version::RUST_ANALYZER_SPAN_SUPPORT, None, binary_server_version)
    }

    /// Talks to the warm server `warm`, which marks the scope of each expansion: the passed
    /// through variables of `environment` go with each.
    pub(crate) fn warm(
        warm: Warm,
        environment: Arc<Environment>,
    ) -> io::Result<ProcMacroServerProcess> {
        let Warm { link, traced } = warm;
        let state = ProcessSrvState {
            process: Exit::Warm(LinkExit(Arc::clone(&link))),
            stdin: Writer::Warm(LinkWriter::new(Arc::clone(&link))),
            stdout: Reader::Warm(LinkReader::new(link)),
            tracer: traced.map(|(live, socket)| Tracer::new(live, socket)),
            scratch: Scratch::default(),
        };
        Self::start(state, version::SCOPED_EXPANSIONS, Some(environment), || {
            // alloc: error path, the server named in a refusal.
            "a warm server".to_owned()
        })
    }

    /// Checks the server's version, which must be `least` or newer, and turns on source spans.
    fn start(
        state: ProcessSrvState,
        least: u32,
        environment: Option<Arc<Environment>>,
        binary_server_version: impl Fn() -> String,
    ) -> io::Result<ProcMacroServerProcess> {
        let format = ProtocolFormat::BidirectionalPostcardPrototype;
        let mut srv = ProcMacroServerProcess {
            state: Parked::new(state),
            version: 0,
            protocol: Protocol::from(format),
            exited: OnceLock::new(),
            environment,
        };
        fn claim(srv: &ProcMacroServerProcess) -> Claimed<'_> {
            srv.claim().expect("a connection no one else has yet is free")
        }
        let v = claim(&srv).version_check(Some(&reject_subrequests)).map_err(|e| {
            // alloc: error path, why the handshake failed.
            io::Error::other(format!("proc-macro server version check failed: {e}"))
        })?;
        if v > version::CURRENT_API_VERSION {
            // alloc: error path, as above.
            return Err(io::Error::other(format!(
                "proc-macro server is too new for blaze: API {v}, supported API {}, server {}",
                version::CURRENT_API_VERSION,
                binary_server_version()
            )));
        }
        if v < version::RUST_ANALYZER_SPAN_SUPPORT {
            // alloc: error path, as above.
            return Err(io::Error::other(format!(
                "proc-macro server API {v} does not support source spans"
            )));
        }
        if v < least {
            // alloc: error path, as above.
            return Err(io::Error::other(format!(
                "proc-macro server API {v} does not mark the scope of each expansion, which a \
                 warm server must (API {least}): what an expansion read can't be told apart, so \
                 it can't be kept; server {}",
                binary_server_version()
            )));
        }
        srv.version = v;
        let new_mode = claim(&srv)
            .enable_rust_analyzer_spans(Some(&reject_subrequests))
            // alloc: error path, as above.
            .map_err(|e| io::Error::other(format!("proc-macro source-span setup failed: {e}")))?;
        if new_mode != SpanMode::RustAnalyzer {
            return Err(io::Error::other("proc-macro server did not enable source spans"));
        }
        match &mut srv.protocol {
            Protocol::LegacyJson { mode } | Protocol::BidirectionalPostcardPrototype { mode } => {
                *mode = new_mode;
            }
        }
        tracing::info!("Proc-macro server API {v}, protocol: {:?}", srv.protocol);
        Ok(srv)
    }

    /// The connection, the caller's until the claim is dropped: `None` while another request
    /// runs on it.
    pub(crate) fn claim(&self) -> Option<Claimed<'_>> {
        let state = self.state.take()?;
        Some(Claimed { process: self, state: RefCell::new(Some(state)) })
    }

    /// Returns the server error if the process has exited.
    pub(crate) fn exited(&self) -> Option<&ServerError> {
        self.exited.get().map(|it| &it.0)
    }

    /// Retrieves the API version of the proc-macro server.
    pub(crate) fn version(&self) -> u32 {
        self.version
    }

    /// Enable support for rust-analyzer span mode if the server supports it.
    pub(crate) fn rust_analyzer_spans(&self) -> bool {
        match self.protocol {
            Protocol::LegacyJson { mode } | Protocol::BidirectionalPostcardPrototype { mode } => {
                mode == SpanMode::RustAnalyzer
            }
        }
    }
}

impl Claimed<'_> {
    pub(crate) fn exited(&self) -> Option<&ServerError> {
        self.process.exited()
    }

    pub(crate) fn version(&self) -> u32 {
        self.process.version()
    }

    pub(crate) fn rust_analyzer_spans(&self) -> bool {
        self.process.rust_analyzer_spans()
    }

    /// Finds proc-macros in a given dynamic library.
    pub(crate) fn find_proc_macros(
        &self,
        dylib_path: &AbsPath,
    ) -> Result<Result<Vec<(String, ProcMacroKind)>, String>, ServerError> {
        match self.process.protocol {
            Protocol::LegacyJson { .. } => legacy_protocol::find_proc_macros(self, dylib_path),

            Protocol::BidirectionalPostcardPrototype { .. } => {
                bidirectional_protocol::find_proc_macros(self, dylib_path, &|_| {
                    Ok(SubResponse::Cancel {
                        // alloc: error path, a sub-request refused.
                        reason: String::from(
                            "Server should not do a sub request when loading proc-macros",
                        ),
                    })
                })
            }
        }
    }

    /// Checks the API version of the running proc-macro server.
    fn version_check(&self, callback: Option<SubCallback<'_>>) -> Result<u32, ServerError> {
        match self.process.protocol {
            Protocol::LegacyJson { .. } => legacy_protocol::version_check(self),
            Protocol::BidirectionalPostcardPrototype { .. } => {
                let cb = callback.expect("callback required for bidirectional protocol");
                bidirectional_protocol::version_check(self, cb)
            }
        }
    }

    /// Enable support for rust-analyzer span mode if the server supports it.
    fn enable_rust_analyzer_spans(
        &self,
        callback: Option<SubCallback<'_>>,
    ) -> Result<SpanMode, ServerError> {
        match self.process.protocol {
            Protocol::LegacyJson { .. } => legacy_protocol::enable_rust_analyzer_spans(self),
            Protocol::BidirectionalPostcardPrototype { .. } => {
                let cb = callback.expect("callback required for bidirectional protocol");
                bidirectional_protocol::enable_rust_analyzer_spans(self, cb)
            }
        }
    }

    pub(crate) fn expand(
        &self,
        proc_macro: &ProcMacro,
        subtree: tt::SubtreeView<'_>,
        attr: Option<tt::SubtreeView<'_>>,
        mut env: Vec<(String, String)>,
        def_site: Span,
        call_site: Span,
        mixed_site: Span,
        current_dir: String,
        callback: Option<SubCallback<'_>>,
    ) -> Result<Result<tt::TopSubtree, String>, ServerError> {
        warm::set_traced(None);
        let passed = self.process.environment.as_deref().map_or(&[][..], Environment::passed);
        let bidirectional =
            matches!(self.process.protocol, Protocol::BidirectionalPostcardPrototype { .. });
        if bidirectional && self.version() >= version::WIRE_TREES {
            return self.expand_wire(
                proc_macro,
                subtree,
                attr,
                [passed, &env],
                [def_site, call_site, mixed_site],
                &current_dir,
                callback.expect("callback required for bidirectional protocol"),
            );
        }
        let sent = match &self.process.environment {
            Some(_) => Environment::sent(passed, &env),
            None => std::mem::take(&mut env),
        };
        match self.process.protocol {
            Protocol::LegacyJson { .. } => legacy_protocol::expand(
                proc_macro,
                self,
                subtree,
                attr,
                sent,
                def_site,
                call_site,
                mixed_site,
                current_dir,
            ),
            Protocol::BidirectionalPostcardPrototype { .. } => bidirectional_protocol::expand(
                proc_macro,
                self,
                subtree,
                attr,
                sent,
                def_site,
                call_site,
                mixed_site,
                current_dir,
                callback.expect("callback required for bidirectional protocol"),
                [passed, &env],
            ),
        }
    }

    /// An expansion by a server of [`version::WIRE_TREES`] over the connection's [`Scratch`]:
    /// the request is written from what the caller holds, sub-requests are answered between,
    /// and the answer's tree is read where it lies in its frame. Once the buffers are as long
    /// as an expansion needs, it allocates only the tree it returns, what `callback` does, and
    /// what the connection's pipe does per frame. Its environment is `passed`, then `call`.
    fn expand_wire(
        &self,
        proc_macro: &ProcMacro,
        subtree: tt::SubtreeView<'_>,
        attr: Option<tt::SubtreeView<'_>>,
        env @ [passed, call]: [&[(String, String)]; 2],
        sites: [Span; 3],
        current_dir: &str,
        callback: SubCallback<'_>,
    ) -> Result<Result<tt::TopSubtree, String>, ServerError> {
        if let Some(err) = self.exited() {
            // clone: error path, the server's exit told to each caller.
            return Err(err.clone());
        }
        let scope = self.scope();
        let answered = self.with_state(|state| {
            let ProcessSrvState { stdin, stdout, scratch, .. } = state;
            let Scratch { frame, trees, aligned, sent } = scratch;
            sent.clear();
            trees.clear();
            for site in &sites {
                sent.span(site);
            }
            write_tree(subtree, sent, trees);
            if let Some(attr) = attr {
                write_tree(attr, sent, trees);
            }
            let ask = WireAsk {
                lib: proc_macro.dylib_path.as_str(),
                passed,
                call,
                current_dir,
                macro_name: &proc_macro.name,
                trees,
                has_attributes: attr.is_some(),
                sites: sites.map(WireSpan::of),
            };
            postcard::encode_into(&ask.message(), frame).map_err(wrap_encode)?;
            postcard::write(stdin, frame).map_err(wrap_io("failed to write initial request"))?;
            loop {
                let Some(read) =
                    postcard::read(stdout, frame).map_err(wrap_io("failed to read message"))?
                else {
                    return Err(closed());
                };
                let len = postcard::unframe(read).map_err(wrap_decode)?;
                let request = match postcard::parse::<Heard<'_>>(&frame[..len]) {
                    Ok(Heard::Response(HeardResponse::ExpandMacroWire(Ok(reply)))) => {
                        let tree = read_tree(reply.tree, sent, aligned).map_err(answer_error)?;
                        return Ok(Ok(tree));
                    }
                    Ok(Heard::Response(HeardResponse::ExpandMacroWire(Err(panic)))) => {
                        return Ok(Err(panic.0));
                    }
                    Ok(Heard::SubRequest(request)) => request,
                    Ok(other) => return Err(unexpected(&other)),
                    Err(err) => return Err(wrap_decode(err)),
                };
                let joined_in = match &request {
                    SubRequest::SpanJoin { file_id, .. } => Some(*file_id),
                    _ => None,
                };
                let response = answer(callback, request);
                sent.sub_response(joined_in, &response);
                let response = BidirectionalMessage::SubResponse(response);
                postcard::encode_into(&response, frame).map_err(wrap_encode)?;
                postcard::write(stdin, frame).map_err(wrap_io("failed to write sub-response"))?;
            }
        });
        if answered.is_ok() {
            self.trace(scope, env);
        }
        answered
    }

    pub(crate) fn send_task_legacy<Request, Response>(
        &self,
        send: impl FnOnce(
            &mut Writer,
            &mut Reader,
            Request,
            &mut String,
        ) -> Result<Option<Response>, ServerError>,
        req: Request,
    ) -> Result<Response, ServerError> {
        // alloc: an older server's JSON line, once per request; the connection speaks postcard.
        let mut buf = String::new();
        self.with_state(|state| {
            send(&mut state.stdin, &mut state.stdout, req, &mut buf).and_then(|res| {
                res.ok_or_else(|| {
                    // alloc: error path.
                    let message = "proc-macro server did not respond with data".to_owned();
                    ServerError {
                        // alloc: error path, as above.
                        io: Some(Arc::new(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            message.clone(), // clone: error path, as above.
                        ))),
                        message,
                    }
                })
            })
        })
    }

    /// Runs `f` on the connection's state. The state is borrowed from the claim alone, so the
    /// sub-requests `f` answers lock nothing.
    fn with_state<R>(
        &self,
        f: impl FnOnce(&mut ProcessSrvState) -> Result<R, ServerError>,
    ) -> Result<R, ServerError> {
        let mut state = self.state.borrow_mut();
        let state = state.as_mut().expect("a claim holds its connection's state until dropped");
        f(&mut *state).map_err(|e| {
            if e.io.as_ref().map(|it| it.kind()) == Some(io::ErrorKind::BrokenPipe) {
                match state.process.exit_err() {
                    None => e,
                    Some(server_error) => {
                        let exited =
                            self.process.exited.get_or_init(|| AssertUnwindSafe(server_error));
                        // clone: error path, the server's exit told to each caller.
                        exited.0.clone()
                    }
                }
            } else {
                e
            }
        })
    }

    pub(crate) fn run_bidirectional(
        &self,
        initial: BidirectionalMessage,
        callback: SubCallback<'_>,
    ) -> Result<BidirectionalMessage, ServerError> {
        self.with_state(|state| {
            let ProcessSrvState { stdin, stdout, scratch, .. } = state;
            bidirectional_protocol::run_conversation(
                stdin,
                stdout,
                &mut scratch.frame,
                initial,
                callback,
            )
        })
    }

    /// Runs the conversation of one expansion by a server older than [`version::WIRE_TREES`].
    /// Of a traced warm server, it is the next scope of the connection, and what it read is
    /// this thread's to take once the server answered ([`warm::take_traced`]): it saw the
    /// variables `env`, what the shell passed and then the crate's.
    pub(crate) fn run_expansion(
        &self,
        initial: BidirectionalMessage,
        callback: SubCallback<'_>,
        env: [&[(String, String)]; 2],
    ) -> Result<BidirectionalMessage, ServerError> {
        let scope = self.scope();
        let answered = self.run_bidirectional(initial, callback);
        if answered.is_ok() {
            self.trace(scope, env);
        }
        answered
    }

    /// The scope the connection's next expansion runs in, if its server is traced.
    fn scope(&self) -> Option<u64> {
        let mut state = self.state.borrow_mut();
        Some(state.as_mut()?.tracer.as_mut()?.scope())
    }

    /// Makes what the expansion in `scope` read this thread's to take: it saw the variables
    /// `env`, what the shell passed and then the crate's.
    fn trace(&self, scope: Option<u64>, env: [&[(String, String)]; 2]) {
        let Some(scope) = scope else { return };
        let mut state = self.state.borrow_mut();
        if let Some(tracer) = state.as_mut().and_then(|state| state.tracer.as_mut()) {
            warm::set_traced(Some(tracer.traced(scope, env)));
        }
    }
}

/// Manages the execution of the proc-macro server process.
#[derive(Debug)]
pub(crate) struct Process {
    child: JodChild,
}

impl Process {
    /// Runs a new proc-macro server process with the specified environment variables.
    fn run<'a>(
        path: &AbsPath,
        env: impl IntoIterator<
            Item = (impl AsRef<std::ffi::OsStr>, &'a Option<impl 'a + AsRef<std::ffi::OsStr>>),
        >,
        format: Option<&str>,
    ) -> io::Result<Process> {
        let child = JodChild(mk_child(path, env, format)?);
        Ok(Process { child })
    }

    /// Retrieves stdin and stdout handles for the process.
    fn stdio(&mut self) -> Option<(ChildStdin, BufReader<ChildStdout>)> {
        let stdin = self.child.stdin.take()?;
        let stdout = self.child.stdout.take()?;
        let read = BufReader::new(stdout);

        Some((stdin, read))
    }
}

/// Creates and configures a new child process for the proc-macro server.
fn mk_child<'a>(
    path: &AbsPath,
    extra_env: impl IntoIterator<
        Item = (impl AsRef<std::ffi::OsStr>, &'a Option<impl 'a + AsRef<std::ffi::OsStr>>),
    >,
    format: Option<&str>,
) -> io::Result<Child> {
    #[allow(clippy::disallowed_methods)]
    let mut cmd = Command::new(path);
    for env in extra_env {
        match env {
            (key, Some(val)) => cmd.env(key, val),
            (key, None) => cmd.env_remove(key),
        };
    }
    if let Some(format) = format {
        cmd.arg("--format");
        cmd.arg(format);
    }
    cmd.env("RUST_ANALYZER_INTERNALS_DO_NOT_USE", "this is unstable");
    let mut cmd = crate::wrap::wrapped(cmd)?;
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit());
    if cfg!(windows) {
        let mut path_var = std::ffi::OsString::new();
        path_var.push(path.parent().unwrap().parent().unwrap());
        path_var.push("\\bin;");
        path_var.push(std::env::var_os("PATH").unwrap_or_default());
        cmd.env("PATH", path_var);
    }
    cmd.spawn()
}

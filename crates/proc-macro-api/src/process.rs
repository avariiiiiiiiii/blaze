//! Handle process life-time and message passing for proc-macro client

use std::{
    cell::RefCell,
    ffi::OsString,
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
        self, SubCallback,
        msg::{BidirectionalMessage, SubResponse},
        reject_subrequests,
    },
    legacy_protocol::{self, SpanMode},
    version,
    warm::{self, Environment, LinkExit, LinkReader, LinkWriter, Tracer, Warm},
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

pub trait ProcessExit: Send + Sync {
    fn exit_err(&mut self) -> Option<ServerError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        bidirectional_protocol::msg::Response,
        legacy_protocol::msg::{Message as _, ServerConfig},
    };
    use crate::pool::ProcMacroServerPool;
    use std::{
        io::Cursor,
        sync::atomic::{AtomicUsize, Ordering},
    };

    struct Running;
    impl ProcessExit for Running {
        fn exit_err(&mut self) -> Option<ServerError> {
            None
        }
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
                Ok((Box::new(Running), Box::new(io::sink()), Box::new(Cursor::new(bytes.clone()))))
            },
            None,
            || "test server".to_owned(),
        );
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
        result
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
        let link = Arc::new(warm::Script {
            sent: Mutex::default(),
            answers: Mutex::new([bytes].into()),
            ended: Mutex::default(),
        });
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

impl ProcessExit for Process {
    fn exit_err(&mut self) -> Option<ServerError> {
        match self.child.try_wait() {
            Ok(None) | Err(_) => None,
            Ok(Some(status)) => {
                let mut msg = String::new();
                if !status.success()
                    && let Some(stderr) = self.child.stderr.as_mut()
                {
                    _ = stderr.read_to_string(&mut msg);
                }
                Some(ServerError {
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
    process: Box<dyn ProcessExit>,
    stdin: Box<dyn Write + Send + Sync>,
    stdout: Box<dyn BufRead + Send + Sync>,
    /// Of a warm server in a sandbox that traces: its live record, read by scope.
    tracer: Option<Tracer>,
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
                    env.clone(),
                    format.map(|format| format.to_string()).as_deref(),
                )?;
                let (stdin, stdout) = process.stdio().expect("couldn't access child stdio");

                Ok((Box::new(process), Box::new(stdin), Box::new(stdout)))
            },
            version,
            || {
                #[expect(clippy::disallowed_methods)]
                Command::new(process_path)
                    .arg("--version")
                    .output()
                    .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
                    .unwrap_or_else(|_| "unknown version".to_owned())
            },
        )
    }

    /// Invokes `spawn` and performs a version check.
    pub(crate) fn run(
        spawn: impl Fn(
            Option<ProtocolFormat>,
        ) -> io::Result<(
            Box<dyn ProcessExit>,
            Box<dyn Write + Send + Sync>,
            Box<dyn BufRead + Send + Sync>,
        )>,
        _version: Option<&Version>,
        binary_server_version: impl Fn() -> String,
    ) -> io::Result<ProcMacroServerProcess> {
        // Blaze owns the pinned server. Source queries need its bidirectional protocol:
        // JSON cannot ask for a span's text, file or position and would return placeholders.
        let format = ProtocolFormat::BidirectionalPostcardPrototype;
        let (process, stdin, stdout) = spawn(Some(format))?;
        let state = ProcessSrvState { process, stdin, stdout, tracer: None };
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
            process: Box::new(LinkExit(link.clone())),
            stdin: Box::new(LinkWriter::new(link.clone())),
            stdout: Box::new(LinkReader::new(link)),
            tracer: traced.map(|(live, socket)| Tracer::new(live, socket)),
        };
        Self::start(state, version::SCOPED_EXPANSIONS, Some(environment), || {
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
            io::Error::other(format!("proc-macro server version check failed: {e}"))
        })?;
        if v > version::CURRENT_API_VERSION {
            return Err(io::Error::other(format!(
                "proc-macro server is too new for blaze: API {v}, supported API {}, server {}",
                version::CURRENT_API_VERSION,
                binary_server_version()
            )));
        }
        if v < version::RUST_ANALYZER_SPAN_SUPPORT {
            return Err(io::Error::other(format!(
                "proc-macro server API {v} does not support source spans"
            )));
        }
        if v < least {
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
        env: Vec<(String, String)>,
        def_site: Span,
        call_site: Span,
        mixed_site: Span,
        current_dir: String,
        callback: Option<SubCallback<'_>>,
    ) -> Result<Result<tt::TopSubtree, String>, ServerError> {
        warm::set_traced(None);
        let (env, seen) = match &self.process.environment {
            Some(environment) => {
                let passed = environment.passed();
                let call = env.iter().map(|(name, value)| (name.as_str(), value.as_str()));
                let seen = Environment::seen(passed, call);
                (Environment::sent(passed, env), Some(seen))
            }
            None => (env, None),
        };
        match self.process.protocol {
            Protocol::LegacyJson { .. } => legacy_protocol::expand(
                proc_macro,
                self,
                subtree,
                attr,
                env,
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
                env,
                def_site,
                call_site,
                mixed_site,
                current_dir,
                callback.expect("callback required for bidirectional protocol"),
                seen,
            ),
        }
    }

    pub(crate) fn send_task_legacy<Request, Response>(
        &self,
        send: impl FnOnce(
            &mut dyn Write,
            &mut dyn BufRead,
            Request,
            &mut String,
        ) -> Result<Option<Response>, ServerError>,
        req: Request,
    ) -> Result<Response, ServerError> {
        self.with_io(String::new(), |writer, reader, buf| {
            send(writer, reader, req, buf).and_then(|res| {
                res.ok_or_else(|| {
                    let message = "proc-macro server did not respond with data".to_owned();
                    ServerError {
                        io: Some(Arc::new(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            message.clone(),
                        ))),
                        message,
                    }
                })
            })
        })
    }

    /// Runs `f` on the connection's pipes. The state is borrowed from the claim alone, so the
    /// sub-requests `f` answers lock nothing.
    fn with_io<R, B>(
        &self,
        mut buf: B,
        f: impl FnOnce(&mut dyn Write, &mut dyn BufRead, &mut B) -> Result<R, ServerError>,
    ) -> Result<R, ServerError> {
        let mut state = self.state.borrow_mut();
        let state = state.as_mut().expect("a claim holds its connection's state until dropped");
        f(&mut state.stdin, &mut state.stdout, &mut buf).map_err(|e| {
            if e.io.as_ref().map(|it| it.kind()) == Some(io::ErrorKind::BrokenPipe) {
                match state.process.exit_err() {
                    None => e,
                    Some(server_error) => {
                        self.process.exited.get_or_init(|| AssertUnwindSafe(server_error)).0.clone()
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
        self.with_io(Vec::new(), |writer, reader, buf| {
            bidirectional_protocol::run_conversation(writer, reader, buf, initial, callback)
        })
    }

    /// Runs the conversation of one expansion. Of a traced warm server, it is the next scope
    /// of the connection, and what it read is this thread's to take once the server answered
    /// ([`warm::take_traced`]): it saw the environment `seen`.
    pub(crate) fn run_expansion(
        &self,
        initial: BidirectionalMessage,
        callback: SubCallback<'_>,
        seen: Option<Vec<(OsString, OsString)>>,
    ) -> Result<BidirectionalMessage, ServerError> {
        fn tracer(state: &mut Option<ProcessSrvState>) -> Option<&mut Tracer> {
            state.as_mut()?.tracer.as_mut()
        }
        let scope = tracer(&mut self.state.borrow_mut()).map(Tracer::scope);
        let answered = self.with_io(Vec::new(), |writer, reader, buf| {
            bidirectional_protocol::run_conversation(writer, reader, buf, initial, callback)
        });
        if answered.is_ok()
            && let Some(scope) = scope
        {
            let mut state = self.state.borrow_mut();
            if let Some(tracer) = tracer(&mut state) {
                warm::set_traced(Some(tracer.traced(&scope, seen.unwrap_or_default())));
            }
        }
        answered
    }
}

/// Manages the execution of the proc-macro server process.
#[derive(Debug)]
struct Process {
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

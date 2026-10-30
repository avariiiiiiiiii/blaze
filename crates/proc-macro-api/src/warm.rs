//! Proc-macro servers blaze keeps warm (docs/v2/plan/lanes/usercode.md, #804, #805): each
//! started once, in a sandbox that traces what it reads, by the daemon's pool
//! ([`blaze_usercode::pool`]), and talked to over the Unix socket the pool listens on
//! ([`Link`], [`Bridge`]).
//!
//! A server of [`version::SCOPED_EXPANSIONS`] marks the thread that runs each expansion: it
//! calls `access("/.blaze-test/expand-<n>")` before the macro runs and `access("/.blaze-test/")`
//! after, `n` counting the connection's expansions from 0. The sandbox writes each thing it
//! sees to the live record with the scope of the thread that did it. After each answer the
//! client reads what was appended, and what the expansion read is the record's lines of its
//! scope and those of no scope (the server's own, as it loads a library), assembled against
//! the files as they are then and the environment the expansion was given
//! ([`blaze_usercode::trace::assemble`]). The thread that asked takes it with [`take_traced`].
//!
//! A server is found by its [`Server::key`]: its program, sandbox and the variables blaze sets
//! for it, never one the user's shell passes through. Those are sent with each expansion
//! instead ([`Environment::pass`]), so what a macro reads of them is traced per expansion and
//! one server serves every shell (#805).
//!
//! [`version::SCOPED_EXPANSIONS`]: crate::version::SCOPED_EXPANSIONS

use std::{
    cell::RefCell,
    collections::{BTreeSet, HashMap},
    ffi::OsString,
    fs::File,
    io::{self, BufRead, Read, Write},
    path::PathBuf,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU32, Ordering},
        mpsc,
    },
};

use blaze_core::{Command, Completion, Conn, Done, Effect, Sandbox, Tag, Traced};
use blaze_ids::{ContentId, Hasher, Idx, Interner, Sharded};
use blaze_usercode::{
    confine::LIVE,
    inputs::Disk,
    pool::{SOCKET, WarmKey},
    trace::{self, Line, Seen},
};
use rustc_hash::FxHashMap;

#[cfg(test)]
use std::{collections::VecDeque, sync::Mutex};

use crate::{ServerError, process::ProcessExit};

/// What a scope a server marks an expansion with starts with: the `n` of [`Tracer::scope`]
/// follows.
pub const SCOPE: &str = "expand-";

/// The variables blaze sets for every server, whatever it expands.
pub const SET: [(&str, &str); 1] = [("RUST_ANALYZER_INTERNALS_DO_NOT_USE", "this is unstable")];

/// The bytes between blaze and one server, as the daemon's driver carries them. Calls block
/// the thread that expands until the driver has done what they ask.
pub trait Link: Send + Sync {
    /// Sends all of `bytes`.
    fn send(&self, bytes: Vec<u8>) -> io::Result<()>;
    /// What the server sent since the last receive, at most `most` bytes: empty once it hung up.
    fn recv(&self, most: usize) -> io::Result<Vec<u8>>;
    /// Why the server is gone, once it is.
    fn ended(&self) -> Option<String>;
}

/// How much one receive asks for.
const RECV: usize = 1 << 16;

/// The writing half of a [`Link`] as [`Write`]: what is written is sent at each flush.
pub(crate) struct LinkWriter {
    link: Arc<dyn Link>,
    buffer: Vec<u8>,
}

impl LinkWriter {
    pub(crate) fn new(link: Arc<dyn Link>) -> LinkWriter {
        LinkWriter { link, buffer: Vec::new() }
    }
}

impl Write for LinkWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let bytes = std::mem::take(&mut self.buffer);
        self.link.send(bytes).map_err(|error| broken(&*self.link, error))
    }
}

/// The reading half of a [`Link`] as [`BufRead`].
pub(crate) struct LinkReader {
    link: Arc<dyn Link>,
    buffer: Vec<u8>,
    at: usize,
}

impl LinkReader {
    pub(crate) fn new(link: Arc<dyn Link>) -> LinkReader {
        LinkReader { link, buffer: Vec::new(), at: 0 }
    }
}

impl Read for LinkReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let available = self.fill_buf()?;
        let n = available.len().min(out.len());
        out[..n].copy_from_slice(&available[..n]);
        self.consume(n);
        Ok(n)
    }
}

impl BufRead for LinkReader {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.at == self.buffer.len() {
            self.buffer = self.link.recv(RECV).map_err(|error| broken(&*self.link, error))?;
            self.at = 0;
        }
        Ok(&self.buffer[self.at..])
    }

    fn consume(&mut self, n: usize) {
        self.at = (self.at + n).min(self.buffer.len());
    }
}

/// A failed send or receive as a broken pipe, which is what has the client ask whether the
/// server ended ([`ProcessExit`]).
fn broken(link: &dyn Link, error: io::Error) -> io::Error {
    match link.ended() {
        Some(why) => io::Error::new(io::ErrorKind::BrokenPipe, format!("{error}: {why}")),
        None => error,
    }
}

/// What tells the client a warm server ended: its [`Link`], which the driver told.
pub(crate) struct LinkExit(pub(crate) Arc<dyn Link>);

impl ProcessExit for LinkExit {
    fn exit_err(&mut self) -> Option<ServerError> {
        let why = self.0.ended()?;
        Some(ServerError { message: format!("the warm proc-macro server ended: {why}"), io: None })
    }
}

enum Ask {
    Send(Vec<u8>, mpsc::Sender<io::Result<Vec<u8>>>),
    Recv(usize, mpsc::Sender<io::Result<Vec<u8>>>),
}

/// The client's half of a [`Bridge`].
pub struct BridgeLink {
    asks: mpsc::Sender<Ask>,
    wake: Box<dyn Fn() + Send + Sync>,
    /// Set once, by [`Bridge::end`]: the first reason wins.
    ended: Arc<OnceLock<String>>,
}

impl BridgeLink {
    fn ask(&self, ask: impl FnOnce(mpsc::Sender<io::Result<Vec<u8>>>) -> Ask) -> io::Result<Vec<u8>> {
        if let Some(why) = self.ended.get() {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, why.clone()));
        }
        let (answer, answered) = mpsc::channel();
        self.asks.send(ask(answer)).map_err(|_| gone())?;
        (self.wake)();
        answered.recv().map_err(|_| gone())?
    }
}

fn gone() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "the daemon's driver dropped the server's connection")
}

impl Link for BridgeLink {
    fn send(&self, bytes: Vec<u8>) -> io::Result<()> {
        self.ask(|answer| Ask::Send(bytes, answer)).map(drop)
    }

    fn recv(&self, most: usize) -> io::Result<Vec<u8>> {
        self.ask(|answer| Ask::Recv(most, answer))
    }

    fn ended(&self) -> Option<String> {
        self.ended.get().cloned()
    }
}

/// A server's connection as the driver's state machines see it: what the expanding threads
/// ask of the connection, as [`Effect::Send`] and [`Effect::Recv`] on it, and their
/// completions back to the thread that asked. Sans-IO: the driver calls it from its loop.
pub struct Bridge {
    conn: Conn,
    tags: u64,
    next: u64,
    asks: mpsc::Receiver<Ask>,
    waiting: HashMap<Tag, mpsc::Sender<io::Result<Vec<u8>>>>,
    ended: Arc<OnceLock<String>>,
}

impl Bridge {
    /// The bridge of the server on `conn`, asking with tags `tags..tags + 2^32`, and the
    /// client's half. `wake` is called when a thread asked something: the driver then calls
    /// [`Bridge::effects`].
    pub fn new(conn: Conn, tags: u64, wake: Box<dyn Fn() + Send + Sync>) -> (Bridge, Arc<BridgeLink>) {
        let (sender, asks) = mpsc::channel();
        let ended = Arc::new(OnceLock::new());
        let link = Arc::new(BridgeLink { asks: sender, wake, ended: ended.clone() });
        let bridge = Bridge { conn, tags, next: 0, asks, waiting: HashMap::new(), ended };
        (bridge, link)
    }

    pub fn conn(&self) -> Conn {
        self.conn
    }

    /// What the threads asked since the last call.
    pub fn effects(&mut self, effects: &mut Vec<Effect>) {
        while let Ok(ask) = self.asks.try_recv() {
            if let Some(why) = self.ended.get() {
                let answer = match ask {
                    Ask::Send(_, answer) | Ask::Recv(_, answer) => answer,
                };
                _ = answer.send(Err(io::Error::new(io::ErrorKind::BrokenPipe, why.clone())));
                continue;
            }
            assert!(self.next < 1 << 32, "the bridge has used every tag it was given");
            let tag = Tag(self.tags + self.next);
            self.next += 1;
            let answer = match ask {
                Ask::Send(bytes, answer) => {
                    effects.push(Effect::Send { conn: self.conn, bytes, tag });
                    answer
                }
                Ask::Recv(most, answer) => {
                    effects.push(Effect::Recv { conn: self.conn, most, tag });
                    answer
                }
            };
            self.waiting.insert(tag, answer);
        }
    }

    pub fn owns(&self, tag: Tag) -> bool {
        self.waiting.contains_key(&tag)
    }

    /// Takes a completion of its own to the thread that asked.
    pub fn complete(&mut self, completion: Completion) {
        let Some(answer) = self.waiting.remove(&completion.tag) else { return };
        let answered = match completion.done {
            Done::Sent => Ok(Vec::new()),
            Done::Read(bytes) => Ok(bytes),
            Done::Failed(error) => Err(io::Error::other(error.0)),
            other => Err(io::Error::other(format!("the connection completed with {other:?}"))),
        };
        _ = answer.send(answered);
    }

    /// The server ended, or its connection did, as `why` says: what is asked of it from now
    /// on, and what is waiting, fails with that.
    pub fn end(&mut self, why: String) {
        _ = self.ended.set(why);
        let why = self.ended.get().expect("set just now, or before");
        for (_, answer) in self.waiting.drain() {
            _ = answer.send(Err(io::Error::new(io::ErrorKind::BrokenPipe, why.clone())));
        }
    }
}

/// How a warm server is started.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Server {
    /// The server's program (`blaze-proc-macro-srv`) and the content ID of its bytes.
    pub program: PathBuf,
    pub code: ContentId,
    pub dir: PathBuf,
    /// What its sandbox lets it read: the toolchain, the libraries it loads, the sources a
    /// macro may read.
    pub read: Vec<PathBuf>,
    /// What its sandbox lets it write: a temporary directory.
    pub write: Vec<PathBuf>,
    pub memory: Option<u64>,
    /// `false` only for `BLAZE_SANDBOX=off`: then nothing it expands is traced, nor kept.
    pub sandboxed: bool,
}

impl Server {
    pub fn command(&self) -> Command {
        Command {
            program: self.program.clone(),
            args: ["--format", "bidirectional-postcard-prototype"].map(OsString::from).to_vec(),
            env: set(),
            dir: self.dir.clone(),
            stdin: Vec::new(),
            sandbox: match self.sandboxed {
                true => Sandbox::On {
                    read: self.read.clone(),
                    write: self.write.clone(),
                    network: false,
                    trace: true,
                },
                false => Sandbox::Off,
            },
            memory: self.memory,
        }
    }

    pub fn key(&self) -> WarmKey {
        WarmKey::of(&self.code, &self.command(), &set())
    }
}

fn set() -> Vec<(OsString, OsString)> {
    SET.iter().map(|(name, value)| (OsString::from(name), OsString::from(value))).collect()
}

/// The environment the expansions of warm servers are given, besides the variables of the
/// crate they expand for: what blaze sets for a server ([`SET`]) and what the user's shell
/// passes through, as the daemon was last told ([`Environment::pass`]).
///
/// Every environment passed is kept once, by its content ID, in a table whose values never
/// move, and the one in use is its index behind one atomic: an expansion borrows it with one
/// acquire load, and a `pass` publishes another with one release store. A shell has few
/// environments, so keeping each is what lets a reader never wait or copy.
#[derive(Default)]
pub struct Environment {
    all: Sharded<Vec<(String, String)>>,
    /// The index of the one in use, plus one; 0 before the first `pass`.
    now: AtomicU32,
}

impl std::fmt::Debug for Environment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Environment").field("passed", &self.passed()).finish()
    }
}

impl Environment {
    pub fn new() -> Arc<Environment> {
        Arc::new(Environment::default())
    }

    /// The variables the user's shell passes through to the expansions asked for from now on.
    /// A variable whose name or value is not UTF-8 can't be sent to a server, so it is refused,
    /// naming it: an expansion would not see it as the macro under rustc would.
    pub fn pass(&self, env: Vec<(OsString, OsString)>) -> Result<(), String> {
        let mut passed = Vec::with_capacity(env.len());
        for (name, value) in env {
            match (name.into_string(), value.into_string()) {
                (Ok(name), Ok(value)) => passed.push((name, value)),
                (Ok(name), Err(_)) => {
                    return Err(format!(
                        "the variable {name} is not UTF-8, so no proc macro can be given it"
                    ));
                }
                (Err(name), _) => {
                    return Err(format!(
                        "a variable named {name:?} is not UTF-8, so no proc macro can be given it"
                    ));
                }
            }
        }
        passed.sort();
        let mut id = Hasher::new("blaze v2 passed environment 1");
        id.update(&(passed.len() as u64).to_le_bytes());
        for (name, value) in &passed {
            id.part(name.as_bytes()).part(value.as_bytes());
        }
        let at = self.all.intern(id.finish(), || passed);
        self.now.store(at.raw() + 1, Ordering::Release);
        Ok(())
    }

    /// The passed-through variables now: one expansion sends and is checked against one
    /// snapshot of them, borrowed where it is kept.
    pub fn passed(&self) -> &[(String, String)] {
        match self.now.load(Ordering::Acquire) {
            0 => &[],
            at => self.all.get(Idx::from_raw(at - 1)).as_slice(),
        }
    }

    /// The environment an expansion sees that is sent `passed` and `call`: the server's own,
    /// then `passed` over it, then `call` over that, as the server sets them in order.
    pub fn seen<'a>(
        passed: &[(String, String)],
        call: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Vec<(OsString, OsString)> {
        let mut seen: FxHashMap<OsString, OsString> = set().into_iter().collect();
        let passed = passed.iter().map(|(name, value)| (name.as_str(), value.as_str()));
        for (name, value) in passed.chain(call) {
            seen.insert(name.into(), value.into());
        }
        let mut seen: Vec<(OsString, OsString)> = seen.into_iter().collect();
        seen.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        seen
    }

    /// What is sent to a server for an expansion with the crate's variables `call`: `passed`
    /// first, so that the crate's are what the macro sees of a name both have.
    pub fn sent(passed: &[(String, String)], call: Vec<(String, String)>) -> Vec<(String, String)> {
        let mut sent = passed.to_vec();
        sent.extend(call);
        sent
    }
}

thread_local! {
    /// What the last expansion this thread asked of a server read, once its server answered.
    static TRACED: RefCell<Option<Result<Traced, String>>> = const { RefCell::new(None) };
}

/// What the last expansion this thread asked for read, as its server's sandbox traced it:
/// `None` when it was not expanded by a traced server (`BLAZE_SANDBOX=off`), `Err` when its
/// record could not be read or an input it read could not be. Taken once.
pub fn take_traced() -> Option<Result<Traced, String>> {
    TRACED.with(|it| it.borrow_mut().take())
}

pub(crate) fn set_traced(traced: Option<Result<Traced, String>>) {
    TRACED.with(|it| *it.borrow_mut() = traced);
}

/// A warm server's sandbox's live record, read by scope.
pub(crate) struct Tracer {
    live: PathBuf,
    /// The variables blaze set for this one server: its socket and its record.
    own: Vec<(OsString, OsString)>,
    /// How much of the record has been read: up to the end of its last whole line.
    read: u64,
    next: u64,
    /// The lines of no scope so far, which are every expansion's.
    // BTreeSet: walked in order on every expansion while it still grows; that order picks which
    // spelling of one path `trace::assemble` keeps.
    common: BTreeSet<Line>,
    disk: Disk,
    /// The record, open from the first expansion on, and the buffer its new bytes are read
    /// into, reused.
    file: Option<File>,
    bytes: Vec<u8>,
}

impl Tracer {
    /// The tracer of the server whose live record is `live` and whose socket is `socket`.
    pub(crate) fn new(live: PathBuf, socket: PathBuf) -> Tracer {
        let own =
            vec![(SOCKET.into(), socket.into_os_string()), (LIVE.into(), live.clone().into_os_string())];
        Tracer {
            live,
            own,
            read: 0,
            next: 0,
            common: BTreeSet::new(),
            disk: Disk::new(Vec::new()),
            file: None,
            bytes: Vec::new(),
        }
    }

    /// The scope of the next expansion of the connection.
    pub(crate) fn scope(&mut self) -> String {
        let scope = format!("{SCOPE}{}", self.next);
        self.next += 1;
        scope
    }

    /// What the expansion of `scope` read, once its server answered it: it saw the
    /// environment `seen` and this server's own variables. The lines of `scope` are hashed
    /// where they lie in the buffer; only the new lines of no scope are owned, kept as
    /// [`Line`]s for every later expansion; the rest are skipped.
    pub(crate) fn traced(&mut self, scope: &str, seen: Vec<(OsString, OsString)>) -> Result<Traced, String> {
        let live = self.live.display();
        let unreadable = |error: io::Error| format!("the sandbox's live record {live} can't be read: {error}");
        let file = match &mut self.file {
            Some(file) => file,
            None => self.file.insert(File::open(&self.live).map_err(unreadable)?),
        };
        let filled = appended(file, self.read, &mut self.bytes).map_err(unreadable)?;
        let whole = self.bytes[..filled].iter().rposition(|it| *it == b'\n').map_or(0, |at| at + 1);
        let text = std::str::from_utf8(&self.bytes[..whole])
            .map_err(|error| format!("the sandbox's live record {live} is not text: {error}"))?;
        let mut mine = Vec::new();
        for (number, line) in text.split_terminator('\n').enumerate() {
            let bad = || format!("the sandbox's live record {live}: line {} is not a record line", number + 1);
            let mut parts = line.splitn(3, '\t');
            let seen_as = parts.next().and_then(Seen::of).ok_or_else(bad)?;
            let (of, what) = (parts.next().ok_or_else(bad)?, parts.next().ok_or_else(bad)?);
            if of.is_empty() {
                if !plumbing(seen_as, what) {
                    self.common.insert(Line { seen: seen_as, scope: String::new(), what: what.to_owned() });
                }
            } else if of == scope {
                mine.push((seen_as, what));
            }
        }
        self.read += whole as u64;
        let mut env = seen;
        env.retain(|it| !self.own.iter().any(|own| own.0 == it.0));
        env.extend(self.own.iter().cloned());
        self.disk.set_env(env);
        let common = self.common.iter().map(|line| (line.seen, line.what.as_str()));
        trace::assemble_seen(mine.into_iter().chain(common), &mut self.disk).map_err(|error| error.to_string())
    }
}

/// Reads what `file` holds from `at` on into `bytes`, which is kept as long as its longest
/// read and never cleared, and says how much was read: one positioned read when the record grew
/// by less than the buffer. A short read of a regular file is its end, so no read of 0 follows.
fn appended(file: &File, at: u64, bytes: &mut Vec<u8>) -> io::Result<usize> {
    if bytes.is_empty() {
        bytes.resize(1 << 16, 0);
    }
    let mut filled = 0;
    loop {
        #[cfg(unix)]
        let read = std::os::unix::fs::FileExt::read_at(file, &mut bytes[filled..], at + filled as u64);
        #[cfg(windows)]
        let read = std::os::windows::fs::FileExt::seek_read(file, &mut bytes[filled..], at + filled as u64);
        match read {
            Ok(0) => return Ok(filled),
            Ok(n) => filled += n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
        if filled < bytes.len() {
            return Ok(filled);
        }
        bytes.resize(bytes.len() * 2, 0);
    }
}

/// A line of no scope that is the server finding its socket and record before it serves: what
/// a server does with them is the same whatever their values, which differ per server.
fn plumbing(seen: Seen, what: &str) -> bool {
    seen == Seen::Env && (what == SOCKET || what == LIVE)
}

/// One warm server a client is made of.
pub struct Warm {
    pub link: Arc<dyn Link>,
    /// Its sandbox's live record and its socket, as the machine names them; `None` for a
    /// server outside a sandbox ([`Server::sandboxed`]), which traces nothing.
    pub traced: Option<(PathBuf, PathBuf)>,
}

/// Where a client gets another warm server of its key when every one it has is in the middle
/// of an expansion: the daemon's pool ([`blaze_usercode::pool::Pool::take`], or
/// [`blaze_usercode::pool::Pool::start`] when none is idle), through its driver, connected as
/// [`Bridge::new`] is. A server's protocol is one request at a time, and one connection per
/// expansion in flight is what keeps it so with no lock: an expansion asked for while another
/// answers a sub-request from the database gets its own server instead of waiting on itself.
pub trait Servers: Send + Sync {
    /// Another server of the client's key, connected and the client's from now on.
    fn connect(&self) -> io::Result<Warm>;
}

/// A link that plays a server from answers given beforehand, for tests.
#[cfg(test)]
pub(crate) struct Script {
    pub(crate) sent: Mutex<Vec<u8>>,
    pub(crate) answers: Mutex<VecDeque<Vec<u8>>>,
    pub(crate) ended: Mutex<Option<String>>,
}

#[cfg(test)]
impl Link for Script {
    fn send(&self, bytes: Vec<u8>) -> io::Result<()> {
        self.sent.lock().unwrap().extend(bytes);
        Ok(())
    }

    fn recv(&self, most: usize) -> io::Result<Vec<u8>> {
        let mut answers = self.answers.lock().unwrap();
        let Some(mut next) = answers.pop_front() else { return Ok(Vec::new()) };
        if next.len() > most {
            let rest = next.split_off(most);
            answers.push_front(rest);
        }
        Ok(next)
    }

    fn ended(&self) -> Option<String> {
        self.ended.lock().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(name: &str, text: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("proc-macro-api-warm-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let live = dir.join("live");
        std::fs::write(&live, text).unwrap();
        live
    }

    fn append(live: &PathBuf, text: &str) {
        let mut file = std::fs::OpenOptions::new().append(true).open(live).unwrap();
        file.write_all(text.as_bytes()).unwrap();
    }

    fn env_line(scope: &str, name: &str) -> String {
        Line::new(Seen::Env, scope, name).text()
    }

    #[test]
    fn each_expansion_reads_its_scope_and_no_scope_only() {
        let live = record("scopes", "");
        let mut tracer = Tracer::new(live.clone(), "/pool/socket/0/s".into());
        let first = tracer.scope();
        let second = tracer.scope();
        assert_eq!((first.as_str(), second.as_str()), ("expand-0", "expand-1"));
        append(&live, &env_line("", "COMMON"));
        append(&live, &env_line(&first, "FIRST"));
        append(&live, &env_line(&second, "SECOND"));
        let env = |names: &[&str]| -> Vec<(OsString, OsString)> {
            names.iter().map(|it| (OsString::from(*it), OsString::from("v"))).collect()
        };
        let traced = tracer.traced(&first, env(&["COMMON", "FIRST", "SECOND"])).unwrap();
        let names: Vec<_> = traced.env.iter().map(|it| it.0.to_str().unwrap().to_owned()).collect();
        assert_eq!(names, ["COMMON", "FIRST"]);
        // What the next expansion's scope wrote before this one's answer was read is gone with
        // that read; what no scope wrote stays every expansion's.
        append(&live, &env_line(&second, "LATER"));
        let traced = tracer.traced(&second, env(&["COMMON", "LATER"])).unwrap();
        let names: Vec<_> = traced.env.iter().map(|it| it.0.to_str().unwrap().to_owned()).collect();
        assert_eq!(names, ["COMMON", "LATER"]);
    }

    #[test]
    fn a_line_still_being_written_waits_for_the_next_read() {
        let live = record("partial", "");
        let mut tracer = Tracer::new(live.clone(), "/s".into());
        let scope = tracer.scope();
        let line = env_line(&scope, "HALF");
        append(&live, &line[..line.len() - 3]);
        assert!(tracer.traced(&scope, Vec::new()).unwrap().env.is_empty());
        append(&live, &line[line.len() - 3..]);
        assert_eq!(tracer.traced(&scope, Vec::new()).unwrap().env.len(), 1);
    }

    #[test]
    fn a_servers_own_socket_and_record_are_not_what_it_expands_from() {
        let live = record("plumbing", "");
        let mut tracer = Tracer::new(live.clone(), "/s".into());
        append(&live, &env_line("", SOCKET));
        append(&live, &env_line("", LIVE));
        let scope = tracer.scope();
        assert!(tracer.traced(&scope, Vec::new()).unwrap().env.is_empty());
        // A macro that reads one itself is keyed by this server's value.
        let scope = tracer.scope();
        append(&live, &env_line(&scope, SOCKET));
        let traced = tracer.traced(&scope, Vec::new()).unwrap();
        assert_eq!(traced.env, [(OsString::from(SOCKET), Some(OsString::from("/s")))]);
    }

    #[test]
    fn a_missing_or_broken_record_is_an_error() {
        let mut tracer = Tracer::new("/nonexistent/proc-macro-api/live".into(), "/s".into());
        let scope = tracer.scope();
        assert!(tracer.traced(&scope, Vec::new()).unwrap_err().contains("can't be read"));
        let live = record("broken", "not a line\n");
        let mut tracer = Tracer::new(live, "/s".into());
        let scope = tracer.scope();
        assert!(tracer.traced(&scope, Vec::new()).is_err());
    }

    #[test]
    fn a_file_read_in_scope_is_hashed_as_it_is() {
        let live = record("file", "");
        let file = live.with_file_name("input.txt");
        std::fs::write(&file, "one").unwrap();
        let mut tracer = Tracer::new(live.clone(), "/s".into());
        let scope = tracer.scope();
        append(&live, &Line::new(Seen::Read, &scope, file.to_str().unwrap()).text());
        let traced = tracer.traced(&scope, Vec::new()).unwrap();
        let hash = blaze_usercode::inputs::hash_reader(&b"one"[..]).unwrap();
        assert_eq!(traced.files, [(file.clone(), Some(hash))]);
        std::fs::write(&file, "three").unwrap();
        let scope = tracer.scope();
        append(&live, &Line::new(Seen::Read, &scope, file.to_str().unwrap()).text());
        assert_ne!(tracer.traced(&scope, Vec::new()).unwrap().files[0].1, Some(hash));
    }

    #[test]
    fn the_environment_is_the_servers_then_the_shells_then_the_crates() {
        let passed = [("PATH".to_owned(), "/a".to_owned()), ("X".to_owned(), "shell".to_owned())];
        let call = vec![("X".to_owned(), "crate".to_owned())];
        let seen = Environment::seen(&passed, call.iter().map(|(n, v)| (n.as_str(), v.as_str())));
        let get = |name: &str| seen.iter().find(|it| it.0 == name).map(|it| it.1.clone());
        assert_eq!(get("X"), Some("crate".into()));
        assert_eq!(get("PATH"), Some("/a".into()));
        assert_eq!(get(SET[0].0), Some(SET[0].1.into()));
        assert!(seen.windows(2).all(|it| it[0].0 < it[1].0), "sorted by name, each once: {seen:?}");
        let sent = Environment::sent(&passed, call);
        assert_eq!(sent.last(), Some(&("X".to_owned(), "crate".to_owned())));
    }

    #[test]
    fn a_server_is_not_keyed_by_what_the_shell_passes_through() {
        let server = Server {
            program: "/blaze/libexec/blaze-proc-macro-srv".into(),
            code: ContentId::of("test", b"server"),
            dir: "/".into(),
            read: vec!["/toolchain".into()],
            write: Vec::new(),
            memory: None,
            sandboxed: true,
        };
        let environment = Environment::new();
        let before = server.key();
        environment.pass(vec![("PATH".into(), "/b".into())]).unwrap();
        assert_eq!(server.key(), before);
        assert!(matches!(server.command().sandbox, Sandbox::On { trace: true, network: false, .. }));
        let other = Server { read: vec!["/other".into()], ..server.clone() };
        assert_ne!(other.key(), before);
    }

    #[test]
    fn what_was_passed_stays_borrowable_after_the_next_pass_and_one_seen_again_is_kept_once() {
        let environment = Environment::new();
        assert!(environment.passed().is_empty());
        environment.pass(vec![("B".into(), "2".into()), ("A".into(), "1".into())]).unwrap();
        let first = environment.passed();
        assert_eq!(first, [("A".to_owned(), "1".to_owned()), ("B".to_owned(), "2".to_owned())]);
        environment.pass(vec![("A".into(), "other".into())]).unwrap();
        assert_eq!(environment.passed(), [("A".to_owned(), "other".to_owned())]);
        assert_eq!(first[0].1, "1");
        environment.pass(vec![("A".into(), "1".into()), ("B".into(), "2".into())]).unwrap();
        assert!(std::ptr::eq(environment.passed(), first));
        assert_eq!(environment.all.len(), 2);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for n in 0..100 {
                    environment.pass(vec![("N".into(), (n % 3).to_string().into())]).unwrap();
                }
            });
            for _ in 0..100 {
                let passed = environment.passed();
                assert!(passed.len() <= 2, "{passed:?}");
            }
        });
        assert_eq!(environment.all.len(), 5);
    }

    #[test]
    fn the_first_reason_a_bridge_ended_is_the_one_told() {
        let (mut bridge, link) = Bridge::new(Conn(2), 0, Box::new(|| {}));
        assert_eq!(link.ended(), None);
        bridge.end("it exited with 1".to_owned());
        bridge.end("its connection closed".to_owned());
        assert_eq!(link.ended().as_deref(), Some("it exited with 1"));
    }

    #[cfg(unix)]
    #[test]
    fn a_variable_that_is_not_utf8_is_refused_by_name() {
        use std::os::unix::ffi::OsStringExt as _;
        let environment = Environment::new();
        let bad = OsString::from_vec(vec![0xff]);
        let said = environment.pass(vec![("NAME".into(), bad)]).unwrap_err();
        assert!(said.contains("NAME"));
    }

    #[test]
    fn the_bridge_turns_calls_into_effects_and_completions_into_answers() {
        let (mut bridge, link) = Bridge::new(Conn(3), 100, Box::new(|| {}));
        let thread = std::thread::spawn({
            let link = link.clone();
            move || {
                link.send(b"hello".to_vec()).unwrap();
                link.recv(8).unwrap()
            }
        });
        let mut answered = 0;
        while answered < 2 {
            let mut effects = Vec::new();
            bridge.effects(&mut effects);
            for effect in effects {
                let tag = effect.tag();
                assert!(bridge.owns(tag));
                assert!(tag.0 >= 100);
                let done = match effect {
                    Effect::Send { conn, bytes, .. } => {
                        assert_eq!((conn, bytes.as_slice()), (Conn(3), b"hello".as_slice()));
                        Done::Sent
                    }
                    Effect::Recv { conn, most, .. } => {
                        assert_eq!((conn, most), (Conn(3), 8));
                        Done::Read(b"world".to_vec())
                    }
                    other => panic!("{other:?}"),
                };
                bridge.complete(Completion { tag, done });
                assert!(!bridge.owns(tag));
                answered += 1;
            }
            std::thread::yield_now();
        }
        assert_eq!(thread.join().unwrap(), b"world");
    }

    #[test]
    fn an_ended_bridge_fails_what_waits_and_what_comes() {
        let (mut bridge, link) = Bridge::new(Conn(1), 0, Box::new(|| {}));
        bridge.end("it exited with 101".to_owned());
        let error = link.send(Vec::new()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert!(LinkExit(link).exit_err().unwrap().message.contains("101"));
    }

    #[test]
    fn the_link_reads_and_writes_as_io() {
        let script = Arc::new(Script {
            sent: Mutex::default(),
            answers: Mutex::new(VecDeque::from([b"ab\0c".to_vec(), b"d\0".to_vec()])),
            ended: Mutex::default(),
        });
        let mut writer = LinkWriter::new(script.clone());
        writer.write_all(b"xy").unwrap();
        assert!(script.sent.lock().unwrap().is_empty());
        writer.flush().unwrap();
        assert_eq!(*script.sent.lock().unwrap(), b"xy");
        let mut reader = LinkReader::new(script);
        let mut buf = Vec::new();
        reader.read_until(0, &mut buf).unwrap();
        assert_eq!(buf, b"ab\0");
        buf.clear();
        reader.read_until(0, &mut buf).unwrap();
        assert_eq!(buf, b"cd\0");
    }
}

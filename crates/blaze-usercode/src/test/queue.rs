//! One global queue of tests (#1443): a sans-IO state machine over every test binary of a
//! request, running children from every binary on all cores.
//!
//! - **A process per test, from a fork server.** Each binary starts once, in its sandbox
//!   (tracing what it reads), as a fork server ([`super::FORK_SERVER`]): blaze writes an empty
//!   record for its sandbox, listens on a Unix socket of its own (`Effect::Listen`), and
//!   spawns the binary with the socket's path in [`wire::SOCKET`] and the record's in
//!   [`confine::LIVE`]. The server connects and says hello with its tests; for each test the
//!   queue sends a run, the server forks a child that marks its scope and runs the one test,
//!   and answers how it ended with its output ([`wire`]). The lines the sandbox appended to
//!   the record under the test's scope (and under none) are what the test read
//!   ([`Trace::Lines`]); the record is read after each answer, so every line of the child is
//!   in it.
//! - **Critical first.** Every test of every binary is in one order, the longest expected
//!   first ([`Job::expected`], from the durations kept with the passes), and the next test
//!   whose server is ready runs whenever a core is free ([`Config::jobs`]).
//! - **Timeouts, retries, fail-fast.** A test past its [`Job::timeout`] is killed through its
//!   server (`Frame::Kill`); a server that does not end it within [`Config::kill_grace`] is
//!   killed itself. A failed test runs again only up to its [`Job::retries`] (non-zero only
//!   for tests named flaky). With [`Config::fail_fast`], the first failure stops new tests;
//!   those not started are [`Outcome::NotRun`].
//! - **libtest's threads.** A binary of a package that opts out ([`Mode::Threads`]) runs as
//!   one spawn with libtest's own arguments and as many threads as it is given cores; its
//!   result is per binary ([`Finished::test`] is `None`), and what it read is the whole
//!   process's ([`Trace::Whole`]).
//!
//! The queue asks with tags `tags..tags + 2^32` ([`Config::tags`]). It reads no clock: the
//! driver's `now` comes with each call. It is [`Queue::finished`] once every test has a result
//! and every process it started has ended; timers still in flight then are stale and their
//! completions are dropped.

use std::{
    borrow::Cow,
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet, HashMap},
    ffi::OsString,
    path::PathBuf,
    time::Duration,
};

use blaze_core::{
    Address, Command, Completion, Conn, Done, Effect, Exit, FileName, Sandbox, Tag, Traced,
};
use blaze_ids::ContentId;

use super::wire::{self, End, Frame, Frames};
use crate::{
    confine,
    trace::{self, BadRecord, Inputs, Line, Seen},
};

/// How a binary's tests run.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Mode {
    /// A process per test, from a fork server: the default.
    Process,
    /// libtest's threads in one process, for a package whose manifest asks for it: `args` are
    /// what the user gave libtest, after the names; the whole run is killed past `timeout`.
    Threads { args: Vec<OsString>, timeout: Duration },
}

/// A test to run.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Job {
    pub name: String,
    /// It is `#[ignore]`d and was asked for anyway.
    pub ignore: bool,
    /// How long it took when it last ran; zero for a test never run.
    pub expected: Duration,
    pub timeout: Duration,
    /// How many times it runs again after a failure: non-zero only for a test named flaky.
    pub retries: u32,
}

/// A test binary and the tests of it to run.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Binary {
    /// [`crate::Reached::binary`].
    pub id: ContentId,
    /// For people: `blaze-usercode lib`.
    pub name: String,
    pub program: PathBuf,
    /// The whole environment the tests see; what of it they read is traced.
    pub env: Vec<(OsString, OsString)>,
    pub dir: PathBuf,
    pub read: Vec<PathBuf>,
    pub write: Vec<PathBuf>,
    pub network: bool,
    pub memory: Option<u64>,
    pub mode: Mode,
    pub jobs: Vec<Job>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Config {
    /// The driver's root, as the processes it starts see it.
    pub root: PathBuf,
    /// The request's directory under the root, where the sockets and records go.
    pub dir: FileName,
    /// How many tests (children, or libtest threads) run at once: the cores.
    pub jobs: usize,
    pub fail_fast: bool,
    pub kill_grace: Duration,
    pub tags: u64,
}

/// Why a test failed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Failure {
    /// It exited with this code: it failed as libtest judges, and its output says why.
    Code(i32),
    /// A signal ended it: an abort, a crash.
    Signal(i32),
    /// It ran past its timeout and was killed.
    TimedOut(Duration),
    /// It could not be run or its result could not be had, and why: the binary could not be
    /// started in its sandbox, its server ended, it has no test of that name.
    Lost(String),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Outcome {
    Passed,
    Failed(Failure),
    /// Not run: an earlier failure stopped the run (fail-fast), or the request was cancelled.
    NotRun,
}

/// What a test read at run time.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Trace {
    /// The lines of the sandbox's record of its scope and of none, without blaze's own (its
    /// socket, its record, the variables it set for the server).
    Lines(Vec<Line>),
    /// What a binary run whole read, from its sandbox.
    Whole(Traced),
    /// Not known, and why: no pass can be kept.
    Unknown(String),
}

impl Trace {
    /// What was read, hashed as `inputs` has it now: what [`super::select::record`] keeps a
    /// pass by. To be asked as the result comes, before anything else changes the files.
    pub fn traced<I: Inputs>(&self, inputs: &mut I) -> Result<Option<Traced>, I::Error> {
        match self {
            Trace::Lines(lines) => trace::assemble(lines, None, inputs).map(Some),
            Trace::Whole(traced) => Ok(Some(traced.clone())),
            Trace::Unknown(_) => Ok(None),
        }
    }
}

/// A test's result.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Finished {
    /// Its binary, by its place in the queue's binaries.
    pub binary: usize,
    /// `None` for a binary run whole as libtest's threads.
    pub test: Option<String>,
    pub outcome: Outcome,
    /// How many times it ran: more than one only for a retried test.
    pub attempts: u32,
    /// What it wrote, standard output and error together; each earlier attempt's first.
    pub output: Vec<u8>,
    /// How long its last attempt took, by the driver's clock.
    pub took: Duration,
    pub trace: Trace,
}

/// The size of each read of a record.
const CHUNK: usize = 1 << 20;
/// The job of a binary run whole, in the order.
const WHOLE: usize = usize::MAX;
/// The least a receive from a fork server asks for; more when a longer frame is coming.
const RECV: usize = 1 << 16;
/// A job its server's hello did not name.
const NO_TEST: u32 = u32::MAX;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum For {
    Write(usize),
    Listen(usize),
    Spawn(usize),
    Recv(usize),
    Send(usize),
    Hangup,
    Read(usize),
    Deadline(u64),
    Grace(u64),
    Whole(usize),
    WholeDeadline(usize),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    /// Its record is being made.
    Writing,
    /// Spawned and listened for; not yet said hello.
    Starting,
    Ready,
    /// Told to quit; its exit is awaited.
    Quitting,
    Ended,
}

/// A test that ended, waiting for the record to be read past its lines.
struct Ended {
    id: u64,
    end: End,
    output: Vec<u8>,
    at: Duration,
}

struct Server {
    phase: Phase,
    socket: FileName,
    record: FileName,
    /// The absolute paths of the socket and the record, as the record names them.
    socket_path: String,
    record_path: String,
    spawn: Option<Tag>,
    listen: Option<Tag>,
    conn: Option<Conn>,
    hung_up: bool,
    /// Why it is being killed, said to the tests it loses.
    lost: Option<String>,
    frames: Frames,
    /// Each job's index in the server's hello, [`NO_TEST`] for one it did not name: what a
    /// run is sent with, by direct index.
    job_index: Vec<u32>,
    /// The job each test name of the hello is, for the scope of a record's line: one hash of
    /// the borrowed scope per line, nothing allocated for it.
    by_scope: HashMap<String, u32>,
    running: BTreeSet<u64>,
    /// How far the record has been read, and a last line not yet whole.
    at: u64,
    partial: Vec<u8>,
    /// The lines of each job's scope, by job, taken by its last attempt's result.
    scoped: Vec<Vec<Line>>,
    /// The lines of every scope, each once (in the order first seen), with the kinds of
    /// [`Seen`] each `what` has among them as bits: a line every test is given is kept, and
    /// so cloned into each result, only once however often the server repeats it.
    common: Vec<Line>,
    common_seen: HashMap<String, u16>,
    /// Frames for the server not yet given to the driver, and the timers that go after them:
    /// what one call to the queue says to a server goes as one send.
    outbox: Vec<u8>,
    timers: Vec<Effect>,
    dirty: bool,
    reading: bool,
    waiting: Vec<Ended>,
    next: Vec<Ended>,
    /// Why the record can't be read: every test's trace is unknown.
    broken: Option<String>,
}

struct Attempt {
    binary: usize,
    job: usize,
    number: u32,
    started: Duration,
    timed_out: bool,
}

struct Whole {
    spawn: Tag,
    started: Duration,
    slots: usize,
    timed_out: bool,
}

struct Bin {
    plan: Binary,
    /// Jobs (one for a binary run whole) without a final result.
    left: usize,
    server: Option<Server>,
    whole: Option<Whole>,
}

/// The earlier attempts of a retried test.
#[derive(Default)]
struct Tries {
    done: u32,
    output: Vec<u8>,
}

pub struct Queue {
    config: Config,
    bins: Vec<Bin>,
    /// Not started: the longest expected first, then by binary and job.
    pending: BTreeSet<(Reverse<Duration>, usize, usize)>,
    /// What of `pending` can start now (its server said hello, or it is a binary run
    /// whole), in the same order: the next test is its first, with no scan past those of
    /// servers still starting.
    ready: BTreeSet<(Reverse<Duration>, usize, usize)>,
    /// Servers with frames or timers in `outbox` and `timers`.
    dirty: Vec<usize>,
    attempts: BTreeMap<u64, Attempt>,
    tries: HashMap<(usize, usize), Tries>,
    tags: BTreeMap<Tag, For>,
    next_tag: u64,
    next_id: u64,
    busy: usize,
    stopping: bool,
    cancelled: bool,
    now: Duration,
    results: Vec<Finished>,
}

impl Queue {
    /// A queue of `binaries`' jobs; asks to start what it can.
    pub fn new(config: Config, binaries: Vec<Binary>, now: Duration, effects: &mut Vec<Effect>) -> Queue {
        let mut queue = Queue {
            config,
            bins: Vec::new(),
            pending: BTreeSet::new(),
            ready: BTreeSet::new(),
            dirty: Vec::new(),
            attempts: BTreeMap::new(),
            tries: HashMap::new(),
            tags: BTreeMap::new(),
            next_tag: 0,
            next_id: 0,
            busy: 0,
            stopping: false,
            cancelled: false,
            now,
            results: Vec::new(),
        };
        for (b, plan) in binaries.into_iter().enumerate() {
            let left = match plan.mode {
                Mode::Process => plan.jobs.len(),
                Mode::Threads { .. } => usize::from(!plan.jobs.is_empty()),
            };
            queue.bins.push(Bin { plan, left, server: None, whole: None });
            if left == 0 {
                continue;
            }
            match queue.bins[b].plan.mode {
                Mode::Threads { .. } => {
                    let expected = queue.bins[b].plan.jobs.iter().map(|it| it.expected).sum();
                    queue.pending.insert((Reverse(expected), b, WHOLE));
                    queue.ready.insert((Reverse(expected), b, WHOLE));
                }
                Mode::Process if !cfg!(unix) => {
                    let why = format!(
                        "{}: this platform has no fork, so no fork server: its package must run its tests as libtest's threads",
                        queue.bins[b].plan.name
                    );
                    for j in 0..queue.bins[b].plan.jobs.len() {
                        queue.finish(b, Some(j), Outcome::Failed(Failure::Lost(why.clone())), Vec::new(), Duration::ZERO, Trace::Unknown(why.clone()));
                    }
                }
                Mode::Process => {
                    for (j, job) in queue.bins[b].plan.jobs.iter().enumerate() {
                        queue.pending.insert((Reverse(job.expected), b, j));
                    }
                    queue.start_server(b, effects);
                }
            }
        }
        queue.schedule(effects);
        queue.flush(effects);
        queue
    }

    pub fn owns(&self, tag: Tag) -> bool {
        tag.0.wrapping_sub(self.config.tags) < 1 << 32
    }

    /// The results that came since the last call.
    pub fn take(&mut self) -> Vec<Finished> {
        std::mem::take(&mut self.results)
    }

    /// The binaries, by the index a [`Finished`] names.
    pub fn binary(&self, at: usize) -> &Binary {
        &self.bins[at].plan
    }

    /// Every test has a result and every process the queue started has ended.
    pub fn finished(&self) -> bool {
        self.bins.iter().all(|bin| {
            bin.left == 0
                && bin.whole.is_none()
                && bin.server.as_ref().is_none_or(|it| it.spawn.is_none() && it.phase != Phase::Writing)
        })
    }

    /// Stops the run: what has not started is not run, and what runs is killed and reported
    /// not run.
    pub fn cancel(&mut self, now: Duration, effects: &mut Vec<Effect>) {
        self.now = now;
        self.cancelled = true;
        self.stop();
        for b in 0..self.bins.len() {
            if let Some(whole) = &self.bins[b].whole {
                effects.push(Effect::Kill { tag: whole.spawn });
            }
            self.kill_server(b, "the run was cancelled".to_owned(), effects);
        }
        self.flush(effects);
    }

    /// Takes a completion of one of its tags.
    pub fn complete(&mut self, completion: Completion, now: Duration, effects: &mut Vec<Effect>) {
        self.completed(completion, now, effects);
        self.flush(effects);
    }

    fn completed(&mut self, completion: Completion, now: Duration, effects: &mut Vec<Effect>) {
        self.now = now;
        let tag = completion.tag;
        let Some(&purpose) = self.tags.get(&tag) else { return };
        if !matches!(purpose, For::Listen(_)) {
            self.tags.remove(&tag);
        }
        match purpose {
            For::Write(b) => self.written(b, completion.done, effects),
            For::Listen(b) => self.listened(b, tag, completion.done, effects),
            For::Spawn(b) => {
                let server = self.server(b);
                server.spawn = None;
                let (why, output) = match completion.done {
                    Done::Exited(exit) => {
                        let why = server.lost.take().unwrap_or_else(|| ended(&exit));
                        (why, [exit.stdout, exit.stderr].concat())
                    }
                    Done::Failed(error) => (
                        format!("{} could not be started in its sandbox: {error}", self.bins[b].plan.program.display()),
                        Vec::new(),
                    ),
                    other => (format!("its spawn completed as {other:?}"), Vec::new()),
                };
                self.server_ended(b, why, output, effects);
            }
            For::Recv(b) => self.received(b, completion.done, effects),
            For::Send(b) => {
                if let Done::Failed(error) = completion.done {
                    self.kill_server(b, format!("sending to its fork server: {error}"), effects);
                }
            }
            For::Hangup => {}
            For::Read(b) => self.read(b, completion.done, effects),
            For::Deadline(id) => {
                let Some(attempt) = self.attempts.get_mut(&id) else { return };
                let b = attempt.binary;
                let running = self.bins[b].server.as_ref().is_some_and(|it| it.running.contains(&id));
                if !running || attempt.timed_out {
                    return;
                }
                attempt.timed_out = true;
                self.send(b, &Frame::Kill { id });
                let grace = self.tag(For::Grace(id));
                self.timer(b, self.config.kill_grace, grace);
            }
            For::Grace(id) => {
                let Some(attempt) = self.attempts.get(&id) else { return };
                let (b, j) = (attempt.binary, attempt.job);
                if self.bins[b].server.as_ref().is_some_and(|it| it.running.contains(&id)) {
                    let why = format!(
                        "{} did not end {:?} after it was killed, so its fork server was killed",
                        self.bins[b].plan.jobs[j].name, self.config.kill_grace
                    );
                    self.kill_server(b, why, effects);
                }
            }
            For::Whole(b) => self.whole_ended(b, completion.done),
            For::WholeDeadline(b) => {
                if let Some(whole) = &mut self.bins[b].whole
                    && !whole.timed_out
                {
                    whole.timed_out = true;
                    effects.push(Effect::Kill { tag: whole.spawn });
                }
            }
        }
        self.schedule(effects);
    }

    fn tag(&mut self, purpose: For) -> Tag {
        let tag = Tag(self.config.tags.wrapping_add(self.next_tag));
        self.next_tag += 1;
        self.tags.insert(tag, purpose);
        tag
    }

    fn server(&mut self, b: usize) -> &mut Server {
        self.bins[b].server.as_mut().expect("a binary run as a process per test has a server")
    }

    fn file(&self, name: String) -> FileName {
        FileName::new(&format!("{}/{name}", self.config.dir)).expect("a name under the request's directory")
    }

    fn path(&self, file: &FileName) -> PathBuf {
        self.config.root.join(file.as_str())
    }

    fn start_server(&mut self, b: usize, effects: &mut Vec<Effect>) {
        let socket = self.file(format!("t{b}"));
        let record = self.file(format!("r{b}"));
        let server = Server {
            phase: Phase::Writing,
            socket_path: self.path(&socket).to_string_lossy().into_owned(),
            record_path: self.path(&record).to_string_lossy().into_owned(),
            socket,
            record: record.clone(),
            spawn: None,
            listen: None,
            conn: None,
            hung_up: false,
            lost: None,
            frames: Frames::new(),
            job_index: Vec::new(),
            by_scope: HashMap::new(),
            running: BTreeSet::new(),
            at: 0,
            partial: Vec::new(),
            scoped: Vec::new(),
            common: Vec::new(),
            common_seen: HashMap::new(),
            outbox: Vec::new(),
            timers: Vec::new(),
            dirty: false,
            reading: false,
            waiting: Vec::new(),
            next: Vec::new(),
            broken: None,
        };
        self.bins[b].server = Some(server);
        let tag = self.tag(For::Write(b));
        // Made empty, with its directory, before the sandbox appends to it.
        effects.push(Effect::Write { file: record, bytes: Vec::new(), tag });
    }

    fn written(&mut self, b: usize, done: Done, effects: &mut Vec<Effect>) {
        if let Done::Failed(error) = done {
            let why = format!("making the record of its sandbox, {}: {error}", self.server(b).record);
            self.server(b).phase = Phase::Ended;
            self.server_ended(b, why, Vec::new(), effects);
            return;
        }
        if self.bins[b].left == 0 {
            self.server(b).phase = Phase::Ended;
            return;
        }
        let plan = &self.bins[b].plan;
        let server = self.bins[b].server.as_ref().expect("a server");
        let mut env = plan.env.clone();
        env.push((wire::SOCKET.into(), server.socket_path.clone().into()));
        env.push((confine::LIVE.into(), server.record_path.clone().into()));
        let mut read = plan.read.clone();
        read.push(PathBuf::from(&server.socket_path));
        let command = Command {
            program: plan.program.clone(),
            args: Vec::new(),
            env,
            dir: plan.dir.clone(),
            stdin: Vec::new(),
            sandbox: Sandbox::On { read, write: plan.write.clone(), network: plan.network, trace: true },
            memory: plan.memory,
        };
        let at = Address::Unix(server.socket.clone());
        let listen = self.tag(For::Listen(b));
        let spawn = self.tag(For::Spawn(b));
        let server = self.server(b);
        server.phase = Phase::Starting;
        server.listen = Some(listen);
        server.spawn = Some(spawn);
        effects.push(Effect::Listen { at, tag: listen });
        effects.push(Effect::Spawn { command: Box::new(command), tag: spawn });
    }

    fn listened(&mut self, b: usize, tag: Tag, done: Done, effects: &mut Vec<Effect>) {
        match done {
            Done::Accepted { conn } => {
                let server = self.server(b);
                if server.conn.is_some() || server.phase != Phase::Starting {
                    let hangup = self.tag(For::Hangup);
                    effects.push(Effect::Hangup { conn, tag: hangup });
                    return;
                }
                server.conn = Some(conn);
                let recv = self.tag(For::Recv(b));
                effects.push(Effect::Recv { conn, most: RECV, tag: recv });
                effects.push(Effect::Unlisten { tag });
            }
            Done::Unlistened => {
                self.tags.remove(&tag);
                self.server(b).listen = None;
            }
            Done::Failed(error) => {
                self.tags.remove(&tag);
                self.server(b).listen = None;
                if self.server(b).conn.is_none() {
                    let socket = self.server(b).socket.clone();
                    self.kill_server(b, format!("listening on {socket}: {error}"), effects);
                }
            }
            _ => {}
        }
    }

    fn received(&mut self, b: usize, done: Done, effects: &mut Vec<Effect>) {
        let bytes = match done {
            Done::Read(bytes) => bytes,
            Done::Failed(error) => {
                self.kill_server(b, format!("reading from its fork server: {error}"), effects);
                return;
            }
            _ => return,
        };
        if bytes.is_empty() {
            let server = self.server(b);
            server.hung_up = true;
            if !matches!(server.phase, Phase::Quitting | Phase::Ended) {
                self.kill_server(b, "its fork server closed its socket".to_owned(), effects);
            }
            return;
        }
        self.server(b).frames.push_owned(bytes);
        loop {
            let frame = match self.server(b).frames.pop() {
                Ok(Some(frame)) => frame,
                Ok(None) => break,
                Err(why) => {
                    self.kill_server(b, format!("its fork server broke the protocol: {why}"), effects);
                    return;
                }
            };
            if let Err(why) = self.frame(b, frame, effects) {
                self.kill_server(b, format!("its fork server broke the protocol: {why}"), effects);
                return;
            }
        }
        let server = self.server(b);
        if let Some(conn) = server.conn
            && server.phase != Phase::Ended
        {
            let most = server.frames.needed().max(RECV);
            let recv = self.tag(For::Recv(b));
            effects.push(Effect::Recv { conn, most, tag: recv });
        }
    }

    fn frame(&mut self, b: usize, frame: Frame, effects: &mut Vec<Effect>) -> Result<(), String> {
        match frame {
            Frame::Hello { version, tests, .. } => {
                if self.server(b).phase != Phase::Starting {
                    return Err("a second hello".to_owned());
                }
                if version != wire::VERSION {
                    return Err(format!("its version is {version}, and blaze speaks {}", wire::VERSION));
                }
                let bin = &mut self.bins[b];
                let jobs: HashMap<&str, u32> =
                    bin.plan.jobs.iter().enumerate().map(|(j, it)| (it.name.as_str(), j as u32)).collect();
                let server = bin.server.as_mut().expect("a server");
                server.job_index = vec![NO_TEST; bin.plan.jobs.len()];
                server.scoped = vec![Vec::new(); bin.plan.jobs.len()];
                for (at, name) in tests.into_iter().enumerate() {
                    if let Some(&j) = jobs.get(name.as_str()) {
                        server.job_index[j as usize] = at as u32;
                        server.by_scope.insert(name, j);
                    }
                }
                server.phase = Phase::Ready;
                let mut missing = Vec::new();
                for entry in self.pending.iter().filter(|it| it.1 == b) {
                    match self.bins[b].server.as_ref().expect("a server").job_index[entry.2] {
                        NO_TEST => missing.push(*entry),
                        _ => _ = self.ready.insert(*entry),
                    }
                }
                for entry in missing {
                    self.pending.remove(&entry);
                    let why = format!("{} has no test named {}", self.bins[b].plan.name, self.bins[b].plan.jobs[entry.2].name);
                    self.finish(b, Some(entry.2), Outcome::Failed(Failure::Lost(why.clone())), Vec::new(), Duration::ZERO, Trace::Unknown(why));
                }
                self.maybe_quit(b, effects);
                Ok(())
            }
            Frame::Ended { id, end, output } => {
                let server = self.server(b);
                if !server.running.remove(&id) {
                    return Err(format!("an end of {id}, which it does not run"));
                }
                self.busy -= 1;
                let attempt = self.attempts.get(&id).expect("a test the server runs is an attempt");
                let j = attempt.job;
                let outcome = self.outcome(attempt, &end);
                if outcome != Outcome::Passed {
                    // Decided before a core is given to another test: a retry keeps its place in
                    // the order, and fail-fast starts nothing more.
                    if attempt.number <= self.bins[b].plan.jobs[j].retries && !self.stopping {
                        let number = attempt.number;
                        self.attempts.remove(&id);
                        let tries = self.tries.entry((b, j)).or_default();
                        tries.done = number;
                        tries.output.extend_from_slice(format!("--- attempt {number} ---\n").as_bytes());
                        tries.output.extend_from_slice(&output);
                        let entry = (Reverse(self.bins[b].plan.jobs[j].expected), b, j);
                        self.pending.insert(entry);
                        if self.server(b).phase == Phase::Ready {
                            self.ready.insert(entry);
                        }
                        return Ok(());
                    }
                    if self.config.fail_fast {
                        self.stop();
                    }
                }
                let ended = Ended { id, end, output, at: self.now };
                let server = self.server(b);
                match server.reading {
                    true => server.next.push(ended),
                    false => {
                        server.waiting.push(ended);
                        self.start_read(b, effects);
                    }
                }
                Ok(())
            }
            other => Err(format!("it sent {other:?}, which only blaze sends")),
        }
    }

    fn start_read(&mut self, b: usize, effects: &mut Vec<Effect>) {
        if let Some(why) = self.server(b).broken.clone() {
            for ended in std::mem::take(&mut self.server(b).waiting) {
                self.attempt_ended(b, ended, Trace::Unknown(why.clone()), effects);
            }
            return;
        }
        let tag = self.tag(For::Read(b));
        let server = self.server(b);
        server.reading = true;
        effects.push(Effect::Read { file: server.record.clone(), at: server.at, len: CHUNK, tag });
    }

    fn read(&mut self, b: usize, done: Done, effects: &mut Vec<Effect>) {
        let server = self.server(b);
        server.reading = false;
        match done {
            Done::Read(bytes) => {
                let len = bytes.len();
                server.at += len as u64;
                match server.partial.is_empty() {
                    true => server.partial = bytes,
                    false => server.partial.extend_from_slice(&bytes),
                }
                if let Some(end) = server.partial.iter().rposition(|it| *it == b'\n') {
                    let partial = std::mem::take(&mut server.partial);
                    if let Err(bad) = take_lines(server, &partial[..=end]) {
                        server.broken = Some(format!("the sandbox's record {}: {bad}", server.record));
                    }
                    server.partial = partial;
                    server.partial.drain(..=end);
                }
                if len == CHUNK && server.broken.is_none() {
                    self.start_read(b, effects);
                    return;
                }
            }
            Done::Failed(error) => {
                server.broken = Some(format!("reading the sandbox's record {}: {error}", server.record));
            }
            _ => {}
        }
        let broken = self.server(b).broken.clone();
        let waiting = std::mem::take(&mut self.server(b).waiting);
        for ended in waiting {
            let Some(j) = self.attempts.get(&ended.id).map(|it| it.job) else { continue };
            let trace = match &broken {
                Some(why) => Trace::Unknown(why.clone()),
                None => Trace::Lines(self.lines(b, j)),
            };
            self.attempt_ended(b, ended, trace, effects);
        }
        let server = self.server(b);
        if !server.next.is_empty() {
            server.waiting = std::mem::take(&mut server.next);
            self.start_read(b, effects);
        }
    }

    /// The lines of the record a test of `b` read: those of every scope, then its own, which
    /// are moved out (only its last attempt's result asks).
    fn lines(&mut self, b: usize, j: usize) -> Vec<Line> {
        let server = self.server(b);
        let own = server.scoped.get_mut(j).map(std::mem::take).unwrap_or_default();
        let mut lines = Vec::with_capacity(server.common.len() + own.len());
        lines.extend_from_slice(&server.common);
        lines.extend(own);
        lines
    }

    /// How a run of a test that ended as `end` went.
    fn outcome(&self, attempt: &Attempt, end: &End) -> Outcome {
        let timeout = self.bins[attempt.binary].plan.jobs[attempt.job].timeout;
        match (end, attempt.timed_out) {
            (_, true) => Outcome::Failed(Failure::TimedOut(timeout)),
            (End::Exited(0), false) => Outcome::Passed,
            (End::Exited(code), false) => Outcome::Failed(Failure::Code(*code)),
            (End::Signaled(signal), false) => Outcome::Failed(Failure::Signal(*signal)),
            (End::NotStarted(why), false) => Outcome::Failed(Failure::Lost(why.clone())),
        }
    }

    /// A test whose run ended and is not run again, once what it read is known.
    fn attempt_ended(&mut self, b: usize, ended: Ended, trace: Trace, effects: &mut Vec<Effect>) {
        let Some(attempt) = self.attempts.remove(&ended.id) else { return };
        let j = attempt.job;
        let outcome = self.outcome(&attempt, &ended.end);
        let took = ended.at.saturating_sub(attempt.started);
        let output = match self.tries.remove(&(b, j)) {
            Some(mut tries) => {
                tries.output.extend_from_slice(format!("--- attempt {} ---\n", attempt.number).as_bytes());
                tries.output.extend_from_slice(&ended.output);
                tries.output
            }
            None => ended.output,
        };
        let outcome = if self.cancelled && outcome != Outcome::Passed { Outcome::NotRun } else { outcome };
        self.finish_attempts(b, Some(j), outcome, attempt.number, output, took, trace);
        self.maybe_quit(b, effects);
    }

    fn finish(&mut self, b: usize, job: Option<usize>, outcome: Outcome, output: Vec<u8>, took: Duration, trace: Trace) {
        self.finish_attempts(b, job, outcome, 1, output, took, trace);
    }

    fn finish_attempts(
        &mut self,
        b: usize,
        job: Option<usize>,
        outcome: Outcome,
        attempts: u32,
        output: Vec<u8>,
        took: Duration,
        trace: Trace,
    ) {
        let failed = matches!(outcome, Outcome::Failed(_));
        let attempts = match outcome {
            Outcome::NotRun => attempts.saturating_sub(1),
            _ => attempts,
        };
        self.results.push(Finished {
            binary: b,
            test: job.map(|j| self.bins[b].plan.jobs[j].name.clone()),
            outcome,
            attempts,
            output,
            took,
            trace,
        });
        self.bins[b].left -= 1;
        if failed && self.config.fail_fast {
            self.stop();
        }
    }

    /// No test starts from now on: those not started are not run.
    fn stop(&mut self) {
        if self.stopping {
            return;
        }
        self.stopping = true;
        self.ready.clear();
        for (_, b, j) in std::mem::take(&mut self.pending) {
            let job = (j != WHOLE).then_some(j);
            let tries = self.tries.remove(&(b, j)).unwrap_or_default();
            let why = "not run: an earlier failure or a cancel stopped the run".to_owned();
            self.finish_attempts(b, job, Outcome::NotRun, tries.done + 1, tries.output, Duration::ZERO, Trace::Unknown(why));
        }
    }

    /// A server with nothing left to run is told to quit; one not yet ready is killed.
    fn maybe_quit(&mut self, b: usize, effects: &mut Vec<Effect>) {
        if self.bins[b].left != 0 {
            return;
        }
        let Some(server) = &self.bins[b].server else { return };
        match server.phase {
            Phase::Ready => {
                self.send(b, &Frame::Quit);
                self.server(b).phase = Phase::Quitting;
            }
            Phase::Starting if server.lost.is_none() => {
                if let Some(spawn) = server.spawn {
                    effects.push(Effect::Kill { tag: spawn });
                    self.server(b).lost = Some("nothing was left for it to run".to_owned());
                }
            }
            Phase::Starting => {}
            Phase::Writing | Phase::Quitting | Phase::Ended => {}
        }
    }

    /// `frame` to the server, in its outbox until the call ends ([`Queue::flush`]).
    fn send(&mut self, b: usize, frame: &Frame) {
        let server = self.server(b);
        if server.conn.is_none() || server.hung_up {
            return;
        }
        frame.encode(&mut server.outbox);
        self.dirty(b);
    }

    /// A timer that goes to the driver after the frames sent to `b` so far.
    fn timer(&mut self, b: usize, after: Duration, tag: Tag) {
        self.server(b).timers.push(Effect::Timer { after, tag });
        self.dirty(b);
    }

    fn dirty(&mut self, b: usize) {
        let server = self.server(b);
        if !server.dirty {
            server.dirty = true;
            self.dirty.push(b);
        }
    }

    /// What this call said to each server, as one send of all its frames, then its timers.
    fn flush(&mut self, effects: &mut Vec<Effect>) {
        for b in std::mem::take(&mut self.dirty) {
            self.flush_server(b, effects);
        }
    }

    fn flush_server(&mut self, b: usize, effects: &mut Vec<Effect>) {
        let Some(server) = self.bins[b].server.as_mut() else { return };
        server.dirty = false;
        let bytes = std::mem::take(&mut server.outbox);
        let timers = std::mem::take(&mut server.timers);
        let conn = server.conn.filter(|_| !server.hung_up);
        if let Some(conn) = conn
            && !bytes.is_empty()
        {
            let tag = self.tag(For::Send(b));
            effects.push(Effect::Send { conn, bytes, tag });
        }
        effects.extend(timers);
    }

    /// Kills the server, so its tests are lost with `why` once its spawn ends.
    fn kill_server(&mut self, b: usize, why: String, effects: &mut Vec<Effect>) {
        let Some(server) = &mut self.bins[b].server else { return };
        if server.phase == Phase::Ended {
            return;
        }
        server.lost.get_or_insert(why.clone());
        match server.spawn {
            Some(spawn) => effects.push(Effect::Kill { tag: spawn }),
            // Its record is still being made: nothing was started.
            None => {
                server.phase = Phase::Ended;
                self.server_ended(b, why, Vec::new(), effects);
            }
        }
    }

    /// The server's process ended (or never started): what it ran and what was left for it
    /// are lost with `why`.
    fn server_ended(&mut self, b: usize, why: String, output: Vec<u8>, effects: &mut Vec<Effect>) {
        // What was said to it goes before the hang-up, as it was said.
        self.flush_server(b, effects);
        let server = self.server(b);
        let was = server.phase;
        server.phase = Phase::Ended;
        if let Some(conn) = server.conn
            && !server.hung_up
        {
            server.hung_up = true;
            let tag = self.tag(For::Hangup);
            effects.push(Effect::Hangup { conn, tag });
        }
        if let Some(listen) = self.server(b).listen {
            effects.push(Effect::Unlisten { tag: listen });
        }
        let running: Vec<u64> = std::mem::take(&mut self.server(b).running).into_iter().collect();
        let why = match was {
            Phase::Quitting if running.is_empty() => return,
            _ => format!("{}: {why}", self.bins[b].plan.name),
        };
        for id in running {
            self.busy -= 1;
            let Some(attempt) = self.attempts.remove(&id) else { continue };
            let outcome = match self.cancelled {
                true => Outcome::NotRun,
                false => Outcome::Failed(Failure::Lost(why.clone())),
            };
            let output = [self.tries.remove(&(b, attempt.job)).map(|it| it.output).unwrap_or_default(), output.clone()].concat();
            let took = self.now.saturating_sub(attempt.started);
            self.finish_attempts(b, Some(attempt.job), outcome, attempt.number, output, took, Trace::Unknown(why.clone()));
        }
        let left: Vec<(Reverse<Duration>, usize, usize)> = self.pending.iter().filter(|it| it.1 == b).copied().collect();
        for entry in left {
            self.pending.remove(&entry);
            self.ready.remove(&entry);
            let earlier = self.tries.remove(&(b, entry.2)).map(|it| it.output).unwrap_or_default();
            self.finish(b, Some(entry.2), Outcome::Failed(Failure::Lost(why.clone())), [earlier, output.clone()].concat(), Duration::ZERO, Trace::Unknown(why.clone()));
        }
    }

    /// Starts the first tests in the order whose servers are ready, while cores are free.
    /// Servers with nothing left to run are let go.
    fn schedule(&mut self, effects: &mut Vec<Effect>) {
        while !self.stopping && self.busy < self.config.jobs.max(1) {
            let Some(entry) = self.ready.pop_first() else { break };
            self.pending.remove(&entry);
            let (_, b, j) = entry;
            match j {
                WHOLE => self.start_whole(b, effects),
                _ => self.start_job(b, j),
            }
        }
        for b in 0..self.bins.len() {
            self.maybe_quit(b, effects);
        }
    }

    fn start_job(&mut self, b: usize, j: usize) {
        let id = self.next_id;
        self.next_id += 1;
        let number = self.tries.get(&(b, j)).map_or(1, |it| it.done + 1);
        let timeout = self.bins[b].plan.jobs[j].timeout;
        let index = self.server(b).job_index[j];
        self.attempts.insert(id, Attempt { binary: b, job: j, number, started: self.now, timed_out: false });
        self.server(b).running.insert(id);
        self.busy += 1;
        self.send(b, &Frame::Run { id, index });
        let deadline = self.tag(For::Deadline(id));
        self.timer(b, timeout, deadline);
    }

    fn start_whole(&mut self, b: usize, effects: &mut Vec<Effect>) {
        let plan = &self.bins[b].plan;
        let Mode::Threads { args: given, timeout } = &plan.mode else { unreachable!("a binary run whole is in threads mode") };
        let slots = self.config.jobs.max(1).saturating_sub(self.busy).min(plan.jobs.len()).max(1);
        let mut args: Vec<OsString> = plan.jobs.iter().map(|it| OsString::from(&it.name)).collect();
        args.push("--exact".into());
        args.push(format!("--test-threads={slots}").into());
        if plan.jobs.iter().any(|it| it.ignore) {
            args.push("--include-ignored".into());
        }
        args.extend(given.iter().cloned());
        let command = Command {
            program: plan.program.clone(),
            args,
            env: plan.env.clone(),
            dir: plan.dir.clone(),
            stdin: Vec::new(),
            sandbox: Sandbox::On { read: plan.read.clone(), write: plan.write.clone(), network: plan.network, trace: true },
            memory: plan.memory,
        };
        let timeout = *timeout;
        let spawn = self.tag(For::Whole(b));
        let deadline = self.tag(For::WholeDeadline(b));
        self.busy += slots;
        self.bins[b].whole = Some(Whole { spawn, started: self.now, slots, timed_out: false });
        effects.push(Effect::Spawn { command: Box::new(command), tag: spawn });
        effects.push(Effect::Timer { after: timeout, tag: deadline });
    }

    fn whole_ended(&mut self, b: usize, done: Done) {
        let Some(whole) = self.bins[b].whole.take() else { return };
        self.busy -= whole.slots;
        let took = self.now.saturating_sub(whole.started);
        let timeout = match &self.bins[b].plan.mode {
            Mode::Threads { timeout, .. } => *timeout,
            Mode::Process => Duration::ZERO,
        };
        let (outcome, output, trace) = match done {
            Done::Exited(exit) => {
                let exit = *exit;
                let outcome = match (whole.timed_out, exit.code, exit.signal) {
                    (true, _, _) => Outcome::Failed(Failure::TimedOut(timeout)),
                    (false, Some(0), _) => Outcome::Passed,
                    (false, Some(code), _) => Outcome::Failed(Failure::Code(code)),
                    (false, None, signal) => Outcome::Failed(Failure::Signal(signal.unwrap_or(0))),
                };
                let trace = match exit.traced {
                    Some(traced) => Trace::Whole(traced),
                    None => Trace::Unknown("its sandbox kept no record of what it read".to_owned()),
                };
                (outcome, [exit.stdout, exit.stderr].concat(), trace)
            }
            Done::Failed(error) => {
                let why = format!("{} could not be started in its sandbox: {error}", self.bins[b].plan.program.display());
                (Outcome::Failed(Failure::Lost(why.clone())), Vec::new(), Trace::Unknown(why))
            }
            other => {
                let why = format!("its spawn completed as {other:?}");
                (Outcome::Failed(Failure::Lost(why.clone())), Vec::new(), Trace::Unknown(why))
            }
        };
        let outcome = if self.cancelled && outcome != Outcome::Passed { Outcome::NotRun } else { outcome };
        self.finish(b, None, outcome, output, took, trace);
    }
}

/// Whether a line of the record is of blaze's own doing, not the tests': its socket, its
/// record, the variables it set for the server.
fn ours(server: &Server, seen: Seen, what: &str) -> bool {
    match seen {
        Seen::Env => what == wire::SOCKET || what == confine::LIVE,
        Seen::Read | Seen::Looked | Seen::Listed => what == server.socket_path || what == server.record_path,
        _ => false,
    }
}

/// A line of the record without its line break, borrowed: the parts [`Line::parse`] reads.
fn record_line(text: &str) -> Option<(Seen, &str, &str)> {
    let mut parts = text.splitn(3, '\t');
    let seen = Seen::of(parts.next()?)?;
    Some((seen, parts.next()?, parts.next()?))
}

/// The whole lines of `text` (which ends with a line break) into the server's lines, as
/// [`trace::parse`] of its lossy text reads them, but in place: each line is read borrowed,
/// and only one that is kept is made a [`Line`]. A line not UTF-8 is read lossily alone,
/// which is the same: a replaced sequence never spans a line break.
fn take_lines(server: &mut Server, text: &[u8]) -> Result<(), BadRecord> {
    for (number, piece) in text.split_inclusive(|it| *it == b'\n').enumerate() {
        let Some(piece) = piece.strip_suffix(b"\n") else { break };
        let piece = match std::str::from_utf8(piece) {
            Ok(it) => Cow::Borrowed(it),
            Err(_) => Cow::Owned(String::from_utf8_lossy(piece).into_owned()),
        };
        let Some((seen, scope, what)) = record_line(&piece) else {
            return Err(BadRecord { line: number + 1, text: piece.chars().take(200).collect() });
        };
        if ours(server, seen, what) {
            continue;
        }
        if scope.is_empty() {
            let bit = 1u16 << seen as u16;
            match server.common_seen.get_mut(what) {
                Some(kinds) if *kinds & bit != 0 => continue,
                Some(kinds) => *kinds |= bit,
                None => _ = server.common_seen.insert(what.to_owned(), bit),
            }
            server.common.push(Line { seen, scope: String::new(), what: what.to_owned() });
            continue;
        }
        // A scope no job of this server has is no test's to be given.
        if let Some(&j) = server.by_scope.get(scope) {
            server.scoped[j as usize].push(Line { seen, scope: scope.to_owned(), what: what.to_owned() });
        }
    }
    Ok(())
}

/// How a server's process ended, for the tests it loses.
fn ended(exit: &Exit) -> String {
    match (exit.code, exit.signal) {
        (Some(code), _) => format!("its fork server exited with code {code}"),
        (None, Some(signal)) => format!("its fork server was ended by signal {signal}"),
        (None, None) => "its fork server ended".to_owned(),
    }
}

#[cfg(test)]
mod tests;

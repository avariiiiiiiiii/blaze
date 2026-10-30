//! One global queue of tests (#1443): a sans-IO state machine over every test binary of a
//! request, running children from every binary on all cores.
//!
//! - **A process per test, from a fork server.** Each binary starts once, in its sandbox
//!   (tracing what it reads), as a fork server ([`super::fork_server`]): blaze writes an empty
//!   record for its sandbox, listens on a Unix socket of its own (`Effect::Listen`), and
//!   spawns the binary with the socket's path in [`wire::SOCKET`] and the record's in
//!   [`confine::LIVE`]. The server connects and says hello with its tests; for each test the
//!   queue sends a run, the server forks a child that marks its scope and runs the one test,
//!   and answers how it ended with its output and what it executed ([`wire`]). The lines the
//!   sandbox appended to the record under the test's scope (and under none) are what the test
//!   read ([`Trace::Lines`]); the record is read after each answer, so every line of the child
//!   is in it.
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
//! The hot path, a test started and ended, allocates nothing past its result: a test is a
//! `u32` unit indexing columns, the order is bitsets over ranks, attempts and tags are slabs
//! reused through free lists, a record's paths and variables are interned once per server and
//! its lines kept as `u32`s, and every deadline is one timer wheel ([`wheel`]) behind one
//! driver timer at its next expiry.
//!
//! The queue asks with tags `tags..tags + 2^32` ([`Config::tags`]). It reads no clock: the
//! driver's `now` comes with each call. It is [`Queue::finished`] once every test has a result
//! and every process it started has ended; timers still in flight then are stale and their
//! completions are dropped.

use std::{borrow::Cow, ffi::OsString, io::Write as _, path::PathBuf, time::Duration};

use rustc_hash::FxHashMap;

use blaze_core::{
    Address, Command, Completion, Conn, Done, Effect, Exit, FileName, Sandbox, Tag, Traced,
};
use blaze_ids::{ContentId, radix};

use super::wire::{self, End, Executed, Frame, Frames};
use crate::{
    confine,
    trace::{self, BadRecord, Inputs, Line, Seen},
};

mod names;
mod wheel;

use names::Names;
use wheel::{UNSET, Wheel};

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
    Lines(Lines),
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
            Trace::Lines(lines) => {
                let lines: Vec<Line> = lines
                    .iter()
                    .map(|(seen, what)| Line { seen, scope: String::new(), what: what.to_owned() })
                    .collect();
                trace::assemble(&lines, None, inputs).map(Some)
            }
            Trace::Whole(traced) => Ok(Some(traced.clone())),
            Trace::Unknown(_) => Ok(None),
        }
    }
}

/// The lines a test of a fork server read, each its kind and what it names, the names in one
/// buffer: those of every scope first, each kind and name once, then the test's own. Scopes
/// are left out: every line is of the test or of none, and [`trace::assemble`] reads both.
#[derive(Clone, PartialEq, Eq, Default, Debug)]
pub struct Lines {
    text: String,
    /// Each line's kind and the end of what it names in `text`.
    lines: Vec<(Seen, u32)>,
}

impl Lines {
    pub fn new() -> Lines {
        Lines::default()
    }

    /// A line of `seen` naming `what`.
    ///
    /// # Panics
    ///
    /// Past 4 GiB of names.
    pub fn push(&mut self, seen: Seen, what: &str) {
        assert!(self.try_push(seen, what), "the lines of a test are past 4 GiB");
    }

    /// `false`, and nothing pushed, past 4 GiB of names.
    fn try_push(&mut self, seen: Seen, what: &str) -> bool {
        let Ok(end) = u32::try_from(self.text.len() + what.len()) else { return false };
        self.text.push_str(what);
        self.lines.push((seen, end));
        true
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Each line, borrowed, in order.
    pub fn iter(&self) -> impl Iterator<Item = (Seen, &str)> + '_ {
        let mut start = 0;
        self.lines.iter().map(move |&(seen, end)| {
            let what = &self.text[start..end as usize];
            start = end as usize;
            (seen, what)
        })
    }
}

impl<'a> FromIterator<(Seen, &'a str)> for Lines {
    fn from_iter<T: IntoIterator<Item = (Seen, &'a str)>>(iter: T) -> Lines {
        let mut lines = Lines::new();
        for (seen, what) in iter {
            lines.push(seen, what);
        }
        lines
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
    /// The jump-table slots its last attempt executed, as its fork server read them, or why
    /// they are not known.
    pub executed: Executed,
}

/// The size of each read of a record.
const CHUNK: usize = 1 << 20;
/// The job of a unit that is a binary run whole.
const WHOLE: u32 = u32::MAX;
/// The least a receive from a fork server asks for; more when a longer frame is coming.
const RECV: usize = 1 << 16;
/// A job its server's hello did not name.
const NO_TEST: u32 = u32::MAX;
/// What a binary run whole executed: no fork server, so no region.
const WHOLE_UNRECORDED: &str = "run whole as libtest's threads";
/// Said of every test a stop leaves unstarted.
const STOPPED: &str = "not run: an earlier failure or a cancel stopped the run";
/// The wheel's tick, in nanoseconds.
const TICK: u128 = 1_000_000;

/// What a tag of the queue is for, by its slot in the tag slab.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum For {
    /// A slot not in use.
    Free,
    Write(u32),
    Listen(u32),
    Spawn(u32),
    Recv(u32),
    Send(u32),
    Hangup,
    Read(u32),
    /// The driver timer at the wheel's next expiry.
    Tick,
    Whole(u32),
}

/// What a timer of the wheel is for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Due {
    /// An attempt past its timeout, by its id.
    Deadline(u64),
    /// An attempt not ended within the kill grace.
    Grace(u64),
    /// A binary run whole past its timeout.
    Whole(u32),
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
    executed: Executed,
    at: Duration,
}

struct Server {
    phase: Phase,
    socket: FileName,
    record: FileName,
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
    /// The hello's names that are jobs', and the job each is, by its ID: a record line's
    /// scope is one hash of the borrowed text.
    scopes: Names,
    scope_job: Vec<u32>,
    /// What the record's lines name, interned.
    names: Names,
    /// The IDs in `names` of its socket's and its record's paths (as the record names them)
    /// and of the two variables blaze set for it: the lines that are blaze's own.
    ours: [u32; 4],
    /// The tests it runs now, by id, in the order they started: those a lost server takes
    /// end in that order.
    running: Vec<u64>,
    /// How far the record has been read, and a last line not yet whole.
    at: u64,
    partial: Vec<u8>,
    /// Each job's lines ([`pack`]), by job, taken by its last attempt's result.
    scoped: Vec<Vec<u32>>,
    /// The lines of every scope ([`pack`]), each once in the order first seen, and the kinds
    /// of [`Seen`] each name has among them as bits, by name: a line every test is given is
    /// kept only once however often the server repeats it.
    common: Vec<u32>,
    common_kinds: Vec<u8>,
    /// Frames for the server not yet given to the driver: what one call to the queue says to
    /// a server goes as one send.
    outbox: Vec<u8>,
    dirty: bool,
    reading: bool,
    waiting: Vec<Ended>,
    next: Vec<Ended>,
    /// Why the record can't be read: every test's trace is unknown.
    broken: Option<String>,
}

/// One run of a test, in the attempt slab: its id is its generation and its slot.
#[derive(Clone, Copy)]
struct Attempt {
    unit: u32,
    number: u32,
    generation: u32,
    live: bool,
    /// Sent to its server, and not yet said to have ended.
    running: bool,
    timed_out: bool,
    started: Duration,
    /// Its timers in the wheel, or [`UNSET`].
    deadline: u64,
    grace: u64,
}

struct Whole {
    spawn: Tag,
    started: Duration,
    slots: usize,
    timed_out: bool,
    deadline: u64,
}

struct Bin {
    plan: Binary,
    /// Units without a final result.
    left: usize,
    server: Option<Server>,
    whole: Option<Whole>,
    /// Its units: `first..first + units`.
    first: u32,
    units: u32,
}

/// A set of `u32` positions as bits, with the lowest word that may have one, so the first is
/// found by a scan that only moves forward until a lower bit is set again.
#[derive(Clone, Debug)]
struct Bits {
    words: Vec<u64>,
    low: usize,
}

impl Bits {
    fn empty(len: usize) -> Bits {
        let words = vec![0; len.div_ceil(64)];
        Bits { low: words.len(), words }
    }

    fn full(len: usize) -> Bits {
        let mut words = vec![u64::MAX; len.div_ceil(64)];
        if let Some(last) = words.last_mut()
            && len % 64 != 0
        {
            *last = (1 << (len % 64)) - 1;
        }
        Bits { words, low: 0 }
    }

    fn has(&self, at: u32) -> bool {
        self.words[at as usize / 64] & (1 << (at % 64)) != 0
    }

    fn set(&mut self, at: u32) {
        let word = at as usize / 64;
        self.words[word] |= 1 << (at % 64);
        self.low = self.low.min(word);
    }

    fn clear(&mut self, at: u32) {
        self.words[at as usize / 64] &= !(1 << (at % 64));
    }

    fn clear_all(&mut self) {
        self.words.fill(0);
        self.low = self.words.len();
    }

    fn first(&mut self) -> Option<u32> {
        while self.low < self.words.len() {
            let word = self.words[self.low];
            if word != 0 {
                return Some((self.low * 64) as u32 + word.trailing_zeros());
            }
            self.low += 1;
        }
        None
    }

    /// The positions set, in order.
    fn ones(&self) -> impl Iterator<Item = u32> + '_ {
        self.words.iter().enumerate().flat_map(|(at, &word)| {
            let mut left = word;
            std::iter::from_fn(move || {
                let bit = (left != 0).then(|| left.trailing_zeros())?;
                left &= left - 1;
                Some((at * 64) as u32 + bit)
            })
        })
    }
}

/// How a unit's last attempt went, for its result.
struct Went {
    outcome: Outcome,
    attempts: u32,
    output: Vec<u8>,
    took: Duration,
    trace: Trace,
    executed: Executed,
}

impl Went {
    /// Lost before it ran, and why.
    fn lost(why: String, output: Vec<u8>) -> Went {
        Went {
            outcome: Outcome::Failed(Failure::Lost(why.clone())),
            attempts: 1,
            output,
            took: Duration::ZERO,
            trace: Trace::Unknown(why.clone()),
            executed: Executed::Not(why),
        }
    }
}

pub struct Queue {
    config: Config,
    bins: Vec<Bin>,
    /// Each unit's binary and job ([`WHOLE`] for a binary run whole): a unit is a job of a
    /// binary run as a process per test, or a binary run whole, numbered binary by binary.
    unit_bin: Vec<u32>,
    unit_job: Vec<u32>,
    /// Every unit in the order they start, the longest expected first and then by unit, and
    /// each unit's place in it: `order[rank[unit]] == unit`.
    order: Vec<u32>,
    rank: Vec<u32>,
    /// By rank: not started yet; and of those, what can start now (its server said hello, or
    /// it is a binary run whole).
    pending: Bits,
    ready: Bits,
    /// By unit, cold: how many earlier attempts it had (retries), and their output.
    tries: Vec<u32>,
    tries_output: Vec<Vec<u8>>,
    attempts: Vec<Attempt>,
    free_attempts: Vec<u32>,
    /// What each tag is for, by `tag - tags`, and the slots free.
    tags: Vec<For>,
    free_tags: Vec<u32>,
    wheel: Wheel<Due>,
    fired: Vec<Due>,
    /// The driver timer at the wheel's next expiry, and the tick it is for.
    armed: Option<(Tag, u64)>,
    /// Servers with frames in their outbox.
    dirty: Vec<u32>,
    /// Binaries left with nothing to run, to let go.
    quit: Vec<u32>,
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
            bins: Vec::with_capacity(binaries.len()),
            unit_bin: Vec::new(),
            unit_job: Vec::new(),
            order: Vec::new(),
            rank: Vec::new(),
            pending: Bits::empty(0),
            ready: Bits::empty(0),
            tries: Vec::new(),
            tries_output: Vec::new(),
            attempts: Vec::new(),
            free_attempts: Vec::new(),
            tags: Vec::new(),
            free_tags: Vec::new(),
            wheel: Wheel::new(tick(now)),
            fired: Vec::new(),
            armed: None,
            dirty: Vec::new(),
            quit: Vec::new(),
            busy: 0,
            stopping: false,
            cancelled: false,
            now,
            results: Vec::new(),
        };
        // Sorted on the longest first: the complement of each unit's expected nanoseconds.
        let mut keyed: Vec<(u64, u32)> = Vec::new();
        for (b, plan) in binaries.into_iter().enumerate() {
            let first = queue.unit_bin.len() as u32;
            let b = u32::try_from(b).expect("at most 2^32 binaries");
            match plan.mode {
                Mode::Threads { .. } if !plan.jobs.is_empty() => {
                    let expected: Duration = plan.jobs.iter().map(|it| it.expected).sum();
                    keyed.push((!nanos(expected), queue.unit_bin.len() as u32));
                    queue.unit_bin.push(b);
                    queue.unit_job.push(WHOLE);
                }
                Mode::Threads { .. } => {}
                Mode::Process => {
                    for (j, job) in plan.jobs.iter().enumerate() {
                        keyed.push((!nanos(job.expected), queue.unit_bin.len() as u32));
                        queue.unit_bin.push(b);
                        queue.unit_job.push(j as u32);
                    }
                }
            }
            assert!(queue.unit_bin.len() < WHOLE as usize, "at most 2^32 - 1 tests");
            let units = queue.unit_bin.len() as u32 - first;
            queue.bins.push(Bin { plan, left: units as usize, server: None, whole: None, first, units });
        }
        radix::sort_pairs(&mut keyed);
        let count = keyed.len();
        queue.order = keyed.iter().map(|it| it.1).collect();
        queue.rank = vec![0; count];
        for (at, &unit) in queue.order.iter().enumerate() {
            queue.rank[unit as usize] = at as u32;
        }
        queue.pending = Bits::full(count);
        queue.ready = Bits::empty(count);
        queue.tries = vec![0; count];
        queue.tries_output = vec![Vec::new(); count];
        for b in 0..queue.bins.len() {
            if queue.bins[b].left == 0 {
                continue;
            }
            match queue.bins[b].plan.mode {
                Mode::Threads { .. } => {
                    let rank = queue.rank[queue.bins[b].first as usize];
                    queue.ready.set(rank);
                }
                Mode::Process if !cfg!(unix) => {
                    let why = format!(
                        "{}: this platform has no fork, so no fork server: its package must run its tests as libtest's threads",
                        queue.bins[b].plan.name
                    );
                    let Bin { first, units, .. } = queue.bins[b];
                    for unit in first..first + units {
                        queue.pending.clear(queue.rank[unit as usize]);
                        queue.finish(unit, Went::lost(why.clone(), Vec::new()));
                    }
                }
                Mode::Process => queue.start_server(b, effects),
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
        self.let_go(effects);
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
        let Some(purpose) = self.purpose(tag) else { return };
        if !matches!(purpose, For::Listen(_)) {
            self.release_tag(tag);
        }
        match purpose {
            For::Free => {}
            For::Write(b) => self.written(b as usize, completion.done, effects),
            For::Listen(b) => self.listened(b as usize, tag, completion.done, effects),
            For::Spawn(b) => {
                let b = b as usize;
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
            For::Recv(b) => self.received(b as usize, completion.done, effects),
            For::Send(b) => {
                if let Done::Failed(error) = completion.done {
                    self.kill_server(b as usize, format!("sending to its fork server: {error}"), effects);
                }
            }
            For::Hangup => {}
            For::Read(b) => self.read(b as usize, completion.done, effects),
            For::Tick => {
                if self.armed.is_some_and(|it| it.0 == tag) {
                    self.armed = None;
                }
            }
            For::Whole(b) => self.whole_ended(b as usize, completion.done),
        }
        self.expire(effects);
        self.schedule(effects);
    }

    fn tag(&mut self, purpose: For) -> Tag {
        let at = match self.free_tags.pop() {
            Some(at) => {
                self.tags[at as usize] = purpose;
                at
            }
            None => {
                let at = u32::try_from(self.tags.len()).expect("at most 2^32 tags in flight");
                self.tags.push(purpose);
                at
            }
        };
        Tag(self.config.tags.wrapping_add(u64::from(at)))
    }

    fn purpose(&self, tag: Tag) -> Option<For> {
        let at = usize::try_from(tag.0.wrapping_sub(self.config.tags)).ok()?;
        self.tags.get(at).copied().filter(|it| *it != For::Free)
    }

    fn release_tag(&mut self, tag: Tag) {
        let at = tag.0.wrapping_sub(self.config.tags) as u32;
        self.tags[at as usize] = For::Free;
        self.free_tags.push(at);
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

    /// The slot of the live attempt `id`, if it is one.
    fn attempt(&self, id: u64) -> Option<usize> {
        let at = id as u32 as usize;
        let attempt = self.attempts.get(at)?;
        (attempt.live && attempt.generation == (id >> 32) as u32).then_some(at)
    }

    /// A new attempt of `unit`, by its id.
    fn new_attempt(&mut self, unit: u32, number: u32) -> u64 {
        let at = match self.free_attempts.pop() {
            Some(at) => at,
            None => {
                let at = u32::try_from(self.attempts.len()).expect("at most 2^32 tests at once");
                self.attempts.push(Attempt {
                    unit,
                    number,
                    generation: 0,
                    live: false,
                    running: false,
                    timed_out: false,
                    started: self.now,
                    deadline: UNSET,
                    grace: UNSET,
                });
                at
            }
        };
        let attempt = &mut self.attempts[at as usize];
        *attempt = Attempt {
            unit,
            number,
            generation: attempt.generation,
            live: true,
            running: false,
            timed_out: false,
            started: self.now,
            deadline: UNSET,
            grace: UNSET,
        };
        (u64::from(attempt.generation) << 32) | u64::from(at)
    }

    /// The attempt in slot `at` let go, with its timers.
    fn free_attempt(&mut self, at: usize) {
        let attempt = &mut self.attempts[at];
        attempt.live = false;
        attempt.generation = attempt.generation.wrapping_add(1);
        let timers = [attempt.deadline, attempt.grace];
        for timer in timers {
            self.wheel.remove(timer);
        }
        self.free_attempts.push(at as u32);
    }

    /// The timers due by now, acted on.
    fn expire(&mut self, effects: &mut Vec<Effect>) {
        let mut fired = std::mem::take(&mut self.fired);
        self.wheel.advance(tick(self.now), &mut fired);
        for due in fired.drain(..) {
            match due {
                Due::Deadline(id) => {
                    let Some(at) = self.attempt(id) else { continue };
                    let attempt = &mut self.attempts[at];
                    attempt.deadline = UNSET;
                    if !attempt.running || attempt.timed_out {
                        continue;
                    }
                    attempt.timed_out = true;
                    let b = self.unit_bin[attempt.unit as usize] as usize;
                    self.send(b, &Frame::Kill { id });
                    let grace = self.wheel.insert(due_tick(self.now, self.config.kill_grace), Due::Grace(id));
                    self.attempts[at].grace = grace;
                }
                Due::Grace(id) => {
                    let Some(at) = self.attempt(id) else { continue };
                    self.attempts[at].grace = UNSET;
                    if !self.attempts[at].running {
                        continue;
                    }
                    let unit = self.attempts[at].unit as usize;
                    let (b, j) = (self.unit_bin[unit] as usize, self.unit_job[unit] as usize);
                    let why = format!(
                        "{} did not end {:?} after it was killed, so its fork server was killed",
                        self.bins[b].plan.jobs[j].name, self.config.kill_grace
                    );
                    self.kill_server(b, why, effects);
                }
                Due::Whole(b) => {
                    if let Some(whole) = &mut self.bins[b as usize].whole
                        && !whole.timed_out
                    {
                        whole.deadline = UNSET;
                        whole.timed_out = true;
                        effects.push(Effect::Kill { tag: whole.spawn });
                    }
                }
            }
        }
        self.fired = fired;
    }

    fn start_server(&mut self, b: usize, effects: &mut Vec<Effect>) {
        let socket = self.file(format!("t{b}"));
        let record = self.file(format!("r{b}"));
        let socket_path = self.path(&socket).to_string_lossy().into_owned();
        let record_path = self.path(&record).to_string_lossy().into_owned();
        let mut names = Names::new();
        let ours = [socket_path.as_str(), record_path.as_str(), wire::SOCKET, confine::LIVE]
            .map(|it| names.intern(it).expect("an empty table has room for four names"));
        let server = Server {
            phase: Phase::Writing,
            socket,
            record: record.clone(),
            spawn: None,
            listen: None,
            conn: None,
            hung_up: false,
            lost: None,
            frames: Frames::new(),
            job_index: Vec::new(),
            scopes: Names::new(),
            scope_job: Vec::new(),
            names,
            ours,
            running: Vec::new(),
            at: 0,
            partial: Vec::new(),
            scoped: Vec::new(),
            common: Vec::new(),
            common_kinds: Vec::new(),
            outbox: Vec::new(),
            dirty: false,
            reading: false,
            waiting: Vec::new(),
            next: Vec::new(),
            broken: None,
        };
        self.bins[b].server = Some(server);
        let tag = self.tag(For::Write(b as u32));
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
        let [socket_path, record_path, ..] = server.ours.map(|it| server.names.get(it));
        let mut env = plan.env.clone();
        env.push((wire::SOCKET.into(), socket_path.into()));
        env.push((confine::LIVE.into(), record_path.into()));
        let mut read = plan.read.clone();
        read.push(PathBuf::from(socket_path));
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
        let listen = self.tag(For::Listen(b as u32));
        let spawn = self.tag(For::Spawn(b as u32));
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
                let recv = self.tag(For::Recv(b as u32));
                effects.push(Effect::Recv { conn, most: RECV, tag: recv });
                effects.push(Effect::Unlisten { tag });
            }
            Done::Unlistened => {
                self.release_tag(tag);
                self.server(b).listen = None;
            }
            Done::Failed(error) => {
                self.release_tag(tag);
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
            let recv = self.tag(For::Recv(b as u32));
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
                let jobs: FxHashMap<&str, u32> =
                    bin.plan.jobs.iter().enumerate().map(|(j, it)| (it.name.as_str(), j as u32)).collect();
                let server = bin.server.as_mut().expect("a server");
                server.job_index = vec![NO_TEST; bin.plan.jobs.len()];
                server.scoped = vec![Vec::new(); bin.plan.jobs.len()];
                for (at, name) in tests.iter().enumerate() {
                    let Some(&j) = jobs.get(name.as_str()) else { continue };
                    server.job_index[j as usize] = at as u32;
                    let id = server.scopes.intern(name).ok_or("it named more tests than blaze holds")?;
                    if id as usize == server.scope_job.len() {
                        server.scope_job.push(j);
                    }
                }
                server.phase = Phase::Ready;
                for rank in self.pending_of(b) {
                    let unit = self.order[rank as usize];
                    let j = self.unit_job[unit as usize] as usize;
                    if self.server(b).job_index[j] != NO_TEST {
                        self.ready.set(rank);
                        continue;
                    }
                    self.pending.clear(rank);
                    let why = format!("{} has no test named {}", self.bins[b].plan.name, self.bins[b].plan.jobs[j].name);
                    self.finish(unit, Went::lost(why, Vec::new()));
                }
                self.maybe_quit(b, effects);
                Ok(())
            }
            Frame::Ended { id, end, output, executed } => {
                let at = self
                    .attempt(id)
                    .filter(|&at| {
                        let attempt = &self.attempts[at];
                        attempt.running && self.unit_bin[attempt.unit as usize] as usize == b
                    })
                    .ok_or_else(|| format!("an end of {id}, which it does not run"))?;
                let server = self.server(b);
                if let Some(place) = server.running.iter().position(|it| *it == id) {
                    server.running.remove(place);
                }
                self.busy -= 1;
                let attempt = &mut self.attempts[at];
                attempt.running = false;
                let timers = [std::mem::replace(&mut attempt.deadline, UNSET), std::mem::replace(&mut attempt.grace, UNSET)];
                for timer in timers {
                    self.wheel.remove(timer);
                }
                let Attempt { unit, number, .. } = self.attempts[at];
                let outcome = self.outcome(at, &end);
                if outcome != Outcome::Passed {
                    // Decided before a core is given to another test: a retry keeps its place in
                    // the order, and fail-fast starts nothing more.
                    let j = self.unit_job[unit as usize] as usize;
                    if number <= self.bins[b].plan.jobs[j].retries && !self.stopping {
                        self.free_attempt(at);
                        self.tries[unit as usize] = number;
                        let tries = &mut self.tries_output[unit as usize];
                        _ = writeln!(tries, "--- attempt {number} ---");
                        tries.extend_from_slice(&output);
                        let rank = self.rank[unit as usize];
                        self.pending.set(rank);
                        if self.server(b).phase == Phase::Ready {
                            self.ready.set(rank);
                        }
                        return Ok(());
                    }
                    if self.config.fail_fast {
                        self.stop();
                    }
                }
                let ended = Ended { id, end, output, executed, at: self.now };
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
            let mut waiting = std::mem::take(&mut self.server(b).waiting);
            for ended in waiting.drain(..) {
                self.attempt_ended(b, ended, Trace::Unknown(why.clone()), effects);
            }
            self.server(b).waiting = waiting;
            return;
        }
        let tag = self.tag(For::Read(b as u32));
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
                    if let Err(why) = take_lines(server, &partial[..=end]) {
                        server.broken = Some(format!("the sandbox's record {}: {why}", server.record));
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
        let mut waiting = std::mem::take(&mut self.server(b).waiting);
        for ended in waiting.drain(..) {
            let Some(at) = self.attempt(ended.id) else { continue };
            let trace = match &broken {
                Some(why) => Trace::Unknown(why.clone()),
                None => self.lines(b, self.attempts[at].unit),
            };
            self.attempt_ended(b, ended, trace, effects);
        }
        let server = self.server(b);
        server.waiting = waiting;
        if !server.next.is_empty() {
            std::mem::swap(&mut server.waiting, &mut server.next);
            self.start_read(b, effects);
        }
    }

    /// The lines of the record a unit of `b` read: those of every scope, then its own, which
    /// are let go (only its last attempt's result asks).
    fn lines(&mut self, b: usize, unit: u32) -> Trace {
        let j = self.unit_job[unit as usize] as usize;
        let server = self.server(b);
        let own = server.scoped.get_mut(j).map(std::mem::take).unwrap_or_default();
        let packed = || server.common.iter().chain(&own).map(|&it| unpack(it));
        let text: usize = packed().map(|(id, _)| server.names.get(id).len()).sum();
        let mut lines = Lines { text: String::with_capacity(text), lines: Vec::with_capacity(server.common.len() + own.len()) };
        for (id, seen) in packed() {
            if !lines.try_push(seen, server.names.get(id)) {
                return Trace::Unknown("the lines the test read are past 4 GiB".to_owned());
            }
        }
        Trace::Lines(lines)
    }

    /// How a run, in attempt slot `at`, of a test that ended as `end` went.
    fn outcome(&self, at: usize, end: &End) -> Outcome {
        let attempt = &self.attempts[at];
        let unit = attempt.unit as usize;
        let timeout = self.bins[self.unit_bin[unit] as usize].plan.jobs[self.unit_job[unit] as usize].timeout;
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
        let Some(at) = self.attempt(ended.id) else { return };
        let Attempt { unit, number, started, .. } = self.attempts[at];
        let outcome = self.outcome(at, &ended.end);
        self.free_attempt(at);
        let took = ended.at.saturating_sub(started);
        let output = match self.tries[unit as usize] {
            0 => ended.output,
            _ => {
                self.tries[unit as usize] = 0;
                let mut output = std::mem::take(&mut self.tries_output[unit as usize]);
                _ = writeln!(output, "--- attempt {number} ---");
                output.extend_from_slice(&ended.output);
                output
            }
        };
        let outcome = if self.cancelled && outcome != Outcome::Passed { Outcome::NotRun } else { outcome };
        self.finish(unit, Went { outcome, attempts: number, output, took, trace, executed: ended.executed });
        self.maybe_quit(b, effects);
    }

    fn finish(&mut self, unit: u32, went: Went) {
        let (b, j) = (self.unit_bin[unit as usize] as usize, self.unit_job[unit as usize]);
        let failed = matches!(went.outcome, Outcome::Failed(_));
        let attempts = match went.outcome {
            Outcome::NotRun => went.attempts.saturating_sub(1),
            _ => went.attempts,
        };
        self.results.push(Finished {
            binary: b,
            test: (j != WHOLE).then(|| self.bins[b].plan.jobs[j as usize].name.clone()),
            outcome: went.outcome,
            attempts,
            output: went.output,
            took: went.took,
            trace: went.trace,
            executed: went.executed,
        });
        self.bins[b].left -= 1;
        if self.bins[b].left == 0 {
            self.quit.push(b as u32);
        }
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
        self.ready.clear_all();
        let pending = std::mem::replace(&mut self.pending, Bits::empty(self.order.len()));
        for rank in pending.ones() {
            let unit = self.order[rank as usize];
            let done = std::mem::take(&mut self.tries[unit as usize]);
            let output = std::mem::take(&mut self.tries_output[unit as usize]);
            self.finish(
                unit,
                Went {
                    outcome: Outcome::NotRun,
                    attempts: done + 1,
                    output,
                    took: Duration::ZERO,
                    trace: Trace::Unknown(STOPPED.to_owned()),
                    executed: Executed::Not(STOPPED.to_owned()),
                },
            );
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

    /// The binaries left with nothing to run, let go.
    fn let_go(&mut self, effects: &mut Vec<Effect>) {
        let mut quit = std::mem::take(&mut self.quit);
        for &b in &quit {
            self.maybe_quit(b as usize, effects);
        }
        quit.clear();
        quit.append(&mut self.quit);
        std::mem::swap(&mut self.quit, &mut quit);
    }

    /// `frame` to the server, in its outbox until the call ends ([`Queue::flush`]).
    fn send(&mut self, b: usize, frame: &Frame) {
        let server = self.server(b);
        if server.conn.is_none() || server.hung_up {
            return;
        }
        frame.encode(&mut server.outbox);
        if !server.dirty {
            server.dirty = true;
            self.dirty.push(b as u32);
        }
    }

    /// What this call said to each server, as one send of all its frames; then the driver
    /// timer moved earlier if the wheel's next expiry is.
    fn flush(&mut self, effects: &mut Vec<Effect>) {
        let mut dirty = std::mem::take(&mut self.dirty);
        for &b in &dirty {
            self.flush_server(b as usize, effects);
        }
        dirty.clear();
        self.dirty = dirty;
        let Some(next) = self.wheel.next() else { return };
        if self.armed.is_some_and(|it| it.1 <= next) {
            return;
        }
        let tag = self.tag(For::Tick);
        let after = Duration::from_millis(next).saturating_sub(self.now);
        effects.push(Effect::Timer { after, tag });
        self.armed = Some((tag, next));
    }

    fn flush_server(&mut self, b: usize, effects: &mut Vec<Effect>) {
        let Some(server) = self.bins[b].server.as_mut() else { return };
        server.dirty = false;
        let conn = server.conn.filter(|_| !server.hung_up);
        match conn {
            Some(conn) if !server.outbox.is_empty() => {
                let bytes = std::mem::take(&mut server.outbox);
                let tag = self.tag(For::Send(b as u32));
                effects.push(Effect::Send { conn, bytes, tag });
            }
            _ => server.outbox.clear(),
        }
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
        let running = std::mem::take(&mut self.server(b).running);
        let why = match was {
            Phase::Quitting if running.is_empty() => return,
            _ => format!("{}: {why}", self.bins[b].plan.name),
        };
        for id in running {
            self.busy -= 1;
            let Some(at) = self.attempt(id) else { continue };
            let Attempt { unit, number, started, .. } = self.attempts[at];
            self.free_attempt(at);
            let outcome = match self.cancelled {
                true => Outcome::NotRun,
                false => Outcome::Failed(Failure::Lost(why.clone())),
            };
            self.tries[unit as usize] = 0;
            let earlier = std::mem::take(&mut self.tries_output[unit as usize]);
            let took = self.now.saturating_sub(started);
            self.finish(
                unit,
                Went {
                    outcome,
                    attempts: number,
                    output: [earlier, output.clone()].concat(),
                    took,
                    trace: Trace::Unknown(why.clone()),
                    executed: Executed::Not(why.clone()),
                },
            );
        }
        for rank in self.pending_of(b) {
            let unit = self.order[rank as usize];
            self.pending.clear(rank);
            self.ready.clear(rank);
            self.tries[unit as usize] = 0;
            let earlier = std::mem::take(&mut self.tries_output[unit as usize]);
            self.finish(unit, Went::lost(why.clone(), [earlier, output.clone()].concat()));
        }
    }

    /// The ranks of `b`'s units not started, in order.
    fn pending_of(&self, b: usize) -> Vec<u32> {
        let Bin { first, units, .. } = self.bins[b];
        let mut ranks: Vec<u32> =
            (first..first + units).map(|unit| self.rank[unit as usize]).filter(|&rank| self.pending.has(rank)).collect();
        radix::sort(&mut ranks);
        ranks
    }

    /// Starts the first tests in the order whose servers are ready, while cores are free.
    /// Servers with nothing left to run are let go.
    fn schedule(&mut self, effects: &mut Vec<Effect>) {
        while !self.stopping && self.busy < self.config.jobs.max(1) {
            let Some(rank) = self.ready.first() else { break };
            self.ready.clear(rank);
            self.pending.clear(rank);
            let unit = self.order[rank as usize];
            match self.unit_job[unit as usize] {
                WHOLE => self.start_whole(self.unit_bin[unit as usize] as usize, effects),
                _ => self.start_job(unit),
            }
        }
        self.let_go(effects);
    }

    fn start_job(&mut self, unit: u32) {
        let (b, j) = (self.unit_bin[unit as usize] as usize, self.unit_job[unit as usize] as usize);
        let number = self.tries[unit as usize] + 1;
        let timeout = self.bins[b].plan.jobs[j].timeout;
        let index = self.server(b).job_index[j];
        let id = self.new_attempt(unit, number);
        self.server(b).running.push(id);
        self.busy += 1;
        self.send(b, &Frame::Run { id, index });
        let deadline = self.wheel.insert(due_tick(self.now, timeout), Due::Deadline(id));
        let attempt = &mut self.attempts[id as u32 as usize];
        attempt.running = true;
        attempt.deadline = deadline;
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
        let spawn = self.tag(For::Whole(b as u32));
        let deadline = self.wheel.insert(due_tick(self.now, timeout), Due::Whole(b as u32));
        self.busy += slots;
        self.bins[b].whole = Some(Whole { spawn, started: self.now, slots, timed_out: false, deadline });
        effects.push(Effect::Spawn { command: Box::new(command), tag: spawn });
    }

    fn whole_ended(&mut self, b: usize, done: Done) {
        let Some(whole) = self.bins[b].whole.take() else { return };
        self.wheel.remove(whole.deadline);
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
        let executed = Executed::Not(WHOLE_UNRECORDED.to_owned());
        let unit = self.bins[b].first;
        self.finish(unit, Went { outcome, attempts: 1, output, took, trace, executed });
    }
}

/// `duration` in nanoseconds, saturating.
fn nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

/// The wheel's tick `now` is in.
fn tick(now: Duration) -> u64 {
    u64::try_from(now.as_nanos() / TICK).unwrap_or(u64::MAX)
}

/// The first tick at or past `now + after`: a timer never fires early.
fn due_tick(now: Duration, after: Duration) -> u64 {
    u64::try_from(now.saturating_add(after).as_nanos().div_ceil(TICK)).unwrap_or(u64::MAX)
}

/// Each kind of [`Seen`] by its number, as [`pack`] keeps it.
const SEEN: [Seen; 8] = [
    Seen::Read,
    Seen::Looked,
    Seen::Listed,
    Seen::Env,
    Seen::Untraceable,
    Seen::RefusedWrite,
    Seen::RefusedNet,
    Seen::Incomplete,
];

/// A line as a `u32`: the ID of what it names (at most [`names::MOST`]) and its kind.
fn pack(id: u32, seen: Seen) -> u32 {
    (id << 3) | seen as u32
}

fn unpack(packed: u32) -> (u32, Seen) {
    (packed >> 3, SEEN[(packed & 7) as usize])
}

/// Whether a line of the record is of blaze's own doing, not the tests': its socket, its
/// record, the variables it set for the server.
fn ours(server: &Server, seen: Seen, id: u32) -> bool {
    let [socket, record, socket_var, live_var] = server.ours;
    match seen {
        Seen::Env => id == socket_var || id == live_var,
        Seen::Read | Seen::Looked | Seen::Listed => id == socket || id == record,
        _ => false,
    }
}

/// A line of the record without its line break, borrowed: the parts [`Line::parse`] reads,
/// its kind by its first byte.
fn record_line(text: &str) -> Option<(Seen, &str, &str)> {
    let bytes = text.as_bytes();
    if bytes.len() < 2 || bytes[1] != b'\t' {
        return None;
    }
    let seen = match bytes[0] {
        b'R' => Seen::Read,
        b'L' => Seen::Looked,
        b'D' => Seen::Listed,
        b'E' => Seen::Env,
        b'T' => Seen::Untraceable,
        b'W' => Seen::RefusedWrite,
        b'N' => Seen::RefusedNet,
        b'X' => Seen::Incomplete,
        _ => return None,
    };
    let rest = &text[2..];
    let tab = rest.find('\t')?;
    Some((seen, &rest[..tab], &rest[tab + 1..]))
}

/// The whole lines of `text` (which ends with a line break) into the server's lines, as
/// [`trace::parse`] of its lossy text reads them, but in place: each line is read borrowed,
/// and what it names interned. A line not UTF-8 is read lossily alone, which is the same: a
/// replaced sequence never spans a line break.
fn take_lines(server: &mut Server, text: &[u8]) -> Result<(), String> {
    for (number, piece) in text.split_inclusive(|it| *it == b'\n').enumerate() {
        let Some(piece) = piece.strip_suffix(b"\n") else { break };
        let piece = match std::str::from_utf8(piece) {
            Ok(it) => Cow::Borrowed(it),
            Err(_) => Cow::Owned(String::from_utf8_lossy(piece).into_owned()),
        };
        let Some((seen, scope, what)) = record_line(&piece) else {
            return Err(BadRecord { line: number + 1, text: piece.chars().take(200).collect() }.to_string());
        };
        let job = match scope.is_empty() {
            true => None,
            false => match server.scopes.find(scope) {
                Some(id) => Some(server.scope_job[id as usize]),
                // A scope no job of this server has is no test's to be given.
                None => continue,
            },
        };
        let id = server
            .names
            .intern(what)
            .ok_or_else(|| format!("it names more than {} paths and variables, or 4 GiB of them", names::MOST))?;
        if ours(server, seen, id) {
            continue;
        }
        match job {
            Some(j) => server.scoped[j as usize].push(pack(id, seen)),
            None => {
                let at = id as usize;
                if at >= server.common_kinds.len() {
                    server.common_kinds.resize(at + 1, 0);
                }
                let bit = 1 << seen as u8;
                if server.common_kinds[at] & bit != 0 {
                    continue;
                }
                server.common_kinds[at] |= bit;
                server.common.push(pack(id, seen));
            }
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

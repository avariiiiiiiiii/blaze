//! blaze's sans-IO core (docs/v2/core.md section 3, docs/v2/plane.md).
//!
//! A state machine of blaze's (the build's plane, a journal, the store's index) does no IO,
//! reads no clock and starts no thread. It asks for IO by pushing [`Effect`]s, each with a
//! [`Tag`], and is told how each went by a [`Completion`] with the same tag. A [`Driver`] runs
//! the effects: [`sim::Sim`] in one thread under a seeded scheduler that reorders, fails and
//! crashes (deterministic simulation tests), [`threads::Threads`] on the real file system, and
//! on Linux [`uring::Uring`], on one io_uring.
//!
//! Files are named relative to the driver's root, so the same state machine runs in a
//! simulation, a temporary directory or the daemon's state directory unchanged.
//!
//! On macOS [`kqueue::Kqueue`] keeps timers, sockets, processes and FSEvents' watches on one
//! kqueue and the file calls on threads. A process of [`Effect::Spawn`] in a sandbox is made
//! by the [`Confine`] a driver is given; a driver given none refuses it.

#[cfg(target_os = "linux")]
mod cgroup;
#[cfg(target_os = "linux")]
mod fanotify;
#[cfg(target_os = "macos")]
mod footprint;
mod fs;
#[cfg(target_os = "macos")]
mod fsevents;
#[cfg(target_os = "macos")]
pub mod kqueue;
#[cfg(unix)]
mod net;
#[cfg(unix)]
mod process;
pub mod sim;
pub mod threads;
#[cfg(target_os = "linux")]
pub mod uring;
#[cfg(target_os = "linux")]
mod watch;

use std::{fmt, time::Duration};

/// Makes the sandbox of a process: given to a driver by whoever owns the platform's sandbox
/// (`blaze-usercode`), so the driver starts what it says in place of the program. A driver
/// given none fails every spawn whose [`Sandbox`] is on.
pub trait Confine: Send + Sync {
    /// What to start so that `command` runs in its sandbox, usually a launcher with the
    /// program after it. `scratch` is a directory of this run's own, empty, made by the
    /// driver and removed once [`Confine::traced`] has been asked: where the launcher keeps
    /// its record. An error says why no sandbox can be made here; nothing runs then.
    fn confine(&self, command: &Command, scratch: &std::path::Path) -> Result<Confined, String>;
    /// What the sandbox recorded the process read, once it has exited: asked only of a
    /// sandbox that traces.
    fn traced(&self, command: &Command, scratch: &std::path::Path) -> Result<Traced, String>;
}

/// The process a [`Confine`] starts for a command: its whole environment is this one.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Confined {
    pub program: std::path::PathBuf,
    pub args: Vec<std::ffi::OsString>,
    pub env: Vec<(std::ffi::OsString, std::ffi::OsString)>,
    pub dir: std::path::PathBuf,
}

/// Names a request so its completion can be matched to it. The state machine chooses tags; a
/// driver hands them back unchanged.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Tag(pub u64);

/// A file under the driver's root: `/`-separated, relative, without `.` or `..`.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct FileName(Box<str>);

impl FileName {
    /// `None` for a name that would leave the driver's root or names nothing.
    pub fn new(name: &str) -> Option<FileName> {
        let ok = !name.is_empty()
            && !name.starts_with('/')
            && name.split('/').all(|it| !it.is_empty() && it != "." && it != "..");
        ok.then(|| FileName(name.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether `self` is `dir` or under it.
    pub fn is_under(&self, dir: &FileName) -> bool {
        self.0 == dir.0
            || self.0.strip_prefix(&*dir.0).is_some_and(|rest| rest.starts_with('/'))
    }
}

impl fmt::Display for FileName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// IO a state machine asks for.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Effect {
    /// The whole file. A file that is not there reads as empty: a journal not yet written is
    /// one with no records.
    ReadAll { file: FileName, tag: Tag },
    /// `len` bytes at `at`, or fewer at the end of the file.
    Read { file: FileName, at: u64, len: usize, tag: Tag },
    /// Bytes added at the end of the file, creating it. Not durable until a [`Effect::Sync`] of
    /// the file asked for after this one has completed.
    Append { file: FileName, bytes: Vec<u8>, tag: Tag },
    /// Everything appended to the file before this was asked for is durable once it completes.
    Sync { file: FileName, tag: Tag },
    /// The file cut to its first `len` bytes (made if missing); durable once a later sync
    /// completes.
    Truncate { file: FileName, len: u64, tag: Tag },
    /// The file's whole contents replaced by `bytes`, durably and all at once: a reader or a
    /// crash sees the old contents or the new, never a part. Its directories are made.
    Write { file: FileName, bytes: Vec<u8>, tag: Tag },
    /// Every file and directory under `dir`, at any depth, sorted by name, each with its
    /// [`Meta`]. A symbolic link is listed as a file of its own length and not followed.
    /// Completes with [`Done::Listed`]; a `dir` that is not there fails.
    List { dir: FileName, tag: Tag },
    /// `bytes` written at `at` of `file`, which exists, past its end making it longer; durable
    /// once a later [`Effect::Sync`] of the file completes. Completes with [`Done::Written`].
    /// For a link writing the pages that changed into a clone of the last executable.
    WriteAt { file: FileName, at: u64, bytes: Vec<u8>, tag: Tag },
    /// `from` renamed over `to`, atomically: a reader sees the old `to` or the new, and the
    /// new name survives a crash once it completes. Completes with [`Done::Renamed`].
    Rename { from: FileName, to: FileName, tag: Tag },
    /// The file's permission bits set to `mode` (0o755 for an executable). Completes with
    /// [`Done::Moded`].
    Mode { file: FileName, mode: u32, tag: Tag },
    /// `to` made a file with `from`'s contents that shares its blocks (`FICLONE`,
    /// `clonefile`). Fails where the file system cannot share blocks, naming it: nothing
    /// copies instead.
    Clone { from: FileName, to: FileName, tag: Tag },
    /// `to` made another name of `from`, for outputs nothing writes in place.
    Link { from: FileName, to: FileName, tag: Tag },
    /// What the file is, or that it is not there.
    Stat { file: FileName, tag: Tag },
    /// The file, or an empty directory, gone. Gone already is done.
    Remove { file: FileName, tag: Tag },
    /// Completes with [`Done::Changed`] each time files under `dir` change, until an
    /// [`Effect::Unwatch`] of the same tag, which ends it with [`Done::Unwatched`], or a
    /// [`Done::Failed`], which ends it too. It is in flight until it ends; an unwatch has no
    /// completion of its own but the watch's end.
    Watch { dir: FileName, tag: Tag },
    Unwatch { tag: Tag },
    /// Completes once `after` has passed.
    Timer { after: Duration, tag: Tag },
    /// The whole file mapped read-only into memory, shared with every other mapping of it
    /// (a pack of the store, the daemon's image). Completes with [`Done::Mapped`].
    Map { file: FileName, tag: Tag },
    /// A process started as `command` says, in its sandbox; completes with [`Done::Exited`]
    /// once it has exited and its output is read, or with [`Done::Failed`] when it could not
    /// start (no sandbox can be made here is a failure, never a run outside one).
    Spawn { command: Box<Command>, tag: Tag },
    /// The process of the spawn `tag` killed: its spawn completes, with the signal. No
    /// completion of its own; a spawn that ended already is left as it is.
    Kill { tag: Tag },
    /// Completes with [`Done::Accepted`] for each connection made to `at`, until an
    /// [`Effect::Unlisten`] of the same tag, which ends it with [`Done::Unlistened`]. In
    /// flight until it ends, as a watch is.
    Listen { at: Address, tag: Tag },
    Unlisten { tag: Tag },
    /// A connection made to `at`; completes with [`Done::Connected`].
    Connect { at: Address, tag: Tag },
    /// The bytes the peer of `conn` sent since the last receive, at most `most`: empty once it
    /// has hung up.
    Recv { conn: Conn, most: usize, tag: Tag },
    /// All of `bytes` sent on `conn`.
    Send { conn: Conn, bytes: Vec<u8>, tag: Tag },
    /// `conn` closed; a receive in flight on it completes empty.
    Hangup { conn: Conn, tag: Tag },
}

impl Effect {
    pub fn tag(&self) -> Tag {
        match self {
            Effect::ReadAll { tag, .. }
            | Effect::Read { tag, .. }
            | Effect::Append { tag, .. }
            | Effect::Sync { tag, .. }
            | Effect::Truncate { tag, .. }
            | Effect::Write { tag, .. }
            | Effect::List { tag, .. }
            | Effect::WriteAt { tag, .. }
            | Effect::Rename { tag, .. }
            | Effect::Mode { tag, .. }
            | Effect::Clone { tag, .. }
            | Effect::Link { tag, .. }
            | Effect::Stat { tag, .. }
            | Effect::Remove { tag, .. }
            | Effect::Watch { tag, .. }
            | Effect::Unwatch { tag }
            | Effect::Timer { tag, .. }
            | Effect::Map { tag, .. }
            | Effect::Spawn { tag, .. }
            | Effect::Kill { tag }
            | Effect::Listen { tag, .. }
            | Effect::Unlisten { tag }
            | Effect::Connect { tag, .. }
            | Effect::Recv { tag, .. }
            | Effect::Send { tag, .. }
            | Effect::Hangup { tag, .. } => *tag,
        }
    }
}

/// Where [`Effect::Listen`] listens or [`Effect::Connect`] connects. A socket listened on is
/// reachable by its owner only; a TCP address listened on is for a webhook behind the VM's
/// own proxy. TLS is above this, in whoever speaks it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Address {
    /// A Unix socket under the driver's root, made with mode 0600.
    Unix(FileName),
    Tcp(std::net::SocketAddr),
}

/// A connection a listen accepted or a connect made, named by the driver.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Conn(pub u64);

/// A process to run. Paths outside the driver's root (a toolchain's, the C compiler's) are
/// absolute; everything the process may touch is in its [`Sandbox`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Command {
    pub program: std::path::PathBuf,
    pub args: Vec<std::ffi::OsString>,
    /// The whole environment: nothing of the daemon's is inherited.
    pub env: Vec<(std::ffi::OsString, std::ffi::OsString)>,
    pub dir: std::path::PathBuf,
    pub stdin: Vec<u8>,
    pub sandbox: Sandbox,
    /// The most memory it may use, held by the system (a cgroup on Linux, a resource limit
    /// on macOS): the backstop behind admission's estimates.
    pub memory: Option<u64>,
}

/// What a spawned process may see and do. User code (build scripts, proc macros, tests) runs
/// in one always; [`Sandbox::Off`] is only for what `BLAZE_SANDBOX=off` or a documented
/// manifest opt-out says.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Sandbox {
    Off,
    On {
        /// Read-only, with what is under them.
        read: Vec<std::path::PathBuf>,
        /// Writable, with what is under them: an `OUT_DIR`, a temporary directory.
        write: Vec<std::path::PathBuf>,
        network: bool,
        /// Whether to record what it read ([`Traced`]).
        trace: bool,
    },
}

/// What a sandboxed process read, recorded by its sandbox: its result is keyed by exactly
/// these (docs/v2/usercode.md section 6.5), so the paths are as the process named them.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Traced {
    /// Each file it opened to read, and the BLAKE3 hash of its bytes then; `None` for a file
    /// it looked for and did not find.
    pub files: Vec<(std::path::PathBuf, Option<[u8; 32]>)>,
    /// Each directory it listed, and the hash of its sorted names then.
    pub dirs: Vec<(std::path::PathBuf, [u8; 32])>,
    /// Each environment variable it read, and its value then.
    pub env: Vec<(std::ffi::OsString, Option<std::ffi::OsString>)>,
    /// What it read that cannot be traced (the clock, randomness, the network): a result
    /// with any of these is keyed as cargo keys it.
    pub untraceable: Vec<Untraceable>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Untraceable {
    Clock,
    Random,
    Network,
    /// A file read through a descriptor the sandbox did not see opened.
    Descriptor,
}

/// How a spawned process ended.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Exit {
    /// Its exit code, or `None` when a signal ended it ([`Exit::signal`]).
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Of a sandbox that traces.
    pub traced: Option<Traced>,
    /// The most memory it held, as the system measured it.
    pub peak_memory: u64,
}

/// A file mapped read-only by [`Effect::Map`]: its bytes, which stay as they are for as long
/// as any clone of it lives (the store and the image only ever replace a file by renaming
/// another over it, never write one in place).
#[derive(Clone)]
pub struct Mapping(pub std::sync::Arc<dyn AsRef<[u8]> + Send + Sync>);

impl Mapping {
    pub fn bytes(&self) -> &[u8] {
        (*self.0).as_ref()
    }
}

impl PartialEq for Mapping {
    fn eq(&self, other: &Mapping) -> bool {
        self.bytes() == other.bytes()
    }
}

impl Eq for Mapping {}

impl fmt::Debug for Mapping {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Mapping({} bytes)", self.bytes().len())
    }
}

/// What [`Effect::Stat`] found.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Meta {
    pub len: u64,
    pub dir: bool,
    /// Nanoseconds since the epoch of its last change, where the file system keeps it; 0 in a
    /// simulation, which has no clock of the world.
    pub modified_ns: u64,
}

/// How an [`Effect`] went.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Done {
    Read(Vec<u8>),
    Appended,
    Synced,
    Truncated,
    Written,
    Listed(Vec<(FileName, Meta)>),
    Renamed,
    Moded,
    Cloned,
    Linked,
    Stat(Option<Meta>),
    Removed,
    /// Files under a watched directory changed: each is named once, in no order. `lost` says
    /// the system dropped changes (its queue overflowed), so whoever watches must look at the
    /// whole directory again: the names are not all of them.
    Changed { files: Vec<FileName>, lost: bool },
    Unwatched,
    Fired,
    Mapped(Mapping),
    Exited(Box<Exit>),
    Accepted { conn: Conn },
    Connected { conn: Conn },
    Unlistened,
    Sent,
    HungUp,
    /// It did not happen. For an append, none of its bytes are durable unless a later sync
    /// says so; for a sync, nothing appended since the last successful one is known durable;
    /// for a write, the file is as it was.
    Failed(IoError),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct IoError(pub String);

impl fmt::Display for IoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Completion {
    pub tag: Tag,
    pub done: Done,
}

/// Runs effects and reports their completions, in any order. Every method is called from the
/// one thread that runs the state machines.
pub trait Driver {
    /// Starts every effect in `effects`, leaving it empty.
    fn submit(&mut self, effects: &mut Vec<Effect>);
    /// Moves completions into `out`, waiting up to `wait` for the first if none is ready.
    /// Returns how many it moved.
    fn poll(&mut self, out: &mut Vec<Completion>, wait: Duration) -> usize;
    /// How many effects were submitted and have not completed.
    fn in_flight(&self) -> usize;
    /// Monotonic time since the driver began.
    fn now(&self) -> Duration;
}

/// Runs `effects` and polls until every one of them has completed, handing each completion to
/// `on`, which may push more effects (submitted before the next poll). For drivers whose
/// effects all complete; a timer far ahead holds it until it fires, and a watch until it is
/// unwatched.
pub fn run_until_idle(
    driver: &mut dyn Driver,
    effects: &mut Vec<Effect>,
    mut on: impl FnMut(Completion, &mut Vec<Effect>),
) {
    let mut done = Vec::new();
    loop {
        driver.submit(effects);
        if driver.in_flight() == 0 {
            return;
        }
        driver.poll(&mut done, Duration::from_secs(60));
        for completion in done.drain(..) {
            on(completion, effects);
        }
    }
}

#[cfg(test)]
mod tests;

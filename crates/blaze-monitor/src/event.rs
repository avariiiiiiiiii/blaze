//! What the client, the daemon and the workers tell the monitor: one [`Event`] per thing
//! that happened, sent after it happened. Nothing reads them back to decide a build.

use serde::{Deserialize, Serialize};

/// One thing that happened, as its owner saw it.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum Event {
    Build(Build),
    Compiled(Compiled),
    Live(Live),
    Tests(Tests),
}

/// Who built the programs of a `cargo blaze` command or of an editor's edit.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum By {
    /// The daemon's workers built every program.
    Daemon,
    /// Cargo built, for the reason in [`Build::why`].
    Cargo,
    /// The source has errors; nothing was built.
    Error,
}

/// A build, from the client that asked for it (or the daemon, for an editor's edit).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Build {
    /// The directory the build was asked in, or the edited file's workspace.
    pub workspace: String,
    /// `build`, `run`, `test`, `bench` or `edit`.
    pub command: String,
    pub by: By,
    /// Why cargo built, or the error; empty when the daemon built.
    pub why: String,
    pub succeeded: bool,
    /// The process that asked.
    pub pid: u32,
    /// When it started, in microseconds since the Unix epoch.
    pub at_us: u64,
    pub took_us: u64,
}

/// What a worker did with one program for a build: a patch, a whole object, or a refusal.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Compiled {
    /// The root of the checkout the worker built.
    pub workspace: String,
    /// The executable the program links to (or the object, when it links to none).
    pub binary: String,
    pub krate: String,
    /// `patch`, `object`, `error` (the source has errors) or `cargo` (cargo builds it).
    pub kind: String,
    pub functions: u64,
    pub files: u64,
    /// Why it was not a patch, or why the worker refused it; empty otherwise.
    pub why: String,
    /// When it ended, in microseconds since the Unix epoch.
    pub at_us: u64,
    pub took_us: u64,
}

/// What a worker sent a running program: a patch to apply, or word to restart.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Live {
    pub executable: String,
    pub pid: u32,
    pub sequence: u64,
    /// `sent` (a patch was published) or `restart`.
    pub outcome: String,
    pub why: String,
    pub functions: u64,
    pub statics: u64,
    /// When it was published, in microseconds since the Unix epoch.
    pub at_us: u64,
    /// From the edit to the publish.
    pub took_us: u64,
}

/// One test executable's run under `cargo blaze test`.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Tests {
    pub workspace: String,
    pub package: String,
    /// `lib`, `test`, `bin`, ...
    pub kind: String,
    pub executable: String,
    /// `affected` when `RA_JIT_TESTS=affected` picked the tests, else `all`.
    pub selection: String,
    /// How many tests the executable has, when it was listed.
    pub total: u64,
    /// The exit status of the run; `-1` when it was killed.
    pub status: i32,
    pub at_us: u64,
    pub took_us: u64,
    pub tests: Vec<Test>,
}

/// Why a test ran or did not, under `RA_JIT_TESTS=affected`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Picked {
    /// It reaches a function that changed since it last passed.
    Changed,
    /// Nothing it reaches changed since it last passed.
    Skipped,
    /// It reads something blaze can't track ([`Test::why`]), so it always runs.
    Always,
    /// It ran in the sandbox, which checks it reads nothing outside what is tracked.
    Sandboxed,
    /// No selection: every test ran.
    Every,
}

/// How a test ended.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Passed,
    Failed,
    Ignored,
    /// It was not run this time.
    NotRun,
    /// It was picked but its line was not seen (the run was killed, or printed no result).
    Unknown,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Test {
    pub name: String,
    pub picked: Picked,
    /// Why it always runs: what it reads.
    pub why: String,
    pub path: String,
    pub outcome: Outcome,
    /// From the start of the executable to its result line, an upper bound on how long it
    /// took (libtest prints no times).
    pub ended_us: u64,
}

//! What a build says as it goes, for whoever watches it: `blaze build` and `run` render it
//! live, `blaze status` and `watch` attach to a build another terminal started, `--events`
//! writes it for tools, and `blaze why` reads the last build's. The daemon keeps each
//! build's records in order; the same build gives the same records but for their times.
//!
//! Times are nanoseconds since the build began and counts are integers: no floats.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// One thing a build said, `at_ns` after it began.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub at_ns: u64,
    pub event: Event,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A build began: the command that asked for it, in the workspace at `root`.
    Began { build: u64, command: String, root: PathBuf },
    /// A step of the build began, of what when it is of one crate or program.
    Started { step: Step, of: Option<String> },
    /// The step began last ended.
    Ended { step: Step, of: Option<String> },
    /// How far the build of a crate is.
    Crate { name: String, state: CrateState, fns: Fns },
    /// Why a unit or a function was compiled again rather than taken as it was.
    Rebuilt { name: String, krate: String, cause: Cause },
    /// How many more functions were compiled again than the [`Event::Rebuilt`] records name.
    Unlisted { count: u64 },
    /// What blaze does not build, which fails the build (docs/architecture.md, "Never
    /// guess; refuse visibly").
    Refused { name: String, why: String },
    /// A line that is no step: a warning, a note.
    Said { level: Level, text: String },
    /// The build is done.
    Finished { ok: bool, summary: Summary },
}

/// The steps of a build, in the order they come.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Step {
    /// Reading the manifests and the lock file, running build scripts and proc macros:
    /// rust-analyzer's database is made.
    LoadWorkspace,
    /// Fetching what the build needs that is not on the machine: crates, a toolchain.
    Fetch,
    /// Resolving the names of the crates.
    Resolve,
    /// Type checking, borrow checking and lowering to MIR.
    FrontEnd,
    /// Compiling functions to machine code, or taking them from the store.
    Codegen,
    /// The standard library, compiled once and kept in the store.
    Std,
    /// Writing the program.
    Link,
    /// `blaze check`'s diagnostics.
    Check,
    /// Running the program (`blaze run`).
    Run,
    /// Running tests (`blaze test`).
    Test,
}

impl Step {
    /// The word a line says it with, cargo's where cargo has one.
    pub fn word(self) -> &'static str {
        match self {
            Step::LoadWorkspace => "Loading",
            Step::Fetch => "Fetching",
            Step::Resolve => "Resolving",
            Step::FrontEnd => "Checking",
            Step::Codegen => "Compiling",
            Step::Std => "Compiling",
            Step::Link => "Linking",
            Step::Check => "Checking",
            Step::Run => "Running",
            Step::Test => "Testing",
        }
    }

    /// What it is of when it is of no one crate.
    pub fn whole(self) -> &'static str {
        match self {
            Step::LoadWorkspace => "workspace",
            Step::Fetch => "dependencies",
            Step::Resolve => "names",
            Step::FrontEnd => "types",
            Step::Codegen => "functions",
            Step::Std => "the standard library (once, kept in the store)",
            Step::Link => "program",
            Step::Check => "crates",
            Step::Run => "program",
            Step::Test => "tests",
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CrateState {
    Waiting,
    Running,
    /// Every function was in the store or unchanged.
    UpToDate,
    Built,
    Failed,
}

/// The functions of a crate: how many the build has, how many are done, and of those how
/// many came from the store or were unchanged rather than compiled.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Fns {
    pub total: u64,
    pub done: u64,
    pub kept: u64,
}

impl Fns {
    pub fn compiled(self) -> u64 {
        self.done - self.kept.min(self.done)
    }
}

/// Why something was compiled again.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum Cause {
    /// Nothing was kept of it: no store entry, or no store.
    Cold,
    /// A file it is written in changed.
    Edited { file: PathBuf },
    /// Its body changed, and nothing it is seen by.
    Body,
    /// Its signature changed: what calls it is compiled again too.
    Signature,
    /// A function it inlines or calls by its code changed.
    Callee { name: String },
    /// Something it is compiled from changed, in a file other than its own: a type, a
    /// constant, a function it inlines.
    Uses,
    /// A dependency is of another version, features or flags.
    Dependency { name: String },
    /// What it is compiled with changed: the toolchain, a flag, a variable.
    Flags { what: String },
}

impl Cause {
    /// A few words for a summary line: "body edited", "call it".
    pub fn short(&self) -> String {
        match self {
            Cause::Cold => "not kept".to_owned(),
            Cause::Edited { file } => format!("{} edited", file.display()),
            Cause::Body => "body edited".to_owned(),
            Cause::Signature => "signature changed".to_owned(),
            Cause::Callee { name } => format!("calls {name}"),
            Cause::Uses => "uses what changed".to_owned(),
            Cause::Dependency { name } => format!("{name} changed"),
            Cause::Flags { what } => format!("{what} changed"),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Note,
    Warning,
    Error,
}

/// What a finished build did, for its last line.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    pub took_ns: u64,
    pub crates: u64,
    pub fns: Fns,
    /// What was compiled again and why, the first few.
    pub rebuilt: Vec<(String, Cause)>,
    /// How many more there were than `rebuilt` lists.
    pub more: u64,
    pub error: Option<String>,
}

/// What a client asks the daemon of builds.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum Subscribe {
    /// The records of the build under way in the workspace at `root`, or the last one, from
    /// its first, then each as it comes until it finishes.
    Build { root: PathBuf },
    /// Every build's, as they come, until the client hangs up.
    All,
}

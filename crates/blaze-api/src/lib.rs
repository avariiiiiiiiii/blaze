//! The agent API (docs/v2/plan/lanes/ci.md, docs/v2/plan.md section 1; #277; rule no-mcp): the daemon's
//! only interface.
//!
//! Every message is a [`Envelope`] of [`SCHEMA`], encoded in RFC 8949's core deterministic
//! CBOR ([`blaze_protocol::cbor`]) and framed as [`blaze_protocol::write_frame`] frames it, over
//! a Unix socket or the body of an HTTP request. A reader of another schema refuses the
//! message by name. One connection carries many requests; each [`Response`] names its
//! request's id, and a request may have many (events, then its end).
//!
//! The shapes are for LLMs: whole answers, no paging cursors to keep, every error naming what
//! and where.
//!
//! - [`frames`]: frames in and out of a byte stream, and the schema check.
//! - [`http`]: HTTP/1.1, sans-IO, for the API's HTTP side and the webhook.
//! - [`server`]: the daemon's listener, as a state machine over [`blaze_core`]'s sockets: what
//!   [`Arrived`] and where each [`Reply`] goes.
//! - [`session`]: what each request does, over the VFS, the database and the [`build::Builder`].
//! - [`client`]: the CLI's and CI's side, blocking.
//! - [`json`]: the rendering for people.

pub mod build;
pub mod client;
pub mod frames;
pub mod http;
pub mod json;
pub mod server;
pub mod session;

use blaze_core::Conn;
use blaze_ids::{ContentId, Fingerprint};
use blaze_query::{QueryKey, Why};
use blaze_vfs::{Change, Op, View};
use serde::{Deserialize, Serialize};

/// The version of every body's schema. A change to any type here is a new number.
pub const SCHEMA: u32 = 5;

/// The most bytes a request the daemon reads may be: CBOR from a peer is decoded only up to
/// this.
pub const LONGEST_REQUEST: u32 = blaze_protocol::LONGEST_FRAME;

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Envelope<T> {
    pub schema: u32,
    /// Chosen by the client; a response carries the id of its request.
    pub id: u64,
    pub body: T,
}

impl<T> Envelope<T> {
    pub fn new(id: u64, body: T) -> Envelope<T> {
        Envelope { schema: SCHEMA, id, body }
    }
}

/// A view as a request names it.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum ViewRef {
    /// A head (a workspace, a branch).
    Head(String),
    /// A root, by content ID.
    Root(View),
    /// A head or root with ops applied, moving nothing: a what-if.
    WhatIf { base: Box<ViewRef>, ops: Vec<Op> },
}

/// What to build or run.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Targets {
    /// Packages by name; empty for the workspace's default members.
    pub packages: Vec<String>,
    pub profile: String,
    /// A target triple; `None` for the machine's.
    pub target: Option<String>,
    pub features: Vec<String>,
    pub all_features: bool,
    pub no_default_features: bool,
    /// Everything a cargo command line says beyond the fields above, for `blaze build`,
    /// `run`, `test`, `check` and `cargo blaze`; `None` for an agent's request, built as cargo
    /// builds the fields above with its defaults. Where both say something, this wins. A
    /// check with cargo's `--keep-going` checks every member past one that fails.
    pub cargo: Option<Cargo>,
}

/// A cargo command line, as the CLI was run.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct Cargo {
    /// `build`, `check`, `test`, `bench` or `run`.
    pub command: String,
    /// The arguments after the subcommand and before `--`, as typed.
    pub args: Vec<String>,
    /// Where the view's root is on the client's machine, absolute: what cargo's paths
    /// (`CARGO_MANIFEST_DIR`, `file!()`, the target directory) are under.
    pub root: String,
    /// The directory the command ran in, absolute.
    pub cwd: String,
    /// The command's environment, sorted by name. Held in memory for the request only: no
    /// query value, log or store entry holds a variable's value unless cargo reads it by name.
    pub env: Vec<(String, String)>,
    /// The files outside the view cargo reads, by absolute path: `$CARGO_HOME/config.toml`,
    /// the `.cargo/config.toml` of each directory above the root, a `--config` file outside
    /// it. Never credentials.
    pub files: Vec<(String, Vec<u8>)>,
    /// Whether diagnostics are rendered with colors (cargo's `--color`, resolved by the
    /// client against its terminal).
    pub color: bool,
}

/// The environment's values may be secrets: Debug names the variables only.
impl std::fmt::Debug for Cargo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let env: Vec<&str> = self.env.iter().map(|(name, _)| name.as_str()).collect();
        let files: Vec<&str> = self.files.iter().map(|(path, _)| path.as_str()).collect();
        f.debug_struct("Cargo")
            .field("command", &self.command)
            .field("args", &self.args)
            .field("root", &self.root)
            .field("cwd", &self.cwd)
            .field("env", &env)
            .field("files", &files)
            .field("color", &self.color)
            .finish()
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum Request {
    /// Applies ops to a head, rebased if it moved; answered with [`Response::Committed`].
    Commit { head: String, base: ContentId, ops: Vec<Op> },
    /// Checks the view; answered with diagnostics.
    Check { view: ViewRef, targets: Targets },
    Build { view: ViewRef, targets: Targets },
    /// Runs the tests the view's change reached (all, with `all`), filtered by name.
    Test { view: ViewRef, targets: Targets, filter: Vec<String>, all: bool },
    Run { view: ViewRef, targets: Targets, bin: Option<String>, args: Vec<String> },
    /// The files that differ between two views.
    Diff { from: ViewRef, to: ViewRef },
    /// Why a query reran in the last build of a view.
    Why { view: ViewRef, query: QueryKey },
    /// What the daemon is doing: its requests, memory, views.
    Status,
    /// Cancels the request `id` of this connection.
    Cancel { id: u64 },
    /// Resolves a view to its root and keeps it open until [`Request::Close`] or the
    /// connection ends; answered with [`Response::Opened`].
    Open { view: ViewRef },
    Close { root: ContentId },
    /// What the change from `from` to `to` reaches (items, facts, functions compiled again,
    /// tests), building nothing; answered with [`Response::Reached`].
    Reach { from: ViewRef, to: ViewRef, targets: Targets },
    /// Each candidate's ops applied to `base` as a what-if, checked (and tested, with `test`),
    /// all at once: each candidate's responses as [`Response::Of`], then [`Response::Ranked`].
    BestOf {
        base: ViewRef,
        candidates: Vec<Vec<Op>>,
        targets: Targets,
        test: bool,
        filter: Vec<String>,
    },
    /// The production oracle: the view built again from nothing and compared; answered with
    /// [`Response::Verified`].
    Verify { view: ViewRef, targets: Targets },
    /// The daemon's state made durable, before the VM snapshots itself; answered with
    /// [`Response::Snapshotted`] once it is.
    Snapshot,
    /// Every instance the view's last build compiled or found, with the hash its code was
    /// made from; answered with [`Response::CodegenInputs`].
    CodegenInputs { view: ViewRef, targets: Targets },
    /// Every result the view's last build ran or kept, with its edges; answered with
    /// [`Response::Trace`].
    Trace { view: ViewRef, targets: Targets },
    /// The size of each of the daemon's long-lived tables; answered with [`Response::Tables`].
    Tables { view: ViewRef },
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum Response {
    Committed { root: ContentId, rebased: bool },
    /// A diagnostic, as rustc would give it, with its code.
    Diagnostic(Diagnostic),
    /// A build's output, placed. `profile` as cargo's `Finished` line names it:
    /// "`dev` profile [unoptimized + debuginfo]".
    Built { artifacts: Vec<Artifact>, profile: String },
    /// A unit began: cargo's `Compiling foo v0.1.0 (/path)` line.
    Unit(UnitStarted),
    /// A build script ran: cargo's `build-script-executed` message.
    BuildScript(BuildScriptRan),
    /// What a build, test or run did, before its [`Response::Done`].
    Work(Worked),
    TestResult(TestResult),
    Ran { code: Option<i32>, stdout: Vec<u8>, stderr: Vec<u8> },
    Diff { changes: Vec<Change> },
    Why(Why),
    Status(Status),
    /// The request is over; `ok` false after an error or a failed build.
    Done { ok: bool },
    Error(ApiError),
    Opened { root: ContentId },
    Reached(Reach),
    /// A response of candidate `candidate` of a best-of-N.
    Of { candidate: u32, response: Box<Response> },
    /// Best first.
    Ranked(Vec<Ranked>),
    Verified { mismatches: Vec<Mismatch> },
    /// Every head's root, as made durable.
    Snapshotted { heads: Vec<(String, ContentId)> },
    /// Each instance's key (`blaze_codegen::InstanceKey`) with the code hash
    /// (`blaze_items::CodeHash`) it was made from, sorted by key.
    CodegenInputs(Vec<(ContentId, ContentId)>),
    Trace(Vec<Traced>),
    Tables(Vec<Table>),
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Diagnostic {
    pub level: Level,
    /// rustc's error code (`E0308`), or a lint's name.
    pub code: Option<String>,
    pub message: String,
    pub spans: Vec<Span>,
    pub children: Vec<Diagnostic>,
    /// The facts the query that gave it read: what to change to make it go away, and why it
    /// came back after an edit.
    pub facts: Vec<FactRead>,
    /// rustc's text for it, as rustc prints it (in color when the request's
    /// [`Cargo::color`]).
    pub rendered: String,
    /// The unit it is of, for cargo's `compiler-message` and its "generated N warnings".
    pub unit: Option<UnitOf>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct UnitOf {
    pub package_id: String,
    pub manifest_path: String,
    pub target: UnitTarget,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct UnitStarted {
    pub package_id: String,
    pub name: String,
    pub version: String,
    /// What cargo prints after the version: `(/abs/path)` for a path package,
    /// `(https://github.com/o/r#0123abcd)` for a git one, none for a registry's.
    pub source: Option<String>,
    pub verb: Verb,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verb {
    Compiling,
    Checking,
    Fresh,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct BuildScriptRan {
    pub package_id: String,
    pub linked_libs: Vec<String>,
    pub linked_paths: Vec<String>,
    pub cfgs: Vec<String>,
    pub env: Vec<(String, String)>,
    pub out_dir: String,
}

/// A unit's target as cargo's JSON messages name it.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct UnitTarget {
    pub kind: Vec<String>,
    pub crate_types: Vec<String>,
    pub name: String,
    pub src_path: String,
    pub edition: String,
    pub doc: bool,
    pub doctest: bool,
    pub test: bool,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct UnitProfile {
    pub opt_level: String,
    pub debuginfo: u32,
    pub debug_assertions: bool,
    pub overflow_checks: bool,
    pub test: bool,
}

/// What a build did: the counts the oracle's over-invalidation and the budgets read.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct Worked {
    /// Queries executed.
    pub reran: u64,
    /// Queries verified green.
    pub green: u64,
    /// Functions whose code was generated.
    pub compiled: u64,
    /// Functions found in the store.
    pub found: u64,
    pub link: LinkKind,
    /// The plane's critical path of the request.
    pub critical_path_us: u64,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum LinkKind {
    /// The image was patched in place.
    Patched,
    /// The image was placed whole.
    Placed,
}

/// One result of a view's last build ([`Request::Trace`]).
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Traced {
    pub key: QueryKey,
    pub fingerprint: Fingerprint,
    /// In read order.
    pub edges: Vec<blaze_query::Edge>,
    /// The build ran it, rather than keeping it.
    pub ran: bool,
}

/// One of the daemon's long-lived tables ([`Request::Tables`]): `token`, `syntax-node`,
/// `hir-expr`, `mir-statement`, `memo`, `memo-edge`, `relocation` and the rest.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Table {
    pub name: String,
    pub elements: u64,
    /// All its columns and side tables.
    pub bytes: u64,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    Error,
    Warning,
    Note,
    Help,
    /// rustc's closing lines ("For more information about this error, try `rustc --explain
    /// E0308`."): printed with no level before them, `"failure-note"` in its JSON.
    FailureNote,
}

/// A place in a file of the view, by 1-based line and column of characters.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Span {
    pub file: String,
    pub line: u32,
    pub column: u32,
    pub end_line: u32,
    pub end_column: u32,
    pub label: Option<String>,
    pub byte_start: u32,
    pub byte_end: u32,
    pub primary: bool,
    pub replacement: Option<String>,
    /// `MachineApplicable`, `MaybeIncorrect`, `HasPlaceholders`, `Unspecified`.
    pub applicability: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Artifact {
    /// cargo's package id (`path+file:///w/a#0.1.0`).
    pub package: String,
    /// `bin`, `lib`, `test`, `example`, `bench`, `proc-macro`, `custom-build`, `doctest`.
    pub kind: String,
    /// The main file placed (the executable for one that runs), and its content.
    pub path: String,
    pub content: ContentId,
    pub manifest_path: String,
    pub target: UnitTarget,
    pub profile: UnitProfile,
    pub features: Vec<String>,
    /// Every file placed, as cargo's `filenames`.
    pub filenames: Vec<String>,
    pub executable: Option<String>,
    pub fresh: bool,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct TestResult {
    pub binary: String,
    pub name: String,
    pub outcome: TestOutcome,
    pub output: Vec<u8>,
    pub millis: u64,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum TestOutcome {
    Passed,
    Failed,
    Ignored,
    /// Not run: the edit reached none of its code; its last result stands.
    Unreached,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Status {
    pub requests: Vec<u64>,
    pub views: u32,
    pub memory_bytes: u64,
    pub budget_bytes: u64,
}

/// What failed, and where.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct ApiError {
    pub what: String,
    /// A file, a package, a query, a function: where it failed, as precisely as known.
    pub at: Option<String>,
}

impl ApiError {
    pub fn new(what: impl Into<String>, at: Option<String>) -> ApiError {
        ApiError { what: what.into(), at }
    }
}

/// A fact a result read (`blaze_items::Fact::key`), as a diagnostic, a reach or a mismatch
/// names it.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct FactRead {
    pub key: QueryKey,
    /// What it stands for, for people and LLMs: "the signature of `parser::Token::new`".
    pub what: String,
}

/// What a change from one view to another reaches, without building it.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
pub struct Reach {
    pub changes: Vec<Change>,
    /// The items whose hashes changed, each with the facts of it that changed.
    pub items: Vec<ReachedItem>,
    /// The functions that would be compiled again, by path.
    pub compiled: Vec<String>,
    /// The tests whose reached code changed, which a test request would run.
    pub tests: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct ReachedItem {
    /// The item's path, as rustc prints it.
    pub path: String,
    pub facts: Vec<FactRead>,
}

/// One candidate of a best-of-N, as it was ranked: fewest errors, then fewest failed tests,
/// then most passed, then fewest warnings, then its place in the request; a candidate whose
/// ops did not apply or whose job could not run goes after every one that ran.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Ranked {
    pub candidate: u32,
    /// The base's root for a candidate whose ops did not apply.
    pub root: ContentId,
    pub errors: u32,
    pub warnings: u32,
    pub passed: u32,
    pub failed: u32,
    /// The candidate's request ended with `ok` true.
    pub ok: bool,
}

/// A result the database held that a build from nothing made otherwise: the oracle's finding.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Mismatch {
    pub query: QueryKey,
    pub incremental: Fingerprint,
    pub scratch: Fingerprint,
    /// The fact whose edge should have changed and did not, where the oracle found it.
    pub fact: Option<FactRead>,
}

/// A request of a connection: what the server and the session name a reply's destination by.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct RequestRef {
    pub conn: Conn,
    pub id: u64,
}

/// What the server hands the session.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Arrived {
    Request { from: RequestRef, request: Request },
    /// The connection ended: its requests are cancelled and its replies dropped.
    Gone { conn: Conn },
}

/// A response for the server to send.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Reply {
    pub to: RequestRef,
    pub response: Response,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_round_trips_in_deterministic_cbor() {
        let request = Envelope::new(7, Request::Cancel { id: 3 });
        let bytes = blaze_protocol::cbor::to_vec(&request).unwrap();
        let back: Envelope<Request> = blaze_protocol::cbor::from_slice(&bytes).unwrap();
        assert_eq!(back, request);
        assert_eq!(back.schema, SCHEMA);
    }
}

//! The agent API (docs/v2/ci.md, docs/v2/README.md section 1; #277; rule no-mcp): the daemon's
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
pub const SCHEMA: u32 = 3;

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
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum Response {
    Committed { root: ContentId, rebased: bool },
    /// A diagnostic, as rustc would give it, with its code.
    Diagnostic(Diagnostic),
    /// A build's output, placed.
    Built { artifacts: Vec<Artifact> },
    TestResult(TestResult),
    Ran { code: Option<i32>, stdout: Vec<u8>, stderr: Vec<u8> },
    Diff { changes: Vec<Change> },
    Why(Why),
    Status(Status),
    /// The request is over; `ok` false after an error or a failed build.
    Done { ok: bool },
    Error(ApiError),
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Diagnostic {
    pub level: Level,
    /// rustc's error code (`E0308`), or a lint's name.
    pub code: Option<String>,
    pub message: String,
    pub spans: Vec<Span>,
    pub children: Vec<Diagnostic>,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    Error,
    Warning,
    Note,
    Help,
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
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Artifact {
    pub package: String,
    pub kind: String,
    /// Where it was placed, and its content.
    pub path: String,
    pub content: ContentId,
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
/// then most passed, then fewest warnings, then its place in the request.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Ranked {
    pub candidate: u32,
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

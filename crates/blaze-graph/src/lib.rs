//! blaze's build driver as a graph that does no IO (docs/driver.md).
//!
//! [`machine`] is the graph: nodes with explicit states, and a step function from a completion
//! to the requests it makes and the records it says. It reads no file, clock or thread. [`exec`]
//! does what it asks, every ready node at once as the pools allow, and feeds back what came of
//! it. [`trace`] keeps a build's inputs, so a build replays as a test.
//!
//! [`plane`] is the same graph for every request a daemon has under way
//! (docs/v2/plan/lanes/core/plane.md): single flight, fair share, critical path and memory
//! admission, with [`admission`]'s estimates of what each task holds and [`arena`]'s measure
//! and cap of what it does hold. It remembers no result: a key done before is the [`Memo`]'s,
//! the query database's. [`Machine`] is the plane with one request.
//!
//! [`pool`] is the threads the plane's tasks run on, one per core, stealing work nearest first
//! by [`topology`] (a CCD or a cluster, then a socket, then any). [`pump`] is the rule for a
//! wait inside a task (#1301): the waiting thread runs other work, and a wait that would close
//! a cycle is [`Cancelled`] and its task posted again. [`build`] says a build's nodes:
//! the workspace load, codegen with its store writes, links and test runs (docs/driver.md
//! steps 4 to 7).

pub mod admission;
pub mod arena;
pub mod build;
pub mod exec;
pub mod history;
pub mod machine;
pub mod plane;
pub mod pool;
pub mod pump;
mod sync;
pub mod topology;
pub mod trace;

pub use machine::{Config, Error, Input, Machine, NodeId, Outcome, Output, Pool, Spec};
pub use plane::{
    Budget, Key, Memo, Plane, PlaneConfig, PlaneInput, PlaneOutput, RequestId, RequestStats,
};

/// A task stopped before its end because going on could not end: a wait that would close a
/// cycle of waits (#1301), memory that could not be had while every holder waits for more, or
/// its request ended. Its task is posted again whole; nothing of it is kept.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled, to be run again")
    }
}

impl std::error::Error for Cancelled {}

#[cfg(test)]
#[allow(clippy::disallowed_types, reason = "a test's own bookkeeping, off every hot path")]
mod plane_tests;
#[cfg(test)]
#[allow(clippy::disallowed_types, reason = "a test's own bookkeeping, off every hot path")]
mod tests;

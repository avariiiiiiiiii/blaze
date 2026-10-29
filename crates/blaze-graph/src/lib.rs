//! blaze's build driver as a graph that does no IO (docs/driver.md).
//!
//! [`machine`] is the graph: nodes with explicit states, and a step function from a completion
//! to the requests it makes and the records it says. It reads no file, clock or thread. [`exec`]
//! does what it asks, every ready node at once as the pools allow, and feeds back what came of
//! it. [`trace`] keeps a build's inputs, so a build replays as a test.
//!
//! [`plane`] is the same graph for every request a daemon has under way (docs/v2/plane.md):
//! memo by key, single flight, fair share, critical path and memory admission, with
//! [`admission`]'s estimates of what each task holds.

pub mod admission;
pub mod exec;
pub mod machine;
pub mod plane;
pub mod trace;

pub use machine::{Config, Error, Input, Machine, NodeId, Outcome, Output, Pool, Spec};
pub use plane::{Budget, Key, Plane, PlaneConfig, PlaneInput, PlaneOutput, RequestId};

#[cfg(test)]
mod tests;

//! blaze's build driver as a graph that does no IO (docs/driver.md).
//!
//! [`machine`] is the graph: nodes with explicit states, and a step function from a completion
//! to the requests it makes and the records it says. It reads no file, clock or thread. [`exec`]
//! does what it asks, every ready node at once as the pools allow, and feeds back what came of
//! it. [`trace`] keeps a build's inputs, so a build replays as a test.

pub mod exec;
pub mod machine;
pub mod trace;

pub use machine::{Config, Error, Input, Machine, NodeId, Outcome, Output, Pool, Spec};

#[cfg(test)]
mod tests;

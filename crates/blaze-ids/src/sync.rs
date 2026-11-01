//! The synchronization types of [`crate::Sharded`] and the domain keys: std's, or the model
//! checker's in a model build (docs/v2/plan/lanes/safety/model.md, "The switch").

#[cfg(not(blaze_check))]
#[allow(clippy::disallowed_types, reason = "named only by `Shard::lock`, which allows it")]
pub(crate) use std::sync::{Mutex, OnceLock, PoisonError, atomic};

#[cfg(blaze_check)]
pub(crate) use blaze_check::sync::{Mutex, OnceLock, PoisonError, atomic};

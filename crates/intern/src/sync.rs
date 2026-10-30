//! The synchronization types of [`crate::arena`]: std's, or the model checker's in a model build
//! (docs/v2/plan/lanes/safety/model.md, "The switch").

#[cfg(not(blaze_check))]
pub(crate) use std::sync::{Mutex, MutexGuard, PoisonError, atomic};

#[cfg(blaze_check)]
pub(crate) use blaze_check::sync::{Mutex, MutexGuard, PoisonError, atomic};

//! The synchronization types of [`crate::arena`] and the stores of [`crate::intern`] and
//! [`crate::intern_slice`]: std's, or the model checker's in a model build
//! (docs/v2/plan/lanes/safety/model.md, "The switch").

#[cfg(not(blaze_check))]
#[allow(clippy::disallowed_types, reason = "named only by `arena::lock::InsertLock`, which allows it")]
pub(crate) use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError, atomic};

#[cfg(blaze_check)]
pub(crate) use blaze_check::sync::{Mutex, MutexGuard, OnceLock, PoisonError, atomic};

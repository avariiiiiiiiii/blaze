//! The synchronization types of the crate's lock-free code: `std`'s, or under `--cfg blaze_check`
//! the model checker's (docs/v2/plan/lanes/safety/model.md, "The switch"). `arena`'s allocator
//! counts stay `std`'s: the checker allocates, and what it allocates goes through them.

#[cfg(not(blaze_check))]
#[allow(clippy::disallowed_types, reason = "named only by `pump::Lock`, which allows it")]
pub(crate) use std::{
    hint,
    sync::{Condvar, Mutex, MutexGuard, OnceLock, atomic},
    thread, thread_local,
};

#[cfg(blaze_check)]
pub(crate) use blaze_check::{
    hint,
    sync::{Condvar, Mutex, MutexGuard, OnceLock, atomic},
    thread, thread_local,
};

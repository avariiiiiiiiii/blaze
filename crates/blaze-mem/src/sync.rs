//! The atomics the lock-free stacks, pools, counters and [`crate::swiss::Index`] use, and the
//! lock and condition variable of the frame cap: `std`'s, or under `cfg(blaze_check)` the model
//! checker's (docs/v2/plan/lanes/safety/model.md "The
//! switch").

#[cfg(not(blaze_check))]
pub(crate) use std::sync::{Condvar, Mutex, MutexGuard, OnceLock, PoisonError, atomic};

#[cfg(blaze_check)]
pub(crate) use blaze_check::sync::{
    Condvar, Mutex, MutexGuard, OnceLock, PoisonError, atomic,
};

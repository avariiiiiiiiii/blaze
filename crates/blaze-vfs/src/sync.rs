//! The synchronization types of the store's lock-free code: std's, or the model checker's in a
//! model build (docs/v2/plan/lanes/safety/model.md), where nothing else changes.

#[cfg(not(blaze_check))]
pub(crate) use std::sync::{Mutex, PoisonError, atomic};

#[cfg(blaze_check)]
pub(crate) use blaze_check::{
    sync::{Mutex, PoisonError, atomic},
    thread,
};

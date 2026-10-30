//! SIGTERM and SIGINT as a flag the loop reads between turns. The one place `serve` uses
//! `unsafe`: installing a handler is a libc call, and std has no safe way to do it.

use std::sync::atomic::{AtomicBool, Ordering};

static SIGNALLED: AtomicBool = AtomicBool::new(false);

/// Whether SIGTERM or SIGINT has arrived since [`catch`].
pub fn signalled() -> bool {
    SIGNALLED.load(Ordering::Acquire)
}

/// Turns SIGTERM and SIGINT into [`signalled`] for the rest of the process.
#[cfg(unix)]
pub fn catch() {
    extern "C" fn on(_: libc::c_int) {
        SIGNALLED.store(true, Ordering::Release);
    }
    let handler = on as extern "C" fn(libc::c_int) as libc::sighandler_t;
    // SAFETY: `handler` is `on`, an `extern "C" fn(c_int)` that only stores to an atomic, which
    // is async-signal-safe; were it to lock or allocate, a signal arriving inside malloc could
    // deadlock or corrupt the heap.
    unsafe { libc::signal(libc::SIGTERM, handler) };
    // SAFETY: the same handler as for SIGTERM, which only stores to an atomic.
    unsafe { libc::signal(libc::SIGINT, handler) };
}

#[cfg(not(unix))]
pub fn catch() {}

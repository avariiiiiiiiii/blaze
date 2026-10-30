//! The scope of each expansion, for the sandbox blaze runs a warm server in (proc-macro-api's
//! `version::SCOPED_EXPANSIONS`): the thread that runs the `n`-th expansion of the server,
//! counted from 0 over every expansion it was asked for, says `expand-<n>` before the macro
//! runs and nothing after. The sandbox writes what a thread reads with the scope it said
//! last, so the client can tell each expansion's reads apart.

#[cfg(unix)]
use std::ffi::{CString, c_char, c_int};

/// What the sandbox takes as a marker: `access` of a path under it, which names no file.
#[cfg(unix)]
const MARK: &str = "/.blaze-test/";

/// What a scope starts with: the expansion's number follows.
pub(crate) const SCOPE: &str = "expand-";

#[cfg(unix)]
unsafe extern "C" {
    fn access(path: *const c_char, mode: c_int) -> c_int;
}

/// No sandbox blaze has elsewhere traces, so there is nothing to say.
#[cfg(not(unix))]
fn say(_: &str) {}

#[cfg(unix)]
fn say(name: &str) {
    let Ok(path) = CString::new(format!("{MARK}{name}")) else { return };
    // SAFETY: `path` is a C string that lives across the call; `access` reads it and nothing
    // else. It fails, as no such file is there, and the failure is the point: outside a
    // sandbox the call does nothing.
    _ = unsafe { access(path.as_ptr(), 0) };
}

/// Marks this thread as running the expansion `number` until [`Marked`] is dropped.
pub(crate) fn mark(number: u64) -> Marked {
    say(&format!("{SCOPE}{number}"));
    Marked(())
}

/// The scope of this thread, which ends with it.
pub(crate) struct Marked(());

impl Drop for Marked {
    fn drop(&mut self) {
        say("");
    }
}

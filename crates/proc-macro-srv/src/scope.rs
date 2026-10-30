//! The scope of each expansion, for the sandbox blaze runs a warm server in (proc-macro-api's
//! `version::SCOPED_EXPANSIONS`): the thread that runs the `n`-th expansion of the server,
//! counted from 0 over every expansion it was asked for, says `expand-<n>` before the macro
//! runs and nothing after. The sandbox writes what a thread reads with the scope it said
//! last, so the client can tell each expansion's reads apart.

#[cfg(unix)]
use std::{
    ffi::{CStr, c_char, c_int},
    io::Write,
};

/// What the sandbox takes as a marker: `access` of a path under it, which names no file.
#[cfg(unix)]
const MARK: &str = "/.blaze-test/";

/// What a scope starts with: the expansion's number follows.
pub(crate) const SCOPE: &str = "expand-";

/// The longest marker said, with its NUL: [`MARK`], [`SCOPE`] and a `u64`'s 20 digits.
#[cfg(unix)]
const LONGEST: usize = MARK.len() + SCOPE.len() + 20 + 1;

#[cfg(unix)]
unsafe extern "C" {
    fn access(path: *const c_char, mode: c_int) -> c_int;
}

/// No sandbox blaze has elsewhere traces, so there is nothing to say.
#[cfg(not(unix))]
fn say(_: Option<u64>) {}

/// Says the scope of expansion `number`, or none: the marker is written on the stack.
#[cfg(unix)]
fn say(number: Option<u64>) {
    let mut path = [0; LONGEST];
    let mut rest = &mut path[..LONGEST - 1];
    let written = match number {
        Some(number) => write!(rest, "{MARK}{SCOPE}{number}"),
        None => write!(rest, "{MARK}"),
    };
    if written.is_err() {
        return;
    }
    let Ok(path) = CStr::from_bytes_until_nul(&path) else { return };
    // SAFETY: `path` is a C string that lives across the call; `access` reads it and nothing
    // else. It fails, as no such file is there, and the failure is the point: outside a
    // sandbox the call does nothing.
    _ = unsafe { access(path.as_ptr(), 0) };
}

/// Marks this thread as running the expansion `number` until [`Marked`] is dropped.
pub(crate) fn mark(number: u64) -> Marked {
    say(Some(number));
    Marked(())
}

/// The scope of this thread, which ends with it.
pub(crate) struct Marked(());

impl Drop for Marked {
    fn drop(&mut self) {
        say(None);
    }
}

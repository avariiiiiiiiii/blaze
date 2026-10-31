//! SIGTERM and SIGINT as a flag the loop reads between turns, and a line in the daemon's log
//! for each signal that ends it ([`record_ends`]). The one place `serve` uses `unsafe`:
//! installing a handler is a libc call, and std has no safe way to do it.

use std::sync::atomic::{AtomicBool, Ordering};

/// What the line a signal that ends the daemon writes starts with, then its pid, then
/// [`ENDED_BY`], the signal's number and its name: `blaze serve: pid 7 ended by signal 6,
/// SIGABRT: process abort signal`. The client quotes it from the log.
const PID_SAYS: &str = "blaze serve: pid ";
const ENDED_BY: &str = " ended by signal ";

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

/// The signals that end a process by default and that a handler sees: SIGKILL and SIGSTOP
/// can't be caught, std ignores SIGPIPE, [`catch`] has SIGTERM and SIGINT, and std's own
/// SIGSEGV and SIGBUS handlers say a stack overflow (then abort: SIGABRT, here) and leave
/// any other fault to the default, which says nothing.
#[cfg(unix)]
const ENDING: [libc::c_int; 14] = [
    libc::SIGHUP,
    libc::SIGQUIT,
    libc::SIGILL,
    libc::SIGTRAP,
    libc::SIGABRT,
    libc::SIGFPE,
    libc::SIGUSR1,
    libc::SIGUSR2,
    libc::SIGALRM,
    libc::SIGXCPU,
    libc::SIGXFSZ,
    libc::SIGVTALRM,
    libc::SIGPROF,
    libc::SIGSYS,
];

/// Has each of [`ENDING`] that has its default action write `blaze serve: pid N ended by
/// signal S, NAME: ...` to stderr (the daemon's log) and then end the process as the default
/// would, so a client the daemon hung up on can say how it ended. A signal something else
/// handles or ignores is left so. A process forked from the daemon has the handler until it
/// runs a program (blaze-core's spawn resets every one before its exec) and says its own pid.
#[cfg(unix)]
pub fn record_ends() {
    extern "C" fn ended(signal: libc::c_int) {
        let mut line = Line { bytes: [0; 160], len: 0 };
        line.put(PID_SAYS.as_bytes());
        // SAFETY: `getpid` takes nothing and is async-signal-safe.
        line.number(u64::from(unsafe { libc::getpid() }.unsigned_abs()));
        line.put(ENDED_BY.as_bytes());
        line.number(u64::from(signal.unsigned_abs()));
        line.put(blaze_core::signal::described(signal).as_bytes());
        line.put(b"\n");
        // SAFETY: `line.bytes` is live for the call and `line.len` of it are written; `write`
        // is async-signal-safe. A short or failed write loses the line, nothing else.
        unsafe { libc::write(libc::STDERR_FILENO, line.bytes.as_ptr().cast(), line.len) };
        // SAFETY: `SA_RESETHAND` made the signal's action the default again as this handler
        // was entered, and `SA_NODEFER` left it unblocked, so this ends the process as the
        // signal would have; `raise` is async-signal-safe.
        unsafe { libc::raise(signal) };
    }
    for signal in ENDING {
        // SAFETY: `sigaction` is integers, a mask of integers and a handler address, for
        // which all-zero bytes are a valid value (no handler, no flags, an empty mask).
        let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
        // SAFETY: `old` is live for the call to fill; a null new action changes nothing.
        if unsafe { libc::sigaction(signal, std::ptr::null(), &mut old) } != 0 || old.sa_sigaction != libc::SIG_DFL {
            continue;
        }
        // SAFETY: as for `old`.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = ended as extern "C" fn(libc::c_int) as libc::sighandler_t;
        action.sa_flags = libc::SA_RESETHAND | libc::SA_NODEFER | libc::SA_ONSTACK;
        // SAFETY: `action.sa_mask` is a live `sigset_t` of this frame for the call to fill.
        unsafe { libc::sigemptyset(&mut action.sa_mask) };
        // SAFETY: `action` is live and whole for the call, and a null old action is allowed.
        // Its handler `ended` writes from its own stack and calls `getpid`, `write` and
        // `raise`, all async-signal-safe: it neither allocates nor locks, which could deadlock
        // a process interrupted inside malloc or holding the lock.
        unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) };
    }
}

#[cfg(not(unix))]
pub fn record_ends() {}

/// A line built in a signal handler: on the stack, cut at its room, never allocated.
#[cfg(unix)]
struct Line {
    bytes: [u8; 160],
    len: usize,
}

#[cfg(unix)]
impl Line {
    fn put(&mut self, bytes: &[u8]) {
        let room = &mut self.bytes[self.len..];
        let taken = bytes.len().min(room.len());
        room[..taken].copy_from_slice(&bytes[..taken]);
        self.len += taken;
    }

    fn number(&mut self, mut value: u64) {
        let mut digits = [0u8; 20];
        let mut at = digits.len();
        loop {
            at -= 1;
            digits[at] = b'0' + (value % 10) as u8;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        self.put(&digits[at..]);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_line_says_its_numbers_and_is_cut_at_its_room() {
        let mut line = Line { bytes: [0; 160], len: 0 };
        line.put(PID_SAYS.as_bytes());
        line.number(4_294_967_295);
        line.put(ENDED_BY.as_bytes());
        line.number(0);
        assert_eq!(&line.bytes[..line.len], b"blaze serve: pid 4294967295 ended by signal 0");
        line.put(&[b'x'; 200]);
        assert_eq!(line.len, 160);
    }
}

//! The lines `blaze` prints itself, in cargo's shape: a status word right-aligned in
//! twelve columns, on standard error.

use std::io::IsTerminal as _;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::{Duration, Instant};

const GREEN: &str = "\x1b[1m\x1b[92m";
const CYAN: &str = "\x1b[1m\x1b[96m";
const YELLOW: &str = "\x1b[1m\x1b[93m";
const RED: &str = "\x1b[1m\x1b[91m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

/// What the command's `--color` said: [`AUTO`], [`ALWAYS`] or [`NEVER`].
static COLOR: AtomicU8 = AtomicU8::new(AUTO);
const AUTO: u8 = 0;
const ALWAYS: u8 = 1;
const NEVER: u8 = 2;

/// Has the lines colored as `when` says, cargo's `--color`, or else as `CARGO_TERM_COLOR`
/// does: `always`, `never`, or `auto`, when standard error is a terminal.
pub fn color(when: Option<&str>) {
    let when = when.map(str::to_owned).or_else(|| std::env::var("CARGO_TERM_COLOR").ok());
    let when = match when.as_deref() {
        Some("always") => ALWAYS,
        Some("never") => NEVER,
        _ => AUTO,
    };
    COLOR.store(when, Ordering::Relaxed);
}

/// Whether the lines are colored.
pub fn colored() -> bool {
    match COLOR.load(Ordering::Relaxed) {
        ALWAYS => true,
        NEVER => false,
        _ => std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
    }
}

/// Whether what goes to standard output is colored: as [`colored`], with standard output's
/// terminal for `auto`.
pub fn colored_out() -> bool {
    match COLOR.load(Ordering::Relaxed) {
        ALWAYS => true,
        NEVER => false,
        _ => std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
    }
}

fn painted(color: &str, word: &str) -> String {
    match colored() {
        true => format!("{color}{word}{RESET}"),
        false => word.to_owned(),
    }
}

/// `     Running target/debug/app`, unless the build is quiet.
pub fn status(quiet: bool, word: &str, rest: &str) {
    if !quiet {
        eprintln!("{} {rest}", painted(GREEN, &format!("{word:>12}")));
    }
}

/// Why cargo does what the daemon would have. Never quiet: a build that went another way
/// than asked says so.
pub fn fallback(reason: &str, then: &str) {
    eprintln!("{} {reason}; {then}", painted(YELLOW, "blaze:"));
}

pub fn error(text: &str) {
    eprintln!("{}: {text}", painted(RED, "error"));
}

/// [`error`] with no newline after it, as cargo ends a few of clap's.
pub fn error_unended(text: &str) {
    eprint!("{}: {text}", painted(RED, "error"));
}

pub fn warning(text: &str) {
    eprintln!("{}: {text}", painted(YELLOW, "warning"));
}

pub fn note(text: &str) {
    eprintln!("{}: {text}", painted(BOLD, "note"));
}

/// A note as cargo's shell says one (`Shell::note`), green, which a diagnostic's is not.
pub fn shell_note(text: &str) {
    eprintln!("{}: {text}", painted(GREEN, "note"));
}

/// Whether the command was asked to be quiet (`-q`), which shows no [`Waiting`].
static QUIET: AtomicBool = AtomicBool::new(false);

pub(crate) fn quiet(quiet: bool) {
    QUIET.store(quiet, Ordering::Relaxed);
}

/// The columns of the terminal standard error is, which cargo passes rustc as
/// `--diagnostic-width`. None on Windows, which has no `TIOCGWINSZ`.
#[cfg(not(unix))]
pub(crate) fn terminal_width() -> Option<usize> {
    None
}

/// The columns of the terminal standard error is, which cargo passes rustc as
/// `--diagnostic-width`.
#[cfg(unix)]
pub(crate) fn terminal_width() -> Option<usize> {
    if !std::io::stderr().is_terminal() {
        return None;
    }
    // SAFETY: `TIOCGWINSZ` writes a `winsize` and nothing else.
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    let got = unsafe { libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ, &mut size) };
    (got == 0 && size.ws_col > 0).then_some(usize::from(size.ws_col))
}

/// How long a command waits before a terminal shows what it waits for: an answer sooner
/// than this is shown nothing of, as cargo shows no progress bar of a quick build.
const SHOWN_AFTER: Duration = Duration::from_millis(500);

/// What a command that waits for the daemon shows of what the daemon says it is at. On a
/// terminal: one line, written over as it changes and with the seconds of the step, taken
/// away when the answer comes, as cargo's progress bar is. Elsewhere: a line for each step
/// the worker names, once, and nothing of a wait it names no step of (a build of an edit),
/// for a log to read as cargo's.
pub(crate) struct Waiting {
    asked: Instant,
    /// The word of a wait whose step is not said: `Building`, `Checking`.
    doing: &'static str,
    step: Option<((String, String), Instant)>,
    drawn: bool,
}

impl Waiting {
    pub(crate) fn new(doing: &'static str) -> Waiting {
        Waiting { asked: Instant::now(), doing, step: None, drawn: false }
    }

    pub(crate) fn show(&mut self, step: Option<(String, String)>) {
        if QUIET.load(Ordering::Relaxed) {
            return;
        }
        let terminal = std::io::stderr().is_terminal();
        let changed = self.step.as_ref().map(|it| &it.0) != step.as_ref();
        if changed {
            self.step = step.map(|it| (it, Instant::now()));
            if !terminal && let Some(((word, what), _)) = &self.step {
                status(false, word, what);
            }
        }
        if !terminal || self.asked.elapsed() < SHOWN_AFTER {
            return;
        }
        let ((word, what), since) = match &self.step {
            Some(((word, what), since)) => ((word.as_str(), what.as_str()), *since),
            None => ((self.doing, ""), self.asked),
        };
        let secs = since.elapsed().as_secs();
        let after = if secs == 0 { String::new() } else { format!(" ({secs} s)") };
        // A line longer than the terminal wraps, and is then not one to write over.
        let room = terminal_width().unwrap_or(80).saturating_sub(12 + 1 + after.len() + 1);
        let what: String = match what.chars().count() > room {
            true => what.chars().take(room.saturating_sub(3)).chain("...".chars()).collect(),
            false => what.to_owned(),
        };
        eprint!("\r{} {what}{after}\x1b[K", painted(CYAN, &format!("{word:>12}")));
        self.drawn = true;
    }
}

impl Drop for Waiting {
    fn drop(&mut self) {
        if self.drawn {
            eprint!("\r\x1b[K");
        }
    }
}

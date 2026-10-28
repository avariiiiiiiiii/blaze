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
pub(crate) fn color(when: Option<&str>) {
    let when = when.map(str::to_owned).or_else(|| std::env::var("CARGO_TERM_COLOR").ok());
    let when = match when.as_deref() {
        Some("always") => ALWAYS,
        Some("never") => NEVER,
        _ => AUTO,
    };
    COLOR.store(when, Ordering::Relaxed);
}

/// Whether the lines are colored.
pub(crate) fn colored() -> bool {
    match COLOR.load(Ordering::Relaxed) {
        ALWAYS => true,
        NEVER => false,
        _ => std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
    }
}

/// Whether what goes to standard output is colored: as [`colored`], with standard output's
/// terminal for `auto`.
pub(crate) fn colored_out() -> bool {
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
pub(crate) fn status(quiet: bool, word: &str, rest: &str) {
    if !quiet {
        eprintln!("{} {rest}", painted(GREEN, &format!("{word:>12}")));
    }
}

/// Why cargo does what the daemon would have. Never quiet: a build that went another way
/// than asked says so.
pub(crate) fn fallback(reason: &str, then: &str) {
    eprintln!("{} {reason}; {then}", painted(YELLOW, "blaze:"));
}

pub(crate) fn error(text: &str) {
    eprintln!("{}: {text}", painted(RED, "error"));
}

/// [`error`] with no newline after it, as cargo ends a few of clap's.
pub(crate) fn error_unended(text: &str) {
    eprint!("{}: {text}", painted(RED, "error"));
}

pub(crate) fn warning(text: &str) {
    eprintln!("{}: {text}", painted(YELLOW, "warning"));
}

pub(crate) fn note(text: &str) {
    eprintln!("{}: {text}", painted(BOLD, "note"));
}

/// A note as cargo's shell says one (`Shell::note`), green, which a diagnostic's is not.
pub(crate) fn shell_note(text: &str) {
    eprintln!("{}: {text}", painted(GREEN, "note"));
}

/// Whether the command was asked to be quiet (`-q`), which shows no [`Shown`].
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

/// What `--events` asked: the records of the build on standard output, as JSON lines or as
/// the frames of [`blaze_protocol`]. JSON is a rendering of the same records, lossless.
static EVENTS: AtomicU8 = AtomicU8::new(NONE);
const NONE: u8 = 0;
const JSON: u8 = 1;
const CBOR: u8 = 2;

pub(crate) fn events(format: &str) -> Result<(), String> {
    let format = match format {
        "json" => JSON,
        "cbor" => CBOR,
        other => return Err(format!("`--events {other}`: the records are `json` or `cbor`")),
    };
    EVENTS.store(format, Ordering::Relaxed);
    Ok(())
}

/// Writes `record` to standard output as `--events` asked.
fn event_out(record: &blaze_protocol::events::Record) {
    use std::io::Write as _;
    let mut out = std::io::stdout().lock();
    let _ = match EVENTS.load(Ordering::Relaxed) {
        JSON => serde_json::to_string(record).map_or(Ok(()), |line| writeln!(out, "{line}")),
        CBOR => match blaze_protocol::cbor::to_vec(record) {
            Ok(item) => blaze_protocol::write_frame(&mut out, &item),
            Err(_) => Ok(()),
        },
        _ => return,
    };
    let _ = out.flush();
}

/// How long a command waits before a terminal shows what it waits for: an answer sooner
/// than this is shown nothing of but its last line, as cargo shows no progress bar of a
/// quick build.
const SHOWN_AFTER: Duration = Duration::from_millis(500);

/// What a command that waits for the daemon shows of the build's records
/// ([`blaze_protocol::events`]). On a terminal: a region below the lines that stay, drawn again as
/// records come and time passes, and at the end the build's last line in its place
/// ([`blaze_ui::Live`]). Elsewhere: cargo's lines, for a log ([`blaze_ui::Plain`]). Nothing
/// when the command is quiet.
pub(crate) struct Shown {
    asked: Instant,
    to: Option<Rendering>,
    /// What a terminal is not shown yet, before [`SHOWN_AFTER`].
    held: Option<String>,
    /// Every record, for `blaze why` ([`super::why::keep`]).
    records: Vec<blaze_protocol::events::Record>,
}

enum Rendering {
    Live(blaze_ui::Live),
    Plain(blaze_ui::Plain),
}

impl Shown {
    pub(crate) fn new() -> Shown {
        let terminal = std::io::stderr().is_terminal();
        let to = match () {
            () if QUIET.load(Ordering::Relaxed) => None,
            () if terminal => {
                let width = terminal_width().unwrap_or(80);
                Some(Rendering::Live(blaze_ui::Live::new(width, colored())))
            }
            () => Some(Rendering::Plain(blaze_ui::Plain::new(colored()))),
        };
        let held = terminal.then(String::new);
        Shown { asked: Instant::now(), to, held, records: Vec::new() }
    }

    fn now_ns(&self) -> u64 {
        u64::try_from(self.asked.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }

    pub(crate) fn record(&mut self, record: &blaze_protocol::events::Record) {
        event_out(record);
        self.records.push(record.clone());
        let now_ns = self.now_ns();
        let out = match &mut self.to {
            Some(Rendering::Live(live)) => live.record(record, now_ns),
            Some(Rendering::Plain(plain)) => plain.record(record),
            None => return,
        };
        self.write(&out);
    }

    /// Draws the region again for the time that passed, or says a step is still under way.
    pub(crate) fn tick(&mut self) {
        let now_ns = self.now_ns();
        let out = match &mut self.to {
            Some(Rendering::Live(live)) => {
                if let Some(width) = terminal_width() {
                    live.resize(width);
                }
                live.tick(now_ns)
            }
            Some(Rendering::Plain(plain)) => plain.tick(now_ns),
            None => return,
        };
        self.write(&out);
    }

    /// The answer came: the region is taken away, and what was held back of a quick build is
    /// its last line alone. The records are kept for `blaze why`.
    pub(crate) fn done(&mut self) {
        super::why::keep(&std::mem::take(&mut self.records));
        let Some(Rendering::Live(live)) = &mut self.to else { return };
        // The record of the build's end drew its last line in the region's place already.
        let out = match live.build().finished {
            Some(_) => String::new(),
            None => live.finish(),
        };
        match self.held.take() {
            Some(_) => {
                let last = live.build().finished.as_ref().filter(|it| !blaze_ui::fresh(it));
                let last = last.map(|it| blaze_ui::summary_line(it, colored()) + "\n");
                eprint!("{}", last.unwrap_or_default());
            }
            None => eprint!("{out}"),
        }
        self.to = None;
    }

    fn write(&mut self, out: &str) {
        if let Some(held) = &mut self.held {
            held.push_str(out);
            if self.asked.elapsed() < SHOWN_AFTER {
                return;
            }
            let held = self.held.take().unwrap_or_default();
            eprint!("{held}");
            return;
        }
        eprint!("{out}");
    }
}

impl Drop for Shown {
    fn drop(&mut self) {
        if let Some(Rendering::Live(live)) = &mut self.to
            && self.held.is_none()
            && live.build().finished.is_none()
        {
            eprint!("{}", live.finish());
        }
    }
}

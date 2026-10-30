//! cargo's own lines as text (`core/shell.rs`, `core/compiler/job_queue/mod.rs`,
//! `core/compiler/mod.rs`, `cargo-util`'s `ProcessBuilder` and `ProcessError`): a status
//! word right-aligned in twelve columns, `error:` and `warning:`, the `Finished` line, a unit's
//! name, a command line as cargo quotes it. Every function here is pure; the caller writes the
//! text to the stream cargo writes it to.

use std::{path::Path, time::Duration};

use blaze_api::UnitTarget;

/// cargo's `HEADER` and `GOOD` (`util/style.rs`): bright green, bold.
pub const GOOD: &str = "\x1b[1m\x1b[92m";
/// cargo's `ERROR`: annotate-snippets' bright red, bold.
pub const ERROR: &str = "\x1b[1m\x1b[91m";
/// cargo's `WARN` and `INVALID`: annotate-snippets' bright yellow, bold.
pub const WARN: &str = "\x1b[1m\x1b[93m";
/// cargo's `VALID` and `LITERAL`: bright cyan, bold.
pub const VALID: &str = "\x1b[1m\x1b[96m";
/// cargo's `PLACEHOLDER`: cyan.
pub const PLACEHOLDER: &str = "\x1b[36m";
pub const BOLD: &str = "\x1b[1m";
pub const RESET: &str = "\x1b[0m";

/// `text` in `style` when `color`.
pub fn paint(style: &str, text: &str, color: bool) -> String {
    match color {
        true => format!("{style}{text}{RESET}"),
        false => text.to_owned(),
    }
}

/// `   Compiling foo v0.1.0 (/w/foo)`: cargo's `Shell::status`, with its newline.
pub fn status(word: &str, rest: &str, color: bool) -> String {
    let mut line = String::with_capacity(rest.len() + 24);
    status_word(&mut line, word, color);
    line.push_str(rest);
    line.push('\n');
    line
}

/// The start of a [`status`] line in `out`: `word` right-aligned in twelve columns, then a space.
pub fn status_word(out: &mut String, word: &str, color: bool) {
    if color {
        out.push_str(GOOD);
    }
    out.extend(std::iter::repeat_n(' ', 12usize.saturating_sub(word.len())));
    out.push_str(word);
    if color {
        out.push_str(RESET);
    }
    out.push(' ');
}

/// `error: what`: cargo's `Shell::error`, with its newline.
pub fn error(what: &str, color: bool) -> String {
    format!("{}{} {what}\n", paint(ERROR, "error", color), paint(BOLD, ":", color))
}

/// `warning: what`: cargo's `Shell::warn`, with its newline.
pub fn warning(what: &str, color: bool) -> String {
    format!("{}{} {what}\n", paint(WARN, "warning", color), paint(BOLD, ":", color))
}

/// A time as cargo's `util::elapsed` writes it: `0.12s`, `1m 05s`.
pub fn elapsed(took: Duration) -> String {
    let secs = took.as_secs();
    match secs >= 60 {
        true => format!("{}m {:02}s", secs / 60, secs % 60),
        false => format!("{secs}.{:02}s", took.subsec_nanos() / 10_000_000),
    }
}

/// What follows `Finished`: `` `dev` profile [unoptimized + debuginfo] target(s) in 0.12s ``,
/// of the daemon's `profile` (`Response::Built`).
pub fn finished(profile: &str, took: Duration) -> String {
    format!("{profile} target(s) in {}", elapsed(took))
}

/// A package as cargo's status lines name it: `foo v0.1.0 (/w/foo)`, `serde v1.0.0`.
pub fn package(name: &str, version: &str, source: Option<&str>) -> String {
    match source {
        Some(source) => format!("{name} v{version} {source}"),
        None => format!("{name} v{version}"),
    }
}

/// A target as cargo's `Target::description_named` names it: `lib`, `bin "foo"`,
/// `build script`.
pub fn target_named(target: &UnitTarget) -> String {
    match target.kind.first().map(String::as_str) {
        Some("bin") => format!("bin \"{}\"", target.name),
        Some("test") => format!("test \"{}\"", target.name),
        Some("bench") => format!("bench \"{}\"", target.name),
        Some("example") => format!("example \"{}\"", target.name),
        Some("custom-build") => "build script".to_owned(),
        _ => "lib".to_owned(),
    }
}

/// A unit as cargo's `descriptive_pkg_name` names it: `` `foo` (lib) ``, `` `foo` (lib test) ``
/// for one compiled with `--test` that is no test or bench target.
pub fn unit_named(package: &str, target: &UnitTarget, test: bool) -> String {
    let kind = target.kind.first().map(String::as_str);
    let mode = match test && !matches!(kind, Some("test" | "bench")) {
        true => " test",
        false => "",
    };
    format!("`{package}` ({}{mode})", target_named(target))
}

/// The package name in cargo's package id: `path+file:///w/foo#0.1.0` is `foo`,
/// `registry+https://github.com/rust-lang/crates.io-index#serde@1.0.0` is `serde`.
pub fn package_name(package_id: &str) -> &str {
    let (url, fragment) = package_id.split_once('#').unwrap_or((package_id, ""));
    if let Some((name, _version)) = fragment.split_once('@') {
        return name;
    }
    let url = url.split(['?', '#']).next().unwrap_or(url);
    url.trim_end_matches('/').rsplit('/').next().unwrap_or(url)
}

/// The version in cargo's package id: the part after `@`, or the whole fragment.
pub fn package_version(package_id: &str) -> &str {
    let fragment = package_id.split_once('#').map_or("", |(_, it)| it);
    fragment.split_once('@').map_or(fragment, |(_, version)| version)
}

/// What a unit's warnings came to, once it ended (`JobQueue::report_warning_count`).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Warned {
    pub total: u32,
    pub duplicates: u32,
    /// The warnings with a machine-applicable suggestion; `None` once the unit has an error,
    /// which takes `cargo fix`'s hint away.
    pub fixable: Option<u32>,
}

/// `` `foo` (lib) generated 2 warnings (1 duplicate) (run `cargo fix --lib -p foo` to apply 1
/// suggestion) ``: what follows `warning:` for a unit with warnings, `None` for one without.
/// The hint is for a local unit that is no build script.
pub fn generated(package: &str, target: &UnitTarget, test: bool, local: bool, warned: Warned) -> Option<String> {
    if warned.total == 0 {
        return None;
    }
    let mut said = format!("{} generated ", unit_named(package, target, test));
    match warned.total {
        1 => said.push_str("1 warning"),
        n => said.push_str(&format!("{n} warnings")),
    }
    match warned.duplicates {
        0 => {}
        1 => said.push_str(" (1 duplicate)"),
        n => said.push_str(&format!(" ({n} duplicates)")),
    }
    let custom_build = target.kind.first().is_some_and(|it| it == "custom-build");
    if let Some(fixable @ 1..) = warned.fixable
        && local
        && !custom_build
    {
        let named = target_named(target);
        let mut args = match target.kind.first().map(String::as_str) {
            Some("bin" | "test" | "bench" | "example") => named,
            _ => format!("{named} -p {package}"),
        };
        if test && !target.kind.first().is_some_and(|it| it == "test" || it == "bench") {
            args.push_str(" --tests");
        }
        let suggestions = match fixable {
            1 => "1 suggestion".to_owned(),
            n => format!("{n} suggestions"),
        };
        said.push_str(&format!(" (run `cargo fix --{args}` to apply {suggestions})"));
    }
    Some(said)
}

/// `` could not compile `foo` (lib) due to 1 previous error; 1 warning emitted ``: what follows
/// `error:` for a unit that failed (`core/compiler/mod.rs`, `rustc`).
pub fn could_not_compile(package: &str, target: &UnitTarget, test: bool, errors: u32, warnings: u32) -> String {
    let errors = match errors {
        0 => String::new(),
        1 => " due to 1 previous error".to_owned(),
        n => format!(" due to {n} previous errors"),
    };
    let warnings = match warnings {
        0 => String::new(),
        1 => "; 1 warning emitted".to_owned(),
        n => format!("; {n} warnings emitted"),
    };
    format!("could not compile {}{errors}{warnings}", unit_named(package, target, test))
}

/// `arg` as cargo-util's `ProcessBuilder` shows it, quoted as `shell-escape` quotes it.
pub fn escape(arg: &str) -> String {
    #[cfg(windows)]
    {
        escape_windows(arg)
    }
    #[cfg(not(windows))]
    {
        escape_unix(arg)
    }
}

/// `shell_escape::unix::escape`.
pub fn escape_unix(arg: &str) -> String {
    let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '=' | '/' | ',' | '.' | '+');
    if !arg.is_empty() && arg.chars().all(plain) {
        return arg.to_owned();
    }
    let mut quoted = String::with_capacity(arg.len() + 2);
    quoted.push('\'');
    for c in arg.chars() {
        match c {
            '\'' | '!' => {
                quoted.push_str("'\\");
                quoted.push(c);
                quoted.push('\'');
            }
            _ => quoted.push(c),
        }
    }
    quoted.push('\'');
    quoted
}

/// `shell_escape::windows::escape`.
pub fn escape_windows(arg: &str) -> String {
    let needs = arg.is_empty() || arg.chars().any(|c| matches!(c, '"' | '\t' | '\n' | ' '));
    if !needs {
        return arg.to_owned();
    }
    let mut quoted = String::from("\"");
    let mut chars = arg.chars().peekable();
    loop {
        let mut slashes = 0;
        while chars.next_if_eq(&'\\').is_some() {
            slashes += 1;
        }
        match chars.next() {
            Some('"') => {
                quoted.extend(std::iter::repeat_n('\\', slashes * 2 + 1));
                quoted.push('"');
            }
            Some(c) => {
                quoted.extend(std::iter::repeat_n('\\', slashes));
                quoted.push(c);
            }
            None => {
                quoted.extend(std::iter::repeat_n('\\', slashes * 2));
                break;
            }
        }
    }
    quoted.push('"');
    quoted
}

/// `` `target/debug/foo a 'b c'` ``: a command as cargo's status lines show it, without the
/// variables cargo set for it (whose values are never printed here).
pub fn command_line(program: &str, args: &[String]) -> String {
    let mut line = format!("`{program}");
    for arg in args {
        line.push(' ');
        line.push_str(&escape(arg));
    }
    line.push('`');
    line
}

/// A path as cargo shows one it runs: from `cwd` when it is below it.
pub fn shown(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd).unwrap_or(path).display().to_string()
}

/// How a program cargo ran ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ended {
    Code(i32),
    /// Killed by the signal (Unix).
    Signal(i32),
}

impl Ended {
    pub fn of(status: std::process::ExitStatus) -> Ended {
        #[cfg(unix)]
        if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(&status) {
            return Ended::Signal(signal);
        }
        Ended::Code(status.code().unwrap_or(1))
    }

    pub fn success(self) -> bool {
        self == Ended::Code(0)
    }
}

/// `exit status: 1`, `signal: 11, SIGSEGV: invalid memory reference`: cargo-util's
/// `status_to_string`.
pub fn ended(ended: Ended) -> String {
    match ended {
        Ended::Signal(signal) => format!("signal: {signal}{}", signal_named(signal)),
        Ended::Code(code) => code_named(code),
    }
}

#[cfg(unix)]
fn signal_named(signal: i32) -> &'static str {
    match signal {
        libc::SIGABRT => ", SIGABRT: process abort signal",
        libc::SIGALRM => ", SIGALRM: alarm clock",
        libc::SIGFPE => ", SIGFPE: erroneous arithmetic operation",
        libc::SIGHUP => ", SIGHUP: hangup",
        libc::SIGILL => ", SIGILL: illegal instruction",
        libc::SIGINT => ", SIGINT: terminal interrupt signal",
        libc::SIGKILL => ", SIGKILL: kill",
        libc::SIGPIPE => ", SIGPIPE: write on a pipe with no one to read",
        libc::SIGQUIT => ", SIGQUIT: terminal quit signal",
        libc::SIGSEGV => ", SIGSEGV: invalid memory reference",
        libc::SIGTERM => ", SIGTERM: termination signal",
        libc::SIGBUS => ", SIGBUS: access to undefined memory",
        libc::SIGSYS => ", SIGSYS: bad system call",
        libc::SIGTRAP => ", SIGTRAP: trace/breakpoint trap",
        _ => "",
    }
}

#[cfg(not(unix))]
fn signal_named(_: i32) -> &'static str {
    ""
}

#[cfg(not(windows))]
fn code_named(code: i32) -> String {
    format!("exit status: {code}")
}

/// std's `exit code: 0xc0000005` for an NTSTATUS, with cargo's name for it.
#[cfg(windows)]
fn code_named(code: i32) -> String {
    let status = code as u32;
    if status & 0x8000_0000 == 0 {
        return format!("exit code: {code}");
    }
    let name = match status {
        0xC000_0005 => "STATUS_ACCESS_VIOLATION",
        0xC000_0006 => "STATUS_IN_PAGE_ERROR",
        0xC000_0008 => "STATUS_INVALID_HANDLE",
        0xC000_000D => "STATUS_INVALID_PARAMETER",
        0xC000_0017 => "STATUS_NO_MEMORY",
        0xC000_001D => "STATUS_ILLEGAL_INSTRUCTION",
        0xC000_0025 => "STATUS_NONCONTINUABLE_EXCEPTION",
        0xC000_0026 => "STATUS_INVALID_DISPOSITION",
        0xC000_008C => "STATUS_ARRAY_BOUNDS_EXCEEDED",
        0xC000_008D => "STATUS_FLOAT_DENORMAL_OPERAND",
        0xC000_008E => "STATUS_FLOAT_DIVIDE_BY_ZERO",
        0xC000_008F => "STATUS_FLOAT_INEXACT_RESULT",
        0xC000_0090 => "STATUS_FLOAT_INVALID_OPERATION",
        0xC000_0091 => "STATUS_FLOAT_OVERFLOW",
        0xC000_0092 => "STATUS_FLOAT_STACK_CHECK",
        0xC000_0093 => "STATUS_FLOAT_UNDERFLOW",
        0xC000_0094 => "STATUS_INTEGER_DIVIDE_BY_ZERO",
        0xC000_0095 => "STATUS_INTEGER_OVERFLOW",
        0xC000_0096 => "STATUS_PRIVILEGED_INSTRUCTION",
        0xC000_00FD => "STATUS_STACK_OVERFLOW",
        0xC000_0135 => "STATUS_DLL_NOT_FOUND",
        0xC000_0138 => "STATUS_ORDINAL_NOT_FOUND",
        0xC000_0139 => "STATUS_ENTRYPOINT_NOT_FOUND",
        0xC000_013A => "STATUS_CONTROL_C_EXIT",
        0xC000_0142 => "STATUS_DLL_INIT_FAILED",
        0xC000_02B4 => "STATUS_FLOAT_MULTIPLE_FAULTS",
        0xC000_02B5 => "STATUS_FLOAT_MULTIPLE_TRAPS",
        0xC000_02C9 => "STATUS_REG_NAT_CONSUMPTION",
        0xC000_0374 => "STATUS_HEAP_CORRUPTION",
        0xC000_0409 => "STATUS_STACK_BUFFER_OVERRUN",
        0xC000_0420 => "STATUS_ASSERTION_FAILURE",
        _ => return format!("exit code: {status:#x}"),
    };
    format!("exit code: {status:#x}, {name}")
}

/// `text` without the escapes that color a terminal (`anstream::adapter::strip_str`): CSI
/// sequences to their final byte, OSC sequences to BEL or ST.
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for c in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\x07' || (c == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod tests;

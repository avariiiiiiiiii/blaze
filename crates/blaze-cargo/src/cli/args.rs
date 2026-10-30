//! cargo's command lines, read as cargo's clap reads them (`cargo/src/bin/cargo/commands/`,
//! `util/command_prelude.rs`): which flags each command takes, which take a value and how, what
//! is the program's. A command line clap would refuse is refused with clap's text, which exits
//! 1. Everything else goes to the daemon as typed (`Cargo::args`), whose load is cargo's.

use std::ffi::OsString;

use super::say;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Command {
    Build,
    Check,
    Run,
    Test,
    Bench,
}

impl Command {
    /// The command a word names, with cargo's aliases.
    pub fn named(word: &str) -> Option<Command> {
        match word {
            "build" | "b" => Some(Command::Build),
            "check" | "c" => Some(Command::Check),
            "run" | "r" => Some(Command::Run),
            "test" | "t" => Some(Command::Test),
            "bench" => Some(Command::Bench),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Command::Build => "build",
            Command::Check => "check",
            Command::Run => "run",
            Command::Test => "test",
            Command::Bench => "bench",
        }
    }

    /// The profile cargo builds with when none is named.
    pub fn profile(self) -> &'static str {
        match self {
            Command::Build | Command::Check | Command::Run => "dev",
            Command::Test => "test",
            Command::Bench => "bench",
        }
    }

    /// clap's usage line, after `Usage: `: the command, then its placeholders.
    fn usage(self) -> (&'static str, &'static [&'static str]) {
        match self {
            Command::Build => ("cargo build", &["[OPTIONS]"]),
            Command::Check => ("cargo check", &["[OPTIONS]"]),
            Command::Run => ("cargo run", &["[OPTIONS]", "[ARGS]..."]),
            Command::Test => ("cargo test", &["[OPTIONS]", "[TESTNAME]", "[-- [ARGS]...]"]),
            Command::Bench => ("cargo bench", &["[OPTIONS]", "[BENCHNAME]", "[-- [ARGS]...]"]),
        }
    }

    fn has_positionals(self) -> bool {
        matches!(self, Command::Run | Command::Test | Command::Bench)
    }

    fn flags(self) -> &'static [&'static [Flag]] {
        match self {
            Command::Build => &[
                GLOBAL, MESSAGE, PACKAGES, JOBS, KEEP_GOING, FUTURE, TARGETS, FEATURES, RELEASE,
                PROFILE, TARGET, BUILD_ONLY, REPORTS, MANIFEST,
            ],
            Command::Check => &[
                GLOBAL, MESSAGE, PACKAGES, JOBS, KEEP_GOING, FUTURE, TARGETS, FEATURES, RELEASE,
                PROFILE, TARGET, REPORTS, MANIFEST,
            ],
            Command::Run => &[
                GLOBAL, MESSAGE, PACKAGE, JOBS, KEEP_GOING, RUN_TARGETS, FEATURES, RELEASE, PROFILE,
                TARGET, REPORTS, MANIFEST,
            ],
            Command::Test => &[
                GLOBAL, MESSAGE, TEST_ONLY, RUNNING, PACKAGES, JOBS, FUTURE, TARGETS, FEATURES,
                RELEASE, PROFILE, TARGET, REPORTS, MANIFEST,
            ],
            Command::Bench => &[
                GLOBAL, MESSAGE, RUNNING, PACKAGES, JOBS, FUTURE, TARGETS, FEATURES, PROFILE,
                TARGET, REPORTS, MANIFEST,
            ],
        }
    }

    fn flag(self, found: impl Fn(&Flag) -> bool) -> Option<&'static Flag> {
        self.flags().iter().flat_map(|it| it.iter()).find(|it| found(it))
    }
}

/// How a flag takes its value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Takes {
    Switch,
    /// `-v`, `-vv`.
    Count,
    /// The rest of its word, or the next word.
    Value,
    /// The rest of its word, or the next word unless that is a flag (`num_args(0..=1)`).
    Optional,
    /// `--timings=html` only (`require_equals`).
    Equals,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Flag {
    long: Option<&'static str>,
    short: Option<char>,
    takes: Takes,
    value: &'static str,
    possible: &'static [&'static str],
}

impl Flag {
    const fn new(long: &'static str, short: Option<char>, takes: Takes, value: &'static str) -> Flag {
        Flag { long: Some(long), short, takes, value, possible: &[] }
    }

    const fn switch(long: &'static str) -> Flag {
        Flag::new(long, None, Takes::Switch, "")
    }

    /// As clap shows it in an error: `--features <FEATURES>`, `--bin [<NAME>]`, `-Z <FLAG>`.
    fn shown(&self) -> String {
        let name = match (self.long, self.short) {
            (Some(long), _) => format!("--{long}"),
            (None, Some(short)) => format!("-{short}"),
            (None, None) => String::new(),
        };
        match self.takes {
            Takes::Switch | Takes::Count => name,
            Takes::Value => format!("{name} <{}>", self.value),
            Takes::Optional => format!("{name} [<{}>]", self.value),
            Takes::Equals => format!("{name}[=<{}>]", self.value),
        }
    }
}

const COLORS: &[&str] = &["auto", "always", "never"];
const FORMATS: &[&str] = &[
    "human",
    "short",
    "json",
    "json-diagnostic-short",
    "json-diagnostic-rendered-ansi",
    "json-render-diagnostics",
];

const GLOBAL: &[Flag] = &[
    Flag::new("verbose", Some('v'), Takes::Count, ""),
    Flag::new("quiet", Some('q'), Takes::Switch, ""),
    Flag { long: Some("color"), short: None, takes: Takes::Value, value: "WHEN", possible: COLORS },
    Flag::new("config", None, Takes::Value, "KEY=VALUE|PATH"),
    Flag { long: None, short: Some('Z'), takes: Takes::Value, value: "FLAG", possible: &[] },
    Flag::switch("frozen"),
    Flag::switch("locked"),
    Flag::switch("offline"),
    Flag::new("help", Some('h'), Takes::Switch, ""),
];
const MESSAGE: &[Flag] = &[Flag {
    long: Some("message-format"),
    short: None,
    takes: Takes::Value,
    value: "FMT",
    possible: FORMATS,
}];
const PACKAGES: &[Flag] = &[
    Flag::new("package", Some('p'), Takes::Optional, "SPEC"),
    Flag::switch("workspace"),
    Flag::switch("all"),
    Flag::new("exclude", None, Takes::Value, "SPEC"),
];
const PACKAGE: &[Flag] = &[Flag::new("package", Some('p'), Takes::Optional, "SPEC")];
const JOBS: &[Flag] = &[Flag::new("jobs", Some('j'), Takes::Value, "N")];
const KEEP_GOING: &[Flag] = &[Flag::switch("keep-going")];
const FUTURE: &[Flag] = &[Flag::switch("future-incompat-report")];
const TARGETS: &[Flag] = &[
    Flag::switch("lib"),
    Flag::switch("bins"),
    Flag::new("bin", None, Takes::Optional, "NAME"),
    Flag::switch("examples"),
    Flag::new("example", None, Takes::Optional, "NAME"),
    Flag::switch("tests"),
    Flag::new("test", None, Takes::Optional, "NAME"),
    Flag::switch("benches"),
    Flag::new("bench", None, Takes::Optional, "NAME"),
    Flag::switch("all-targets"),
];
const RUN_TARGETS: &[Flag] = &[
    Flag::new("bin", None, Takes::Optional, "NAME"),
    Flag::new("example", None, Takes::Optional, "NAME"),
];
const FEATURES: &[Flag] = &[
    Flag::new("features", Some('F'), Takes::Value, "FEATURES"),
    Flag::switch("all-features"),
    Flag::switch("no-default-features"),
];
const RELEASE: &[Flag] = &[Flag::new("release", Some('r'), Takes::Switch, "")];
const PROFILE: &[Flag] = &[Flag::new("profile", None, Takes::Value, "PROFILE-NAME")];
const TARGET: &[Flag] = &[
    Flag::new("target", None, Takes::Optional, "TRIPLE"),
    Flag::new("target-dir", None, Takes::Value, "DIRECTORY"),
];
const BUILD_ONLY: &[Flag] = &[
    Flag::new("artifact-dir", None, Takes::Value, "PATH"),
    Flag::new("out-dir", None, Takes::Value, "PATH"),
    Flag::switch("build-plan"),
];
const REPORTS: &[Flag] = &[
    Flag::switch("unit-graph"),
    Flag::new("timings", None, Takes::Equals, "FMTS"),
    Flag::switch("ignore-rust-version"),
];
const MANIFEST: &[Flag] = &[
    Flag::new("manifest-path", None, Takes::Value, "PATH"),
    Flag::new("lockfile-path", None, Takes::Value, "PATH"),
];
/// `--reached` and `--ub*` are blaze's (extensions, opt-in): only the tests an edit reached run;
/// the tests run in blaze's MIR evaluator, checked for undefined behavior (`hir-ty-ub`).
const TEST_ONLY: &[Flag] = &[
    Flag::switch("doc"),
    Flag::switch("reached"),
    Flag::switch("ub"),
    Flag::new("ub-seed", None, Takes::Value, "N"),
    Flag::new("ub-schedules", None, Takes::Value, "K"),
];

/// blaze's own flags: read here, never on cargo's command line.
fn blaze_only(flag: &Flag) -> bool {
    matches!(flag.long, Some("reached" | "ub" | "ub-seed" | "ub-schedules"))
}
const RUNNING: &[Flag] = &[Flag::switch("no-run"), Flag::switch("no-fail-fast")];

/// What blaze was asked.
#[derive(Debug, PartialEq)]
pub enum Asked {
    Cargo(Box<Invocation>),
    /// `blaze stop`: the daemon made durable and ended.
    Stop,
    /// `blaze status`: what the daemon is doing.
    Status,
    Help(Option<Command>),
    Version,
    /// A command that is none of blaze's.
    Unknown(String),
}

/// One of cargo's build commands, read.
#[derive(Debug, PartialEq, Default)]
pub struct Invocation {
    pub command: Option<Command>,
    /// The words before `--`, as typed, flags typed before the command first: cargo's to read.
    /// None of `run`'s program arguments, nor blaze's own `--reached` and `--ub*`.
    pub args: Vec<String>,
    /// `test`'s and `bench`'s `TESTNAME`.
    pub filter: Option<String>,
    /// `run`'s arguments of the program; what follows `--` for `test` and `bench`.
    pub program: Vec<OsString>,
    pub flags: Flags,
    /// `-h`/`--help` came before anything clap refuses: the command's help, and nothing else.
    pub help: bool,
}

impl Invocation {
    pub fn command(&self) -> Command {
        self.command.unwrap_or(Command::Build)
    }

    /// What each test executable is run with: the filter, then what follows `--`, then for
    /// `bench` libtest's `--bench`, as cargo's `ops::run_tests` and `run_benches` pass them.
    pub fn test_args(&self) -> Vec<OsString> {
        let mut args: Vec<OsString> = self.filter.iter().map(OsString::from).collect();
        args.extend(self.program.iter().cloned());
        if self.command == Some(Command::Bench) {
            args.push("--bench".into());
        }
        args
    }

    /// The profile cargo's `Finished` line and the daemon's `Targets` name.
    pub fn profile(&self) -> String {
        match (&self.flags.profile, self.flags.release) {
            (Some(profile), _) => profile.clone(),
            (None, true) => "release".to_owned(),
            (None, false) => self.command().profile().to_owned(),
        }
    }
}

/// The flags the client reads itself, or puts in `Targets`.
#[derive(Debug, PartialEq, Default)]
pub struct Flags {
    pub manifest_path: Option<String>,
    /// `-p` with a value; `-p` alone is cargo's to answer, with the packages it knows.
    pub packages: Vec<String>,
    pub workspace: bool,
    pub excludes: Vec<String>,
    pub features: Vec<String>,
    pub all_features: bool,
    pub no_default_features: bool,
    pub release: bool,
    pub profile: Option<String>,
    pub targets: Vec<String>,
    /// Each `--bin`, with its name when it has one.
    pub bins: Vec<Option<String>>,
    pub examples: Vec<Option<String>>,
    pub message_format: Vec<String>,
    pub color: Option<String>,
    pub config: Vec<String>,
    pub verbose: u32,
    pub quiet: bool,
    pub doc: bool,
    pub no_run: bool,
    pub no_fail_fast: bool,
    pub reached: bool,
    /// `--ub`, or either of its values, which imply it.
    pub ub: bool,
    pub ub_seed: Option<u64>,
    pub ub_schedules: Option<u32>,
}

impl Flags {
    fn read(&mut self, flag: &Flag, value: Option<&str>) {
        let owned = || value.map(str::to_owned);
        match flag.long.unwrap_or("") {
            "manifest-path" => self.manifest_path = owned(),
            "package" => self.packages.extend(owned()),
            "workspace" | "all" => self.workspace = true,
            "exclude" => self.excludes.extend(owned()),
            "features" => {
                let features = value.unwrap_or_default().split([',', ' ']);
                self.features.extend(features.filter(|it| !it.is_empty()).map(str::to_owned));
            }
            "all-features" => self.all_features = true,
            "no-default-features" => self.no_default_features = true,
            "release" => self.release = true,
            "profile" => self.profile = owned(),
            "target" => self.targets.extend(owned()),
            "bin" => self.bins.push(owned()),
            "example" => self.examples.push(owned()),
            "message-format" => self.message_format.extend(owned()),
            "color" => self.color = owned(),
            "config" => self.config.extend(owned()),
            "verbose" => self.verbose += 1,
            "quiet" => self.quiet = true,
            "doc" => self.doc = true,
            "no-run" => self.no_run = true,
            "no-fail-fast" => self.no_fail_fast = true,
            "reached" => self.reached = true,
            "ub" => self.ub = true,
            // `check_value` parsed them first.
            "ub-seed" => {
                self.ub = true;
                self.ub_seed = value.and_then(|it| it.parse().ok());
            }
            "ub-schedules" => {
                self.ub = true;
                self.ub_schedules = value.and_then(|it| it.parse().ok());
            }
            _ => {}
        }
    }
}

/// A command line clap refuses, with what it says.
#[derive(Debug, PartialEq, Eq)]
pub struct Usage {
    pub command: Command,
    pub said: Said,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Said {
    Unknown { arg: String, similar: Option<String> },
    NoValue { flag: &'static Flag },
    BadValue { value: String, flag: &'static Flag },
    ExtraValue { value: String, flag: &'static Flag },
    /// A number clap's `value_parser` refuses, with why (`invalid digit found in string`).
    NotNumber { value: String, flag: &'static Flag, why: String },
    NotUtf8,
}

/// The arguments after `blaze` (or after the `blaze` cargo passes `cargo-blaze`), read.
pub fn parse(words: Vec<OsString>) -> Result<Asked, Usage> {
    let mut globals = Vec::new();
    let mut words = words.into_iter();
    while let Some(word) = words.next() {
        let text = word.to_str().map(str::to_owned);
        match text.as_deref() {
            Some("--help" | "-h" | "help") => {
                let command = words.next().and_then(|it| it.to_str().and_then(Command::named));
                return Ok(Asked::Help(command));
            }
            Some("--version" | "-V") => return Ok(Asked::Version),
            Some("stop") => return Ok(Asked::Stop),
            Some("status") => return Ok(Asked::Status),
            Some(other) if other.starts_with('-') && other.len() > 1 => {
                // A global flag before the command, and its value when that is the next word.
                let attached = match other.strip_prefix("--") {
                    Some(long) => long.contains('='),
                    None => other.len() > 2,
                };
                let takes_next = !attached && global(other).is_some_and(|it| it.takes == Takes::Value);
                globals.push(word);
                if takes_next && let Some(value) = words.next() {
                    globals.push(value);
                }
            }
            Some(name) => match Command::named(name) {
                Some(command) => {
                    globals.extend(words);
                    return command_line(command, globals).map(|it| match it.help {
                        true => Asked::Help(Some(command)),
                        false => Asked::Cargo(Box::new(it)),
                    });
                }
                None => return Ok(Asked::Unknown(name.to_owned())),
            },
            None => return Ok(Asked::Unknown(word.to_string_lossy().into_owned())),
        }
    }
    Ok(Asked::Help(None))
}

fn global(word: &str) -> Option<&'static Flag> {
    let long = word.strip_prefix("--").map(|it| it.split('=').next().unwrap_or(it));
    let short = word.strip_prefix('-').and_then(|it| it.chars().next());
    GLOBAL.iter().find(|flag| match long {
        Some(long) => flag.long == Some(long),
        None => flag.short == short,
    })
}

/// The words after a build command, as clap reads them for it.
pub fn command_line(command: Command, words: Vec<OsString>) -> Result<Invocation, Usage> {
    let refused = |said| Usage { command, said };
    let mut asked = Invocation { command: Some(command), ..Invocation::default() };
    let mut words = words.into_iter().peekable();
    let mut ended = false;
    while let Some(word) = words.next() {
        if word == "--" {
            ended = true;
            break;
        }
        let Some(text) = word.to_str() else {
            match command {
                Command::Run => {
                    asked.program.push(word);
                    run_program(&mut asked, &mut words);
                    break;
                }
                _ => return Err(refused(Said::NotUtf8)),
            }
        };
        let text = text.to_owned();
        if !text.starts_with('-') || text == "-" {
            match command {
                Command::Run => {
                    asked.program.push(word);
                    run_program(&mut asked, &mut words);
                    break;
                }
                Command::Test | Command::Bench if asked.filter.is_none() => {
                    asked.args.push(text.clone());
                    asked.filter = Some(text);
                    continue;
                }
                _ => return Err(refused(Said::Unknown { arg: text, similar: None })),
            }
        }
        let mut typed = vec![text.clone()];
        if let Some(long) = text.strip_prefix("--") {
            let (name, attached) = match long.split_once('=') {
                Some((name, value)) => (name, Some(value.to_owned())),
                None => (long, None),
            };
            let Some(flag) = command.flag(|it| it.long == Some(name)) else {
                return Err(refused(Said::Unknown { arg: format!("--{name}"), similar: similar(command, name) }));
            };
            let value = match (flag.takes, attached) {
                (Takes::Switch | Takes::Count, Some(value)) => {
                    return Err(refused(Said::ExtraValue { value, flag }));
                }
                (Takes::Switch | Takes::Count, None) => None,
                (Takes::Value | Takes::Optional | Takes::Equals, Some(value)) => Some(value),
                (Takes::Value, None) => match next_value(&mut words) {
                    Some(value) => {
                        typed.push(value.clone());
                        Some(value)
                    }
                    None => return Err(refused(Said::NoValue { flag })),
                },
                (Takes::Optional, None) => {
                    let value = next_value(&mut words);
                    typed.extend(value.clone());
                    value
                }
                (Takes::Equals, None) => None,
            };
            if flag.long == Some("help") {
                return Ok(Invocation { command: Some(command), help: true, ..Invocation::default() });
            }
            check_value(flag, value.as_deref()).map_err(refused)?;
            asked.flags.read(flag, value.as_deref());
            if !blaze_only(flag) {
                asked.args.extend(typed);
            }
            continue;
        }
        // A cluster of short flags: `-rq`, `-vv`, `-j8`, `-pfoo`, `-p=foo`.
        let shorts: Vec<char> = text[1..].chars().collect();
        let mut at = 0;
        while at < shorts.len() {
            let c = shorts[at];
            let Some(flag) = command.flag(|it| it.short == Some(c)) else {
                return Err(refused(Said::Unknown { arg: format!("-{c}"), similar: None }));
            };
            at += 1;
            if flag.long == Some("help") {
                return Ok(Invocation { command: Some(command), help: true, ..Invocation::default() });
            }
            let rest: String = shorts[at..].iter().collect();
            let rest = rest.strip_prefix('=').unwrap_or(&rest).to_owned();
            let value = match flag.takes {
                Takes::Switch | Takes::Count => None,
                Takes::Value | Takes::Optional | Takes::Equals if !rest.is_empty() => {
                    at = shorts.len();
                    Some(rest)
                }
                Takes::Value => match next_value(&mut words) {
                    Some(value) => {
                        typed.push(value.clone());
                        Some(value)
                    }
                    None => return Err(refused(Said::NoValue { flag })),
                },
                Takes::Optional => {
                    let value = next_value(&mut words);
                    typed.extend(value.clone());
                    value
                }
                Takes::Equals => None,
            };
            check_value(flag, value.as_deref()).map_err(refused)?;
            asked.flags.read(flag, value.as_deref());
        }
        asked.args.extend(typed);
    }
    if ended {
        let rest: Vec<OsString> = words.collect();
        match command {
            Command::Build | Command::Check => {
                if let Some(first) = rest.first() {
                    let arg = first.to_string_lossy().into_owned();
                    return Err(refused(Said::Unknown { arg, similar: None }));
                }
            }
            _ => asked.program.extend(rest),
        }
    }
    Ok(asked)
}

/// The next word as a flag's value, unless it is a flag itself.
fn next_value(words: &mut std::iter::Peekable<std::vec::IntoIter<OsString>>) -> Option<String> {
    let next = words.peek()?.to_str()?;
    if next.starts_with('-') && next != "-" {
        return None;
    }
    words.next()?.into_string().ok()
}

/// `run`'s program has the rest: once its first argument comes, clap reads no more flags
/// (`trailing_var_arg`), and the first `--` among them is dropped.
fn run_program(asked: &mut Invocation, words: &mut impl Iterator<Item = OsString>) {
    let mut dropped = false;
    for word in words {
        if word == "--" && !dropped {
            dropped = true;
            continue;
        }
        asked.program.push(word);
    }
}

fn check_value(flag: &'static Flag, value: Option<&str>) -> Result<(), Said> {
    let Some(value) = value else { return Ok(()) };
    let not_number = |why: String| Said::NotNumber { value: value.to_owned(), flag, why };
    match flag.long {
        Some("ub-seed") => return value.parse::<u64>().map(drop).map_err(|it| not_number(it.to_string())),
        // `value_parser!(u32).range(1..)`.
        Some("ub-schedules") => {
            return match value.parse::<u32>() {
                Ok(0) => Err(not_number(format!("0 is not in 1..={}", u32::MAX))),
                Ok(_) => Ok(()),
                Err(error) => Err(not_number(error.to_string())),
            };
        }
        _ => {}
    }
    if flag.possible.is_empty() {
        return Ok(());
    }
    let parts: Vec<&str> = match flag.long {
        // `value_delimiter(',')` and `ignore_case(true)`.
        Some("message-format") => value.split(',').collect(),
        _ => vec![value],
    };
    for part in parts {
        let known = match flag.long {
            Some("message-format") => flag.possible.iter().any(|it| it.eq_ignore_ascii_case(part)),
            _ => flag.possible.contains(&part),
        };
        if !known {
            return Err(Said::BadValue { value: part.to_owned(), flag });
        }
    }
    Ok(())
}

/// clap's did-you-mean of a long flag: the command's flag most like `typed`, of a Jaro
/// similarity over 0.7 (`clap_builder::parser::features::suggestions`).
fn similar(command: Command, typed: &str) -> Option<String> {
    let longs = command.flags().iter().flat_map(|it| it.iter()).filter_map(|it| it.long);
    best(typed, longs).map(|it| format!("--{it}"))
}

fn best<'a>(typed: &str, among: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let mut best: Option<(f64, &str)> = None;
    for candidate in among {
        let confidence = jaro(typed, candidate);
        if confidence > 0.7 && best.is_none_or(|(it, _)| confidence > it) {
            best = Some((confidence, candidate));
        }
    }
    best.map(|(_, it)| it)
}

/// strsim's `jaro`.
pub fn jaro(a: &str, b: &str) -> f64 {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    if a.len() == 1 && b.len() == 1 {
        return if a[0] == b[0] { 1.0 } else { 0.0 };
    }
    let window = (a.len().max(b.len()) / 2).saturating_sub(1);
    let mut b_used = vec![false; b.len()];
    let mut matches = 0usize;
    let mut transpositions = 0usize;
    let mut b_at = 0usize;
    for (i, &ca) in a.iter().enumerate() {
        let start = i.saturating_sub(window);
        let end = (i + window + 1).min(b.len());
        for j in start..end {
            if !b_used[j] && b[j] == ca {
                b_used[j] = true;
                matches += 1;
                if j < b_at {
                    transpositions += 1;
                }
                b_at = j;
                break;
            }
        }
    }
    if matches == 0 {
        return 0.0;
    }
    let m = matches as f64;
    (m / a.len() as f64 + m / b.len() as f64 + (m - transpositions as f64) / m) / 3.0
}

/// clap's text for a refused command line, in cargo's styles when `color`; clap's exit code
/// for it in cargo is 1.
pub fn refusal(usage: &Usage, color: bool) -> String {
    let paint = |style: &str, text: &str| say::paint(style, text, color);
    let error = paint(say::ERROR, "error:");
    let more = format!("For more information, try '{}'.\n", paint(say::VALID, "--help"));
    let usage_line = || {
        let (literal, placeholders) = usage.command.usage();
        let mut line = format!("{} {}", paint(say::GOOD, "Usage:"), paint(say::VALID, literal));
        for placeholder in placeholders {
            line.push(' ');
            line.push_str(&paint(say::PLACEHOLDER, placeholder));
        }
        line
    };
    let possible = |flag: &Flag| {
        let values: Vec<String> = flag.possible.iter().map(|it| paint(say::VALID, it)).collect();
        format!("  [possible values: {}]\n", values.join(", "))
    };
    match &usage.said {
        Said::Unknown { arg, similar } => {
            let mut said = format!("{error} unexpected argument '{}' found\n", paint(say::WARN, arg));
            match similar {
                Some(similar) => {
                    let tip = paint(say::VALID, "tip:");
                    said += &format!("\n  {tip} a similar argument exists: '{}'\n", paint(say::VALID, similar));
                }
                None if usage.command.has_positionals() => {
                    let tip = paint(say::VALID, "tip:");
                    let trailing = paint(say::VALID, &format!("-- {arg}"));
                    said += &format!("\n  {tip} to pass '{}' as a value, use '{trailing}'\n", paint(say::VALID, arg));
                }
                None => {}
            }
            format!("{said}\n{}\n\n{more}", usage_line())
        }
        Said::NoValue { flag } => {
            let mut said = format!(
                "{error} a value is required for '{}' but none was supplied\n",
                paint(say::WARN, &flag.shown())
            );
            if !flag.possible.is_empty() {
                said += &possible(flag);
            }
            format!("{said}\n{more}")
        }
        Said::BadValue { value, flag } => {
            let mut said = format!(
                "{error} invalid value '{}' for '{}'\n",
                paint(say::WARN, value),
                paint(say::VALID, &flag.shown())
            );
            said += &possible(flag);
            if let Some(similar) = best(value, flag.possible.iter().copied()) {
                let tip = paint(say::VALID, "tip:");
                said += &format!("\n  {tip} a similar value exists: '{}'\n", paint(say::VALID, similar));
            }
            format!("{said}\n{more}")
        }
        Said::ExtraValue { value, flag } => format!(
            "{error} unexpected value '{}' for '{}' found; no more were expected\n\n{}\n\n{more}",
            paint(say::WARN, value),
            paint(say::VALID, &flag.shown()),
            usage_line()
        ),
        Said::NotNumber { value, flag, why } => format!(
            "{error} invalid value '{}' for '{}': {why}\n\n{more}",
            paint(say::WARN, value),
            paint(say::VALID, &flag.shown())
        ),
        Said::NotUtf8 => format!(
            "{error} invalid UTF-8 was detected in one or more arguments\n\n{}\n\n{more}",
            usage_line()
        ),
    }
}

/// `blaze <command> --help`: the command's flags as this client reads them.
pub fn help(command: Command) -> String {
    let (literal, placeholders) = command.usage();
    let mut said = format!(
        "{}: cargo's `{}`, built by blaze's daemon\n\nUsage: {literal} {}\n\nOptions:\n",
        command.name(),
        command.name(),
        placeholders.join(" ")
    );
    for flag in command.flags().iter().flat_map(|it| it.iter()) {
        let line = match (flag.short, flag.long) {
            (Some(short), Some(_)) => format!("  -{short}, {}\n", flag.shown()),
            _ if flag.long.is_some() => format!("      {}\n", flag.shown()),
            _ => format!("  {}\n", flag.shown()),
        };
        said.push_str(&line);
    }
    said
}

#[cfg(test)]
mod tests;

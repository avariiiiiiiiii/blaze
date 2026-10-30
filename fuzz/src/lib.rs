//! blaze's coverage-guided fuzzer, and the targets for every parser of untrusted bytes
//! (`targets/`, one binary each; fuzz/README.md lists them).
//!
//! The engine reads what SanitizerCoverage counts (`raw`): an inline 8-bit counter per block,
//! the PC table that says which block is a function's entry, and the operands of comparisons.
//! An input is kept when it reaches a new feature (a counter in a new AFL bucket of hit
//! counts, or a new value-profile bit), and the kept inputs are changed by libFuzzer's
//! mutations (`mutate`). A failing input is written out and can be minimized (`minimize`).
//! Fuzzing needs a build with SanitizerCoverage and AddressSanitizer; without them `fuzz`
//! stops naming the flags. `run`, `minimize` and `seeds` work in any build, and each target's
//! `#[test]` runs its seeds in a plain one.

#![feature(core_intrinsics, sanitize)]
#![allow(internal_features)]
#![deny(unsafe_code)]
// A command-line tool: it reports on stderr.
#![allow(clippy::print_stderr)]

mod corpus;
mod features;
mod minimize;
mod mutate;
#[allow(unsafe_code)]
mod raw;
#[cfg(test)]
mod tests;

use std::{
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{
        OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crate::{
    corpus::Corpus,
    features::Seen,
    mutate::{Mutator, Traced},
    raw::Coverage,
};

/// What a fuzz build is given, in the names of rustc's flags (blaze takes them in `RUSTFLAGS`).
pub(crate) const FLAGS: &str = "build with `-Zsanitizer=address -Csanitizer-coverage-level=3 \
     -Zsanitizer-coverage=inline-8bit-counters,pc-table,trace-cmp` in RUSTFLAGS (fuzz/README.md)";

/// A fuzz target: one parser of untrusted bytes and what must hold of it on any input.
pub struct Target {
    /// The binary's name and its corpus directory, `fuzz/corpus/<name>/`.
    pub name: &'static str,
    /// Runs one input; panics when an invariant fails. Must return on any bytes, join every
    /// thread it starts, and keep no state between calls that changes what it checks.
    pub check: fn(&[u8]),
    /// Valid inputs built through the writer side of the API.
    pub seeds: fn() -> Vec<Vec<u8>>,
    /// Tokens the mutator inserts (magic numbers, tags, field names), beside what the
    /// comparisons it sees teach it.
    pub dictionary: &'static [&'static [u8]],
    /// The longest input the mutator makes (the seeds may be longer).
    pub max_len: usize,
    /// How long CI fuzzes this target after merge.
    pub budget: Duration,
}

impl Target {
    /// `fuzz/corpus/<name>/`, the seeds in the source tree.
    pub fn corpus(&self) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus").join(self.name)
    }

    /// `fuzz/out/<name>/`, where a fuzz run keeps what it found (ignored by git).
    fn out(&self) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("out").join(self.name)
    }
}

/// The plain `#[test]` of a target: its corpus files, `seeds()`, and blaze_fuzz's
/// deterministic changes of both (20000 unless `BLAZE_FUZZ_RUNS` says), each through `check`.
pub fn seeds(target: &Target) {
    for input in blaze_fuzz::inputs(target.corpus(), (target.seeds)()) {
        (target.check)(&input);
    }
}

const USAGE: &str = "usage: <target> fuzz [--seconds N] [--runs N] [--seed N] [--max-len N] [--timeout-ms N] [--no-value-profile] [--out DIR]
       <target> run FILE...
       <target> minimize FILE [--seconds N] [--timeout-ms N] [--out DIR]
       <target> seeds
FILE is an input in hexadecimal, as in fuzz/corpus/<target>/.";

enum Command {
    Fuzz,
    Run(Vec<PathBuf>),
    Minimize(PathBuf),
    Seeds,
}

struct Options {
    command: Command,
    seconds: Option<u64>,
    runs: Option<u64>,
    seed: Option<u64>,
    max_len: Option<usize>,
    timeout: Duration,
    value_profile: bool,
    out: Option<PathBuf>,
}

impl Options {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Options, String> {
        let mut args = args.into_iter();
        let command = args.next().ok_or("no command")?;
        let mut options = Options {
            command: Command::Seeds,
            seconds: None,
            runs: None,
            seed: None,
            max_len: None,
            timeout: Duration::from_secs(10),
            value_profile: true,
            out: None,
        };
        let mut files = Vec::new();
        while let Some(arg) = args.next() {
            let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
            let number = |name: &str, text: String| text.parse::<u64>().map_err(|e| format!("{name} {text}: {e}"));
            match arg.as_str() {
                "--seconds" => options.seconds = Some(number(&arg, value(&arg)?)?),
                "--runs" => options.runs = Some(number(&arg, value(&arg)?)?),
                "--seed" => options.seed = Some(number(&arg, value(&arg)?)?),
                "--max-len" => options.max_len = Some(number(&arg, value(&arg)?)? as usize),
                "--timeout-ms" => options.timeout = Duration::from_millis(number(&arg, value(&arg)?)?),
                "--no-value-profile" => options.value_profile = false,
                "--out" => options.out = Some(PathBuf::from(value(&arg)?)),
                flag if flag.starts_with("--") => return Err(format!("unknown flag {flag}")),
                _ => files.push(PathBuf::from(arg)),
            }
        }
        options.command = match command.as_str() {
            "fuzz" | "seeds" if !files.is_empty() => return Err(format!("`{command}` takes no files")),
            "fuzz" => Command::Fuzz,
            "seeds" => Command::Seeds,
            "run" if files.is_empty() => return Err("`run` needs a file".to_owned()),
            "run" => Command::Run(files),
            "minimize" if files.len() == 1 => Command::Minimize(files.remove(0)),
            "minimize" => return Err("`minimize` takes one file".to_owned()),
            other => return Err(format!("unknown command {other:?}")),
        };
        Ok(options)
    }
}

/// A target binary's `main`.
pub fn main(target: &Target) -> ExitCode {
    raw::keep();
    let options = match Options::parse(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(e) => {
            eprintln!("{}: {e}\n{USAGE}", target.name);
            return ExitCode::from(2);
        }
    };
    let out = options.out.clone().unwrap_or_else(|| target.out());
    let done = match &options.command {
        Command::Fuzz => fuzz(target, &options, &out),
        Command::Run(files) => run(target, files),
        Command::Minimize(file) => minimize::minimize(
            file,
            &out,
            Duration::from_secs(options.seconds.unwrap_or(60)),
            options.timeout,
        ),
        Command::Seeds => {
            seeds(target);
            eprintln!("{}: the seeds and their changes pass", target.name);
            Ok(ExitCode::SUCCESS)
        }
    };
    done.unwrap_or_else(|e| {
        eprintln!("{}: error: {e}", target.name);
        ExitCode::from(2)
    })
}

/// Each file once, through `check`, with nothing caught: a failure ends the process as it
/// would anywhere (what `minimize` watches for).
fn run(target: &Target, files: &[PathBuf]) -> Result<ExitCode, String> {
    for file in files {
        let input = corpus::read(file)?;
        (target.check)(&input);
        eprintln!("{}: {} passes ({} bytes)", target.name, file.display(), input.len());
    }
    Ok(ExitCode::SUCCESS)
}

/// What ended a run.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Finding {
    Crash,
    Timeout,
}

static OUT: OnceLock<PathBuf> = OnceLock::new();
static EPOCH: OnceLock<Instant> = OnceLock::new();
static DUMPED: AtomicBool = AtomicBool::new(false);
static NAME: OnceLock<&'static str> = OnceLock::new();

fn now_ms() -> u64 {
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// Writes the input being run to `out/<crash|timeout>-<hash>.hex`, once per process, and says
/// how to replay and minimize it. Called from the panic hook, AddressSanitizer's error hook and
/// the watchdog.
pub(crate) fn dump(finding: Finding) {
    if DUMPED.swap(true, Ordering::AcqRel) {
        return;
    }
    let (Some(out), Some(input)) = (OUT.get(), raw::current()) else {
        return;
    };
    let prefix = match finding {
        Finding::Crash => "crash-",
        Finding::Timeout => "timeout-",
    };
    let name = NAME.get().copied().unwrap_or("<target>");
    match corpus::write(out, prefix, &input) {
        Ok(path) => eprintln!(
            "blaze-fuzzer: {finding:?} on {} bytes, written to {}\n  replay:   {name} run {}\n  minimize: {name} minimize {}",
            input.len(),
            path.display(),
            path.display(),
            path.display()
        ),
        Err(e) => eprintln!("blaze-fuzzer: {finding:?}, and its input could not be written: {e}"),
    }
}

/// Ends the process, writing the input out, when one runs past `timeout` milliseconds. Its
/// counters are the engine's, not the target's: `Coverage::new` drops them.
#[inline(never)]
fn watchdog(timeout: u64) {
    loop {
        std::thread::sleep(Duration::from_millis((timeout / 4).clamp(10, 1000)));
        if raw::claim_timed_out(now_ms(), timeout) {
            eprintln!("blaze-fuzzer: an input ran past {timeout} ms");
            dump(Finding::Timeout);
            std::process::abort();
        }
    }
}

/// The fuzz loop: until the time budget or the count of runs is spent, or a run fails.
fn fuzz(target: &Target, options: &Options, out: &Path) -> Result<ExitCode, String> {
    let coverage = Coverage::new(&[watchdog as usize, dump as usize, now_ms as usize])?;
    if !raw::asan() {
        return Err(format!("this binary was built without AddressSanitizer: {FLAGS}"));
    }
    if options.value_profile && !raw::return_address_works() {
        return Err(
            "the code generator gives a null `core::intrinsics::return_address`, which the value profile needs to \
             tell comparisons apart; give --no-value-profile to fuzz without it"
                .to_owned(),
        );
    }
    let kept = out.join("corpus");
    std::fs::create_dir_all(&kept).map_err(|e| format!("{}: {e}", kept.display()))?;
    _ = OUT.set(out.to_owned());
    _ = NAME.set(target.name);
    now_ms();
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        hook(info);
        dump(Finding::Crash);
    }));
    let timeout = options.timeout.as_millis() as u64;
    std::thread::Builder::new()
        .name("watchdog".to_owned())
        .spawn(move || watchdog(timeout))
        .map_err(|e| format!("the watchdog: {e}"))?;

    let seed = options.seed.unwrap_or_else(|| {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |it| it.as_nanos() as u64)
    });
    let budget = Duration::from_secs(options.seconds.unwrap_or(target.budget.as_secs()));
    let deadline = Instant::now() + budget;
    let runs = options.runs.unwrap_or(u64::MAX);
    let mut fuzzer = Fuzzer {
        target,
        coverage: &coverage,
        seen: Seen::new((coverage.total() << 3) + raw::VALUE_BITS),
        corpus: Corpus::default(),
        features: Vec::with_capacity(coverage.total() + raw::VALUE_BITS),
        value_profile: options.value_profile,
        runs: 0,
        started: Instant::now(),
    };
    eprintln!(
        "{}: seed {seed}, {} counters, {budget:?}, out {}",
        target.name,
        coverage.total(),
        out.display()
    );

    // The empty input, the corpus files, the seeds built in code, and what earlier runs kept.
    let mut initial = vec![Vec::new()];
    initial.extend(corpus::read_dir(&target.corpus())?);
    initial.extend((target.seeds)());
    let built = initial.len();
    initial.extend(corpus::read_dir(&kept)?);
    for (i, input) in initial.into_iter().enumerate() {
        match fuzzer.try_input(input)? {
            Tried::Failed => return Ok(fuzzer.failed()),
            Tried::Kept(input) if i < built => {
                corpus::write(&kept, "", input)?;
            }
            Tried::Kept(_) | Tried::Dropped(_) => {}
        }
    }
    if fuzzer.corpus.len() == 0 {
        return Err(format!("no input reached a counter: `{}` itself was built without coverage", target.name));
    }
    fuzzer.status("loaded");

    let max_len = options.max_len.unwrap_or(target.max_len);
    let mut mutator = Mutator::new(seed, target.dictionary, Traced);
    let mut data = Vec::with_capacity(max_len);
    let mut next_status = 1u64;
    let mut last_status = Instant::now();
    while fuzzer.runs < runs && Instant::now() < deadline {
        data.clear();
        data.extend_from_slice(fuzzer.corpus.pick(&mut mutator.random));
        let other = fuzzer.corpus.pick(&mut mutator.random);
        mutator.mutate(&mut data, other, max_len);
        match fuzzer.try_input(std::mem::take(&mut data))? {
            Tried::Failed => return Ok(fuzzer.failed()),
            Tried::Kept(input) => {
                corpus::write(&kept, "", input)?;
                data = Vec::with_capacity(max_len);
            }
            Tried::Dropped(input) => data = input,
        }
        if fuzzer.runs >= next_status || last_status.elapsed() > Duration::from_secs(10) {
            fuzzer.status("pulse");
            next_status = fuzzer.runs.saturating_mul(2);
            last_status = Instant::now();
        }
    }
    fuzzer.status("done");
    Ok(ExitCode::SUCCESS)
}

struct Fuzzer<'a> {
    target: &'a Target,
    coverage: &'a Coverage<'static>,
    seen: Seen,
    corpus: Corpus,
    features: Vec<u32>,
    value_profile: bool,
    runs: u64,
    started: Instant,
}

enum Tried<'c> {
    Failed,
    /// It reached something new: the corpus has it now.
    Kept(&'c [u8]),
    /// Nothing new: the buffer back, to reuse.
    Dropped(Vec<u8>),
}

impl Fuzzer<'_> {
    fn try_input(&mut self, input: Vec<u8>) -> Result<Tried<'_>, String> {
        self.features.clear();
        let passed = self.coverage.run(self.target.check, &input, now_ms());
        self.coverage.drain(&mut self.features, self.value_profile);
        self.runs += 1;
        if !passed {
            return Ok(Tried::Failed);
        }
        let new = self.seen.merge(&self.features);
        if new == 0 {
            return Ok(Tried::Dropped(input));
        }
        self.corpus.add(input, new);
        Ok(Tried::Kept(self.corpus.last()))
    }

    fn failed(&self) -> ExitCode {
        // The panic hook wrote the input out; an AddressSanitizer report never returns here.
        eprintln!("{}: failed after {} runs", self.target.name, self.runs);
        ExitCode::FAILURE
    }

    fn status(&self, what: &str) {
        let seconds = self.started.elapsed().as_secs_f64().max(1e-3);
        eprintln!(
            "#{} {what} features: {} corpus: {} ({} bytes) exec/s: {:.0}",
            self.runs,
            self.seen.count(),
            self.corpus.len(),
            self.corpus.bytes(),
            self.runs as f64 / seconds
        );
    }
}

//! `blaze build|run|test|check|bench` and `cargo blaze <the same>`, `blaze stop`, `blaze
//! status`: a client of the agent API with cargo's flags, output and exit codes
//! (docs/v2/plan/lanes/cargo/notes.md section 7). It reads no manifest and commits nothing (the
//! daemon's watcher has the edits): it finds the checkout, sends one request with the whole
//! command line, prints the responses as cargo prints its build, and runs what `run` and `test`
//! run itself, on the terminal.
//!
//! - [`args`]: cargo's command lines, as its clap reads them.
//! - [`place`]: the checkout, the config files outside it, the environment, the color.
//! - [`daemon`]: finding and starting `blaze serve`, and the [`daemon::Api`] it is spoken to by.
//! - [`print`]: a build's responses as cargo's lines; [`say`] and [`json`] are their texts.
//! - [`run`] and [`test`]: what runs after the build.

pub mod args;
pub mod daemon;
pub mod json;
pub mod place;
pub mod print;
pub mod run;
pub mod say;
pub mod test;

use std::{
    ffi::OsString,
    io::{self, IsTerminal as _, Write},
    process::ExitCode,
    time::Instant,
};

use blaze_api::{Cargo, Request, Response, TestResult, ViewRef};

use self::{
    args::{Asked, Command, Invocation},
    daemon::{Api, Start},
    place::Place,
    print::{Printer, Shown},
    test::{Programs, Speaking},
};

/// cargo's code for a build or test that failed, and for any error of its own.
pub const FAILED: i32 = 101;
/// clap's code for a command line it refuses.
pub const USAGE: i32 = 1;

const HELP: &str = "\
blaze: Rust's cargo, built by blaze's daemon

Usage: blaze <COMMAND> [OPTIONS]
       cargo blaze <COMMAND> [OPTIONS]

Commands:
    build, b    Compile the current package
    check, c    Analyze the current package and report errors, but don't build object files
    run, r      Run a binary or example of the local package
    test, t     Run the tests
    bench       Run the benchmarks
    serve       Run the daemon the other commands are clients of
    stop        Make the daemon's state durable and stop it
    status      What the daemon is doing

Options:
    -V, --version  Print version info and exit
    -h, --help     Print help

Every command takes cargo's flags; `blaze help <COMMAND>` lists them.
";

/// The `blaze` program, and `cargo-blaze`, which cargo runs for `cargo blaze` as
/// `cargo-blaze blaze <command> ...`.
pub fn main(args: impl Iterator<Item = OsString>) -> ExitCode {
    let words = words(args);
    if words.first().is_some_and(|it| it == "serve") {
        return crate::serve::main(words.into_iter().skip(1));
    }
    let code = command(words);
    match u8::try_from(code) {
        Ok(code) => ExitCode::from(code),
        Err(_) => std::process::exit(code),
    }
}

/// The words after the program's name, without the `blaze` cargo passes before them.
pub fn words(args: impl Iterator<Item = OsString>) -> Vec<OsString> {
    let mut words: Vec<OsString> = args.skip(1).collect();
    if words.first().is_some_and(|it| it == "blaze") {
        words.remove(0);
    }
    words
}

/// Whether stderr is colored when nothing says how: clap's choice for its own errors.
fn auto_color() -> bool {
    place::color(Some("auto"), |name| std::env::var(name).ok(), io::stderr().is_terminal()).unwrap_or(false)
}

/// One command line, run: its exit code.
pub fn command(words: Vec<OsString>) -> i32 {
    let asked = match args::parse(words) {
        Ok(asked) => asked,
        Err(usage) => {
            eprint!("{}", args::refusal(&usage, auto_color()));
            return USAGE;
        }
    };
    let said = |result: anyhow::Result<i32>| match result {
        Ok(code) => code,
        Err(error) => {
            eprint!("{}", say::error(&format!("{error:#}"), auto_color()));
            FAILED
        }
    };
    match asked {
        Asked::Help(None) => {
            print!("{HELP}");
            0
        }
        Asked::Help(Some(command)) => {
            print!("{}", args::help(command));
            0
        }
        Asked::Version => {
            println!("{}", crate::version());
            0
        }
        Asked::Unknown(name) => {
            let what = format!(
                "no such command: `{name}`\n\nhelp: blaze's commands are build, check, run, test, bench, \
                 serve, stop and status"
            );
            eprint!("{}", say::error(&what, auto_color()));
            FAILED
        }
        Asked::Stop => said(daemon::stop(&mut io::stderr().lock(), auto_color())),
        Asked::Status => said(daemon::status(&mut io::stdout().lock(), &mut io::stderr().lock(), auto_color())),
        Asked::Cargo(asked) => cargo(&asked),
    }
}

/// The request of a cargo command line, once its place and `Cargo` are known.
pub fn request(asked: &Invocation, cargo: Cargo) -> Request {
    let view = ViewRef::Head(cargo.root.clone());
    let targets = place::targets(asked, cargo);
    match asked.command() {
        Command::Check => Request::Check { view, targets },
        Command::Test | Command::Bench if asked.flags.reached => {
            Request::Test { view, targets, filter: asked.filter.iter().cloned().collect(), all: false }
        }
        Command::Build | Command::Run | Command::Test | Command::Bench => Request::Build { view, targets },
    }
}

/// Everything of the machine the request needs: where it runs and what it sends.
fn prepared(asked: &Invocation, color: bool) -> Result<(Place, Request), String> {
    let cwd = std::env::current_dir().map_err(|error| format!("the current directory: {error}"))?;
    let env = place::env(std::env::vars_os())?;
    let place = place::find(&cwd, asked.flags.manifest_path.as_deref(), place::script(&asked.args))?;
    let home = crate::cargo::home::cargo_home();
    let files = place::config_files(&place, &asked.flags.config, home.as_deref())?;
    let cargo = place::cargo(asked, &place, env, files, color)?;
    let request = request(asked, cargo);
    Ok((place, request))
}

/// `build`, `check`, `run`, `test` or `bench`.
fn cargo(asked: &Invocation) -> i32 {
    let started = Instant::now();
    let refused = |what: &str, color: bool| {
        eprint!("{}", say::error(what, color));
        FAILED
    };
    let var = |name: &str| std::env::var(name).ok();
    let color = match place::color(asked.flags.color.as_deref(), var, io::stderr().is_terminal()) {
        Ok(color) => color,
        Err(what) => return refused(&what, auto_color()),
    };
    let format = match print::format(&asked.flags.message_format) {
        Ok(format) => format,
        Err(what) => return refused(&what, color),
    };
    let (place, request) = match prepared(asked, color) {
        Ok(prepared) => prepared,
        Err(what) => return refused(&what, color),
    };
    let mut client = match daemon::connect(Start::IfMissing) {
        Ok(Some(client)) => client,
        Ok(None) => return refused("no daemon answers, and none could be started", color),
        Err(error) => return refused(&format!("{error:#}"), color),
    };
    let shown = Shown { format, quiet: asked.flags.quiet, verbose: asked.flags.verbose, color };
    let (mut out, mut err) = (io::stdout().lock(), io::stderr().lock());
    let then = drive(&mut client, asked, &place, request, shown, started, &mut out, &mut err, &mut test::Spawned);
    drop(client);
    match then {
        Ok(Then::Exit(code)) => code,
        Ok(Then::Exec(exec)) => {
            drop(out);
            run::exec(&exec, &mut err, color)
        }
        // stdout or stderr closed under it: nothing more can be said.
        Err(_) => FAILED,
    }
}

/// What follows the build.
#[derive(Debug, PartialEq, Eq)]
pub enum Then {
    Exit(i32),
    /// `run`'s program, to become.
    Exec(run::Exec),
}

/// The request sent and its responses printed, then what `run` or `test` runs after it.
#[expect(clippy::too_many_arguments, reason = "the command, where it runs, how it speaks and what it speaks to")]
pub fn drive<A: Api, O: Write, E: Write, P: Programs>(
    api: &mut A,
    asked: &Invocation,
    place: &Place,
    request: Request,
    shown: Shown,
    started: Instant,
    out: &mut O,
    err: &mut E,
    programs: &mut P,
) -> io::Result<Then> {
    let mut printer = Printer::new(shown, &place.root, out, err);
    let mut results: Vec<TestResult> = Vec::new();
    let mut unwritten = None;
    let called = api.call(request, |response| match response {
        Response::TestResult(result) => results.push(result),
        response => {
            if let Err(error) = printer.on(response) {
                unwritten.get_or_insert(error);
            }
        }
    });
    if let Some(error) = unwritten {
        return Err(error);
    }
    let ok = match called {
        Ok(ok) => ok,
        Err(error) => {
            printer.err().write_all(say::error(&error.to_string(), shown.color).as_bytes())?;
            return Ok(Then::Exit(FAILED));
        }
    };
    printer.finish(ok, started.elapsed())?;
    if !ok {
        return Ok(Then::Exit(FAILED));
    }
    let speaking = Speaking { cwd: &place.cwd, color: shown.color, quiet: shown.quiet, verbose: shown.verbose };
    let artifacts = printer.take_artifacts();
    match asked.command() {
        Command::Build | Command::Check => Ok(Then::Exit(0)),
        Command::Run => {
            let exec = run::pick(&artifacts, &asked.flags).and_then(|it| run::planned(it, &asked.program, &place.cwd));
            match exec {
                Ok(exec) => {
                    if !shown.quiet {
                        printer.err().write_all(say::status("Running", &exec.shown(), shown.color).as_bytes())?;
                    }
                    printer.err().flush()?;
                    Ok(Then::Exec(exec))
                }
                Err(what) => {
                    printer.err().write_all(say::error(&what, shown.color).as_bytes())?;
                    Ok(Then::Exit(FAILED))
                }
            }
        }
        Command::Test | Command::Bench if asked.flags.reached => {
            let (out, err) = printer.streams();
            let passed = test::reached(&results, speaking, out, err)?;
            Ok(Then::Exit(if passed { 0 } else { FAILED }))
        }
        Command::Test | Command::Bench => test::run(asked, &artifacts, speaking, printer.err(), programs).map(Then::Exit),
    }
}

#[cfg(test)]
mod tests;

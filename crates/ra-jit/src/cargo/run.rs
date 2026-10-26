//! Running what was built, as cargo does: the program of `cargo run`, the test executables
//! of `cargo test`.

use std::{
    path::Path,
    process::{Command, ExitStatus},
};

use anyhow::Context as _;

use crate::{
    monitor::event::{self, Event, Outcome, Picked},
    plan::Selection,
};

use super::{
    affected::{self, Pick},
    cli::Invocation,
    sandboxed, say,
    state::{Held, Kind, Program, State, as_built, cargo_path},
};

/// The exit status of cargo when a test fails or a program can't be run.
const FAILED: i32 = 101;

/// Runs the program `asked` names, and ends as it ended. `None` when cargo has to say
/// which program that is: there are several, or none.
pub(crate) fn program(
    asked: &Invocation,
    state: &State,
    cwd: &Path,
    held: Option<Held>,
) -> anyhow::Result<Option<i32>> {
    let runnable = |it: &&Program| !it.test && matches!(it.kind, Kind::Bin | Kind::Example);
    let mut programs: Vec<&Program> = state.programs.iter().filter(runnable).collect();
    if let Some(name) = asked.flags.bin.as_ref().or(asked.flags.example.as_ref()) {
        programs.retain(|it| it.name == *name);
    } else if let [default] = programs.iter().filter(|it| it.default_run).collect::<Vec<_>>()[..] {
        // Cargo runs a `default-run` when one package it builds has one.
        programs = vec![*default];
    }
    let &[program] = programs.as_slice() else { return Ok(None) };
    let shown = shown_path(&program.executable, cwd);
    let args = asked.program_args().map(|it| format!(" {it}")).collect::<String>();
    say::status(asked.flags.quiet, "Running", &format!("`{shown}{args}`"));
    let mut command = Command::new(&program.executable);
    configured(&mut command, state);
    command.args(asked.program_args()).envs(program.env.iter().cloned());
    as_built(&mut command)?;
    let status =
        ended(&mut command, held, state).with_context(|| format!("could not execute {shown}"))?;
    Ok(Some(exit_code(status)))
}

/// Sets for `command` what the `[env]` of cargo's configuration sets, as cargo does: what
/// cargo sets for the program itself goes over it.
fn configured(command: &mut Command, state: &State) {
    for it in &state.configured {
        if it.force || std::env::var_os(&it.name).is_none() {
            command.env(&it.name, &it.value);
        }
    }
}

/// Runs `command` to its end, started under `held` or under cargo's locks on the build
/// directories taken now: a cargo building there meanwhile removes the executables and
/// writes them again, and one started then is not there.
fn ended(command: &mut Command, held: Option<Held>, state: &State) -> std::io::Result<ExitStatus> {
    let held = held.unwrap_or_else(|| state.hold());
    let started = started(command);
    drop(held);
    started?.wait()
}

/// The most of what a program of tests printed last that is kept to read its failures from:
/// one whose list of them is longer is taken to have said none.
const PRINTED_KEPT: usize = 4 << 20;

/// Runs `command` as [`ended`] does, with what it prints passed on as it comes, and the tests
/// libtest's summary at its end says failed: `None` when it says none, as a program that
/// crashed, or exited in a test, does not. libtest colors its output as it would on this
/// terminal, which it does not see through the pipe.
fn listing_failures(
    command: &mut Command,
    held: Option<Held>,
    state: &State,
) -> std::io::Result<(ExitStatus, Option<Vec<String>>, Vec<Ended>)> {
    use std::io::{IsTerminal as _, Read as _, Write as _};
    let out = std::io::stdout();
    let colored = command.get_args().any(|it| {
        let it = it.to_string_lossy();
        it == "--color" || it.starts_with("--color=")
    });
    if !colored && out.is_terminal() {
        command.arg("--color=always");
    }
    command.stdout(std::process::Stdio::piped());
    let held = held.unwrap_or_else(|| state.hold());
    let child = started(command);
    drop(held);
    let mut child = child?;
    let begun = std::time::Instant::now();
    let (mut printed, mut ended, mut line) = (Vec::new(), Vec::new(), Vec::new());
    if let Some(mut from) = child.stdout.take() {
        let mut chunk = [0; 8192];
        loop {
            let read = match from.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            // A reader that went away does not stop the tests, as it would not under cargo.
            let mut out = out.lock();
            _ = out.write_all(&chunk[..read]).and_then(|()| out.flush());
            printed.extend_from_slice(&chunk[..read]);
            for &byte in &chunk[..read] {
                if byte != b'\n' {
                    line.push(byte);
                    continue;
                }
                let at = begun.elapsed().as_micros() as u64;
                ended.extend(ended_in(&String::from_utf8_lossy(&line), at));
                line.clear();
            }
            if printed.len() > PRINTED_KEPT {
                printed.drain(..printed.len() - PRINTED_KEPT);
            }
        }
    }
    let status = child.wait()?;
    Ok((status, failures(&String::from_utf8_lossy(&printed)), ended))
}

/// A test as libtest said it ended: its name, how, and when from the program's start.
type Ended = (String, Outcome, u64);

/// The test a line of libtest's says ended, `test NAME ... ok`, as it ended at `at`.
fn ended_in(line: &str, at: u64) -> Option<Ended> {
    let plain = without_colors(line);
    let (name, outcome) = plain.trim_end().strip_prefix("test ")?.rsplit_once(" ... ")?;
    let outcome = match outcome {
        "ok" => Outcome::Passed,
        "FAILED" => Outcome::Failed,
        it if it.starts_with("ignored") => Outcome::Ignored,
        _ => return None,
    };
    Some((name.to_owned(), outcome, at))
}

/// What the monitor is told of a run of the tests of `test`: each test, why it ran or did
/// not, and how it ended.
fn noted(
    state: &State,
    test: &Program,
    pick: &Pick,
    ended: &[Ended],
    status: i32,
    at_us: u64,
    took_us: u64,
) -> event::Tests {
    let ended: std::collections::BTreeMap<&str, &Ended> =
        ended.iter().map(|it| (it.0.as_str(), it)).collect();
    let outcome = |name: &str, ran: bool| match ended.get(name) {
        Some(&&(_, outcome, at)) => (outcome, at),
        None if ran => (Outcome::Unknown, 0),
        None => (Outcome::NotRun, 0),
    };
    let mut tests = Vec::new();
    let mut one = |name: &str, picked, why: &str, path: &str, ran| {
        let (outcome, ended_us) = outcome(name, ran);
        let (why, path) = (why.to_owned(), path.to_owned());
        tests.push(event::Test { name: name.to_owned(), picked, why, path, outcome, ended_us });
    };
    let (selection, total) = match pick {
        Pick::Asked => {
            for (name, outcome, at) in ended.values() {
                let (why, path) = (String::new(), String::new());
                tests.push(event::Test {
                    name: name.clone(),
                    picked: Picked::Every,
                    why,
                    path,
                    outcome: *outcome,
                    ended_us: *at,
                });
            }
            ("all", ended.len())
        }
        Pick::Some { names, total, always, apart, passed } => {
            let chosen: std::collections::BTreeSet<&str> =
                names.iter().flatten().map(String::as_str).collect();
            let apart: std::collections::BTreeMap<&str, &str> =
                apart.iter().map(|(name, why)| (name.as_str(), why.as_str())).collect();
            for it in always {
                one(&it.name, Picked::Always, &it.why, it.path.as_deref().unwrap_or(""), true);
            }
            for name in passed.names() {
                if let Some(why) = apart.get(name) {
                    one(name, Picked::Sandboxed, why, "", true);
                } else if names.is_none() || chosen.contains(name) {
                    one(name, Picked::Changed, "", "", true);
                } else {
                    one(name, Picked::Skipped, "", "", false);
                }
            }
            ("affected", *total)
        }
    };
    event::Tests {
        workspace: crate::monitor::shown(&state.root),
        package: test.package.clone(),
        kind: format!("{:?}", test.kind).to_lowercase(),
        executable: crate::monitor::shown(&test.executable),
        selection: selection.to_owned(),
        total: total as u64,
        status,
        at_us,
        took_us,
        tests,
    }
}

/// The tests libtest's summary lists as failed in what a program of tests printed: the names
/// under the last `failures:` before `test result: FAILED`.
fn failures(printed: &str) -> Option<Vec<String>> {
    let plain = without_colors(printed);
    let end = plain.rfind("\ntest result: FAILED")?;
    let start = plain[..end].rfind("\nfailures:\n")? + "\nfailures:\n".len();
    let names: Vec<String> = plain[start..end]
        .lines()
        .filter_map(|it| it.strip_prefix("    "))
        .map(str::to_owned)
        .collect();
    (!names.is_empty()).then_some(names)
}

/// `text` without the escapes that color a terminal's text.
fn without_colors(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        if chars.next() == Some('[') {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        }
    }
    out
}

/// Starts `command` once the daemon is not writing its program (`splice::unwritten`): one
/// started while the file is open for writing is refused ("text file busy").
pub(crate) fn started(command: &mut Command) -> std::io::Result<std::process::Child> {
    #[cfg(unix)]
    let _unwritten = crate::splice::unwritten(command.get_program().as_ref());
    command.spawn()
}

/// Runs the test executables one after another, each in the directory of its package, and
/// stops at the first that fails unless told not to. The result is cargo's exit status. Under
/// `RA_JIT_TESTS=affected` each runs only the tests an edit reached since they passed
/// ([`affected`]).
pub(crate) fn tests(
    asked: &Invocation,
    state: &State,
    cwd: &Path,
    mut held: Option<Held>,
) -> anyhow::Result<i32> {
    let mut failed = Vec::new();
    let (mut left, mut always_run) = (0, 0);
    let sandbox = sandboxed::wanted(affected::on());
    if let Some(Err(why)) = &sandbox {
        say::fallback(
            &format!("tests run outside a sandbox: {why}"),
            "those that reach files, the environment, processes or the network run every time",
        );
    }
    for test in state.programs.iter().filter(|it| it.test) {
        let source = test.source.strip_prefix(&test.package_dir).unwrap_or(&test.source);
        let unit = if matches!(test.kind, Kind::Test | Kind::Bench) { "" } else { "unittests " };
        let shown = shown_path(&test.executable, cwd);
        let mut running = format!("{unit}{} ({shown})", source.display());
        let mut picked = Vec::new();
        let mut command = Command::new(&test.executable);
        configured(&mut command, state);
        command.envs(test.env.iter().cloned());
        command.current_dir(&test.package_dir);
        as_built(&mut command)?;
        let sandbox_env = matches!(sandbox, Some(Ok(()))).then(|| sandboxed::env(&command));
        let identity = sandbox_env.as_deref().map(sandboxed::identity_of);
        let pick = affected::pick(asked, state, test, identity.as_deref());
        let (at_us, begun) = (crate::monitor::now_us(), std::time::Instant::now());
        if let Pick::Some { names: Some(names), total, .. } = &pick {
            left += total - names.len();
            if names.is_empty() {
                let note = noted(state, test, &pick, &[], 0, at_us, 0);
                crate::monitor::note_now(Event::Tests(note));
                let unchanged = format!("{}, unchanged since the last pass", counted(*total));
                say::status(asked.flags.quiet, "Skipping", &format!("{running}: {unchanged}"));
                continue;
            }
            running.push_str(&format!(": {} of {}", names.len(), counted(*total)));
            picked.push("--exact".to_owned());
            picked.extend(names.iter().cloned());
        }
        say::status(asked.flags.quiet, "Running", &running);
        if let Pick::Some { always, apart, .. } = &pick {
            always_run += always.len();
            for test in always.iter().filter(|_| asked.flags.verbose) {
                let by = test.path.as_ref().map(|it| format!(", by {it}")).unwrap_or_default();
                say::status(
                    asked.flags.quiet,
                    "Always",
                    &format!("{}: {}{by}", test.name, test.why),
                );
            }
            for (name, why) in apart.iter().filter(|_| asked.flags.verbose) {
                say::status(asked.flags.quiet, "Sandboxed", &format!("{name}: {why}"));
            }
            let names: Vec<String> = apart.iter().map(|(name, _)| name.clone()).collect();
            if let (Some(env), Some(dir), false) =
                (sandbox_env, state.plan_dir(test), names.is_empty())
            {
                sandboxed::prepare(state, test, dir, &mut command, env, &names)
                    .context("writing what the sandbox of the tests is")?;
            }
        }
        command.args(asked.program_args()).args(&picked);
        let ran = match &pick {
            Pick::Some { .. } => listing_failures(&mut command, held.take(), state),
            Pick::Asked => ended(&mut command, held.take(), state).map(|it| (it, None, Vec::new())),
        };
        let (status, failures, said) = ran.with_context(|| format!("could not execute {shown}"))?;
        let took_us = begun.elapsed().as_micros() as u64;
        let code = status.code().unwrap_or(-1);
        let note = noted(state, test, &pick, &said, code, at_us, took_us);
        crate::monitor::note_now(Event::Tests(note));
        match pick {
            Pick::Some { passed, names, .. } if !status.success() => {
                passed.failed(names.as_deref(), failures.as_deref())
            }
            Pick::Some { passed, .. } => passed.passed(),
            Pick::Asked => {}
        }
        if !status.success() {
            failed.push(rerun_flags(test));
            if !asked.flags.no_fail_fast {
                break;
            }
        }
    }
    let why = if asked.flags.verbose { "said above" } else { "`-v` says why" };
    let always = format!("{} run every time ({why})", counted(always_run));
    if left > 0 {
        let all = format!("`{}=all` runs every test", affected::VAR);
        let unchanged = format!("{}, unchanged since the last pass", counted(left));
        let unchanged = match always_run {
            0 => format!("{unchanged}; {all}"),
            _ => format!("{unchanged}; {always}; {all}"),
        };
        say::status(asked.flags.quiet, "Skipped", &unchanged);
    } else if always_run > 0 {
        say::status(asked.flags.quiet, "Skipped", &format!("no test; {always}"));
    }
    match failed.as_slice() {
        [] => Ok(0),
        [one] => {
            say::error(&format!("test failed, to rerun pass `{one}`"));
            Ok(FAILED)
        }
        several => {
            say::error(&format!("{} targets failed:", several.len()));
            several.iter().for_each(|it| eprintln!("    `{it}`"));
            Ok(FAILED)
        }
    }
}

/// Runs the doc tests with cargo, which hands them to ra-jit in rustdoc's place: ra-jit
/// builds each example as its own program over its code of the library.
pub(crate) fn doc_tests(asked: &Invocation) -> anyhow::Result<i32> {
    let mut args = vec!["--doc".to_owned()];
    args.extend(asked.build_args.iter().cloned());
    if asked.flags.no_fail_fast {
        args.push("--no-fail-fast".to_owned());
    }
    args.extend(asked.positional.iter().cloned());
    args.push("--".to_owned());
    args.extend(asked.trailing.iter().cloned());
    only_doc_tests(asked, &args)
}

/// Runs `cargo test` with `args`, which ask for the doc tests alone, with ra-jit in
/// rustdoc's place. ra-jit is told the build `asked` is, whose features cargo builds the
/// libraries with. Cargo's run of rustdoc for each library is only written down, and they are
/// made once cargo is done, by one run of ra-jit that loads the workspace once for them all.
pub(crate) fn only_doc_tests(asked: &Invocation, args: &[String]) -> anyhow::Result<i32> {
    use std::io::{BufRead as _, IsTerminal as _, Write as _};
    let rustdoc = std::env::var_os("RUSTDOC").unwrap_or_else(|| "rustdoc".into());
    let cwd = std::env::current_dir()?;
    let asked_in = asked.flags.manifest_path.as_ref().map_or(cwd.clone(), |it| cwd.join(it));
    let build = Selection { asked_in: Some(asked_in), ..asked.selection() };
    let replay = crate::doctest::Replay {
        dir: std::env::temp_dir().join(format!("ra-jit-doctests-{}", std::process::id())),
        no_fail_fast: asked.flags.no_fail_fast,
        quiet: asked.flags.quiet,
    };
    std::fs::create_dir_all(&replay.dir)?;
    let mut cargo = crate::doctest::as_asked(cargo_path());
    cargo
        .env("RUSTDOC", std::env::current_exe().context("finding ra-jit itself")?)
        .env(crate::doctest::ENABLED, rustdoc)
        .env(crate::doctest::BUILD, serde_json::to_string(&build)?)
        .env(crate::doctest::RECORD, &replay.dir);
    // The daemon's store, as its workers have it: what the examples link of the standard
    // library is compiled once, not for each library each run.
    match super::daemon::store() {
        Some(store) => cargo.env("RA_JIT_CACHE", store),
        None => cargo.env_remove("RA_JIT_CACHE"),
    };
    // Cargo says `Doc-tests` of every library before any of them runs: the run that makes
    // them says it of each as it does. Its colors stay as they are on a terminal.
    cargo.stderr(std::process::Stdio::piped());
    let terminal = std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    if terminal && std::env::var_os("CARGO_TERM_COLOR").is_none() {
        cargo.env("CARGO_TERM_COLOR", "always");
    }
    cargo.arg("test").args(args);
    as_built(&mut cargo)?;
    let mut cargo = cargo.spawn().context("running `cargo test --doc`")?;
    let said = cargo.stderr.take().context("cargo's standard error is not read")?;
    let mut stderr = std::io::stderr().lock();
    for line in std::io::BufReader::new(said).split(b'\n') {
        let line = line?;
        if !says_doc_tests(&line) {
            stderr.write_all(&line)?;
            stderr.write_all(b"\n")?;
        }
    }
    drop(stderr);
    let code = exit_code(cargo.wait().context("running `cargo test --doc`")?);
    let replayed = match code {
        0 => replay.command().and_then(|ra_jit| {
            ra_jit
                .map(|mut it| Ok(exit_code(it.status().context("running the doctests")?)))
                .transpose()
        }),
        _ => Ok(None),
    };
    _ = std::fs::remove_dir_all(&replay.dir);
    Ok(replayed?.unwrap_or(code))
}

/// Whether cargo's line is the `Doc-tests <crate>` it says as it runs rustdoc for a library.
fn says_doc_tests(line: &[u8]) -> bool {
    let (text, mut plain) = (String::from_utf8_lossy(line), String::new());
    let mut chars = text.chars();
    while let Some(char) = chars.next() {
        match char {
            // A color: `ESC [ ... m`.
            '\x1b' => _ = chars.by_ref().find(char::is_ascii_alphabetic),
            _ => plain.push(char),
        }
    }
    let name = plain.trim().strip_prefix("Doc-tests ");
    name.is_some_and(|it| !it.is_empty() && !it.contains(' '))
}

fn counted(count: usize) -> String {
    match count {
        1 => "1 test".to_owned(),
        _ => format!("{count} tests"),
    }
}

/// The flags that have cargo run this test target alone.
fn rerun_flags(test: &Program) -> String {
    let target = match test.kind {
        Kind::Lib => "--lib".to_owned(),
        Kind::Bin => format!("--bin {}", test.name),
        Kind::Example => format!("--example {}", test.name),
        Kind::Test => format!("--test {}", test.name),
        Kind::Bench => format!("--bench {}", test.name),
    };
    format!("-p {} {target}", test.package)
}

/// The path as cargo shows it: from the directory the command runs in, when it is below.
fn shown_path(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd).unwrap_or(path).display().to_string()
}

/// What this command ends with after a process that ended with `status`: its code, or
/// the code a shell gives one a signal ended.
pub(crate) fn exit_code(status: ExitStatus) -> i32 {
    #[cfg(unix)]
    if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(&status) {
        return 128 + signal;
    }
    status.code().unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::failures;

    #[test]
    fn the_summary_names_the_failures() {
        let pretty = "\nrunning 3 tests\ntest a ... ok\ntest b ... FAILED\ntest c::d ... FAILED\n\n\
            failures:\n\n---- b stdout ----\nfailures:\n    not_a_test\n\n---- c::d stdout ----\n\n\
            failures:\n    b\n    c::d\n\ntest result: FAILED. 1 passed; 2 failed; 0 ignored\n\n";
        assert_eq!(failures(pretty), Some(vec!["b".to_owned(), "c::d".to_owned()]));
        let colored =
            "\nfailures:\n    b\n\ntest result: \x1b[31mFAILED\x1b[0m. 0 passed; 1 failed\n";
        assert_eq!(failures(colored), Some(vec!["b".to_owned()]));
        assert_eq!(
            failures("\nrunning 1 test\ntest a ... ok\n\ntest result: ok. 1 passed\n"),
            None
        );
        // A program that stopped in a test has no summary.
        assert_eq!(failures("\nrunning 2 tests\ntest a ... ok\n"), None);
    }
}

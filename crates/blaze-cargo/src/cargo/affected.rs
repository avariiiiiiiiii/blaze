//! `blaze test` runs, of each program of tests the daemon built, the tests that have not
//! passed with what they are a function of as it is now, and says how many passed before and
//! where ([`results`]). The daemon writes beside the program a hash of each test's code with
//! all it reaches, and what it reaches that its code does not decide ([`plan::REACH`]). A
//! hermetic test, one that reaches nothing outside the program and nothing blaze can't see
//! into, is kept by that hash alone. A test that reaches the world outside its code runs
//! every time, unless the workspace opts into sandboxed tests: the sandbox records what it
//! read, and that is part of its key too ([`results::Results::find`]). One that reaches what
//! nothing records (foreign code, assembly, a static other tests change, code blaze can't
//! compile) runs every time. `BLAZE_TESTS=all` runs every test, as cargo does.
//!
//! `BLAZE_TESTS=verify` picks the same, runs every test anyway and says of each that had a
//! pass whether it passed again: one that failed is flaky, or its key misses something it is a
//! function of, which is a bug.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use super::{
    cli::Invocation,
    plan,
    results::{self, Found, Key, Passed, Results},
    state::{Kind, Program, State},
};
use crate::sandbox::Line;

pub const VAR: &str = "BLAZE_TESTS";

/// The most bytes of names a program of tests is given to pick its tests by; past it, it runs
/// them all.
const NAMES_AT_MOST: usize = 64 << 10;

/// Whether this command runs only the tests that have not passed as they are now, or checks
/// that those that did pass again ([`verifying`]): unless `BLAZE_TESTS=all`.
pub fn on() -> bool {
    std::env::var(VAR).map_or(true, |it| it != "all")
}

/// Whether this command runs every test and checks that those that passed before pass again.
pub fn verifying() -> bool {
    std::env::var(VAR).is_ok_and(|it| it == "verify")
}

/// Has the daemon write [`plan::REACH`] at its builds of the programs of tests of `state`
/// while only the tests an edit reached are run, and stop when every test is. The one there
/// stays: a build without the ask takes it away, so one left is of the program as it is, and
/// a command after `BLAZE_TESTS=all` uses it with no build for it.
pub fn ask(state: &State) {
    let on = on();
    for program in state.programs.iter().filter(|it| it.test) {
        let Some(dir) = state.plan_dir(program) else { continue };
        let wanted = dir.join(plan::REACH_WANTED);
        match on {
            true if wanted.is_file() => {}
            // A fresh target has no directory for the program until it is built.
            true => _ = std::fs::create_dir_all(dir).and_then(|()| std::fs::write(&wanted, "")),
            false => _ = std::fs::remove_file(&wanted),
        }
    }
}

/// What of a program of tests this command runs.
pub enum Pick {
    /// What the command asks, every test unless it names some: nothing is known of what an
    /// edit reached, or the command picks the tests itself.
    Asked,
    /// The tests `names` names, of the `total` the program has, or all of them when `None`;
    /// `always` are those of them run every time, and `reaching` those that reach the world
    /// outside their code and run in the sandbox with what they read recorded, and why each
    /// runs.
    Some {
        names: Option<Vec<String>>,
        total: usize,
        always: Vec<Always>,
        reaching: Vec<(String, String)>,
        /// The tests that passed under their keys, each with where: not run, or under
        /// [`verifying`] run to see that they pass again.
        cached: BTreeMap<String, String>,
        /// The tests that run every time for having passed and failed under one key, with
        /// where they passed.
        flaky: Vec<(String, String)>,
        keys: Keys,
        /// Every test of the program, in the order the daemon wrote them.
        planned: Vec<Planned>,
    },
}

/// A test that is run every time, and why.
pub struct Always {
    pub name: String,
    pub why: String,
    /// The calls that take the test to what it reaches, from the test on, joined by ` -> `.
    pub path: Option<String>,
}

/// What is known of a test of a program before it runs, for `tests.ndjson`.
pub struct Planned {
    pub name: String,
    /// Whether its end is a function of its code alone.
    pub hermetic: bool,
    /// What makes it not hermetic and what it reaches that does not, each a JSON list of
    /// [`plan::Reason`]s as [`plan::REACH`] has it.
    pub reasons: String,
    pub nondeterministic: String,
    pub cached: Cached,
}

/// Whether a test's pass is kept, as `tests.ndjson` has it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cached {
    /// It passed under its key as it is now, and is not run (but under [`verifying`]).
    Hit,
    /// It has a key and no pass under it: it runs, and a pass is kept.
    Miss,
    /// It runs every time and is never kept.
    Never,
}

impl Cached {
    pub fn name(self) -> &'static str {
        match self {
            Cached::Hit => "hit",
            Cached::Miss => "miss",
            Cached::Never => "never",
        }
    }
}

/// The first key of each test of a program that may be kept, and whether it reaches the world
/// outside its code; and the program's [`Passed`].
pub struct Keys(BTreeMap<String, (Key, bool)>, std::cell::RefCell<Passed>);

/// What a run of a program of tests ended with, for [`Keys::settle`].
pub struct Ended<'a> {
    /// The tests it was asked to run, all `total` of them when `None`.
    pub ran: Option<&'a [String]>,
    pub total: usize,
    /// Whether it failed.
    pub failed: bool,
    /// The tests libtest's summary said failed, or the sandbox failed, when that is known.
    pub failures: Option<&'a [String]>,
    /// How many tests libtest's summaries counted (passed, failed and ignored).
    pub counted: Option<usize>,
    /// What the sandbox recorded of the run.
    pub record: Option<&'a [Line]>,
}

impl Keys {
    /// Keeps in `results` how the tests of a run `ended`: those that passed, and those that
    /// failed, for a flaky test to be told. Nothing when it is not known which tests ran and
    /// how each ended. The names of the tests found flaky.
    pub fn settle(&self, results: &mut Results, ended: &Ended<'_>) -> Vec<String> {
        let ran: Vec<&String> = match ended.ran {
            Some(names) => names.iter().collect(),
            None => self.0.keys().collect(),
        };
        let failures: BTreeSet<&str> = match (ended.failed, ended.failures) {
            (false, _) => BTreeSet::new(),
            (true, Some(failures)) => failures.iter().map(String::as_str).collect(),
            (true, None) => return Vec::new(),
        };
        // A summary that counts other tests than were asked for says nothing of each.
        if ended.counted != Some(ended.ran.map_or(ended.total, <[String]>::len)) {
            return Vec::new();
        }
        let mut flaky = Vec::new();
        let mut passed = self.1.borrow_mut();
        for name in ran {
            let Some((first, reaches)) = self.0.get(name) else { continue };
            let reads = match (reaches, ended.record) {
                (false, _) => None,
                (true, Some(record)) => match results::reads_of(record, name) {
                    Some(reads) => Some(reads),
                    None => continue,
                },
                (true, None) => continue,
            };
            let ok = !failures.contains(name.as_str());
            let found_flaky = results.keep(first, reads.as_ref(), ok);
            match (ok && !found_flaky, reaches) {
                (true, false) => passed.insert(*first, results.from().to_owned()),
                _ => passed.remove(first),
            }
            if found_flaky {
                flaky.push(name.clone());
            }
        }
        passed.write();
        results.flush();
        flaky
    }
}

/// Which tests of `test` this command runs. `results` are the store's, `None` when there is
/// none; `run` stands for what the command builds and runs the tests with; `sandbox` is what
/// stands for the environment of a sandboxed test ([`super::sandboxed::identity_of`]) when the
/// tests run sandboxed, and why they run outside otherwise. A hermetic test is kept by its code
/// wherever it runs; one that reaches the world outside its code is kept with what it read in
/// the sandbox, and runs every time outside it, as those of `outside`, which its package takes
/// out, do.
pub fn pick(
    asked: &Invocation,
    state: &State,
    test: &Program,
    results: Option<&mut Results>,
    run: &str,
    sandbox: Result<&str, &str>,
    outside: &BTreeSet<String>,
) -> Pick {
    if !on() || !asked.positional.is_empty() || !plain(&asked.trailing) {
        return Pick::Asked;
    }
    let Some(dir) = state.plan_dir(test) else { return Pick::Asked };
    let Ok(reach) = std::fs::read_to_string(dir.join(plan::REACH)) else { return Pick::Asked };
    let mut lines = reach.lines();
    // One of an older blaze, written before the program was built again.
    if lines.next() != Some(plan::REACH_FORMAT) {
        return Pick::Asked;
    }
    // Hashed only for a test that is kept: a program is read whole, and an integration test's
    // tests that all run every time need none.
    let hashed = std::cell::OnceCell::new();
    let programs = || hashed.get_or_init(|| programs_run_by(state, test, dir)).as_str();
    let mut results = results;
    let mut passed = Passed::read(results.is_some().then_some(dir));
    let (mut names, mut keys, mut always, mut reaching) =
        (Vec::new(), BTreeMap::new(), Vec::new(), Vec::new());
    let (mut cached, mut flaky, mut total, mut planned) =
        (BTreeMap::new(), Vec::new(), 0, Vec::new());
    for line in lines {
        let mut parts = line.splitn(3, '\t');
        let (Some(hash), Some(name)) = (parts.next(), parts.next()) else { continue };
        if hash == "*" {
            return Pick::Asked;
        }
        total += 1;
        let rest = parts.next().unwrap_or_default();
        if hash == "-" {
            names.push(name.to_owned());
            always.push(Always { name: name.to_owned(), why: rest.to_owned(), path: None });
            let reason = plan::Reason {
                kind: "benchmark".to_owned(),
                what: name.to_owned(),
                why: rest.to_owned(),
                chain: Vec::new(),
            };
            let reasons = serde_json::to_string(&[reason]).unwrap_or_else(|_| "[]".to_owned());
            planned.push(Planned {
                name: name.to_owned(),
                hermetic: false,
                reasons,
                nondeterministic: "[]".to_owned(),
                cached: Cached::Never,
            });
            continue;
        }
        let (blocking, others) = rest.split_once('\t').unwrap_or((rest, "[]"));
        let (reasons, nondeterministic) = (blocking.to_owned(), others.to_owned());
        let (mark, hash) = match hash.as_bytes().first() {
            Some(b'~' | b'!') => (&hash[..1], &hash[1..]),
            _ => ("", hash),
        };
        let hermetic = mark.is_empty();
        let sandboxed = sandbox.ok().filter(|_| !outside.contains(name));
        let reads = mark == "~" && sandboxed.is_some();
        if !hermetic && !reads {
            // Said only under `--verbose`: the reasons of thousands of tests are megabytes.
            let first = match asked.flags.verbose {
                true => serde_json::from_str::<Vec<plan::Reason>>(blocking)
                    .ok()
                    .and_then(|it| it.into_iter().next()),
                false => None,
            };
            let why = first.as_ref().map_or_else(
                || "it reaches what its code does not decide".to_owned(),
                |it| it.why.clone(),
            );
            let path = first.map(|it| {
                it.chain.iter().map(|call| call.name.as_str()).collect::<Vec<_>>().join(" -> ")
            });
            names.push(name.to_owned());
            always.push(Always { name: name.to_owned(), why, path });
            let name = name.to_owned();
            let cached = Cached::Never;
            planned.push(Planned { name, hermetic, reasons, nondeterministic, cached });
            continue;
        }
        let identity = match (reads, sandboxed) {
            (true, Some(env)) => format!("~{hash}{}{env}{run}", programs()),
            _ => format!("{hash}{}{run}", programs()),
        };
        let first = Results::first(&identity, name);
        keys.insert(name.to_owned(), (first, reads));
        let here = passed.get(&first).filter(|_| !reads && !verifying());
        let found = match (here, results.as_mut()) {
            (Some(from), _) => Found::Passed { from: from.to_owned() },
            (None, Some(results)) => results.find(&first, reads),
            (None, None) => Found::Missing(None),
        };
        if let (Found::Passed { from }, false) = (&found, reads) {
            passed.insert(first, from.clone());
        }
        let kept = match &found {
            Found::Passed { .. } => Cached::Hit,
            _ => Cached::Miss,
        };
        planned.push(Planned {
            name: name.to_owned(),
            hermetic,
            reasons,
            nondeterministic,
            cached: kept,
        });
        match found {
            Found::Passed { from } => {
                cached.insert(name.to_owned(), from);
                continue;
            }
            Found::Flaky { from } => flaky.push((name.to_owned(), from)),
            Found::Missing(why) => {
                if let (Some(why), true) = (why, reads) {
                    reaching.push((name.to_owned(), why));
                    names.push(name.to_owned());
                    continue;
                }
            }
        }
        names.push(name.to_owned());
        if reads {
            reaching.push((
                name.to_owned(),
                "it has not passed with what it read as it is now".to_owned(),
            ));
        }
    }
    if verifying() {
        names.extend(cached.keys().cloned());
        reaching.extend(
            cached
                .keys()
                .filter(|it| keys.get(*it).is_some_and(|(_, reaches)| *reaches))
                .map(|it| (it.clone(), "it is verified".to_owned())),
        );
    }
    passed.write();
    let long = names.iter().map(|it| it.len() + 1).sum::<usize>() > NAMES_AT_MOST;
    let names = (names.len() != total && !long).then_some(names);
    let keys = Keys(keys, std::cell::RefCell::new(passed));
    Pick::Some { names, total, always, reaching, cached, flaky, keys, planned }
}

/// What stands for what the command `asked` builds and runs its tests with, beside each test's
/// code: the features, profile, targets, configuration and unstable flags it builds with, the
/// machine, and the harness's flags that change how a test runs (all but those that only change
/// what is shown).
pub fn run_identity(asked: &Invocation) -> String {
    let flags = &asked.flags;
    let mut features = flags.features.clone();
    features.sort();
    let shown_only = |it: &&String| {
        matches!(it.as_str(), "-q" | "--quiet" | "--show-output") || it.starts_with("--color")
    };
    let mut trailing = asked.trailing.iter().filter(|it| !shown_only(it)).peekable();
    let mut harness = Vec::new();
    while let Some(arg) = trailing.next() {
        harness.push(arg.clone());
        if arg == "--test-threads" {
            harness.extend(trailing.next().cloned());
        }
    }
    let fields = [
        format!(
            "features {features:?} all {} no-default {}",
            flags.all_features, flags.no_default_features
        ),
        format!("profile {:?}", flags.profile),
        format!("targets {:?}", flags.targets),
        format!("config {:?} unstable {:?}", flags.config, flags.unstable),
        format!("on {}-{}", std::env::consts::ARCH, std::env::consts::OS),
        format!("harness {harness:?}"),
    ];
    let hash = sha2::Digest::finalize(<sha2::Sha256 as sha2::Digest>::new_with_prefix(
        fields.join("\n").as_bytes(),
    ));
    format!("+{}", crate::content::hex(&hash[..16]))
}

/// Whether what follows `--` leaves the tests to the program to pick and says nothing of how
/// a test ends: what only shows the run otherwise. One that runs the ignored tests, or lists
/// them, runs as asked.
pub fn plain(trailing: &[String]) -> bool {
    let mut args = trailing.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-q" | "--quiet" | "--nocapture" | "--no-capture" | "--show-output" => {}
            "--test-threads" | "--color" if args.next().is_some() => {}
            _ if arg.starts_with("--test-threads=") || arg.starts_with("--color=") => {}
            _ => return false,
        }
    }
    true
}

/// What stands for the programs an integration test of `test`'s package runs, which cargo
/// gives it as `CARGO_BIN_EXE_<name>`: their bytes. Empty for a unit test.
fn programs_run_by(state: &State, test: &Program, dir: &Path) -> String {
    if !matches!(test.kind, Kind::Test | Kind::Bench) {
        return String::new();
    }
    let runs = |it: &&Program| it.package == test.package && it.kind == Kind::Bin && !it.test;
    let paths: Vec<&Path> =
        state.programs.iter().filter(runs).map(|it| it.executable.as_path()).collect();
    results::programs(&paths, Some(dir))
}

#[cfg(test)]
mod tests {
    use super::plain;

    fn of(args: &[&str]) -> bool {
        plain(&args.iter().map(|it| (*it).to_owned()).collect::<Vec<_>>())
    }

    #[test]
    fn what_only_shows_the_run_is_plain() {
        assert!(of(&[]));
        assert!(of(&["--nocapture", "-q", "--test-threads", "4", "--color=never"]));
        assert!(!of(&["--ignored"]));
        assert!(!of(&["--include-ignored"]));
        assert!(!of(&["--list"]));
        assert!(!of(&["--test-threads"]));
        assert!(!of(&["some_test"]));
    }
}

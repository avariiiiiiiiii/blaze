//! What `blaze test` writes of each test it was asked to run, for people and programs to tell
//! what is slow to test and why: `target/blaze/tests.ndjson`, a [`Line`] a test, appended as
//! each test executable finishes and written again at the end sorted by package, target and
//! name, so two runs diff line by line; and `target/blaze/tests.summary.json`, the counts and
//! what makes the most tests not hermetic ([`Summary`]). `blaze test --why` runs no test and
//! prints a table of the same, `--why --json` the lines. docs/cargo.md has the schema.

use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    io::Write as _,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use super::{
    affected::{Cached, Planned},
    plan::Reason,
    state::{Kind, Program},
};

/// The version of the schema of [`Line`] and [`Summary`], their `v`.
pub const SCHEMA: u32 = 1;

/// How many of each ranking [`Summary`] keeps.
const TOP: usize = 50;

/// A test, as a line of `tests.ndjson` ([`ndjson`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub v: u32,
    pub package: String,
    /// `lib`, or the kind and name of the target: `bin:rg`, `test:integration`.
    pub target: String,
    /// libtest's name of it.
    pub name: String,
    /// Whether its end is a function of its code alone, so that a pass is kept by its code.
    pub hermetic: bool,
    /// `hit`: it passed as it is now and was not run; `miss`: it ran, and a pass is kept;
    /// `never`: it runs every time.
    pub cached: String,
    /// `passed`, `failed`, `cached` (not run for a pass kept), `unknown` (its executable
    /// ended without saying) or `not-run` (`--why`).
    pub result: String,
    /// It passed and failed under one key, and runs every time.
    pub flaky: bool,
    /// What makes it not hermetic, a kind once each, by the fewest calls: a JSON list of
    /// [`Reason`]s, as the daemon wrote it.
    pub reasons: String,
    /// What it reaches that a pass does not stand for but only makes a flaky test flaky:
    /// the clock, randomness, threads. A JSON list as `reasons` is.
    pub nondeterministic: String,
}

/// What of a [`Line`] is not its reasons, as `tests.ndjson` has it first.
#[derive(Serialize)]
struct Head<'a> {
    v: u32,
    package: &'a str,
    target: &'a str,
    name: &'a str,
    hermetic: bool,
    cached: &'a str,
    result: &'a str,
    flaky: bool,
}

/// What [`Summary`] reads of a [`Reason`], borrowed from the JSON of a line.
#[derive(Deserialize)]
struct Seen<'a> {
    #[serde(borrow)]
    kind: Cow<'a, str>,
    #[serde(borrow)]
    what: Cow<'a, str>,
    #[serde(borrow)]
    chain: Vec<SeenCall<'a>>,
}

#[derive(Deserialize)]
struct SeenCall<'a> {
    #[serde(rename = "fn", borrow)]
    name: Cow<'a, str>,
    #[serde(borrow)]
    file: Option<Cow<'a, str>>,
    line: Option<u32>,
    col: Option<u32>,
}

impl Line {
    fn order(&self) -> (&str, &str, &str) {
        (&self.package, &self.target, &self.name)
    }
}

/// How a test executable's run ended, for its [`Line`]s.
pub struct Ended<'a> {
    /// `--why`: nothing ran.
    pub planned_only: bool,
    /// The tests it was asked to run, all of them when `None`.
    pub ran: Option<&'a [String]>,
    pub failed: bool,
    /// The tests that failed, when that is known.
    pub failures: Option<&'a [String]>,
    pub verifying: bool,
    /// The tests found flaky.
    pub flaky: &'a BTreeSet<String>,
}

/// The target `program` is, as a [`Line`] names it.
pub fn target_of(program: &Program) -> String {
    let kind = match program.kind {
        Kind::Lib => return "lib".to_owned(),
        Kind::Bin => "bin",
        Kind::Example => "example",
        Kind::Test => "test",
        Kind::Bench => "bench",
    };
    format!("{kind}:{}", program.name)
}

/// The lines of the tests of `program`, as `planned` and how its run `ended` say.
pub fn lines_of(program: &Program, planned: &[Planned], ended: &Ended<'_>) -> Vec<Line> {
    let target = target_of(program);
    let failures: Option<BTreeSet<&str>> =
        ended.failures.map(|it| it.iter().map(String::as_str).collect());
    let ran: Option<BTreeSet<&str>> = ended.ran.map(|it| it.iter().map(String::as_str).collect());
    let mut lines: Vec<Line> = planned
        .iter()
        .map(|test| {
            let name = test.name.as_str();
            let result = match () {
                _ if ended.planned_only => "not-run",
                _ if test.cached == Cached::Hit && !ended.verifying => "cached",
                _ if ran.as_ref().is_some_and(|it| !it.contains(name)) => "not-run",
                _ if !ended.failed => "passed",
                _ => match &failures {
                    Some(failed) if failed.contains(name) => "failed",
                    Some(_) => "passed",
                    None => "unknown",
                },
            };
            Line {
                v: SCHEMA,
                package: program.package.clone(),
                target: target.clone(),
                name: test.name.clone(),
                hermetic: test.hermetic,
                cached: test.cached.name().to_owned(),
                result: result.to_owned(),
                flaky: ended.flaky.contains(name),
                reasons: test.reasons.clone(),
                nondeterministic: test.nondeterministic.clone(),
            }
        })
        .collect();
    lines.sort_by(|a, b| a.order().cmp(&b.order()));
    lines
}

/// `tests.ndjson` as it is written: what `blaze test` ran so far.
pub struct Report {
    dir: PathBuf,
    /// Each line with its text, made once.
    lines: Vec<(Line, String)>,
    file: Option<std::fs::File>,
}

impl Report {
    /// The report of this command, in `target`, the target directory: `tests.ndjson` starts
    /// empty.
    pub fn new(target: &Path) -> Report {
        let dir = target.join("blaze");
        let file = std::fs::create_dir_all(&dir)
            .and_then(|()| std::fs::File::create(dir.join("tests.ndjson")))
            .ok();
        Report { dir, lines: Vec::new(), file }
    }

    /// Adds the lines of a test executable that finished.
    pub fn add(&mut self, lines: Vec<Line>) {
        let lines: Vec<(Line, String)> = lines
            .into_iter()
            .map(|line| {
                let text = ndjson(&line);
                (line, text)
            })
            .collect();
        if let Some(file) = &mut self.file {
            let text: String = lines.iter().map(|(_, text)| text.as_str()).collect();
            _ = file.write_all(text.as_bytes());
        }
        self.lines.extend(lines);
    }

    /// Writes `tests.ndjson` again, sorted, and `tests.summary.json`; the lines, the summary
    /// and the text of `tests.ndjson`.
    pub fn finish(mut self) -> (Vec<Line>, Summary, String) {
        self.lines.sort_by(|(a, _), (b, _)| a.order().cmp(&b.order()));
        let text: String = self.lines.iter().map(|(_, text)| text.as_str()).collect();
        let lines: Vec<Line> = self.lines.into_iter().map(|(line, _)| line).collect();
        let summary = Summary::of(&lines);
        write_whole(&self.dir.join("tests.ndjson"), text.as_bytes());
        if let Ok(json) = serde_json::to_string_pretty(&summary) {
            write_whole(&self.dir.join("tests.summary.json"), format!("{json}\n").as_bytes());
        }
        (lines, summary, text)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

fn write_whole(file: &Path, bytes: &[u8]) {
    let writing = file.with_extension("writing");
    if std::fs::write(&writing, bytes).and_then(|()| std::fs::rename(&writing, file)).is_err() {
        _ = std::fs::remove_file(&writing);
    }
}

/// A line of `tests.ndjson`, with its newline: the reasons are copied in as they are.
pub fn ndjson(line: &Line) -> String {
    let head = Head {
        v: line.v,
        package: &line.package,
        target: &line.target,
        name: &line.name,
        hermetic: line.hermetic,
        cached: &line.cached,
        result: &line.result,
        flaky: line.flaky,
    };
    let Ok(head) = serde_json::to_string(&head) else { return String::new() };
    let head = head.strip_suffix('}').unwrap_or(&head);
    format!(
        "{head},\"reasons\":{},\"nondeterministic\":{}}}\n",
        line.reasons, line.nondeterministic
    )
}

/// What `tests.summary.json` holds: how many tests are hermetic and kept, and what makes the
/// most tests not hermetic, the place to start to make a repository's tests faster.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    pub v: u32,
    pub tests: usize,
    pub hermetic: usize,
    pub not_hermetic: usize,
    /// How many tests are of each `cached`: `hit`, `miss`, `never`.
    pub cached: BTreeMap<String, usize>,
    /// How many tests are of each `result`.
    pub results: BTreeMap<String, usize>,
    /// How many tests each kind makes not hermetic.
    pub by_kind: BTreeMap<String, usize>,
    /// What makes the most tests not hermetic, by kind and what, the most first.
    pub top_reasons: Vec<TopReason>,
    /// The functions of the workspace the most tests reach it through: the last of the
    /// workspace on the way to a reason, the most first.
    pub top_helpers: Vec<TopHelper>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct TopReason {
    pub kind: String,
    pub what: String,
    /// How many tests it makes not hermetic.
    pub tests: usize,
    /// The first of them, by package, target and name.
    pub example: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct TopHelper {
    #[serde(rename = "fn")]
    pub name: String,
    /// Where it calls on to the reason.
    pub file: Option<String>,
    pub line: Option<u32>,
    pub col: Option<u32>,
    /// The kinds of reason reached through it.
    pub kinds: Vec<String>,
    /// How many tests reach a reason through it.
    pub tests: usize,
    /// How many of those reach every reason that makes them not hermetic through it: they
    /// are hermetic once it no longer reaches any.
    pub only_through_it: usize,
}

impl Summary {
    pub fn of(lines: &[Line]) -> Summary {
        let count = |it: &mut BTreeMap<String, usize>, key: &str| {
            *it.entry(key.to_owned()).or_default() += 1;
        };
        let (mut cached, mut results, mut by_kind) =
            (BTreeMap::new(), BTreeMap::new(), BTreeMap::new());
        let seen: Vec<Vec<Seen<'_>>> = (lines.iter())
            .map(|it| serde_json::from_str(&it.reasons).unwrap_or_default())
            .collect();
        // A test by package, target and name, the order of `lines` sorted.
        type Test<'a> = (&'a str, &'a str, &'a str);
        let mut reasons: BTreeMap<(&str, &str), (usize, Test<'_>)> = BTreeMap::new();
        type At<'a> = (&'a str, Option<&'a str>, Option<u32>, Option<u32>);
        let mut helpers: BTreeMap<At<'_>, (usize, usize, BTreeSet<&str>)> = BTreeMap::new();
        for (line, seen) in lines.iter().zip(&seen) {
            count(&mut cached, &line.cached);
            count(&mut results, &line.result);
            let test = (line.package.as_str(), line.target.as_str(), line.name.as_str());
            let mut through = BTreeSet::new();
            for reason in seen {
                count(&mut by_kind, &reason.kind);
                let entry = reasons.entry((&*reason.kind, &*reason.what)).or_insert((0, test));
                entry.0 += 1;
                entry.1 = entry.1.min(test);
                let helper = helper(&reason.chain)
                    .map(|it| (&*it.name, it.file.as_deref(), it.line, it.col));
                through.insert(helper);
                let Some(at) = helper else { continue };
                helpers.entry(at).or_default().2.insert(&*reason.kind);
            }
            let only = through.len() == 1;
            for key in through.into_iter().flatten() {
                if let Some((tests, only_through_it, _)) = helpers.get_mut(&key) {
                    *tests += 1;
                    *only_through_it += usize::from(only);
                }
            }
        }
        let mut top_reasons: Vec<TopReason> = reasons
            .into_iter()
            .map(|((kind, what), (tests, (package, target, name)))| TopReason {
                kind: kind.to_owned(),
                what: what.to_owned(),
                tests,
                example: format!("{package} {target} {name}"),
            })
            .collect();
        top_reasons.sort_by(|a, b| {
            b.tests.cmp(&a.tests).then_with(|| (&a.kind, &a.what).cmp(&(&b.kind, &b.what)))
        });
        top_reasons.truncate(TOP);
        let mut top_helpers: Vec<TopHelper> = helpers
            .into_iter()
            .map(|((name, file, line, col), (tests, only_through_it, kinds))| TopHelper {
                name: name.to_owned(),
                file: file.map(str::to_owned),
                line,
                col,
                kinds: kinds.into_iter().map(str::to_owned).collect(),
                tests,
                only_through_it,
            })
            .collect();
        top_helpers.sort_by(|a, b| {
            (b.only_through_it, b.tests)
                .cmp(&(a.only_through_it, a.tests))
                .then_with(|| (&a.name, &a.file, a.line).cmp(&(&b.name, &b.file, b.line)))
        });
        top_helpers.truncate(TOP);
        let hermetic = lines.iter().filter(|it| it.hermetic).count();
        Summary {
            v: SCHEMA,
            tests: lines.len(),
            hermetic,
            not_hermetic: lines.len() - hermetic,
            cached,
            results,
            by_kind,
            top_reasons,
            top_helpers,
        }
    }
}

/// The last call on `chain` made by code of the workspace, whose file is told relative to it.
fn helper<'a, 'b>(chain: &'b [SeenCall<'a>]) -> Option<&'b SeenCall<'a>> {
    chain.iter().rev().find(|it| it.file.as_deref().is_some_and(|file| !file.starts_with('/')))
}

/// `blaze test --why`'s table of `lines` and `summary`, with where the files are.
pub fn table(lines: &[Line], summary: &Summary, dir: &Path) -> String {
    let mut out = String::new();
    let percent = match summary.tests {
        0 => 100.0,
        tests => summary.hermetic as f64 * 100.0 / tests as f64,
    };
    let of = |key: &str| summary.cached.get(key).copied().unwrap_or(0);
    out.push_str(&format!(
        "{} of {} tests are hermetic ({percent:.0}%): {} passed as they are now, {} to run and \
         keep, {} run every time\n",
        summary.hermetic,
        summary.tests,
        of("hit"),
        of("miss"),
        of("never"),
    ));
    if !summary.top_reasons.is_empty() {
        out.push_str("\nWhat makes the most tests not hermetic:\n");
        out.push_str(&format!("  {:>6}  {:<12}  {}\n", "tests", "kind", "what"));
        for it in summary.top_reasons.iter().take(20) {
            out.push_str(&format!("  {:>6}  {:<12}  {}\n", it.tests, it.kind, it.what));
        }
    }
    if !summary.top_helpers.is_empty() {
        out.push_str(
            "\nThe functions of the workspace they are reached through (a test is hermetic \
             once all it reaches through them is gone):\n",
        );
        out.push_str(&format!("  {:>6}  {:>6}  {}\n", "tests", "only", "function"));
        for it in summary.top_helpers.iter().take(20) {
            let place =
                it.file.as_deref().map(|file| format!(" ({file}:{})", it.line.unwrap_or(0)));
            out.push_str(&format!(
                "  {:>6}  {:>6}  {}{} [{}]\n",
                it.tests,
                it.only_through_it,
                it.name,
                place.unwrap_or_default(),
                it.kinds.join(", ")
            ));
        }
    }
    let not: Vec<&Line> = lines.iter().filter(|it| !it.hermetic).collect();
    if !not.is_empty() {
        out.push_str(&format!("\nNot hermetic ({}):\n", not.len()));
        for line in not {
            let reasons: Vec<Reason> = serde_json::from_str(&line.reasons).unwrap_or_default();
            let why: Vec<String> = reasons
                .iter()
                .map(|it| {
                    let chain: Vec<&str> = it.chain.iter().map(|it| it.name.as_str()).collect();
                    format!("{} `{}` by {}", it.kind, it.what, chain.join(" -> "))
                })
                .collect();
            out.push_str(&format!(
                "  {} {} {}: {}\n",
                line.package,
                line.target,
                line.name,
                why.join("; ")
            ));
        }
    }
    out.push_str(&format!(
        "\nEvery test: {}; the counts: {}\n",
        dir.join("tests.ndjson").display(),
        dir.join("tests.summary.json").display()
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::{super::plan::Call, *};

    fn reason(kind: &str, what: &str, chain: &[(&str, Option<&str>)]) -> Reason {
        Reason {
            kind: kind.to_owned(),
            what: what.to_owned(),
            why: format!("it reaches `{what}`"),
            chain: chain.iter().map(|(name, at)| Call::new((*name).to_owned(), *at)).collect(),
        }
    }

    fn line(name: &str, reasons: Vec<Reason>) -> Line {
        let (reasons, nondeterministic): (Vec<_>, Vec<_>) =
            reasons.into_iter().partition(|it| it.blocks());
        Line {
            v: SCHEMA,
            package: "p".to_owned(),
            target: "lib".to_owned(),
            name: name.to_owned(),
            hermetic: reasons.is_empty(),
            cached: if reasons.is_empty() { "hit" } else { "never" }.to_owned(),
            result: if reasons.is_empty() { "cached" } else { "passed" }.to_owned(),
            flaky: false,
            reasons: serde_json::to_string(&reasons).unwrap(),
            nondeterministic: serde_json::to_string(&nondeterministic).unwrap(),
        }
    }

    #[test]
    fn a_call_is_read_from_where_it_is() {
        let call = Call::new("f".to_owned(), Some("src/lib.rs:3:9"));
        assert_eq!(
            (call.file.as_deref(), call.line, call.col),
            (Some("src/lib.rs"), Some(3), Some(9))
        );
        let call = Call::new("f".to_owned(), None);
        assert_eq!((call.file, call.line, call.col), (None, None, None));
    }

    #[test]
    fn the_summary_ranks_what_makes_the_most_tests_not_hermetic() {
        let fixture = [("tests::fixture", Some("src/tests.rs:4:5")), ("std::fs::read", None)];
        let lines = vec![
            line("tests::a", vec![reason("fs", "open", &[("tests::a", Some("src/tests.rs:9:1"))])]),
            line("tests::b", vec![reason("fs", "open", &fixture)]),
            line(
                "tests::c",
                vec![reason("fs", "open", &fixture), reason("env", "getenv", &fixture)],
            ),
            line("tests::d", vec![reason("random", "RandomState::new", &[])]),
            line("tests::e", vec![]),
        ];
        let summary = Summary::of(&lines);
        assert_eq!((summary.tests, summary.hermetic, summary.not_hermetic), (5, 2, 3));
        assert_eq!(summary.by_kind.get("fs"), Some(&3));
        assert_eq!(summary.by_kind.get("random"), None);
        assert_eq!(summary.top_reasons[0].what, "open");
        assert_eq!(summary.top_reasons[0].tests, 3);
        assert_eq!(summary.top_reasons[0].example, "p lib tests::a");
        let first = &summary.top_helpers[0];
        assert_eq!(first.name, "tests::fixture");
        assert_eq!((first.tests, first.only_through_it), (2, 2));
        assert_eq!(first.kinds, ["env", "fs"]);
        // Sorted, the same lines give the same summary in any order.
        let mut shuffled = lines.clone();
        shuffled.reverse();
        assert_eq!(Summary::of(&shuffled), summary);
    }

    #[test]
    fn a_line_reads_back_as_it_was_written() {
        let written =
            line("tests::a", vec![reason("fs", "open", &[("tests::a", Some("src/a.rs:1:2"))])]);
        let text = ndjson(&written);
        assert!(text.starts_with("{\"v\":1,") && text.ends_with("}\n"));
        assert!(text.contains("\"fn\":\"tests::a\""));
        let read: serde_json::Value = serde_json::from_str(text.trim_end()).unwrap();
        assert_eq!(read["name"], "tests::a");
        assert_eq!(read["hermetic"], false);
        let reasons: Vec<Reason> = serde_json::from_value(read["reasons"].clone()).unwrap();
        assert_eq!(serde_json::to_string(&reasons).unwrap(), written.reasons);
        assert_eq!(read["nondeterministic"], serde_json::json!([]));
    }
}

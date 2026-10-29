//! The agent API's responses ([`blaze_api::Response`]) as text for people: what a client of
//! the API (the CLI, a CI log) prints. Diagnostics as rustc writes them, with the facts they
//! read; tests as libtest writes them. JSON for programs is [`blaze_api::json`]'s.

use blaze_api::{Diagnostic, Level, Ranked, Response, TestOutcome, TestResult};
use blaze_query::QueryKey;
use blaze_vfs::ChangeKind;

use crate::{CYAN, DIM, GREEN, RED, YELLOW, paint};

/// The most bytes of a test's or a run's output shown.
const OUTPUT: usize = 64 << 10;

/// The text of one response, each line ending in a newline; empty for one that says nothing
/// to a person (a request's successful end).
pub fn lines(response: &Response, color: bool) -> String {
    let mut out = String::new();
    write(&mut out, response, color);
    out
}

fn write(out: &mut String, response: &Response, color: bool) {
    match response {
        Response::Committed { root, rebased } => {
            let rebased = if *rebased { " (rebased onto the head)" } else { "" };
            line(out, format!("{} {root}{rebased}", paint(GREEN, "committed", color)));
        }
        Response::Diagnostic(diagnostic) => diagnostic_lines(out, diagnostic, 0, color),
        Response::Built { artifacts } => {
            for artifact in artifacts {
                let word = paint(GREEN, &format!("{:>12}", "Built"), color);
                line(
                    out,
                    format!(
                        "{word} {} {} at {} ({})",
                        artifact.package, artifact.kind, artifact.path, artifact.content
                    ),
                );
            }
        }
        Response::TestResult(result) => test_lines(out, result, color),
        Response::Ran { code, stdout, stderr } => {
            bytes(out, stdout);
            bytes(out, stderr);
            match code {
                Some(0) => {}
                Some(code) => {
                    line(out, format!("{} exited with {code}", paint(RED, "error", color)));
                }
                None => line(out, format!("{} ended by a signal", paint(RED, "error", color))),
            }
        }
        Response::Diff { changes } => {
            for change in changes {
                let mark = match change.kind {
                    ChangeKind::Added => paint(GREEN, "A", color),
                    ChangeKind::Removed => paint(RED, "D", color),
                    ChangeKind::Modified => paint(YELLOW, "M", color),
                };
                line(out, format!("{mark} {}", change.path));
            }
        }
        Response::Why(why) => {
            line(out, format!("{} reran because:", key(&why.query)));
            for changed in &why.chain {
                line(
                    out,
                    format!(
                        "  {} changed ({:?} -> {:?})",
                        key(&changed.key),
                        changed.was,
                        changed.now
                    ),
                );
            }
            if let Some(input) = &why.input {
                line(out, format!("  from the input {input:?}"));
            }
        }
        Response::Status(status) => {
            let requests = status.requests.len();
            line(
                out,
                format!(
                    "{requests} {} under way, {} {} open, {} of {} bytes of memory",
                    if requests == 1 { "request" } else { "requests" },
                    status.views,
                    if status.views == 1 { "view" } else { "views" },
                    status.memory_bytes,
                    status.budget_bytes
                ),
            );
        }
        Response::Done { ok: true } => {}
        Response::Done { ok: false } => line(out, paint(RED, "failed", color)),
        Response::Error(error) => {
            let at = error.at.as_deref().map(|at| format!(" (at {at})")).unwrap_or_default();
            line(out, format!("{}: {}{at}", paint(RED, "error", color), error.what));
        }
        Response::Opened { root } => line(out, format!("{} {root}", paint(GREEN, "opened", color))),
        Response::Reached(reach) => {
            line(
                out,
                format!(
                    "{} files changed; {} items, {} functions to compile, {} tests reached",
                    reach.changes.len(),
                    reach.items.len(),
                    reach.compiled.len(),
                    reach.tests.len()
                ),
            );
            for item in &reach.items {
                line(out, format!("  {}", item.path));
                for fact in &item.facts {
                    line(out, format!("    {} {}", paint(DIM, "changed:", color), fact.what));
                }
            }
            for function in &reach.compiled {
                line(out, format!("  {} {function}", paint(CYAN, "compile", color)));
            }
            for test in &reach.tests {
                line(out, format!("  {} {test}", paint(CYAN, "test", color)));
            }
        }
        Response::Of { candidate, response } => {
            let mut inner = String::new();
            write(&mut inner, response, color);
            for said in inner.lines() {
                line(out, format!("[{candidate}] {said}"));
            }
        }
        Response::Ranked(ranked) => {
            for (place, it) in ranked.iter().enumerate() {
                ranked_line(out, place, it, color);
            }
        }
        Response::Verified { mismatches } if mismatches.is_empty() => {
            let word = paint(GREEN, "verified", color);
            line(out, format!("{word} every result matched a build from nothing"));
        }
        Response::Verified { mismatches } => {
            line(
                out,
                format!(
                    "{}: {} results differ from a build from nothing",
                    paint(RED, "error", color),
                    mismatches.len()
                ),
            );
            for mismatch in mismatches {
                let fact = mismatch
                    .fact
                    .as_ref()
                    .map(|fact| format!(", the fact that missed: {}", fact.what))
                    .unwrap_or_default();
                line(
                    out,
                    format!(
                        "  {}: {:?} kept, {:?} from nothing{fact}",
                        key(&mismatch.query),
                        mismatch.incremental,
                        mismatch.scratch
                    ),
                );
            }
        }
        Response::Snapshotted { heads } => {
            let word = paint(GREEN, "snapshotted", color);
            line(out, format!("{word} {} heads durable", heads.len()));
            for (head, root) in heads {
                line(out, format!("  {head} {root}"));
            }
        }
    }
}

fn diagnostic_lines(out: &mut String, diagnostic: &Diagnostic, depth: usize, color: bool) {
    let (word, color_of) = match diagnostic.level {
        Level::Error => ("error", RED),
        Level::Warning => ("warning", YELLOW),
        Level::Note => ("note", GREEN),
        Level::Help => ("help", CYAN),
    };
    let code = diagnostic.code.as_deref().map(|code| format!("[{code}]")).unwrap_or_default();
    if depth == 0 {
        let word = paint(color_of, &format!("{word}{code}"), color);
        line(out, format!("{word}: {}", diagnostic.message));
    } else {
        line(out, format!("   = {}: {}", paint(color_of, word, color), diagnostic.message));
    }
    for span in &diagnostic.spans {
        let label = span.label.as_deref().map(|label| format!(": {label}")).unwrap_or_default();
        let arrow = paint(CYAN, "-->", color);
        line(out, format!("  {arrow} {}:{}:{}{label}", span.file, span.line, span.column));
    }
    for child in &diagnostic.children {
        diagnostic_lines(out, child, depth + 1, color);
    }
    for fact in &diagnostic.facts {
        line(out, format!("   = {} {}", paint(DIM, "read:", color), fact.what));
    }
}

fn test_lines(out: &mut String, result: &TestResult, color: bool) {
    let verdict = match result.outcome {
        TestOutcome::Passed => paint(GREEN, "ok", color),
        TestOutcome::Failed => paint(RED, "FAILED", color),
        TestOutcome::Ignored => paint(YELLOW, "ignored", color),
        TestOutcome::Unreached => paint(DIM, "unreached", color),
    };
    line(out, format!("test {} ... {verdict} ({} ms)", result.name, result.millis));
    if result.outcome == TestOutcome::Failed {
        bytes(out, &result.output);
    }
}

fn ranked_line(out: &mut String, place: usize, ranked: &Ranked, color: bool) {
    let verdict = if ranked.ok { paint(GREEN, "ok", color) } else { paint(RED, "failed", color) };
    line(
        out,
        format!(
            "{}. candidate {} ({}): {verdict}, {} errors, {} warnings, {} passed, {} failed",
            place + 1,
            ranked.candidate,
            ranked.root,
            ranked.errors,
            ranked.warnings,
            ranked.passed,
            ranked.failed
        ),
    );
}

fn key(key: &QueryKey) -> String {
    format!("query {:#06x}:{}", key.kind.0, key.input)
}

/// Output of user code: lossily as text, its last [`OUTPUT`] bytes, each line ended.
fn bytes(out: &mut String, bytes: &[u8]) {
    let start = bytes.len().saturating_sub(OUTPUT);
    let text = String::from_utf8_lossy(&bytes[start..]);
    if start > 0 {
        line(out, format!("({start} bytes cut)"));
    }
    for said in text.lines() {
        line(out, said.to_owned());
    }
}

fn line(out: &mut String, text: String) {
    out.push_str(&text);
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use blaze_api::{ApiError, FactRead, Span};
    use blaze_ids::ContentId;
    use blaze_query::QueryKind;

    use super::*;

    fn fact(what: &str) -> FactRead {
        let key = QueryKey { kind: QueryKind(0x1000), input: ContentId([1; 32]) };
        FactRead { key, what: what.to_owned() }
    }

    #[test]
    fn a_diagnostic_reads_as_rustc_writes_it_with_its_facts() {
        let diagnostic = Diagnostic {
            level: Level::Error,
            code: Some("E0308".to_owned()),
            message: "mismatched types".to_owned(),
            spans: vec![Span {
                file: "src/main.rs".to_owned(),
                line: 3,
                column: 17,
                end_line: 3,
                end_column: 19,
                label: Some("expected `u32`, found `&str`".to_owned()),
            }],
            children: vec![Diagnostic {
                level: Level::Note,
                code: None,
                message: "expected due to this".to_owned(),
                spans: Vec::new(),
                children: Vec::new(),
                facts: Vec::new(),
            }],
            facts: vec![fact("the signature of `main::parse`")],
        };
        assert_eq!(
            lines(&Response::Diagnostic(diagnostic), false),
            "error[E0308]: mismatched types\n\
             \x20 --> src/main.rs:3:17: expected `u32`, found `&str`\n\
             \x20  = note: expected due to this\n\
             \x20  = read: the signature of `main::parse`\n"
        );
    }

    #[test]
    fn a_failed_test_shows_its_output_and_a_passed_one_does_not() {
        let mut result = TestResult {
            binary: "t".to_owned(),
            name: "parses".to_owned(),
            outcome: TestOutcome::Passed,
            output: b"noise\n".to_vec(),
            millis: 4,
        };
        let passed = lines(&Response::TestResult(result.clone()), false);
        assert_eq!(passed, "test parses ... ok (4 ms)\n");
        result.outcome = TestOutcome::Failed;
        assert_eq!(
            lines(&Response::TestResult(result), false),
            "test parses ... FAILED (4 ms)\nnoise\n"
        );
    }

    #[test]
    fn a_candidates_lines_are_marked_with_it() {
        let inner = Response::Error(ApiError::new("no such file", Some("src/a.rs".to_owned())));
        let of = Response::Of { candidate: 2, response: Box::new(inner) };
        assert_eq!(lines(&of, false), "[2] error: no such file (at src/a.rs)\n");
    }

    #[test]
    fn a_successful_end_says_nothing() {
        assert_eq!(lines(&Response::Done { ok: true }, false), "");
        assert_eq!(lines(&Response::Done { ok: false }, false), "failed\n");
    }

    #[test]
    fn long_output_keeps_its_tail() {
        let output = vec![b'x'; OUTPUT + 10];
        let ran = Response::Ran { code: Some(0), stdout: output, stderr: Vec::new() };
        assert!(lines(&ran, false).starts_with("(10 bytes cut)\n"));
    }
}

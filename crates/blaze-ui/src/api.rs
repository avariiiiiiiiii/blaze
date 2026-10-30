//! The agent API's responses ([`blaze_api::Response`]) as text for people: what a client of
//! the API (the CLI, a CI log) prints. Diagnostics as rustc writes them, with the facts they
//! read; tests as libtest writes them. JSON for programs is [`blaze_api::json`]'s, but for the
//! forms lane proof's harness reads: [`work_json`], [`codegen_inputs_json`] and [`trace_cbor`].
//!
//! Every line is written into the one `String` it ends in, with no `String` of its own.

use std::fmt;

use blaze_api::{
    Diagnostic, FetchVerb, Level, LinkKind, Ranked, Response, TestOutcome, TestResult, Traced,
    Verb, Worked,
};
use blaze_ids::ContentId;
use blaze_query::QueryKey;
use blaze_v2_proof::TraceBuilder;
use blaze_vfs::ChangeKind;

use crate::{CYAN, DIM, GREEN, Painted, RED, YELLOW};

/// The most bytes of a test's or a run's output shown.
const OUTPUT: usize = 64 << 10;

/// The text of one response, each line ending in a newline; empty for one that says nothing
/// to a person (a request's successful end).
pub fn lines(response: &Response, color: bool) -> String {
    let mut out = String::new();
    lines_into(&mut out, response, color);
    out
}

/// [`lines`], written at the end of `out`.
pub fn lines_into(out: &mut String, response: &Response, color: bool) {
    let paint = |code, text| Painted(code, text, color);
    // A status word as cargo writes it: right-aligned in 12 columns.
    let status = |word: &'static str| Painted(GREEN, Status(word), color);
    match response {
        Response::Committed { root, rebased } => {
            let rebased = if *rebased { " (rebased onto the head)" } else { "" };
            put!(out, "{} {root}{rebased}\n", paint(GREEN, "committed"));
        }
        Response::Diagnostic(diagnostic) => diagnostic_lines(out, diagnostic, 0, color),
        Response::Built { artifacts, profile, .. } => {
            for it in artifacts {
                let (package, kind, path, content) = (&it.package, &it.kind, &it.path, &it.content);
                put!(out, "{} {package} {kind} at {path} ({content})\n", status("Built"));
            }
            put!(out, "{} {profile}\n", status("Finished"));
        }
        Response::Unit(unit) => {
            let verb = match unit.verb {
                Verb::Compiling => "Compiling",
                Verb::Checking => "Checking",
                Verb::Fresh => "Fresh",
            };
            put!(out, "{} {} v{}", status(verb), unit.name, unit.version);
            if let Some(source) = &unit.source {
                put!(out, " {source}");
            }
            out.push('\n');
        }
        Response::Fetched { verb, what } => {
            let verb = match verb {
                FetchVerb::Updating => "Updating",
                FetchVerb::Downloaded => "Downloaded",
            };
            put!(out, "{} {what}\n", status(verb));
        }
        Response::BuildScript(_) | Response::UnitEnded(_) => {}
        Response::Work(worked) => {
            let link = match worked.link {
                LinkKind::Patched => "patched in place",
                LinkKind::Placed => "placed whole",
            };
            put!(
                out,
                "{} {} queries ran, {} green; {} functions compiled, {} found; {link}; \
                 critical path {} us\n",
                paint(DIM, "work:"),
                worked.reran,
                worked.green,
                worked.compiled,
                worked.found,
                worked.critical_path_us
            );
        }
        Response::CodegenInputs(inputs) => {
            for (instance, code) in inputs {
                put!(out, "{instance:x} {code:x}\n");
            }
        }
        Response::Trace(traced) => {
            for result in traced {
                let ran = if result.ran { "ran" } else { "kept" };
                let (edges, fingerprint) = (result.edges.len(), result.fingerprint);
                put!(out, "{} {ran}, {edges} edges, {fingerprint:?}\n", Key(&result.key));
            }
        }
        Response::Tables(tables) => {
            for table in tables {
                let (name, elements, bytes) = (&table.name, table.elements, table.bytes);
                put!(out, "{name}: {elements} elements, {bytes} bytes\n");
            }
        }
        Response::TestResult(result) => test_lines(out, result, color),
        Response::Ran { code, stdout, stderr } => {
            bytes(out, stdout);
            bytes(out, stderr);
            match code {
                Some(0) => {}
                Some(code) => put!(out, "{} exited with {code}\n", paint(RED, "error")),
                None => put!(out, "{} ended by a signal\n", paint(RED, "error")),
            }
        }
        Response::Diff { changes } => {
            for change in changes {
                let mark = match change.kind {
                    ChangeKind::Added => paint(GREEN, "A"),
                    ChangeKind::Removed => paint(RED, "D"),
                    ChangeKind::Modified => paint(YELLOW, "M"),
                };
                put!(out, "{mark} {}\n", change.path);
            }
        }
        Response::Why(why) => {
            put!(out, "{} reran because:\n", Key(&why.query));
            for changed in &why.chain {
                let (was, now) = (&changed.was, &changed.now);
                put!(out, "  {} changed ({was:?} -> {now:?})\n", Key(&changed.key));
            }
            if let Some(input) = &why.input {
                put!(out, "  from the input {input:?}\n");
            }
        }
        Response::Status(status) => {
            let requests = status.requests.len();
            put!(
                out,
                "{requests} {} under way, {} {} open, {} of {} bytes of memory\n",
                if requests == 1 { "request" } else { "requests" },
                status.views,
                if status.views == 1 { "view" } else { "views" },
                status.memory_bytes,
                status.budget_bytes
            );
        }
        Response::Done { ok: true } => {}
        Response::Done { ok: false } => put!(out, "{}\n", paint(RED, "failed")),
        Response::Error(error) => {
            put!(out, "{}: {}", paint(RED, "error"), error.what);
            if let Some(at) = &error.at {
                put!(out, " (at {at})");
            }
            out.push('\n');
        }
        Response::Opened { root } => put!(out, "{} {root}\n", paint(GREEN, "opened")),
        Response::Reached(reach) => {
            put!(
                out,
                "{} files changed; {} items, {} functions to compile, {} tests reached\n",
                reach.changes.len(),
                reach.items.len(),
                reach.compiled.len(),
                reach.tests.len()
            );
            for item in &reach.items {
                put!(out, "  {}\n", item.path);
                for fact in &item.facts {
                    put!(out, "    {} {}\n", paint(DIM, "changed:"), fact.what);
                }
            }
            for function in &reach.compiled {
                put!(out, "  {} {function}\n", paint(CYAN, "compile"));
            }
            for test in &reach.tests {
                put!(out, "  {} {test}\n", paint(CYAN, "test"));
            }
        }
        Response::Of { candidate, response } => {
            let from = out.len();
            lines_into(out, response, color);
            let inner = out.split_off(from);
            for said in inner.lines() {
                put!(out, "[{candidate}] {said}\n");
            }
        }
        Response::Ranked(ranked) => {
            for (place, it) in ranked.iter().enumerate() {
                ranked_line(out, place, it, color);
            }
        }
        Response::Verified { mismatches } if mismatches.is_empty() => {
            put!(out, "{} every result matched a build from nothing\n", paint(GREEN, "verified"));
        }
        Response::Verified { mismatches } => {
            let (error, count) = (paint(RED, "error"), mismatches.len());
            put!(out, "{error}: {count} results differ from a build from nothing\n");
            for mismatch in mismatches {
                let (kept, scratch) = (&mismatch.incremental, &mismatch.scratch);
                put!(out, "  {}: {kept:?} kept, {scratch:?} from nothing", Key(&mismatch.query));
                if let Some(fact) = &mismatch.fact {
                    put!(out, ", the fact that missed: {}", fact.what);
                }
                out.push('\n');
            }
        }
        Response::Snapshotted { heads } => {
            put!(out, "{} {} heads durable\n", paint(GREEN, "snapshotted"), heads.len());
            for (head, root) in heads {
                put!(out, "  {head} {root}\n");
            }
        }
    }
}

/// A word right-aligned in 12 columns, as cargo's status lines start.
struct Status(&'static str);

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:>12}", self.0)
    }
}

/// rustc's own text when the diagnostic carries it, else made from its parts; then the facts.
fn diagnostic_lines(out: &mut String, diagnostic: &Diagnostic, depth: usize, color: bool) {
    if depth == 0 && !diagnostic.rendered.is_empty() {
        out.push_str(&diagnostic.rendered);
        if !diagnostic.rendered.ends_with('\n') {
            out.push('\n');
        }
        facts_lines(out, diagnostic, color);
        return;
    }
    let (word, color_of) = match diagnostic.level {
        Level::Error => ("error", RED),
        Level::Warning => ("warning", YELLOW),
        Level::Note => ("note", GREEN),
        Level::Help => ("help", CYAN),
        Level::FailureNote => {
            out.push_str(&diagnostic.message);
            out.push('\n');
            return;
        }
    };
    let message = &diagnostic.message;
    match (depth, &diagnostic.code) {
        (0, Some(code)) => {
            let word = Painted(color_of, format_args!("{word}[{code}]"), color);
            put!(out, "{word}: {message}\n");
        }
        (0, None) => put!(out, "{}: {message}\n", Painted(color_of, word, color)),
        _ => put!(out, "   = {}: {message}\n", Painted(color_of, word, color)),
    }
    for span in &diagnostic.spans {
        let arrow = Painted(CYAN, "-->", color);
        put!(out, "  {arrow} {}:{}:{}", span.file, span.line, span.column);
        if let Some(label) = &span.label {
            put!(out, ": {label}");
        }
        out.push('\n');
    }
    for child in &diagnostic.children {
        diagnostic_lines(out, child, depth + 1, color);
    }
    facts_lines(out, diagnostic, color);
}

fn facts_lines(out: &mut String, diagnostic: &Diagnostic, color: bool) {
    for fact in &diagnostic.facts {
        put!(out, "   = {} {}\n", Painted(DIM, "read:", color), fact.what);
    }
}

fn test_lines(out: &mut String, result: &TestResult, color: bool) {
    let outcome = result.cached.as_ref().map_or(result.outcome, |cached| cached.outcome);
    let (code, verdict) = match outcome {
        TestOutcome::Passed => (GREEN, "ok"),
        TestOutcome::Failed => (RED, "FAILED"),
        TestOutcome::Ignored => (YELLOW, "ignored"),
        TestOutcome::Unreached => (DIM, "unreached"),
    };
    let verdict = Painted(code, verdict, color);
    match &result.cached {
        None => {
            put!(out, "test {} ... {verdict} ({} ms)\n", result.name, result.millis);
        }
        Some(cached) => {
            put!(out, "test {} ... {verdict} (cached, {} ms)\n", result.name, cached.millis);
        }
    }
    if result.outcome == TestOutcome::Failed {
        bytes(out, &result.output);
    }
}

fn ranked_line(out: &mut String, place: usize, ranked: &Ranked, color: bool) {
    let verdict = match ranked.ok {
        true => Painted(GREEN, "ok", color),
        false => Painted(RED, "failed", color),
    };
    put!(
        out,
        "{}. candidate {} ({}): {verdict}, {} errors, {} warnings, {} passed, {} failed\n",
        place + 1,
        ranked.candidate,
        ranked.root,
        ranked.errors,
        ranked.warnings,
        ranked.passed,
        ranked.failed
    );
}

/// A query as the lines name it: `query 0x1000:<input>`.
struct Key<'a>(&'a QueryKey);

impl fmt::Display for Key<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "query {:#06x}:{}", self.0.kind.0, self.0.input)
    }
}

/// What a request did, as the CLI prints it under `--message-format=json`: one line, newline
/// ended, in cargo's `reason` style.
pub fn work_json(worked: &Worked) -> String {
    let link = match worked.link {
        LinkKind::Patched => "patched",
        LinkKind::Placed => "placed",
    };
    format!(
        "{{\"reason\": \"blaze-work\", \"reran\": {}, \"green\": {}, \"compiled\": {}, \
         \"found\": {}, \"link\": \"{link}\", \"critical_path_us\": {}}}\n",
        worked.reran, worked.green, worked.compiled, worked.found, worked.critical_path_us
    )
}

/// `blaze codegen-inputs --format=json`: one object from each instance key's hex to its code
/// hash's hex, in the response's order (sorted by key), newline ended.
pub fn codegen_inputs_json(inputs: &[(ContentId, ContentId)]) -> String {
    // `"<64 hex>": "<64 hex>", ` is 136 bytes a pair.
    let mut out = String::with_capacity(3 + inputs.len() * 136);
    out.push('{');
    for (at, (instance, code)) in inputs.iter().enumerate() {
        let comma = if at > 0 { ", " } else { "" };
        put!(&mut out, "{comma}\"{instance:x}\": \"{code:x}\"");
    }
    out.push_str("}\n");
    out
}

/// `blaze trace --out PATH`: the deterministic CBOR of the [`blaze_v2_proof::Trace`] the
/// results make, what the `blaze-v2-proof` binary compares.
pub fn trace_cbor(traced: &[Traced]) -> Result<Vec<u8>, blaze_protocol::cbor::Error> {
    let mut trace = TraceBuilder::default();
    for result in traced {
        trace.insert(result.key, result.fingerprint, &result.edges, result.ran);
    }
    blaze_protocol::cbor::to_vec(&trace.finish())
}

/// Output of user code: lossily as text, its last [`OUTPUT`] bytes, each line ended.
fn bytes(out: &mut String, bytes: &[u8]) {
    let start = bytes.len().saturating_sub(OUTPUT);
    let text = String::from_utf8_lossy(&bytes[start..]);
    if start > 0 {
        put!(out, "({start} bytes cut)\n");
    }
    for said in text.lines() {
        out.push_str(said);
        out.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use blaze_api::{ApiError, Basis, Cached, FactRead, Span};
    use blaze_ids::ContentId;
    use blaze_query::QueryKind;

    use super::*;

    fn fact(what: &str) -> FactRead {
        let key = QueryKey { kind: QueryKind(0x1000), input: ContentId([1; 32]) };
        FactRead { key, what: what.to_owned() }
    }

    fn diagnostic() -> Diagnostic {
        Diagnostic {
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
                byte_start: 40,
                byte_end: 42,
                primary: true,
                replacement: None,
                applicability: None,
            }],
            children: vec![Diagnostic {
                level: Level::Note,
                code: None,
                message: "expected due to this".to_owned(),
                spans: Vec::new(),
                children: Vec::new(),
                facts: Vec::new(),
                rendered: String::new(),
                unit: None,
            }],
            facts: vec![fact("the signature of `main::parse`")],
            rendered: String::new(),
            unit: None,
        }
    }

    #[test]
    fn a_diagnostic_reads_as_rustc_writes_it_with_its_facts() {
        assert_eq!(
            lines(&Response::Diagnostic(diagnostic()), false),
            "error[E0308]: mismatched types\n\
             \x20 --> src/main.rs:3:17: expected `u32`, found `&str`\n\
             \x20  = note: expected due to this\n\
             \x20  = read: the signature of `main::parse`\n"
        );
    }

    /// In color, each word is painted as `paint` painted it: the code, the word, the reset.
    #[test]
    fn a_diagnostic_in_color_paints_its_words() {
        let (red, green, cyan, dim) = (RED, GREEN, CYAN, DIM);
        let reset = crate::RESET;
        assert_eq!(
            lines(&Response::Diagnostic(diagnostic()), true),
            format!(
                "{red}error[E0308]{reset}: mismatched types\n\
                 \x20 {cyan}-->{reset} src/main.rs:3:17: expected `u32`, found `&str`\n\
                 \x20  = {green}note{reset}: expected due to this\n\
                 \x20  = {dim}read:{reset} the signature of `main::parse`\n"
            )
        );
        let failed = Response::Done { ok: false };
        assert_eq!(lines(&failed, true), format!("{red}failed{reset}\n"));
    }

    #[test]
    fn work_is_one_json_line_in_the_form_proof_reads() {
        let worked = Worked {
            reran: 12,
            green: 300,
            compiled: 2,
            found: 40,
            link: LinkKind::Patched,
            critical_path_us: 1500,
        };
        let line = work_json(&worked);
        assert_eq!(
            line,
            "{\"reason\": \"blaze-work\", \"reran\": 12, \"green\": 300, \"compiled\": 2, \
             \"found\": 40, \"link\": \"patched\", \"critical_path_us\": 1500}\n"
        );
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["link"], "patched");
    }

    #[test]
    fn codegen_inputs_are_one_object_of_hex() {
        let inputs =
            [(ContentId([1; 32]), ContentId([2; 32])), (ContentId([3; 32]), ContentId([4; 32]))];
        let text = codegen_inputs_json(&inputs);
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value[format!("{:x}", ContentId([3; 32]))], format!("{:x}", ContentId([4; 32])));
        assert_eq!(value.as_object().unwrap().len(), 2);
        assert_eq!(codegen_inputs_json(&[]), "{}\n");
        let pairs: Vec<String> = inputs
            .iter()
            .map(|(instance, code)| format!("\"{instance:x}\": \"{code:x}\""))
            .collect();
        assert_eq!(text, format!("{{{}}}\n", pairs.join(", ")));
    }

    #[test]
    fn a_trace_is_written_as_proofs_trace() {
        let key = |n: u8| QueryKey { kind: QueryKind(0x2000), input: ContentId([n; 32]) };
        let traced = [
            Traced {
                key: key(2),
                fingerprint: blaze_ids::Fingerprint(7),
                edges: vec![blaze_query::Edge { read: key(1), saw: blaze_ids::Fingerprint(5) }],
                ran: true,
            },
            Traced {
                key: key(1),
                fingerprint: blaze_ids::Fingerprint(5),
                edges: Vec::new(),
                ran: false,
            },
        ];
        let bytes = trace_cbor(&traced).unwrap();
        blaze_protocol::cbor::check(&bytes).unwrap();
        let trace: blaze_v2_proof::Trace = blaze_protocol::cbor::from_slice(&bytes).unwrap();
        assert_eq!(trace.keys, [key(1), key(2)]);
        assert!(trace.ran.get(1) && !trace.ran.get(0));
        assert_eq!(trace.reads, [0]);
        assert_eq!(trace.edges_from, [0, 0, 1]);
    }

    #[test]
    fn rustcs_own_text_is_printed_as_it_is_then_the_facts() {
        let diagnostic = Diagnostic {
            level: Level::Warning,
            code: None,
            message: "unused variable: `x`".to_owned(),
            spans: Vec::new(),
            children: Vec::new(),
            facts: vec![fact("what `x` in `main` resolves to")],
            rendered: "warning: unused variable: `x`\n --> src/main.rs:2:9\n".to_owned(),
            unit: None,
        };
        assert_eq!(
            lines(&Response::Diagnostic(diagnostic), false),
            "warning: unused variable: `x`\n --> src/main.rs:2:9\n\
             \x20  = read: what `x` in `main` resolves to\n"
        );
    }

    #[test]
    fn a_failed_test_shows_its_output_and_a_passed_one_does_not() {
        let mut result = TestResult {
            binary: "t".to_owned(),
            name: "parses".to_owned(),
            cached: None,
            outcome: TestOutcome::Passed,
            output: b"noise\n".to_vec(),
            millis: 4,
        };
        let passed = lines(&Response::TestResult(result.clone()), false);
        assert_eq!(passed, "test parses ... ok (4 ms)\n");
        result.outcome = TestOutcome::Failed;
        assert_eq!(
            lines(&Response::TestResult(result.clone()), false),
            "test parses ... FAILED (4 ms)\nnoise\n"
        );
        let cached = Cached { basis: Basis::Executed, millis: 9, outcome: TestOutcome::Passed };
        let kept = TestResult {
            cached: Some(cached),
            millis: 0,
            output: Vec::new(),
            outcome: TestOutcome::Unreached,
            ..result
        };
        assert_eq!(lines(&Response::TestResult(kept), false), "test parses ... ok (cached, 9 ms)\n");
    }

    #[test]
    fn a_candidates_lines_are_marked_with_it() {
        let inner = Response::Error(ApiError::new("no such file", Some("src/a.rs".to_owned())));
        let of = Response::Of { candidate: 2, response: Box::new(inner) };
        assert_eq!(lines(&of, false), "[2] error: no such file (at src/a.rs)\n");
        // After what `out` had, which stays as it was.
        let mut out = "before\n".to_owned();
        lines_into(&mut out, &of, false);
        assert_eq!(out, "before\n[2] error: no such file (at src/a.rs)\n");
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

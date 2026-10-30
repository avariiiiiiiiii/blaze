use std::time::Duration;

use blaze_api::{
    ApiError, Artifact, Diagnostic, Level, Response, Span, UnitOf, UnitProfile, UnitStarted, Verb,
};
use blaze_ids::ContentId;

use super::*;
use crate::cli::say::tests::target;

#[test]
fn message_formats_as_cargo_reads_them() {
    let format = |values: &[&str]| super::format(&values.iter().map(|it| it.to_string()).collect::<Vec<_>>());
    assert_eq!(format(&[]), Ok(Format::Human));
    assert_eq!(format(&["short"]), Ok(Format::Short));
    assert_eq!(format(&["JSON"]), Ok(Format::Json { short: false, ansi: false, render_diagnostics: false }));
    assert_eq!(
        format(&["json-diagnostic-short,json-render-diagnostics"]),
        Ok(Format::Json { short: true, ansi: false, render_diagnostics: true })
    );
    assert_eq!(
        format(&["json", "json-diagnostic-rendered-ansi"]),
        Ok(Format::Json { short: false, ansi: true, render_diagnostics: false })
    );
    let two = Err("cannot specify two kinds of `message-format` arguments".to_owned());
    assert_eq!(format(&["json,human"]), two);
    assert_eq!(format(&["short", "json-render-diagnostics"]), two);
    assert_eq!(format(&["xml"]), Err("invalid message format specifier: `xml`".to_owned()));
}

fn started(name: &str, verb: Verb) -> Response {
    Response::Unit(UnitStarted {
        package_id: format!("path+file:///w/{name}#0.1.0"),
        name: name.to_owned(),
        version: "0.1.0".to_owned(),
        source: Some(format!("(/w/{name})")),
        verb,
    })
}

fn lib_of(name: &str) -> UnitOf {
    UnitOf {
        package_id: format!("path+file:///w/{name}#0.1.0"),
        manifest_path: format!("/w/{name}/Cargo.toml"),
        target: target("lib", name),
        test: false,
    }
}

fn diagnostic(level: Level, message: &str, rendered: &str, unit: Option<UnitOf>) -> Diagnostic {
    Diagnostic {
        level,
        code: None,
        message: message.to_owned(),
        spans: Vec::new(),
        children: Vec::new(),
        facts: Vec::new(),
        rendered: rendered.to_owned(),
        unit,
    }
}

fn fixable(mut warning: Diagnostic) -> Diagnostic {
    let span = Span {
        file: "src/lib.rs".to_owned(),
        line: 1,
        column: 5,
        end_line: 1,
        end_column: 6,
        label: None,
        byte_start: 4,
        byte_end: 5,
        primary: true,
        replacement: Some("_x".to_owned()),
        applicability: Some("MachineApplicable".to_owned()),
    };
    let mut help = diagnostic(Level::Help, "prefix it with an underscore", "", None);
    help.spans.push(span);
    warning.children.push(help);
    warning
}

fn built() -> Response {
    let artifact = Artifact {
        package: "path+file:///w/foo#0.1.0".to_owned(),
        kind: "lib".to_owned(),
        path: "/w/foo/target/debug/libfoo.rlib".to_owned(),
        content: ContentId([0; 32]),
        manifest_path: "/w/foo/Cargo.toml".to_owned(),
        target: target("lib", "foo"),
        profile: UnitProfile {
            opt_level: "0".to_owned(),
            debuginfo: 2,
            debug_assertions: true,
            overflow_checks: true,
            test: false,
        },
        features: Vec::new(),
        filenames: vec!["/w/foo/target/debug/libfoo.rlib".to_owned()],
        executable: None,
        fresh: false,
        runner: Vec::new(),
        env: Vec::new(),
        harness: true,
    };
    Response::Built { members: 1, artifacts: vec![artifact], profile: "`dev` profile [unoptimized + debuginfo]".to_owned() }
}

/// What the printer wrote to stdout and stderr of `responses`, ended with `ok`.
fn printed(shown: Shown, responses: &[Response], ok: bool) -> (String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let mut printer = Printer::new(shown, Path::new("/w"), &mut out, &mut err);
    for response in responses {
        printer.on(response.clone()).unwrap();
    }
    printer.finish(ok, Duration::from_millis(1234)).unwrap();
    drop(printer);
    (String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
}

const UNUSED: &str = "warning: unused variable: `x`\n --> src/lib.rs:1:5\n\n";

#[test]
fn a_build_with_warnings_as_cargo_prints_it() {
    let warning = fixable(diagnostic(Level::Warning, "unused variable: `x`", UNUSED, Some(lib_of("foo"))));
    let emitted = diagnostic(Level::Warning, "2 warnings emitted", "warning: 2 warnings emitted\n\n", Some(lib_of("foo")));
    let responses = [
        started("bar", Verb::Fresh),
        started("foo", Verb::Compiling),
        started("foo", Verb::Compiling),
        Response::Diagnostic(warning.clone()),
        Response::Diagnostic(warning),
        Response::Diagnostic(emitted),
        built(),
        Response::Done { ok: true },
    ];
    let (out, err) = printed(Shown::default(), &responses, true);
    assert_eq!(out, "");
    assert_eq!(
        err,
        format!(
            "   Compiling foo v0.1.0 (/w/foo)\n{UNUSED}warning: `foo` (lib) generated 2 warnings (1 duplicate) \
             (run `cargo fix --lib -p foo` to apply 1 suggestion)\n    Finished `dev` profile \
             [unoptimized + debuginfo] target(s) in 1.23s\n"
        )
    );

    let (_, verbose) = printed(Shown { verbose: 1, ..Shown::default() }, &responses, true);
    assert!(verbose.starts_with("       Fresh bar v0.1.0 (/w/bar)\n   Compiling foo v0.1.0 (/w/foo)\n"), "{verbose}");

    let (_, quiet) = printed(Shown { quiet: true, ..Shown::default() }, &responses, true);
    assert_eq!(quiet, UNUSED);
}

#[test]
fn a_units_summary_comes_as_it_ends_and_once() {
    let warning = diagnostic(Level::Warning, "unused variable: `x`", UNUSED, Some(lib_of("foo")));
    let responses = [
        started("foo", Verb::Compiling),
        Response::Diagnostic(warning),
        Response::UnitEnded(lib_of("foo")),
        started("bar", Verb::Compiling),
        Response::UnitEnded(lib_of("bar")),
        built(),
        Response::Done { ok: true },
    ];
    let (_, err) = printed(Shown::default(), &responses, true);
    assert_eq!(
        err,
        format!(
            "   Compiling foo v0.1.0 (/w/foo)\n{UNUSED}warning: `foo` (lib) generated 1 warning\n   Compiling bar \
             v0.1.0 (/w/bar)\n    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.23s\n"
        )
    );
}

#[test]
fn what_a_request_did_is_one_json_line_of_blazes() {
    let worked = blaze_api::Worked {
        link: blaze_api::LinkKind::Patched,
        found: 3,
        green: 40,
        reran: 2,
        compiled: 1,
        critical_path_us: 1500,
    };
    let json = Shown { format: Format::Json { short: false, ansi: false, render_diagnostics: false }, ..Shown::default() };
    let (out, _) = printed(json, &[Response::Work(worked), Response::Done { ok: true }], true);
    assert_eq!(
        out,
        "{\"reason\": \"blaze-work\", \"reran\": 2, \"green\": 40, \"compiled\": 1, \"found\": 3, \"link\": \
         \"patched\", \"critical_path_us\": 1500}\n{\"reason\":\"build-finished\",\"success\":true}\n"
    );
    let (out, _) = printed(Shown::default(), &[Response::Work(worked), Response::Done { ok: true }], true);
    assert_eq!(out, "", "under the human format it is not printed");
}

#[test]
fn a_unit_that_failed() {
    let error = diagnostic(Level::Error, "mismatched types", "error[E0308]: mismatched types\n\n", Some(lib_of("foo")));
    let warning = fixable(diagnostic(Level::Warning, "unused variable: `x`", UNUSED, Some(lib_of("foo"))));
    let aborting = diagnostic(Level::Error, "aborting due to 1 previous error", "error: aborting due to 1 previous error\n\n", Some(lib_of("foo")));
    let explain = diagnostic(
        Level::FailureNote,
        "For more information about this error, try `rustc --explain E0308`.",
        "For more information about this error, try `rustc --explain E0308`.\n",
        Some(lib_of("foo")),
    );
    let responses = [
        started("foo", Verb::Compiling),
        Response::Diagnostic(warning),
        Response::Diagnostic(error),
        Response::Diagnostic(aborting),
        Response::Diagnostic(explain),
        Response::Done { ok: false },
    ];
    let (_, err) = printed(Shown { color: false, ..Shown::default() }, &responses, false);
    assert_eq!(
        err,
        format!(
            "   Compiling foo v0.1.0 (/w/foo)\n{UNUSED}error[E0308]: mismatched types\n\nFor more information \
             about this error, try `rustc --explain E0308`.\nwarning: `foo` (lib) generated 1 warning\nerror: \
             could not compile `foo` (lib) due to 1 previous error; 1 warning emitted\n"
        )
    );
    let (_, quiet) = printed(Shown { color: false, quiet: true, ..Shown::default() }, &responses, false);
    assert!(quiet.ends_with("`rustc --explain E0308`.\nerror: could not compile `foo` (lib) due to 1 previous error; 1 warning emitted\n"), "{quiet}");
}

#[test]
fn levels_in_color_as_cargos_shell_writes_them() {
    let error = diagnostic(Level::Error, "mismatched types", "error[E0308]: mismatched types\n", Some(lib_of("foo")));
    let warning = diagnostic(Level::Warning, "unused variable: `x`", UNUSED, Some(lib_of("foo")));
    let responses = [Response::Diagnostic(warning), Response::Diagnostic(error), Response::Done { ok: false }];
    let (_, err) = printed(Shown { color: true, ..Shown::default() }, &responses, false);
    assert!(
        err.ends_with(&format!(
            "{}warning{}: `foo` (lib) generated 1 warning\n{}error{}: could not compile `foo` (lib) due to 1 \
             previous error; 1 warning emitted\n",
            say::WARN,
            say::RESET,
            say::ERROR,
            say::RESET
        )),
        "{err:?}"
    );
}

#[test]
fn rustcs_summary_is_skipped_under_json_too() {
    let json = Shown { format: Format::Json { short: false, ansi: false, render_diagnostics: false }, ..Shown::default() };
    let emitted = diagnostic(Level::Warning, "1 warning emitted", "warning: 1 warning emitted\n\n", Some(lib_of("foo")));
    let (out, _) = printed(json, &[Response::Diagnostic(emitted), Response::Done { ok: true }], true);
    assert_eq!(out, "{\"reason\":\"build-finished\",\"success\":true}\n");
}

#[test]
fn one_rendered_text_is_printed_once_with_or_without_its_newline() {
    let first = diagnostic(Level::Warning, "unused variable: `x`", "warning: a\n", Some(lib_of("foo")));
    let second = diagnostic(Level::Warning, "unused variable: `x`", "warning: a", Some(lib_of("foo")));
    let responses = [Response::Diagnostic(first), Response::Diagnostic(second), Response::Done { ok: true }];
    let (_, err) = printed(Shown::default(), &responses, true);
    assert_eq!(err, "warning: a\nwarning: `foo` (lib) generated 2 warnings (1 duplicate)\n");
}

#[test]
fn units_of_one_package_are_told_apart() {
    let lib = diagnostic(Level::Warning, "a", "warning: a\n", Some(lib_of("foo")));
    let test = diagnostic(Level::Warning, "a", "warning: b\n", Some(UnitOf { test: true, ..lib_of("foo") }));
    let responses = [Response::Diagnostic(lib), Response::Diagnostic(test), Response::Done { ok: true }];
    let (_, err) = printed(Shown::default(), &responses, true);
    assert_eq!(
        err,
        "warning: a\nwarning: b\nwarning: `foo` (lib) generated 1 warning\nwarning: `foo` (lib test) generated 1 warning\n"
    );
}

/// A build's responses: each unit starting, warnings, artifacts, a unit ending.
fn a_build(n: usize) -> Vec<Response> {
    let mut responses = Vec::new();
    for at in 0..n {
        let name = format!("p{at}");
        responses.push(started(&name, Verb::Compiling));
        let warning = fixable(diagnostic(Level::Warning, "unused variable: `x`", UNUSED, Some(lib_of(&name))));
        responses.push(Response::Diagnostic(warning.clone()));
        responses.push(Response::Diagnostic(warning));
        responses.push(Response::UnitEnded(lib_of(&name)));
    }
    responses.push(Response::Fetched { verb: FetchVerb::Downloaded, what: "serde v1.0.219".to_owned() });
    responses.push(Response::Error(ApiError::new("no package `nope` in the workspace", None)));
    responses
}

/// The printer allocates per request, not per response: once its buffers have grown and the
/// units are known, printing a build's responses allocates nothing, in every format.
#[test]
fn printing_responses_warm_allocates_nothing() {
    let formats = [
        Format::Human,
        Format::Short,
        Format::Json { short: false, ansi: false, render_diagnostics: false },
        Format::Json { short: false, ansi: true, render_diagnostics: true },
    ];
    for format in formats {
        for verbose in [0, 1] {
            let shown = Shown { format, verbose, color: true, ..Shown::default() };
            let (mut out, mut err) = (Vec::with_capacity(1 << 20), Vec::with_capacity(1 << 20));
            let mut printer = Printer::new(shown, Path::new("/w"), &mut out, &mut err);
            for response in a_build(64) {
                printer.on(response).unwrap();
            }
            printer.printed.clear();
            printer.named.clear();
            for seen in &mut printer.units {
                seen.counted = Counted::default();
            }
            let responses = a_build(64);
            blaze_alloc_count::assert_at_most(0, "printing a warm build's responses", || {
                for response in responses {
                    printer.on(response).unwrap();
                }
            });
        }
    }
}

#[test]
fn json_on_stdout_and_status_on_stderr() {
    let json = Shown { format: Format::Json { short: false, ansi: false, render_diagnostics: false }, ..Shown::default() };
    let warning = diagnostic(Level::Warning, "unused variable: `x`", UNUSED, Some(lib_of("foo")));
    let responses = [
        started("foo", Verb::Compiling),
        Response::Diagnostic(warning.clone()),
        built(),
        Response::Done { ok: true },
    ];
    let (out, err) = printed(json, &responses, true);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 3, "{out}");
    assert!(lines[0].starts_with(r#"{"reason":"compiler-message","package_id":"path+file:///w/foo#0.1.0","#), "{}", lines[0]);
    assert!(lines[0].contains(r#""message":{"rendered":"warning: unused variable: `x`\n --> src/lib.rs:1:5\n\n","#));
    assert!(lines[1].starts_with(r#"{"reason":"compiler-artifact","#));
    assert_eq!(lines[2], r#"{"reason":"build-finished","success":true}"#);
    // No "generated" line: cargo counts a unit's warnings where it renders them.
    assert_eq!(err, "   Compiling foo v0.1.0 (/w/foo)\n    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.23s\n");

    let rendering = Shown { format: Format::Json { short: false, ansi: false, render_diagnostics: true }, ..Shown::default() };
    let (out, err) = printed(rendering, &responses, true);
    assert!(!out.contains("compiler-message"));
    assert!(err.contains(UNUSED));
    assert!(err.contains("warning: `foo` (lib) generated 1 warning\n"));
}

#[test]
fn an_error_of_the_daemon() {
    let responses = [Response::Error(ApiError::new("no package `nope` in the workspace", None)), Response::Done { ok: false }];
    let (out, err) = printed(Shown::default(), &responses, false);
    assert_eq!(out, "");
    assert_eq!(err, "error: no package `nope` in the workspace\n");
}

#[test]
fn what_the_daemon_fetched() {
    let responses = [
        Response::Fetched { verb: FetchVerb::Updating, what: "crates.io index".to_owned() },
        Response::Fetched { verb: FetchVerb::Downloaded, what: "serde v1.0.219".to_owned() },
        Response::Done { ok: false },
    ];
    let (out, err) = printed(Shown::default(), &responses, false);
    assert_eq!(out, "");
    assert_eq!(err, "    Updating crates.io index\n  Downloaded serde v1.0.219\n");
    let (_, quiet) = printed(Shown { quiet: true, ..Shown::default() }, &responses, false);
    assert_eq!(quiet, "");
}

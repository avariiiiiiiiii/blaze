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
    Response::Built { artifacts: vec![artifact], profile: "`dev` profile [unoptimized + debuginfo]".to_owned() }
}

/// What the printer wrote to stdout and stderr of `responses`, ended with `ok`.
fn printed(shown: Shown, responses: &[Response], ok: bool) -> (String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let mut printer = Printer::new(shown, Path::new("/w"), &mut out, &mut err);
    for response in responses {
        printer.on(response).unwrap();
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
             about this error, try `rustc --explain E0308`.\nerror: could not compile `foo` (lib) due to 1 \
             previous error; 1 warning emitted\n"
        )
    );
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

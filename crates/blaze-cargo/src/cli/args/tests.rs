use std::ffi::OsString;

use super::*;

fn os(words: &[&str]) -> Vec<OsString> {
    words.iter().map(OsString::from).collect()
}

fn cargo(words: &[&str]) -> Invocation {
    match parse(os(words)).unwrap() {
        Asked::Cargo(asked) => *asked,
        other => panic!("not a build: {other:?}"),
    }
}

fn refused(words: &[&str]) -> String {
    refusal(&parse(os(words)).unwrap_err(), false)
}

#[test]
fn commands_and_their_aliases() {
    for (word, command) in [
        ("build", Command::Build),
        ("b", Command::Build),
        ("check", Command::Check),
        ("c", Command::Check),
        ("run", Command::Run),
        ("r", Command::Run),
        ("test", Command::Test),
        ("t", Command::Test),
        ("bench", Command::Bench),
    ] {
        assert_eq!(cargo(&[word]).command, Some(command), "{word}");
    }
    assert_eq!(parse(os(&[])), Ok(Asked::Help(None)));
    assert_eq!(parse(os(&["--help"])), Ok(Asked::Help(None)));
    assert_eq!(parse(os(&["help", "test"])), Ok(Asked::Help(Some(Command::Test))));
    assert_eq!(parse(os(&["build", "--release", "-h"])), Ok(Asked::Help(Some(Command::Build))));
    assert_eq!(parse(os(&["-V"])), Ok(Asked::Version));
    assert_eq!(parse(os(&["stop"])), Ok(Asked::Stop));
    assert_eq!(parse(os(&["status"])), Ok(Asked::Status));
    assert_eq!(parse(os(&["frobnicate"])), Ok(Asked::Unknown("frobnicate".to_owned())));
}

#[test]
fn flags_before_the_command_are_cargos_too() {
    let asked = cargo(&["-q", "--color", "never", "b", "--release", "-vv"]);
    assert_eq!(asked.command(), Command::Build);
    assert_eq!(asked.args, ["-q", "--color", "never", "--release", "-vv"]);
    assert!(asked.flags.quiet && asked.flags.release);
    assert_eq!(asked.flags.color.as_deref(), Some("never"));
    assert_eq!(asked.flags.verbose, 2);
    assert_eq!(asked.profile(), "release");
}

#[test]
fn values_as_clap_takes_them() {
    let asked = cargo(&["build", "-p", "a", "--package=b", "-pc", "-F", "x y,z", "--features=w", "-j8", "--bin", "--target", "aarch64-apple-darwin"]);
    assert_eq!(asked.flags.packages, ["a", "b", "c"]);
    assert_eq!(asked.flags.features, ["x", "y", "z", "w"]);
    assert_eq!(asked.flags.bins, [None::<String>]);
    assert_eq!(asked.flags.targets, ["aarch64-apple-darwin"]);
    // `-p` without a value is cargo's to answer, with the packages it knows.
    let asked = cargo(&["build", "-p", "--release"]);
    assert!(asked.flags.packages.is_empty());
    assert!(asked.flags.release);
    assert_eq!(asked.args, ["-p", "--release"]);
    let asked = cargo(&["build", "--message-format=json,short", "--profile", "dist"]);
    assert_eq!(asked.flags.message_format, ["json,short"]);
    assert_eq!(asked.profile(), "dist");
}

#[test]
fn test_names_its_filter_and_the_harness_args() {
    let asked = cargo(&["test", "parse", "--no-fail-fast", "--", "--exact", "--nocapture"]);
    assert_eq!(asked.filter.as_deref(), Some("parse"));
    assert_eq!(asked.args, ["parse", "--no-fail-fast"]);
    assert_eq!(asked.program, os(&["--exact", "--nocapture"]));
    assert_eq!(asked.test_args(), os(&["parse", "--exact", "--nocapture"]));
    assert!(asked.flags.no_fail_fast);
    assert_eq!(asked.profile(), "test");

    let bench = cargo(&["bench", "fast"]);
    assert_eq!(bench.test_args(), os(&["fast", "--bench"]));
    assert_eq!(bench.profile(), "bench");

    let reached = cargo(&["test", "--reached"]);
    assert!(reached.flags.reached);
    assert!(reached.args.is_empty());
}

#[test]
fn run_has_the_rest() {
    let asked = cargo(&["run", "--release", "a", "--verbose", "--", "b"]);
    assert_eq!(asked.args, ["--release"]);
    assert_eq!(asked.program, os(&["a", "--verbose", "b"]));
    assert_eq!(asked.flags.verbose, 0);
    let asked = cargo(&["run", "--bin", "app", "--", "-x", "--", "y"]);
    assert_eq!(asked.flags.bins, [Some("app".to_owned())]);
    assert_eq!(asked.program, os(&["-x", "--", "y"]));
}

#[test]
fn an_unknown_flag() {
    assert_eq!(
        refused(&["build", "--frobnicate"]),
        "error: unexpected argument '--frobnicate' found\n\nUsage: cargo build [OPTIONS]\n\nFor more \
         information, try '--help'.\n"
    );
    assert_eq!(
        refused(&["build", "--relese"]),
        "error: unexpected argument '--relese' found\n\n  tip: a similar argument exists: '--release'\n\n\
         Usage: cargo build [OPTIONS]\n\nFor more information, try '--help'.\n"
    );
    assert_eq!(
        refused(&["test", "a", "b"]),
        "error: unexpected argument 'b' found\n\n  tip: to pass 'b' as a value, use '-- b'\n\nUsage: cargo \
         test [OPTIONS] [TESTNAME] [-- [ARGS]...]\n\nFor more information, try '--help'.\n"
    );
    assert_eq!(
        refused(&["check", "-x"]),
        "error: unexpected argument '-x' found\n\nUsage: cargo check [OPTIONS]\n\nFor more information, try '--help'.\n"
    );
    // `--doc` and `--reached` are `test`'s alone.
    assert!(parse(os(&["build", "--reached"])).is_err());
    assert!(parse(os(&["build", "--", "x"])).is_err());
}

#[test]
fn a_flag_without_its_value() {
    assert_eq!(
        refused(&["build", "--jobs"]),
        "error: a value is required for '--jobs <N>' but none was supplied\n\nFor more information, try '--help'.\n"
    );
    assert_eq!(
        refused(&["build", "--features", "--release"]),
        "error: a value is required for '--features <FEATURES>' but none was supplied\n\nFor more \
         information, try '--help'.\n"
    );
    assert_eq!(
        refused(&["build", "--message-format"]),
        "error: a value is required for '--message-format <FMT>' but none was supplied\n  [possible values: \
         human, short, json, json-diagnostic-short, json-diagnostic-rendered-ansi, json-render-diagnostics]\n\n\
         For more information, try '--help'.\n"
    );
}

#[test]
fn a_value_clap_refuses() {
    assert_eq!(
        refused(&["build", "--color", "alwys"]),
        "error: invalid value 'alwys' for '--color <WHEN>'\n  [possible values: auto, always, never]\n\n  tip: \
         a similar value exists: 'always'\n\nFor more information, try '--help'.\n"
    );
    assert_eq!(
        refused(&["build", "--release=yes"]),
        "error: unexpected value 'yes' for '--release' found; no more were expected\n\nUsage: cargo build \
         [OPTIONS]\n\nFor more information, try '--help'.\n"
    );
    assert!(parse(os(&["build", "--message-format", "JSON,Short"])).is_ok());
    assert!(parse(os(&["build", "--message-format", "json,xml"])).is_err());
}

#[test]
fn refusals_in_cargos_colors() {
    let colored = refusal(&parse(os(&["build", "--frobnicate"])).unwrap_err(), true);
    assert!(
        colored.starts_with("\x1b[1m\x1b[91merror:\x1b[0m unexpected argument '\x1b[1m\x1b[93m--frobnicate\x1b[0m' found\n"),
        "{colored:?}"
    );
    assert!(colored.contains("\x1b[1m\x1b[92mUsage:\x1b[0m \x1b[1m\x1b[96mcargo build\x1b[0m \x1b[36m[OPTIONS]\x1b[0m"));
}

#[test]
fn jaro_as_strsim() {
    assert!((jaro("martha", "marhta") - 0.944_444).abs() < 1e-5);
    assert_eq!(jaro("", ""), 1.0);
    assert_eq!(jaro("a", ""), 0.0);
}

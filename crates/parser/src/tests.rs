mod prefix_entries;
mod top_entries;

use std::{
    fmt::Write,
    fs,
    path::{Path, PathBuf},
};

use expect_test::expect_file;

use crate::{Edition, LexedStr, TopEntryPoint};

#[rustfmt::skip]
#[path = "../test_data/generated/runner.rs"]
mod runner;

fn infer_edition(file_path: &Path) -> Edition {
    let file_content = std::fs::read_to_string(file_path).unwrap();
    if let Some(edition) = file_content.strip_prefix("//@ edition: ") {
        edition[..4].parse().expect("invalid edition directive")
    } else {
        Edition::CURRENT
    }
}

#[test]
fn lex_ok() {
    for case in TestCase::list("lexer/ok") {
        let _guard = stdx::panic_context::enter(format!("{:?}", case.rs));
        let actual = lex(&case.text, infer_edition(&case.rs));
        expect_file![case.rast].assert_eq(&actual)
    }
}

#[test]
fn lex_err() {
    for case in TestCase::list("lexer/err") {
        let _guard = stdx::panic_context::enter(format!("{:?}", case.rs));
        let actual = lex(&case.text, infer_edition(&case.rs));
        expect_file![case.rast].assert_eq(&actual)
    }
}

fn lex(text: &str, edition: Edition) -> String {
    let lexed = LexedStr::new(edition, text);

    let mut res = String::new();
    for i in 0..lexed.len() {
        let kind = lexed.kind(i);
        let text = lexed.text(i);
        let error = lexed.error(i);

        let error = error.map(|err| format!(" error: {err}")).unwrap_or_default();
        writeln!(res, "{kind:?} {text:?}{error}").unwrap();
    }
    res
}

#[test]
fn parse_ok() {
    for case in TestCase::list("parser/ok") {
        let _guard = stdx::panic_context::enter(format!("{:?}", case.rs));
        let (actual, errors) = parse(TopEntryPoint::SourceFile, &case.text, Edition::CURRENT);
        assert!(!errors, "errors in an OK file {}:\n{actual}", case.rs.display());
        expect_file![case.rast].assert_eq(&actual);
    }
}

#[test]
fn parse_err() {
    for case in TestCase::list("parser/err") {
        let _guard = stdx::panic_context::enter(format!("{:?}", case.rs));
        let (actual, errors) = parse(TopEntryPoint::SourceFile, &case.text, Edition::CURRENT);
        assert!(errors, "no errors in an ERR file {}:\n{actual}", case.rs.display());
        expect_file![case.rast].assert_eq(&actual)
    }
}

fn parse(entry: TopEntryPoint, text: &str, edition: Edition) -> (String, bool) {
    let lexed = LexedStr::new(edition, text);
    let input = lexed.to_input(edition);
    let output = entry.parse(&input);

    let mut buf = String::new();
    let mut errors = Vec::new();
    let mut indent = String::new();
    let mut depth = 0;
    let mut len = 0;
    lexed.intersperse_trivia(&output, &mut |step| match step {
        crate::StrStep::Token { kind, text } => {
            assert!(depth > 0);
            len += text.len();
            writeln!(buf, "{indent}{kind:?} {text:?}").unwrap();
        }
        crate::StrStep::Enter { kind } => {
            assert!(depth > 0 || len == 0);
            depth += 1;
            writeln!(buf, "{indent}{kind:?}").unwrap();
            indent.push_str("  ");
        }
        crate::StrStep::Exit => {
            assert!(depth > 0);
            depth -= 1;
            indent.pop();
            indent.pop();
        }
        crate::StrStep::Error { msg, pos } => {
            assert!(depth > 0);
            errors.push(format!("error {pos}: {msg}\n"))
        }
    });
    assert_eq!(
        len,
        text.len(),
        "didn't parse all text.\nParsed:\n{}\n\nAll:\n{}\n",
        &text[..len],
        text
    );

    for (token, msg) in lexed.errors() {
        let pos = lexed.text_start(token);
        errors.push(format!("error {pos}: {msg}\n"));
    }

    let has_errors = !errors.is_empty();
    for e in errors {
        buf.push_str(&e);
    }
    (buf, has_errors)
}

/// The tree's lines without their indentation, and the errors.
fn parse_lines(text: &str, edition: Edition) -> (Vec<String>, Vec<String>) {
    let (out, _) = parse(TopEntryPoint::SourceFile, text, edition);
    let (errors, tree): (Vec<_>, Vec<_>) =
        out.lines().map(|it| it.trim_start().to_owned()).partition(|it| it.starts_with("error "));
    (tree, errors)
}

fn has_run(lines: &[String], run: &[&str]) -> bool {
    lines.windows(run.len()).any(|window| window.iter().zip(run).all(|(a, b)| a == b))
}

const LABELED_LOOP_IN_BREAK: &str =
    "parentheses are required around this expression to avoid confusion with a labeled break expression";

/// rustc's `parse_expr_break`: the labeled loop is the value of an unlabeled `break`, with
/// `LabeledLoopInBreak` where the loop ends.
#[test]
fn a_break_of_a_labeled_loop_takes_it_as_its_value() {
    let text = "fn f() -> u32 { loop { break 'a: loop { break 'a 42; }; } }";
    let (tree, errors) = parse_lines(text, Edition::CURRENT);
    assert!(
        has_run(
            &tree,
            &[
                "BREAK_EXPR",
                "BREAK_KW \"break\"",
                "WHITESPACE \" \"",
                "LOOP_EXPR",
                "LABEL",
                "LIFETIME",
                "LIFETIME_IDENT \"'a\"",
                "COLON \":\"",
            ],
        ),
        "{tree:#?}"
    );
    assert!(
        has_run(&tree, &["BREAK_EXPR", "BREAK_KW \"break\"", "WHITESPACE \" \"", "LIFETIME"]),
        "the inner `break 'a 42` keeps its label: {tree:#?}"
    );
    let end = text.find("42; }").unwrap() + "42; }".len();
    assert_eq!(errors, [format!("error {end}: {LABELED_LOOP_IN_BREAK}")]);
}

/// Each loop and a block after the label, in every edition; a label before anything else is
/// the label's own error, not this one; a labeled break with a labeled value is no error.
#[test]
fn every_labeled_value_of_a_break_is_parenthesized() {
    for edition in Edition::iter() {
        for value in ["'a: loop {}", "'a: while x {}", "'a: for x in y {}", "'a: {}"] {
            let text = format!("fn f() {{ loop {{ break {value}; }} }}");
            let (tree, errors) = parse_lines(&text, edition);
            let end = text.find(value).unwrap() + value.len();
            assert_eq!(errors, [format!("error {end}: {LABELED_LOOP_IN_BREAK}")], "{value}");
            assert!(!tree.iter().any(|it| it == "ERROR"), "{value}: {tree:#?}");
        }
        let (_, errors) = parse_lines("fn f() { loop { break 'a: 1; } }", edition);
        assert!(errors.iter().any(|it| it.ends_with("expected a loop or block")), "{errors:?}");
        assert!(!errors.iter().any(|it| it.ends_with(LABELED_LOOP_IN_BREAK)), "{errors:?}");
        let (_, errors) =
            parse_lines("fn f() { 'b: loop { break 'b 'a: loop {}; } }", edition);
        assert_eq!(errors, Vec::<String>::new());
    }
}

/// rustc's lexer before 2021 gives a `macro_rules!` matcher `'r`, `#`, `lt` for `'r#lt` and
/// `c`, `"x"` for `c"x"` (tests/ui/lifetimes/raw/three-tokens.rs); the tree has them as tokens.
#[test]
fn a_raw_lifetime_and_a_c_string_are_split_before_2021() {
    let text = "macro_rules! m { ('r # lt) => {}; (c \"x\") => {}; }\nm!('r#lt);\nm!(c\"x\");\n";
    for edition in [Edition::Edition2015, Edition::Edition2018] {
        let (tree, errors) = parse_lines(text, edition);
        assert_eq!(errors, Vec::<String>::new(), "{edition:?}");
        assert!(
            has_run(&tree, &["L_PAREN \"(\"", "LIFETIME_IDENT \"'r\"", "POUND \"#\"", "IDENT \"lt\""]),
            "{edition:?}: {tree:#?}"
        );
        assert!(
            has_run(&tree, &["L_PAREN \"(\"", "IDENT \"c\"", "STRING \"\\\"x\\\"\""]),
            "{edition:?}: {tree:#?}"
        );
    }
    for edition in [Edition::Edition2021, Edition::Edition2024] {
        let (tree, _) = parse_lines(text, edition);
        assert!(tree.iter().any(|it| it == "LIFETIME_IDENT \"'r#lt\""), "{edition:?}");
        assert!(tree.iter().any(|it| it == "C_STRING \"c\\\"x\\\"\""), "{edition:?}");
    }
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct TestCase {
    rs: PathBuf,
    rast: PathBuf,
    text: String,
}

impl TestCase {
    fn list(path: &'static str) -> Vec<TestCase> {
        let crate_root_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let test_data_dir = crate_root_dir.join("test_data");
        let dir = test_data_dir.join(path);

        let mut res = Vec::new();
        let read_dir = fs::read_dir(&dir)
            .unwrap_or_else(|err| panic!("can't `read_dir` {}: {err}", dir.display()));
        for file in read_dir {
            let file = file.unwrap();
            let path = file.path();
            if path.extension().unwrap_or_default() == "rs" {
                let rs = path;
                let rast = rs.with_extension("rast");
                let text = fs::read_to_string(&rs).unwrap();
                res.push(TestCase { rs, rast, text });
            }
        }
        res.sort();
        res
    }
}

#[track_caller]
fn run_and_expect_no_errors(path: &str) {
    run_and_expect_no_errors_with_edition(path, Edition::CURRENT)
}

#[track_caller]
fn run_and_expect_errors(path: &str) {
    run_and_expect_errors_with_edition(path, Edition::CURRENT)
}

#[track_caller]
fn run_and_expect_no_errors_with_edition(path: &str, edition: Edition) {
    let path = PathBuf::from(path);
    let text = std::fs::read_to_string(&path).unwrap();
    let (actual, errors) = parse(TopEntryPoint::SourceFile, &text, edition);
    assert!(!errors, "errors in an OK file {}:\n{actual}", path.display());
    let mut p = PathBuf::from("..");
    p.push(path);
    p.set_extension("rast");
    expect_file![p].assert_eq(&actual)
}

#[track_caller]
fn run_and_expect_errors_with_edition(path: &str, edition: Edition) {
    let path = PathBuf::from(path);
    let text = std::fs::read_to_string(&path).unwrap();
    let (actual, errors) = parse(TopEntryPoint::SourceFile, &text, edition);
    assert!(errors, "no errors in an ERR file {}:\n{actual}", path.display());
    let mut p = PathBuf::from("..");
    p.push(path);
    p.set_extension("rast");
    expect_file![p].assert_eq(&actual)
}

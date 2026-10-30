use blaze_api::{Artifact, BuildScriptRan, Diagnostic, Level, Span, UnitOf, UnitProfile, UnitTarget};
use blaze_ids::ContentId;

use super::*;

fn bin_target(dir: &str) -> UnitTarget {
    UnitTarget {
        kind: vec!["bin".to_owned()],
        crate_types: vec!["bin".to_owned()],
        name: "foo".to_owned(),
        src_path: format!("{dir}/src/main.rs"),
        edition: "2021".to_owned(),
        doc: true,
        doctest: false,
        test: true,
    }
}

const TARGET: &str = r#"{"kind":["bin"],"crate_types":["bin"],"name":"foo","src_path":"/w/foo/src/main.rs","edition":"2021","doc":true,"doctest":false,"test":true}"#;

#[test]
fn build_finished() {
    assert_eq!(build_finished(true), r#"{"reason":"build-finished","success":true}"#);
    assert_eq!(build_finished(false), r#"{"reason":"build-finished","success":false}"#);
}

#[test]
fn compiler_artifact() {
    let artifact = Artifact {
        package: "path+file:///w/foo#0.1.0".to_owned(),
        kind: "bin".to_owned(),
        path: "/w/foo/target/debug/foo".to_owned(),
        content: ContentId([0; 32]),
        manifest_path: "/w/foo/Cargo.toml".to_owned(),
        target: bin_target("/w/foo"),
        profile: UnitProfile {
            opt_level: "0".to_owned(),
            debuginfo: 2,
            debug_assertions: true,
            overflow_checks: true,
            test: false,
        },
        features: vec!["default".to_owned()],
        filenames: vec!["/w/foo/target/debug/foo".to_owned()],
        executable: Some("/w/foo/target/debug/foo".to_owned()),
        fresh: false,
        runner: Vec::new(),
        env: vec![("CARGO_PKG_NAME".to_owned(), "foo".to_owned())],
        harness: true,
    };
    let expected = format!(
        r#"{{"reason":"compiler-artifact","package_id":"path+file:///w/foo#0.1.0","manifest_path":"/w/foo/Cargo.toml","target":{TARGET},"profile":{{"opt_level":"0","debuginfo":2,"debug_assertions":true,"overflow_checks":true,"test":false}},"features":["default"],"filenames":["/w/foo/target/debug/foo"],"executable":"/w/foo/target/debug/foo","fresh":false}}"#
    );
    assert_eq!(super::artifact(&artifact), expected);
    let lib = Artifact { executable: None, fresh: true, ..artifact };
    assert!(super::artifact(&lib).ends_with(r#""executable":null,"fresh":true}"#));
}

#[test]
fn build_script_executed() {
    let ran = BuildScriptRan {
        package_id: "path+file:///w/foo#0.1.0".to_owned(),
        linked_libs: vec!["static=z".to_owned()],
        linked_paths: vec!["native=/w/foo/target/debug/build/foo-1/out".to_owned()],
        cfgs: vec!["has_z".to_owned()],
        env: vec![("Z_VERSION".to_owned(), "1.3".to_owned())],
        out_dir: "/w/foo/target/debug/build/foo-1/out".to_owned(),
    };
    assert_eq!(
        build_script(&ran),
        r#"{"reason":"build-script-executed","package_id":"path+file:///w/foo#0.1.0","linked_libs":["static=z"],"linked_paths":["native=/w/foo/target/debug/build/foo-1/out"],"cfgs":["has_z"],"env":[["Z_VERSION","1.3"]],"out_dir":"/w/foo/target/debug/build/foo-1/out"}"#
    );
}

#[test]
fn strings_as_serde_json_writes_them() {
    let text = "a\"b\\c\nd\te\u{1b}[0m\u{7f}é/";
    assert_eq!(J::str(text).line(), serde_json::to_string(text).unwrap());
}

fn span(file: &str, (line, column): (u32, u32), (end_line, end_column): (u32, u32), bytes: (u32, u32)) -> Span {
    Span {
        file: file.to_owned(),
        line,
        column,
        end_line,
        end_column,
        label: None,
        byte_start: bytes.0,
        byte_end: bytes.1,
        primary: true,
        replacement: None,
        applicability: None,
    }
}

fn unused(rendered: &str) -> Diagnostic {
    let at = span("src/main.rs", (2, 9), (2, 10), (20, 21));
    let help = Diagnostic {
        level: Level::Help,
        code: None,
        message: "if this is intentional, prefix it with an underscore".to_owned(),
        spans: vec![Span {
            replacement: Some("_x".to_owned()),
            applicability: Some("MachineApplicable".to_owned()),
            ..at.clone()
        }],
        children: Vec::new(),
        facts: Vec::new(),
        rendered: String::new(),
        unit: None,
    };
    Diagnostic {
        level: Level::Warning,
        code: Some("unused_variables".to_owned()),
        message: "unused variable: `x`".to_owned(),
        spans: vec![at],
        children: vec![help],
        facts: Vec::new(),
        rendered: rendered.to_owned(),
        unit: None,
    }
}

fn checkout() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/main.rs"), "fn main() {\r\n    let x = 1;\r\n}\r\n").unwrap();
    std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
    let root = dir.path().display().to_string();
    (dir, root)
}

#[test]
fn a_compiler_message_as_cargo_writes_it_again() {
    let (_dir, root) = checkout();
    let unit = UnitOf {
        package_id: format!("path+file://{root}#0.1.0"),
        manifest_path: format!("{root}/Cargo.toml"),
        target: bin_target(&root),
        test: false,
    };
    let rendered = "\u{1b}[1m\u{1b}[33mwarning\u{1b}[0m: unused variable: `x`\n\n";
    let mut sources = Sources::new(std::path::Path::new(&root));
    let said = compiler_message(&unit, &unused(rendered), false, &mut sources);
    let target = format!(
        r#"{{"kind":["bin"],"crate_types":["bin"],"name":"foo","src_path":"{root}/src/main.rs","edition":"2021","doc":true,"doctest":false,"test":true}}"#
    );
    let text = r#"[{"highlight_end":10,"highlight_start":9,"text":"    let x = 1;"}]"#;
    let expected = format!(
        concat!(
            r#"{{"reason":"compiler-message","package_id":"path+file://{root}#0.1.0","manifest_path":"{root}/Cargo.toml","target":{target},"#,
            r#""message":{{"rendered":"warning: unused variable: `x`\n\n","$message_type":"diagnostic","#,
            r#""children":[{{"children":[],"code":null,"level":"help","message":"if this is intentional, prefix it with an underscore","rendered":null,"#,
            r#""spans":[{{"byte_end":21,"byte_start":20,"column_end":10,"column_start":9,"expansion":null,"file_name":"src/main.rs","is_primary":true,"label":null,"line_end":2,"line_start":2,"suggested_replacement":"_x","suggestion_applicability":"MachineApplicable","text":{text}}}]}}],"#,
            r#""code":{{"code":"unused_variables","explanation":null}},"level":"warning","message":"unused variable: `x`","#,
            r#""spans":[{{"byte_end":21,"byte_start":20,"column_end":10,"column_start":9,"expansion":null,"file_name":"src/main.rs","is_primary":true,"label":null,"line_end":2,"line_start":2,"suggested_replacement":null,"suggestion_applicability":null,"text":{text}}}]}}}}"#,
        ),
        root = root,
        target = target,
        text = text,
    );
    assert_eq!(said, expected);
}

#[test]
fn a_compiler_message_in_color_is_rustcs_as_it_wrote_it() {
    let (_dir, root) = checkout();
    let unit = UnitOf {
        package_id: format!("path+file://{root}#0.1.0"),
        manifest_path: format!("{root}/Cargo.toml"),
        target: bin_target(&root),
        test: false,
    };
    let rendered = "\u{1b}[33mwarning\u{1b}[0m\n";
    let mut sources = Sources::new(std::path::Path::new(&root));
    let said = compiler_message(&unit, &unused(rendered), true, &mut sources);
    let message = said.split_once(r#""message":"#).unwrap().1;
    let text = r#"[{"text":"    let x = 1;","highlight_start":9,"highlight_end":10}]"#;
    let expected = format!(
        concat!(
            r#"{{"$message_type":"diagnostic","message":"unused variable: `x`","code":{{"code":"unused_variables","explanation":null}},"level":"warning","#,
            r#""spans":[{{"file_name":"src/main.rs","byte_start":20,"byte_end":21,"line_start":2,"line_end":2,"column_start":9,"column_end":10,"is_primary":true,"text":{text},"label":null,"suggested_replacement":null,"suggestion_applicability":null,"expansion":null}}],"#,
            r#""children":[{{"message":"if this is intentional, prefix it with an underscore","code":null,"level":"help","#,
            r#""spans":[{{"file_name":"src/main.rs","byte_start":20,"byte_end":21,"line_start":2,"line_end":2,"column_start":9,"column_end":10,"is_primary":true,"text":{text},"label":null,"suggested_replacement":"_x","suggestion_applicability":"MachineApplicable","expansion":null}}],"children":[],"rendered":null}}],"#,
            r#""rendered":"\u001b[33mwarning\u001b[0m\n"}}}}"#,
        ),
        text = text,
    );
    assert_eq!(message, expected);
}

#[test]
fn a_span_over_lines_and_one_of_no_file() {
    let (_dir, root) = checkout();
    let manifest = format!("{root}/Cargo.toml");
    let mut sources = Sources::new(std::path::Path::new(&root));
    let lines = sources.lines("src/main.rs", std::path::Path::new(&manifest)).unwrap().to_vec();
    let over = span("src/main.rs", (1, 11), (3, 2), (10, 28));
    assert_eq!(
        J::Arr(span_text(&over, &lines)).line(),
        r#"[{"text":"fn main() {","highlight_start":11,"highlight_end":12},{"text":"    let x = 1;","highlight_start":1,"highlight_end":15},{"text":"}","highlight_start":1,"highlight_end":2}]"#
    );
    assert!(sources.lines("src/gone.rs", std::path::Path::new(&manifest)).is_none());
    let absolute = format!("{root}/src/main.rs");
    assert_eq!(sources.lines(&absolute, std::path::Path::new("/elsewhere/Cargo.toml")).unwrap()[0], "fn main() {");
}

//! cargo's rule of what reruns a script (`tests/testsuite/build_script.rs` of rust-lang/cargo
//! at 5f94df478: `rebuild_only_on_explicit_paths`, `rerun_if_directory`,
//! `rerun_if_env_changes`, `rerun_if_published_directory`), as data.

use super::*;

#[test]
fn nothing_printed_watches_the_whole_package() {
    let watched = watched(&Output::default(), Path::new("/p/foo"));
    assert_eq!(watched, Watched {
        paths: vec!["/p/foo".into()],
        whole_package: true,
        env: Vec::new()
    });
    // Other directives don't name what the script read.
    let output = Output { cfgs: vec!["foo".into()], ..Output::default() };
    assert!(super::watched(&output, Path::new("/p/foo")).whole_package);
}

// The separator joined with is the host's.
#[cfg(unix)]
#[test]
fn printed_paths_are_made_whole_against_the_package() {
    let output = Output {
        rerun_if_changed: vec![
            "build.rs".into(),
            "/abs/file".into(),
            "src/dir".into(),
            "../shared/x".into(),
            "build.rs".into(),
        ],
        ..Output::default()
    };
    let watched = watched(&output, Path::new("/p/foo"));
    assert!(!watched.whole_package);
    assert_eq!(watched.paths, [
        PathBuf::from("/p/foo/build.rs"),
        PathBuf::from("/abs/file"),
        PathBuf::from("/p/foo/src/dir"),
        PathBuf::from("/p/foo/../shared/x"),
    ]);
    assert!(watched.env.is_empty());
}

/// A variable alone is enough for cargo to stop watching the package: the script then reruns
/// for the variable only.
#[test]
fn variables_alone() {
    let output = Output {
        rerun_if_env_changed: vec!["FOO".into(), "BAR".into(), "FOO".into()],
        ..Output::default()
    };
    let watched = watched(&output, Path::new("/p/foo"));
    assert_eq!(watched, Watched { paths: Vec::new(), whole_package: false, env: vec![
        "FOO".into(),
        "BAR".into()
    ] });
}

#[test]
fn what_the_whole_package_leaves_out() {
    assert!(skipped(Path::new("target"), false));
    assert!(skipped(Path::new("./target"), false));
    assert!(!skipped(Path::new("src/target"), false), "only the package's own");
    assert!(!skipped(Path::new("target/debug"), false), "below a skipped one is never asked");
    assert!(skipped(Path::new("crates/inner"), true), "another package");
    assert!(!skipped(Path::new("crates/inner"), false));
    assert!(!skipped(Path::new(""), true), "the package itself");
    assert!(!skipped(Path::new("."), true), "the package itself");
    assert!(!skipped(Path::new(".git"), false), "watched: more than cargo is only slower");
}

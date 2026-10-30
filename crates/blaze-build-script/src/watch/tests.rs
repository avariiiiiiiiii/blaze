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

/// Each value kept once, the first time, as a scan of what is kept has it: past [`SCAN`]
/// values by hash, with hashes made to collide by a list of the same few values in many
/// spellings that compare equal as paths.
#[test]
fn kept_once_by_hash_as_by_scan() {
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = |bound: u64| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state % bound
    };
    for _ in 0..2_000 {
        let len = next(3 * SCAN as u64) as usize;
        let names: Vec<String> =
            (0..len).map(|_| format!("V{}", next(len as u64 / 2 + 1))).collect();
        let paths: Vec<String> = (0..len)
            .map(|_| match next(4) {
                0 => format!("src/{}.rs", next(8)),
                1 => format!("./src/{}.rs", next(8)),
                2 => format!("src//{}.rs", next(8)),
                _ => format!("/abs/{}", next(8)),
            })
            .collect();
        let watched = watched_of(&paths, &names, Path::new("/p/foo"));
        let (mut want_paths, mut want_env) = (Vec::<PathBuf>::new(), Vec::<String>::new());
        for path in &paths {
            let whole = Path::new("/p/foo").join(path);
            if !want_paths.contains(&whole) {
                want_paths.push(whole);
            }
        }
        for name in &names {
            if !want_env.contains(name) {
                want_env.push(name.clone());
            }
        }
        if len == 0 {
            assert!(watched.whole_package);
            continue;
        }
        assert_eq!(watched.paths, want_paths, "{paths:?}");
        assert_eq!(watched.env, want_env, "{names:?}");
    }
}

/// What a run's output makes cargo watch, read in the pass that keeps only the `rerun-if-*`
/// values, as made from the whole [`Output`]: the same, or the same error.
#[test]
fn watched_in_the_output_as_from_the_parsed_output() {
    let package = crate::unit::Package {
        name: "foo".into(),
        version: "0.0.1".into(),
        descr: Some("foo v0.0.1 (/p/foo)".into()),
        ..crate::unit::Package::default()
    };
    let cx = output::Context {
        package: &package,
        out_dir_when_generated: Path::new("/old/out"),
        out_dir: Path::new("/new/out"),
        nightly_features_allowed: false,
        rustc_bootstrap: None,
    };
    let lines = [
        "cargo:rerun-if-changed=build.rs",
        "cargo::rerun-if-changed=src/a.rs",
        "cargo:rerun-if-changed=/old/out/gen.rs",
        "cargo:rerun-if-env-changed=CC",
        "cargo::rerun-if-env-changed=CFLAGS",
        "cargo:rustc-cfg=x",
        "cargo::warning=w",
        "cargo::error=e",
        "cargo::unknown=v",
        "cargo:k=v",
        "not a directive",
        "  cargo:rerun-if-changed=  spaced  ",
        "cargo::rerun-if-changed",
    ];
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = |bound: usize| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state % bound as u64) as usize
    };
    let mut ok = 0;
    for _ in 0..5_000 {
        let mut stdout = String::new();
        for _ in 0..next(40) {
            stdout.push_str(lines[next(lines.len())]);
            stdout.push('\n');
        }
        let made =
            output::parse(stdout.as_bytes(), &cx).map(|it| watched(&it, Path::new("/p/foo")));
        let read = watched_in(stdout.as_bytes(), &cx, Path::new("/p/foo"));
        match (read, made) {
            (Ok(read), Ok(made)) => {
                assert_eq!(read, made, "{stdout:?}");
                ok += 1;
            }
            (Err(read), Err(made)) => assert_eq!(read.0, made.0, "{stdout:?}"),
            (read, made) => panic!("{stdout:?}: {read:?} against {made:?}"),
        }
    }
    assert!(ok > 100, "{ok} read");
}

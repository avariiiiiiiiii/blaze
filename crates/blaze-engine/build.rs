//! `BLAZE_VERSION` and `BLAZE_VERSION_LINE`, which blaze this is (see [`version`]), and
//! `BLAZE_SOURCES`: what this build of blaze is built from, for the store to key what it
//! keeps by (`blaze_store::built_from`). A hash of every file of the workspace's crates, the
//! vendored ones included, of the manifest and lock file that pick the crates of crates.io, of
//! the files outside them a crate compiles in, of the features, the target and whether debug
//! assertions are on, which some of codegen checks under. The compiler that builds blaze is
//! not: what blaze compiles does not depend on it, and the toolchain it compiles against is
//! in the store's key apart. Two such builds of the same sources compile the same code into
//! the store, wherever they are built. Outside the workspace, as a published crate builds, it
//! is not set, and the store keys by the bytes of the executable.
//!
//! The SHA-256 is `blaze-sha256`'s, which has no dependencies, rather than `sha2`'s: a build
//! dependency is built for the machine that builds, and `tools/nix/units` holds one graph per
//! target computed on Linux, where `sha2`'s `cpufeatures` needs no `libc`, which it needs on
//! Apple's aarch64.

use std::path::{Path, PathBuf};

use blaze_sha256::Sha256;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    version();
    println!("cargo:rerun-if-env-changed=CARGO_WORKSPACE_DIR");
    let Some(root) = std::env::var_os("CARGO_WORKSPACE_DIR").map(PathBuf::from) else { return };
    let (lock, crates, vendor) =
        (root.join("Cargo.lock"), root.join("crates"), root.join("vendor"));
    if !lock.is_file() || !crates.is_dir() {
        return;
    }
    let mut files = Vec::new();
    for top in [&crates, &vendor] {
        println!("cargo:rerun-if-changed={}", top.display());
        listed(top, &mut files);
    }
    // `harness/toolchain` is compiled in (`cargo/state.rs`).
    for file in ["Cargo.toml", "Cargo.lock", ".cargo/config.toml", "harness/toolchain"] {
        let path = root.join(file);
        println!("cargo:rerun-if-changed={}", path.display());
        files.push(path);
    }
    files.sort();
    let mut hash = Sha256::new();
    let mut field = |bytes: &[u8]| {
        hash.update(&(bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    };
    for path in &files {
        let Ok(bytes) = std::fs::read(path) else { continue };
        let name = path.strip_prefix(&root).unwrap_or(path).to_string_lossy().replace('\\', "/");
        field(name.as_bytes());
        field(&bytes);
    }
    let mut features: Vec<String> =
        std::env::vars().map(|it| it.0).filter(|it| it.starts_with("CARGO_FEATURE_")).collect();
    features.sort();
    field(features.join(" ").as_bytes());
    field(std::env::var("TARGET").unwrap_or_default().as_bytes());
    field(&[u8::from(std::env::var_os("CARGO_CFG_DEBUG_ASSERTIONS").is_some())]);
    let hex: String = hash.finish().iter().map(|byte| format!("{byte:02x}")).collect();
    println!("cargo:rustc-env=BLAZE_SOURCES={hex}");
}

/// Which blaze this is, from the one place it is said: `BLAZE_RELEASE`, the tag a release is
/// built as (`v0.YYYYMMDD.N`), which only release.yaml sets. The workspace's own version in
/// Cargo.toml stays `0.0.0` on main, so that no release changes the identity of every crate
/// (and every crate's place in blaze's store); only this crate, which changes with every commit
/// anyway, is told the release. Sets
///
/// - `BLAZE_VERSION`: the release's `0.YYYYMMDD.N`; for a build of the source tree (Cargo.toml's
///   `0.0.0`, no `BLAZE_RELEASE`) `0.0.0-dev`, which is no release and which no pin names; for
///   a crate published with its version written in, that version.
/// - `BLAZE_VERSION_LINE`, what `--version` prints first: `blaze 0.20261028.9 (fb82b09
///   2026-10-28)` for a release, as rustc says its own, and `blaze 0.0.0-dev (fb82b09)` for a
///   build of the tree. The commit is `BLAZE_GIT_SHA` and `BLAZE_GIT_DATE` (release.yaml and the
///   flake set them, where there may be no `.git`), else the checkout's HEAD, else none. A
///   release must be told both; the version stays field 2 (.github/actions/blaze reads it).
///
/// The build runs again when HEAD moves (its reflog grows with each commit and checkout).
fn version() {
    for name in ["BLAZE_RELEASE", "BLAZE_GIT_SHA", "BLAZE_GIT_DATE"] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    let given = |name: &str| std::env::var(name).ok().filter(|it| !it.is_empty());
    let package = std::env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION");
    let release = given("BLAZE_RELEASE");
    let version = match &release {
        Some(tag) => {
            let dated = tag.strip_prefix("v0.").and_then(|it| it.split_once('.')).is_some_and(
                |(day, n)| {
                    day.len() == 8
                        && day.bytes().all(|b| b.is_ascii_digit())
                        && !n.is_empty()
                        && n.bytes().all(|b| b.is_ascii_digit())
                        && (n == "0" || !n.starts_with('0'))
                },
            );
            assert!(dated, "BLAZE_RELEASE is {tag:?}, not a release's tag v0.YYYYMMDD.N");
            let version = tag[1..].to_owned();
            assert!(
                package == "0.0.0" || package == version,
                "BLAZE_RELEASE is {tag}, and the workspace's version is {package}: a release's is \
                 either the tree's 0.0.0 or its own"
            );
            version
        }
        None if package == "0.0.0" => "0.0.0-dev".to_owned(),
        None => package,
    };
    // TZ: the date is UTC's, as release.yaml's `BLAZE_GIT_DATE`.
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .env("TZ", "UTC0")
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .ok()
            .filter(|out| out.status.success())?;
        String::from_utf8(out.stdout)
            .ok()
            .map(|it| it.trim().to_owned())
            .filter(|it| !it.is_empty())
    };
    let (sha, date) = match (given("BLAZE_GIT_SHA"), given("BLAZE_GIT_DATE")) {
        (Some(sha), date) => (Some(sha), date),
        (None, _) => {
            for path in ["HEAD", "logs/HEAD"] {
                if let Some(file) =
                    git(&["rev-parse", "--path-format=absolute", "--git-path", path])
                {
                    println!("cargo:rerun-if-changed={file}");
                }
            }
            let sha = git(&["rev-parse", "--short=7", "HEAD"]);
            let date = sha.as_ref().and_then(|_| {
                git(&["show", "-s", "--date=format-local:%Y-%m-%d", "--format=%cd", "HEAD"])
            });
            (sha, date)
        }
    };
    let line = match (&release, sha, date) {
        (Some(_), Some(sha), Some(date)) => format!("blaze {version} ({sha} {date})"),
        (Some(tag), sha, date) => panic!(
            "BLAZE_RELEASE is {tag}, and a release names its commit and the commit's date: \
             BLAZE_GIT_SHA is {sha:?} and BLAZE_GIT_DATE is {date:?}"
        ),
        (None, Some(sha), _) => format!("blaze {version} ({sha})"),
        (None, None, _) => format!("blaze {version}"),
    };
    println!("cargo:rustc-env=BLAZE_VERSION={version}");
    println!("cargo:rustc-env=BLAZE_VERSION_LINE={line}");
}

/// Every file below `dir`, but in hidden directories and in what cargo and npm build into.
fn listed(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            let built = name == "target" || name == "node_modules";
            if !name.to_string_lossy().starts_with('.') && !built {
                listed(&entry.path(), files);
            }
        } else {
            files.push(entry.path());
        }
    }
}

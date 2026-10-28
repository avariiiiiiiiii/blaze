//! Which blaze a workspace pins: `version` under `[workspace.metadata.blaze]` in the root
//! manifest, a release's version (`0.YYYYMMDD.N`). The `blaze` the user runs runs that
//! release when it is not that release itself, from `~/.blaze/versions/<version>`, fetched
//! once when it is not there (`versions.rs`), as Bazelisk runs the Bazel a workspace names.
//! A project pins blaze, and so the Rust blaze ships; Rust's own toolchain files are not read
//! (docs/toolchain.md).
//!
//! Shared by the launcher (`bin/blaze.rs`), which must stay small and quick on every
//! command: std and `toml` only.

use std::path::{Path, PathBuf};

/// The variable the launcher sets for the blaze it runs: the manifest that pinned it. A blaze
/// started with it is the pinned one, and runs no other.
pub(crate) const PINNED_BY: &str = "BLAZE_PINNED_BY";

/// This blaze's version, as a pin names it.
pub(crate) const OURS: &str = env!("CARGO_PKG_VERSION");

/// Where a command run with `args` in `cwd` starts looking for its workspace: the directory
/// of `--manifest-path`, else `cwd`.
pub(crate) fn start(args: &[String], cwd: &Path) -> PathBuf {
    let mut words = args.iter().take_while(|it| *it != "--");
    while let Some(word) = words.next() {
        let named = match word.strip_prefix("--manifest-path") {
            Some("") => words.next().map(String::as_str),
            Some(rest) => rest.strip_prefix('='),
            None => None,
        };
        if let Some(manifest) = named {
            let manifest = cwd.join(manifest);
            return manifest.parent().map_or_else(|| cwd.to_owned(), Path::to_owned);
        }
    }
    cwd.to_owned()
}

/// The root manifest of the workspace `dir` is in, as cargo finds it: the nearest
/// `Cargo.toml` that has a `[workspace]`, or the one its `package.workspace` names, or else
/// the nearest `Cargo.toml` itself. `None` outside any package.
pub(crate) fn root_manifest(dir: &Path) -> Option<PathBuf> {
    let mut nearest = None;
    for dir in dir.ancestors() {
        let manifest = dir.join("Cargo.toml");
        let Ok(text) = std::fs::read_to_string(&manifest) else { continue };
        let Ok(table) = toml::from_str::<toml::Table>(&text) else {
            // cargo says what is wrong with it.
            return Some(nearest.unwrap_or(manifest));
        };
        if table.contains_key("workspace") {
            return Some(manifest);
        }
        let named = table.get("package").and_then(|it| it.get("workspace"));
        if let Some(root) = named.and_then(toml::Value::as_str) {
            return Some(dir.join(root).join("Cargo.toml"));
        }
        nearest.get_or_insert(manifest);
    }
    nearest
}

/// The version of blaze `manifest` pins, `None` when it pins none. `Err` is a pin that is not
/// a version.
pub(crate) fn pinned(manifest: &Path) -> Result<Option<String>, String> {
    let Ok(text) = std::fs::read_to_string(manifest) else { return Ok(None) };
    let Ok(table) = toml::from_str::<toml::Table>(&text) else { return Ok(None) };
    let blaze =
        table.get("workspace").and_then(|it| it.get("metadata")).and_then(|it| it.get("blaze"));
    let Some(version) = blaze.and_then(|it| it.get("version")) else { return Ok(None) };
    match version.as_str() {
        Some(version) if is_version(version) => Ok(Some(version.to_owned())),
        _ => Err(format!(
            "{} pins blaze to {version} under `[workspace.metadata.blaze]`, which is not a \
             version of blaze (`0.YYYYMMDD.N`, as `blaze pin` writes it)",
            manifest.display()
        )),
    }
}

/// Whether `version` is a release's version: numbers with dots between, so that it names a
/// directory and an asset and nothing else.
pub(crate) fn is_version(version: &str) -> bool {
    !version.is_empty()
        && version.split('.').all(|it| !it.is_empty() && it.bytes().all(|b| b.is_ascii_digit()))
}

/// Where blaze keeps the releases a pin ran: `versions` in `BLAZE_HOME`, else in `.blaze` in
/// the user's home.
pub(crate) fn versions() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("BLAZE_HOME").filter(|it| !it.is_empty()) {
        return Some(PathBuf::from(home).join("versions"));
    }
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })?;
    Some(PathBuf::from(home).join(".blaze").join("versions"))
}

/// The `blaze` of `version` where [`versions`] keeps it.
pub(crate) fn installed(version: &str) -> Option<PathBuf> {
    Some(versions()?.join(version).join(format!("blaze{}", std::env::consts::EXE_SUFFIX)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_read() {
        assert!(is_version("0.20261027.0"));
        assert!(!is_version("0.20261027."));
        assert!(!is_version("../0.1"));
        assert!(!is_version("nightly"));
        assert!(!is_version(""));
    }

    #[test]
    fn the_root_manifest_holds_the_pin() {
        let root = std::env::temp_dir().join(format!("blaze-pin-{}", std::process::id()));
        let member = root.join("crates/a");
        std::fs::create_dir_all(member.join("src")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/a\"]\n\n[workspace.metadata.blaze]\nversion = \"0.20261027.0\"\n",
        )
        .unwrap();
        std::fs::write(member.join("Cargo.toml"), "[package]\nname = \"a\"\n").unwrap();
        let found = root_manifest(&member.join("src")).unwrap();
        assert_eq!(found, root.join("Cargo.toml"));
        assert_eq!(pinned(&found).unwrap().as_deref(), Some("0.20261027.0"));
        assert_eq!(pinned(&member.join("Cargo.toml")).unwrap(), None);
        std::fs::write(root.join("Cargo.toml"), "[workspace.metadata.blaze]\nversion = 3\n")
            .unwrap();
        assert!(pinned(&found).is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_manifest_path_starts_the_search() {
        let cwd = Path::new("/w");
        let args = |line: &str| line.split(' ').map(str::to_owned).collect::<Vec<_>>();
        assert_eq!(start(&args("build --manifest-path x/Cargo.toml"), cwd), cwd.join("x"));
        assert_eq!(start(&args("build --manifest-path=/y/Cargo.toml"), cwd), Path::new("/y"));
        assert_eq!(start(&args("run -- --manifest-path z/Cargo.toml"), cwd), cwd);
    }
}

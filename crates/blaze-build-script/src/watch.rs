//! What cargo's rule watches of a run, as data: for a run that read what can't be traced (the
//! clock, randomness, the network), which is keyed as cargo keys it (#1450). cargo's rule is
//! `build_script_local_fingerprints` and `local_fingerprints_deps` in
//! `src/core/compiler/fingerprint/mod.rs`, and for the whole package `PathSource::list_files`
//! (`list_files_walk`) in `src/sources/path.rs`, in rust-lang/cargo at 5f94df478; [`Rerun`]
//! is the same rule as cargo checks it on disk.
//!
//! [`Rerun`]: crate::apply::Rerun

use std::{
    ffi::OsStr,
    path::{Component, Path, PathBuf},
};

use crate::output::Output;

/// What a change to reruns a script, as cargo's rule has it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Watched {
    /// Whole paths, in the order printed and each once: a file, or a directory meaning
    /// everything below it (links followed). cargo reruns a script for a path that is
    /// missing on every build, until it is there. With `whole_package`, the package's
    /// directory alone, less what [`skipped`] leaves out.
    pub paths: Vec<PathBuf>,
    /// The script printed neither `rerun-if-changed` nor `rerun-if-env-changed`: any change to
    /// the package's files reruns it. (For a package from a registry or git, whose files
    /// don't change, cargo watches its version alone.)
    pub whole_package: bool,
    /// `rerun-if-env-changed`, in the order printed and each once. cargo compares the value
    /// `[env]` gives a variable when it has it, else its own environment's.
    pub env: Vec<String>,
}

/// What `output` makes cargo watch, for the package whose directory is `package_dir`.
pub fn watched(output: &Output, package_dir: &Path) -> Watched {
    if output.rerun_if_changed.is_empty() && output.rerun_if_env_changed.is_empty() {
        let paths = vec![package_dir.to_owned()];
        return Watched { paths, whole_package: true, env: Vec::new() };
    }
    let mut paths: Vec<PathBuf> = Vec::new();
    for path in &output.rerun_if_changed {
        let whole = package_dir.join(path);
        if !paths.contains(&whole) {
            paths.push(whole);
        }
    }
    let mut env: Vec<String> = Vec::new();
    for name in &output.rerun_if_env_changed {
        if !env.contains(name) {
            env.push(name.clone());
        }
    }
    Watched { paths, whole_package: false, env }
}

/// Whether cargo's rule for the whole package leaves out the directory at `relative` (a path
/// below the package's directory) and everything below it: the package's own `target`, and a
/// directory that holds a `Cargo.toml` (`has_manifest`), which is another package. Nothing
/// else is left out: the manifest's `include` and `exclude` and a git checkout's ignore files
/// only narrow cargo's listing, and watching more than cargo is only slower.
pub fn skipped(relative: &Path, has_manifest: bool) -> bool {
    let mut parts = relative.components().filter(|it| *it != Component::CurDir);
    let Some(first) = parts.next() else { return false };
    has_manifest || (first == Component::Normal(OsStr::new("target")) && parts.next().is_none())
}

#[cfg(test)]
mod tests;

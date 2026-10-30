//! Where a command runs and what of the machine it sends with it: the checkout's root, the
//! config files cargo reads outside it, the environment and whether diagnostics are colored.
//! Nothing here reads a manifest: the checkout is found with `std::fs::metadata` alone, and the
//! daemon's load is the one load.

use std::{
    ffi::OsString,
    io,
    path::{Component, Path, PathBuf},
};

use blaze_api::{Cargo, Targets};

use super::args::Invocation;

/// Where a command runs.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Place {
    /// The directory the command ran in, absolute.
    pub cwd: PathBuf,
    /// The root manifest, as cargo finds it.
    pub manifest: PathBuf,
    /// The checkout: what the view is of.
    pub root: PathBuf,
}

/// cargo's `root_manifest` (`util/command_prelude.rs`) and `find_root_manifest_for_wd`
/// (`util/important_paths.rs`), with their errors. `script` is `-Zscript`, under which a `.rs`
/// file or one without an extension is a manifest too.
pub fn manifest(cwd: &Path, manifest_path: Option<&str>, script: bool) -> Result<PathBuf, String> {
    if let Some(typed) = manifest_path {
        let path = normalize(&cwd.join(typed));
        let embedded = is_embedded(&path);
        if !path.ends_with("Cargo.toml") && !embedded {
            return Err("the manifest-path must be a path to a Cargo.toml file".to_owned());
        }
        let Ok(found) = std::fs::metadata(&path) else {
            return Err(format!("manifest path `{typed}` does not exist"));
        };
        if found.is_dir() {
            return Err(format!("manifest path `{typed}` is a directory but expected a file"));
        }
        if embedded && !script {
            return Err(format!("embedded manifest `{}` requires `-Zscript`", path.display()));
        }
        return Ok(path);
    }
    // One stat per directory on the way up; `cargo.toml` is looked for only once nothing was found.
    if let Some(dir) = cwd.ancestors().find(|dir| std::fs::metadata(dir.join("Cargo.toml")).is_ok()) {
        return Ok(dir.join("Cargo.toml"));
    }
    let lowercase = cwd.ancestors().any(|dir| std::fs::metadata(dir.join("cargo.toml")).is_ok());
    let said = format!("could not find `Cargo.toml` in `{}` or any parent directory", cwd.display());
    match lowercase {
        true => Err(format!("{said}, but found cargo.toml please try to rename it to Cargo.toml")),
        false => Err(said),
    }
}

/// cargo's `toml::is_embedded`: a single-file package.
fn is_embedded(path: &Path) -> bool {
    let ext = path.extension();
    (ext.is_none() || ext.is_some_and(|it| it == "rs")) && std::fs::metadata(path).is_ok_and(|it| it.is_file())
}

/// cargo-util's `paths::normalize_path`: `.` dropped and `..` taken back, without the disk.
pub fn normalize(path: &Path) -> PathBuf {
    let mut normal = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(..) | Component::RootDir => normal.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normal.pop();
            }
            Component::Normal(part) => normal.push(part),
        }
    }
    normal
}

/// Whether `-Zscript` is among the arguments.
pub fn script(args: &[String]) -> bool {
    let mut words = args.iter();
    while let Some(word) = words.next() {
        let flag = match word.strip_prefix("-Z") {
            Some("") => words.next().map(String::as_str),
            Some(rest) => Some(rest),
            None => None,
        };
        if flag == Some("script") {
            return true;
        }
    }
    false
}

/// The checkout of `manifest`: the root of the git repository it is in (a workspace's root and
/// its members are in the same one), else the highest directory above it with a `Cargo.toml`,
/// which the workspace's root manifest is in whichever of them it is.
pub fn root(manifest: &Path) -> PathBuf {
    let dir = manifest.parent().unwrap_or(manifest);
    if let Some(repository) = dir.ancestors().find(|it| std::fs::metadata(it.join(".git")).is_ok()) {
        return repository.to_owned();
    }
    let highest = dir.ancestors().filter(|it| std::fs::metadata(it.join("Cargo.toml")).is_ok()).last();
    highest.unwrap_or(dir).to_owned()
}

pub fn find(cwd: &Path, manifest_path: Option<&str>, script: bool) -> Result<Place, String> {
    let manifest = manifest(cwd, manifest_path, script)?;
    let root = root(&manifest);
    Ok(Place { cwd: cwd.to_owned(), manifest, root })
}

/// The files of cargo's configuration outside the checkout, which the view does not have:
/// `.cargo/config` and `.cargo/config.toml` of each directory from `cwd` up that is not in
/// the checkout, `$CARGO_HOME`'s, and each `--config` that names a file outside it
/// (`GlobalContext::walk_tree`, `cli_args_as_table`). Never `credentials.toml`.
///
/// A directory without `.cargo` costs one stat; a file is read without one, its absence being the
/// read's `NotFound`.
pub fn config_files(place: &Place, configs: &[String], cargo_home: Option<&Path>) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    let mut add = |path: PathBuf| -> Result<(), String> {
        if path.starts_with(&place.root) {
            return Ok(());
        }
        if files.iter().any(|(it, _)| Path::new(it) == path) {
            return Ok(());
        }
        match std::fs::read(&path) {
            Ok(bytes) => files.push((utf8(&path)?, bytes)),
            Err(error) if matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::IsADirectory | io::ErrorKind::NotADirectory) => {}
            Err(error) => return Err(format!("reading {}: {error}", path.display())),
        }
        Ok(())
    };
    for dir in place.cwd.ancestors().filter(|it| !it.starts_with(&place.root)) {
        let cargo = dir.join(".cargo");
        if std::fs::metadata(&cargo).is_ok_and(|it| it.is_dir()) {
            add(cargo.join("config"))?;
            add(cargo.join("config.toml"))?;
        }
    }
    if let Some(home) = cargo_home {
        add(home.join("config"))?;
        add(home.join("config.toml"))?;
    }
    for config in configs.iter().filter(|it| !it.is_empty()) {
        add(normalize(&place.cwd.join(config)))?;
    }
    Ok(files)
}

/// A path as the API carries it.
pub fn utf8(path: &Path) -> Result<String, String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("the path {} is not UTF-8, which a request to the daemon can't carry", path.display()))
}

/// The command's environment, sorted by name. A variable that is not UTF-8 is refused by its
/// name alone: its value is never shown.
pub fn env(vars: impl Iterator<Item = (OsString, OsString)>) -> Result<Vec<(String, String)>, String> {
    let mut env = Vec::with_capacity(vars.size_hint().0);
    for (name, value) in vars {
        let named = name.to_string_lossy().into_owned();
        let (Ok(name), Ok(value)) = (name.into_string(), value.into_string()) else {
            return Err(format!(
                "the environment variable `{named}` is not UTF-8, which a request to the daemon can't carry"
            ));
        };
        env.push((name, value));
    }
    env.sort_unstable();
    Ok(env)
}

/// Whether what cargo writes to stderr is colored: `--color` (the last one), else
/// `CARGO_TERM_COLOR`, else `auto`, which is anstream's choice for a stream that is a
/// `terminal` or not (`NO_COLOR`, `CLICOLOR_FORCE`, `CLICOLOR`, `TERM`, `CI`).
pub fn color(flag: Option<&str>, var: impl Fn(&str) -> Option<String>, terminal: bool) -> Result<bool, String> {
    let when = match flag {
        Some(flag) => Some(flag.to_owned()),
        None => var("CARGO_TERM_COLOR"),
    };
    match when.as_deref() {
        Some("always") => Ok(true),
        Some("never") => Ok(false),
        Some("auto") | None => Ok(auto(&var, terminal)),
        Some(_) => Err("`CARGO_TERM_COLOR` must be auto, always, or never".to_owned()),
    }
}

/// anstream's `AutoStream::choice`.
fn auto(var: &impl Fn(&str) -> Option<String>, terminal: bool) -> bool {
    let clicolor = var("CLICOLOR").map(|it| it != "0");
    if var("NO_COLOR").is_some_and(|it| !it.is_empty()) {
        return false;
    }
    if var("CLICOLOR_FORCE").is_some_and(|it| it != "0") {
        return true;
    }
    if clicolor == Some(false) {
        return false;
    }
    let term = cfg!(windows) || var("TERM").is_some_and(|it| it != "dumb");
    terminal && (term || clicolor == Some(true) || var("CI").is_some())
}

/// What the command sends of itself (`Targets::cargo`).
pub fn cargo(asked: &Invocation, place: &Place, env: Vec<(String, String)>, files: Vec<(String, Vec<u8>)>, color: bool) -> Result<Cargo, String> {
    Ok(Cargo {
        command: asked.command().name().to_owned(),
        args: asked.args.clone(),
        root: utf8(&place.root)?,
        cwd: utf8(&place.cwd)?,
        env,
        files,
        color,
    })
}

/// The six fields of `Targets` the flags say, and the whole command line.
pub fn targets(asked: &Invocation, cargo: Cargo) -> Targets {
    let flags = &asked.flags;
    Targets {
        packages: flags.packages.clone(),
        profile: asked.profile(),
        target: flags.targets.first().cloned(),
        features: flags.features.clone(),
        all_features: flags.all_features,
        no_default_features: flags.no_default_features,
        cargo: Some(cargo),
    }
}

#[cfg(test)]
mod tests;

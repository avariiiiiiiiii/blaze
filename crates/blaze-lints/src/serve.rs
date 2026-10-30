//! What the daemon's checker answers a lint request with: for each crate of the packages
//! asked of, its diagnostics as rustc would write them in each build of it cargo makes, the
//! one without its tests and the one with them. The client says which builds there are and
//! writes them as cargo does.

use std::path::{Path, PathBuf};

use base_db::{Crate, Db, FileId};
use blaze_diagnostics::{
    emit::{self, Sources, Style},
    model::{Child, Diag},
};
use rustc_hash::FxHashSet;
use serde::{Deserialize, Serialize};

use crate::{
    check::{Found, Setup},
    conf::{self, Conf},
    level::CommandLine,
};

/// A lint request: the packages whose crates are linted, and how diagnostics are written.
#[derive(Serialize, Deserialize, Debug, Default)]
pub struct Ask {
    pub packages: Vec<Package>,
    /// `--error-format=short`, as `--message-format=short` and `json-diagnostic-short` ask.
    pub short: bool,
    /// In colour: cargo asks rustc for it when it writes to a terminal, or JSON asks for it.
    pub ansi: bool,
    /// The terminal's columns, when cargo would pass them to rustc.
    pub width: Option<usize>,
}

/// A package to lint, and what rustc would be given for it.
#[derive(Serialize, Deserialize, Debug, Default)]
pub struct Package {
    pub name: String,
    pub manifest_dir: PathBuf,
    /// The lint flags rustc would be given, in its order: the package's `[lints]`,
    /// `RUSTFLAGS`, then `cargo clippy -- …`'s.
    pub flags: Vec<String>,
    /// `CLIPPY_CONF_DIR`.
    pub conf_dir: Option<PathBuf>,
    /// The package's `rust-version`, which clippy takes as its MSRV.
    pub rust_version: Option<String>,
}

/// What the checker found.
#[derive(Serialize, Deserialize, Debug, Default)]
pub struct Answer {
    pub crates: Vec<Linted>,
    /// What stopped a package from being linted, as clippy says it, by package.
    pub failed: Vec<(String, String)>,
}

/// A crate linted: its root file, and its diagnostics in a build without its tests and in one
/// with them (`--test`), when it has them.
#[derive(Serialize, Deserialize, Debug, Default)]
pub struct Linted {
    pub package: String,
    pub root: PathBuf,
    pub plain: Vec<Message>,
    pub test: Option<Vec<Message>>,
}

/// A diagnostic as rustc writes it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// Its text as rustc writes it to stderr, the empty line after it included.
    pub rendered: String,
    /// rustc's JSON of it, with that text as `rendered`.
    pub json: String,
    pub error: bool,
    /// Whether `cargo clippy --fix` would apply it.
    pub fixable: bool,
}

/// Lints the crates of `crates` the packages of `ask` have. Files are named from `root`, the
/// workspace's, when they are below it; `path_of` is where a file is.
pub fn answer(
    db: &Db,
    crates: &[Crate],
    root: &Path,
    path_of: &impl Fn(FileId) -> Option<PathBuf>,
    ask: &Ask,
) -> Answer {
    let name_of = |file: FileId| -> Option<String> {
        let path = path_of(file)?;
        Some(match path.strip_prefix(root) {
            Ok(relative) => relative.to_string_lossy().into_owned(),
            Err(_) => path.to_string_lossy().into_owned(),
        })
    };
    let style = Style { short: ask.short, ansi: ask.ansi, width: ask.width.unwrap_or(emit::WIDTH), ui_testing: false };
    let read = |name: &str| std::fs::read_to_string(root.join(name)).ok();
    let mut sources = Sources::default();
    let mut answer = Answer::default();
    let mut setups: Vec<Option<Setup>> = Vec::new();
    for package in &ask.packages {
        match setup(package) {
            Ok(setup) => setups.push(Some(setup)),
            Err(why) => {
                answer.failed.push((package.name.clone(), why));
                setups.push(None);
            }
        }
    }
    let has_tests = |krate: Crate| {
        hir::attach_db(db, || {
            hir::Crate::from(krate).cfg(db).check_atom(&cfg::CfgAtom::Flag(intern::sym::test))
        })
    };
    // A crate built for its tests as well is linted once, as that build: what it finds outside
    // `cfg(test)` is what the plain build finds.
    let tested: FxHashSet<FileId> = crates
        .iter()
        .filter(|&&it| has_tests(it))
        .map(|&it| hir::Crate::from(it).root_file(db))
        .collect();
    for &krate in crates {
        let has_tests = has_tests(krate);
        if !has_tests && tested.contains(&hir::Crate::from(krate).root_file(db)) {
            continue;
        }
        let env = krate.env(db);
        let Some(dir) = env.get("CARGO_MANIFEST_DIR") else { continue };
        let Some(at) = ask.packages.iter().position(|it| it.manifest_dir == Path::new(&dir)) else {
            continue;
        };
        let Some(setup) = &setups[at] else { continue };
        let hir_crate = hir::Crate::from(krate);
        let Some(root_path) = path_of(hir_crate.root_file(db)) else { continue };
        let found = crate::check(db, hir_crate, setup, &name_of);
        let plain: Vec<&Found> = found.iter().filter(|it| !it.test_only).collect();
        let test = has_tests.then(|| written(found.iter(), &mut sources, &read, style));
        answer.crates.push(Linted {
            package: ask.packages[at].name.clone(),
            root: root_path,
            plain: written(plain.into_iter(), &mut sources, &read, style),
            test,
        });
    }
    answer
}

fn setup(package: &Package) -> Result<Setup, String> {
    let command_line = CommandLine::parse(&package.flags)?;
    let path = conf::find(package.conf_dir.as_deref(), &package.manifest_dir);
    let conf = Conf::load(path, package.rust_version.as_deref())?;
    Ok(Setup { command_line, conf })
}

/// The diagnostics of one build, as rustc writes them: what is said once a build is said with
/// the first diagnostic that has it, and a diagnostic said again is not. The files they point
/// into that `sources` has not read yet are read by `read`.
fn written<'a>(
    found: impl Iterator<Item = &'a Found>,
    sources: &mut Sources,
    read: impl Fn(&str) -> Option<String>,
    style: Style,
) -> Vec<Message> {
    let found: Vec<&Found> = found.collect();
    for name in sources.unread(found.iter().map(|it| &it.diagnostic)) {
        sources.add(name, read(name));
    }
    let mut once: FxHashSet<Child> = FxHashSet::default();
    let mut seen = FxHashSet::default();
    let mut messages = Vec::new();
    for Found { diagnostic, .. } in found {
        // clone: a diagnostic of the crate, once per build it is written in
        let mut diagnostic: Diag = diagnostic.clone();
        diagnostic.children.retain(|child| !child.once || once.insert(child.clone()));
        let rendered = emit::written(&diagnostic, sources, style);
        if !seen.insert(rendered.clone()) {
            continue;
        }
        let json = emit::json_rendered(&diagnostic, sources, rendered.clone());
        messages.push(Message {
            json: if style.ansi { json } else { cargo_order(&json) },
            rendered,
            error: diagnostic.is_error(),
            fixable: diagnostic.is_fixable(),
        });
    }
    messages
}

/// rustc's JSON line `line` as cargo writes it again when it takes the colours out of
/// `rendered` (`on_stderr_line_inner` under `MessageFormat::Json`): `rendered`, every other key
/// but `code` sorted at every depth, then `code`. With colours cargo passes rustc's line as it
/// is.
fn cargo_order(line: &str) -> String {
    #[derive(Deserialize, Serialize)]
    struct CompilerMessage {
        rendered: String,
        #[serde(flatten)]
        other: std::collections::BTreeMap<String, serde_json::Value>,
        code: Option<serde_json::Value>,
    }
    let mut message: CompilerMessage = serde_json::from_str(line)
        .unwrap_or_else(|error| panic!("blaze-lints: rustc's JSON of a diagnostic: {error}"));
    message.other.values_mut().for_each(sort_keys);
    message.code.iter_mut().for_each(sort_keys);
    serde_json::to_string(&message)
        .unwrap_or_else(|error| panic!("blaze-lints: a diagnostic as cargo writes it: {error}"))
}

/// Sorts the keys of every object in `value`, as serde_json's `Map` without `preserve_order`
/// has them, whatever features the build unifies.
fn sort_keys(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<(String, serde_json::Value)> = std::mem::take(map).into_iter().collect();
            entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            for (key, mut it) in entries {
                sort_keys(&mut it);
                map.insert(key, it);
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(sort_keys),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_diagnostic_is_written_as_cargo_writes_it_again() {
        let line = concat!(
            r#"{"$message_type":"diagnostic","message":"m","code":{"code":"clippy::x","explanation":null},"#,
            r#""level":"warning","spans":[{"file_name":"a.rs","byte_start":0,"expansion":null}],"children":[],"rendered":"r"}"#,
        );
        let expected = concat!(
            r#"{"rendered":"r","$message_type":"diagnostic","children":[],"level":"warning","message":"m","#,
            r#""spans":[{"byte_start":0,"expansion":null,"file_name":"a.rs"}],"code":{"code":"clippy::x","explanation":null}}"#,
        );
        assert_eq!(cargo_order(line), expected);
    }
}

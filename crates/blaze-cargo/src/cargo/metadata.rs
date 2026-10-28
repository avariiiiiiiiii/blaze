//! The workspace as `cargo metadata --format-version 1` prints it, read by blaze-manifest as
//! cargo 1.99.0 reads it and resolved by blaze-resolve, with no cargo run (docs/resolve.md).

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use blaze_manifest::diagnostic::ManifestDiagnostic;
use blaze_manifest::metadata::{export_no_deps, root_manifest};
use blaze_manifest::{Config, Context, Workspace};
use cargo_metadata::Metadata;

/// The workspace found from the current directory, or from `manifest_path`, with cargo's
/// `--config` arguments `config`. A workspace cargo refuses ends the command as cargo ends:
/// its message, status 101.
pub fn no_deps(manifest_path: Option<&Path>, config: &[String]) -> Metadata {
    let cwd = std::env::current_dir().unwrap_or_default();
    let config = Config::from_process().and_then(|mut process| {
        process.start("metadata", config)?;
        Ok(process)
    });
    match config.and_then(|config| read(config, manifest_path)) {
        Ok(metadata) => metadata,
        Err(err) => {
            refused(&err, &cwd);
            std::process::exit(101)
        }
    }
}

fn read(config: Config, manifest_path: Option<&Path>) -> anyhow::Result<Metadata> {
    let ctx = Context::new(config);
    let root = root_manifest(manifest_path, &ctx)?;
    let ws = Workspace::new(&root, &ctx)?;
    let json = serde_json::to_value(export_no_deps(&ws))?;
    serde_json::from_value(json).context("blaze's own reading of the workspace is not cargo's form")
}

/// How the daemon's loads read a workspace for the build `selection` names: blaze's own
/// reading and resolution, the graph that of the build, its features unified over the other
/// members only as `resolver.feature-unification` says. `BLAZE_OWN_RESOLVE=0` leaves both to
/// cargo.
pub fn own(selection: &crate::plan::Selection) -> Option<project_model::OwnMetadata> {
    if std::env::var_os("BLAZE_OWN_RESOLVE").is_some_and(|it| it == "0") {
        return None;
    }
    let selection = selection.clone();
    let root = |manifest: &project_model::ManifestPath| {
        let manifest: &Path = manifest.as_ref();
        let cwd = manifest.parent().unwrap_or(Path::new("/"));
        let root = blaze_metadata::workspace_root(cwd, std::env::vars().collect(), manifest)?;
        let root = paths::AbsPathBuf::assert_utf8(root);
        Ok(project_model::ManifestPath::try_from(root).expect("a manifest is a file"))
    };
    let metadata = move |manifest: &project_model::ManifestPath,
                         triple: Option<&str>,
                         cfg: &[cfg::CfgAtom],
                         env: &rustc_hash::FxHashMap<String, Option<String>>| {
        // The build's members are those cargo would pick where it was asked, as `-p`,
        // `--workspace` and the default members choose them from there.
        let loaded: &Path = manifest.as_ref();
        let (cwd, manifest) = match &selection.asked_in {
            Some(asked_in) if asked_in.is_dir() => (asked_in.clone(), None),
            Some(asked_in) => {
                (asked_in.parent().unwrap_or(Path::new("/")).to_owned(), Some(asked_in.as_path()))
            }
            None => (loaded.parent().unwrap_or(Path::new("/")).to_owned(), Some(loaded)),
        };
        let mut vars: std::collections::BTreeMap<String, String> = std::env::vars().collect();
        for (key, value) in env {
            match value {
                Some(value) => vars.insert(key.clone(), value.clone()),
                None => vars.remove(key),
            };
        }
        let cfg: Vec<cargo_platform::Cfg> = cfg.iter().filter_map(platform_cfg).collect();
        let triple = triple.context("no target triple")?;
        let asked = blaze_metadata::Asked {
            cwd: &cwd,
            env: vars.into_iter().collect(),
            manifest_path: manifest,
            config: &selection.config,
            features: &selection.features,
            all_features: selection.all_features,
            no_default_features: selection.no_default_features,
            build: Some(blaze_metadata::Build {
                packages: &selection.packages,
                workspace: selection.workspace,
                excludes: &selection.excludes,
                dev_units: selection.dev_deps,
            }),
            platform: Some(blaze_metadata::TargetPlatform { triple, cfg: &cfg }),
            offline: false,
            store: None,
            rustc_version: Some(crate::cargo::toolchain::ours_release()),
        };
        let json = blaze_metadata::metadata(&asked)?;
        serde_json::from_str(&json).context("blaze's own metadata is not cargo's form")
    };
    Some(project_model::OwnMetadata {
        root: std::sync::Arc::new(root),
        metadata: std::sync::Arc::new(metadata),
        library: std::sync::Arc::new(library),
    })
}

/// What a library's metadata is asked with: its manifest, the features, the target, whether
/// `--no-deps`, and the variables set or unset for it.
type LibraryAsked = (PathBuf, String, Option<String>, bool, Vec<(String, Option<String>)>);

/// `cargo metadata` of the standard library's workspace at `manifest` (a toolchain's
/// `library/Cargo.toml`), as rust-analyzer's `Sysroot::load_workspace` asked cargo for it, with
/// cargo told it is a nightly and the toolchain's lock file never written: read by
/// blaze-manifest, resolved by blaze-metadata, its crates.io packages those the toolchain
/// vendors (`library/vendor`, which its `.cargo/config.toml` replaces crates.io with). The
/// toolchain's source does not change, so each way of asking is answered once per process.
fn library(
    manifest: &project_model::ManifestPath,
    features: &project_model::CargoFeatures,
    triple: Option<&str>,
    no_deps: bool,
    env: &rustc_hash::FxHashMap<String, Option<String>>,
) -> anyhow::Result<Metadata> {
    static READ: std::sync::OnceLock<
        std::sync::Mutex<rustc_hash::FxHashMap<LibraryAsked, Metadata>>,
    > = std::sync::OnceLock::new();
    let manifest: &Path = manifest.as_ref();
    let mut set: Vec<(String, Option<String>)> =
        env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    set.sort();
    let key =
        (manifest.to_owned(), format!("{features:?}"), triple.map(str::to_owned), no_deps, set);
    let read = READ.get_or_init(Default::default);
    if let Some(metadata) = read.lock().unwrap_or_else(|it| it.into_inner()).get(&key) {
        return Ok(metadata.clone());
    }
    let metadata = library_read(manifest, features, triple, no_deps, env)?;
    read.lock().unwrap_or_else(|it| it.into_inner()).insert(key, metadata.clone());
    Ok(metadata)
}

fn library_read(
    manifest: &Path,
    features: &project_model::CargoFeatures,
    triple: Option<&str>,
    no_deps: bool,
    env: &rustc_hash::FxHashMap<String, Option<String>>,
) -> anyhow::Result<Metadata> {
    // cargo is run in the library's directory, whose `.cargo/config.toml` is read.
    let cwd = manifest.parent().context("a manifest with no directory")?;
    let mut vars: std::collections::BTreeMap<String, String> = std::env::vars().collect();
    for (key, value) in env {
        match value {
            Some(value) => vars.insert(key.clone(), value.clone()),
            None => vars.remove(key),
        };
    }
    let (features, all_features, no_default_features) = match features {
        project_model::CargoFeatures::All => (Vec::new(), true, false),
        project_model::CargoFeatures::Selected { features, no_default_features } => {
            (features.clone(), false, *no_default_features)
        }
    };
    // `--filter-platform`: the target's `cfg`, as blaze answers it for its release.
    let cfg = match triple {
        None => Vec::new(),
        Some(triple) => {
            let printed = crate::answers::ours_said(&["--print", "cfg", "--target", triple])?;
            let cfg = printed.lines().filter(|it| !it.is_empty()).map(|it| {
                use std::str::FromStr;
                cargo_platform::Cfg::from_str(it).with_context(|| format!("the cfg {it}"))
            });
            cfg.collect::<anyhow::Result<Vec<_>>>()?
        }
    };
    let asked = blaze_metadata::Asked {
        cwd,
        env: vars.into_iter().collect(),
        manifest_path: Some(manifest),
        config: &[],
        features: &features,
        all_features,
        no_default_features,
        build: None,
        platform: triple.map(|triple| blaze_metadata::TargetPlatform { triple, cfg: &cfg }),
        offline: false,
        store: None,
        rustc_version: Some(crate::cargo::toolchain::ours_release()),
    };
    let json = match no_deps {
        true => blaze_metadata::metadata_no_deps(&asked)?,
        false => blaze_metadata::metadata_locked(&asked)?,
    };
    serde_json::from_str(&json).context("blaze's own metadata of the library is not cargo's form")
}

/// The programs a build of `selection` links of `targets`, asked for in `cwd` (of the package
/// of `manifest`, when there is one), planned with blaze's own resolution for the triple
/// `target` or the host's: what cargo's unit graph has. The plan is kept in `kept_in` and
/// used again while what it was made of is unchanged ([`Plan::made_of`]): resolving the
/// workspace is most of what the client of a no-op build does.
pub fn programs(
    selection: &crate::plan::Selection,
    cwd: &Path,
    manifest: Option<&Path>,
    targets: blaze_metadata::Targets<'_>,
    kept_in: &Path,
) -> anyhow::Result<Vec<blaze_metadata::Program>> {
    let key = plan_key(selection, cwd, manifest, targets);
    let path = kept_in.join(format!("{key}.json"));
    let kept = std::fs::read(&path).ok().and_then(|it| serde_json::from_slice::<Plan>(&it).ok());
    if let Some(kept) = kept.filter(|it| it.key == key && it.is_current()) {
        return Ok(kept.programs);
    }
    let started = std::time::SystemTime::now();
    let (json, programs) = resolved_programs(selection, cwd, manifest, targets)?;
    let plan = Plan { key, made_of: Plan::made_of(&json, cwd), programs };
    // A file written while the plan was made may be newer than what was read of it.
    let settled = plan.made_of.iter().all(|(_, stamp)| stamp.is_none_or(|it| it.0 < started));
    if settled && std::fs::create_dir_all(kept_in).is_ok() {
        let partial = path.with_extension(format!("{}.partial", std::process::id()));
        let written = serde_json::to_vec(&plan).ok().map(|it| std::fs::write(&partial, it));
        if written.is_some_and(|it| it.is_ok()) {
            _ = std::fs::rename(&partial, &path);
        }
    }
    Ok(plan.programs)
}

/// What the plan of a build is asked with, apart from the files it reads: the selection and
/// targets, where it is asked, the variables cargo's configuration and the toolchain are read
/// from, and the rustc the platform is asked of.
fn plan_key(
    selection: &crate::plan::Selection,
    cwd: &Path,
    manifest: Option<&Path>,
    targets: blaze_metadata::Targets<'_>,
) -> String {
    use sha2::{Digest as _, Sha256};
    let tests = targets.test.map(|it| (it.lib, it.test));
    let named = (targets.bin, targets.example, targets.lib, tests);
    let mut vars: Vec<(String, String)> = std::env::vars()
        .filter(|(name, _)| name.starts_with("CARGO") || name.starts_with("RUST") || name == "HOME")
        .collect();
    vars.sort_unstable();
    let asked = serde_json::json!({
        "selection": selection,
        "cwd": cwd,
        "manifest": manifest,
        "targets": format!("{named:?}"),
        "vars": vars,
        "release": crate::cargo::toolchain::ours_says(),
    });
    let digest = Sha256::digest(asked.to_string().as_bytes());
    digest[..8].iter().map(|it| format!("{it:02x}")).collect()
}

/// When a file was last written and its length, or `None` where there is none.
type Stamp = Option<(std::time::SystemTime, u64)>;

fn stamp(path: &Path) -> Stamp {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// A build's programs as [`programs`] keeps them, with the files they were planned from.
#[derive(serde::Serialize, serde::Deserialize)]
struct Plan {
    key: String,
    made_of: Vec<(PathBuf, Stamp)>,
    programs: Vec<blaze_metadata::Program>,
}

impl Plan {
    fn is_current(&self) -> bool {
        self.made_of.iter().all(|(path, was)| stamp(path) == *was)
    }

    /// What a plan made with the metadata `json`, asked for in `cwd`, is made of: the lock
    /// file and root manifest of the workspace; of each package not from a registry or git,
    /// its manifest and the directories cargo finds its targets in, and the directory it is
    /// in, which a `members` glob reads; and the configuration files cargo reads from `cwd` up
    /// and in cargo's home.
    fn made_of(json: &str, cwd: &Path) -> Vec<(PathBuf, Stamp)> {
        let metadata: serde_json::Value = serde_json::from_str(json).unwrap_or_default();
        let mut files = Vec::new();
        if let Some(root) = metadata["workspace_root"].as_str().map(Path::new) {
            files.extend([root.join("Cargo.lock"), root.join("Cargo.toml")]);
        }
        let packages = metadata["packages"].as_array().into_iter().flatten();
        let local = packages.filter(|it| it["source"].is_null());
        for manifest in local.filter_map(|it| it["manifest_path"].as_str()).map(Path::new) {
            files.push(manifest.to_owned());
            let Some(dir) = manifest.parent() else { continue };
            files.extend(dir.parent().map(Path::to_owned));
            files.push(dir.join("src"));
            for found in ["src/bin", "examples", "tests", "benches"].map(|it| dir.join(it)) {
                let entries = std::fs::read_dir(&found).into_iter().flatten().flatten();
                files.extend(entries.map(|it| it.path()).filter(|it| it.is_dir()));
                files.push(found);
            }
        }
        for dir in cwd.ancestors() {
            files.extend([".cargo/config", ".cargo/config.toml"].map(|it| dir.join(it)));
        }
        let home = std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|it| PathBuf::from(it).join(".cargo")));
        if let Some(home) = home {
            files.extend(["config", "config.toml"].map(|it| home.join(it)));
        }
        files.sort_unstable();
        files.dedup();
        files
            .into_iter()
            .map(|it| {
                let stamp = stamp(&it);
                (it, stamp)
            })
            .collect()
    }
}

/// [`programs`] resolved: the metadata of the build and its programs.
fn resolved_programs(
    selection: &crate::plan::Selection,
    cwd: &Path,
    manifest: Option<&Path>,
    targets: blaze_metadata::Targets<'_>,
) -> anyhow::Result<(String, Vec<blaze_metadata::Program>)> {
    {
        let asked = |args: &[&str]| crate::answers::ours_said(args);
        let triple = match &selection.target {
            Some(triple) => triple.clone(),
            None => {
                let told = asked(&["-vV"])?;
                let host = told.lines().find_map(|it| it.strip_prefix("host: "));
                host.context("rustc names no host")?.to_owned()
            }
        };
        let printed = asked(&["--print", "cfg", "--target", &triple])?;
        let cfg = printed.lines().filter(|it| !it.is_empty()).map(|it| {
            use std::str::FromStr;
            cargo_platform::Cfg::from_str(it).with_context(|| format!("the cfg {it}"))
        });
        let cfg = cfg.collect::<anyhow::Result<Vec<_>>>()?;
        let asked = blaze_metadata::Asked {
            cwd,
            env: std::env::vars().collect(),
            manifest_path: manifest,
            config: &selection.config,
            features: &selection.features,
            all_features: selection.all_features,
            no_default_features: selection.no_default_features,
            build: Some(blaze_metadata::Build {
                packages: &selection.packages,
                workspace: selection.workspace,
                excludes: &selection.excludes,
                dev_units: selection.dev_deps,
            }),
            platform: Some(blaze_metadata::TargetPlatform { triple: &triple, cfg: &cfg }),
            offline: false,
            store: None,
            rustc_version: Some(crate::cargo::toolchain::ours_release()),
        };
        blaze_metadata::programs(&asked, targets)
    }
}

/// The unit graph of the build `selection` names, asked for in `cwd` (of the package of
/// `manifest`, when there is one): what `cargo build --unit-graph -Zunstable-options` prints
/// for it, made by blaze-units from blaze's own resolution with no cargo run. `triple` and
/// `cfg` are the platform built for; `env` the variables of whoever asks, where they are not
/// this process's. A build blaze-units cannot make the graph of is an error naming why.
pub fn units(
    selection: &crate::plan::Selection,
    cwd: &Path,
    manifest: Option<&Path>,
    asked: &crate::plan::Asked<'_>,
) -> anyhow::Result<blaze_units::UnitGraph> {
    if let Some(flag) = selection.unstable.first() {
        anyhow::bail!("blaze's unit graph is not made for cargo's unstable flag `-Z {flag}`");
    }
    let mut vars: std::collections::BTreeMap<String, String> = std::env::vars().collect();
    for (key, value) in asked.env {
        match value {
            Some(value) => vars.insert(key.clone(), value.clone()),
            None => vars.remove(key),
        };
    }
    let cfg: Vec<cargo_platform::Cfg> = asked.cfg.iter().filter_map(platform_cfg).collect();
    let built_for = blaze_units::Platformed { triple: asked.triple.to_owned(), cfg };
    // Build scripts and proc macros are built for the host, which is the platform built for
    // unless `--target` names another.
    let target = selection.target.as_ref().map(|_| built_for.clone());
    let host = match &target {
        None => built_for,
        Some(_) => {
            let said = |args: &[&str]| crate::answers::ours_said(args);
            let told = said(&["-vV"])?;
            let host = told.lines().find_map(|it| it.strip_prefix("host: "));
            let host = host.context("rustc names no host")?.to_owned();
            let printed = said(&["--print", "cfg", "--target", &host])?;
            let cfg = printed.lines().filter(|it| !it.is_empty()).map(|it| {
                use std::str::FromStr;
                cargo_platform::Cfg::from_str(it).with_context(|| format!("the cfg {it}"))
            });
            blaze_units::Platformed { triple: host, cfg: cfg.collect::<anyhow::Result<_>>()? }
        }
    };
    // blaze asks for what `cargo build --profile <profile>` builds, with `--all-targets`
    // when the tests are built too.
    let filter = match selection.dev_deps {
        true => blaze_units::Filter::all_targets(),
        false => blaze_units::Filter::Default { required_features_filterable: true },
    };
    let intent = blaze_units::Intent::Build;
    let platformed = target.as_ref().unwrap_or(&host);
    let asked = blaze_metadata::Asked {
        cwd,
        env: vars.into_iter().collect(),
        manifest_path: manifest,
        config: &selection.config,
        features: &selection.features,
        all_features: selection.all_features,
        no_default_features: selection.no_default_features,
        build: Some(blaze_metadata::Build {
            packages: &selection.packages,
            workspace: selection.workspace,
            excludes: &selection.excludes,
            dev_units: filter.need_dev_deps(intent),
        }),
        platform: Some(blaze_metadata::TargetPlatform {
            triple: &platformed.triple,
            cfg: &platformed.cfg,
        }),
        offline: false,
        store: None,
        rustc_version: Some(crate::cargo::toolchain::ours_release()),
    };
    let units = blaze_metadata::Units {
        intent,
        filter: &filter,
        release: false,
        profile: Some(&selection.profile),
        host: &host,
        target: target.as_ref(),
    };
    blaze_metadata::units(&asked, &units).context("making the build's unit graph")
}

/// The targets the flags `targets` pick (`--lib`, `--tests`, `--bin NAME`, ...), as cargo's
/// `from_raw_arguments` reads them.
fn filter(targets: &[String]) -> blaze_units::Filter {
    let (mut lib, mut all_targets) = (false, false);
    let (mut all_bins, mut all_tests, mut all_examples, mut all_benches) =
        (false, false, false, false);
    let (mut bins, mut tests, mut examples, mut benches) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut flags = targets.iter();
    while let Some(flag) = flags.next() {
        let mut named = |to: &mut Vec<String>| to.extend(flags.next().cloned());
        match flag.as_str() {
            "--lib" => lib = true,
            "--all-targets" => all_targets = true,
            "--bins" => all_bins = true,
            "--tests" => all_tests = true,
            "--examples" => all_examples = true,
            "--benches" => all_benches = true,
            "--bin" => named(&mut bins),
            "--test" => named(&mut tests),
            "--example" => named(&mut examples),
            "--bench" => named(&mut benches),
            _ => {}
        }
    }
    blaze_units::Filter::from_flags(
        lib,
        bins,
        all_bins,
        tests,
        all_tests,
        examples,
        all_examples,
        benches,
        all_benches,
        all_targets,
    )
}

/// The unit graph of `cargo check` of `selection` with the target flags `targets`, asked for
/// in `cwd` (of the package of `manifest`, when there is one), for the triple `selection`
/// names or the host's: the targets the check picks, each checked as a test or not, and what
/// they depend on.
pub fn checked(
    selection: &crate::plan::Selection,
    targets: &[String],
    cwd: &Path,
    manifest: Option<&Path>,
) -> anyhow::Result<blaze_units::UnitGraph> {
    if let Some(flag) = selection.unstable.first() {
        anyhow::bail!("blaze's unit graph is not made for cargo's unstable flag `-Z {flag}`");
    }
    let said = |args: &[&str]| crate::answers::ours_said(args);
    let platformed = |triple: &str| -> anyhow::Result<blaze_units::Platformed> {
        let printed = said(&["--print", "cfg", "--target", triple])?;
        let cfg = printed.lines().filter(|it| !it.is_empty()).map(|it| {
            use std::str::FromStr;
            cargo_platform::Cfg::from_str(it).with_context(|| format!("the cfg {it}"))
        });
        let cfg = cfg.collect::<anyhow::Result<_>>()?;
        Ok(blaze_units::Platformed { triple: triple.to_owned(), cfg })
    };
    let told = said(&["-vV"])?;
    let host = told.lines().find_map(|it| it.strip_prefix("host: "));
    let host = platformed(host.context("rustc names no host")?)?;
    let target = selection.target.as_deref().map(platformed).transpose()?;
    let filter = filter(targets);
    let intent = blaze_units::Intent::Check { test: false };
    let built_for = target.as_ref().unwrap_or(&host);
    let asked = blaze_metadata::Asked {
        cwd,
        env: std::env::vars().collect(),
        manifest_path: manifest,
        config: &selection.config,
        features: &selection.features,
        all_features: selection.all_features,
        no_default_features: selection.no_default_features,
        build: Some(blaze_metadata::Build {
            packages: &selection.packages,
            workspace: selection.workspace,
            excludes: &selection.excludes,
            dev_units: filter.need_dev_deps(intent),
        }),
        platform: Some(blaze_metadata::TargetPlatform {
            triple: &built_for.triple,
            cfg: &built_for.cfg,
        }),
        offline: false,
        store: None,
        rustc_version: told.lines().find_map(|it| it.strip_prefix("release: ")),
    };
    let units = blaze_metadata::Units {
        intent,
        filter: &filter,
        release: false,
        profile: Some(&selection.profile),
        host: &host,
        target: target.as_ref(),
    };
    blaze_metadata::units(&asked, &units).context("making the check's unit graph")
}

/// A `cfg` as rust-analyzer has it, as cargo's platforms read it.
fn platform_cfg(atom: &cfg::CfgAtom) -> Option<cargo_platform::Cfg> {
    use std::str::FromStr;
    match atom {
        cfg::CfgAtom::Flag(name) => cargo_platform::Cfg::from_str(name.as_str()).ok(),
        cfg::CfgAtom::KeyValue { key, value } => {
            cargo_platform::Cfg::from_str(&format!("{key}={:?}", value.as_str())).ok()
        }
    }
}

/// `err` as cargo prints it.
pub fn refused(err: &anyhow::Error, cwd: &Path) {
    if let Some(diagnostic) = err.downcast_ref::<ManifestDiagnostic>() {
        eprintln!("{}", diagnostic.render(cwd));
        return;
    }
    let mut chain = err.chain();
    super::say::error(&chain.next().map(ToString::to_string).unwrap_or_default());
    for cause in chain {
        eprintln!("\nCaused by:");
        for line in cause.to_string().lines() {
            if line.is_empty() { eprintln!() } else { eprintln!("  {line}") }
        }
    }
}

// `expected.json` is what cargo printed on Linux, Linux paths put in a JSON text as they are,
// which a Windows path's `\` is not (blaze-manifest's `tests/fixtures.rs`).
#[cfg(all(test, unix))]
mod tests {
    use std::path::{Path, PathBuf};

    use cargo_metadata::Metadata;

    fn copy_dir(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                copy_dir(&entry.path(), &to.join(entry.file_name()));
            } else {
                std::fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
            }
        }
    }

    /// Each workspace of blaze-manifest's fixtures that cargo reads is, as `cargo_metadata`
    /// gives it to the rest of blaze, what cargo 1.99.0 printed for it.
    #[test]
    fn fixtures_are_what_cargo_metadata_gives() {
        let dir =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../blaze-manifest/tests/fixtures");
        let scratch = std::env::temp_dir().join(format!("blaze-metadata-{}", std::process::id()));
        let home = scratch.join("cargo-home");
        std::fs::create_dir_all(&home).unwrap();
        let home = home.canonicalize().unwrap();
        let mut read = 0;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let fixture = entry.unwrap().path();
            let expected = std::fs::read_to_string(fixture.join("expected.json")).unwrap();
            if expected.contains("\"error\"") {
                continue;
            }
            let root = scratch.join(fixture.file_name().unwrap());
            copy_dir(&fixture, &root);
            let root = root.canonicalize().unwrap();
            let cwd = match std::fs::read_to_string(fixture.join("fixture.json")) {
                Ok(text) => {
                    let spec: serde_json::Value = serde_json::from_str(&text).unwrap();
                    root.join(spec["cwd"].as_str().unwrap_or(""))
                }
                Err(_) => root.clone(),
            };
            let expected = expected
                .replace("$ROOT", &root.display().to_string())
                .replace("$CARGO_HOME", &home.display().to_string());
            let expected: Metadata = serde_json::from_str(&expected).unwrap();
            let env = [("CARGO_HOME".to_owned(), home.display().to_string())];
            let config = blaze_manifest::Config::load(&cwd, env, &[]).unwrap();
            let got = super::read(config, None).unwrap();
            assert_eq!(got, expected, "{}", fixture.display());
            read += 1;
        }
        let _ = std::fs::remove_dir_all(&scratch);
        assert!(read >= 3, "{read} fixtures read");
    }

    /// The features of `shared` in the build of `packages` of #596's workspace, where only
    /// `other` asks for `shared/extra`, with `config` as its `.cargo/config.toml`; or why
    /// blaze refuses the build.
    fn shared_features(config: &str, packages: &[&str]) -> Result<Vec<String>, String> {
        let root = std::env::temp_dir().join(format!(
            "blaze-unification-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let package = |name: &str, deps: &str| {
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n{deps}")
        };
        let files = [
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"app\", \"other\", \"shared\"]\nresolver = \"2\"\n"
                    .to_owned(),
            ),
            (".cargo/config.toml", config.to_owned()),
            ("shared/Cargo.toml", package("shared", "\n[features]\nextra = []\n")),
            (
                "shared/src/lib.rs",
                "pub fn extra() -> bool { cfg!(feature = \"extra\") }\n".to_owned(),
            ),
            (
                "app/Cargo.toml",
                package("app", "\n[dependencies]\nshared = { path = \"../shared\" }\n"),
            ),
            (
                "app/src/main.rs",
                "fn main() { println!(\"extra = {}\", shared::extra()); }\n".to_owned(),
            ),
            (
                "other/Cargo.toml",
                package(
                    "other",
                    "\n[dependencies]\nshared = { path = \"../shared\", features = [\"extra\"] }\n",
                ),
            ),
            ("other/src/lib.rs", "pub fn f() -> bool { shared::extra() }\n".to_owned()),
        ];
        for (path, text) in files {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let root = root.canonicalize().unwrap();
        let home = root.join("cargo-home");
        std::fs::create_dir_all(&home).unwrap();
        let packages: Vec<String> = packages.iter().map(|it| it.to_string()).collect();
        let asked = blaze_metadata::Asked {
            cwd: &root,
            env: vec![("CARGO_HOME".to_owned(), home.display().to_string())],
            manifest_path: None,
            config: &[],
            features: &[],
            all_features: false,
            no_default_features: false,
            build: Some(blaze_metadata::Build {
                packages: &packages,
                workspace: false,
                excludes: &[],
                dev_units: false,
            }),
            platform: Some(blaze_metadata::TargetPlatform {
                triple: "x86_64-unknown-linux-gnu",
                cfg: &[],
            }),
            offline: true,
            store: None,
            rustc_version: Some(crate::cargo::toolchain::ours_release()),
        };
        let made = blaze_metadata::metadata(&asked);
        let _ = std::fs::remove_dir_all(&root);
        let metadata: serde_json::Value =
            serde_json::from_str(&made.map_err(|err| format!("{err:#}"))?).unwrap();
        let nodes = metadata["resolve"]["nodes"].as_array().unwrap();
        let shared =
            nodes.iter().find(|it| it["id"].as_str().unwrap().contains("shared#")).unwrap();
        let features = shared["features"].as_array().unwrap().iter();
        Ok(features.map(|it| it.as_str().unwrap().to_owned()).collect())
    }

    /// A kept plan is used while nothing it was made of changes, and not once a target is
    /// added, a manifest is written or a package is added beside the members.
    #[test]
    fn a_plan_is_kept_until_what_it_read_changes() {
        let root = std::env::temp_dir().join(format!("blaze-plan-{}", std::process::id()));
        let app = root.join("crates/app");
        std::fs::create_dir_all(app.join("src")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = [\"crates/*\"]\n").unwrap();
        std::fs::write(app.join("Cargo.toml"), "[package]\nname = \"app\"\n").unwrap();
        std::fs::write(app.join("src/main.rs"), "fn main() {}\n").unwrap();
        let json = serde_json::json!({
            "workspace_root": root,
            "packages": [
                { "source": null, "manifest_path": app.join("Cargo.toml") },
                { "source": "registry+https://github.com/rust-lang/crates.io-index",
                  "manifest_path": "/nowhere/Cargo.toml" },
            ],
        })
        .to_string();
        let made = || super::Plan {
            key: String::new(),
            made_of: super::Plan::made_of(&json, &app),
            programs: Vec::new(),
        };
        let plan = made();
        assert!(plan.made_of.iter().any(|(it, _)| it == &root.join("crates")));
        assert!(!plan.made_of.iter().any(|(it, _)| it == Path::new("/nowhere/Cargo.toml")));
        assert!(plan.is_current());
        std::fs::create_dir_all(app.join("src/bin")).unwrap();
        assert!(!plan.is_current(), "a new `src/bin`");
        let plan = made();
        std::fs::write(app.join("Cargo.toml"), "[package]\nname = \"app\"\nedition = \"2024\"\n")
            .unwrap();
        assert!(!plan.is_current(), "a manifest written");
        let plan = made();
        std::fs::create_dir_all(root.join("crates/more")).unwrap();
        assert!(!plan.is_current(), "a package beside the members");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `resolver.feature-unification`, as nightly cargo reads it (#596): `workspace` resolves
    /// the features of every member, `selected` (and the setting without
    /// `-Zfeature-unification`) of those built, `package` of each package built alone.
    #[test]
    fn feature_unification_is_cargo_s() {
        let mode = |mode: &str| {
            format!(
                "[unstable]\nfeature-unification = true\n\n[resolver]\nfeature-unification = \"{mode}\"\n"
            )
        };
        let extra = Ok(vec!["extra".to_owned()]);
        assert_eq!(shared_features(&mode("workspace"), &["app"]), extra);
        assert_eq!(shared_features(&mode("selected"), &["app"]), Ok(vec![]));
        assert_eq!(shared_features(&mode("package"), &["app"]), Ok(vec![]));
        assert_eq!(shared_features(&mode("package"), &["other"]), extra);
        assert_eq!(shared_features(&mode("selected"), &["app", "other"]), extra);
        assert_eq!(shared_features("", &["app"]), Ok(vec![]));
        let unflagged = "[resolver]\nfeature-unification = \"workspace\"\n";
        assert_eq!(shared_features(unflagged, &["app"]), Ok(vec![]));
        let refused = shared_features(&mode("package"), &["app", "other"]).unwrap_err();
        assert!(refused.contains("resolver.feature-unification = \"package\""), "{refused}");
    }
}

//! One manifest read and normalized ([`ManifestQuery`]: its `[workspace.package]` and
//! `[workspace.dependencies]` inherited, its targets discovered), and the workspace: its root,
//! members and default members ([`WorkspaceQuery`]). Each keeps the files it read
//! ([`Snapshot`]), so a later query reads the same manifests again from exactly those.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use blaze_format::SchemaHash;
use blaze_ids::ContentId;
use blaze_manifest::files::Files;
use blaze_manifest::manifest::{EitherManifest, read_manifest};
use blaze_manifest::{Context, Workspace};
use blaze_query::{Cancelled, Cx, Query, QueryKind};
use serde::{Deserialize, Serialize};

use super::config::{ConfigChain, ConfigOf, ConfigRead};
use super::files::{Fill, Reads, Snapshot, run_over};
use super::invocation::Asking;
use super::{load_error, unmounted_error};
use crate::query::{Cbor, LoadError, Versioned, key_input, producer};

/// A manifest by absolute path, read with a configuration, as the package of `source` (the
/// source as a lock file writes it; `None` for the path package in its directory).
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) struct ManifestKey {
    pub path: String,
    pub config: ConfigOf,
    pub source: Option<String>,
}

/// What reading one manifest read and said.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub(crate) struct ManifestRead {
    /// Every file it read (the config's too), TOML in its canonical form.
    pub snapshot: Snapshot,
    /// Its warnings, or cargo's error reading it.
    pub read: Result<Vec<String>, LoadError>,
}

impl Versioned for ManifestRead {
    const SCHEMA: SchemaHash = blaze_format::schema("blaze v2 cargo manifest read 2");
}

pub(crate) struct ManifestQuery;

impl Query for ManifestQuery {
    const KIND: QueryKind = QueryKind(0x6016);
    const NAME: &'static str = "cargo manifest";
    const PRODUCER: ContentId = producer("blaze v2 cargo manifest 1");
    type Key = ManifestKey;
    type Value = Cbor<ManifestRead>;

    fn input(key: &ManifestKey) -> ContentId {
        key_input("blaze v2 cargo manifest key 1", key)
    }

    fn execute(cx: &mut Cx<'_>, key: &ManifestKey) -> Result<Cbor<ManifestRead>, Cancelled> {
        let config = match cx.get::<ConfigChain>(&key.config)?.get() {
            Ok(config) => config,
            Err(error) => return Ok(Cbor(ManifestRead { snapshot: Snapshot::default(), read: Err(error) })),
        };
        let fill = Fill { config: &key.config, own: Some(&key.path), members: false };
        let path = PathBuf::from(&key.path);
        let (read, used) = run_over(cx, config.snapshot.clone(), &fill, |_, files, _| {
            Ok(read_one(&config, files, &path, key.source.as_deref()))
        })?;
        let read = match used.unmounted() {
            Some(at) => Err(unmounted_error(at)),
            None => read.map_err(|error| load_error(&error, Some(key.path.clone()))),
        };
        Ok(Cbor(ManifestRead { snapshot: used.canonical(), read }))
    }
}

fn read_one(
    config: &ConfigRead,
    files: Rc<dyn Files>,
    path: &Path,
    source: Option<&str>,
) -> anyhow::Result<Vec<String>> {
    let ctx = Context::with_files(super::config::build(config, files.clone())?, files);
    let source = match source {
        Some(source) => manifest_source(source, &ctx.config)?,
        None => blaze_manifest::source::SourceId::for_manifest_path(path)?,
    };
    Ok(match read_manifest(path, source, &ctx)? {
        EitherManifest::Real(m) => m.warnings.clone(),
        EitherManifest::Virtual(m) => m.warnings.clone(),
    })
}

/// A workspace: from its manifest (none: the one cargo finds from the cwd, or
/// `--manifest-path`), with a configuration.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) struct WorkspaceKey {
    pub manifest: Option<String>,
    pub config: ConfigOf,
}

impl WorkspaceKey {
    pub fn view() -> WorkspaceKey {
        WorkspaceKey { manifest: None, config: ConfigOf::View }
    }
}

/// A workspace as read: where it is, its members, and every file reading it read.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub(crate) struct WorkspaceRead {
    pub config: ConfigRead,
    /// The manifest cargo was pointed at, and the root's.
    pub current: String,
    pub root_manifest: String,
    pub members: Vec<String>,
    pub default_members: Vec<String>,
    pub is_virtual: bool,
    /// The config's and every manifest's files, TOML in its canonical form.
    pub snapshot: Snapshot,
    /// cargo's warnings of the workspace and its manifests, as it prints them after `warning: `.
    pub warnings: Vec<String>,
}

impl Versioned for Result<WorkspaceRead, LoadError> {
    const SCHEMA: SchemaHash = blaze_format::schema("blaze v2 cargo workspace read 2");
}

pub(crate) struct WorkspaceQuery;

impl Query for WorkspaceQuery {
    const KIND: QueryKind = QueryKind(0x6017);
    const NAME: &'static str = "cargo workspace";
    const PRODUCER: ContentId = producer("blaze v2 cargo workspace 1");
    type Key = WorkspaceKey;
    type Value = Cbor<Result<WorkspaceRead, LoadError>>;

    fn input(key: &WorkspaceKey) -> ContentId {
        key_input("blaze v2 cargo workspace key 1", key)
    }

    fn execute(cx: &mut Cx<'_>, key: &WorkspaceKey) -> Result<Cbor<Result<WorkspaceRead, LoadError>>, Cancelled> {
        let config = match cx.get::<ConfigChain>(&key.config)?.get() {
            Ok(config) => config,
            Err(error) => return Ok(Cbor(Err(error))),
        };
        let manifest_path = match (&key.manifest, &key.config) {
            (Some(path), _) => Some(path.clone()),
            (None, ConfigOf::View) => match cx.get::<Asking>(&())?.get() {
                Ok(asked) => asked.manifest_path.map(|it| match it.starts_with('/') {
                    true => it,
                    false => format!("{}/{it}", asked.cwd),
                }),
                Err(error) => return Ok(Cbor(Err(error))),
            },
            (None, ConfigOf::Fixed { .. }) => None,
        };
        let fill = Fill { config: &key.config, own: None, members: true };
        let (found, used) = run_over(cx, config.snapshot.clone(), &fill, |_, files, _| {
            Ok(read_workspace(&config, files, manifest_path.as_deref()))
        })?;
        if let Some(at) = used.unmounted() {
            return Ok(Cbor(Err(unmounted_error(at))));
        }
        Ok(Cbor(found.map(|mut read| {
            read.snapshot = used.canonical();
            read
        })))
    }
}

fn read_workspace(
    config: &ConfigRead,
    files: Rc<dyn Files>,
    manifest_path: Option<&str>,
) -> Result<WorkspaceRead, LoadError> {
    let ctx = Context::with_files(
        super::config::build(config, files.clone()).map_err(|error| load_error(&error, None))?,
        files.clone(),
    );
    let current = root_manifest(manifest_path, &config.cwd, &*files)?;
    let ws = Workspace::new(Path::new(&current), &ctx).map_err(|error| load_error(&error, Some(current.clone())))?;
    let show = |path: &Path| path.display().to_string();
    let members: Vec<String> = ws.members().map(|(path, _)| show(path)).collect();
    let single = members.len() == 1 && ws.root_manifest() == ws.current_manifest();
    let mut warnings: Vec<String> = ws.workspace_warnings().to_vec();
    for (path, warning) in ws.warnings() {
        warnings.push(match single {
            true => warning.to_owned(),
            false => format!("{}: {warning}", path.display()),
        });
    }
    Ok(WorkspaceRead {
        config: config.clone(),
        current: show(ws.current_manifest()),
        root_manifest: show(ws.root_manifest()),
        default_members: ws.default_members().map(|(path, _)| show(path)).collect(),
        members,
        is_virtual: ws.is_virtual(),
        snapshot: Snapshot::default(),
        warnings,
    })
}

/// The manifest cargo reads: `--manifest-path`'s, or the nearest `Cargo.toml` from the cwd up,
/// as cargo's `find_root_manifest_for_wd` and `root_manifest` say it.
fn root_manifest(manifest_path: Option<&str>, cwd: &str, files: &dyn Files) -> Result<String, LoadError> {
    use blaze_manifest::files::FileKind;
    if let Some(path) = manifest_path {
        let path = super::files::normal(Path::new(path)).unwrap_or_else(|| path.to_owned());
        if !path.ends_with("Cargo.toml") {
            return Err(LoadError { what: "the manifest-path must be a path to a Cargo.toml file".to_owned(), at: Some(path) });
        }
        if files.kind(Path::new(&path)).is_none() {
            return Err(LoadError { what: format!("manifest path `{path}` does not exist"), at: Some(path) });
        }
        return Ok(path);
    }
    let mut dir = Some(cwd.to_owned());
    while let Some(at) = dir {
        let candidate = match at.as_str() {
            "/" => "/Cargo.toml".to_owned(),
            _ => format!("{at}/Cargo.toml"),
        };
        if files.kind(Path::new(&candidate)) == Some(FileKind::File) {
            return Ok(candidate);
        }
        let lower = match at.as_str() {
            "/" => "/cargo.toml".to_owned(),
            _ => format!("{at}/cargo.toml"),
        };
        if files.kind(Path::new(&lower)) == Some(FileKind::File) {
            return Err(LoadError {
                what: format!(
                    "could not find `Cargo.toml` in `{cwd}` or any parent directory, but found cargo.toml please try to rename it to Cargo.toml"
                ),
                at: None,
            });
        }
        dir = super::files::ancestors(&format!("{at}/x")).into_iter().nth(1);
        if at == "/" {
            break;
        }
    }
    Err(LoadError { what: format!("could not find `Cargo.toml` in `{cwd}` or any parent directory"), at: None })
}

/// The workspace `read` describes read again over its snapshot (and `extra`, files a later
/// query read itself), handed to `then` with its context.
pub(crate) fn replay<'db, R>(
    cx: &mut Cx<'db>,
    read: &WorkspaceRead,
    config_of: &ConfigOf,
    extra: &Snapshot,
    mut then: impl FnMut(&mut Cx<'db>, &Context, &Workspace<'_>, Rc<dyn Files>, &Reads<'_>) -> Result<R, Cancelled>,
) -> Result<(Result<R, LoadError>, Snapshot), Cancelled> {
    let mut base = read.snapshot.clone();
    base.merge(extra.clone());
    let fill = Fill { config: config_of, own: None, members: true };
    run_over(cx, base, &fill, |cx, files, reads| {
        let config = match super::config::build(&read.config, files.clone()) {
            Ok(config) => config,
            Err(error) => return Ok(Err(load_error(&error, None))),
        };
        let ctx = Context::with_files(config, files.clone());
        let ws = match Workspace::new(Path::new(&read.current), &ctx) {
            Ok(ws) => ws,
            Err(error) => return Ok(Err(load_error(&error, Some(read.current.clone())))),
        };
        then(cx, &ctx, &ws, files, reads).map(Ok)
    })
}

/// The source of a package a lock file pins to `source`, as blaze-manifest names it
/// (blaze-metadata's `manifest_source`).
pub(crate) fn manifest_source(
    source: &str,
    config: &blaze_manifest::Config,
) -> anyhow::Result<blaze_manifest::source::SourceId> {
    use anyhow::Context as _;
    use blaze_manifest::source::{CRATES_IO_INDEX, SourceId};
    use cargo_util_schemas::core::GitReference;
    let (kind, rest) = source.split_once('+').context("a source with no kind")?;
    match kind {
        "registry" if rest == CRATES_IO_INDEX => SourceId::crates_io(config),
        "registry" => Ok(SourceId::for_registry(&rest.parse()?)),
        "sparse" => Ok(SourceId::for_registry(&source.parse()?)),
        "git" => {
            let mut url: url::Url = rest.parse()?;
            let query: rustc_hash::FxHashMap<String, String> = url.query_pairs().into_owned().collect();
            let reference = match (query.get("branch"), query.get("tag"), query.get("rev")) {
                (Some(branch), _, _) => GitReference::Branch(branch.clone()),
                (_, Some(tag), _) => GitReference::Tag(tag.clone()),
                (_, _, Some(rev)) => GitReference::Rev(rev.clone()),
                _ => GitReference::DefaultBranch,
            };
            url.set_fragment(None);
            url.set_query(None);
            Ok(SourceId::for_git(&url, reference))
        }
        _ => anyhow::bail!("the source {source} is not one blaze reads yet"),
    }
}

//! Where cargo has each package the resolve pins, and the registries' indexes the resolve reads
//! (docs/v2/plan/lanes/cargo/notes.md section 3): every file read through the view's mounts,
//! never the disk or the network. What is not mounted yet is a [`Fetch`] the daemon makes and
//! mounts where cargo keeps it; offline, it is cargo's error.
//!
//! - `[source]` replacement is cargo 1.99's `SourceConfigMap` (`src/sources/config.rs`), copied
//!   from v1's `crates/blaze-registry/src/sources.rs` and read from the configuration as v1's
//!   `crates/blaze-metadata/src/lib.rs` (`source_config`) read it.
//! - A registry package is its `.crate` unpacked at `$CARGO_HOME/registry/src/<dir>/<name>-<version>`
//!   (cargo's `RegistrySource`); a sparse index file is at
//!   `$CARGO_HOME/registry/index/<dir>/.cache/<prefix path>`, as served; a git package is in its
//!   checkout at `$CARGO_HOME/git/checkouts/<ident>/<short commit>` (cargo's `GitSource`).
//! - A `local-registry` and a `directory` source are read in place. A git index is not read:
//!   blaze reads sparse indexes only.

mod index;
mod registry;
#[cfg(test)]
mod tests;
mod tree;

use anyhow::{anyhow, bail};
use blaze_manifest::Config;
use blaze_query::{Cancelled, Cx};
use blaze_resolve::{CRATES_IO_REGISTRY, GitReference, PackageId, SourceId, SourceKind};
use cargo_util_schemas::core as manifest_core;
use rustc_hash::FxHashMap;
use serde::Deserialize;

use self::index::{crate_url, index_path};
use crate::load::files::{Found, normal, read_dir, read_file};
use crate::load::load_error;
use crate::query::{Fetch, FetchKind, LoadError};

pub(crate) use self::registry::Index;

/// The sources a build reads packages from, as its configuration names them.
pub(crate) struct Sources {
    map: SourceMap,
    /// `$CARGO_HOME`, absolute, `/`-separated.
    home: String,
    offline: Option<&'static str>,
}

/// Where a package is.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Located {
    /// The package's `Cargo.toml`, absolute, under a mount.
    Here { manifest: String },
    Needs(Fetch),
    Failed(LoadError),
}

/// A registry's index file, as read.
pub(crate) enum IndexFile {
    Here(Vec<u8>),
    /// The registry has no such file (or, offline, it is not fetched).
    Absent,
    Needs(Fetch),
    Failed(anyhow::Error),
}

/// The `config.json` at the root of a sparse index.
#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct RegistryConfig {
    /// Where a `.crate` file is: a template of `{crate}`, `{version}`, `{prefix}`,
    /// `{lowerprefix}` and `{sha256-checksum}`, or, with none of them, a URL that
    /// `/{crate}/{version}/download` is added to.
    dl: String,
    #[serde(default)]
    auth_required: bool,
}

impl Sources {
    /// From the configuration the workspace was read with: `[source]` replacements,
    /// `[registries]`, `registry.index`, `registries.crates-io.protocol` (sparse by default),
    /// `$CARGO_HOME` (`config.home()`). `offline` is `Some("--offline")` or `Some("--frozen")`
    /// when the command line or `net.offline` makes the build offline.
    pub(crate) fn new(config: &Config, offline: Option<&'static str>) -> anyhow::Result<Sources> {
        let home = normal(config.home())
            .ok_or_else(|| anyhow!("`$CARGO_HOME` (`{}`) is not an absolute path", config.home().display()))?;
        Ok(Sources { map: SourceMap::new(config)?, home, offline })
    }

    /// A package the resolve pins that is not a path package: where cargo has its `Cargo.toml`,
    /// or what to fetch first.
    pub(crate) fn locate(
        &self,
        cx: &mut Cx<'_>,
        id: PackageId,
        checksum: Option<&str>,
    ) -> Result<Located, Cancelled> {
        let source = id.source_id();
        let at = Some(format!("{} v{}", id.name(), id.version()));
        let failed = |error: anyhow::Error| Located::Failed(load_error(&error, at.clone()));
        if source.is_path() {
            return Ok(failed(anyhow!("`{id}` is a path package, read where it is")));
        }
        let from = match self.map.replacement(source) {
            Ok(from) => from,
            Err(error) => return Ok(failed(error)),
        };
        Ok(match from.kind() {
            SourceKind::SparseRegistry | SourceKind::LocalRegistry => {
                match self.registry_package(cx, id, from, checksum)? {
                    Ok(located) => located,
                    Err(error) => failed(download_error(id, error)),
                }
            }
            SourceKind::Registry => failed(git_index(from)),
            SourceKind::Git(_) => match self.git_package(cx, id, from)? {
                Ok(located) => located,
                Err(error) => failed(error),
            },
            SourceKind::Directory => {
                let Some(root) = local_path(from) else {
                    return Ok(failed(anyhow!("{from} is not a directory blaze can name")));
                };
                match tree::vendored_package(cx, &root, &from.to_string(), id, checksum)? {
                    Ok(manifest) => Located::Here { manifest },
                    Err(error) if from != source => {
                        failed(error.context(format!("failed to download replaced source {source}")))
                    }
                    Err(error) => failed(error),
                }
            }
            SourceKind::Path => failed(anyhow!("{source} is replaced with the path {from}, which is not a source")),
        })
    }

    /// A package of a sparse or local registry: its directory unpacked, or the `.crate` to
    /// fetch, checked by the lock file's checksum or else its index's.
    fn registry_package(
        &self,
        cx: &mut Cx<'_>,
        id: PackageId,
        from: SourceId,
        checksum: Option<&str>,
    ) -> Result<anyhow::Result<Located>, Cancelled> {
        let name = id.name().as_str();
        let version = id.version().to_string();
        let at = self.package_dir(from, name, &version);
        if let Found::Here(_) = read_dir(cx, &at)? {
            return Ok(Ok(Located::Here { manifest: format!("{at}/Cargo.toml") }));
        }
        if let Some(flag) = self.offline {
            return Ok(Err(anyhow!("attempting to make an HTTP request, but {flag} was specified")));
        }
        let sha256 = match checksum.filter(|it| !it.is_empty()) {
            Some(it) => it.to_owned(),
            None => {
                let no_hash = || anyhow!("no hash listed for {id}");
                match self.index_file(cx, from, name)? {
                    IndexFile::Here(bytes) => {
                        let lines = index::parse_file(&bytes);
                        let line = lines.into_iter().find(|it| &it.entry.version == id.version());
                        match line {
                            Some(line)
                                if line.status != index::Status::Invalid
                                    && !line.entry.checksum.is_empty() =>
                            {
                                line.entry.checksum
                            }
                            _ => return Ok(Err(no_hash())),
                        }
                    }
                    IndexFile::Absent => return Ok(Err(no_hash())),
                    IndexFile::Needs(fetch) => return Ok(Ok(Located::Needs(fetch))),
                    IndexFile::Failed(error) => return Ok(Err(error)),
                }
            }
        };
        let url = if from.kind() == &SourceKind::LocalRegistry {
            let Some(root) = local_path(from) else {
                return Ok(Err(anyhow!("{from} is not a directory blaze can name")));
            };
            let file = format!("{root}/{name}-{version}.crate");
            match read_file(cx, &file)? {
                Found::Here(_) => format!("file://{file}"),
                Found::Absent => return Ok(Err(anyhow!("failed to read `{file}`"))),
                Found::Unmounted => return Ok(Err(unmounted_local(&root))),
            }
        } else if from.is_crates_io() {
            format!("https://static.crates.io/crates/{name}/{version}/download")
        } else {
            let base = match sparse_base(from) {
                Ok(base) => base,
                Err(error) => return Ok(Err(error)),
            };
            let config_path = format!("{}/config.json", self.index_dir(from));
            let config = match read_file(cx, &config_path)? {
                Found::Here(bytes) => match serde_json::from_slice::<RegistryConfig>(&bytes) {
                    Ok(config) => config,
                    Err(error) => {
                        return Ok(Err(anyhow::Error::new(error)
                            .context("failed to parse registry's config.json")));
                    }
                },
                Found::Absent => return Ok(Err(anyhow!("config.json not found"))),
                Found::Unmounted => {
                    return Ok(Ok(Located::Needs(Fetch {
                        what: format!("{} config.json", from.display_index()),
                        url: format!("{base}config.json"),
                        kind: FetchKind::IndexFile,
                        at: config_path,
                    })));
                }
            };
            if config.auth_required {
                return Ok(Err(anyhow!(
                    "{} requires authentication (`auth-required` in its config.json), and blaze \
                     sends no registry token yet",
                    from.display_index()
                )));
            }
            crate_url(&config.dl, name, &version, &sha256)
        };
        Ok(Ok(Located::Needs(Fetch {
            what: format!("{name} v{version}"),
            url,
            kind: FetchKind::Crate { sha256 },
            at,
        })))
    }

    /// A package of a git source: in the checkout of the commit the lock file pins.
    fn git_package(
        &self,
        cx: &mut Cx<'_>,
        id: PackageId,
        from: SourceId,
    ) -> Result<anyhow::Result<Located>, Cancelled> {
        let source = id.source_id();
        let Some(commit) = source.precise_git_fragment() else {
            return Ok(Err(anyhow!(
                "blaze reads the git source {source} only at the commit Cargo.lock pins, and it \
                 pins none for `{} v{}`",
                id.name(),
                id.version()
            )));
        };
        let short = commit.get(..7).unwrap_or(commit);
        let at = format!("{}/git/checkouts/{}/{short}", self.home, from.git_ident());
        if let Found::Here(_) = read_dir(cx, &at)? {
            return Ok(tree::checkout_package(cx, &at, id)?.map(|manifest| Located::Here { manifest }));
        }
        if let Some(flag) = self.offline {
            return Ok(Err(download_error(
                id,
                anyhow!("attempting to make an HTTP request, but {flag} was specified"),
            )));
        }
        Ok(Ok(Located::Needs(Fetch {
            what: format!("git repository `{}`", from.url()),
            url: from.url().to_string(),
            kind: FetchKind::Git { commit: commit.to_owned() },
            at,
        })))
    }

    /// The index file of `name` in the registry `from` (a registry after `[source]`
    /// replacement): a sparse index's where cargo keeps it, a local registry's in place.
    pub(crate) fn index_file(
        &self,
        cx: &mut Cx<'_>,
        from: SourceId,
        name: &str,
    ) -> Result<IndexFile, Cancelled> {
        let relative = index_path(name);
        Ok(match from.kind() {
            SourceKind::SparseRegistry => {
                let base = match sparse_base(from) {
                    Ok(base) => base,
                    Err(error) => return Ok(IndexFile::Failed(error)),
                };
                let at = format!("{}/.cache/{relative}", self.index_dir(from));
                match read_file(cx, &at)? {
                    Found::Here(bytes) => IndexFile::Here(bytes),
                    Found::Absent => IndexFile::Absent,
                    Found::Unmounted if self.offline.is_some() => IndexFile::Absent,
                    Found::Unmounted => IndexFile::Needs(Fetch {
                        what: format!("{} file of `{name}`", from.display_index()),
                        url: format!("{base}{relative}"),
                        kind: FetchKind::IndexFile,
                        at,
                    }),
                }
            }
            SourceKind::LocalRegistry => {
                let Some(root) = local_path(from) else {
                    return Ok(IndexFile::Failed(anyhow!("{from} is not a directory blaze can name")));
                };
                match read_dir(cx, &root)? {
                    Found::Here(_) => {}
                    Found::Absent => {
                        return Ok(IndexFile::Failed(anyhow!(
                            "local registry path is not a directory: {root}"
                        )));
                    }
                    Found::Unmounted => return Ok(IndexFile::Failed(unmounted_local(&root))),
                }
                let index = format!("{root}/index");
                if !matches!(read_dir(cx, &index)?, Found::Here(_)) {
                    return Ok(IndexFile::Failed(anyhow!(
                        "local registry index path is not a directory: {index}"
                    )));
                }
                match read_file(cx, &format!("{index}/{relative}"))? {
                    Found::Here(bytes) => IndexFile::Here(bytes),
                    Found::Absent => IndexFile::Absent,
                    Found::Unmounted => IndexFile::Failed(unmounted_local(&root)),
                }
            }
            SourceKind::Registry => IndexFile::Failed(git_index(from)),
            _ => IndexFile::Failed(anyhow!("{from} has no index")),
        })
    }

    /// Whether a registry package is fetched, which offline is what cargo's
    /// `is_crate_downloaded` asks: a local registry's always are.
    pub(crate) fn is_downloaded(
        &self,
        cx: &mut Cx<'_>,
        from: SourceId,
        name: &str,
        version: &semver::Version,
    ) -> Result<bool, Cancelled> {
        if from.kind() == &SourceKind::LocalRegistry {
            return Ok(true);
        }
        let at = self.package_dir(from, name, &version.to_string());
        Ok(matches!(read_dir(cx, &at)?, Found::Here(_)))
    }

    pub(crate) fn offline(&self) -> Option<&'static str> {
        self.offline
    }

    /// The registry `id`'s packages come from, when `id` is a registry and what replaces it is
    /// one too.
    pub(crate) fn registry_of(&self, id: SourceId) -> anyhow::Result<Option<SourceId>> {
        if !id.is_registry() {
            return Ok(None);
        }
        let from = self.map.replacement(id)?;
        Ok(from.is_registry().then_some(from))
    }

    /// As cargo's `ReplacedSource::describe`: the source read, and the one it replaces unless
    /// that is crates.io read over its sparse index.
    pub(crate) fn describe(&self, source: SourceId) -> String {
        let from = self.map.replacement(source).unwrap_or(source);
        if from == source || (from.is_crates_io() && source.is_crates_io()) {
            describe(from)
        } else {
            format!("{} (which is replacing {source})", describe(from))
        }
    }

    pub(crate) fn is_replaced(&self, source: SourceId) -> bool {
        let from = self.map.replacement(source).unwrap_or(source);
        from != source && !(from.is_crates_io() && source.is_crates_io())
    }

    /// `$CARGO_HOME/registry/src/<dir>/<name>-<version>`.
    fn package_dir(&self, from: SourceId, name: &str, version: &str) -> String {
        format!("{}/registry/src/{}/{name}-{version}", self.home, from.registry_dir_name())
    }

    /// `$CARGO_HOME/registry/index/<dir>`.
    fn index_dir(&self, from: SourceId) -> String {
        format!("{}/registry/index/{}", self.home, from.registry_dir_name())
    }
}

/// cargo's error for a package it could not fetch.
fn download_error(id: PackageId, error: anyhow::Error) -> anyhow::Error {
    error.context(format!("failed to download `{} v{}`", id.name(), id.version()))
}

fn git_index(from: SourceId) -> anyhow::Error {
    anyhow!(
        "blaze reads sparse registry indexes only, and {from} has a git index (`{}`): read it \
         over its sparse index (a `sparse+` index URL, or `registries.crates-io.protocol = \
         \"sparse\"` for crates.io)",
        from.url()
    )
}

fn unmounted_local(root: &str) -> anyhow::Error {
    anyhow!("blaze reads the local registry `{root}` in place, and it is under no mount of the view")
}

/// A sparse index's URL without `sparse+`, ending in `/`, as cargo's `HttpRegistry::new` takes
/// it.
fn sparse_base(from: SourceId) -> anyhow::Result<String> {
    let url = from.url().as_str();
    if !from.url().path().ends_with('/') {
        bail!("sparse registry url must end in a slash `/`: {url}");
    }
    Ok(url.strip_prefix("sparse+").unwrap_or(url).to_owned())
}

/// The directory of a `local-registry` or `directory` source, as the load names paths.
fn local_path(id: SourceId) -> Option<String> {
    normal(&id.url().to_file_path().ok()?)
}

/// How cargo's sources describe themselves (each `Source::describe`).
fn describe(id: SourceId) -> String {
    match id.kind() {
        SourceKind::Registry | SourceKind::SparseRegistry | SourceKind::LocalRegistry => {
            id.display_index()
        }
        SourceKind::Directory => match id.url().to_file_path() {
            Ok(path) => format!("directory source `{}`", path.display()),
            Err(()) => id.to_string(),
        },
        SourceKind::Git(_) => format!("Git repository {id}"),
        SourceKind::Path => match id.url().to_file_path() {
            Ok(path) => path.display().to_string(),
            Err(()) => id.to_string(),
        },
    }
}

/// A `[source]` table's location and what replaces it.
#[derive(Clone)]
struct Replacement {
    id: SourceId,
    /// The name of the source that replaces it and where that is defined.
    with: Option<(String, String)>,
}

/// The sources cargo knows by name, and which of them replaces which (cargo's
/// `SourceConfigMap`, v1's `SourceMap`).
struct SourceMap {
    by_name: FxHashMap<String, Replacement>,
    names: FxHashMap<SourceId, String>,
    /// The `[registries]` a `replace-with` names, as cargo's `SourceId::alt_registry` reads
    /// them: one whose index is not configured is not here.
    registries: FxHashMap<String, SourceId>,
    crates_io: SourceId,
}

impl SourceMap {
    fn new(config: &Config) -> anyhow::Result<SourceMap> {
        let crates_io = match config.crates_io_maybe_sparse_http()?.kind {
            manifest_core::SourceKind::SparseRegistry => SourceId::crates_io_sparse(),
            _ => SourceId::crates_io(),
        };
        let mut map = SourceMap {
            by_name: FxHashMap::default(),
            names: FxHashMap::default(),
            registries: FxHashMap::default(),
            crates_io,
        };
        map.add(CRATES_IO_REGISTRY, Replacement { id: SourceId::crates_io(), with: None })?;
        map.add(CRATES_IO_REGISTRY, Replacement { id: crates_io, with: None })?;
        for (name, def) in config.source_configs()? {
            if name == CRATES_IO_REGISTRY && def.replace_with.is_none() {
                continue;
            }
            let id = match name.as_str() {
                CRATES_IO_REGISTRY => crates_io,
                _ => source_id(&name, &def.id)?,
            };
            map.add(&name, Replacement { id, with: def.replace_with.clone() })?;
        }
        let named: Vec<String> = map
            .by_name
            .values()
            .filter_map(|it| it.with.as_ref().map(|(name, _)| name.clone()))
            .filter(|name| !map.by_name.contains_key(name))
            .collect();
        for name in named {
            if let Ok(id) = blaze_manifest::source::SourceId::alt_registry(config, &name)
                && let Ok(id) = SourceId::for_alt_registry(&id.url, &name)
            {
                map.registries.insert(name, id);
            }
        }
        Ok(map)
    }

    fn add(&mut self, name: &str, replacement: Replacement) -> anyhow::Result<()> {
        if let Some(old) = self.names.insert(replacement.id, name.to_owned())
            && name != CRATES_IO_REGISTRY
        {
            bail!(
                "source `{name}` defines source {}, but that source is already defined by `{old}`\n\
                 note: Sources are not allowed to be defined multiple times.",
                replacement.id
            );
        }
        self.by_name.insert(name.to_owned(), replacement);
        Ok(())
    }

    /// The source the packages of `id` come from: `id` itself, or the end of its chain of
    /// `replace-with`. A vendored replacement of a git source needs `id` to be pinned, as a
    /// lock file pins it to a commit.
    fn replacement(&self, id: SourceId) -> anyhow::Result<SourceId> {
        let Some(original) = self.names.get(&id) else { return Ok(id) };
        let mut name = original;
        let mut at = "";
        let replaced = loop {
            let Some(replacement) = self.by_name.get(name) else {
                if let Some(id) = self.registries.get(name) {
                    break *id;
                }
                bail!(
                    "could not find a configured source with the name `{name}` when attempting \
                     to lookup `{original}` (configuration in `{at}`)"
                );
            };
            match &replacement.with {
                Some((with, definition)) => {
                    name = with;
                    at = definition;
                }
                None if replacement.id == id => return Ok(id),
                None => break replacement.id,
            }
            if name == original {
                bail!(
                    "detected a cycle of `replace-with` sources, the source `{name}` is \
                     eventually replaced with itself (configuration in `{at}`)"
                );
            }
        };
        if supports_checksums(id) && !supports_checksums(replaced) {
            bail!(
                "cannot replace `{original}` with `{name}`, the source `{original}` supports \
                 checksums, but `{name}` does not\n\na lock file compatible with `{original}` \
                 cannot be generated in this situation"
            );
        }
        if requires_precise(id) && !id.has_precise() {
            bail!(
                "the source {original} requires a lock file to be present first before it can be\n\
                 used against vendored source code\n\nremove the source replacement \
                 configuration, generate a lock file, and then\nrestore the source replacement \
                 configuration to continue the build"
            );
        }
        Ok(replaced)
    }
}

/// A `[source.<name>]` location as blaze-manifest reads it, as the resolver names sources.
fn source_id(name: &str, id: &blaze_manifest::source::SourceId) -> anyhow::Result<SourceId> {
    let path = || {
        id.url.to_file_path().map_err(|()| anyhow!("`source.{name}` names `{}`, which is not a path", id.url))
    };
    match &id.kind {
        manifest_core::SourceKind::Registry | manifest_core::SourceKind::SparseRegistry => {
            SourceId::for_source_replacement_registry(&id.url, name)
        }
        manifest_core::SourceKind::LocalRegistry => SourceId::for_local_registry(&path()?),
        manifest_core::SourceKind::Directory => SourceId::for_directory(&path()?),
        manifest_core::SourceKind::Git(reference) => {
            let reference = match reference {
                manifest_core::GitReference::Branch(it) => GitReference::Branch(it.clone()),
                manifest_core::GitReference::Tag(it) => GitReference::Tag(it.clone()),
                manifest_core::GitReference::Rev(it) => GitReference::Rev(it.clone()),
                manifest_core::GitReference::DefaultBranch => GitReference::DefaultBranch,
            };
            SourceId::for_git(&id.url, reference)
        }
        _ => bail!("`source.{name}` is not a source blaze reads yet"),
    }
}

fn supports_checksums(id: SourceId) -> bool {
    matches!(
        id.kind(),
        SourceKind::Registry
            | SourceKind::SparseRegistry
            | SourceKind::LocalRegistry
            | SourceKind::Directory
    )
}

fn requires_precise(id: SourceId) -> bool {
    matches!(id.kind(), SourceKind::Git(_) | SourceKind::Directory)
}

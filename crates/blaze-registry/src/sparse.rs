//! A registry served over HTTP, read file by file as cargo 1.99's `HttpRegistry`
//! (`src/sources/registry/http_remote.rs`) reads it: `config.json`, then each package's file,
//! asked for again only with `If-None-Match` or `If-Modified-Since` once it is in the cache,
//! and only when the index is to be updated.

use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Context, bail};
use rustc_hash::FxHashSet;
use serde_derive::Deserialize;

use crate::{
    SourceId,
    http::{Http, HttpNotSuccessful, Retry},
};

/// The `config.json` at the root of an index.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct RegistryConfig {
    /// Where a `.crate` file is: a template of `{crate}`, `{version}`, `{prefix}`,
    /// `{lowerprefix}` and `{sha256-checksum}`, or, with none of them, a URL that
    /// `/{crate}/{version}/download` is added to.
    pub dl: String,
    pub api: Option<String>,
    #[serde(default)]
    pub auth_required: bool,
}

impl RegistryConfig {
    pub const NAME: &str = "config.json";
}

/// What reading an index file came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadResponse {
    /// The cache of it is up to date.
    CacheValid,
    Data {
        raw: Vec<u8>,
        /// What the cache of it is made from, to ask with next time: `etag: <value>`,
        /// `last-modified: <value>` or `Unknown`; none for an index that is files on disk.
        index_version: Option<String>,
    },
    NotFound,
}

/// The files of one sparse registry.
pub struct SparseIndex {
    source: SourceId,
    /// The index's URL without `sparse+`, ending in `/`.
    base: String,
    /// `$CARGO_HOME/registry/index/<name>`.
    dir: PathBuf,
    http: Arc<dyn Http>,
    retry: Retry,
    offline: bool,
    /// Whether files in the cache are to be checked with the server: cargo checks only when
    /// it updates the index (`invalidate_cache`), which it does unless every package it is
    /// asked about is locked and found.
    update: AtomicBool,
    /// The files asked for since the index was last updated.
    fresh: Mutex<FxHashSet<String>>,
    config: Mutex<Option<RegistryConfig>>,
}

impl SparseIndex {
    pub fn new(
        source: SourceId,
        dir: PathBuf,
        http: Arc<dyn Http>,
        retry: Retry,
        offline: bool,
    ) -> anyhow::Result<SparseIndex> {
        let url = source.url().as_str();
        if !url.ends_with('/') {
            bail!("sparse registry url must end in a slash `/`: {url}");
        }
        let base = url.strip_prefix("sparse+").context("a sparse registry's URL")?.to_owned();
        Ok(SparseIndex {
            source,
            base,
            dir,
            http,
            retry,
            offline,
            update: AtomicBool::new(false),
            fresh: Mutex::default(),
            config: Mutex::default(),
        })
    }

    pub fn source(&self) -> &SourceId {
        &self.source
    }

    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    /// From now on, check every file with the server once.
    pub fn invalidate(&self) {
        self.fresh().clear();
        self.update.store(true, Ordering::Relaxed);
    }

    pub fn is_updated(&self) -> bool {
        self.update.load(Ordering::Relaxed)
    }

    fn fresh(&self) -> std::sync::MutexGuard<'_, FxHashSet<String>> {
        self.fresh.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn is_fresh(&self, path: &str) -> bool {
        !self.is_updated() || self.offline || self.fresh().contains(path)
    }

    /// Whether [`Sparse::config`] would read `config.json` from disk, asking no server.
    pub fn config_is_cached(&self) -> bool {
        let path = self.dir.join(RegistryConfig::NAME);
        self.is_fresh(RegistryConfig::NAME)
            && std::fs::read(&path)
                .ok()
                .is_some_and(|it| serde_json::from_slice::<RegistryConfig>(&it).is_ok())
    }

    /// The registry's `config.json`: the copy on disk while it is fresh, else the server's,
    /// which is then written to disk. `None` when the server has none.
    pub fn config(&self) -> anyhow::Result<Option<RegistryConfig>> {
        let mut config = self.config.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(config) = &*config
            && self.is_fresh(RegistryConfig::NAME)
        {
            return Ok(Some(config.clone()));
        }
        let path = self.dir.join(RegistryConfig::NAME);
        let on_disk = || serde_json::from_slice::<RegistryConfig>(&std::fs::read(&path).ok()?).ok();
        if self.is_fresh(RegistryConfig::NAME)
            && let Some(found) = on_disk()
        {
            *config = Some(found.clone());
            return Ok(Some(found));
        }
        let read = match self.fetch(RegistryConfig::NAME, None)? {
            LoadResponse::Data { raw, .. } => {
                let read: RegistryConfig = serde_json::from_slice(&raw)
                    .with_context(|| format!("the config.json of {}", self.base))?;
                if std::fs::create_dir_all(&self.dir).is_ok() {
                    _ = crate::write_whole(&path, &raw);
                }
                Some(read)
            }
            LoadResponse::NotFound => None,
            LoadResponse::CacheValid => bail!("config.json is never stored in the index cache"),
        };
        *config = read.clone();
        Ok(read)
    }

    /// An index file, `path` under the index's URL: [`LoadResponse::CacheValid`] when the
    /// cached copy of `index_version` is fresh or the server says it has not changed.
    pub fn load(&self, path: &str, index_version: Option<&str>) -> anyhow::Result<LoadResponse> {
        let Some(config) = self.config()? else { return Ok(LoadResponse::NotFound) };
        if config.auth_required {
            bail!(
                "{} requires authentication, which blaze does not do yet \
                 (`registry.global-credential-providers`)",
                self.base
            );
        }
        if index_version.is_some() {
            if self.is_fresh(path) {
                return Ok(LoadResponse::CacheValid);
            }
        } else if self.fresh().contains(path) {
            return Ok(LoadResponse::NotFound);
        }
        let condition =
            index_version.and_then(|it| it.split_once(':')).and_then(|(key, value)| match key {
                "etag" => Some(("if-none-match", value.trim())),
                "last-modified" => Some(("if-modified-since", value.trim())),
                _ => None,
            });
        self.fetch(path, condition)
    }

    fn fetch(&self, path: &str, condition: Option<(&str, &str)>) -> anyhow::Result<LoadResponse> {
        if self.offline {
            return Ok(LoadResponse::NotFound);
        }
        let url = format!("{}{path}", self.base);
        let mut headers = vec![("cargo-protocol", "version=1"), ("accept", "text/plain")];
        headers.extend(condition);
        let answer = self.retry.run(|| {
            let answer = self
                .http
                .get(&url, &headers)
                .with_context(|| format!("download of {path} failed"))?;
            match answer.status {
                200 | 304 | 404 => Ok(answer),
                _ => Err(HttpNotSuccessful::new(&url, &answer).into()),
            }
        });
        self.fresh().insert(path.to_owned());
        let answer = answer?;
        Ok(match answer.status {
            200 => {
                let index_version = if let Some(etag) = answer.header("etag") {
                    format!("etag: {etag}")
                } else if let Some(modified) = answer.header("last-modified") {
                    format!("last-modified: {modified}")
                } else {
                    "Unknown".to_owned()
                };
                LoadResponse::Data { raw: answer.body, index_version: Some(index_version) }
            }
            304 => LoadResponse::CacheValid,
            _ => LoadResponse::NotFound,
        })
    }
}

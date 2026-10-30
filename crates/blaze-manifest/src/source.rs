//! Where a package comes from, as cargo's `SourceId` names it in `cargo metadata`.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::anyhow;
use cargo_util_schemas::core::{GitReference, SourceKind};
use url::Url;

use crate::config::Config;
use crate::files::Files;

pub const CRATES_IO_INDEX: &str = "https://github.com/rust-lang/crates.io-index";
pub const CRATES_IO_REGISTRY: &str = "crates-io";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceId {
    pub kind: SourceKind,
    pub url: Url,
    /// The name of the registry in `[registries]`, for one named there.
    pub registry_key: Option<String>,
}

impl SourceId {
    pub fn for_path(path: &Path) -> anyhow::Result<SourceId> {
        let url = Url::from_file_path(path)
            .map_err(|()| anyhow!("invalid path url `{}`", path.display()))?;
        Ok(SourceId { kind: SourceKind::Path, url, registry_key: None })
    }

    /// The source of the package whose manifest is at `manifest_path`: its directory.
    pub fn for_manifest_path(manifest_path: &Path) -> anyhow::Result<SourceId> {
        SourceId::for_path(manifest_path.parent().unwrap())
    }

    pub fn for_git(url: &Url, reference: GitReference) -> SourceId {
        SourceId { kind: SourceKind::Git(reference), url: url.clone(), registry_key: None }
    }

    fn remote_kind(url: &Url) -> SourceKind {
        if url.as_str().starts_with("sparse+") {
            SourceKind::SparseRegistry
        } else {
            SourceKind::Registry
        }
    }

    pub fn for_registry(url: &Url) -> SourceId {
        SourceId { kind: Self::remote_kind(url), url: url.clone(), registry_key: None }
    }

    pub fn for_alt_registry(url: &Url, key: &str) -> SourceId {
        SourceId { kind: Self::remote_kind(url), url: url.clone(), registry_key: Some(key.into()) }
    }

    /// crates.io as cargo names it in metadata: its git index, whatever protocol fetches it.
    pub fn crates_io<F: Files>(config: &Config<F>) -> anyhow::Result<SourceId> {
        config.check_registry_index_not_set()?;
        Ok(SourceId::for_alt_registry(&CRATES_IO_INDEX.parse().unwrap(), CRATES_IO_REGISTRY))
    }

    /// A registry by its name in `[registries]`.
    pub fn alt_registry<F: Files>(config: &Config<F>, key: &str) -> anyhow::Result<SourceId> {
        if key == CRATES_IO_REGISTRY {
            return SourceId::crates_io(config);
        }
        let url = config.registry_index(key)?;
        Ok(SourceId::for_alt_registry(&url, key))
    }

    pub fn is_path(&self) -> bool {
        self.kind == SourceKind::Path
    }

    pub fn local_path(&self) -> Option<PathBuf> {
        self.is_path().then(|| self.url.to_file_path().unwrap())
    }

    /// The URL with its protocol and, for git, its reference: what metadata prints.
    pub fn as_url(&self) -> SourceIdAsUrl<'_> {
        SourceIdAsUrl { id: self, encoded: false }
    }

    /// crates.io, through its git index or its sparse one.
    pub fn is_crates_io(&self) -> bool {
        matches!(self.kind, SourceKind::Registry | SourceKind::SparseRegistry)
            && matches!(self.url.as_str(), CRATES_IO_INDEX | CRATES_IO_SPARSE_INDEX)
    }
}

const CRATES_IO_SPARSE_INDEX: &str = "sparse+https://index.crates.io/";

/// cargo's `SourceId` display, as its messages name a source: a git URL with its reference, a
/// registry's name, a path.
impl fmt::Display for SourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            SourceKind::Git(reference) => {
                write!(f, "{}", self.url)?;
                if let Some(pretty) = reference.pretty_ref(true) {
                    write!(f, "?{pretty}")?;
                }
                Ok(())
            }
            SourceKind::Registry | SourceKind::SparseRegistry if self.is_crates_io() => {
                f.write_str("crates.io")
            }
            SourceKind::Registry | SourceKind::SparseRegistry => match &self.registry_key {
                Some(key) => f.write_str(key),
                None => url_display(&self.url, f),
            },
            _ => url_display(&self.url, f),
        }
    }
}

/// cargo's `url_display`: a `file:` URL as its path.
fn url_display(url: &Url, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    if url.scheme() == "file"
        && let Ok(path) = url.to_file_path()
        && let Some(path) = path.to_str()
    {
        return f.write_str(path);
    }
    f.write_str(url.as_str())
}

pub struct SourceIdAsUrl<'a> {
    id: &'a SourceId,
    encoded: bool,
}

impl fmt::Display for SourceIdAsUrl<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(protocol) = self.id.kind.protocol() {
            write!(f, "{protocol}+")?;
        }
        write!(f, "{}", self.id.url)?;
        if let SourceKind::Git(reference) = &self.id.kind
            && let Some(pretty) = reference.pretty_ref(self.encoded)
        {
            write!(f, "?{pretty}")?;
        }
        Ok(())
    }
}

impl serde::Serialize for SourceId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if self.is_path() { s.serialize_none() } else { s.collect_str(&self.as_url()) }
    }
}

/// A URL as cargo's `IntoUrl` parses one, with its hint for scp-like git addresses.
pub(crate) fn into_url(s: &str) -> anyhow::Result<Url> {
    Url::parse(s).map_err(|e| {
        if s.starts_with("git@") {
            anyhow!(
                "invalid url `{s}`: {e}; try using `{}` instead",
                format_args!("ssh://{}", s.replacen(':', "/", 1))
            )
        } else {
            anyhow!("invalid url `{s}`: {e}")
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::PackageId;

    fn id(source_id: SourceId) -> String {
        PackageId { name: "a".to_owned(), version: semver::Version::new(1, 2, 3), source_id }
            .to_string()
    }

    fn url(s: &str) -> Url {
        s.parse().unwrap()
    }

    /// Each kind of source as cargo's `PackageId` display names it in a message.
    #[test]
    fn a_package_reads_as_cargo_names_it() {
        let crates_io = SourceId::for_alt_registry(&url(CRATES_IO_INDEX), CRATES_IO_REGISTRY);
        assert!(crates_io.is_crates_io());
        assert_eq!(id(crates_io), "a v1.2.3");
        let sparse = SourceId::for_registry(&url(CRATES_IO_SPARSE_INDEX));
        assert!(sparse.is_crates_io());
        assert_eq!(id(sparse), "a v1.2.3");
        let named = SourceId::for_alt_registry(&url("https://r.example/index"), "mine");
        assert_eq!(id(named), "a v1.2.3 (mine)");
        let unnamed = SourceId::for_registry(&url("https://r.example/index"));
        assert_eq!(id(unnamed), "a v1.2.3 (https://r.example/index)");
        let branch = GitReference::Branch("dev".to_owned());
        assert_eq!(
            id(SourceId::for_git(&url("https://g.example/a"), branch)),
            "a v1.2.3 (https://g.example/a?branch=dev)"
        );
        let default = GitReference::DefaultBranch;
        assert_eq!(
            id(SourceId::for_git(&url("https://g.example/a"), default)),
            "a v1.2.3 (https://g.example/a)"
        );
        #[cfg(unix)]
        assert_eq!(id(SourceId::for_path(Path::new("/w/a")).unwrap()), "a v1.2.3 (/w/a)");
    }
}

//! A package as `cargo metadata` prints it, read as blaze-resolve's `Summary`, and the root
//! manifest's `[patch]` and `[replace]` tables: blaze-metadata's `summary.rs`, which v2 cannot
//! call (blaze-metadata depends on this crate), with the root manifest given as its text
//! rather than read from the disk.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use blaze_resolve::workspace_resolve::Patch;
use blaze_resolve::{
    DepKind, Dependency, GitReference, InternedString, PackageIdSpec, Platform, SourceId, Summary,
};
use serde_json::Value;
use url::Url;

const LIB_KINDS: &[&str] = &["lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro"];

fn str_of<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key].as_str().ok_or_else(|| anyhow!("no `{key}` in a package"))
}

/// What the resolve and its graph read of one package.
pub(crate) struct Read {
    pub summary: Summary,
    /// The crate name of its library, `-` as `_`.
    pub lib: Option<InternedString>,
    pub proc_macro_lib: bool,
    pub any_proc_macro: bool,
}

fn package_source(package: &Value) -> Result<SourceId> {
    match package["source"].as_str() {
        Some(source) => SourceId::from_url(source),
        None => SourceId::for_path(
            Path::new(str_of(package, "manifest_path")?).parent().context("a manifest's dir")?,
        ),
    }
}

fn dependency(dep: &Value) -> Result<Dependency> {
    let source = match (dep["source"].as_str(), dep["path"].as_str()) {
        (Some(source), _) => SourceId::from_url(source)?,
        (None, Some(path)) => SourceId::for_path(Path::new(path))?,
        (None, None) => SourceId::crates_io(),
    };
    // A path or git dependency without a `version` is written `*` but, unlike `version = "*"`,
    // matches prereleases.
    let req = match str_of(dep, "req")? {
        "*" if !source.is_registry() => None,
        req => Some(req),
    };
    let mut d = Dependency::parse(str_of(dep, "name")?, req, source)?;
    d.set_kind(match dep["kind"].as_str() {
        None => DepKind::Normal,
        Some("dev") => DepKind::Development,
        Some("build") => DepKind::Build,
        Some(kind) => return Err(anyhow!("dependency kind `{kind}`")),
    });
    if let Some(rename) = dep["rename"].as_str() {
        d.set_explicit_name_in_toml(rename);
    }
    d.set_optional(dep["optional"].as_bool().unwrap_or(false));
    d.set_default_features(dep["uses_default_features"].as_bool().unwrap_or(true));
    let features: Vec<&str> =
        dep["features"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    d.set_features(features);
    if let Some(target) = dep["target"].as_str() {
        d.set_platform(Some(target.parse::<Platform>()?));
    }
    Ok(d)
}

/// `package`, the JSON `cargo metadata` prints of it, read.
pub(crate) fn read(package: &Value) -> Result<Read> {
    let id = blaze_resolve::PackageId::try_new(
        str_of(package, "name")?,
        str_of(package, "version")?,
        package_source(package)?,
    )?;
    let deps = package["dependencies"]
        .as_array()
        .into_iter()
        .flatten()
        .map(dependency)
        .collect::<Result<Vec<_>>>()?;
    let features: BTreeMap<InternedString, Vec<InternedString>> = package["features"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(name, values)| {
            let values = values.as_array().into_iter().flatten().filter_map(Value::as_str);
            (InternedString::new(name), values.map(InternedString::new).collect())
        })
        .collect();
    let summary = Summary::new(id, deps, &features, package["links"].as_str(), None)?;
    let kinds = |target: &Value| -> Vec<String> {
        let kinds = target["kind"].as_array().into_iter().flatten().filter_map(Value::as_str);
        kinds.map(str::to_owned).collect()
    };
    let targets: Vec<&Value> = package["targets"].as_array().into_iter().flatten().collect();
    let lib = targets.iter().find(|it| kinds(it).iter().any(|k| LIB_KINDS.contains(&k.as_str())));
    Ok(Read {
        summary,
        lib: match lib {
            Some(lib) => Some(InternedString::new(&str_of(lib, "name")?.replace('-', "_"))),
            None => None,
        },
        proc_macro_lib: lib.is_some_and(|it| kinds(it).iter().any(|k| k == "proc-macro")),
        any_proc_macro: targets.iter().any(|it| kinds(it).iter().any(|k| k == "proc-macro")),
    })
}

/// `path` with `.` and `..` taken out, as cargo writes a path dependency's source.
pub(crate) fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

/// The root manifest, `text` being what it reads, as the tables below read it.
pub(crate) struct Root<'a> {
    pub dir: &'a Path,
    pub table: toml::Table,
}

impl<'a> Root<'a> {
    pub fn new(dir: &'a Path, text: &str) -> Result<Root<'a>> {
        Ok(Root { dir, table: toml::from_str(text).with_context(|| dir.join("Cargo.toml").display().to_string())? })
    }
}

/// One entry of a `[patch]` table of the root manifest.
fn patch_dependency(root: &Path, name: &str, entry: &toml::Value) -> Result<Option<Dependency>> {
    let get = |key: &str| entry.get(key).and_then(|v| v.as_str());
    let (version, source) = match entry {
        toml::Value::String(version) => (Some(version.as_str()), SourceId::crates_io()),
        _ if get("registry").is_some() => return Ok(None),
        _ => {
            let source = if let Some(path) = get("path") {
                SourceId::for_path(&normalize(&root.join(path)))?
            } else if let Some(git) = get("git") {
                let reference = match (get("branch"), get("tag"), get("rev")) {
                    (Some(b), _, _) => GitReference::Branch(b.to_owned()),
                    (_, Some(t), _) => GitReference::Tag(t.to_owned()),
                    (_, _, Some(r)) => GitReference::Rev(r.to_owned()),
                    _ => GitReference::DefaultBranch,
                };
                SourceId::for_git(&git.parse()?, reference)?
            } else {
                SourceId::crates_io()
            };
            (get("version"), source)
        }
    };
    let mut dep = Dependency::parse(get("package").unwrap_or(name), version, source)?;
    if get("package").is_some() {
        dep.set_explicit_name_in_toml(name);
    }
    Ok(Some(dep))
}

/// The directories of the path packages the root manifest's `[patch]` names.
pub(crate) fn patch_paths(root: &Root<'_>) -> Vec<PathBuf> {
    let tables = root.table.get("patch").and_then(|p| p.as_table()).into_iter().flatten();
    let entries = tables.flat_map(|(_, table)| table.as_table().into_iter().flatten());
    let paths = entries.filter_map(|(_, entry)| entry.get("path")?.as_str());
    paths.map(|path| normalize(&root.dir.join(path))).collect()
}

/// The root manifest's `[patch]`, without entries that name an alternative registry.
pub(crate) fn patch_tables(root: &Root<'_>) -> Result<Vec<(Url, Vec<Patch>)>> {
    let manifest_path = root.dir.join("Cargo.toml");
    let mut tables = Vec::new();
    for (key, table) in root.table.get("patch").and_then(|p| p.as_table()).into_iter().flatten() {
        let url = if key == "crates-io" {
            SourceId::crates_io().url().clone()
        } else {
            match key.parse::<Url>() {
                Ok(url) => url,
                Err(_) => continue,
            }
        };
        let mut patches = Vec::new();
        for (name, entry) in table.as_table().into_iter().flatten() {
            let Some(dep) = patch_dependency(root.dir, name, entry)? else { continue };
            patches.push(Patch { dep, loc: manifest_path.display().to_string() });
        }
        tables.push((url, patches));
    }
    Ok(tables)
}

/// The root manifest's `[replace]`, as cargo 1.99's `toml::replace` reads it: each key a spec
/// with a version (its source crates.io unless it names one), each value the source it is
/// replaced with, required at exactly that version.
pub(crate) fn replace_table(root: &Root<'_>) -> Result<Vec<(PackageIdSpec, Dependency)>> {
    let Some(table) = root.table.get("replace").and_then(|r| r.as_table()) else {
        return Ok(Vec::new());
    };
    if root.table.contains_key("patch") {
        bail!("cannot specify both [replace] and [patch]");
    }
    let mut replace = Vec::new();
    for (key, entry) in table {
        let mut spec = PackageIdSpec::parse(key).with_context(|| {
            format!(
                "replacements must specify a valid semver version to replace, but `{key}` does not"
            )
        })?;
        if spec.url().is_none() {
            spec = spec.with_url(SourceId::crates_io().url().clone());
        }
        if entry.is_str() || entry.get("version").is_some() {
            bail!("replacements cannot specify a version requirement, but found one for `{spec}`");
        }
        if entry.get("registry").is_some() {
            bail!("blaze reads no `registry` in `[replace]` (for `{spec}`)");
        }
        let version = spec.version().ok_or_else(|| {
            anyhow!("replacements must specify a version to replace, but `{spec}` does not")
        })?;
        let mut entry = entry.clone();
        if let Some(it) = entry.as_table_mut() {
            it.insert("version".to_owned(), toml::Value::String(format!("={version}")));
        }
        let dep = patch_dependency(root.dir, spec.name(), &entry)?.context("a `[replace]` entry")?;
        replace.push((spec, dep));
    }
    Ok(replace)
}

/// The directories of the path packages the root manifest's `[replace]` names.
pub(crate) fn replace_paths(replace: &[(PackageIdSpec, Dependency)]) -> Vec<PathBuf> {
    replace.iter().filter_map(|(_, dep)| dep.source_id().local_path()).collect()
}

/// The directories of a package's path dependencies, from its `cargo metadata` JSON.
pub(crate) fn path_dependencies(package: &Value) -> Vec<PathBuf> {
    let deps = package["dependencies"].as_array().into_iter().flatten();
    deps.filter(|it| it["source"].is_null())
        .filter_map(|it| it["path"].as_str())
        .map(|it| normalize(Path::new(it)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn replace_of(table: &str) -> Result<Vec<(PackageIdSpec, Dependency)>> {
        replace_table(&Root::new(Path::new("/w"), &format!("[workspace]\n\n[replace]\n{table}"))?)
    }

    #[test]
    fn a_name_and_version_is_a_crates_io_package_replaced_at_exactly_that_version() {
        let replace = replace_of("\"foo:0.1.0\" = { path = \"vendor/../foo\" }").unwrap();
        let [(spec, dep)] = &replace[..] else { panic!("{replace:?}") };
        assert_eq!(spec.name(), "foo");
        assert_eq!(spec.version(), Some(semver::Version::new(0, 1, 0)));
        assert_eq!(spec.url(), Some(SourceId::crates_io().url()));
        assert_eq!(dep.version_req().to_string(), "=0.1.0");
        assert_eq!(dep.source_id().local_path(), Some(PathBuf::from("/w/foo")));
        assert_eq!(replace_paths(&replace), vec![PathBuf::from("/w/foo")]);
    }

    #[test]
    fn what_cargo_refuses_is_refused() {
        let err = |table| replace_of(table).unwrap_err().to_string();
        assert_eq!(
            err("foo = { path = \"foo\" }"),
            "replacements must specify a version to replace, but \
             `https://github.com/rust-lang/crates.io-index#foo` does not"
        );
        let both = "[workspace]\n[patch.crates-io]\nfoo = { path = \"foo\" }\n[replace]\n\"foo:0.1.0\" = { path = \"foo\" }";
        let root = Root::new(Path::new("/w"), both).unwrap();
        assert_eq!(replace_table(&root).unwrap_err().to_string(), "cannot specify both [replace] and [patch]");
    }

    #[test]
    fn patch_paths_are_the_roots() {
        let root = Root::new(Path::new("/w"), "[patch.crates-io]\na = { path = \"vendor/a\" }\nb = \"1\"\n").unwrap();
        assert_eq!(patch_paths(&root), vec![PathBuf::from("/w/vendor/a")]);
        assert_eq!(patch_tables(&root).unwrap()[0].1.len(), 2);
    }
}

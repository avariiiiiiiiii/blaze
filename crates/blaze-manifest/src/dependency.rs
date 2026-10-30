//! A dependency as a manifest declares it (cargo's `Dependency`), and the feature map of a
//! package (cargo's `build_feature_map`), with cargo's checks and messages.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use anyhow::{Context as _, bail};
use cargo_platform::Platform;
use cargo_util_schemas::manifest::FeatureName;
use rustc_hash::{FxHashMap, FxHashSet};
use semver::VersionReq;
use serde::Serialize;

use crate::edit_distance::closest_msg;
use crate::source::SourceId;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DepKind {
    Normal,
    Development,
    Build,
}

impl DepKind {
    pub fn kind_table(&self) -> &'static str {
        match self {
            DepKind::Normal => "dependencies",
            DepKind::Development => "dev-dependencies",
            DepKind::Build => "build-dependencies",
        }
    }
}

impl Serialize for DepKind {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            DepKind::Normal => None,
            DepKind::Development => Some("dev"),
            DepKind::Build => Some("build"),
        }
        .serialize(s)
    }
}

#[derive(Clone, Debug)]
pub struct Dependency {
    /// The name of the package depended on, which is what `name_in_toml` renames.
    pub name: String,
    pub source_id: SourceId,
    /// The registry named by `registry` or `registry-index`, when one is.
    pub registry_id: Option<SourceId>,
    /// `None` is any version, cargo's `OptVersionReq::Any`.
    pub req: Option<VersionReq>,
    pub kind: DepKind,
    pub optional: bool,
    pub public: bool,
    pub features: Vec<String>,
    pub default_features: bool,
    pub platform: Option<Platform>,
    pub explicit_name_in_toml: Option<String>,
    /// `artifact = ..` with `-Z bindeps`.
    pub artifact: Option<Artifact>,
}

/// An artifact dependency's `artifact`, `lib` and `target`, as cargo's `Artifact::parse` checks
/// them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Artifact {
    /// As written: `bin`, `bin:<name>`, `cdylib`, `staticlib`.
    pub kinds: Vec<String>,
    pub lib: bool,
    /// `target = "<triple>"`, or `"target"` for the build's own.
    pub target: Option<String>,
}

impl Artifact {
    pub fn parse(kinds: &[String], lib: bool, target: Option<&str>) -> anyhow::Result<Artifact> {
        for kind in kinds {
            if !matches!(kind.as_str(), "bin" | "cdylib" | "staticlib") && !kind.starts_with("bin:") {
                bail!("'{kind}' is not a valid artifact specifier");
            }
        }
        if kinds.iter().any(|it| it == "bin") && kinds.iter().any(|it| it.starts_with("bin:")) {
            bail!(
                "Cannot specify both 'bin' and 'bin:<name>' binary artifacts, as 'bin' selects all available binaries."
            );
        }
        let mut unique: Vec<&str> = kinds.iter().map(String::as_str).collect();
        unique.sort_unstable();
        unique.dedup();
        let dupes = kinds.len() - unique.len();
        if dupes != 0 {
            bail!("Found {dupes} duplicate binary artifact{}", if dupes > 1 { "s" } else { "" });
        }
        if target == Some("") {
            bail!("target was empty");
        }
        Ok(Artifact { kinds: kinds.to_vec(), lib, target: target.map(str::to_owned) })
    }
}

impl Dependency {
    pub fn parse(name: &str, version: Option<&str>, source_id: SourceId) -> anyhow::Result<Self> {
        let req = version
            .map(|v| {
                VersionReq::parse(v).with_context(|| {
                    format!("failed to parse the version requirement `{v}` for dependency `{name}`")
                })
            })
            .transpose()?;
        Ok(Dependency {
            name: name.to_owned(),
            source_id,
            registry_id: None,
            req,
            kind: DepKind::Normal,
            optional: false,
            public: false,
            features: Vec::new(),
            default_features: true,
            platform: None,
            explicit_name_in_toml: None,
            artifact: None,
        })
    }

    /// What the manifest calls it: the key in its dependency table.
    pub fn name_in_toml(&self) -> &str {
        self.explicit_name_in_toml.as_deref().unwrap_or(&self.name)
    }

    /// Whether it is built for the package's own targets, which dev-dependencies are not.
    pub fn is_transitive(&self) -> bool {
        self.kind != DepKind::Development
    }

    pub fn serialized(&self) -> SerializedDependency<'_> {
        SerializedDependency {
            name: &self.name,
            source: (!self.source_id.is_path()).then(|| self.source_id.as_url().to_string()),
            req: match &self.req {
                Some(req) => req.to_string(),
                None => "*".to_owned(),
            },
            kind: self.kind,
            rename: self.explicit_name_in_toml.as_deref(),
            optional: self.optional,
            uses_default_features: self.default_features,
            features: &self.features,
            artifact: self.artifact.as_ref(),
            target: self.platform.as_ref(),
            registry: self.registry_id.map(|id| id.url().to_string()),
            path: self.source_id.local_path(),
        }
    }
}

/// A dependency as `cargo metadata` prints it, fields in cargo's order.
#[derive(Serialize)]
pub struct SerializedDependency<'a> {
    pub name: &'a str,
    pub source: Option<String>,
    pub req: String,
    pub kind: DepKind,
    pub rename: Option<&'a str>,
    pub optional: bool,
    pub uses_default_features: bool,
    pub features: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact: Option<&'a Artifact>,
    pub target: Option<&'a Platform>,
    pub registry: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
}

/// One entry of a feature's list.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FeatureValue {
    Feature(String),
    Dep { dep_name: String },
    DepFeature { dep_name: String, dep_feature: String, weak: bool },
}

impl FeatureValue {
    pub fn new(feature: &str) -> FeatureValue {
        match feature.split_once('/') {
            Some((dep, dep_feat)) => {
                let dep_name = dep.strip_suffix('?');
                FeatureValue::DepFeature {
                    dep_name: dep_name.unwrap_or(dep).to_owned(),
                    dep_feature: dep_feat.to_owned(),
                    weak: dep_name.is_some(),
                }
            }
            None => match feature.strip_prefix("dep:") {
                Some(dep_name) => FeatureValue::Dep { dep_name: dep_name.to_owned() },
                None => FeatureValue::Feature(feature.to_owned()),
            },
        }
    }

    fn explicit_dep_name(&self) -> Option<&str> {
        match self {
            FeatureValue::Dep { dep_name } => Some(dep_name),
            _ => None,
        }
    }

    fn feature_or_dep_name(&self) -> &str {
        match self {
            FeatureValue::Feature(name) => name,
            FeatureValue::Dep { dep_name } | FeatureValue::DepFeature { dep_name, .. } => dep_name,
        }
    }
}

impl fmt::Display for FeatureValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FeatureValue::Feature(feat) => write!(f, "{feat}"),
            FeatureValue::Dep { dep_name } => write!(f, "dep:{dep_name}"),
            FeatureValue::DepFeature { dep_name, dep_feature, weak } => {
                let weak = if *weak { "?" } else { "" };
                write!(f, "{dep_name}{weak}/{dep_feature}")
            }
        }
    }
}

pub type FeatureMap = BTreeMap<String, Vec<FeatureValue>>;

/// The checks of cargo's `Summary::new`, then its feature map: the declared features, plus one
/// implicit feature per optional dependency that no `dep:` names.
pub fn build_summary_features(
    features: &BTreeMap<String, Vec<String>>,
    dependencies: &[Dependency],
) -> anyhow::Result<FeatureMap> {
    for dep in dependencies {
        if dep.optional && !dep.is_transitive() {
            bail!("dev-dependencies are not allowed to be optional: `{}`", dep.name_in_toml());
        }
    }

    let mut dep_map: FxHashMap<&str, bool> = FxHashMap::default();
    for dep in dependencies {
        *dep_map.entry(dep.name_in_toml()).or_insert(false) |= dep.optional;
    }

    let mut map: FeatureMap = features
        .iter()
        .map(|(feature, list)| {
            (feature.clone(), list.iter().map(|v| FeatureValue::new(v)).collect())
        })
        .collect();

    let explicitly_listed: FxHashSet<String> =
        map.values().flatten().filter_map(|fv| fv.explicit_dep_name()).map(str::to_owned).collect();

    for dep in dependencies {
        if !dep.optional {
            continue;
        }
        let dep_name = dep.name_in_toml();
        if features.contains_key(dep_name) || explicitly_listed.contains(dep_name) {
            continue;
        }
        map.insert(dep_name.to_owned(), vec![FeatureValue::Dep { dep_name: dep_name.to_owned() }]);
    }

    for (feature, fvs) in &map {
        FeatureName::new(feature.as_str())?;
        for fv in fvs {
            let dep_data = dep_map.get(fv.feature_or_dep_name());
            let is_any_dep = dep_data.is_some();
            let is_optional_dep = dep_data.is_some_and(|&o| o);
            match fv {
                FeatureValue::Feature(f) => {
                    if !features.contains_key(f) {
                        if !is_any_dep {
                            let closest = closest_msg(f, features.keys(), |k| k, "feature");
                            bail!(
                                "feature `{feature}` includes `{fv}` which is neither a dependency \
                                 nor another feature{closest}"
                            );
                        }
                        if is_optional_dep {
                            if !map.contains_key(f) {
                                bail!(
                                    "feature `{feature}` includes `{fv}`, but `{f}` is an \
                                     optional dependency without an implicit feature\n\
                                     Use `dep:{f}` to enable the dependency."
                                );
                            }
                        } else {
                            bail!(
                                "feature `{feature}` includes `{fv}`, but `{f}` is not an optional dependency\n\
                                 A non-optional dependency of the same name is defined; \
                                 consider adding `optional = true` to its definition."
                            );
                        }
                    }
                }
                FeatureValue::Dep { dep_name } => {
                    if !is_any_dep {
                        bail!(
                            "feature `{feature}` includes `{fv}`, but `{dep_name}` is not listed as a dependency"
                        );
                    }
                    if !is_optional_dep {
                        bail!(
                            "feature `{feature}` includes `{fv}`, but `{dep_name}` is not an optional dependency\n\
                             A non-optional dependency of the same name is defined; \
                             consider adding `optional = true` to its definition."
                        );
                    }
                }
                FeatureValue::DepFeature { dep_name, dep_feature, weak } => {
                    if dep_feature.contains('/') {
                        bail!(
                            "multiple slashes in feature `{fv}` (included by feature `{feature}`) are not allowed"
                        );
                    }
                    if let Some(stripped_dep) = dep_name.strip_prefix("dep:") {
                        let has_other_dep = explicitly_listed.contains(stripped_dep);
                        let is_optional = dep_map.get(stripped_dep).is_some_and(|&o| o);
                        let extra_help = if *weak || has_other_dep || !is_optional {
                            String::new()
                        } else {
                            format!(
                                "\nIf the intent is to avoid creating an implicit feature \
                                 `{stripped_dep}` for an optional dependency, \
                                 then consider replacing this with two values:\n    \
                                 \"dep:{stripped_dep}\", \"{stripped_dep}/{dep_feature}\""
                            )
                        };
                        bail!(
                            "feature `{feature}` includes `{fv}` with both `dep:` and `/`\n\
                             To fix this, remove the `dep:` prefix.{extra_help}"
                        )
                    }
                    if !is_any_dep {
                        bail!(
                            "feature `{feature}` includes `{fv}`, but `{dep_name}` is not a dependency"
                        );
                    }
                    if *weak && !is_optional_dep {
                        bail!(
                            "feature `{feature}` includes `{fv}` with a `?`, but `{dep_name}` is not an optional dependency\n\
                             A non-optional dependency of the same name is defined; \
                             consider removing the `?` or changing the dependency to be optional"
                        );
                    }
                }
            }
        }
    }

    let used: FxHashSet<&str> = map
        .values()
        .flatten()
        .filter_map(|fv| match fv {
            FeatureValue::Dep { dep_name } | FeatureValue::DepFeature { dep_name, .. } => {
                Some(dep_name.as_str())
            }
            FeatureValue::Feature(_) => None,
        })
        .collect();
    if let Some((dep, _)) =
        dep_map.iter().find(|&(dep, &is_optional)| is_optional && !used.contains(dep))
    {
        bail!(
            "optional dependency `{dep}` is not included in any feature\n\
             Make sure that `dep:{dep}` is included in one of features in the [features] table."
        );
    }

    Ok(map)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::files::tests::Mem;

    fn kinds(kinds: &[&str]) -> Vec<String> {
        kinds.iter().map(|it| (*it).to_owned()).collect()
    }

    #[test]
    fn an_artifact_takes_cargo_s_kinds() {
        let artifact = Artifact::parse(&kinds(&["bin:tool", "cdylib", "staticlib"]), true, Some("target"))
            .unwrap();
        assert_eq!(artifact.kinds, ["bin:tool", "cdylib", "staticlib"]);
        assert!(artifact.lib);
        assert_eq!(artifact.target.as_deref(), Some("target"));
    }

    #[test]
    fn an_artifact_refuses_what_cargo_refuses() {
        let error = |list: &[&str], target: Option<&str>| {
            Artifact::parse(&kinds(list), false, target).unwrap_err().to_string()
        };
        assert_eq!(error(&["rlib"], None), "'rlib' is not a valid artifact specifier");
        assert_eq!(error(&[""], None), "'' is not a valid artifact specifier");
        assert_eq!(
            error(&["bin", "bin:a"], None),
            "Cannot specify both 'bin' and 'bin:<name>' binary artifacts, as 'bin' selects all available binaries."
        );
        assert_eq!(error(&["cdylib", "cdylib"], None), "Found 1 duplicate binary artifact");
        assert_eq!(error(&["bin", "bin", "bin"], None), "Found 2 duplicate binary artifacts");
        assert_eq!(error(&["bin"], Some("")), "target was empty");
    }

    const ROOT: &str = "/blaze-nowhere/bindeps";

    fn files(config: &str, dependency: &str) -> Mem {
        Mem::default()
            .dir(&format!("{ROOT}/home"))
            .file(&format!("{ROOT}/.cargo/config.toml"), config)
            .file(
                &format!("{ROOT}/Cargo.toml"),
                &format!(
                    "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
                     [build-dependencies]\nb = {{ path = \"b\", {dependency} }}\n"
                ),
            )
            .file(&format!("{ROOT}/src/lib.rs"), "")
            .file(
                &format!("{ROOT}/b/Cargo.toml"),
                "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            )
            .file(&format!("{ROOT}/b/src/main.rs"), "")
    }

    fn config(files: Mem, flags: &[&str]) -> anyhow::Result<crate::Config<Mem>> {
        let env = [("CARGO_HOME".to_owned(), format!("{ROOT}/home"))];
        let mut config = crate::Config::load_with(Path::new(ROOT), env, files)?;
        config.cli_unstable(&kinds(flags))?;
        Ok(config)
    }

    fn metadata(files: Mem, flags: &[&str]) -> anyhow::Result<String> {
        let ctx = crate::Context::new(config(files, flags)?);
        let manifest = crate::metadata::root_manifest(None, &ctx)?;
        let ws = crate::Workspace::new(&manifest, &ctx)?;
        Ok(serde_json::to_string(&crate::metadata::export_no_deps(&ws))?)
    }

    #[test]
    fn the_unstable_table_comes_first_and_each_flag_after_it() {
        let table = "[unstable]\nbindeps = true\nbuild-std = [\"std,core\", \"alloc\"]\n";
        let read = config(files(table, "artifact = \"bin\""), &[]).unwrap();
        assert!(read.bindeps());
        assert_eq!(read.unstable().build_std.as_deref(), Some(&kinds(&["std", "core", "alloc"])[..]));
        let read = config(files(table, "artifact = \"bin\""), &["bindeps=no", "build_std=core"]).unwrap();
        assert!(!read.bindeps());
        assert_eq!(read.unstable().build_std.as_deref(), Some(&kinds(&["core"])[..]));
        let read = config(files("", "artifact = \"bin\""), &["unstable-options"]).unwrap();
        assert!(read.unstable().unstable_options && !read.bindeps());
    }

    #[test]
    fn a_flag_blaze_does_not_read_is_an_error_naming_it() {
        let error = |flags: &[&str]| {
            config(files("", "artifact = \"bin\""), flags).map(drop).unwrap_err().to_string()
        };
        assert_eq!(error(&["minimal-versions"]), "blaze does not read `-Z minimal-versions` yet");
        assert_eq!(error(&["bindeps=maybe"]), "flag -Zbindeps expected `no` or `yes`, found: `maybe`");
    }

    #[test]
    fn an_artifact_dependency_needs_bindeps() {
        let error = metadata(files("", "artifact = \"bin\""), &[]).unwrap_err();
        assert!(format!("{error:#}").contains("`artifact = …` requires `-Z bindeps` (b)"), "{error:#}");
        let error = metadata(files("", "lib = true"), &[]).unwrap_err();
        assert!(
            format!("{error:#}").contains("'lib' specifier cannot be used without an 'artifact = …' value (b)"),
            "{error:#}"
        );
    }

    #[test]
    fn with_bindeps_cargo_metadata_says_the_artifact() {
        let json = metadata(files("", "artifact = [\"bin\"], target = \"target\""), &["bindeps"]).unwrap();
        assert!(
            json.contains(r#""features":[],"artifact":{"kinds":["bin"],"lib":false,"target":"target"},"target":null"#),
            "{json}"
        );
        let plain = metadata(files("", "version = \"0.1\""), &[]).unwrap();
        assert!(!plain.contains("\"artifact\""), "{plain}");
    }
}

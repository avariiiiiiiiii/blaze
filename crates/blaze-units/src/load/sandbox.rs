//! The ways out of the sandbox a workspace's manifests name (docs/cargo.md, "Build scripts and
//! proc macros run in a sandbox"), read from the manifests the load already parsed, as v1's
//! `blaze-cargo/src/cargo/sandboxed.rs` reads them from the disk: the workspace's root manifest
//! names packages by name (the only place one from a registry can be named), and a member may
//! take its own build script out. Any other key, or a value of another type, is an error.

use std::path::Path;

use crate::query::LoadError;

/// The table of the workspace's root manifest.
pub(crate) const WORKSPACE_TABLE: &str = "[workspace.metadata.blaze.sandbox]";

/// The table of a package's own manifest.
pub(crate) const PACKAGE_TABLE: &str = "[package.metadata.blaze.sandbox]";

/// What the workspace's root manifest says in [`WORKSPACE_TABLE`] that the load hands on.
#[derive(Default, Debug, PartialEq, Eq)]
pub(crate) struct WorkspaceOptOut {
    /// `unsandboxed-build-scripts`: packages by name, sorted.
    pub build_scripts: Vec<String>,
    /// `build-script-link-dirs`: absolute (a relative one is under the workspace's root),
    /// normal, sorted.
    pub link_dirs: Vec<String>,
}

impl WorkspaceOptOut {
    pub fn unsandboxed(&self, package: &str) -> bool {
        self.build_scripts.binary_search_by(|it| it.as_str().cmp(package)).is_ok()
    }
}

/// `metadata`'s `blaze.sandbox` table: none when there is no such table.
fn sandbox_table(metadata: Option<&toml::Value>) -> Option<&toml::Table> {
    metadata?.get("blaze")?.as_table()?.get("sandbox")?.as_table()
}

fn wrong(table: &str, manifest: &Path, what: &str) -> LoadError {
    LoadError { what: format!("{table} in {}: {what}", manifest.display()), at: Some(manifest.display().to_string()) }
}

/// What `[workspace.metadata]` (`metadata`) of the root manifest at `manifest` says in
/// [`WORKSPACE_TABLE`].
pub(crate) fn workspace(metadata: Option<&toml::Value>, manifest: &Path) -> Result<WorkspaceOptOut, LoadError> {
    let mut out = WorkspaceOptOut::default();
    let Some(table) = sandbox_table(metadata) else { return Ok(out) };
    let wrong = |what: &str| wrong(WORKSPACE_TABLE, manifest, what);
    let root = manifest.parent().unwrap_or(Path::new("/"));
    for (key, value) in table {
        let names = || -> Result<Vec<&str>, LoadError> {
            let list = value.as_array().ok_or_else(|| wrong(&format!("`{key}` is a list")))?;
            list.iter().map(|it| it.as_str().ok_or_else(|| wrong(&format!("`{key}` lists names, as strings")))).collect()
        };
        match key.as_str() {
            "unsandboxed-build-scripts" => out.build_scripts = names()?.into_iter().map(str::to_owned).collect(),
            "build-script-link-dirs" => {
                for dir in names()? {
                    let absolute = super::files::normal(&root.join(dir))
                        .ok_or_else(|| wrong(&format!("`{dir}` of `build-script-link-dirs` is not a UTF-8 path")))?;
                    out.link_dirs.push(absolute);
                }
            }
            // The daemon's or another experiment's: checked here, read where they are used.
            "unsandboxed-proc-macros" | "build-script-env" | "wasi-build-scripts" => {
                names()?;
            }
            other => {
                return Err(wrong(&format!(
                    "`{other}` is not known (`unsandboxed-build-scripts`, `unsandboxed-proc-macros`, \
                     `build-script-env`, `build-script-link-dirs` and `wasi-build-scripts` are)"
                )));
            }
        }
    }
    out.build_scripts.sort_unstable();
    out.build_scripts.dedup();
    out.link_dirs.sort_unstable();
    out.link_dirs.dedup();
    Ok(out)
}

/// Whether `[package.metadata]` (`metadata`) of the member whose manifest is `manifest` says
/// `build-script = false` in [`PACKAGE_TABLE`]. Its other keys are its tests' and are checked
/// here too.
pub(crate) fn build_script_outside(metadata: Option<&toml::Value>, manifest: &Path) -> Result<bool, LoadError> {
    let Some(table) = sandbox_table(metadata) else { return Ok(false) };
    let wrong = |what: &str| wrong(PACKAGE_TABLE, manifest, what);
    let mut outside = false;
    for (key, value) in table {
        match key.as_str() {
            "tests" => {
                value.as_bool().ok_or_else(|| wrong("`tests` is true or false"))?;
            }
            "unsandboxed-tests" => {
                let names = value.as_array().ok_or_else(|| wrong("`unsandboxed-tests` is a list"))?;
                if names.iter().any(|it| !it.is_str()) {
                    return Err(wrong("`unsandboxed-tests` lists tests by their names, as strings"));
                }
            }
            "build-script" => outside = !value.as_bool().ok_or_else(|| wrong("`build-script` is true or false"))?,
            other => {
                return Err(wrong(&format!(
                    "`{other}` is not known (`tests`, `unsandboxed-tests` and `build-script` are)"
                )));
            }
        }
    }
    Ok(outside)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata(text: &str) -> toml::Value {
        toml::Value::Table(toml::from_str(text).unwrap())
    }

    #[test]
    fn the_workspace_names_packages_and_link_dirs() {
        let manifest = Path::new("/w/Cargo.toml");
        let value = metadata(
            "[blaze.sandbox]\nunsandboxed-build-scripts = [\"z-sys\", \"a-sys\"]\n\
             build-script-link-dirs = [\"/opt/lib\", \"vendor/../lib\"]\nbuild-script-env = [\"OPENSSL_DIR\"]\n",
        );
        let out = workspace(Some(&value), manifest).unwrap();
        assert_eq!(out.build_scripts, ["a-sys", "z-sys"]);
        assert_eq!(out.link_dirs, ["/opt/lib", "/w/lib"]);
        assert!(out.unsandboxed("z-sys") && !out.unsandboxed("z"));
        assert_eq!(workspace(None, manifest).unwrap(), WorkspaceOptOut::default());
        assert_eq!(workspace(Some(&metadata("[other]\nx = 1\n")), manifest).unwrap(), WorkspaceOptOut::default());
    }

    #[test]
    fn a_table_said_otherwise_is_an_error() {
        let manifest = Path::new("/w/Cargo.toml");
        let unknown = workspace(Some(&metadata("[blaze.sandbox]\nbuild-scripts = []\n")), manifest).unwrap_err();
        assert!(unknown.what.starts_with("[workspace.metadata.blaze.sandbox] in /w/Cargo.toml: `build-scripts`"), "{}", unknown.what);
        let not_list = workspace(Some(&metadata("[blaze.sandbox]\nunsandboxed-build-scripts = \"a\"\n")), manifest).unwrap_err();
        assert!(not_list.what.ends_with("`unsandboxed-build-scripts` is a list"), "{}", not_list.what);
        let member = Path::new("/w/a/Cargo.toml");
        let not_bool = build_script_outside(Some(&metadata("[blaze.sandbox]\nbuild-script = \"no\"\n")), member).unwrap_err();
        assert!(not_bool.what.ends_with("`build-script` is true or false"), "{}", not_bool.what);
    }

    #[test]
    fn a_member_takes_its_build_script_out() {
        let member = Path::new("/w/a/Cargo.toml");
        assert!(build_script_outside(Some(&metadata("[blaze.sandbox]\nbuild-script = false\n")), member).unwrap());
        assert!(!build_script_outside(Some(&metadata("[blaze.sandbox]\nbuild-script = true\ntests = false\n")), member).unwrap());
        assert!(!build_script_outside(None, member).unwrap());
    }
}

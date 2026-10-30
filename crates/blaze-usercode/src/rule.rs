//! Cargo's rule, for a run that read what can't be traced (#1450): the clock, randomness,
//! the network, a descriptor it was given. What it read of those can't be seen again, so it
//! is rerun when cargo would rerun it, as well as when anything it was seen to read changed:
//! when a path its `rerun-if-changed` names changed (a directory meaning all below it), or,
//! with none named, any file of its package; and when a variable its `rerun-if-env-changed`
//! names changed. Cargo's rule is added to what it was seen to read, never put in its place:
//! a run is never kept for longer than either would keep it.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

use blaze_core::Traced;

use crate::trace::{self, Inputs, PACKAGE, TREE};

/// What cargo watches of a run.
#[derive(Clone, PartialEq, Eq, Default, Debug)]
pub struct CargoRule {
    /// The paths `rerun-if-changed` named, made whole against the package's directory.
    pub paths: Vec<PathBuf>,
    /// The package's directory, when no `rerun-if-changed` was printed: all of it is watched.
    pub package: Option<PathBuf>,
    /// The variables `rerun-if-env-changed` named.
    pub env: Vec<OsString>,
}

/// Adds cargo's rule to what `traced` has, as `inputs` has it now, when the run read what
/// can't be traced; leaves it as it is otherwise.
pub fn widen<I: Inputs>(traced: &mut Traced, rule: &CargoRule, inputs: &mut I) -> Result<(), I::Error> {
    if traced.untraceable.is_empty() {
        return Ok(());
    }
    for path in &rule.paths {
        match inputs.names(path)? {
            // Cargo watches a directory's every file, at any depth.
            Some(_) => {
                let at = path.join(TREE);
                if let Some(hash) = trace::walk(inputs, path, false)? {
                    traced.dirs.push((at, hash));
                }
            }
            None => {
                let hash = inputs.file(path)?;
                traced.files.push((path.clone(), hash));
            }
        }
    }
    if let Some(package) = &rule.package {
        match trace::package(inputs, package)? {
            Some(hash) => traced.dirs.push((package.join(PACKAGE), hash)),
            None => traced.files.push((package.join(PACKAGE), None)),
        }
    }
    for name in &rule.env {
        let value = inputs.env(name);
        traced.env.push((name.clone(), value));
    }
    trace::normalize(traced);
    // A path both read and watched is one input: the watched entry is the read one's value.
    traced.files.dedup_by(|a, b| a.0 == b.0);
    traced.env.dedup_by(|a, b| a.0 == b.0);
    Ok(())
}

/// The rule of a script whose output named `rerun_if_changed` and `rerun_if_env_changed`, in
/// its package at `package_dir`.
pub fn of_directives(
    package_dir: &Path,
    rerun_if_changed: &[PathBuf],
    rerun_if_env_changed: &[OsString],
) -> CargoRule {
    CargoRule {
        paths: rerun_if_changed.iter().map(|it| crate::index::whole(package_dir, it)).collect(),
        package: rerun_if_changed.is_empty().then(|| package_dir.to_owned()),
        env: rerun_if_env_changed.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, ffi::OsStr};

    use blaze_core::Untraceable;

    use super::*;
    use crate::{index::Shape, trace::DIRECTORY};

    #[derive(Default)]
    struct Fixed {
        files: BTreeMap<PathBuf, [u8; 32]>,
        dirs: BTreeMap<PathBuf, Vec<&'static str>>,
        env: Vec<(OsString, OsString)>,
    }

    impl Inputs for Fixed {
        type Error = String;
        fn file(&mut self, path: &Path) -> Result<Option<[u8; 32]>, String> {
            if self.dirs.contains_key(path) {
                return Ok(Some(DIRECTORY));
            }
            Ok(self.files.get(path).copied())
        }
        fn dir(&mut self, path: &Path) -> Result<Option<[u8; 32]>, String> {
            Ok(self.dirs.get(path).map(|names| trace::listing(names.iter().map(OsStr::new))))
        }
        fn env(&mut self, name: &OsStr) -> Option<OsString> {
            self.env.iter().find(|it| it.0 == name).map(|it| it.1.clone())
        }
        fn env_all(&mut self) -> Vec<(OsString, OsString)> {
            self.env.clone()
        }
        fn names(&mut self, path: &Path) -> Result<Option<Vec<(OsString, bool)>>, String> {
            let Some(names) = self.dirs.get(path) else { return Ok(None) };
            let mut out: Vec<(OsString, bool)> = names
                .iter()
                .map(|name| (OsString::from(name), self.dirs.contains_key(&path.join(name))))
                .collect();
            out.sort();
            Ok(Some(out))
        }
    }

    fn fixed() -> Fixed {
        Fixed {
            files: [
                ("/p/build.rs".into(), [1; 32]),
                ("/p/src/lib.rs".into(), [2; 32]),
                ("/p/data/a".into(), [3; 32]),
            ]
            .into(),
            dirs: [("/p".into(), vec!["build.rs", "src", "data"]), ("/p/src".into(), vec!["lib.rs"]), ("/p/data".into(), vec!["a"])]
                .into(),
            env: vec![("VERSION".into(), "1".into())],
        }
    }

    #[test]
    fn a_run_traced_whole_keeps_only_what_it_read() {
        let mut traced = Traced { files: vec![("/p/build.rs".into(), Some([1; 32]))], ..Traced::default() };
        let before = traced.clone();
        widen(&mut traced, &of_directives(Path::new("/p"), &[], &[]), &mut fixed()).unwrap();
        assert_eq!(traced, before);
    }

    #[test]
    fn a_run_that_read_the_clock_is_watched_as_cargo_watches_it() {
        let clock = || Traced {
            files: vec![("/p/build.rs".into(), Some([1; 32]))],
            untraceable: vec![Untraceable::Clock],
            ..Traced::default()
        };
        let mut traced = clock();
        widen(&mut traced, &of_directives(Path::new("/p"), &[], &[]), &mut fixed()).unwrap();
        assert_eq!(traced.dirs.len(), 1);
        assert_eq!(traced.dirs[0].0, Path::new("/p").join(PACKAGE));
        let mut changed = fixed();
        changed.files.insert("/p/src/lib.rs".into(), [9; 32]);
        let shape = Shape::of(&traced);
        assert_eq!(shape.values(&mut fixed()).unwrap(), traced);
        assert_ne!(shape.values(&mut changed).unwrap(), traced);

        let mut named = clock();
        let rule = of_directives(Path::new("/p"), &["data".into(), "build.rs".into()], &["VERSION".into()]);
        widen(&mut named, &rule, &mut fixed()).unwrap();
        assert_eq!(named.files, vec![("/p/build.rs".into(), Some([1; 32]))]);
        assert_eq!(named.dirs[0].0, Path::new("/p/data").join(TREE));
        assert_eq!(named.env, vec![("VERSION".into(), Some("1".into()))]);
        let shape = Shape::of(&named);
        let mut data = fixed();
        data.files.insert("/p/data/a".into(), [7; 32]);
        assert_ne!(shape.values(&mut data).unwrap(), named);
        // What the rule does not name is not watched.
        assert_eq!(shape.values(&mut changed).unwrap(), named);
    }
}

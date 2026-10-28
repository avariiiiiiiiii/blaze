//! The records of the crates the cargo of one of blaze's loads compiles. Its wrapper
//! ([`super::load_wrapper`]) writes one for every crate, next to cargo's own output for the
//! profile, so `cargo clean` takes both.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

/// The directory of the records, in the directory cargo builds a profile in.
const RECORDS: &str = ".blaze-units";

/// The file of a unit's directory that describes a library outside the workspace.
const DESCRIBED: &str = "crate.json";

/// One run of rustc on a crate of the workspace that succeeded.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Record {
    /// The crate's name and cargo's hash of the unit, as in the names of its files.
    pub id: String,
    pub crate_name: String,
    /// The directory rustc ran in, which the paths in its arguments are relative to.
    pub cwd: PathBuf,
    /// The arguments cargo passed, without the ones the wrapper added.
    pub args: Vec<String>,
    /// The root file of the crate.
    pub source: PathBuf,
    /// When rustc was done, in nanoseconds since the epoch. Later records have later stamps.
    pub stamp: u128,
    /// The files rustc wrote that a later build writes again, each with the copy of it.
    pub copies: Vec<Copied>,
    /// The copy of the line rustc ran the linker with, for a crate that is linked.
    pub link_line: Option<PathBuf>,
    /// The variables cargo set for the unit that say where the build puts its programs:
    /// `CARGO_BIN_EXE_<name>`, which `env!` reads in an integration test.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// The variables cargo set for rustc that the crate's source can read with `env!`
    /// ([`super::own_script::read_by_crates`]), for blaze to compile it as rustc did.
    #[serde(default)]
    pub crate_env: BTreeMap<String, String>,
}

/// A file rustc wrote and the copy the daemon reads in its place.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Copied {
    pub original: PathBuf,
    pub copy: PathBuf,
}

impl Record {
    /// Where the records of the units cargo writes to `out_dir` are: in the directory of the
    /// profile, which `deps`, `examples` and `build` are in, in either of cargo's layouts
    /// ([`super::layout`]). A file cargo links from one of those to the profile's directory
    /// is found the same way.
    pub fn dir_for(out_dir: &Path) -> PathBuf {
        super::layout::profile_of(out_dir).unwrap_or(out_dir).join(RECORDS)
    }

    /// The name of the unit of crate `crate_name` that rustc writes to `out_dir` with
    /// `-C extra-filename=extra`, which its record is kept by: `<crate>-<hash>` in both of
    /// cargo's layouts.
    pub fn id_of(crate_name: &str, extra: &str, out_dir: &Path) -> String {
        super::layout::unit_name(crate_name, extra, out_dir)
    }

    /// The name of the unit that wrote `file`, named `<prefix><stem><suffix>`, as
    /// [`Record::id_of`] gives it.
    pub fn id_of_file(file: &Path, prefix: &str, suffix: &str) -> Option<String> {
        let name = file.file_name()?.to_str()?;
        let stem = name.strip_prefix(prefix)?.strip_suffix(suffix)?;
        Some(super::layout::unit_of_file(stem, file.parent()?))
    }

    /// The copy the record of the unit that wrote `library` (`lib<crate><extra>.rlib`, as
    /// its unit is named) has of it, which is what the daemon reads in its place.
    pub fn kept_copy(library: &Path) -> Option<PathBuf> {
        let unit = Record::id_of_file(library, "lib", ".rlib")?;
        let record = Record::dir_for(library.parent()?).join(unit).join("record.json");
        let record: Record = serde_json::from_str(&std::fs::read_to_string(record).ok()?).ok()?;
        record.copy_of(library).map(Path::to_owned)
    }

    /// Writes the record where the next reader finds it whole.
    pub fn write(&self, dir: &Path) -> anyhow::Result<()> {
        let unit = dir.join(&self.id);
        std::fs::create_dir_all(&unit)?;
        let new = unit.join(format!("record.{}.new", std::process::id()));
        std::fs::write(&new, serde_json::to_vec(self)?)?;
        std::fs::rename(&new, unit.join("record.json"))?;
        Ok(())
    }

    /// Writes the record of a library or proc macro of a package outside the workspace, where
    /// [`Record::all`] does not read it: only a build script blaze compiles reads it, to
    /// compile the library from source as rustc did, or to expand with the proc macro
    /// ([`Record::described`]).
    pub fn write_described(&self, dir: &Path) -> anyhow::Result<()> {
        let unit = dir.join(&self.id);
        std::fs::create_dir_all(&unit)?;
        let new = unit.join(format!("{DESCRIBED}.{}.new", std::process::id()));
        std::fs::write(&new, serde_json::to_vec(self)?)?;
        std::fs::rename(&new, unit.join(DESCRIBED))?;
        Ok(())
    }

    /// The record of the unit `id` in `dir`, of the workspace or described.
    pub fn described(dir: &Path, id: &str) -> Option<Record> {
        let unit = dir.join(id);
        let text = std::fs::read_to_string(unit.join("record.json"))
            .or_else(|_| std::fs::read_to_string(unit.join(DESCRIBED)))
            .ok()?;
        serde_json::from_str(&text).ok()
    }

    /// The record of the unit `id` that wrote a file to `out_dir`, and the directory of records
    /// it is in: there, or in cargo's target directory when `out_dir` is in blaze's own and
    /// the unit was one it was seeded with (`seed_own_target`), which cargo built.
    pub fn described_or_seeded(out_dir: &Path, id: &str) -> Option<(Record, PathBuf)> {
        let parts: Vec<_> = out_dir.components().collect();
        let own = |it: &std::path::Component<'_>| {
            [super::own_macro::OWN_DIR, super::own_macro::BY_RUSTC_DIR]
                .iter()
                .any(|dir| it.as_os_str() == *dir)
        };
        let cargos = parts.iter().rposition(own).map(|at| {
            let parts = parts.iter().enumerate().filter(|(it, _)| *it != at);
            parts.map(|(_, it)| it).collect::<PathBuf>()
        });
        [Some(out_dir.to_owned()), cargos].into_iter().flatten().find_map(|dir| {
            let records = Record::dir_for(&dir);
            Some((Record::described(&records, id)?, records))
        })
    }

    fn value(&self, flag: &str) -> Option<&str> {
        let at = self.args.iter().position(|it| it == flag)?;
        self.args.get(at + 1).map(String::as_str)
    }

    fn values<'a>(&'a self, flag: &'a str) -> impl Iterator<Item = &'a str> {
        self.args.windows(2).filter(move |it| it[0] == flag).map(|it| it[1].as_str())
    }

    /// The value of `-C name=value`, however cargo spelled it.
    pub fn codegen(&self, name: &str) -> Option<&str> {
        let joined = self.args.iter().filter_map(|it| it.strip_prefix("-C"));
        let split = self.values("-C");
        joined.chain(split).filter(|it| !it.is_empty()).find_map(|it| {
            let (key, value) = it.split_once('=').unwrap_or((it, ""));
            (key == name).then_some(value)
        })
    }

    pub fn out_dir(&self) -> Option<PathBuf> {
        self.value("--out-dir").map(|it| self.cwd.join(it))
    }

    fn extra_filename(&self) -> &str {
        self.codegen("extra-filename").unwrap_or_default()
    }

    pub fn is_test(&self) -> bool {
        self.args.iter().any(|it| it == "--test")
    }

    fn crate_types(&self) -> Vec<&str> {
        self.values("--crate-type").collect()
    }

    pub fn is_rlib(&self) -> bool {
        !self.is_test() && self.crate_types().iter().any(|it| matches!(*it, "lib" | "rlib"))
    }

    pub fn is_build_script(&self) -> bool {
        self.crate_name.starts_with("build_script_")
    }

    pub fn is_proc_macro(&self) -> bool {
        self.crate_types().contains(&"proc-macro")
    }

    /// Whether rustc links an executable from the crate.
    pub fn is_linked(&self) -> bool {
        self.is_test() || self.crate_types() == ["bin"]
    }

    /// The files of this unit that another unit or the user is given.
    pub fn outputs(&self) -> Vec<PathBuf> {
        let Some(out_dir) = self.out_dir() else { return Vec::new() };
        let (name, extra) = (&self.crate_name, self.extra_filename());
        let mut outputs = Vec::new();
        if self.is_linked() {
            outputs.push(out_dir.join(format!("{name}{extra}{}", std::env::consts::EXE_SUFFIX)));
        }
        if self.is_rlib() {
            outputs.push(out_dir.join(format!("lib{name}{extra}.rlib")));
        }
        if self.is_proc_macro() {
            let (prefix, suffix) = (std::env::consts::DLL_PREFIX, std::env::consts::DLL_SUFFIX);
            outputs.push(out_dir.join(format!("{prefix}{name}{extra}{suffix}")));
        }
        outputs
    }

    /// The copy of `original`, when it is a file of this unit.
    pub fn copy_of(&self, original: &Path) -> Option<&Path> {
        let named = self.copies.iter().find(|it| it.original == original);
        // A target directory reached through a link has two names for the file, which may
        // have been moved to its copy: its directory is still there.
        let same = |it: &&Copied| {
            let dirs = it.original.parent().zip(original.parent());
            it.original.file_name() == original.file_name()
                && dirs
                    .is_some_and(|(it, other)| same_file::is_same_file(it, other).unwrap_or(false))
        };
        named.or_else(|| self.copies.iter().find(same)).map(|it| &*it.copy)
    }
}

/// Now, as a stamp.
pub fn stamp_now() -> u128 {
    stamp_of(SystemTime::now())
}

pub fn stamp_of(time: SystemTime) -> u128 {
    time.duration_since(UNIX_EPOCH).map_or(0, |it| it.as_nanos())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(args: &[&str]) -> Record {
        Record {
            id: "a-1".to_owned(),
            crate_name: "a".to_owned(),
            cwd: PathBuf::from("/ws"),
            args: args.iter().map(|it| it.to_string()).collect(),
            source: PathBuf::from("/ws/src/lib.rs"),
            stamp: 1,
            copies: Vec::new(),
            link_line: None,
            env: BTreeMap::new(),
            crate_env: BTreeMap::new(),
        }
    }

    #[test]
    fn records_are_in_the_profile() {
        let records = Path::new("/t/debug/.blaze-units");
        for dir in [
            "/t/debug/deps",
            "/t/debug/examples",
            "/t/debug/build/a-1",
            "/t/debug",
            "/t/debug/build/a/0123456789abcdef/out",
        ] {
            assert_eq!(Record::dir_for(Path::new(dir)), records, "{dir}");
        }
    }

    #[test]
    fn codegen_flags_in_both_spellings() {
        let unit = record(&["-C", "opt-level=3", "-Cextra-filename=-1", "-C", "debug-assertions"]);
        assert_eq!(unit.codegen("opt-level"), Some("3"));
        assert_eq!(unit.codegen("extra-filename"), Some("-1"));
        assert_eq!(unit.codegen("debug-assertions"), Some(""));
        assert_eq!(unit.codegen("lto"), None);
    }

    #[test]
    fn outputs_by_kind() {
        let lib =
            record(&["--crate-type", "lib", "--out-dir", "t/deps", "-C", "extra-filename=-1"]);
        assert_eq!(lib.outputs(), [PathBuf::from("/ws/t/deps/liba-1.rlib")]);
        assert!(lib.is_rlib() && !lib.is_proc_macro());
        let test = record(&["--crate-type", "lib", "--test", "--out-dir", "/t/deps"]);
        assert!(test.is_test() && !test.is_rlib());
        let macros = record(&["--crate-type", "proc-macro"]);
        assert!(macros.is_proc_macro() && !macros.is_rlib());
    }
}

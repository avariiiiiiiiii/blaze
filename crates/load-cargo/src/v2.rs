//! A build's units lowered into the front end (docs/v2/plan/lanes/cargo/notes.md sections 2
//! and 6): base-db's crate graph, source roots and files, from `blaze_units::query::Units` and
//! the files of the view and the sources mounted beside it, read through the tree and never
//! from the disk.
//!
//! - **Crates**: one per unit that compiles (every mode but `RunCustomBuild`), so a lib's
//!   `Test` unit is a crate of its own with `cfg(test)`, and a package cargo builds for the host
//!   and for the target is two crates. Deps are the unit's, by `extern_crate_name`, in its
//!   order; cfgs are the target's (blaze's target table, never `rustc --print`) with what the
//!   profile, the rustflags and `UnitPlan::cfgs` decide; env is `UnitPlan::env`.
//! - **Files**: every file under each package's directory (and the directory of a target root
//!   outside it), by its absolute path as cargo names it, from the root of its longest mount.
//!   One source root per package: library (`Durability::HIGH`) for the standard library,
//!   registry and git packages; local (`Durability::LOW`) for path packages, as rust-analyzer
//!   has them, so an edit of a member's file invalidates no query of std. A build script
//!   run's `OUT_DIR` is in its package's source root, so what it writes there is read by that
//!   package's crates and no other's.
//! - **Build script runs** are applied by [`Lowered::ran`]; a proc macro's built expanders by
//!   [`Lowered::macros`].

mod crates;
mod db;
mod files;
#[cfg(test)]
mod tests;

use std::{
    collections::{BTreeSet, HashMap},
    fmt,
    sync::{Mutex, MutexGuard, PoisonError},
};

use base_db::{
    AbsPathBuf, ContentId, Crate, CrateWorkspaceData, Env, FileSet, LibraryRoots, LocalRoots,
    SourceDatabase, SourceRoot, SourceRootId, VfsPath, ViewFile, ViewRoot,
};
use blaze_units::{
    query::{BuildConfig, PackagePlan, Units},
    schema::{Mode, Unit},
    stdlib::target::HostCpu,
};
use cfg::{CfgAtom, CfgOptions};
use hir_expand::proc_macro::{CrateProcMacros, ProcMacroLoadResult, ProcMacros};
use rustc_hash::{FxHashMap, FxHashSet};
use salsa::{Durability, Setter};
use triomphe::Arc;
use vfs::file_set::FileSetConfig;

pub use db::{Db, Snapshot};

/// What [`lower`] lowers.
pub struct Lower<'a> {
    pub units: &'a Units,
    pub config: &'a BuildConfig,
    /// The view's root and every mount's root, readable from any thread: the VFS's
    /// `blaze_vfs::Reader` (`&job.files`), or a `blaze_query::MemTree` in a test.
    pub files: &'a dyn blaze_query::Tree,
    /// The view's root (what `Origin::View` mounts are of).
    pub root: ContentId,
    /// The machine's CPU, for a `-C target-cpu=native` in the rustflags; without it such a
    /// flag is an error naming it.
    pub host_cpu: Option<&'a HostCpu>,
}

/// Why units could not be lowered, or a run applied: what, and the path or unit it is about.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LowerError {
    pub what: String,
    pub at: Option<String>,
}

impl LowerError {
    fn new(what: impl Into<String>) -> LowerError {
        LowerError { what: what.into(), at: None }
    }

    fn at(what: impl Into<String>, at: impl Into<String>) -> LowerError {
        LowerError { what: what.into(), at: Some(at.into()) }
    }
}

impl fmt::Display for LowerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.at {
            Some(at) => write!(f, "{}\n  at {at}", self.what),
            None => f.write_str(&self.what),
        }
    }
}

impl std::error::Error for LowerError {}

/// What a build script's run gave: what [`Lowered::ran`] applies to the units depending on it.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Ran {
    /// As `--cfg` takes them (`foo`, `foo="bar"`).
    pub cfgs: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Absolute, as cargo names it.
    pub out_dir: String,
    /// What it left in `OUT_DIR`, by `/`-separated path under it.
    pub files: Vec<(String, Vec<u8>)>,
}

/// A build's units in the front end: its database, and each unit's crate.
pub struct Lowered {
    db: Mutex<Db>,
    units: Vec<Lowering>,
    /// What every source root was made with: a set per package.
    partition: FileSetConfig,
    other: SourceRootId,
    /// By unit: the runs applied to it, by the run's unit.
    applied: Mutex<Vec<Vec<(usize, Applied)>>>,
}

/// One unit as lowered.
struct Lowering {
    krate: Option<Crate>,
    /// `foo v0.1.0 (lib)`, for errors.
    what: String,
    /// A `RunCustomBuild` unit's `OUT_DIR`.
    out_dir: Option<String>,
    /// The units depending on it directly.
    dependents: Vec<usize>,
    /// Its cfg and env before any run applied.
    cfg: CfgOptions,
    env: Vec<(String, String)>,
    proc_macro: bool,
}

struct Applied {
    cfgs: Vec<CfgAtom>,
    env: Vec<(String, String)>,
    out_dir: String,
}

/// The crate graph, source roots and files of `lower.units` in a new database.
pub fn lower(lower: Lower<'_>) -> Result<Lowered, LowerError> {
    let Lower { units, config, files, root, host_cpu } = lower;
    let unit_count = units.graph.units.len();
    if units.plans.len() != unit_count {
        return Err(LowerError::new(format!(
            "the build has {unit_count} units and {} plans of them",
            units.plans.len()
        )));
    }
    let packages: FxHashMap<&str, &PackagePlan> =
        units.packages.iter().map(|it| (it.id.as_str(), it)).collect();
    let package_of = |unit: &Unit| {
        packages.get(unit.pkg_id.as_str()).copied().ok_or_else(|| {
            LowerError::at(
                format!("unit `{}` of package `{}` is of no package of the build", unit.target.name, unit.pkg_id),
                unit.pkg_id.clone(),
            )
        })
    };
    let mut whats = Vec::with_capacity(unit_count);
    for unit in &units.graph.units {
        whats.push(crates::describe(package_of(unit)?, unit));
    }

    // One set of the partition per package with a unit, in the order of `units.packages`.
    let used: FxHashSet<&str> = units.graph.units.iter().map(|it| it.pkg_id.as_str()).collect();
    let mut sets: Vec<Set> = Vec::new();
    let mut set_of: FxHashMap<&str, usize> = FxHashMap::default();
    for package in units.packages.iter().filter(|it| used.contains(it.id.as_str())) {
        let dir = files::parent(&package.manifest_path).to_owned();
        let is_std = units.graph.units.iter().any(|it| it.pkg_id == package.id && it.is_std);
        set_of.insert(package.id.as_str(), sets.len());
        sets.push(Set { prefixes: vec![dir.clone()], loads: vec![dir], library: is_std || !crates::is_path(package) });
    }
    let mut excluded: BTreeSet<String> = BTreeSet::new();
    excluded.insert(units.target_dir.clone());
    excluded.insert(units.build_dir.clone());
    for (index, unit) in units.graph.units.iter().enumerate() {
        let set = set_of[unit.pkg_id.as_str()];
        if unit.mode == Mode::RunCustomBuild {
            let out_dir = units.plans[index].out_dir.clone().ok_or_else(|| {
                LowerError::at("a build script's run has no `OUT_DIR`", whats[index].clone())
            })?;
            excluded.insert(out_dir.clone());
            sets[set].prefixes.push(out_dir);
            continue;
        }
        let src = crates::src_path(unit, &whats[index])?;
        let dir = files::parent(&src);
        let inside = sets[set].loads.iter().any(|load| files::rest(dir, load).is_some());
        if !inside {
            sets[set].prefixes.push(dir.to_owned());
            sets[set].loads.push(dir.to_owned());
        }
    }

    let tree = files::Mounts { mounts: &config.mounts, view: root, tree: files };
    let loads: BTreeSet<String> = sets.iter().flat_map(|it| it.loads.iter().cloned()).collect();
    let mut db = Db::new();
    let mut partition = FileSetConfig::builder();
    for (index, set) in sets.iter().enumerate() {
        let source_root = match set.library {
            true => SourceRoot::new_library(FileSet::default()),
            false => SourceRoot::new_local(FileSet::default()),
        };
        let durability = if set.library { Durability::HIGH } else { Durability::LOW };
        db.set_source_root_with_durability(SourceRootId(index as u32), Arc::new(source_root), durability);
        let mut prefixes = Vec::with_capacity(set.prefixes.len());
        for prefix in &set.prefixes {
            prefixes.push(VfsPath::from(abs(prefix)?));
        }
        partition.add_file_set(prefixes);
    }
    let other = SourceRootId(sets.len() as u32);
    db.set_source_root_with_durability(other, Arc::new(SourceRoot::new_local(FileSet::default())), Durability::LOW);
    let library: FxHashSet<SourceRootId> =
        (0..sets.len()).filter(|&it| sets[it].library).map(|it| SourceRootId(it as u32)).collect();
    let local: FxHashSet<SourceRootId> = (0..=sets.len())
        .filter(|&it| it == sets.len() || !sets[it].library)
        .map(|it| SourceRootId(it as u32))
        .collect();
    _ = LibraryRoots::builder(library).durability(Durability::MEDIUM).new(&db);
    _ = LocalRoots::builder(local).durability(Durability::MEDIUM).new(&db);
    let partition = partition.build();

    for load in &loads {
        // What is under another load, or written by the build, is not this one's.
        let skipped: Vec<&String> = loads
            .iter()
            .chain(excluded.iter())
            .filter(|it| *it != load && files::rest(it, load).is_some())
            .collect();
        let skip = |path: &str| skipped.iter().any(|it| it.as_str() == path);
        let mut loaded = Vec::new();
        tree.walk(load, &skip, &mut loaded)?;
        let built: Vec<AbsPathBuf> = skipped.iter().map(|it| abs(it)).collect::<Result<_, _>>()?;
        set_files(&mut db, load, &built, &partition, other, &loaded)?;
    }

    let mut workspaces: HashMap<String, Arc<CrateWorkspaceData>> = HashMap::new();
    let toolchain = crates::toolchain()?;
    let const_eval = crates::const_eval(units);
    let mut lowerings: Vec<Lowering> = Vec::with_capacity(unit_count);
    for (index, unit) in units.graph.units.iter().enumerate() {
        lowerings.push(Lowering {
            krate: None,
            what: whats[index].clone(),
            out_dir: units.plans[index].out_dir.clone().filter(|_| unit.mode == Mode::RunCustomBuild),
            dependents: Vec::new(),
            cfg: CfgOptions::default(),
            env: Vec::new(),
            proc_macro: unit.target.is_proc_macro() && unit.mode != Mode::RunCustomBuild,
        });
    }
    for (index, unit) in units.graph.units.iter().enumerate() {
        for dep in &unit.dependencies {
            let Some(lowering) = lowerings.get_mut(dep.index) else {
                return Err(LowerError::at(
                    format!("a dependency `{}` on unit {}, which the build lacks", dep.extern_crate_name, dep.index),
                    whats[index].clone(),
                ));
            };
            if !lowering.dependents.contains(&index) {
                lowering.dependents.push(index);
            }
        }
    }

    let order = crates::topological(units, &whats)?;
    let mut all = Vec::with_capacity(order.len());
    for index in order {
        let unit = &units.graph.units[index];
        let plan = &units.plans[index];
        let package = package_of(unit)?;
        let triple = unit.platform.clone().unwrap_or_else(|| config.host.clone());
        let workspace = match workspaces.get(&triple) {
            Some(it) => it.clone(),
            None => {
                let made = crates::workspace(&triple, &toolchain, const_eval)?;
                workspaces.insert(triple.clone(), made.clone());
                made
            }
        };
        let cfg = crates::cfg(unit, plan, &triple, host_cpu, &whats[index])?;
        let src = crates::src_path(unit, &whats[index])?;
        let src_path = VfsPath::from(abs(&src)?);
        let set = partition.classify_path(&src_path).map_or(other, |it| SourceRootId(it as u32));
        let root_file = db.source_root(set).source_root(&db).file_for_path(&src_path).copied();
        let Some(root_file) = root_file else {
            return Err(LowerError::at(
                format!("the root file `{src}` of {} is not among its package's files", whats[index]),
                src,
            ));
        };
        let mut dependencies = Vec::with_capacity(unit.dependencies.len());
        for dep in &unit.dependencies {
            let on = &units.graph.units[dep.index];
            if on.mode == Mode::RunCustomBuild {
                continue;
            }
            let Some(krate) = lowerings[dep.index].krate else {
                return Err(LowerError::at(
                    format!("{} depends on {}, which has no crate", whats[index], whats[dep.index]),
                    whats[index].clone(),
                ));
            };
            dependencies.push(crates::dependency(unit, on, dep, krate, &whats[index])?);
        }
        let data = base_db::BuiltCrateData {
            root_file_id: root_file,
            edition: crates::edition(unit, &whats[index])?,
            dependencies,
            origin: crates::origin(package, unit),
            crate_attrs: Box::new([]),
            target_kind: crates::target_kind(unit, &whats[index])?,
            proc_macro_cwd: Arc::new(abs(&crates::proc_macro_cwd(units, package))?),
        };
        let extra = base_db::ExtraCrateData {
            version: Some(package.version.clone()),
            display_name: Some(base_db::CrateDisplayName::from_canonical_name(&unit.target.name)),
            potential_cfg_options: None,
        };
        let library = sets[set_of[unit.pkg_id.as_str()]].library;
        let durability = if library { Durability::HIGH } else { Durability::MEDIUM };
        let env: Env = plan.env.iter().cloned().collect();
        let krate = Crate::create(&mut db, data, extra, workspace, cfg.clone(), env, durability);
        let lowering = &mut lowerings[index];
        lowering.krate = Some(krate);
        lowering.cfg = cfg;
        lowering.env = plan.env.clone();
        all.push(krate);
    }
    base_db::set_all_crates_with_durability(&mut db, all, Durability::MEDIUM);

    let not_built: FxHashMap<Crate, Arc<CrateProcMacros>> = lowerings
        .iter()
        .filter(|it| it.proc_macro)
        .filter_map(|it| it.krate)
        .map(|krate| {
            let macros = CrateProcMacros::new(Err(base_db::ProcMacroLoadingError::NotYetBuilt));
            (krate, Arc::new(macros))
        })
        .collect();
    ProcMacros::get(&db).set_by_crate(&mut db).with_durability(Durability::HIGH).to(not_built);
    base_db::set_process_env(&mut db, config.invocation.env.iter().cloned().collect());

    Ok(Lowered {
        db: Mutex::new(db),
        applied: Mutex::new((0..unit_count).map(|_| Vec::new()).collect()),
        units: lowerings,
        partition,
        other,
    })
}

/// A set of the partition: one package's.
struct Set {
    /// The directories whose files are its source root's: the package's, its runs' `OUT_DIR`s,
    /// the directory of a target root outside the package's.
    prefixes: Vec<String>,
    /// Those read now: all but the `OUT_DIR`s.
    loads: Vec<String>,
    library: bool,
}

impl Lowered {
    /// The front end's crate of unit `unit` (an index of `units.graph.units`); none for a
    /// `RunCustomBuild` unit.
    pub fn krate(&self, unit: usize) -> Option<Crate> {
        self.units.get(unit)?.krate
    }

    /// A database for a worker to read the front end with while other workers read theirs:
    /// see [`Snapshot`] for what a write does to it.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot::new(self.db().clone())
    }

    /// The run of build script unit `run` applied to every unit that depends on it directly:
    /// its `rustc-cfg`s added to their cfgs, its `rustc-env`s to their env, `OUT_DIR` set, and
    /// the files it left in `OUT_DIR` put into the front end's files (its package's source
    /// root), so `include!` and `env!("OUT_DIR")` read them. Applying a run again replaces
    /// what it applied before, files it no longer left included.
    ///
    /// Called on the daemon's loop thread while workers read snapshots: salsa cancels every
    /// snapshot alive (its reader gets `salsa::Cancelled`, which the caller turns into
    /// `Failed::Again`) and this returns once they are all dropped; a snapshot taken after
    /// reads the run applied.
    pub fn ran(&self, run: usize, ran: &Ran) -> Result<(), LowerError> {
        let Some(lowering) = self.units.get(run) else {
            return Err(LowerError::new(format!("the build has no unit {run}")));
        };
        let Some(out_dir) = &lowering.out_dir else {
            return Err(LowerError::at("not a build script's run", lowering.what.clone()));
        };
        if *out_dir != ran.out_dir {
            return Err(LowerError::at(
                format!("the run's `OUT_DIR` is `{}`, and cargo names it `{out_dir}`", ran.out_dir),
                lowering.what.clone(),
            ));
        }
        let cfgs = ran
            .cfgs
            .iter()
            .map(|it| crates::atom(it).map_err(|what| LowerError::at(what, lowering.what.clone())))
            .collect::<Result<Vec<_>, _>>()?;
        let mut loaded = Vec::with_capacity(ran.files.len());
        for (path, bytes) in &ran.files {
            let id = ContentId::of(blaze_vfs::FILE_DOMAIN, bytes);
            loaded.push(files::Loaded { path: format!("{out_dir}/{path}"), id, bytes: bytes.as_slice().into() });
        }

        let mut db = self.db();
        set_files(&mut db, out_dir, &[], &self.partition, self.other, &loaded)?;
        let mut applied = self.applied.lock().unwrap_or_else(PoisonError::into_inner);
        for &dependent in &lowering.dependents {
            let Some(krate) = self.units[dependent].krate else { continue };
            let runs = &mut applied[dependent];
            runs.retain(|(it, _)| *it != run);
            let applying = Applied { cfgs: cfgs.clone(), env: ran.env.clone(), out_dir: out_dir.clone() };
            runs.push((run, applying));
            runs.sort_by_key(|(it, _)| *it);
            let base = &self.units[dependent];
            let mut cfg = base.cfg.clone();
            let mut env: Env = base.env.iter().cloned().collect();
            for (_, applying) in runs.iter() {
                cfg.extend(applying.cfgs.iter().cloned());
                env.extend(applying.env.iter().cloned());
                env.set("OUT_DIR", applying.out_dir.clone());
            }
            if *krate.cfg_options(&*db) != cfg {
                krate.set_cfg_options(&mut *db, cfg);
            }
            if *krate.env(&*db) != env {
                krate.set_env(&mut *db, env);
            }
        }
        Ok(())
    }

    /// The expanders of proc-macro unit `unit` once it is built (or why it has none), for the
    /// crates depending on it to expand its macros with. Until then they are
    /// `ProcMacroLoadingError::NotYetBuilt`. A write: snapshots alive are cancelled as by
    /// [`Lowered::ran`].
    pub fn macros(&self, unit: usize, macros: ProcMacroLoadResult) -> Result<(), LowerError> {
        let Some(lowering) = self.units.get(unit) else {
            return Err(LowerError::new(format!("the build has no unit {unit}")));
        };
        let (Some(krate), true) = (lowering.krate, lowering.proc_macro) else {
            return Err(LowerError::at("not a proc macro's crate", lowering.what.clone()));
        };
        let macros = macros.map(|mut macros| {
            macros.sort_unstable_by(|a, b| (a.name.as_str(), a.kind).cmp(&(b.name.as_str(), b.kind)));
            macros
        });
        let mut db = self.db();
        let all = ProcMacros::get(&*db);
        let mut by_crate = all.by_crate(&*db).clone();
        by_crate.insert(krate, Arc::new(CrateProcMacros::new(macros)));
        all.set_by_crate(&mut *db).with_durability(Durability::HIGH).to(by_crate);
        Ok(())
    }

    fn db(&self) -> MutexGuard<'_, Db> {
        self.db.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Snapshot {
    /// The absolute path of `file`, as cargo names it.
    pub fn path(&self, file: base_db::FileId) -> Option<String> {
        let root = self.file_source_root(file).source_root_id(&**self);
        let root = self.source_root(root).source_root(&**self);
        Some(root.path_for_file(&file)?.as_path()?.as_str().to_owned())
    }
}

/// Puts `loaded`, the files under `base`, into the source roots `partition` gives them.
fn set_files(
    db: &mut Db,
    base: &str,
    built: &[AbsPathBuf],
    partition: &FileSetConfig,
    other: SourceRootId,
    loaded: &[files::Loaded],
) -> Result<(), LowerError> {
    let base_path = abs(base)?;
    let mut view = Vec::with_capacity(loaded.len());
    for file in loaded {
        let Some(path) = files::rest(&file.path, base).filter(|it| !it.is_empty()) else {
            return Err(LowerError::at(format!("`{}` is not under `{base}`", file.path), file.path.clone()));
        };
        let file = blaze_vfs::File { id: file.id, mode: blaze_vfs::Mode::Regular, bytes: &file.bytes };
        view.push(ViewFile { path, file });
    }
    let root = ViewRoot { base: &base_path, built, partition, other, files: &view };
    db.set_view_root(root).map_err(|error| {
        let at = match &error {
            base_db::ViewError::BadPath(path)
            | base_db::ViewError::Duplicate(path)
            | base_db::ViewError::Mismatch { path, .. }
            | base_db::ViewError::NoSourceRoot { path, .. }
            | base_db::ViewError::NoFileId(path) => format!("{base}/{path}"),
        };
        LowerError::at(error.to_string(), at)
    })?;
    Ok(())
}

fn abs(path: &str) -> Result<AbsPathBuf, LowerError> {
    AbsPathBuf::try_from(path)
        .map_err(|_| LowerError::at(format!("`{path}` is not an absolute path"), path))
}

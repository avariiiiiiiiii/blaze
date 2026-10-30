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
//!   One source root per package, but the standard library's packages share one, the whole of
//!   `library/` (core reads `library/stdarch` and `library/portable-simd` by `#[path]`):
//!   library (`Durability::HIGH`) for the standard library, registry and git packages; local
//!   (`Durability::LOW`) for path packages, as rust-analyzer has them, so an edit of a member's
//!   file invalidates no query of std. A build script run's `OUT_DIR` is in its package's
//!   source root, so what it writes there is read by that package's crates and no other's.
//! - **Build script runs** are applied by [`Lowered::ran`]; a proc macro's built expanders by
//!   [`Lowered::macros`].

#![forbid(unsafe_code)]

mod crates;
mod db;
mod files;
#[cfg(test)]
mod tests;

use std::{collections::HashMap, fmt};

use base_db::{
    AbsPathBuf, ContentId, Crate, CrateWorkspaceData, Env, FileId, FileSet, LibraryRoots,
    LocalRoots, SourceDatabase, SourceRoot, SourceRootId, VfsPath, ViewChange, ViewFile, ViewRoot,
};
use blaze_ids::radix;
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

/// A build's units in the front end, owned by the daemon's loop: the one writer of its
/// database, which hands each task it posts a [`Snapshot`] and the shared [`Crates`]. Nothing
/// in it is locked: the loop alone calls its `&mut self` writes.
pub struct Lowered {
    db: Db,
    crates: std::sync::Arc<Crates>,
    /// By unit: the runs applied to it, by the run's unit.
    applied: Vec<Vec<(usize, Applied)>>,
    /// Every file of the front end by its absolute path as cargo names it; a new map after
    /// each run that adds or takes out files.
    paths: std::sync::Arc<HashMap<FileId, String>>,
}

/// What every task of a build reads and nothing writes once [`lower`] made it: each unit's
/// crate.
pub struct Crates {
    units: Vec<Lowering>,
    /// How many source roots of packages there are: `SourceRootId(0..sets)`, then the one of
    /// files of no package.
    sets: usize,
}

impl Crates {
    /// The front end's crate of unit `unit` (an index of `units.graph.units`); none for a
    /// `RunCustomBuild` unit.
    pub fn krate(&self, unit: usize) -> Option<Crate> {
        self.units.get(unit)?.krate
    }

    /// Whether unit `unit`'s crate is a proc macro's dylib, whose expanders
    /// [`Lowered::macros`] sets once it is built.
    pub fn is_proc_macro(&self, unit: usize) -> bool {
        self.units.get(unit).is_some_and(|it| it.proc_macro)
    }
}

/// One unit as lowered.
struct Lowering {
    krate: Option<Crate>,
    /// `foo v0.1.0 (lib)`, for errors.
    what: String,
    /// Its package's source root.
    set: usize,
    /// A `RunCustomBuild` unit's `OUT_DIR`.
    out_dir: Option<String>,
    /// The units depending on it directly.
    dependents: Vec<usize>,
    /// Its cfg and env before any run applied.
    cfg: CfgOptions,
    env: Vec<(String, String)>,
    /// Whether its crate is a proc macro's dylib (a `proc-macro` target built without `--test`).
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

    // One source root per package with a unit, in the order of `units.packages`, but one for
    // every package of the standard library; each directory read is one root's, the innermost
    // when one is in another's.
    let mut used: FxHashMap<&str, bool> = FxHashMap::default();
    for unit in &units.graph.units {
        *used.entry(unit.pkg_id.as_str()).or_default() |= unit.is_std;
    }
    let mut library: Vec<bool> = Vec::new();
    let mut set_of: FxHashMap<&str, usize> = FxHashMap::default();
    let mut loads: FxHashMap<String, usize> = FxHashMap::default();
    let mut std_set = None;
    for package in &units.packages {
        let Some(&is_std) = used.get(package.id.as_str()) else { continue };
        let set = match (is_std, std_set) {
            (true, Some(set)) => set,
            (true, None) => {
                let set = library.len();
                library.push(true);
                loads.insert(config.library.trim_end_matches('/').to_owned(), set);
                std_set = Some(set);
                set
            }
            (false, _) => {
                let set = library.len();
                library.push(!crates::is_path(package));
                loads.entry(files::parent(&package.manifest_path).to_owned()).or_insert(set);
                set
            }
        };
        set_of.insert(package.id.as_str(), set);
    }
    let mut excluded: FxHashSet<String> = FxHashSet::default();
    excluded.insert(units.target_dir.clone());
    excluded.insert(units.build_dir.clone());
    for (index, unit) in units.graph.units.iter().enumerate() {
        if unit.mode == Mode::RunCustomBuild {
            let out_dir = units.plans[index].out_dir.clone().ok_or_else(|| {
                LowerError::at("a build script's run has no `OUT_DIR`", whats[index].clone())
            })?;
            excluded.insert(out_dir);
            continue;
        }
        let src = crates::src_path(unit, &whats[index])?;
        let dir = files::parent(&src);
        if !loads.keys().any(|load| files::rest(dir, load).is_some()) {
            loads.insert(dir.to_owned(), set_of[unit.pkg_id.as_str()]);
        }
    }
    // Walked by path: the order files get their ids in.
    let mut loads: Vec<(String, usize)> = loads.into_iter().collect();
    loads.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
    let mut excluded: Vec<String> = excluded.into_iter().collect();
    excluded.sort_unstable();

    let tree = files::Mounts { mounts: &config.mounts, view: root, tree: files };
    let mut db = Db::new();
    for (index, &is_library) in library.iter().enumerate() {
        let source_root = match is_library {
            true => SourceRoot::new_library(FileSet::default()),
            false => SourceRoot::new_local(FileSet::default()),
        };
        let durability = if is_library { Durability::HIGH } else { Durability::LOW };
        db.set_source_root_with_durability(SourceRootId(index as u32), Arc::new(source_root), durability);
    }
    let other = SourceRootId(library.len() as u32);
    db.set_source_root_with_durability(other, Arc::new(SourceRoot::new_local(FileSet::default())), Durability::LOW);
    let roots = |library_too: bool| -> FxHashSet<SourceRootId> {
        (0..=library.len())
            .filter(|&it| library.get(it).copied().unwrap_or(false) == library_too)
            .map(|it| SourceRootId(it as u32))
            .collect()
    };
    _ = LibraryRoots::builder(roots(true)).durability(Durability::MEDIUM).new(&db);
    _ = LocalRoots::builder(roots(false)).durability(Durability::MEDIUM).new(&db);

    let mut loaded = Vec::new();
    for (load, set) in &loads {
        let set = *set;
        // What is under another load, or written by the build, is not this one's.
        let skipped: Vec<&String> = loads
            .iter()
            .map(|(it, _)| it)
            .chain(excluded.iter())
            .filter(|it| *it != load && files::rest(it, load).is_some())
            .collect();
        let skip = |path: &str| skipped.iter().any(|it| it.as_str() == path);
        loaded.clear();
        tree.walk(load, &skip, &mut loaded)?;
        let built: Vec<AbsPathBuf> = skipped.iter().map(|it| abs(it)).collect::<Result<_, _>>()?;
        set_files(&mut db, load, &built, set, library.len(), &loaded)?;
    }

    let workspace_root = crates::root(units, &tree)?;
    let toolchain = crates::toolchain()?;
    let mut workspaces: HashMap<&str, Arc<CrateWorkspaceData>> = HashMap::new();
    let mut lowerings: Vec<Lowering> = Vec::with_capacity(unit_count);
    for (index, unit) in units.graph.units.iter().enumerate() {
        let plan = &units.plans[index];
        let compiles = unit.mode != Mode::RunCustomBuild;
        lowerings.push(Lowering {
            krate: None,
            what: whats[index].clone(),
            set: set_of[unit.pkg_id.as_str()],
            out_dir: plan.out_dir.clone().filter(|_| !compiles),
            dependents: Vec::new(),
            cfg: CfgOptions::default(),
            env: Vec::new(),
            proc_macro: compiles && unit.target.is_proc_macro() && !crates::is_test(unit, plan),
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
        let what = &whats[index];
        let package = package_of(unit)?;
        let triple = unit.platform.as_deref().unwrap_or(&config.host);
        let workspace = match workspaces.get(triple) {
            Some(it) => it.clone(),
            None => {
                let made = crates::workspace_data(triple, &toolchain, workspace_root.const_eval)?;
                workspaces.insert(triple, made.clone());
                made
            }
        };
        let cfg = crates::cfg(unit, plan, triple, host_cpu, what)?;
        let src = crates::src_path(unit, what)?;
        let src_path = VfsPath::from(abs(&src)?);
        let set = loads
            .iter()
            .filter(|(load, _)| files::rest(&src, load).is_some())
            .max_by_key(|(load, _)| load.len())
            .map_or(other, |(_, set)| SourceRootId(*set as u32));
        let root_file = db.source_root(set).source_root(&db).file_for_path(&src_path).copied();
        let Some(root_file) = root_file else {
            return Err(LowerError::at(
                format!("the root file `{src}` of {what} is not among its package's files"),
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
                    format!("{what} depends on {}, which has no crate", whats[dep.index]),
                    what.clone(),
                ));
            };
            dependencies.push(crates::dependency(unit, on, dep, krate, what)?);
        }
        let data = base_db::BuiltCrateData {
            root_file_id: root_file,
            edition: crates::edition(unit, what)?,
            dependencies,
            origin: crates::origin(package, unit),
            crate_attrs: Box::new([]),
            target_kind: crates::target_kind(unit, plan, what)?,
            proc_macro_cwd: Arc::new(abs(&crates::proc_macro_cwd(&workspace_root, package))?),
        };
        let extra = base_db::ExtraCrateData {
            version: Some(package.version.clone()),
            display_name: Some(base_db::CrateDisplayName::from_canonical_name(&unit.target.name)),
            potential_cfg_options: None,
        };
        let durability = if library[set_of[unit.pkg_id.as_str()]] { Durability::HIGH } else { Durability::MEDIUM };
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

    let paths = paths_of(&db, library.len());
    Ok(Lowered {
        db,
        applied: (0..unit_count).map(|_| Vec::new()).collect(),
        crates: std::sync::Arc::new(Crates { units: lowerings, sets: library.len() }),
        paths: std::sync::Arc::new(paths),
    })
}

impl Lowered {
    /// [`Crates::krate`].
    pub fn krate(&self, unit: usize) -> Option<Crate> {
        self.crates.krate(unit)
    }

    /// What every task reads: each unit's crate.
    pub fn crates(&self) -> std::sync::Arc<Crates> {
        self.crates.clone()
    }

    /// A handle on the database for one task, made by the loop as it posts the task: a clone,
    /// with nothing shared to take. A later write ([`Lowered::ran`], [`Lowered::macros`])
    /// cancels it: its reader gets `salsa::Cancelled` (the task's `Failed::Again`), and the
    /// write returns once every such handle is dropped. Tasks posted after a write get new
    /// ones, which read what it wrote.
    pub fn snapshot(&self) -> Snapshot {
        self.db.clone()
    }

    /// Every file of the front end by its absolute path as cargo names it, as of now: a run
    /// applied after adds its `OUT_DIR`'s files to the map the next call gives.
    pub fn paths(&self) -> std::sync::Arc<HashMap<FileId, String>> {
        self.paths.clone()
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
    pub fn ran(&mut self, run: usize, ran: &Ran) -> Result<(), LowerError> {
        let crates = self.crates.clone();
        let Some(lowering) = crates.units.get(run) else {
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

        let db = &mut self.db;
        let change = set_files(db, out_dir, &[], lowering.set, crates.sets, &loaded)?;
        if !change.added.is_empty() || !change.removed.is_empty() {
            let mut map = HashMap::clone(&self.paths);
            for id in &change.removed {
                map.remove(id);
            }
            let root = db.source_root(SourceRootId(lowering.set as u32)).source_root(&*db);
            for id in &change.added {
                if let Some(path) = root.path_for_file(id).and_then(VfsPath::as_path) {
                    map.insert(*id, path.as_str().to_owned());
                }
            }
            self.paths = std::sync::Arc::new(map);
        }
        for &dependent in &lowering.dependents {
            let Some(krate) = crates.units[dependent].krate else { continue };
            let runs = &mut self.applied[dependent];
            runs.retain(|(it, _)| *it != run);
            let applying = Applied { cfgs: cfgs.clone(), env: ran.env.clone(), out_dir: out_dir.clone() };
            runs.push((run, applying));
            radix::sort_by_key(runs, |(it, _)| *it as u64);
            let base = &crates.units[dependent];
            let mut cfg = base.cfg.clone();
            let mut env: Env = base.env.iter().cloned().collect();
            for (_, applying) in runs.iter() {
                cfg.extend(applying.cfgs.iter().cloned());
                env.extend(applying.env.iter().cloned());
                env.set("OUT_DIR", applying.out_dir.clone());
            }
            if *krate.cfg_options(&*db) != cfg {
                krate.set_cfg_options(db, cfg);
            }
            if *krate.env(&*db) != env {
                krate.set_env(db, env);
            }
        }
        Ok(())
    }

    /// The expanders of proc-macro unit `unit` once it is built (or why it has none), for the
    /// crates depending on it to expand its macros with. Until then they are
    /// `ProcMacroLoadingError::NotYetBuilt`. A write: snapshots alive are cancelled as by
    /// [`Lowered::ran`].
    pub fn macros(&mut self, unit: usize, macros: ProcMacroLoadResult) -> Result<(), LowerError> {
        let Some(lowering) = self.crates.units.get(unit) else {
            return Err(LowerError::new(format!("the build has no unit {unit}")));
        };
        let (Some(krate), true) = (lowering.krate, lowering.proc_macro) else {
            return Err(LowerError::at("not a proc macro's crate", lowering.what.clone()));
        };
        let macros = macros.map(|mut macros| {
            macros.sort_unstable_by(|a, b| a.name.cmp_order(&b.name).then_with(|| a.kind.cmp(&b.kind)));
            macros
        });
        let db = &mut self.db;
        let all = ProcMacros::get(&*db);
        let mut by_crate = all.by_crate(&*db).clone();
        by_crate.insert(krate, Arc::new(CrateProcMacros::new(macros)));
        all.set_by_crate(db).with_durability(Durability::HIGH).to(by_crate);
        Ok(())
    }
}

/// Every file of the source roots `0..=sets` by its absolute path.
fn paths_of(db: &Db, sets: usize) -> HashMap<FileId, String> {
    let mut paths = HashMap::new();
    for set in 0..=sets {
        let root = db.source_root(SourceRootId(set as u32)).source_root(db);
        for file in root.iter() {
            if let Some(path) = root.path_for_file(&file).and_then(VfsPath::as_path) {
                paths.insert(file, path.as_str().to_owned());
            }
        }
    }
    paths
}

/// Puts `loaded`, the files under `base`, into the source root `set` of the `sets` packages'.
/// Files under `base` it lacks and none of `built` has are taken out.
fn set_files(
    db: &mut Db,
    base: &str,
    built: &[AbsPathBuf],
    set: usize,
    sets: usize,
    loaded: &[files::Loaded],
) -> Result<ViewChange, LowerError> {
    let base_path = abs(base)?;
    // Every file of `loaded` is under `base`, so the one prefix puts each in `set`: a partition
    // of every package's prefixes would put `/w/ab/x.rs` in `/w/a`'s, its prefixes being bytes.
    let mut partition = FileSetConfig::builder();
    for index in 0..sets {
        partition.add_file_set(if index == set { vec![VfsPath::from(base_path.clone())] } else { Vec::new() });
    }
    let partition = partition.build();
    let other = SourceRootId(sets as u32);
    let mut view = Vec::with_capacity(loaded.len());
    for file in loaded {
        let Some(path) = files::rest(&file.path, base).filter(|it| !it.is_empty()) else {
            return Err(LowerError::at(format!("`{}` is not under `{base}`", file.path), file.path.clone()));
        };
        let file = blaze_vfs::File { id: file.id, mode: blaze_vfs::Mode::Regular, bytes: &file.bytes };
        view.push(ViewFile { path, file });
    }
    let root = ViewRoot { base: &base_path, built, partition: &partition, other, files: &view };
    db.set_view_root(root).map_err(|error| {
        let at = match &error {
            base_db::ViewError::BadPath(path)
            | base_db::ViewError::Duplicate(path)
            | base_db::ViewError::Mismatch { path, .. }
            | base_db::ViewError::NoSourceRoot { path, .. }
            | base_db::ViewError::NoFileId(path) => format!("{base}/{path}"),
        };
        LowerError::at(error.to_string(), at)
    })
}

fn abs(path: &str) -> Result<AbsPathBuf, LowerError> {
    AbsPathBuf::try_from(path)
        .map_err(|_| LowerError::at(format!("`{path}` is not an absolute path"), path))
}

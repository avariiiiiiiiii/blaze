//! Each unit's plan: what the pinned nightly's cargo (`cad07cf8e597397fff5d0f6cb79c9f870c0b1dd2`)
//! hashes, passes rustc, sets and writes for it, computed as cargo computes it. Nothing here
//! reads a file: the manifests, the config and the standard library's facts come in.

mod env;
mod flags;
mod hash;
mod outputs;
#[cfg(test)]
mod tests;

use std::path::Path;

use blaze_manifest::Config;
use blaze_manifest::config::CompileKind;
use blaze_manifest::manifest::Manifest;
use blaze_manifest::profiles::{Profile as BaseProfile, ProfileRoot};
use blaze_manifest::targets::TargetKind as ManifestKind;
use blaze_resolve::PackageId;
use rustc_hash::{FxHashMap, FxHashSet};

use self::flags::KindInfo;
use self::hash::{CompileMode, HashedProfile, LtoUnit, Meta, MetaInput, RustcVersion, TargetKind, TrimPaths};
use self::outputs::{CompileFiles, Layout};
use crate::load::load_error;
use crate::query::{LoadError, UnitPlan};
use crate::schema::Unit;
use crate::stdlib::target as platform;
use crate::{Intent, Package};

fn error(what: String) -> LoadError {
    LoadError { what, at: None }
}

pub(crate) struct PlanInput<'a> {
    pub units: &'a [Unit],
    /// The standard library's units' plans, made by its own load; `None` for every other unit.
    pub std_plans: &'a [Option<UnitPlan>],
    /// Every package of the units, by the unit's `pkg_id`.
    pub packages: &'a FxHashMap<String, (PackageId, Package<'a>)>,
    /// Every package's manifest path, by its id.
    pub manifests: &'a FxHashMap<PackageId, (&'a Path, &'a Manifest)>,
    /// The graph's roots, as indexes of `units`.
    pub roots: &'a [usize],
    pub config: &'a Config,
    pub intent: Intent,
    /// Whether the standard library is built (`-Zbuild-std`): only then do its units' hashes
    /// go into their dependents'.
    pub build_std: bool,
    pub ws_root: &'a Path,
    pub ws_manifest: &'a Path,
    /// The sysroot, which the standard library's packages are hashed against.
    pub sysroot: Option<&'a Path>,
    pub target_dir: &'a Path,
    pub build_dir: &'a Path,
    pub host: &'a str,
    pub target: Option<&'a str>,
    /// The profile asked for, by name (`dev`), and its directory's name (`debug`).
    pub requested_profile: &'a str,
    pub profile_dir: &'a str,
    /// `Profiles::base_profile`: the requested profile, for its root and the `Finished` text.
    pub base_profile: &'a BaseProfile,
    pub jobs: Option<u32>,
}

pub(crate) struct PlanOutput {
    /// Of each unit, in the units' order.
    pub plans: Vec<UnitPlan>,
    /// What cargo prints after `Finished`: "`dev` profile [unoptimized + debuginfo]".
    pub finished: String,
}

/// The rustflags of every unit for `triple` in a build for `target`.
pub(crate) fn platform_rustflags(config: &Config, triple: &str, host: &str, target: Option<&str>) -> Result<Vec<String>, LoadError> {
    let kind = match target {
        Some(target) if target == triple => Some(triple),
        _ if triple == host => None,
        _ => return Err(error(format!("a build for {target:?} on `{host}` has no units for `{triple}`"))),
    };
    Ok(flags::kind_info(config, host, target, kind)?.rustflags)
}

/// cargo's `Finished` text of the profile asked for as `requested`.
fn finished(requested: &str, opt_level: &str, debuginfo: bool) -> String {
    let opt = if opt_level == "0" { "unoptimized" } else { "optimized" };
    let debug = if debuginfo { " + debuginfo" } else { "" };
    format!("`{requested}` profile [{opt}{debug}]")
}

pub(crate) fn plan(input: &PlanInput<'_>) -> Result<PlanOutput, LoadError> {
    plan_units(input).map_err(|mut error| {
        if error.at.is_none() {
            error.at = Some(input.ws_manifest.display().to_string());
        }
        error
    })
}

/// A unit's package as the plan uses it.
struct Pkg<'a> {
    id: PackageId,
    manifest: &'a Manifest,
    manifest_path: &'a Path,
}

/// What every unit planned here has worked out before its hashes.
struct Facts<'a> {
    unit: &'a Unit,
    pkg: Pkg<'a>,
    /// 0 the host, 1 the target.
    kind: usize,
    target_kind: TargetKind,
    mode: CompileMode,
    profile: HashedProfile<'a>,
}

/// What is the same for every unit.
struct Shared<'a> {
    input: &'a PlanInput<'a>,
    kinds: Vec<KindInfo>,
    kind_hashes: Vec<u64>,
    rustc: RustcVersion,
    default_lib_metadata: Option<&'a str>,
    new_layout: bool,
}

/// cargo's `should_use_metadata` (`use_extra_filename`) and `use_pkg_dir`, of a unit.
fn use_metadata(shared: &Shared<'_>, facts: &Facts<'_>) -> (bool, bool) {
    let mode = facts.mode;
    if mode.is_any_test() || mode.is_check() {
        return (true, true);
    }
    let kind = &facts.target_kind;
    if shared.new_layout {
        if kind.is_custom_build() {
            return (false, true);
        }
        if (kind.is_dylib() || kind.is_cdylib() || kind.is_executable()) && shared.default_lib_metadata.is_none() {
            return (false, true);
        }
        return (true, true);
    }
    let triple = shared.kinds[facts.kind].triple.as_str();
    let exe = kind.is_executable();
    let named = kind.is_dylib() || kind.is_cdylib() || (exe && triple == "wasm32-unknown-emscripten") || (exe && triple.contains("msvc"));
    if named && facts.pkg.id.source_id().is_path() && shared.default_lib_metadata.is_none() {
        return (false, false);
    }
    (true, true)
}

/// The target of the manifest a unit builds, for its `bin_name` and its kind.
fn manifest_target<'a>(manifest: &'a Manifest, kind: &TargetKind, name: &str) -> Result<&'a blaze_manifest::targets::Target, LoadError> {
    let same = |theirs: &ManifestKind| match (kind, theirs) {
        (TargetKind::Lib(_), ManifestKind::Lib(_))
        | (TargetKind::Bin, ManifestKind::Bin)
        | (TargetKind::Test, ManifestKind::Test)
        | (TargetKind::Bench, ManifestKind::Bench)
        | (TargetKind::ExampleLib(_) | TargetKind::ExampleBin, ManifestKind::ExampleLib(_) | ManifestKind::ExampleBin)
        | (TargetKind::CustomBuild, ManifestKind::CustomBuild) => true,
        _ => false,
    };
    manifest
        .targets
        .iter()
        .find(|it| it.name == name && same(&it.kind))
        .ok_or_else(|| error(format!("`{}` has no {kind:?} target `{name}`", manifest.package_id.name)))
}

fn plan_units(input: &PlanInput<'_>) -> Result<PlanOutput, LoadError> {
    let units = input.units;
    if input.std_plans.len() != units.len() {
        return Err(error(format!("{} standard library plans for {} units", input.std_plans.len(), units.len())));
    }
    if let Some(root) = input.roots.iter().find(|&&at| at >= units.len()) {
        return Err(error(format!("the unit graph's root {root} is no unit")));
    }
    let config = input.config;

    let wrapper = match config.env("RUSTC_WORKSPACE_WRAPPER") {
        Some(it) => !it.is_empty(),
        None => config.build_config().map_err(|e| load_error(&e, None))?.rustc_workspace_wrapper.is_some(),
    };
    if wrapper && input.packages.values().any(|(_, package)| package.is_member) {
        return Err(error("blaze runs no rustc wrapper: `RUSTC_WORKSPACE_WRAPPER` (`build.rustc-workspace-wrapper`) is set".to_owned()));
    }

    let mut kinds = vec![flags::kind_info(config, input.host, input.target, None)?];
    let mut kind_hashes = vec![hash::kind_fingerprint_hash(None)?];
    let mut layouts = vec![Layout::new(input.target_dir, input.build_dir, None, input.profile_dir)];
    if let Some(target) = input.target {
        kinds.push(flags::kind_info(config, input.host, input.target, Some(target))?);
        kind_hashes.push(hash::kind_fingerprint_hash(Some(target))?);
        layouts.push(Layout::new(input.target_dir, input.build_dir, Some(target), input.profile_dir));
    }
    let mut linkers = Vec::with_capacity(kinds.len());
    for (at, info) in kinds.iter().enumerate() {
        let kind = if at == 0 { CompileKind::Host } else { CompileKind::Target(info.triple.clone()) };
        let linker = config.target_linker(&kind, input.host, &info.cfgs).map_err(|e| load_error(&e, None))?;
        linkers.push(linker.map(|it| it.display().to_string()));
    }

    let mut config_env = Vec::new();
    for (name, value) in config.env_config().map_err(|e| load_error(&e, None))? {
        match value.into_string() {
            Ok(value) => config_env.push((name, value)),
            Err(_) => return Err(error(format!("the `[env]` variable `{name}` is not UTF-8"))),
        }
    }

    let shared = Shared {
        input,
        kinds,
        kind_hashes,
        rustc: RustcVersion::of(platform::rustc_version(), || platform::rustc_verbose_version(input.host))?,
        default_lib_metadata: config.env("__CARGO_DEFAULT_LIB_METADATA"),
        new_layout: config.env("__CARGO_TEMPORARY_BUILD_DIR_NEW_LAYOUT_OPT_OUT") != Some("1"),
    };
    let stop_rlib_uplift = shared.rustc.is_nightly() && config.env("__CARGO_TEMPORARY_STOP_RLIB_UPLIFT_OPT_OUT") != Some("1");

    // What each unit planned here is, and the dependencies its hashes take.
    let mut facts: Vec<Option<Facts<'_>>> = Vec::with_capacity(units.len());
    let mut deps: Vec<Vec<usize>> = Vec::with_capacity(units.len());
    for (at, unit) in units.iter().enumerate() {
        let mut edges = Vec::with_capacity(unit.dependencies.len());
        for dep in &unit.dependencies {
            let to = units
                .get(dep.index)
                .ok_or_else(|| error(format!("unit {at} depends on unit {}, which there is not", dep.index)))?;
            if !to.is_std || unit.is_std || input.build_std {
                edges.push(dep.index);
            }
        }
        deps.push(edges);
        if input.std_plans[at].is_some() {
            facts.push(None);
            continue;
        }
        let (id, _) = input
            .packages
            .get(&unit.pkg_id)
            .ok_or_else(|| error(format!("the unit graph's package `{}` is not in the resolve", unit.pkg_id)))?;
        let (manifest_path, manifest) = *input
            .manifests
            .get(id)
            .ok_or_else(|| error(format!("the package `{}` has no manifest", unit.pkg_id)))?;
        let kind = match unit.platform.as_deref() {
            None => 0,
            Some(triple) if Some(triple) == input.target => 1,
            Some(triple) => {
                return Err(error(format!(
                    "the unit graph has a unit of `{}` for `{triple}` in a build for {:?}",
                    unit.pkg_id, input.target
                )));
            }
        };
        let profile = HashedProfile::of(&unit.profile)?;
        if profile.trim_paths != TrimPaths::None {
            return Err(error(format!("blaze does not trim paths (`trim-paths` of `{}`'s profile)", unit.pkg_id)));
        }
        facts.push(Some(Facts {
            unit,
            pkg: Pkg { id: *id, manifest, manifest_path },
            kind,
            target_kind: TargetKind::of(&unit.target),
            mode: CompileMode::of(unit)?,
            profile,
        }));
    }

    // `lto::generate`, over the units planned here.
    let lto_deps: Vec<Vec<usize>> = deps
        .iter()
        .map(|edges| edges.iter().copied().filter(|&to| facts[to].is_some()).collect())
        .collect();
    let lto_units: Vec<Option<LtoUnit<'_>>> = facts
        .iter()
        .zip(&lto_deps)
        .map(|(it, edges)| {
            it.as_ref().map(|it| LtoUnit { kind: &it.target_kind, mode: it.mode, lto: &it.profile.lto, deps: edges })
        })
        .collect();
    let ltos = hash::generate(&lto_units, input.roots);

    // `compute_metadata`, each unit after its dependencies.
    let mut metas: Vec<Option<Meta>> = vec![None; units.len()];
    for at in 0..units.len() {
        meta_of(&shared, &facts, &deps, &ltos, &mut metas, at, 0)?;
    }
    let metas: Vec<Meta> = metas.into_iter().map(|it| it.expect("every unit's metadata is computed")).collect();

    // cargo's `pkg_dir` of each unit planned here.
    let mut pkg_dirs: Vec<String> = Vec::with_capacity(units.len());
    for (at, it) in facts.iter().enumerate() {
        let Some(it) = it else {
            pkg_dirs.push(String::new());
            continue;
        };
        let hash = if metas[at].pkg_dir {
            format!("{:016x}", metas[at].unit_id)
        } else {
            hash::target_short_hash(it.pkg.id, input.ws_root)?
        };
        let separator = if shared.new_layout { '/' } else { '-' };
        pkg_dirs.push(format!("{}{separator}{hash}", it.pkg.id.name()));
    }
    let run_out_dirs: Vec<Option<String>> = facts
        .iter()
        .zip(&pkg_dirs)
        .map(|(it, pkg_dir)| match it {
            Some(it) if it.mode == CompileMode::RunCustomBuild => {
                Some(outputs::run_files(&layouts[it.kind], shared.new_layout, pkg_dir).0)
            }
            _ => None,
        })
        .collect();

    let roots: FxHashSet<usize> = input.roots.iter().copied().collect();
    let primary: FxHashSet<&str> = input.roots.iter().map(|&at| units[at].pkg_id.as_str()).collect();
    let mut package_envs: FxHashMap<&str, Vec<(String, String)>> = FxHashMap::default();
    let mut package_lints: FxHashMap<&str, (Vec<String>, Vec<String>)> = FxHashMap::default();
    let artifact_dir = !matches!(input.intent, Intent::Check { .. });

    let mut plans = Vec::with_capacity(units.len());
    for (at, it) in facts.iter().enumerate() {
        let Some(it) = it else {
            plans.push(input.std_plans[at].clone().expect("a unit planned elsewhere has its plan"));
            continue;
        };
        let unit = it.unit;
        let meta = &metas[at];
        let info = &shared.kinds[it.kind];
        let layout = &layouts[it.kind];
        if !package_envs.contains_key(unit.pkg_id.as_str()) {
            let env = env::package_env(it.pkg.id, it.pkg.manifest, it.pkg.manifest_path)?;
            package_envs.insert(unit.pkg_id.as_str(), env);
        }
        let package_env = &package_envs[unit.pkg_id.as_str()];
        let links = it.pkg.manifest.metadata.links.clone();

        if it.mode == CompileMode::RunCustomBuild {
            let (out_dir, files) = outputs::run_files(layout, shared.new_layout, &pkg_dirs[at]);
            let profile_root = match input.base_profile.root {
                ProfileRoot::Release => "release",
                ProfileRoot::Debug => "debug",
            };
            let env = env::run_env(
                env::RunEnv {
                    package: package_env,
                    out_dir: &out_dir,
                    jobs: input.jobs,
                    target: &info.triple,
                    host: input.host,
                    debug: it.profile.debuginfo.is_turned_on(),
                    opt_level: &unit.profile.opt_level,
                    profile: profile_root,
                    linker: linkers[it.kind].as_deref(),
                    links: links.as_deref(),
                    features: &unit.features,
                    debug_assertions: unit.profile.debug_assertions,
                    cfg_lines: &info.cfg_lines,
                    rustflags: &info.rustflags,
                },
                &config_env,
            );
            plans.push(UnitPlan {
                checked_as_test: unit.checked_as_test,
                metadata: meta.metadata(),
                extra_filename: meta.extra_filename(),
                cfgs: Vec::new(),
                check_cfg: Vec::new(),
                rustflags: Vec::new(),
                lints: Vec::new(),
                env,
                outputs: files,
                out_dir: Some(out_dir),
                links,
            });
            continue;
        }

        let target = manifest_target(it.pkg.manifest, &it.target_kind, &unit.target.name)?;
        let crate_name = unit.target.name.replace('-', "_");
        let is_local = it.pkg.id.source_id().is_path() && !unit.is_std;
        let (lints, lint_check_cfg) = if is_local {
            match package_lints.get(unit.pkg_id.as_str()) {
                Some(found) => found.clone(),
                None => {
                    let found = flags::lints(it.pkg.manifest)?;
                    package_lints.insert(unit.pkg_id.as_str(), found.clone());
                    found
                }
            }
        } else {
            (Vec::new(), Vec::new())
        };
        let mut check_cfg = flags::check_cfg(it.pkg.manifest);
        check_cfg.extend(lint_check_cfg);

        let out_dir = unit
            .dependencies
            .iter()
            .filter_map(|dep| run_out_dirs[dep.index].as_deref())
            .next_back();
        let test_or_bench = it.target_kind.is_test_or_bench();
        let mut bin_exes = Vec::new();
        if test_or_bench {
            for bin in it.pkg.manifest.targets.iter().filter(|it| it.is_bin()) {
                let name = bin.bin_name.as_deref().unwrap_or(&bin.name);
                let path = if artifact_dir {
                    outputs::bin_link(layout, &info.triple, &bin.name, bin.bin_name.as_deref())?.display().to_string()
                } else {
                    format!("placeholder:{name}")
                };
                bin_exes.push((format!("CARGO_BIN_EXE_{name}"), path));
            }
        }
        let env = env::compile_env(
            env::CompileEnv {
                package: package_env,
                crate_name: &crate_name,
                bin_name: it.target_kind.is_executable().then(|| target.bin_name.as_deref().unwrap_or(&target.name)),
                primary: primary.contains(unit.pkg_id.as_str()),
                tmpdir: test_or_bench.then(|| layout.tmp().display().to_string()),
                out_dir,
                bin_exes,
            },
            &config_env,
        );

        let extra = meta.extra_hex();
        let files = outputs::compile_outputs(&CompileFiles {
            mode: it.mode,
            kind: &it.target_kind,
            name: &unit.target.name,
            crate_name: &crate_name,
            bin_name: target.bin_name.as_deref(),
            package: it.pkg.id.name().as_str(),
            triple: &info.triple,
            extra: extra.as_deref(),
            pkg_dir: &pkg_dirs[at],
            layout,
            host_layout: &layouts[0],
            new_layout: shared.new_layout,
            artifact_dir,
            stop_rlib_uplift,
            is_root: roots.contains(&at),
        })?;

        let mut rustflags = unit.profile.rustflags.clone();
        rustflags.extend(info.rustflags.iter().cloned());
        plans.push(UnitPlan {
            checked_as_test: unit.checked_as_test,
            metadata: meta.metadata(),
            extra_filename: meta.extra_filename(),
            cfgs: flags::cfgs(&unit.profile.rustflags, &unit.features, &info.rustflags),
            check_cfg,
            rustflags,
            lints,
            env,
            outputs: files,
            out_dir: None,
            links,
        });
    }

    let base = input.base_profile;
    Ok(PlanOutput { plans, finished: finished(input.requested_profile, &base.opt_level, base.debuginfo.is_turned_on()) })
}

/// The unit's `Metadata`, its dependencies' first.
fn meta_of(
    shared: &Shared<'_>,
    facts: &[Option<Facts<'_>>],
    deps: &[Vec<usize>],
    ltos: &[Option<hash::Lto>],
    metas: &mut [Option<Meta>],
    at: usize,
    depth: usize,
) -> Result<Meta, LoadError> {
    if let Some(meta) = metas[at] {
        return Ok(meta);
    }
    let input = shared.input;
    let unit = &input.units[at];
    if depth > input.units.len() {
        return Err(error(format!("the unit graph has a cycle through `{}`'s `{}`", unit.pkg_id, unit.target.name)));
    }
    let meta = match &facts[at] {
        None => {
            let plan = input.std_plans[at].as_ref().expect("a unit planned elsewhere has its plan");
            let what = format!("the standard library's unit of `{}`", unit.pkg_id);
            Meta::read(&plan.metadata, &plan.extra_filename, &what)?
        }
        Some(it) => {
            let mut dep_metas = Vec::with_capacity(deps[at].len());
            for &dep in &deps[at] {
                dep_metas.push(meta_of(shared, facts, deps, ltos, metas, dep, depth + 1)?);
            }
            let lto = ltos[at].as_ref().ok_or_else(|| {
                error(format!("`{}`'s `{}` is reached from no root of the unit graph", unit.pkg_id, unit.target.name))
            })?;
            let root = if unit.is_std {
                input.sysroot.ok_or_else(|| error(format!("`{}` is a standard library unit, and no sysroot was given", unit.pkg_id)))?
            } else {
                input.ws_root
            };
            let (c_extra_filename, pkg_dir) = use_metadata(shared, it);
            hash::compute_metadata(
                &MetaInput {
                    package: it.pkg.id,
                    root,
                    features: &unit.features,
                    profile: &it.profile,
                    mode: it.mode,
                    lto,
                    kind_hash: shared.kind_hashes[it.kind],
                    host_unit: it.kind == 0,
                    target_name: &unit.target.name,
                    target_kind: &it.target_kind,
                    rustc: &shared.rustc,
                    host: input.host,
                    default_lib_metadata: shared.default_lib_metadata,
                    is_std: unit.is_std,
                    rustflags: &shared.kinds[it.kind].rustflags,
                    c_extra_filename,
                    pkg_dir,
                },
                &dep_metas,
            )?
        }
    };
    metas[at] = Some(meta);
    Ok(meta)
}

//! A unit compiled by blaze's code generator and linked by blaze's linker: its policy (the
//! unit's profile, and each package's where cargo's profile gives it another), its roots (a
//! program's `main`, a test harness's, a library's functions), the build through
//! `blaze_codegen::driver::Build`, and the link into an image kept between jobs, so the next
//! link of the same file relinks only what changed.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Mutex,
};

use blaze_codegen::{
    InstanceKey, NativeLib, NativeLibKind, Target,
    driver::{Build, Built},
    plan::{Mode as BuildMode, OptLevel, Origin, Override, Policy, Profile},
    settings::{DebugInfo, Panic, Toolchain},
};
use blaze_build_script::{
    apply::{Compile, GraphUnit, Linked as LinkedTo, to_link},
    unit as bs,
};
use blaze_ids::ContentId;
use blaze_items::ItemKey;
use blaze_link::{Change, Image, Native, Output};
use blaze_units::{
    query::Units,
    schema::{self, Mode, Unit},
};
use hir_ty::db::HirDatabase;
use hir_ty_codegen::v2::{
    Backends, BuildTime, Databases, HirFrontend, HirInstance, ItemHashes, RootKind, Roots,
};

use super::{Job, files::Mounted, script};

/// What a unit makes, as the linker sees it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Makes {
    /// An `rlib` (or a check's `rmeta`): nothing linked; the functions it compiles are in the
    /// store for the programs that link it.
    Library,
    /// A binary, an example, a test, a build script.
    Executable,
    /// A `cdylib` or a proc macro, under its file's name.
    Shared { proc_macro: bool },
}

/// What `unit` makes; an error for what blaze does not make.
pub(crate) fn makes(unit: &Unit) -> Result<Makes, String> {
    let target = &unit.target;
    let has = |kind: &str| target.crate_types.iter().any(|it| it == kind) || target.kind.iter().any(|it| it == kind);
    if matches!(unit.mode, Mode::Test) || target.is_custom_build() || has("bin") {
        return Ok(Makes::Executable);
    }
    if target.is_proc_macro() {
        return Ok(Makes::Shared { proc_macro: true });
    }
    if has("cdylib") {
        return Ok(Makes::Shared { proc_macro: false });
    }
    if has("staticlib") || has("dylib") {
        let kind = if has("staticlib") { "staticlib" } else { "dylib" };
        return Err(format!(
            "`{}` asks for a `{kind}`, which blaze's linker does not write: crate types `lib`, \
             `rlib`, `cdylib`, `proc-macro` and `bin` are what it builds",
            target.name
        ));
    }
    Ok(Makes::Library)
}

fn root_kind(unit: &Unit, makes: &Makes) -> RootKind {
    match makes {
        Makes::Executable if matches!(unit.mode, Mode::Test) => RootKind::Test,
        Makes::Executable => RootKind::Executable,
        Makes::Shared { proc_macro: true } => RootKind::ProcMacro,
        Makes::Shared { proc_macro: false } => RootKind::Shared,
        Makes::Library => RootKind::Library,
    }
}

/// The triple `unit` is built for.
pub(crate) fn triple<'a>(job: &'a Job, unit: &'a Unit) -> &'a str {
    unit.platform.as_deref().unwrap_or(job.input.config.host.as_str())
}

/// The toolchain `unit` is compiled with: its target, the `-C target-cpu`,
/// `-C target-feature` of its rustflags, the backends of this blaze.
pub(crate) fn toolchain(job: &Job, unit: usize) -> Result<Toolchain, String> {
    let u = job.unit(unit).map_err(|it| it.what)?;
    let plan = job.plan(unit).map_err(|it| it.what)?;
    hir_ty_codegen::v2::toolchain(triple(job, u), &plan.rustflags)
}

fn opt_level(level: &str) -> Result<OptLevel, String> {
    Ok(match level {
        "0" => OptLevel::O0,
        "1" => OptLevel::O1,
        "2" => OptLevel::O2,
        "3" => OptLevel::O3,
        "s" => OptLevel::S,
        "z" => OptLevel::Z,
        other => return Err(format!("opt-level `{other}` is none rustc takes")),
    })
}

pub(crate) fn debuginfo(debuginfo: &schema::DebugInfo) -> Result<DebugInfo, String> {
    Ok(match debuginfo {
        schema::DebugInfo::Level(0) => DebugInfo::None,
        schema::DebugInfo::Level(1) => DebugInfo::Limited,
        schema::DebugInfo::Level(2) => DebugInfo::Full,
        schema::DebugInfo::Named(name) => match name.as_str() {
            "none" | "false" => DebugInfo::None,
            "line-tables-only" | "line-directives-only" => DebugInfo::LineTablesOnly,
            "limited" => DebugInfo::Limited,
            "full" | "true" => DebugInfo::Full,
            other => return Err(format!("debuginfo `{other}` is none rustc takes")),
        },
        schema::DebugInfo::Level(other) => return Err(format!("debuginfo `{other}` is none rustc takes")),
    })
}

fn panic(panic: &str) -> Result<Panic, String> {
    match panic {
        "unwind" => Ok(Panic::Unwind),
        "abort" => Ok(Panic::Abort),
        other => Err(format!("panic strategy `{other}` is none rustc takes")),
    }
}

/// Every unit `unit` depends on, itself first, each once, dependencies before what depends on
/// them after the first.
pub(crate) fn closure(job: &Job, unit: usize) -> Vec<usize> {
    let units = &job.units().graph.units;
    let mut seen = BTreeSet::new();
    let mut order = Vec::new();
    let mut stack = vec![(unit, false)];
    while let Some((at, expanded)) = stack.pop() {
        if expanded {
            order.push(at);
            continue;
        }
        if !seen.insert(at) {
            continue;
        }
        stack.push((at, true));
        if let Some(it) = units.get(at) {
            for dep in it.dependencies.iter().rev() {
                if !seen.contains(&dep.index) {
                    stack.push((dep.index, false));
                }
            }
        }
    }
    order.reverse();
    order
}

/// The crates compiled for build scripts and proc macros, not for the program: every unit a
/// build script's or proc macro's compile depends on.
fn build_time(job: &Job) -> BuildTime {
    let units = &job.units().graph.units;
    let mut crates = rustc_hash::FxHashSet::default();
    for (at, unit) in units.iter().enumerate() {
        let tool = (unit.target.is_custom_build() && !matches!(unit.mode, Mode::RunCustomBuild))
            || unit.target.is_proc_macro();
        if !tool {
            continue;
        }
        for dep in closure(job, at) {
            if let Some(krate) = job.lowered.krate(dep) {
                crates.insert(krate);
            }
        }
    }
    BuildTime(crates)
}

fn is_build_time(unit: &Unit) -> bool {
    unit.target.is_custom_build() || unit.target.is_proc_macro()
}

/// Whose code `unit`'s roots are, for their settings.
pub(crate) fn origin(job: &Job, unit: &Unit) -> Result<Origin, String> {
    let package = job.package(unit).map_err(|it| it.what)?;
    Ok(match () {
        _ if unit.is_std => Origin::Std,
        _ if is_build_time(unit) => Origin::BuildTime(package.name.clone()),
        _ if package.is_member => Origin::Member(package.name.clone()),
        _ => Origin::Dependency(package.name.clone()),
    })
}

/// How `unit`'s build compiles every function: its own profile, and for each package of its
/// closure whose unit cargo gives another profile (`[profile.*.package.*]`), that one.
pub(crate) fn policy(job: &Job, unit: usize, toolchain: Toolchain) -> Result<Policy, String> {
    let root = job.unit(unit).map_err(|it| it.what)?;
    let base = &root.profile;
    let opt = opt_level(&base.opt_level)?;
    let info = debuginfo(&base.debuginfo)?;
    let mut packages = BTreeMap::new();
    for dep in closure(job, unit).into_iter().skip(1) {
        let dep = job.unit(dep).map_err(|it| it.what)?;
        if dep.is_std {
            continue;
        }
        let name = job.package(dep).map_err(|it| it.what)?.name.clone();
        let profile = &dep.profile;
        let level = opt_level(&profile.opt_level)?;
        let dep_info = debuginfo(&profile.debuginfo)?;
        let differs = Override {
            opt_level: (level != opt).then_some(level),
            overflow_checks: (profile.overflow_checks != base.overflow_checks).then_some(profile.overflow_checks),
            debug_assertions: (profile.debug_assertions != base.debug_assertions)
                .then_some(profile.debug_assertions),
            debuginfo: (dep_info != info).then_some(dep_info),
        };
        if differs != Override::default() {
            packages.insert(name, differs);
        }
    }
    Ok(Policy {
        toolchain,
        profile: Profile {
            opt_level: opt,
            overflow_checks: base.overflow_checks,
            debug_assertions: base.debug_assertions,
            debuginfo: info,
            panic: panic(&base.panic)?,
            packages,
            build_override: Override::default(),
        },
        mode: BuildMode::Output,
        fold: true,
    })
}

/// What a unit's codegen made.
pub(crate) struct Generated {
    pub(crate) built: Built,
    pub(crate) roots: Roots,
    pub(crate) frontend: HirFrontend,
    pub(crate) policy: Policy,
}

/// `unit`'s functions compiled from its roots: `Err` with the code generator's errors, each
/// naming the function it could not compile.
pub(crate) fn generate(
    job: &Job,
    unit: usize,
    makes: &Makes,
    mounted: &Mounted,
) -> Result<Result<Generated, Vec<String>>, String> {
    let u = job.unit(unit).map_err(|it| it.what)?;
    let krate = job
        .lowered
        .krate(unit)
        .ok_or_else(|| format!("unit {unit} (`{}`) has no crate in the front end", u.target.name))?;
    let toolchain = toolchain(job, unit)?;
    let policy = policy(job, unit, toolchain)?;
    let snapshot = Mutex::new(job.lowered.snapshot());
    let databases = Databases::new(move || {
        let snapshot = snapshot.lock().unwrap_or_else(|it| it.into_inner());
        Box::new(snapshot.clone()) as Box<dyn HirDatabase + Send>
    });
    let names = {
        let mounted = mounted.clone();
        let paths = job.lowered.paths();
        move |file: base_db::FileId| -> Option<String> { paths.get(&file).map(|path| mounted.name(path)) }
    };
    let frontend = HirFrontend::new(databases, Box::new(ItemHashes::new(Box::new(names))), build_time(job));
    let roots = match frontend.roots(krate, root_kind(u, makes)) {
        Ok(it) => it,
        Err(error) => return Ok(Err(vec![error.what])),
    };
    let origin = origin(job, u)?;
    let asked: Vec<(HirInstance, Origin)> = roots.roots.iter().map(|it| (it.clone(), origin.clone())).collect();
    let backends = Backends::default();
    let built = {
        let store = job.input.store.read().unwrap_or_else(|it| it.into_inner());
        Build::new(&backends, &frontend, &policy, &job.shared.memo).run(&asked, &*store)
    };
    match built {
        Ok(built) => Ok(Ok(Generated { built, roots, frontend, policy })),
        Err(errors) => Ok(Err(errors.into_iter().map(|it| it.what).collect())),
    }
}

/// An image kept linked at one path, and what is in it, so the next link of it is a change.
pub(crate) struct Linked {
    image: Image,
    output: Output,
    functions: BTreeSet<InstanceKey>,
    statics: BTreeSet<(ItemKey, ContentId)>,
    natives: BTreeSet<ContentId>,
    names: BTreeSet<String>,
}

/// `unit` as cargo's `build_map` reads it: its dependencies sorted by package id, its package's
/// script runs, whether it is built for the host, whether it links into what depends on it.
fn graph_unit(units: &Units, unit: usize) -> GraphUnit<usize, usize> {
    let all = &units.graph.units;
    let Some(it) = all.get(unit) else {
        return GraphUnit { deps: Vec::new(), own_runs: Vec::new(), for_host: false, linkable: false };
    };
    let mut deps: Vec<usize> = it.dependencies.iter().map(|dep| dep.index).collect();
    deps.sort_by(|a, b| all[*a].pkg_id.cmp(&all[*b].pkg_id));
    let own_runs = match it.target.is_custom_build() {
        true => Vec::new(),
        false => deps
            .iter()
            .copied()
            .filter(|dep| all[*dep].mode == Mode::RunCustomBuild && all[*dep].pkg_id == it.pkg_id)
            .collect(),
    };
    let linkable = it.target.is_lib()
        && it.target.crate_types.iter().any(|it| matches!(it.as_str(), "lib" | "rlib" | "dylib" | "proc-macro"));
    GraphUnit { deps, own_runs, for_host: it.target.is_custom_build() || it.target.is_proc_macro(), linkable }
}

/// What rustc is given for `unit` from the build scripts it is linked with
/// ([`blaze_build_script::apply::Compile`]).
fn given(job: &Job, unit: usize, scripts: &BTreeMap<usize, script::Applied>) -> Result<Compile, String> {
    let units = job.units();
    let u = job.unit(unit).map_err(|it| it.what)?;
    let mut memo = rustc_hash::FxHashMap::default();
    let to_link = to_link(unit, &|at: &usize| graph_unit(units, *at), &mut memo).to_link;
    let mut linked = Vec::new();
    let mut own = Vec::new();
    for run in to_link {
        let applied = scripts.get(&run).ok_or_else(|| {
            format!("the build script run of `{}` that `{}` is linked with has not run", units.graph.units[run].pkg_id, u.target.name)
        })?;
        let mine = units.graph.units[run].pkg_id == u.pkg_id;
        linked.push(LinkedTo { own: mine, output: &applied.output });
        if mine {
            own.push(&applied.output);
        }
    }
    let package = bs::Package { targets: script::targets(units, &u.pkg_id), ..bs::Package::default() };
    Ok(Compile::of(&package, &script::target_of(u), matches!(u.mode, Mode::Test), &linked, &own))
}

/// The native libraries `unit` links, in rustc's order: each linked crate of its closure, what
/// depends first, the `-l`s its rustc is given (its build script's `rustc-link-lib`s) then its
/// `#[link]`s, then the unit's own `rustc-link-arg`s; and where to look for them, the unit's
/// `-L`s.
fn natives(job: &Job, unit: usize, frontend: &HirFrontend) -> Result<(Vec<NativeLib>, Vec<PathBuf>), String> {
    let scripts = job.scripts.lock().unwrap_or_else(|it| it.into_inner());
    let units = job.units();
    let mine = given(job, unit, &scripts)?;
    // The crates linked in: the unit's, and each library reached through libraries, not through
    // a build script or proc macro, which are built for the host.
    let mut linked_in = BTreeSet::from([unit]);
    let mut pending = vec![unit];
    while let Some(at) = pending.pop() {
        for dep in graph_unit(units, at).deps {
            let node = graph_unit(units, dep);
            if node.linkable && !node.for_host && linked_in.insert(dep) {
                pending.push(dep);
            }
        }
    }
    let mut libs = Vec::new();
    for at in closure(job, unit).into_iter().filter(|it| linked_in.contains(it)) {
        let compile = match at == unit {
            true => mine.clone(),
            false => given(job, at, &scripts)?,
        };
        for lib in &compile.libs {
            libs.push(link_lib(lib)?);
        }
        if let Some(krate) = job.lowered.krate(at) {
            libs.extend(frontend.linked(krate).map_err(|it| it.what)?);
        }
    }
    for arg in &mine.link_args {
        libs.push(link_arg(arg)?);
    }
    Ok((libs, mine.search))
}

/// The bytes of a file, by its path.
pub(crate) type Read = (String, Vec<u8>);

/// `generated` linked into the image at `path` as `makes` says: the file's bytes, and every
/// native input the link read off the machine.
pub(crate) fn link(
    job: &Job,
    unit: usize,
    path: &str,
    makes: &Makes,
    generated: &Generated,
) -> Result<(Vec<u8>, Vec<Read>), String> {
    let u = job.unit(unit).map_err(|it| it.what)?;
    let built = &generated.built;
    let debug = match generated.policy.profile.debuginfo {
        DebugInfo::None => blaze_link::Debug::None,
        _ => blaze_link::Debug::Lines,
    };
    let shared = match makes {
        Makes::Shared { .. } => {
            let name = path.rsplit('/').next().unwrap_or(path).to_owned();
            let exports = built.symbols.iter().map(|it| it.1.clone()).collect();
            Some((name, exports))
        }
        _ => None,
    };
    let output = blaze_link::target::output(triple(job, u), shared, debug, true).map_err(|it| it.to_string())?;
    let (libs, search) = natives(job, unit, &generated.frontend)?;
    let owned = blaze_link::target::natives(&output, &libs, &search).map_err(|it| it.to_string())?;
    let side = generated.frontend.link_inputs(built).map_err(|it| it.what)?;

    let mut images = job.shared.images.lock().unwrap_or_else(|it| it.into_inner());
    let kept = images.remove(path).filter(|it| it.output == output);
    drop(images);
    let mut linked = match kept {
        Some(it) => it,
        None => {
            let to = blaze_core::FileName::new(path.trim_start_matches('/'))
                .ok_or_else(|| format!("`{path}` is no file the linker can name"))?;
            Linked {
                image: Image::new(to, output.clone()).map_err(|it| it.to_string())?,
                output: output.clone(),
                functions: BTreeSet::new(),
                statics: BTreeSet::new(),
                natives: BTreeSet::new(),
                names: BTreeSet::new(),
            }
        }
    };

    let functions: BTreeSet<InstanceKey> = built.functions.iter().map(|it| it.key).collect();
    let statics: BTreeSet<(ItemKey, ContentId)> = built.statics.iter().map(|it| (it.item, it.instance)).collect();
    let native_ids: BTreeSet<ContentId> = owned.iter().map(|it| it.id).collect();
    let patchable: BTreeSet<InstanceKey> = built.patchable.iter().copied().collect();
    let mut names: Vec<(String, Target)> = built
        .symbols
        .iter()
        .map(|(key, name)| {
            let target = match patchable.contains(key) {
                true => Target::Function(*key),
                false => Target::Direct(*key),
            };
            (name.clone(), target)
        })
        .collect();
    names.extend(
        built
            .static_symbols
            .iter()
            .map(|(item, instance, name)| (name.clone(), Target::Static { item: *item, instance: *instance })),
    );
    let name_set: BTreeSet<String> = names.iter().map(|it| it.0.clone()).collect();
    let change = Change {
        added: built.functions.iter().filter(|it| !linked.functions.contains(&it.key)).collect(),
        removed: linked.functions.difference(&functions).copied().collect(),
        // A static's value is not in its key: every static is put again.
        statics: built.statics.iter().collect(),
        removed_statics: linked.statics.iter().copied().collect(),
        names,
        unnamed: linked.names.difference(&name_set).cloned().collect(),
        debug_names: side.debug_names.clone(),
        natives: owned
            .iter()
            .enumerate()
            .filter(|(_, it)| !linked.natives.contains(&it.id))
            .map(|(order, it)| Native {
                id: it.id,
                name: &it.name,
                bytes: &it.bytes,
                kind: it.kind,
                order: order as u32,
            })
            .collect(),
        removed_natives: linked.natives.difference(&native_ids).copied().collect(),
        spans: side.spans.iter().map(|(anchor, bytes)| (*anchor, bytes.as_slice())).collect(),
    };
    let staged = linked.image.stage(change).map_err(|it| it.to_string())?;
    let bytes = linked.image.bytes(staged).map_err(|it| it.to_string())?;
    linked.functions = functions;
    linked.statics = statics;
    linked.natives = native_ids;
    linked.names = name_set;
    job.shared.images.lock().unwrap_or_else(|it| it.into_inner()).insert(path.to_owned(), linked);
    let read = owned.into_iter().map(|it| (it.name, it.bytes)).collect();
    Ok((bytes, read))
}

/// A `rustc-link-lib` as rustc takes `-l`: `[KIND[:MODIFIERS]=]NAME[:RENAME]`.
pub(crate) fn link_lib(value: &str) -> Result<NativeLib, String> {
    let (kind, name) = match value.split_once('=') {
        Some((kind, name)) => (Some(kind), name),
        None => (None, value),
    };
    let (kind, modifiers) = match kind.map(|it| it.split_once(':').unwrap_or((it, ""))) {
        Some((kind, modifiers)) => (kind, modifiers),
        None => ("dylib", ""),
    };
    let mut whole = false;
    let mut bundle = true;
    let mut verbatim = false;
    for modifier in modifiers.split(',').filter(|it| !it.is_empty()) {
        let (on, name) = match modifier.split_at(1) {
            ("+", name) => (true, name),
            ("-", name) => (false, name),
            _ => return Err(format!("`-l {value}`: a link modifier starts with `+` or `-`")),
        };
        match name {
            "whole-archive" => whole = on,
            "bundle" => bundle = on,
            "verbatim" => verbatim = on,
            "as-needed" => {}
            other => return Err(format!("`-l {value}`: unknown link modifier `{other}`")),
        }
    }
    let kind = match kind {
        "dylib" => NativeLibKind::Dylib,
        "static" => NativeLibKind::Static { whole, bundle },
        "framework" => NativeLibKind::Framework,
        "link-arg" => return Err(format!("`-l {value}`: blaze's linker takes no raw link argument")),
        other => return Err(format!("`-l {value}`: unknown library kind `{other}`")),
    };
    // `NAME:RENAME` links `RENAME` where the crate's `#[link]` says `NAME`.
    let name = name.split_once(':').map_or(name, |it| it.1);
    if name.is_empty() {
        return Err(format!("`-l {value}` names no library"));
    }
    Ok(NativeLib { name: name.to_owned(), kind, verbatim })
}

/// What blaze's linker does with a `rustc-link-arg`: `-lNAME` and `-pthread` (gcc's
/// `-lpthread`) are libraries; any other is an error naming it.
pub(crate) fn link_arg(arg: &str) -> Result<NativeLib, String> {
    if arg == "-pthread" {
        return Ok(NativeLib { name: "pthread".to_owned(), kind: NativeLibKind::Dylib, verbatim: false });
    }
    match arg.strip_prefix("-l") {
        Some(name) if !name.is_empty() => {
            Ok(NativeLib { name: name.to_owned(), kind: NativeLibKind::Dylib, verbatim: false })
        }
        _ => Err(format!("blaze's linker takes no link argument `{arg}`")),
    }
}

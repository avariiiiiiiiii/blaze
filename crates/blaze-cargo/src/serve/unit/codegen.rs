//! A unit compiled by blaze's code generator and linked by blaze's linker: its policy (the
//! unit's profile, and each package's where cargo's profile gives it another), its roots (a
//! program's `main`, a test harness's, a library's functions), the build through
//! `blaze_codegen::driver::Build`, and the link into an image kept between jobs, so the next
//! link of the same file relinks only what changed.

#![forbid(unsafe_code)]

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use blaze_codegen::{
    InstanceKey, NativeLib, NativeLibKind, Target,
    driver::{Build, Built},
    plan::{Mode as BuildMode, OptLevel, Origin, Override, Policy, Profile},
    settings::{DebugInfo, Panic, Toolchain},
};
use blaze_build_script::{
    apply::{Compile, Linked as LinkedTo},
    unit as bs,
};
use blaze_ids::ContentId;
use blaze_items::ItemKey;
use blaze_link::{Change, Image, Native, Output};
use blaze_units::schema::{self, Mode, Unit};
use hir_ty_codegen::v2::{
    Backends, BuildTime, Databases, HirFrontend, HirInstance, ItemHashes, RootKind, Roots,
};

use super::{Job, UnitReader, files::Mounted, graph::flag, script};

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

/// The crates compiled for build scripts and proc macros, not for the program, as the code
/// generator takes them. Worked out once per job from [`Graph`](super::graph::Graph)'s bitset.
fn build_time(job: &Job) -> BuildTime {
    let crates = job.build_time.get_or_init(|| {
        let mut crates = rustc_hash::FxHashSet::default();
        for at in 0..job.graph.len() as u32 {
            if job.graph.build_time(at)
                && let Some(krate) = job.crates.krate(at as usize)
            {
                crates.insert(krate);
            }
        }
        BuildTime(crates)
    });
    crates.clone()
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
    // BTreeMap: it is `Profile::packages`, which is serialized and hashed.
    let mut packages = BTreeMap::new();
    for dep in job.graph.closure(unit as u32).into_iter().skip(1) {
        if job.graph.has(dep, flag::STD) {
            continue;
        }
        let dep = job.unit(dep as usize).map_err(|it| it.what)?;
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

/// What a unit's codegen made, reading the task's snapshot.
pub(crate) struct Generated<'db> {
    pub(crate) built: Built,
    pub(crate) roots: Roots,
    pub(crate) frontend: HirFrontend<'db>,
    pub(crate) policy: Policy,
}

/// The code generator's view of the job's front end, its files named as rustc names them. Every
/// thread of the build reads the one snapshot of the task's reader: salsa's reads take `&self`,
/// so none waits on another and none is cloned.
fn frontend<'db>(job: &Job, reader: &'db UnitReader, mounted: &Arc<Mounted>) -> HirFrontend<'db> {
    let names = {
        let (mounted, paths) = (Arc::clone(mounted), Arc::clone(&reader.paths));
        move |file: base_db::FileId| -> Option<String> { paths.get(&file).map(|path| mounted.name(path)) }
    };
    HirFrontend::new(Databases::new(&reader.db), Box::new(ItemHashes::new(Box::new(names))), build_time(job))
}

/// The tests of test unit `unit` as its harness lists them, compiling nothing: the same
/// listing, names and attributes as the test binary's ([`generate`]).
pub(crate) fn test_roots(job: &Job, reader: &UnitReader, unit: usize) -> Result<Roots, String> {
    let u = job.unit(unit).map_err(|it| it.what)?;
    let krate = job
        .crates
        .krate(unit)
        .ok_or_else(|| format!("unit {unit} (`{}`) has no crate in the front end", u.target.name))?;
    frontend(job, reader, &job.mounted).roots(krate, RootKind::Test).map_err(|it| it.what)
}

/// The store as a build reads it: with no lock, under the build's guard.
struct Reading<'a> {
    store: &'a blaze_cas::Reader,
    guard: &'a blaze_cas::Guard,
}

impl blaze_codegen::driver::Lookup for Reading<'_> {
    fn get(&self, key: &blaze_cas::Key) -> Result<Option<Vec<u8>>, String> {
        match self.store.get(self.guard, key) {
            Ok(entry) => Ok(entry.map(|it| it.bytes.to_vec())),
            Err(corrupt) => Err(corrupt.to_string()),
        }
    }
}

/// `unit`'s functions compiled from its roots: `Err` with the code generator's errors, each
/// naming the function it could not compile.
pub(crate) fn generate<'db>(
    job: &Job,
    reader: &'db UnitReader,
    unit: usize,
    makes: &Makes,
    mounted: &Arc<Mounted>,
) -> Result<Result<Generated<'db>, Vec<String>>, String> {
    let u = job.unit(unit).map_err(|it| it.what)?;
    let krate = job
        .crates
        .krate(unit)
        .ok_or_else(|| format!("unit {unit} (`{}`) has no crate in the front end", u.target.name))?;
    let toolchain = toolchain(job, unit)?;
    let policy = policy(job, unit, toolchain)?;
    let frontend = frontend(job, reader, mounted);
    let roots = match frontend.roots(krate, root_kind(u, makes)) {
        Ok(it) => it,
        Err(error) => return Ok(Err(vec![error.what])),
    };
    let origin = origin(job, u)?;
    let asked: Vec<(HirInstance, Origin)> = roots.roots.iter().map(|it| (it.clone(), origin.clone())).collect();
    let backends = Backends::default();
    let built = {
        let guard = blaze_cas::enter();
        let store = Reading { store: &job.input.store, guard: &guard };
        Build::new(&backends, &frontend, &policy, &job.shared.memo).run(&asked, &store)
    };
    match built {
        Ok(built) => Ok(Ok(Generated { built, roots, frontend, policy })),
        Err(errors) => Ok(Err(errors.into_iter().map(|it| it.what).collect())),
    }
}

/// An image kept linked at one path, and what is in it, so the next link of it is a change.
/// Each column is sorted, so a link's diff against it is one merge pass: `O(n)` with no hashing
/// and no allocation but the diff itself (keys are content hashes, 32 bytes each).
pub(crate) struct Linked {
    image: Image,
    output: Output,
    functions: Vec<InstanceKey>,
    statics: Vec<(ItemKey, ContentId)>,
    natives: Vec<ContentId>,
    names: Vec<String>,
}

impl Linked {
    /// The image's tables as the linker counts them, each with its elements and bytes
    /// ([`blaze_api::Response::Tables`]): `relocation` and the rest.
    pub(crate) fn tables(&self) -> Vec<(&'static str, u64, u64)> {
        self.image.tables()
    }
}

/// What of sorted `old` is not in sorted `new`, and what of `new` is not in `old`, in one pass.
pub(crate) fn diff<T: Ord + Copy>(old: &[T], new: &[T], removed: &mut Vec<T>, added: &mut Vec<T>) {
    let (mut a, mut b) = (0, 0);
    while a < old.len() && b < new.len() {
        match old[a].cmp(&new[b]) {
            std::cmp::Ordering::Less => {
                removed.push(old[a]);
                a += 1;
            }
            std::cmp::Ordering::Greater => {
                added.push(new[b]);
                b += 1;
            }
            std::cmp::Ordering::Equal => {
                a += 1;
                b += 1;
            }
        }
    }
    removed.extend_from_slice(&old[a..]);
    added.extend_from_slice(&new[b..]);
}

/// The symbol names of a link, sorted, each kept `String` moved from the last link's rather
/// than copied: only a name new to the image is allocated, and each name gone is moved out.
pub(crate) fn renamed(old: Vec<String>, new: &mut Vec<&str>) -> (Vec<String>, Vec<String>) {
    new.sort_unstable();
    new.dedup();
    let mut kept = Vec::with_capacity(new.len());
    let mut gone = Vec::new();
    let mut old = old.into_iter().peekable();
    for name in new.iter() {
        while old.peek().is_some_and(|it| it.as_str() < *name) {
            gone.extend(old.next());
        }
        match old.peek().is_some_and(|it| it.as_str() == *name) {
            true => kept.extend(old.next()),
            false => kept.push((*name).to_owned()),
        }
    }
    gone.extend(old);
    (kept, gone)
}

/// What rustc is given for `unit` from the build scripts it is linked with
/// ([`blaze_build_script::apply::Compile`]), in cargo's order ([`Graph::links`]).
///
/// [`Graph::links`]: super::graph::Graph::links
fn given(job: &Job, unit: u32) -> Result<Compile, String> {
    let units = job.units();
    let u = job.unit(unit as usize).map_err(|it| it.what)?;
    let runs = job.graph.links(unit);
    let mut linked = Vec::with_capacity(runs.len());
    let mut own = Vec::new();
    for &run in runs {
        let applied = job.scripts[run as usize].get().ok_or_else(|| {
            format!(
                "the build script run of `{}` that `{}` is linked with has not run",
                units.graph.units[run as usize].pkg_id, u.target.name
            )
        })?;
        let mine = job.graph.package(run) == job.graph.package(unit);
        linked.push(LinkedTo { own: mine, output: &applied.output });
        if mine {
            own.push(&applied.output);
        }
    }
    // `Compile::of` asks of the package only whether it has a library.
    let lib = bs::Target { name: String::new(), kind: vec!["lib".to_owned()], crate_types: Vec::new() };
    let targets = match job.graph.package_has_lib(unit) {
        true => vec![lib],
        false => Vec::new(),
    };
    let package = bs::Package { targets, ..bs::Package::default() };
    Ok(Compile::of(&package, &script::target_of(u), job.graph.has(unit, flag::TEST), &linked, &own))
}

/// The native libraries `unit` links, in rustc's order: each crate linked in, what depends
/// first ([`Graph::linked_in`]), the `-l`s its rustc is given (its build script's
/// `rustc-link-lib`s) then its `#[link]`s, then the unit's own `rustc-link-arg`s; and where to
/// look for them, the unit's `-L`s.
///
/// [`Graph::linked_in`]: super::graph::Graph::linked_in
fn natives(job: &Job, unit: usize, frontend: &HirFrontend<'_>) -> Result<(Vec<NativeLib>, Vec<PathBuf>), String> {
    let unit = unit as u32;
    let mine = given(job, unit)?;
    let mut libs = Vec::new();
    for at in job.graph.linked_in(unit) {
        let theirs;
        let compile = match at == unit {
            true => &mine,
            false => {
                theirs = given(job, at)?;
                &theirs
            }
        };
        libs.reserve(compile.libs.len());
        for lib in &compile.libs {
            libs.push(link_lib(lib)?);
        }
        if let Some(krate) = job.crates.krate(at as usize) {
            libs.extend(frontend.linked(krate).map_err(|it| it.what)?);
        }
    }
    for arg in &mine.link_args {
        libs.push(link_arg(arg)?);
    }
    Ok((libs, mine.search))
}

/// A native input a link read off the machine: its path, and the BLAKE3 hash of its contents.
pub(crate) type Read = (String, ContentId);

/// `generated` linked into the image at `path` as `makes` says: the file's bytes, and every
/// native input the link read off the machine.
pub(crate) fn link(
    job: &Job,
    unit: usize,
    path: &str,
    makes: &Makes,
    generated: &Generated<'_>,
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
    let located = blaze_link::target::locate(&output, &libs, &search).map_err(|it| it.to_string())?;
    let side = generated.frontend.link_inputs(built).map_err(|it| it.what)?;

    let kept = job.shared.images.lock().unwrap_or_else(|it| it.into_inner()).remove(path);
    let mut linked = match kept.filter(|it| it.output == output) {
        Some(it) => it,
        None => {
            let to = blaze_core::FileName::new(path.trim_start_matches('/'))
                .ok_or_else(|| format!("`{path}` is no file the linker can name"))?;
            Linked {
                image: Image::new(to, output.clone()).map_err(|it| it.to_string())?,
                output: output.clone(),
                functions: Vec::new(),
                statics: Vec::new(),
                natives: Vec::new(),
                names: Vec::new(),
            }
        }
    };

    // Each native input by the hash of its contents, through the process's table of files by
    // their `stat`: one `stat` for a library the image already has, read only when it is new.
    let mut files = blaze_usercode::inputs::Disk::new(Vec::new());
    let mut read = Vec::with_capacity(located.len());
    let mut native_ids = Vec::with_capacity(located.len());
    for it in &located {
        let hash = blaze_usercode::trace::Inputs::file(&mut files, &it.path)
            .map_err(|error| format!("{}: {}", error.path.display(), error.error))?
            .ok_or_else(|| format!("{}: the native library is gone", it.path.display()))?;
        native_ids.push(ContentId(hash));
        read.push((it.path.to_string_lossy().into_owned(), ContentId(hash)));
    }
    let mut new_natives: Vec<ContentId> = native_ids.clone();
    new_natives.sort_unstable();
    new_natives.dedup();
    let mut removed_natives = Vec::new();
    let mut added_natives = Vec::new();
    diff(&linked.natives, &new_natives, &mut removed_natives, &mut added_natives);
    let mut bytes_of = Vec::with_capacity(added_natives.len());
    for (order, it) in located.iter().enumerate() {
        if added_natives.binary_search(&native_ids[order]).is_ok() {
            let bytes = std::fs::read(&it.path).map_err(|error| format!("{}: {error}", it.path.display()))?;
            bytes_of.push((order, bytes));
        }
    }

    let mut functions: Vec<InstanceKey> = built.functions.iter().map(|it| it.key).collect();
    functions.sort_unstable();
    let mut statics: Vec<(ItemKey, ContentId)> = built.statics.iter().map(|it| (it.item, it.instance)).collect();
    statics.sort_unstable();
    let mut removed = Vec::new();
    let mut added_keys = Vec::new();
    diff(&linked.functions, &functions, &mut removed, &mut added_keys);
    let mut patchable: Vec<InstanceKey> = built.patchable.iter().copied().collect();
    patchable.sort_unstable();
    let mut names: Vec<(String, Target)> = Vec::with_capacity(built.symbols.len() + built.static_symbols.len());
    for (key, name) in &built.symbols {
        let target = match patchable.binary_search(key).is_ok() {
            true => Target::Function(*key),
            false => Target::Direct(*key),
        };
        names.push((name.clone(), target));
    }
    for (item, instance, name) in &built.static_symbols {
        names.push((name.clone(), Target::Static { item: *item, instance: *instance }));
    }
    let mut now_named: Vec<&str> = names.iter().map(|it| it.0.as_str()).collect();
    let (kept_names, unnamed) = renamed(std::mem::take(&mut linked.names), &mut now_named);
    let change = Change {
        added: built.functions.iter().filter(|it| added_keys.binary_search(&it.key).is_ok()).collect(),
        removed,
        // A static's value is not in its key: every static is put again.
        statics: built.statics.iter().collect(),
        removed_statics: std::mem::take(&mut linked.statics),
        names,
        unnamed,
        debug_names: side.debug_names,
        natives: bytes_of
            .iter()
            .map(|(order, bytes)| Native {
                id: native_ids[*order],
                name: &located[*order].name,
                bytes,
                kind: located[*order].kind,
                order: *order as u32,
            })
            .collect(),
        removed_natives,
        spans: side.spans.iter().map(|(anchor, bytes)| (*anchor, bytes.as_slice())).collect(),
    };
    let staged = linked.image.stage(change).map_err(|it| it.to_string())?;
    let bytes = linked.image.bytes(staged).map_err(|it| it.to_string())?;
    linked.functions = functions;
    linked.statics = statics;
    linked.natives = new_natives;
    linked.names = kept_names;
    job.shared.images.lock().unwrap_or_else(|it| it.into_inner()).insert(path.to_owned(), linked);
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

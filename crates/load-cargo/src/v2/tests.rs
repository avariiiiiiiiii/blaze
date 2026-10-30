//! Units written here, over files in a `blaze_query::MemTree`: a workspace of a lib with a
//! build script, a proc macro and a bin depending on the lib by another name, on the standard
//! library's `core`, lowered and checked crate by crate.

#![forbid(unsafe_code)]

use std::path::PathBuf;

use base_db::{Crate, CrateType, SourceRoot, SourceRootId, TargetKind, VfsPath};
use blaze_ids::ContentId;
use blaze_query::MemTree;
use blaze_units::{
    query::{BuildConfig, Command, Invocation, Mount, Origin, PackagePlan, UnitPlan, Units},
    schema::{DebugInfo, Mode, Profile, Strip, Stripped, Target, Unit, UnitDep, UnitGraph},
};
use cfg::CfgAtom;
use intern::Symbol;
use span::Edition;
use triomphe::Arc;

use super::{Db, Lower, LowerError, Lowered, Ran, crates, files, lower};

const HOST: &str = "x86_64-unknown-linux-gnu";
const LIBRARY: &str = "/rustc/abc/library";
const OUT_DIR: &str = "/w/target/debug/build/a-1/out";

const CORE: usize = 0;
const BUILD_SCRIPT: usize = 1;
const RUN: usize = 2;
const LIB: usize = 3;
const MACRO: usize = 4;
const BIN: usize = 5;
const LIB_TEST: usize = 6;

const WORKSPACE: &str = "[workspace]\nmembers = [\"a\", \"b\", \"m\"]\n\n[workspace.metadata.blaze]\nconst-eval = { steps = 5000 }\n";

/// The view's files.
const FILES: &[(&str, &[u8])] = &[
    ("Cargo.toml", WORKSPACE.as_bytes()),
    ("a/Cargo.toml", b"[package]\nname = \"a\"\n"),
    ("a/build.rs", b"fn main() {}\n"),
    ("a/src/lib.rs", b"include!(concat!(env!(\"OUT_DIR\"), \"/gen.rs\"));\n"),
    ("a/src/more.rs", b"pub fn more() {}\n"),
    ("b/Cargo.toml", b"[package]\nname = \"b\"\n"),
    ("b/src/main.rs", b"fn main() { alpha::gen(); }\n"),
    ("m/Cargo.toml", b"[package]\nname = \"m\"\n"),
    ("m/src/lib.rs", b"extern crate proc_macro;\n"),
    ("target/debug/stale.rs", b"not a source\n"),
];

fn view(tree: &MemTree) -> ContentId {
    tree.root(FILES)
}

/// [`FILES`] with each of `changes` made: a path's new bytes, or none for a file gone.
fn changed(tree: &MemTree, changes: &[(&'static str, Option<&'static [u8]>)]) -> ContentId {
    let mut files: Vec<(&str, &[u8])> = FILES.to_vec();
    for &(path, bytes) in changes {
        files.retain(|it| it.0 != path);
        if let Some(bytes) = bytes {
            files.push((path, bytes));
        }
    }
    tree.root(&files)
}

fn library(tree: &MemTree) -> ContentId {
    tree.root(&[
        ("core/Cargo.toml", b"[package]\nname = \"core\"\n"),
        ("core/src/lib.rs", b"#[path = \"../../stdarch/crates/core_arch/src/mod.rs\"] mod core_arch;\n"),
        ("stdarch/crates/core_arch/src/mod.rs", b"pub mod arch {}\n"),
    ])
}

fn profile() -> Profile {
    Profile {
        name: "dev".to_owned(),
        opt_level: "0".to_owned(),
        lto: "false".to_owned(),
        codegen_backend: None,
        codegen_units: None,
        debuginfo: DebugInfo::Level(2),
        split_debuginfo: None,
        debug_assertions: true,
        overflow_checks: true,
        rpath: false,
        incremental: true,
        panic: "unwind".to_owned(),
        strip: Strip::Resolved(Stripped::None),
        rustflags: Vec::new(),
        trim_paths: None,
        hint_mostly_unused: None,
    }
}

fn target(kind: &str, name: &str, src_path: &str, edition: &str) -> Target {
    Target {
        kind: vec![kind.to_owned()],
        crate_types: vec![if kind == "custom-build" { "bin" } else { kind }.to_owned()],
        name: name.to_owned(),
        src_path: PathBuf::from(src_path),
        edition: edition.to_owned(),
        required_features: None,
        doc: true,
        doctest: false,
        test: true,
    }
}

fn dep(index: usize, name: &str, noprelude: bool) -> UnitDep {
    UnitDep {
        index,
        extern_crate_name: name.to_owned(),
        public: None,
        noprelude: noprelude.then_some(true),
        nounused: None,
    }
}

fn unit(pkg: &str, target: Target, mode: Mode, is_std: bool, dependencies: Vec<UnitDep>) -> Unit {
    Unit {
        pkg_id: pkg.to_owned(),
        target,
        profile: profile(),
        platform: None,
        mode,
        checked_as_test: false,
        features: Vec::new(),
        is_std,
        dependencies,
    }
}

fn plan(cfgs: &[&str], env: &[(&str, &str)], out_dir: Option<&str>) -> UnitPlan {
    UnitPlan {
        checked_as_test: false,
        metadata: String::new(),
        extra_filename: String::new(),
        cfgs: cfgs.iter().map(|it| (*it).to_owned()).collect(),
        check_cfg: Vec::new(),
        rustflags: Vec::new(),
        lints: Vec::new(),
        env: env.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect(),
        outputs: Vec::new(),
        out_dir: out_dir.map(str::to_owned),
        links: None,
    }
}

fn package(id: &str, name: &str, manifest_path: &str, source: Option<&str>, is_member: bool) -> PackagePlan {
    PackagePlan {
        id: id.to_owned(),
        name: name.to_owned(),
        version: "0.1.0".to_owned(),
        manifest_path: manifest_path.to_owned(),
        source: source.map(str::to_owned),
        is_member,
    }
}

fn units() -> Units {
    let core = "path+file:///rustc/abc/library/core#0.0.0";
    let a = "path+file:///w/a#0.1.0";
    let b = "path+file:///w/b#0.1.0";
    let m = "path+file:///w/m#0.1.0";
    let units = vec![
        unit(core, target("lib", "core", "/rustc/abc/library/core/src/lib.rs", "2024"), Mode::Build, true, vec![]),
        unit(a, target("custom-build", "build-script-build", "/w/a/build.rs", "2021"), Mode::Build, false, vec![]),
        unit(a, target("custom-build", "build-script-build", "/w/a/build.rs", "2021"), Mode::RunCustomBuild, false, vec![
            dep(BUILD_SCRIPT, "build_script_build", false),
        ]),
        unit(a, target("lib", "a", "/w/a/src/lib.rs", "2021"), Mode::Build, false, vec![
            dep(CORE, "core", true),
            dep(RUN, "build_script_build", false),
        ]),
        unit(m, target("proc-macro", "m", "/w/m/src/lib.rs", "2021"), Mode::Build, false, vec![dep(CORE, "core", true)]),
        unit(b, target("bin", "b", "/w/b/src/main.rs", "2024"), Mode::Build, false, vec![
            dep(LIB, "alpha", false),
            dep(MACRO, "m", false),
            dep(CORE, "core", true),
        ]),
        unit(a, target("lib", "a", "/w/a/src/lib.rs", "2021"), Mode::Test, false, vec![
            dep(CORE, "core", true),
            dep(RUN, "build_script_build", false),
        ]),
    ];
    let plans = vec![
        plan(&[], &[], None),
        plan(&[], &[], None),
        plan(&[], &[], Some(OUT_DIR)),
        plan(&["feature=\"fast\""], &[("CARGO_PKG_NAME", "a"), ("OUT_DIR", OUT_DIR)], None),
        plan(&[], &[("CARGO_PKG_NAME", "m")], None),
        plan(&[], &[("CARGO_PKG_NAME", "b")], None),
        plan(&["feature=\"fast\""], &[("CARGO_PKG_NAME", "a"), ("OUT_DIR", OUT_DIR)], None),
    ];
    let mut packages = vec![
        package(core, "core", "/rustc/abc/library/core/Cargo.toml", Some("(/rustc/abc/library/core)"), false),
        package(a, "a", "/w/a/Cargo.toml", Some("(/w/a)"), true),
        package(b, "b", "/w/b/Cargo.toml", Some("(/w/b)"), true),
        package(m, "m", "/w/m/Cargo.toml", Some("(/w/m)"), true),
    ];
    packages.sort_by(|x, y| x.id.cmp(&y.id));
    Units {
        graph: UnitGraph { version: 1, units, roots: vec![BIN, LIB_TEST] },
        plans,
        packages,
        lock: None,
        warnings: Vec::new(),
        target_dir: "/w/target".to_owned(),
        build_dir: "/w/target".to_owned(),
        finished: "`dev` profile [unoptimized + debuginfo]".to_owned(),
    }
}

fn config(view: ContentId, library: ContentId) -> BuildConfig {
    BuildConfig {
        invocation: Invocation {
            command: Command::Build,
            args: Vec::new(),
            root: "/w".to_owned(),
            cwd: "/w".to_owned(),
            env: Vec::new(),
            files: Vec::new(),
        },
        mounts: vec![
            Mount { at: LIBRARY.to_owned(), origin: Origin::Root(library), under: String::new() },
            Mount { at: "/w".to_owned(), origin: Origin::View, under: String::new() },
        ],
        host: HOST.to_owned(),
        library: LIBRARY.to_owned(),
    }
}

struct Built {
    lowered: Lowered,
}

/// `units` lowered over the view and library of [`view`] and [`library`].
fn lowered(units: &Units) -> Result<Built, LowerError> {
    let tree = MemTree::default();
    let root = view(&tree);
    let config = config(root, library(&tree));
    let lowered = lower(Lower { units, config: &config, files: &tree, root, host_cpu: None })?;
    Ok(Built { lowered })
}

fn krate(built: &Built, unit: usize) -> Crate {
    built.lowered.krate(unit).unwrap_or_else(|| panic!("unit {unit} has no crate"))
}

/// The source root `krate`'s root file is in: what the crate reads files from.
fn root_of(db: &Db, krate: Crate) -> Arc<SourceRoot> {
    let file = krate.data(db).root_file_id;
    let id = db.file_source_root(file).source_root_id(db);
    db.source_root(id).source_root(db)
}

fn root_id_of(db: &Db, krate: Crate) -> SourceRootId {
    db.file_source_root(krate.data(db).root_file_id).source_root_id(db)
}

fn path(text: &str) -> VfsPath {
    VfsPath::from(base_db::AbsPathBuf::try_from(text).unwrap())
}

fn flag(name: &str) -> CfgAtom {
    CfgAtom::Flag(Symbol::intern(name))
}

fn key_value(key: &str, value: &str) -> CfgAtom {
    CfgAtom::KeyValue { key: Symbol::intern(key), value: Symbol::intern(value) }
}

#[test]
fn every_unit_that_compiles_is_a_crate() {
    let built = lowered(&units()).unwrap();
    let db = built.lowered.db();
    assert!(built.lowered.krate(RUN).is_none());
    for unit in [CORE, BUILD_SCRIPT, LIB, MACRO, BIN, LIB_TEST] {
        assert!(built.lowered.krate(unit).is_some(), "unit {unit}");
    }
    assert_eq!(base_db::all_crates(&db).len(), 6);
    // What every task shares answers as the writer does.
    fn shared<T: Send + Sync>(_: &T) {}
    let crates = built.lowered.crates();
    shared(&*crates);
    for unit in [CORE, RUN, LIB, MACRO, BIN] {
        assert_eq!(crates.krate(unit), built.lowered.krate(unit));
    }
    assert!(crates.is_proc_macro(MACRO) && !crates.is_proc_macro(LIB));
    // The lib and its test are two crates of one root file.
    assert_ne!(krate(&built, LIB), krate(&built, LIB_TEST));
    assert_eq!(krate(&built, LIB).data(&db).root_file_id, krate(&built, LIB_TEST).data(&db).root_file_id);
    // A crate comes after every crate it depends on.
    let all = base_db::all_crates(&db);
    let at = |unit| all.iter().position(|it| *it == krate(&built, unit)).unwrap();
    assert!(at(CORE) < at(LIB) && at(LIB) < at(BIN) && at(MACRO) < at(BIN));
}

#[test]
fn deps_are_by_extern_name_in_the_units_order() {
    let built = lowered(&units()).unwrap();
    let db = built.lowered.db();
    let deps = &krate(&built, BIN).data(&db).dependencies;
    let names: Vec<&str> = deps.iter().map(|it| it.name.as_str()).collect();
    assert_eq!(names, ["alpha", "m", "core"]);
    assert_eq!(deps[0].crate_id, krate(&built, LIB));
    assert_eq!(deps[1].crate_id, krate(&built, MACRO));
    assert_eq!(deps[2].crate_id, krate(&built, CORE));
    assert!(deps[0].is_prelude() && !deps[0].is_sysroot());
    assert!(!deps[2].is_prelude() && deps[2].is_sysroot());
    // A build script's run is no crate's dependency.
    let lib_deps: Vec<&str> = krate(&built, LIB).data(&db).dependencies.iter().map(|it| it.name.as_str()).collect();
    assert_eq!(lib_deps, ["core"]);
}

#[test]
fn editions_kinds_and_origins_are_cargos() {
    let built = lowered(&units()).unwrap();
    let db = built.lowered.db();
    let data = |unit| krate(&built, unit).data(&db).clone();
    assert_eq!(data(LIB).edition, Edition::Edition2021);
    assert_eq!(data(BIN).edition, Edition::Edition2024);
    assert_eq!(data(LIB).target_kind, TargetKind::Lib { is_proc_macro: false });
    assert_eq!(data(LIB_TEST).target_kind, TargetKind::Test);
    assert_eq!(data(BIN).target_kind, TargetKind::Bin);
    assert_eq!(data(BUILD_SCRIPT).target_kind, TargetKind::BuildScript);
    assert!(data(MACRO).target_kind.is_proc_macro());
    let types = |unit| data(unit).crate_types.iter().collect::<Vec<_>>();
    assert_eq!(types(LIB), [CrateType::Lib]);
    assert_eq!(types(MACRO), [CrateType::ProcMacro]);
    assert_eq!(types(BIN), [CrateType::Bin]);
    assert_eq!(types(BUILD_SCRIPT), [CrateType::Bin]);
    // rustc builds a `--test` crate as an executable, whatever its target's types.
    assert_eq!(types(LIB_TEST), [CrateType::Bin]);
    assert!(data(CORE).origin.is_lang());
    assert!(data(LIB).origin.is_local());
    // A member's proc macros run in the workspace's root.
    assert_eq!(data(MACRO).proc_macro_cwd.as_str(), "/w");
    let extra = krate(&built, BIN).extra_data(&db);
    assert_eq!(extra.version.as_deref(), Some("0.1.0"));
    assert_eq!(extra.display_name.as_ref().map(|it| it.canonical_name().as_str().to_owned()).as_deref(), Some("b"));
    assert_eq!(krate(&built, LIB).workspace_data(&db).const_eval.steps, 5000);
}

#[test]
fn a_crate_type_rustc_does_not_name_is_an_error_naming_the_unit() {
    let mut units = units();
    units.graph.units[LIB].target.crate_types = vec!["rlib".to_owned(), "shared".to_owned()];
    let Err(error) = lowered(&units) else { panic!("a crate type no rustc names was lowered") };
    assert!(error.what.contains("the crate type `shared`"), "{}", error.what);
    assert!(error.what.contains("`a` (lib"), "{}", error.what);
}

#[test]
fn cfgs_are_the_targets_with_the_units() {
    let built = lowered(&units()).unwrap();
    let db = built.lowered.db();
    let lib = krate(&built, LIB).cfg_options(&db).clone();
    assert!(lib.check_atom(&key_value("feature", "fast")));
    assert!(lib.check_atom(&key_value("target_os", "linux")));
    assert!(lib.check_atom(&flag("debug_assertions")));
    assert!(!lib.check_atom(&flag("test")));
    assert!(!lib.check_atom(&flag("proc_macro")));
    let test = krate(&built, LIB_TEST).cfg_options(&db).clone();
    assert!(test.check_atom(&flag("test")));
    assert!(krate(&built, MACRO).cfg_options(&db).check_atom(&flag("proc_macro")));
}

#[test]
fn env_is_the_units() {
    let built = lowered(&units()).unwrap();
    let db = built.lowered.db();
    assert_eq!(krate(&built, LIB).env(&db).get("CARGO_PKG_NAME").as_deref(), Some("a"));
    assert_eq!(krate(&built, BIN).env(&db).get("CARGO_PKG_NAME").as_deref(), Some("b"));
    assert_eq!(krate(&built, BIN).env(&db).get("OUT_DIR"), None);
}

#[test]
fn files_are_at_cargos_paths_in_their_roots() {
    let built = lowered(&units()).unwrap();
    let db = built.lowered.db();
    let paths = built.lowered.paths();
    let lib_root = krate(&built, LIB).data(&db).root_file_id;
    assert_eq!(paths.get(&lib_root).map(String::as_str), Some("/w/a/src/lib.rs"));
    assert!(db.file_text(lib_root).text(&db).starts_with("include!"));
    // Each package's root holds its files and no other's.
    let a = root_of(&db, krate(&built, LIB));
    assert!(a.file_for_path(&path("/w/a/src/more.rs")).is_some());
    assert!(a.file_for_path(&path("/w/b/src/main.rs")).is_none());
    assert!(!a.is_library);
    // What the build writes is read by nobody.
    assert!(!paths.values().any(|it| it.starts_with("/w/target/")));
    // The standard library is one library root: core reads stdarch by `#[path]`.
    let core = root_of(&db, krate(&built, CORE));
    assert!(core.is_library);
    assert!(core.file_for_path(&path("/rustc/abc/library/stdarch/crates/core_arch/src/mod.rs")).is_some());
    assert_ne!(root_id_of(&db, krate(&built, CORE)), root_id_of(&db, krate(&built, LIB)));
}

#[test]
fn a_path_under_no_mount_is_refused_naming_it() {
    let mut units = units();
    let elsewhere = "path+file:///elsewhere/e#0.1.0";
    units.graph.units.push(unit(elsewhere, target("lib", "e", "/elsewhere/e/src/lib.rs", "2021"), Mode::Build, false, vec![]));
    units.plans.push(plan(&[], &[], None));
    units.packages.push(package(elsewhere, "e", "/elsewhere/e/Cargo.toml", Some("(/elsewhere/e)"), false));
    units.packages.sort_by(|x, y| x.id.cmp(&y.id));
    let Err(error) = lowered(&units) else { panic!("a package under no mount was lowered") };
    assert_eq!(error.at.as_deref(), Some("/elsewhere/e"));
    assert!(error.what.contains("under no mount"), "{}", error.what);
}

#[test]
fn a_run_reaches_the_units_depending_on_it_and_no_other() {
    let mut built = lowered(&units()).unwrap();
    let ran = Ran {
        cfgs: vec!["has_gen".to_owned(), "gen=\"two\"".to_owned()],
        env: vec![("GEN".to_owned(), "1".to_owned())],
        out_dir: OUT_DIR.to_owned(),
        files: vec![("gen.rs".to_owned(), b"pub fn gen() {}\n".to_vec())],
    };
    built.lowered.ran(RUN, &ran).unwrap();
    let db = built.lowered.db();
    for unit in [LIB, LIB_TEST] {
        let krate = krate(&built, unit);
        assert!(krate.cfg_options(&db).check_atom(&flag("has_gen")), "unit {unit}");
        assert!(krate.cfg_options(&db).check_atom(&key_value("gen", "two")), "unit {unit}");
        assert_eq!(krate.env(&db).get("GEN").as_deref(), Some("1"));
        assert_eq!(krate.env(&db).get("OUT_DIR").as_deref(), Some(OUT_DIR));
    }
    let bin = krate(&built, BIN);
    assert!(!bin.cfg_options(&db).check_atom(&flag("has_gen")));
    assert_eq!(bin.env(&db).get("GEN"), None);

    let generated = path(&format!("{OUT_DIR}/gen.rs"));
    let file = root_of(&db, krate(&built, LIB)).file_for_path(&generated).copied().unwrap();
    assert_eq!(&**db.file_text(file).text(&db), "pub fn gen() {}\n");
    assert!(root_of(&db, bin).file_for_path(&generated).is_none());
    assert!(root_of(&db, krate(&built, MACRO)).file_for_path(&generated).is_none());
    assert_eq!(built.lowered.paths().get(&file).map(String::as_str), Some(&*format!("{OUT_DIR}/gen.rs")));

    // Applied again without the file and the cfg, the run leaves neither.
    let again = Ran { cfgs: Vec::new(), files: Vec::new(), ..ran };
    built.lowered.ran(RUN, &again).unwrap();
    let db = built.lowered.db();
    assert!(!krate(&built, LIB).cfg_options(&db).check_atom(&flag("has_gen")));
    assert!(root_of(&db, krate(&built, LIB)).file_for_path(&generated).is_none());
    assert!(!built.lowered.paths().contains_key(&file));
}

#[test]
fn a_run_is_refused_for_what_is_no_run() {
    let mut built = lowered(&units()).unwrap();
    let ran = Ran { out_dir: OUT_DIR.to_owned(), ..Ran::default() };
    assert!(built.lowered.ran(LIB, &ran).is_err());
    let elsewhere = Ran { out_dir: "/w/target/other".to_owned(), ..Ran::default() };
    assert!(built.lowered.ran(RUN, &elsewhere).is_err());
    assert!(built.lowered.macros(LIB, Ok(Vec::new())).is_err());
    assert!(built.lowered.macros(MACRO, Ok(Vec::new())).is_ok());
}

#[test]
fn lowering_twice_gives_equal_crates() {
    let units = units();
    let first = lowered(&units).unwrap();
    let second = lowered(&units).unwrap();
    let (one, two) = (first.lowered.db(), second.lowered.db());
    for unit in [CORE, BUILD_SCRIPT, LIB, MACRO, BIN, LIB_TEST] {
        assert_eq!(krate(&first, unit).content(&one), krate(&second, unit).content(&two), "unit {unit}");
    }
    assert_eq!(base_db::all_crates_content(&one), base_db::all_crates_content(&two));
    // The lib and its test differ by `cfg(test)`.
    assert_ne!(krate(&first, LIB).content(&one), krate(&first, LIB_TEST).content(&one));
}

#[test]
fn cfgs_parse_as_rustc_takes_them() {
    assert_eq!(crates::atom("foo").unwrap(), flag("foo"));
    assert_eq!(crates::atom("feature=\"a-b\"").unwrap(), key_value("feature", "a-b"));
    assert_eq!(crates::atom(r#"k="a\"b""#).unwrap(), key_value("k", "a\"b"));
    assert!(crates::atom("k=unquoted").is_err());
    assert!(crates::atom("1x").is_err());
}

/// Ambiguous for a type that is `Clone`, so naming its item there does not compile.
trait AmbiguousIfClone<A> {
    fn not_clone() {}
}
impl<T: ?Sized> AmbiguousIfClone<()> for T {}
#[allow(dead_code)]
struct IsClone;
impl<T: ?Sized + Clone> AmbiguousIfClone<IsClone> for T {}

/// The database is one value shared by reference: it is `Sync` and `Send`, and has no `Clone`
/// to make a handle with.
const _: fn() = || {
    fn shared<T: Send + Sync>() {}
    shared::<Db>();
    shared::<Lowered>();
    <Db as AmbiguousIfClone<_>>::not_clone();
};

#[test]
fn many_threads_read_the_one_database_by_reference() {
    let mut built = lowered(&units()).unwrap();
    let lowered = &built.lowered;
    let alone: Vec<(usize, bool, Option<String>)> = [LIB, LIB_TEST, BIN, MACRO]
        .into_iter()
        .map(|unit| {
            let krate = lowered.krate(unit).unwrap();
            (unit, krate.cfg_options(lowered.db()).check_atom(&flag("test")), krate.env(lowered.db()).get("CARGO_PKG_NAME"))
        })
        .collect();
    let read: Vec<Vec<(usize, bool, Option<String>)>> = std::thread::scope(|scope| {
        let threads: Vec<_> = (0..8)
            .map(|at| {
                scope.spawn(move || {
                    let db: &Db = lowered.db();
                    let mut units = [LIB, LIB_TEST, BIN, MACRO];
                    units.rotate_left(at % 4);
                    let mut seen: Vec<_> = units
                        .into_iter()
                        .map(|unit| {
                            let krate = lowered.crates().krate(unit).unwrap();
                            (unit, krate.cfg_options(db).check_atom(&flag("test")), krate.env(db).get("CARGO_PKG_NAME"))
                        })
                        .collect();
                    seen.sort();
                    seen
                })
            })
            .collect();
        threads.into_iter().map(|it| it.join().unwrap()).collect()
    });
    let mut alone = alone;
    alone.sort();
    assert!(read.iter().all(|it| *it == alone), "{read:?} against {alone:?}");
    // Once no thread holds it, the one writer writes it.
    let ran = Ran { cfgs: vec!["late".to_owned()], out_dir: OUT_DIR.to_owned(), ..Ran::default() };
    built.lowered.ran(RUN, &ran).unwrap();
    assert!(krate(&built, LIB).cfg_options(built.lowered.db()).check_atom(&flag("late")));
}

#[test]
fn a_run_is_checked_as_it_would_be_applied() {
    let built = lowered(&units()).unwrap();
    let good = Ran { cfgs: vec!["x".to_owned()], out_dir: OUT_DIR.to_owned(), ..Ran::default() };
    assert!(built.lowered.check_ran(RUN, &good).is_ok());
    assert!(built.lowered.check_ran(LIB, &good).is_err());
    let elsewhere = Ran { out_dir: "/w/target/other".to_owned(), ..Ran::default() };
    assert!(built.lowered.check_ran(RUN, &elsewhere).is_err());
    let bad = Ran { cfgs: vec!["k=unquoted".to_owned()], out_dir: OUT_DIR.to_owned(), ..Ran::default() };
    let error = built.lowered.check_ran(RUN, &bad).unwrap_err();
    assert_eq!(error.at.as_deref(), Some(&*built.lowered.crates().units[RUN].what));
    assert!(built.lowered.check_macros(MACRO).is_ok() && built.lowered.check_macros(LIB).is_err());
}

#[test]
fn a_units_identity_covers_what_it_is_made_from_deeply() {
    let units = units();
    let mut first = lowered(&units).unwrap();
    let mut second = lowered(&units).unwrap();
    let all = [CORE, BUILD_SCRIPT, LIB, MACRO, BIN, LIB_TEST];
    let before: Vec<_> = all.iter().map(|&unit| first.lowered.identity(unit)).collect();
    for (at, &unit) in all.iter().enumerate() {
        assert!(before[at].is_some(), "unit {unit}");
        assert_eq!(before[at], second.lowered.identity(unit), "unit {unit}: lowered twice");
    }
    assert_eq!(first.lowered.identity(RUN), None);
    assert_ne!(first.lowered.identity(LIB), first.lowered.identity(LIB_TEST));

    // A file the run leaves, and nothing else: the lib's own cfgs and env are as before, so
    // only the file's content tells its identity apart, and the bin's through the lib's.
    let ran = |text: &str| Ran {
        out_dir: OUT_DIR.to_owned(),
        files: vec![("gen.rs".to_owned(), text.as_bytes().to_vec())],
        ..Ran::default()
    };
    first.lowered.ran(RUN, &ran("pub fn gen() {}\n")).unwrap();
    let after: Vec<_> = all.iter().map(|&unit| first.lowered.identity(unit)).collect();
    let changed = |unit| {
        let at = all.iter().position(|it| *it == unit).unwrap();
        before[at] != after[at]
    };
    assert!(changed(LIB) && changed(LIB_TEST) && changed(BIN));
    assert!(!changed(CORE) && !changed(MACRO));
    first.lowered.ran(RUN, &ran("pub fn gen() { loop {} }\n")).unwrap();
    assert_ne!(first.lowered.identity(LIB), after[2]);
    first.lowered.ran(RUN, &ran("pub fn gen() {}\n")).unwrap();
    assert_eq!(first.lowered.identity(LIB), after[2], "the same inputs, the same identity");
}

#[test]
fn a_refresh_reads_the_view_again_and_no_fixed_mount() {
    let units = units();
    let mut built = lowered(&units).unwrap();
    let core = built.lowered.identity(CORE);
    let lib = built.lowered.identity(LIB);
    let bin_root = krate(&built, BIN).data(built.lowered.db()).root_file_id;
    // The library's root is not in this tree: a refresh that read it would fail.
    let tree = MemTree::default();
    let (more, new): (&'static [u8], &'static [u8]) = (b"pub fn more() -> u8 { 1 }\n", b"pub fn new() {}\n");
    let root = changed(&tree, &[("a/src/more.rs", Some(more)), ("a/src/new.rs", Some(new))]);
    let config = config(root, library(&MemTree::default()));
    let refreshed = built.lowered.refresh(Lower { units: &units, config: &config, files: &tree, root, host_cpu: None });
    assert_eq!(refreshed, Ok(true));
    let db = built.lowered.db();
    let a = root_of(db, krate(&built, LIB));
    let more = a.file_for_path(&path("/w/a/src/more.rs")).copied().unwrap();
    assert!(db.file_text(more).text(&db).starts_with("pub fn more() -> u8"));
    let new = a.file_for_path(&path("/w/a/src/new.rs")).copied().unwrap();
    assert_eq!(built.lowered.paths().get(&new).map(String::as_str), Some("/w/a/src/new.rs"));
    assert_eq!(krate(&built, BIN).data(db).root_file_id, bin_root, "a file that did not change keeps its id");
    assert_eq!(built.lowered.identity(CORE), core, "nothing of the library changed");
    assert_ne!(built.lowered.identity(LIB), lib);
}

#[test]
fn a_refresh_refuses_a_root_file_gone_and_leaves_a_new_workspace_root_to_lower() {
    let units = units();
    let tree = MemTree::default();
    let library = library(&tree);
    let refresh = |changes: &[(&'static str, Option<&'static [u8]>)]| {
        let mut built = lowered(&units).unwrap();
        let root = changed(&tree, changes);
        let config = config(root, library);
        built.lowered.refresh(Lower { units: &units, config: &config, files: &tree, root, host_cpu: None })
    };
    let Err(error) = refresh(&[("b/src/main.rs", None)]) else { panic!("a crate's root file went and nothing said so") };
    assert!(error.what.contains("root file"), "{error}");
    let manifest: &'static [u8] = b"[workspace]\nmembers = [\"a\", \"b\", \"m\"]\n";
    assert_eq!(refresh(&[("Cargo.toml", Some(manifest))]), Ok(false), "the workspace's const-eval is every crate's");
}

#[test]
fn a_walk_leaves_out_the_directories_skipped_and_only_those() {
    let tree = MemTree::default();
    let view = view(&tree);
    let mounts = [Mount { at: "/w".to_owned(), origin: Origin::View, under: String::new() }];
    let mounts = files::Mounts { mounts: &mounts, view, tree: &tree, read_view: std::cell::Cell::new(false) };
    let walked = |skipped: &[String]| {
        let mut out = Vec::new();
        mounts.walk("/w", skipped, &mut out).unwrap();
        out.into_iter().map(|it| it.path).collect::<Vec<_>>()
    };
    let every = walked(&[]);
    assert_eq!(every.len(), FILES.len());
    assert!(every.iter().any(|it| it == "/w/target/debug/stale.rs"));
    assert!(mounts.read_view.get(), "the files were the view's");
    let skipped = walked(&["/w/target".to_owned(), "/w/a/src/lib.rs".to_owned()]);
    assert!(!skipped.iter().any(|it| it.starts_with("/w/target/") || it == "/w/a/src/lib.rs"), "{skipped:?}");
    assert_eq!(skipped.len(), FILES.len() - 2);
    assert_eq!(walked(&["/w/targe".to_owned()]), every, "a skipped path is a whole name, not a prefix");
}

#[test]
fn a_cycle_of_units_is_refused() {
    let mut units = units();
    units.graph.units[CORE].dependencies.push(dep(BIN, "b", false));
    let Err(error) = lowered(&units) else { panic!("a cycle was lowered") };
    assert!(error.what.contains("cycle"), "{}", error.what);
}

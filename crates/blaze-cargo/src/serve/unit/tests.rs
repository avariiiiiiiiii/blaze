use std::{ffi::OsString, path::PathBuf, sync::OnceLock};

use blaze_api::{Artifact, Level, UnitProfile};
use blaze_ids::ContentId;
use blaze_units::{
    query::{PackagePlan, UnitPlan, Units},
    schema::{DebugInfo, Mode, Profile, Strip, Stripped, Target, Unit, UnitDep, UnitGraph},
};
use blaze_usercode::run::Value;

use super::{artifact, codegen, compile, run, script, unit_target};

const HOST: &str = "x86_64-unknown-linux-gnu";

fn target(name: &str, kind: &str, src: &str) -> Target {
    let crate_types = match kind {
        "lib" => vec!["lib".to_owned()],
        _ => vec!["bin".to_owned()],
    };
    Target {
        kind: vec![kind.to_owned()],
        crate_types,
        name: name.to_owned(),
        src_path: PathBuf::from(src),
        edition: "2024".to_owned(),
        required_features: None,
        doc: kind == "lib",
        doctest: kind == "lib",
        test: true,
    }
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
        strip: Strip::Deferred(Stripped::None),
        rustflags: Vec::new(),
        trim_paths: None,
        hint_mostly_unused: None,
    }
}

fn unit(pkg: &str, target: Target, mode: Mode, deps: &[usize]) -> Unit {
    Unit {
        pkg_id: pkg.to_owned(),
        target,
        profile: profile(),
        platform: None,
        mode,
        checked_as_test: false,
        features: vec!["default".to_owned()],
        is_std: false,
        dependencies: deps
            .iter()
            .map(|it| UnitDep { index: *it, extern_crate_name: "dep".to_owned(), public: None, noprelude: None, nounused: None })
            .collect(),
    }
}

fn plan(extra: &str, outputs: &[&str]) -> UnitPlan {
    UnitPlan {
        checked_as_test: false,
        metadata: extra.trim_start_matches('-').to_owned(),
        extra_filename: extra.to_owned(),
        cfgs: Vec::new(),
        check_cfg: Vec::new(),
        rustflags: Vec::new(),
        lints: Vec::new(),
        env: Vec::new(),
        outputs: outputs.iter().map(|it| (*it).to_owned()).collect(),
        out_dir: None,
        links: None,
        unsandboxed: false,
        runner: Vec::new(),
        harness: true,
    }
}

fn package(id: &str, name: &str, dir: &str) -> PackagePlan {
    PackagePlan {
        id: id.to_owned(),
        name: name.to_owned(),
        version: "0.1.0".to_owned(),
        manifest_path: format!("{dir}/Cargo.toml"),
        source: Some(format!("({dir})")),
        is_member: true,
        default_run: None,
    }
}

const P: &str = "path+file:///w/p#0.1.0";
const Z: &str = "path+file:///w/z-sys#0.1.0";

/// `z-sys` (links `z`) with a build script, and `p`, whose script reads `z`'s metadata.
fn build(out: &std::path::Path) -> Units {
    let z_out = out.join("z-sys-0001/out").to_string_lossy().into_owned();
    let p_out = out.join("p-0002/out").to_string_lossy().into_owned();
    let units = vec![
        // 0: z-sys's script compiled, 1: run.
        unit(Z, target("build-script-build", "custom-build", "/w/z-sys/build.rs"), Mode::Build, &[]),
        unit(Z, target("build-script-build", "custom-build", "/w/z-sys/build.rs"), Mode::RunCustomBuild, &[0]),
        // 2: p's script compiled, 3: run, after z-sys's run.
        unit(P, target("build-script-build", "custom-build", "/w/p/build.rs"), Mode::Build, &[]),
        unit(P, target("build-script-build", "custom-build", "/w/p/build.rs"), Mode::RunCustomBuild, &[2, 1]),
    ];
    let script_plan = |out_dir: &str, links: Option<&str>, name: &str| UnitPlan {
        out_dir: Some(out_dir.to_owned()),
        links: links.map(str::to_owned),
        env: vec![("CARGO_PKG_NAME".to_owned(), name.to_owned()), ("PROFILE".to_owned(), "debug".to_owned())],
        ..plan("", &[])
    };
    Units {
        graph: UnitGraph { version: 1, units, roots: vec![3] },
        plans: vec![
            plan("-0001", &["/w/target/debug/build/z-sys-0001/build_script_build-0001"]),
            script_plan(&z_out, Some("z"), "z-sys"),
            plan("-0002", &["/w/target/debug/build/p-0002/build_script_build-0002"]),
            script_plan(&p_out, None, "p"),
        ],
        packages: vec![package(P, "p", "/w/p"), package(Z, "z-sys", "/w/z-sys")],
        lock: None,
        warnings: Vec::new(),
        target_dir: "/w/target".to_owned(),
        build_dir: "/w/target".to_owned(),
        finished: String::new(),
        link_dirs: Vec::new(),
    }
}

fn slots(units: &Units) -> Vec<OnceLock<script::Applied>> {
    units.graph.units.iter().map(|_| OnceLock::new()).collect()
}

fn value(code: i32, stdout: &str, stderr: &str) -> Value {
    Value { code: Some(code), signal: None, stdout: stdout.as_bytes().to_vec(), stderr: stderr.as_bytes().to_vec(), files: Vec::new() }
}

fn of<'a>(units: &'a Units, scripts: &'a [OnceLock<script::Applied>], tools: &'a std::path::Path) -> script::Of<'a> {
    script::Of {
        units,
        host: HOST,
        tools,
        launcher: std::path::Path::new("/opt/blaze/bin/blaze"),
        program: PathBuf::from("/w/target/debug/build/x/build-script-build"),
        scripts,
    }
}

#[test]
fn a_scripts_directives_are_applied_as_cargo_applies_them() {
    let dir = tempfile::tempdir().unwrap();
    let units = build(dir.path());
    let tools = PathBuf::from("/state/tools");
    let scripts = slots(&units);
    let z = script::script_unit(&of(&units, &scripts, &tools), 1).unwrap();
    assert_eq!(z.package.descr(), "z-sys v0.1.0 (/w/z-sys)");
    assert_eq!(z.package.links.as_deref(), Some("z"));
    let out_dir = z.out_dir.to_string_lossy().into_owned();
    let printed = format!(
        "cargo:rustc-cfg=has_z\ncargo::rustc-env=Z_VERSION=1.3\ncargo:rustc-link-lib=static=z\n\
         cargo:rustc-link-search=native={out_dir}/lib\ncargo:warning=z is old\ncargo:include={out_dir}/include\n"
    );
    let env = script::inherited(&[], &units.plans[1]);
    let began = blaze_build_script::run::elided(&z, Vec::new()).unwrap().began;
    let judged = script::judge(&z, &env, &value(0, &printed, ""), began, Some(&[]), None, false);
    let output = judged.output.expect("the run succeeded");
    assert_eq!(output.cfgs, ["has_z"]);
    assert_eq!(output.env, [("Z_VERSION".to_owned(), "1.3".to_owned())]);
    assert_eq!(output.library_links, ["static=z"]);
    assert_eq!(output.library_paths.len(), 1);
    assert_eq!(output.library_paths[0].path(), std::path::Path::new(&format!("native={out_dir}/lib")));
    assert_eq!(output.metadata, [("include".to_owned(), format!("{out_dir}/include"))]);
    // cargo's warning line, of the package by name and version.
    assert_eq!(judged.diagnostics.len(), 1);
    assert_eq!(judged.diagnostics[0].level, Level::Warning);
    assert_eq!(judged.diagnostics[0].rendered, "warning: z-sys@0.1.0: z is old\n");
    let executed = script::executed(Z, &output, &out_dir);
    assert_eq!(executed.linked_libs, ["static=z"]);
    assert_eq!(executed.cfgs, ["has_z"]);
    assert_eq!(executed.env, [("Z_VERSION".to_owned(), "1.3".to_owned())]);
    assert_eq!(codegen::link_lib(&output.library_links[0]).unwrap().name, "z");

    // The dependent's script is told `DEP_Z_INCLUDE`.
    let applied = script::Applied { package_id: Z.to_owned(), links: Some("z".to_owned()), output, out_dir: out_dir.clone() };
    assert!(scripts[1].set(applied).is_ok());
    let p = script::script_unit(&of(&units, &scripts, &tools), 3).unwrap();
    assert_eq!(p.dependencies.len(), 1);
    let env = blaze_build_script::env(&p, &[]).unwrap();
    assert_eq!(env.vars.get(&OsString::from("DEP_Z_INCLUDE")), Some(&OsString::from(format!("{out_dir}/include"))));
    assert_eq!(p.tools.rustc, tools.join(format!("rustc{}", std::env::consts::EXE_SUFFIX)));
}

#[test]
fn a_directive_the_sandbox_refuses_fails_the_build() {
    let dir = tempfile::tempdir().unwrap();
    let units = build(dir.path());
    let scripts = slots(&units);
    let tools = PathBuf::from("/state/tools");
    let z = script::script_unit(&of(&units, &scripts, &tools), 1).unwrap();
    let began = blaze_build_script::run::elided(&z, Vec::new()).unwrap().began;
    let printed = "cargo:rustc-link-search=native=/home/me/lib\n";
    let judged = script::judge(&z, &[], &value(0, printed, ""), began, Some(&[]), None, false);
    assert!(judged.output.is_none());
    assert!(judged.diagnostics[0].message.starts_with("the build script of `z-sys` printed a directive"));
    // Outside the sandbox nothing is checked.
    let judged = script::judge(&z, &[], &value(0, printed, ""), began, None, None, false);
    assert!(judged.output.is_some());
}

#[test]
fn a_failed_script_says_what_cargo_says() {
    let dir = tempfile::tempdir().unwrap();
    let units = build(dir.path());
    let scripts = slots(&units);
    let tools = PathBuf::from("/state/tools");
    let z = script::script_unit(&of(&units, &scripts, &tools), 1).unwrap();
    let began = blaze_build_script::run::elided(&z, Vec::new()).unwrap().began;
    let judged = script::judge(&z, &[], &value(101, "cargo:warning=no zlib\n", "thread 'main' panicked\n"), began, Some(&[]), None, false);
    assert!(judged.output.is_none());
    let [warning, error] = judged.diagnostics.as_slice() else { panic!("{:?}", judged.diagnostics) };
    assert_eq!(warning.rendered, "warning: z-sys@0.1.0: no zlib\n");
    assert_eq!(error.level, Level::Error);
    assert_eq!(
        error.message,
        "failed to run custom build command for `z-sys v0.1.0 (/w/z-sys)`\n\nCaused by:\n  process didn't exit \
         successfully: `/w/target/debug/build/x/build-script-build` (exit status: 101)\n  --- stdout\n  \
         cargo:warning=no zlib\n  \n  --- stderr\n  thread 'main' panicked"
    );
    assert!(error.rendered.starts_with("error: failed to run custom build command for `z-sys v0.1.0 (/w/z-sys)`\n"));

    // An error it logged fails it too, under cargo's cause.
    let judged = script::judge(&z, &[], &value(0, "cargo::error=no zlib\n", ""), began, Some(&[]), None, true);
    let [logged, error] = judged.diagnostics.as_slice() else { panic!("{:?}", judged.diagnostics) };
    assert_eq!(logged.message, "z-sys@0.1.0: no zlib");
    assert_eq!(logged.rendered, "\x1b[1m\x1b[91merror\x1b[0m\x1b[1m:\x1b[0m z-sys@0.1.0: no zlib\n");
    assert_eq!(
        error.message,
        "failed to run custom build command for `z-sys v0.1.0 (/w/z-sys)`\n\nCaused by:\n  build script logged errors"
    );
}

#[test]
fn a_script_sees_the_jobs_environment_with_cargos_over_it_and_none_of_it_is_kept() {
    let job = vec![
        ("PATH".to_owned(), "/usr/bin".to_owned()),
        ("SECRET_TOKEN".to_owned(), "hunter2".to_owned()),
        ("CARGO_PKG_NAME".to_owned(), "wrong".to_owned()),
    ];
    let dir = tempfile::tempdir().unwrap();
    let units = build(dir.path());
    let env = script::inherited(&job, &units.plans[1]);
    let get = |name: &str| env.iter().find(|(it, _)| it == name).map(|(_, value)| value.clone());
    assert_eq!(get("CARGO_PKG_NAME"), Some(OsString::from("z-sys")));
    assert_eq!(get("SECRET_TOKEN"), Some(OsString::from("hunter2")));
    assert_eq!(get("PATH"), Some(OsString::from("/usr/bin")));

    let scripts = slots(&units);
    let tools = PathBuf::from("/state/tools");
    let z = script::script_unit(&of(&units, &scripts, &tools), 1).unwrap();
    let place = blaze_usercode::script::Place {
        code: ContentId::of("blaze v2 test 1", b"script"),
        read: vec!["/w".into()],
        network: false,
        memory: None,
        mode: blaze_usercode::run::Mode::Normal,
    };
    let one = blaze_usercode::script::prepare(&z, &env, &place).unwrap();
    let other: Vec<(String, String)> =
        job.iter().map(|(name, value)| (name.clone(), value.replace("hunter2", "swordfish"))).collect();
    let two = blaze_usercode::script::prepare(&z, &script::inherited(&other, &units.plans[1]), &place).unwrap();
    // A value passed through is in no key: what the script reads of it is traced.
    assert_eq!(one.spec.key, two.spec.key);
    assert!(!one.given.iter().any(|it| it == "SECRET_TOKEN"));
    // Nor in anything said of the run.
    let began = blaze_build_script::run::elided(&z, Vec::new()).unwrap().began;
    let judged = script::judge(&z, &env, &value(1, "", ""), began, Some(&[]), None, false);
    let said = blaze_protocol::cbor::to_vec(&judged.diagnostics).unwrap();
    assert!(!said.windows(7).any(|it| it == b"hunter2"));
}

fn artifact_of(unit: &Unit, plan: &UnitPlan) -> Artifact {
    let package = package(&unit.pkg_id, "p", "/w/p");
    let main = artifact::main(unit, plan).unwrap();
    artifact::artifact(unit, plan, &package, &main, ContentId::of("blaze v2 test 1", b"x"), false, artifact::Runs::default())
}

#[test]
fn a_libs_artifact_is_cargos() {
    let lib = unit(P, target("p", "lib", "/w/p/src/lib.rs"), Mode::Build, &[]);
    let plan = plan(
        "-0123",
        &[
            "/w/target/debug/deps/libp-0123.rlib",
            "/w/target/debug/deps/libp-0123.rmeta",
            "/w/target/debug/deps/p-0123.d",
            "/w/target/debug/libp.rlib",
            "/w/target/debug/libp.d",
        ],
    );
    let it = artifact_of(&lib, &plan);
    assert_eq!(it.kind, "lib");
    assert_eq!(it.filenames, ["/w/target/debug/libp.rlib", "/w/target/debug/deps/libp-0123.rmeta"]);
    assert_eq!(it.executable, None);
    assert_eq!(it.path, "/w/target/debug/libp.rlib");
    assert_eq!(
        it.profile,
        UnitProfile { opt_level: "0".into(), debuginfo: 2, debug_assertions: true, overflow_checks: true, test: false }
    );
    assert_eq!(it.target, unit_target(&lib));
    assert_eq!(it.features, ["default"]);
    assert_eq!(it.manifest_path, "/w/p/Cargo.toml");
}

#[test]
fn a_bins_artifact_is_cargos() {
    let bin = unit(P, target("p", "bin", "/w/p/src/main.rs"), Mode::Build, &[]);
    let plan = plan(
        "-0123",
        &["/w/target/debug/deps/p-0123", "/w/target/debug/deps/p-0123.d", "/w/target/debug/p", "/w/target/debug/p.d"],
    );
    let it = artifact_of(&bin, &plan);
    assert_eq!(it.kind, "bin");
    assert_eq!(it.filenames, ["/w/target/debug/p"]);
    assert_eq!(it.executable.as_deref(), Some("/w/target/debug/p"));
    assert!(!it.profile.test);
}

#[test]
fn a_tests_artifact_is_cargos() {
    let test = unit(P, target("p", "lib", "/w/p/src/lib.rs"), Mode::Test, &[]);
    let plan = plan("-4567", &["/w/target/debug/deps/p-4567", "/w/target/debug/deps/p-4567.d"]);
    let it = artifact_of(&test, &plan);
    assert_eq!(it.kind, "test");
    assert_eq!(it.filenames, ["/w/target/debug/deps/p-4567"]);
    assert_eq!(it.executable.as_deref(), Some("/w/target/debug/deps/p-4567"));
    assert!(it.profile.test);
}

#[test]
fn a_build_scripts_artifact_is_cargos() {
    let script = unit(P, target("build-script-build", "custom-build", "/w/p/build.rs"), Mode::Build, &[]);
    let plan = plan(
        "-89ab",
        &[
            "/w/target/debug/build/p-89ab/build_script_build-89ab",
            "/w/target/debug/build/p-89ab/build_script_build-89ab.d",
            "/w/target/debug/build/p-89ab/build-script-build",
        ],
    );
    let it = artifact_of(&script, &plan);
    assert_eq!(it.kind, "custom-build");
    assert_eq!(it.filenames, ["/w/target/debug/build/p-89ab/build-script-build"]);
    assert_eq!(it.executable, None);
}

#[test]
fn a_program_runs_with_cargos_variables_over_the_jobs() {
    let bin = unit(P, target("p", "bin", "/w/p/src/main.rs"), Mode::Build, &[]);
    let mut plan = plan("-0123", &["/w/target/debug/deps/p-0123", "/w/target/debug/p"]);
    plan.env = vec![
        ("CARGO_MANIFEST_DIR".to_owned(), "/w/p".to_owned()),
        ("CARGO_PKG_NAME".to_owned(), "p".to_owned()),
        ("CARGO_PRIMARY_PACKAGE".to_owned(), "1".to_owned()),
        ("CARGO_CRATE_NAME".to_owned(), "p".to_owned()),
        ("OUT_DIR".to_owned(), "/w/target/debug/build/p-0002/out".to_owned()),
    ];
    let script = [("GIT_HASH".to_owned(), "abc".to_owned())];
    let native = ["/w/target/debug/build/z-sys-0001/out/lib".to_owned(), "/usr/lib/x".to_owned()];
    let env = artifact::run_env(&plan, "/opt/blaze/bin/blaze", &script, &native, "linux");
    let get = |name: &str| env.iter().find(|(it, _)| it == name).map(|(_, value)| value.as_str());
    assert_eq!(get("CARGO"), Some("/opt/blaze/bin/blaze"));
    assert_eq!(get("CARGO_MANIFEST_DIR"), Some("/w/p"));
    assert_eq!(get("OUT_DIR"), Some("/w/target/debug/build/p-0002/out"));
    assert_eq!(get("GIT_HASH"), Some("abc"));
    // What cargo sets for rustc only is not set for the program.
    assert_eq!(get("CARGO_PRIMARY_PACKAGE"), None);
    assert_eq!(get("CARGO_CRATE_NAME"), None);
    assert_eq!(
        get("LD_LIBRARY_PATH"),
        Some("/w/target/debug/build/z-sys-0001/out/lib:/w/target/debug/deps:/w/target/debug")
    );
    assert_eq!(artifact::dylib_var("macos"), "DYLD_FALLBACK_LIBRARY_PATH");

    let package = package(P, "p", "/w/p");
    let runs = artifact::Runs { runner: vec!["qemu-aarch64".to_owned()], env: env.clone() };
    let it = artifact::artifact(&bin, &plan, &package, "/w/target/debug/p", ContentId::of("blaze v2 test 1", b"x"), false, runs);
    assert_eq!(it.runner, ["qemu-aarch64"]);
    assert!(it.harness);
    let job = [
        ("LD_LIBRARY_PATH".to_owned(), "/opt/lib".to_owned()),
        ("CARGO_PKG_NAME".to_owned(), "wrong".to_owned()),
        ("SECRET_TOKEN".to_owned(), "hunter2".to_owned()),
    ];
    let process = run::process_env(&job, &it, "linux");
    let get = |name: &str| process.iter().find(|(it, _)| it == name).map(|(_, value)| value.clone());
    assert_eq!(get("CARGO_PKG_NAME"), Some(OsString::from("p")));
    assert_eq!(get("SECRET_TOKEN"), Some(OsString::from("hunter2")));
    assert_eq!(
        get("LD_LIBRARY_PATH"),
        Some(OsString::from("/w/target/debug/build/z-sys-0001/out/lib:/w/target/debug/deps:/w/target/debug:/opt/lib"))
    );
    // A library's artifact runs nothing, and says no environment.
    let lib = unit(P, target("p", "lib", "/w/p/src/lib.rs"), Mode::Build, &[]);
    let lib_plan = self::plan("-0123", &["/w/target/debug/deps/libp-0123.rlib"]);
    let runs = artifact::Runs { runner: Vec::new(), env };
    let it = artifact::artifact(&lib, &lib_plan, &package, "/w/target/debug/deps/libp-0123.rlib", ContentId::of("blaze v2 test 1", b"x"), false, runs);
    assert!(it.env.is_empty());
}

fn bin_artifact(name: &str) -> Artifact {
    let bin = unit(P, target(name, "bin", "/w/p/src/main.rs"), Mode::Build, &[]);
    let path = format!("/w/target/debug/{name}");
    let plan = plan("-0123", &[&format!("/w/target/debug/deps/{name}-0123"), &path]);
    artifact_of(&bin, &plan)
}

#[test]
fn run_picks_the_binary_cargo_picks() {
    let lib = {
        let lib = unit(P, target("p", "lib", "/w/p/src/lib.rs"), Mode::Build, &[]);
        artifact_of(&lib, &plan("-0123", &["/w/target/debug/deps/libp-0123.rlib"]))
    };
    let one = [lib.clone(), bin_artifact("a")];
    assert_eq!(run::choose(&one, None, None).unwrap().target.name, "a");
    let two = [lib, bin_artifact("a"), bin_artifact("b")];
    assert_eq!(run::choose(&two, Some("b"), None).unwrap().target.name, "b");
    assert_eq!(run::choose(&two, None, Some("b")).unwrap().target.name, "b");
    assert_eq!(
        run::choose(&two, None, None).unwrap_err(),
        "`cargo run` could not determine which binary to run. Use the `--bin` option to specify a binary, or the \
         `default-run` manifest key.\navailable binaries: a, b"
    );
    assert_eq!(run::choose(&two[..1], None, None).unwrap_err(), "a bin target must be available for `cargo run`");
    assert!(run::choose(&two, Some("c"), None).unwrap_err().starts_with("no bin target named `c`"));
}

#[test]
fn a_dependencys_lints_are_capped_and_rustc_closing_lines_hidden() {
    let lib = unit(P, target("p", "lib", "/w/p/src/lib.rs"), Mode::Build, &[]);
    let mut plan = plan("-0123", &[]);
    plan.lints = vec!["--warn=unused_qualifications".to_owned()];
    plan.rustflags = vec!["-D".to_owned(), "warnings".to_owned(), "-Ctarget-cpu=native".to_owned(), "-Aunused".to_owned()];
    plan.check_cfg = vec!["cfg(foo)".to_owned()];
    assert_eq!(
        compile::lint_flags(&lib, &plan, true),
        ["--warn=unused_qualifications", "-D", "warnings", "-Aunused", "--check-cfg", "cfg(foo)"]
    );
    assert_eq!(compile::lint_flags(&lib, &plan, false).last().map(String::as_str), Some("allow"));
    assert!(compile::local(Some("(/w/p)")));
    assert!(!compile::local(Some("(https://github.com/a/b#0123)")));
    assert!(!compile::local(None));
    assert!(compile::cargo_hides("aborting due to 2 previous errors; 1 warning emitted"));
    assert!(compile::cargo_hides("3 warnings emitted"));
    assert!(!compile::cargo_hides("For more information about this error, try `rustc --explain E0308`."));
}

#[test]
fn a_link_library_is_read_as_rustc_reads_l() {
    use blaze_codegen::NativeLibKind;
    let lib = codegen::link_lib("static:+whole-archive,-bundle=foo:bar").unwrap();
    assert_eq!(lib.name, "bar");
    assert_eq!(lib.kind, NativeLibKind::Static { whole: true, bundle: false });
    assert_eq!(codegen::link_lib("ssl").unwrap().kind, NativeLibKind::Dylib);
    assert_eq!(codegen::link_lib("framework=Security").unwrap().kind, NativeLibKind::Framework);
    assert!(codegen::link_lib("weird=x").is_err());
    assert_eq!(codegen::link_arg("-pthread").unwrap().name, "pthread");
    assert!(codegen::link_arg("-Wl,--wrap=malloc").is_err());
}

#[test]
fn a_crate_record_is_an_archive_of_one_member() {
    let lib = unit(P, target("p", "lib", "/w/p/src/lib.rs"), Mode::Build, &[]);
    let bytes = compile::crate_record(&lib, Some(ContentId::of("blaze v2 test 1", b"id")), &[]);
    assert!(bytes.starts_with(b"!<arch>\nlib.rmeta/"));
    assert_eq!(bytes.len() % 2, 0);
}

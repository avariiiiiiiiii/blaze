#![forbid(unsafe_code)]

use std::collections::HashMap;

use blaze_codegen::{plan::OptLevel, settings::Sanitize};
use blaze_units::schema::{DebugInfo, Profile, Strip, Stripped};

use super::{Mounted, codegen_backend, sanitize};

fn profile(name: &str, opt_level: &str, backend: Option<&str>) -> Profile {
    Profile {
        name: name.to_owned(),
        opt_level: opt_level.to_owned(),
        lto: "false".to_owned(),
        codegen_backend: backend.map(str::to_owned),
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

#[test]
fn llvm_and_no_backend_leave_the_choice_to_blaze() {
    for level in [OptLevel::O0, OptLevel::O3] {
        assert_eq!(codegen_backend(&profile("dev", "0", None), "app", level), Ok(()));
        assert_eq!(codegen_backend(&profile("dev", "0", Some("llvm")), "app", level), Ok(()));
    }
}

#[test]
fn cranelift_is_taken_where_the_level_compiles_with_it() {
    let dev = profile("dev", "0", Some("cranelift"));
    assert_eq!(codegen_backend(&dev, "app", OptLevel::O0), Ok(()));
    assert_eq!(codegen_backend(&dev, "app", OptLevel::O1), Ok(()));
    let release = profile("release", "3", Some("cranelift"));
    let why = codegen_backend(&release, "app", OptLevel::O3).unwrap_err();
    assert_eq!(
        why,
        "the profile `release` of `app` asks for codegen-backend `cranelift`, and blaze's code \
         generator takes no backend over the one opt-level 3 compiles with"
    );
}

#[test]
fn a_backend_blaze_has_no_setting_for_is_an_error_naming_it() {
    let stencil = codegen_backend(&profile("dev", "0", Some("stencil")), "app", OptLevel::O0).unwrap_err();
    assert!(stencil.contains("`dev` of `app`") && stencil.contains("`stencil`"), "{stencil}");
    let gcc = codegen_backend(&profile("dev", "0", Some("gcc")), "dep", OptLevel::O0).unwrap_err();
    assert_eq!(
        gcc,
        "the profile `dev` of `dep` asks for codegen-backend `gcc`, which blaze has none of \
         (`cranelift`, `stencil`, `llvm`)"
    );
}

#[test]
fn a_plain_build_is_not_sanitized() {
    let words = |words: &[&str]| words.iter().map(|it| (*it).to_owned()).collect::<Vec<_>>();
    assert_eq!(sanitize(&words(&["-Ctarget-cpu=native", "-Zthreads=8"])), Ok(Sanitize::default()));
    // SanitizerCoverage without its level instruments nothing, so nothing is to link.
    assert_eq!(sanitize(&words(&["-Cpasses=sancov-module"])), Ok(Sanitize::default()));
}

#[test]
fn a_sanitized_build_stops_with_why() {
    let words = |words: &[&str]| words.iter().map(|it| (*it).to_owned()).collect::<Vec<_>>();
    let refused = sanitize(&words(&["-Zsanitizer=address"])).unwrap_err();
    assert!(refused.starts_with("-Z sanitizer=address: "), "{refused}");
    let coverage = words(&["-Cpasses=sancov-module", "-Cllvm-args=-sanitizer-coverage-level=3"]);
    assert_eq!(sanitize(&coverage).unwrap_err(), crate::unstable::UNLINKED);
    let unknown = sanitize(&words(&["-Zno-such-flag"])).unwrap_err();
    assert!(unknown.starts_with("-Z no-such-flag: "), "{unknown}");
}

#[test]
fn what_names_a_file_for_every_worker_is_send_and_sync() {
    fn shared<T: Send + Sync>() {}
    shared::<Mounted>();
    shared::<std::sync::Arc<HashMap<base_db::FileId, String>>>();
}

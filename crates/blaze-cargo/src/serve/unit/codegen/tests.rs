#![forbid(unsafe_code)]

use std::collections::HashMap;

use blaze_codegen::{plan::OptLevel, settings::Sanitize};
use blaze_units::schema::{DebugInfo, Profile, Strip, Stripped};

use super::{Mounted, PgoFlag, codegen_backend, pgo, read_profile, sanitize};

const HOST: &str = "x86_64-unknown-linux-gnu";

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
    assert_eq!(sanitize(&words(&["-Ctarget-cpu=native", "-Zthreads=8"]), HOST), Ok(Sanitize::default()));
    // SanitizerCoverage without its level instruments nothing, so nothing is to link.
    assert_eq!(sanitize(&words(&["-Cpasses=sancov-module"]), HOST), Ok(Sanitize::default()));
}

#[test]
fn a_sanitized_build_stops_with_why() {
    let words = |words: &[&str]| words.iter().map(|it| (*it).to_owned()).collect::<Vec<_>>();
    let refused = sanitize(&words(&["-Zsanitizer=address"]), HOST).unwrap_err();
    assert!(refused.starts_with("-Z sanitizer=address: "), "{refused}");
    let coverage = words(&["-Cpasses=sancov-module", "-Cllvm-args=-sanitizer-coverage-level=3"]);
    assert_eq!(sanitize(&coverage, HOST).unwrap_err(), crate::unstable::UNLINKED);
    let unknown = sanitize(&words(&["-Zno-such-flag"]), HOST).unwrap_err();
    assert!(unknown.starts_with("-Z no-such-flag: "), "{unknown}");
}

#[test]
fn a_profile_to_make_stops_with_the_flag_and_why() {
    let words = |words: &[&str]| words.iter().map(|it| (*it).to_owned()).collect::<Vec<_>>();
    assert_eq!(pgo(&words(&["-Ctarget-cpu=native", "-C", "opt-level=3"])), Ok(PgoFlag::None));
    for (flags, named) in [
        (&["-Cprofile-generate=/tmp/pgo"][..], "`-C profile-generate=/tmp/pgo`"),
        (&["-C", "profile-generate"][..], "`-C profile-generate`"),
        (&["--codegen", "profile_generate=/tmp/pgo"][..], "`-C profile_generate=/tmp/pgo`"),
        // Asked beside a profile to use, it still stops.
        (&["-Cprofile-use=/p", "-Cprofile-generate"][..], "`-C profile-generate`"),
    ] {
        let refused = pgo(&words(flags)).unwrap_err();
        assert!(refused.contains(named), "{refused}");
        assert!(refused.contains("linker writes no profile sections"), "{refused}");
    }
}

#[test]
fn a_profile_to_use_is_its_path_in_every_form_of_the_flag() {
    let words = |words: &[&str]| words.iter().map(|it| (*it).to_owned()).collect::<Vec<_>>();
    for flags in [
        &["-Cprofile-use=/tmp/merged.profdata"][..],
        &["-C", "profile-use=/tmp/merged.profdata"][..],
        &["--codegen=profile-use=/tmp/merged.profdata"][..],
        &["--codegen", "profile-use=/tmp/merged.profdata"][..],
        &["-C", "opt-level=3", "-Cprofile_use=/tmp/merged.profdata", "-Ctarget-cpu=native"][..],
        // The last one given is the one, as rustc takes a codegen option.
        &["-Cprofile-use=/tmp/old.profdata", "-C", "profile-use=/tmp/merged.profdata"][..],
    ] {
        let flags = words(flags);
        assert_eq!(pgo(&flags), Ok(PgoFlag::Use("/tmp/merged.profdata")), "{flags:?}");
    }
    for flags in [&["-Cprofile-use"][..], &["-C", "profile-use="][..]] {
        let refused = pgo(&words(flags)).unwrap_err();
        assert!(refused.contains("names no profile"), "{refused}");
    }
}

#[test]
fn a_profile_that_is_not_there_is_an_error_naming_its_path() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_str().unwrap();
    let missing = read_profile(root, "pgo/merged.profdata").err().unwrap();
    assert!(missing.contains("`-C profile-use=pgo/merged.profdata`"), "{missing}");
    let at = dir.path().join("pgo/merged.profdata");
    assert!(missing.contains(&format!("`{}` can't be read", at.display())), "{missing}");
    let absolute = read_profile(root, "/nonexistent/blaze/merged.profdata").err().unwrap();
    assert!(absolute.contains("`/nonexistent/blaze/merged.profdata` can't be read"), "{absolute}");
}

#[cfg(feature = "llvm")]
#[test]
fn a_profile_read_is_named_by_its_content_and_given() {
    let dir = tempfile::tempdir().unwrap();
    let bytes = b"an indexed profile's bytes".to_vec();
    std::fs::write(dir.path().join("merged.profdata"), &bytes).unwrap();
    let read = read_profile(dir.path().to_str().unwrap(), "merged.profdata").unwrap_or_else(|what| panic!("{what}"));
    assert_eq!(read.id, hir_ty_codegen::v2::profile_id(&bytes));
    assert_eq!(read._given.id(), read.id);
    assert_eq!(read.unstored.lock().unwrap().as_deref(), Some(&bytes[..]));
}

#[cfg(not(feature = "llvm"))]
#[test]
fn a_profile_to_use_without_llvm_stops_naming_the_feature() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("merged.profdata"), b"profile").unwrap();
    let refused = read_profile(dir.path().to_str().unwrap(), "merged.profdata").err().unwrap();
    assert!(refused.contains("`-C profile-use=merged.profdata`") && refused.contains("`llvm`"), "{refused}");
}

#[test]
fn what_names_a_file_for_every_worker_is_send_and_sync() {
    fn shared<T: Send + Sync>() {}
    shared::<Mounted>();
    shared::<std::sync::Arc<HashMap<base_db::FileId, String>>>();
}

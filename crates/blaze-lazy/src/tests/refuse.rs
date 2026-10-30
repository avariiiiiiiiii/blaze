use std::collections::BTreeMap;

use blaze_codegen::{
    Arch,
    plan::{Mode, OptLevel, Override, Policy, Profile},
    settings::{DebugInfo, Panic, Relocation, Toolchain},
};
use blaze_ids::ContentId;

use crate::{
    Purpose, Refused, Session, allow,
    plan::Plan,
    session::SessionError,
    stub::{Abi, Isa, X86Save},
};

fn policy(level: OptLevel, mode: Mode) -> Policy {
    Policy {
        toolchain: Toolchain {
            id: ContentId([1; 32]),
            target: "x86_64-unknown-linux-gnu".to_owned(),
            arch: Arch::X86_64,
            cpu: "x86-64-v3".to_owned(),
            features: Vec::new(),
            cranelift: "0.136".to_owned(),
            llvm: Some("23.1".to_owned()),
            frame_pointers: true,
            relocation: Relocation::Pic,
        },
        profile: Profile {
            opt_level: level,
            overflow_checks: true,
            debug_assertions: true,
            debuginfo: DebugInfo::Full,
            panic: Panic::Unwind,
            packages: BTreeMap::new(),
            build_override: Override::default(),
        },
        mode,
        fold: false,
    }
}

#[test]
fn a_dev_run_or_test_may_compile_lazily() {
    for profile in ["dev", "test", "my-dev"] {
        for level in [OptLevel::O0, OptLevel::O1] {
            let policy = policy(level, Mode::Live);
            assert_eq!(allow(&Purpose { profile, policy: &policy, output: None }), Ok(()));
        }
    }
}

#[test]
fn a_release_profile_is_refused_by_name_or_level() {
    let fast = policy(OptLevel::O3, Mode::Live);
    let refused = allow(&Purpose { profile: "release", policy: &fast, output: None }).unwrap_err();
    assert_eq!(refused, Refused::Release { profile: "release".to_owned(), level: OptLevel::O3 });
    assert!(refused.to_string().contains("profile `release` (opt-level 3)"), "{refused}");
    // A custom profile inheriting from release is optimized.
    for level in [OptLevel::O2, OptLevel::O3, OptLevel::S, OptLevel::Z] {
        let optimized = policy(level, Mode::Live);
        assert!(matches!(
            allow(&Purpose { profile: "profiling", policy: &optimized, output: None }),
            Err(Refused::Release { .. })
        ));
    }
    // `bench` by name, whatever its level says.
    let slow = policy(OptLevel::O0, Mode::Live);
    assert!(matches!(allow(&Purpose { profile: "bench", policy: &slow, output: None }), Err(Refused::Release { .. })));
}

#[test]
fn a_build_that_writes_an_output_is_refused_naming_it() {
    let dev = policy(OptLevel::O0, Mode::Live);
    let refused = allow(&Purpose { profile: "dev", policy: &dev, output: Some("target/debug/app") }).unwrap_err();
    assert!(refused.to_string().contains("target/debug/app"), "{refused}");
    let whole = policy(OptLevel::O0, Mode::Output);
    assert_eq!(
        allow(&Purpose { profile: "dev", policy: &whole, output: None }),
        Err(Refused::Whole { profile: "dev".to_owned() })
    );
}

#[test]
fn a_session_is_refused_for_release_and_makes_no_stubs() {
    let fast = policy(OptLevel::O3, Mode::Live);
    let plan = Plan::new(&[super::function(1)], &[], &[]).unwrap();
    let isa = Isa::X86_64 { abi: Abi::SysV, save: X86Save::Fx };
    let made = Session::new(&Purpose { profile: "release", policy: &fast, output: None }, plan, &isa, 0);
    assert!(matches!(made, Err(SessionError::Refused(Refused::Release { .. }))));
}

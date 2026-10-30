//! How each function of a build is compiled: its backend and settings, and whether its
//! callers reach it through its jump-table slot.
//!
//! - The profile's `opt-level`, with cargo's `[profile.<name>.package.<spec>]` overrides
//!   (`"*"` for every package that is not a workspace member) and `build-override` for what
//!   build scripts and proc macros run, picks the backend (docs/cargo.md, "Backends"): 0 and
//!   1 Cranelift, 2, `s` and `z` LLVM at `-O2`, 3 LLVM at `-O3` (#635: tests are built at the
//!   profile's level). Cranelift optimizes (`opt_level=speed`) all but a workspace member's
//!   code, which no edit reaches and so is compiled once.
//! - The standard library's own functions are compiled as rustc's shipped std is: `-O3`, no
//!   overflow checks, no debug assertions, line tables. A generic of std is compiled as the
//!   crate that instantiates it asks, as rustc compiles it in that crate's codegen unit.
//! - Only a workspace member's functions are reached through slots in a build that takes
//!   edits: nothing else is ever patched, so its callers call it directly (#1404).
//! - Tiering (#807): a build that takes edits starts a function whose level is LLVM's at
//!   Cranelift and puts LLVM's code in its slot once compiled. A build whose output is a file
//!   compiles each function once, at its level: the output never depends on timing.

use std::collections::BTreeMap;

use blaze_ids::{ContentId, Hasher};
use serde::{Deserialize, Serialize};

use crate::{
    Backend,
    settings::{DebugInfo, Panic, Sanitize, Spec, Toolchain},
};

pub const POLICY_DOMAIN: &str = "blaze v2 codegen policy 1";

/// cargo's `opt-level`.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum OptLevel {
    O0,
    O1,
    O2,
    O3,
    S,
    Z,
}

impl OptLevel {
    /// The backend the level is compiled with, Cranelift at `opt_level=none`; `None` when it
    /// asks for LLVM and blaze has none.
    pub fn backend(self, llvm: bool) -> Option<Backend> {
        match self {
            OptLevel::O0 | OptLevel::O1 => Some(Backend::Cranelift { speed: false }),
            OptLevel::O2 | OptLevel::S | OptLevel::Z => llvm.then_some(Backend::Llvm { opt: 2 }),
            OptLevel::O3 => llvm.then_some(Backend::Llvm { opt: 3 }),
        }
    }
}

/// What a profile or one of its overrides sets; `None` leaves what is under it.
#[derive(Serialize, Deserialize, Clone, Default, PartialEq, Eq, Hash, Debug)]
pub struct Override {
    pub opt_level: Option<OptLevel>,
    pub overflow_checks: Option<bool>,
    pub debug_assertions: Option<bool>,
    pub debuginfo: Option<DebugInfo>,
}

/// A cargo profile as resolved for one build (`inherits`, the configuration, environment).
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Hash, Debug)]
pub struct Profile {
    pub opt_level: OptLevel,
    pub overflow_checks: bool,
    pub debug_assertions: bool,
    pub debuginfo: DebugInfo,
    pub panic: Panic,
    /// `[profile.<name>.package.<name>]`, by package name, and `"*"`.
    // BTreeMap: `Profile` is serialized and hashed.
    pub packages: BTreeMap<String, Override>,
    /// `[profile.<name>.build-override]`.
    pub build_override: Override,
}

/// Whose code an instance is, for its settings.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Origin {
    /// A workspace member's.
    Member(String),
    /// A dependency's, by package name.
    Dependency(String),
    /// What a build script or proc macro runs: its own code and its dependencies'.
    BuildTime(String),
    /// A crate of the standard library's.
    Std,
}

/// What a build makes.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Mode {
    /// A file: an executable, a test binary, a library. Each function compiled once.
    Output,
    /// A running program that takes edits (`blaze run`, `blaze test --watch`): members'
    /// functions are reached through slots, and what is at LLVM's level starts at
    /// Cranelift's.
    Live,
}

/// How every function of one build is compiled.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Hash, Debug)]
pub struct Policy {
    pub toolchain: Toolchain,
    pub profile: Profile,
    pub mode: Mode,
    /// #1292: fold functions that differ only in constants. Only where code speed holds:
    /// off for profiles at LLVM's levels unless asked for.
    pub fold: bool,
    /// The build's sanitizers and SanitizerCoverage (`RUSTFLAGS`' `-Zsanitizer` and the
    /// rest), the same for every function.
    pub sanitize: Sanitize,
}

/// How one function is compiled.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Choice {
    /// What the build links first.
    pub first: Spec,
    /// What replaces it in its slot once compiled: a tiered function of a live build.
    pub then: Option<Spec>,
    /// Whether its callers reach it through its slot ([`crate::Target::Function`]) or
    /// directly ([`crate::Target::Direct`]).
    pub patchable: bool,
}

/// Why a function can't be compiled as its build asks.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PlanError {
    /// The level asks for LLVM, and this blaze was built without it.
    NoLlvm { origin: Origin, level: OptLevel },
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanError::NoLlvm { origin, level } => write!(
                f,
                "{origin:?} is compiled at opt-level {level:?}, which needs LLVM, and this blaze \
                 was built without it (`--features llvm`)"
            ),
        }
    }
}

/// The standard library as rustc ships it: what its own functions are compiled as.
const SHIPPED_STD: Override = Override {
    opt_level: Some(OptLevel::O3),
    overflow_checks: Some(false),
    debug_assertions: Some(false),
    debuginfo: Some(DebugInfo::LineTablesOnly),
};

impl Policy {
    pub fn id(&self) -> ContentId {
        let bytes = serde_json::to_vec(self).expect("a policy is plain data");
        let mut hasher = Hasher::new(POLICY_DOMAIN);
        hasher.part(&bytes);
        hasher.finish()
    }

    /// The overrides that apply to `origin`, outermost first, as cargo layers them: the
    /// profile, then `"*"` for a non-member, then the package's own; `build-override` over
    /// all of them for build-time code.
    fn layers(&self, origin: &Origin) -> Vec<&Override> {
        let packages = &self.profile.packages;
        match origin {
            Origin::Member(name) => packages.get(name).into_iter().collect(),
            Origin::Dependency(name) => {
                packages.get("*").into_iter().chain(packages.get(name)).collect()
            }
            Origin::BuildTime(name) => packages
                .get("*")
                .into_iter()
                .chain(packages.get(name))
                .chain([&self.profile.build_override])
                .collect(),
            Origin::Std => vec![&SHIPPED_STD],
        }
    }

    /// How a function whose code is `origin`'s is compiled. `overflow_checks` is the front
    /// end's when the instance says (`#[rustc_inherit_overflow_checks]` or std's MIR, built
    /// without them), else the profile's for the origin.
    pub fn choose(
        &self,
        origin: &Origin,
        overflow_checks: Option<bool>,
    ) -> Result<Choice, PlanError> {
        let profile = &self.profile;
        let mut level = profile.opt_level;
        let mut checks = profile.overflow_checks;
        let mut assertions = profile.debug_assertions;
        let mut debuginfo = profile.debuginfo;
        for layer in self.layers(origin) {
            level = layer.opt_level.unwrap_or(level);
            checks = layer.overflow_checks.unwrap_or(checks);
            assertions = layer.debug_assertions.unwrap_or(assertions);
            debuginfo = layer.debuginfo.unwrap_or(debuginfo);
        }
        let llvm = self.toolchain.llvm.is_some();
        let backend =
            level.backend(llvm).ok_or_else(|| PlanError::NoLlvm { origin: origin.clone(), level })?;
        // Only a member's code is edited, so only it is recompiled: the rest is compiled once
        // and can afford Cranelift's optimizer.
        let member = matches!(origin, Origin::Member(_));
        let backend = match backend {
            Backend::Cranelift { .. } => Backend::Cranelift { speed: !member },
            other => other,
        };
        let live = self.mode == Mode::Live;
        let spec = Spec {
            toolchain: self.toolchain.clone(),
            backend,
            overflow_checks: overflow_checks.unwrap_or(checks),
            debug_assertions: assertions,
            panic: profile.panic,
            debuginfo,
            fold: self.fold && backend.is_cranelift(),
            // A member's code built into another function's would outlive a patch of it.
            mir_inline: matches!(level, OptLevel::O2 | OptLevel::O3) && !live,
            sanitize: self.sanitize,
            policy: self.id(),
        };
        let patchable = live && member;
        let tiered = live && !backend.is_cranelift();
        Ok(if tiered {
            // The first tier runs only until LLVM's code is in the slot: compiled fast.
            Choice {
                first: Spec { fold: self.fold, ..spec.at(Backend::Cranelift { speed: false }) },
                then: Some(spec),
                patchable,
            }
        } else {
            Choice { first: spec, then: None, patchable }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Arch, settings::Relocation};

    fn policy(level: OptLevel, mode: Mode) -> Policy {
        Policy {
            toolchain: Toolchain {
                id: ContentId([1; 32]),
                target: "aarch64-unknown-linux-gnu".to_owned(),
                arch: Arch::Aarch64,
                cpu: "neoverse-v2".to_owned(),
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
            sanitize: Sanitize::default(),
        }
    }

    fn member() -> Origin {
        Origin::Member("app".to_owned())
    }

    #[test]
    fn dev_is_cranelift_and_only_members_are_patched() {
        let policy = policy(OptLevel::O0, Mode::Live);
        let own = policy.choose(&member(), None).unwrap();
        assert_eq!(own.first.backend, Backend::Cranelift { speed: false });
        assert!(own.patchable && own.then.is_none());
        let dependency = policy.choose(&Origin::Dependency("regex".to_owned()), None).unwrap();
        assert!(!dependency.patchable);
    }

    #[test]
    fn only_a_members_cranelift_code_is_unoptimized() {
        for mode in [Mode::Output, Mode::Live] {
            let policy = policy(OptLevel::O0, mode);
            let speed = |origin: Origin| policy.choose(&origin, None).unwrap().first.backend;
            assert_eq!(speed(member()), Backend::Cranelift { speed: false });
            assert_eq!(speed(Origin::Dependency("regex".to_owned())), Backend::Cranelift { speed: true });
            assert_eq!(speed(Origin::BuildTime("serde_derive".to_owned())), Backend::Cranelift { speed: true });
        }
        let dependency = policy(OptLevel::O0, Mode::Output).choose(&Origin::Dependency("regex".to_owned()), None);
        let own = policy(OptLevel::O0, Mode::Output).choose(&member(), None);
        assert_ne!(dependency.unwrap().first.settings(), own.unwrap().first.settings());
    }

    #[test]
    fn std_is_compiled_as_rustc_ships_it() {
        let std = policy(OptLevel::O0, Mode::Output).choose(&Origin::Std, None).unwrap();
        assert_eq!(std.first.backend, Backend::Llvm { opt: 3 });
        assert!(!std.first.overflow_checks && !std.first.debug_assertions);
        assert_eq!(std.first.debuginfo, DebugInfo::LineTablesOnly);
    }

    #[test]
    fn package_overrides_are_layered_as_cargo_layers_them() {
        // `[profile.dev] opt-level = 1` and `[profile.dev.package."*"] opt-level = 3` (#635).
        let mut policy = policy(OptLevel::O1, Mode::Output);
        policy.profile.packages.insert(
            "*".to_owned(),
            Override { opt_level: Some(OptLevel::O3), ..Override::default() },
        );
        policy.profile.packages.insert(
            "slow".to_owned(),
            Override { opt_level: Some(OptLevel::O0), ..Override::default() },
        );
        let own = policy.choose(&member(), None).unwrap();
        assert_eq!(own.first.backend, Backend::Cranelift { speed: false });
        let dependency = policy.choose(&Origin::Dependency("nalgebra".to_owned()), None).unwrap();
        assert_eq!(dependency.first.backend, Backend::Llvm { opt: 3 });
        let named = policy.choose(&Origin::Dependency("slow".to_owned()), None).unwrap();
        assert_eq!(named.first.backend, Backend::Cranelift { speed: true });
        // `"*"` is not for members, even one named like a dependency.
        let member_star = policy.choose(&Origin::Member("nalgebra".to_owned()), None).unwrap();
        assert_eq!(member_star.first.backend, Backend::Cranelift { speed: false });
    }

    #[test]
    fn build_override_is_outermost_for_build_time_code() {
        let mut policy = policy(OptLevel::O3, Mode::Output);
        policy.profile.build_override.opt_level = Some(OptLevel::O0);
        let build = policy.choose(&Origin::BuildTime("serde_derive".to_owned()), None).unwrap();
        assert_eq!(build.first.backend, Backend::Cranelift { speed: true });
    }

    #[test]
    fn a_live_build_tiers_and_an_output_does_not() {
        let mut live = policy(OptLevel::O0, Mode::Live);
        live.profile.packages.insert(
            "*".to_owned(),
            Override { opt_level: Some(OptLevel::O3), ..Override::default() },
        );
        let dependency = Origin::Dependency("memchr".to_owned());
        let tiered = live.choose(&dependency, None).unwrap();
        assert_eq!(tiered.first.backend, Backend::Cranelift { speed: false });
        assert_eq!(tiered.then.as_ref().unwrap().backend, Backend::Llvm { opt: 3 });
        assert_ne!(tiered.first.settings(), tiered.then.unwrap().settings());
        let mut output = live.clone();
        output.mode = Mode::Output;
        let once = output.choose(&dependency, None).unwrap();
        assert_eq!(once.first.backend, Backend::Llvm { opt: 3 });
        assert!(once.then.is_none());
    }

    #[test]
    fn mir_is_inlined_at_two_and_three_outside_live_builds() {
        let inlines = |level, mode| {
            let choice = policy(level, mode).choose(&member(), None).unwrap();
            let last = choice.then.unwrap_or(choice.first);
            last.mir_inline
        };
        for level in [OptLevel::O0, OptLevel::O1, OptLevel::S, OptLevel::Z] {
            assert!(!inlines(level, Mode::Output), "{level:?}");
        }
        for level in [OptLevel::O2, OptLevel::O3] {
            assert!(inlines(level, Mode::Output), "{level:?}");
            assert!(!inlines(level, Mode::Live), "{level:?}");
        }
    }

    #[test]
    fn without_llvm_a_level_that_needs_it_is_refused() {
        let mut policy = policy(OptLevel::O3, Mode::Output);
        policy.toolchain.llvm = None;
        assert!(matches!(policy.choose(&member(), None), Err(PlanError::NoLlvm { .. })));
    }

    #[test]
    fn the_front_ends_overflow_checks_win() {
        let policy = policy(OptLevel::O0, Mode::Output);
        let inherited = policy.choose(&Origin::Std, Some(true)).unwrap();
        assert!(inherited.first.overflow_checks);
        let built_without = policy.choose(&member(), Some(false)).unwrap();
        assert!(!built_without.first.overflow_checks);
    }

    #[test]
    fn the_policy_is_in_every_setting() {
        let a = policy(OptLevel::O0, Mode::Output);
        let mut b = a.clone();
        b.profile.packages.insert("x".to_owned(), Override::default());
        let (a, b) = (a.choose(&member(), None).unwrap(), b.choose(&member(), None).unwrap());
        assert_ne!(a.first.settings(), b.first.settings());
    }
}

//! Everything besides the function that changes its machine code ([`Spec`]), and its content
//! ID ([`crate::Settings`]). A field left out of [`Spec::settings`] is stale code: every
//! field is hashed, and a test changes each one alone.

use blaze_ids::{ContentId, Hasher};
use serde::{Deserialize, Serialize};

use crate::{Arch, Backend, Settings};

pub const SETTINGS_DOMAIN: &str = "blaze v2 codegen settings 1";

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Panic {
    Unwind,
    Abort,
}

/// rustc's `-C debuginfo`.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum DebugInfo {
    None,
    LineTablesOnly,
    Limited,
    Full,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Relocation {
    /// Position-independent: what every executable blaze links is.
    Pic,
    Static,
}

/// How the program is built, for a function's settings: the toolchain and target, the same
/// for every function of a build.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Hash, Debug)]
pub struct Toolchain {
    /// The pinned nightly's and blaze's own code identity (`harness/toolchain` and the
    /// codegen producer): another blaze is other code.
    pub id: ContentId,
    /// The target triple.
    pub target: String,
    pub arch: Arch,
    /// `-C target-cpu`, resolved (`native` is the host's name).
    pub cpu: String,
    /// `-C target-feature`, resolved and sorted.
    pub features: Vec<String>,
    /// The backends' versions, as they name themselves.
    pub cranelift: String,
    /// `None` for a blaze built without LLVM: a unit that asks for it is refused.
    pub llvm: Option<String>,
    pub frame_pointers: bool,
    pub relocation: Relocation,
}

/// What changes one function's machine code besides its MIR and its type arguments' facts.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Hash, Debug)]
pub struct Spec {
    pub toolchain: Toolchain,
    pub backend: Backend,
    /// `-C overflow-checks` of the crate whose MIR the instance is, as rustc builds it
    /// (`#[rustc_inherit_overflow_checks]` takes the caller's).
    pub overflow_checks: bool,
    /// `-C debug-assertions` of the crate that compiles the instance: what
    /// `intrinsics::ub_checks()` is at codegen.
    pub debug_assertions: bool,
    pub panic: Panic,
    pub debuginfo: DebugInfo,
    /// Folding functions that differ only in constants (#1292): changes which body a
    /// function's code is.
    pub fold: bool,
    /// The content ID of the policy that gives every function of the build its settings
    /// ([`crate::plan::Policy::id`]): a function's relocations name its callees by keys made
    /// with their settings, so a change to how a callee is compiled is a change to the
    /// caller's output.
    pub policy: ContentId,
}

impl Spec {
    pub fn settings(&self) -> Settings {
        let Spec {
            toolchain,
            backend,
            overflow_checks,
            debug_assertions,
            panic,
            debuginfo,
            fold,
            policy,
        } = self;
        let Toolchain {
            id,
            target,
            arch,
            cpu,
            features,
            cranelift,
            llvm,
            frame_pointers,
            relocation,
        } = toolchain;
        let mut hasher = Hasher::new(SETTINGS_DOMAIN);
        hasher.id(id).part(target.as_bytes());
        hasher.update(&[crate::content::arch_tag(*arch)]).part(cpu.as_bytes());
        hasher.update(&(features.len() as u64).to_le_bytes());
        for feature in features {
            hasher.part(feature.as_bytes());
        }
        hasher.part(cranelift.as_bytes());
        match llvm {
            Some(llvm) => hasher.update(&[1]).part(llvm.as_bytes()),
            None => hasher.update(&[0]),
        };
        hasher.update(&[*frame_pointers as u8, *relocation as u8]);
        match backend {
            Backend::Cranelift => hasher.update(&[0]),
            Backend::Llvm { opt } => hasher.update(&[1, *opt]),
        };
        hasher.update(&[
            *overflow_checks as u8,
            *debug_assertions as u8,
            *panic as u8,
            *debuginfo as u8,
            *fold as u8,
        ]);
        hasher.id(policy);
        Settings(hasher.finish())
    }

    /// The settings of the same function at another backend: the first tier's of a function
    /// tiered up (#807), or its last.
    pub fn at(&self, backend: Backend) -> Spec {
        Spec { backend, ..self.clone() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn spec() -> Spec {
        Spec {
            toolchain: Toolchain {
                id: ContentId([1; 32]),
                target: "x86_64-unknown-linux-gnu".to_owned(),
                arch: Arch::X86_64,
                cpu: "x86-64".to_owned(),
                features: vec!["+sse2".to_owned()],
                cranelift: "0.136".to_owned(),
                llvm: Some("23.1".to_owned()),
                frame_pointers: false,
                relocation: Relocation::Pic,
            },
            backend: Backend::Cranelift,
            overflow_checks: true,
            debug_assertions: true,
            panic: Panic::Unwind,
            debuginfo: DebugInfo::Full,
            fold: false,
            policy: ContentId([2; 32]),
        }
    }

    #[test]
    fn every_field_is_in_the_settings() {
        let base = spec();
        let changes: Vec<fn(&mut Spec)> = vec![
            |it| it.toolchain.id = ContentId([3; 32]),
            |it| it.toolchain.target = "aarch64-unknown-linux-gnu".to_owned(),
            |it| it.toolchain.arch = Arch::Aarch64,
            |it| it.toolchain.cpu = "znver5".to_owned(),
            |it| it.toolchain.features.push("+avx2".to_owned()),
            |it| it.toolchain.cranelift = "0.137".to_owned(),
            |it| it.toolchain.llvm = None,
            |it| it.toolchain.llvm = Some("24.0".to_owned()),
            |it| it.toolchain.frame_pointers = true,
            |it| it.toolchain.relocation = Relocation::Static,
            |it| it.backend = Backend::Llvm { opt: 3 },
            |it| it.overflow_checks = false,
            |it| it.debug_assertions = false,
            |it| it.panic = Panic::Abort,
            |it| it.debuginfo = DebugInfo::LineTablesOnly,
            |it| it.fold = true,
            |it| it.policy = ContentId([4; 32]),
        ];
        let mut seen = std::collections::BTreeSet::from([base.settings()]);
        for change in changes {
            let mut changed = base.clone();
            change(&mut changed);
            assert!(seen.insert(changed.settings()), "{changed:?} has the settings of another");
        }
    }

    #[test]
    fn features_are_framed() {
        let mut a = spec();
        let mut b = spec();
        a.toolchain.features = vec!["+a".to_owned(), "+b".to_owned()];
        b.toolchain.features = vec!["+a+b".to_owned()];
        assert_ne!(a.settings(), b.settings());
    }

    #[test]
    fn levels_of_llvm_are_settings() {
        let a = spec().at(Backend::Llvm { opt: 2 });
        let b = spec().at(Backend::Llvm { opt: 3 });
        assert_ne!(a.settings(), b.settings());
    }
}

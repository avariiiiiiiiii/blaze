//! User code (docs/v2/plan/lanes/usercode.md section 6.5; #1450, #1480, #1296, #1297, #1443, #804).
//!
//! - A build script's or proc macro's run happens in a [`blaze_core::Sandbox`] that traces
//!   what it read ([`blaze_core::Traced`]). Its result is keyed by its own code hash and
//!   exactly those inputs ([`RunKey`]), not by "the package changed". A run that read what
//!   can't be traced ([`blaze_core::Untraceable`]) is keyed as cargo keys it.
//! - The daemon hashes and checks every output before the store keeps it.
//! - Tests: each test's reached functions are recorded by code hash ([`Reached`]); an edit
//!   reruns the tests that reach a changed hash.
//!
//! The parts: [`trace`] turns what a sandbox saw into a [`Traced`]; [`index`] finds a run's
//! result in the store by the shape of what it read and then by those inputs' values now;
//! [`inputs`] has what reads are checked against (a view and the machine); [`rule`] is cargo's
//! rule for what can't be traced; [`run`] is a run as a state machine, from the store or a
//! sandboxed spawn to a kept result; [`script`] and [`pool`] are build scripts and the warm
//! processes proc macros run in; [`test`] selects and runs tests (docs/v2/plan/lanes/usercode/notes.md).

#![forbid(unsafe_code)]

pub mod confine;
pub mod index;
pub mod inputs;
pub mod pool;
pub mod rule;
pub mod run;
pub mod script;
pub mod test;
pub mod trace;

use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
};

use blaze_core::Traced;
use blaze_ids::{ContentId, Hasher};
use blaze_items::CodeHash;
use serde::{Deserialize, Serialize};

/// What a run of user code is: the code that runs, and what it was given.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum RunKind {
    BuildScript,
    ProcMacro,
    Test,
}

impl RunKind {
    fn byte(self) -> u8 {
        match self {
            RunKind::BuildScript => 0,
            RunKind::ProcMacro => 1,
            RunKind::Test => 2,
        }
    }
}

/// The key a run's result is first looked up by: its code and what it was given (arguments,
/// the environment cargo gives it, a proc macro's input tokens). A result found under it is
/// used only if every input [`Traced`] of it is the same in the view ([`RunKey::traced`]).
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct RunKey {
    pub code: ContentId,
    pub given: ContentId,
}

impl RunKey {
    pub const DOMAIN: &'static str = "blaze v2 run traced 2";

    /// The store's key of the result: this and the hash of what the run read.
    pub fn traced(&self, traced: &Traced) -> ContentId {
        if trace::normal(traced) {
            return trace::Keyer::new(self).traced(traced);
        }
        let mut traced = traced.clone();
        trace::normalize(&mut traced);
        trace::Keyer::new(self).traced(&traced)
    }
}

/// What a run is given, hashed into [`RunKey::given`]: its kind, program arguments, the
/// variables blaze sets for it (cargo's, for a build script), its standard input and what
/// else the caller names. The environment the user's shell passes through is not given: what
/// of it the run reads is traced (#805).
#[derive(Clone, Debug)]
pub struct Given {
    kind: RunKind,
    args: Vec<OsString>,
    // BTreeMap: hashed in order into the cache key `RunKey::given`, and `vars` hands it out in order.
    env: BTreeMap<OsString, OsString>,
    /// The hash of the standard input: given bytes are hashed once, as they are given, and
    /// never copied.
    stdin: [u8; 32],
    /// Each part's hash by its name, sorted by name: hashed in order into `RunKey::given`.
    parts: Vec<(&'static str, [u8; 32])>,
}

impl Given {
    pub const DOMAIN: &'static str = "blaze v2 run given 2";
    const PART: &'static str = "blaze v2 run given part 1";

    pub fn new(kind: RunKind) -> Given {
        Given {
            kind,
            args: Vec::new(),
            env: BTreeMap::new(),
            stdin: Hasher::new(Given::PART).finish().0,
            parts: Vec::new(),
        }
    }

    pub fn arg(&mut self, arg: impl Into<OsString>) -> &mut Given {
        self.args.push(arg.into());
        self
    }

    /// A variable blaze sets; set twice, the last value is the one the run sees.
    pub fn env(&mut self, name: impl Into<OsString>, value: impl Into<OsString>) -> &mut Given {
        self.env.insert(name.into(), value.into());
        self
    }

    pub fn stdin(&mut self, bytes: &[u8]) -> &mut Given {
        self.stdin = Hasher::new(Given::PART).update(bytes).finish().0;
        self
    }

    /// Something else the run is a function of, by a name of the caller's: a macro's input
    /// tokens, its kind and name, the target. Named twice, the last is kept.
    pub fn part(&mut self, name: &'static str, bytes: &[u8]) -> &mut Given {
        let hash = Hasher::new(Given::PART).update(bytes).finish().0;
        match self.parts.binary_search_by(|it| it.0.cmp(name)) {
            Ok(at) => self.parts[at].1 = hash,
            Err(at) => self.parts.insert(at, (name, hash)),
        }
        self
    }

    pub fn id(&mut self, name: &'static str, id: &ContentId) -> &mut Given {
        self.part(name, &id.0)
    }

    pub fn finish(&self) -> ContentId {
        let mut hasher = Hasher::new(Given::DOMAIN);
        hasher.update(&[self.kind.byte()]);
        hasher.update(&(self.args.len() as u64).to_le_bytes());
        for arg in &self.args {
            hasher.part(trace::os_bytes(arg));
        }
        hasher.update(&(self.env.len() as u64).to_le_bytes());
        for (name, value) in &self.env {
            hasher.part(trace::os_bytes(name)).part(trace::os_bytes(value));
        }
        hasher.update(&self.stdin);
        hasher.update(&(self.parts.len() as u64).to_le_bytes());
        for (name, hash) in &self.parts {
            hasher.part(name.as_bytes()).update(hash);
        }
        hasher.finish()
    }

    /// Whether `name` is one blaze sets, so not one to trace.
    pub fn sets(&self, name: &OsStr) -> bool {
        self.env.contains_key(name)
    }

    pub fn vars(&self) -> impl Iterator<Item = (&OsString, &OsString)> {
        self.env.iter()
    }
}

/// The functions one test reached, by code hash.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Reached {
    pub test: String,
    pub binary: ContentId,
    pub reached: Vec<CodeHash>,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn key() -> RunKey {
        RunKey { code: ContentId::of("blaze v2 test 1", b"script"), given: Given::new(RunKind::BuildScript).finish() }
    }

    #[test]
    fn a_traced_key_ignores_order_and_sees_every_value() {
        let a = Traced {
            files: vec![(PathBuf::from("/b"), Some([1; 32])), (PathBuf::from("/a"), None)],
            env: vec![("CC".into(), Some("cc".into()))],
            ..Traced::default()
        };
        let mut b = a.clone();
        b.files.reverse();
        assert_eq!(key().traced(&a), key().traced(&b));
        let mut c = a.clone();
        c.files[0].1 = Some([2; 32]);
        assert_ne!(key().traced(&a), key().traced(&c));
        let mut d = a.clone();
        d.env[0].1 = None;
        assert_ne!(key().traced(&a), key().traced(&d));
        let mut e = a.clone();
        e.untraceable.push(blaze_core::Untraceable::Clock);
        assert_ne!(key().traced(&a), key().traced(&e));
    }

    #[test]
    fn given_is_framed_and_kind_counts() {
        let mut one = Given::new(RunKind::BuildScript);
        one.arg("ab").arg("c");
        let mut two = Given::new(RunKind::BuildScript);
        two.arg("a").arg("bc");
        assert_ne!(one.finish(), two.finish());
        assert_ne!(Given::new(RunKind::ProcMacro).finish(), Given::new(RunKind::Test).finish());
        let mut env_one = Given::new(RunKind::BuildScript);
        env_one.env("A", "1").env("B", "2");
        let mut env_two = Given::new(RunKind::BuildScript);
        env_two.env("B", "2").env("A", "1");
        assert_eq!(env_one.finish(), env_two.finish());
    }
}

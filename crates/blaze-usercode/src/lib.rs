//! User code (docs/v2/usercode.md section 6.5; #1450, #1480, #1296, #1297, #1443, #804).
//!
//! - A build script's or proc macro's run happens in a [`blaze_core::Sandbox`] that traces
//!   what it read ([`blaze_core::Traced`]). Its result is keyed by its own code hash and
//!   exactly those inputs ([`RunKey`]), not by "the package changed". A run that read what
//!   can't be traced ([`blaze_core::Untraceable`]) is keyed as cargo keys it.
//! - The daemon hashes and checks every output before the store keeps it.
//! - Tests: each test's reached functions are recorded by code hash ([`Reached`]); an edit
//!   reruns the tests that reach a changed hash.

use blaze_core::Traced;
use blaze_ids::ContentId;
use blaze_items::CodeHash;
use serde::{Deserialize, Serialize};

/// What a run of user code is: the code that runs, and what it was given.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum RunKind {
    BuildScript,
    ProcMacro,
    Test,
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
    /// The store's key of the result: this and the hash of what the run read.
    pub fn traced(&self, traced: &Traced) -> ContentId {
        let _ = traced;
        unimplemented!("v2: the key of a run with its traced inputs (v2-usercode)")
    }
}

/// The functions one test reached, by code hash.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Reached {
    pub test: String,
    pub binary: ContentId,
    pub reached: Vec<CodeHash>,
}

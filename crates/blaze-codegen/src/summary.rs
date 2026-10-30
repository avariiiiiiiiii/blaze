//! Release builds: what each function tells the others before any is compiled (#899, #176),
//! as ThinLTO's summaries do, and what a function's compile is given from them.
//!
//! A [`Summary`] is a function of the function alone, kept in the store under its instance
//! key, so a warm build decides every function's [`Imports`] from summaries without building
//! any IR. What a compile is given is in its blob's key ([`Imports::blob_key`]): every body it
//! may build in, by instance key (which holds the callee's code hash), and every fact of the
//! whole program it was compiled with. A callee's edit changes the key of every function that
//! imported it, and of no other.

use blaze_ids::{ContentId, Hasher};
use serde::{Deserialize, Serialize};

use crate::InstanceKey;

pub const IMPORTS_DOMAIN: &str = "blaze v2 imports 1";

/// What a function's `#[inline]` asks, as rustc tells LLVM.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Hint {
    None,
    Inline,
    Always,
    Never,
}

/// An argument at a call site, as far as the caller's IR knows it.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Arg {
    Unknown,
    /// An integer or pointer-free constant of `bits` bits.
    Int { bits: u8, value: u128 },
}

/// What LLVM's function attribute inference says of a function, which its callers may be
/// compiled knowing.
#[derive(Serialize, Deserialize, Clone, Copy, Default, PartialEq, Eq, Hash, Debug)]
pub struct Attrs {
    pub nounwind: bool,
    pub readonly: bool,
    pub readnone: bool,
    pub norecurse: bool,
    pub willreturn: bool,
    pub noreturn: bool,
}

/// A call site of a function.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Hash, Debug)]
pub struct Call {
    pub callee: InstanceKey,
    /// How many call sites of it the function has.
    pub count: u32,
    /// Per parameter of the callee: what every one of these call sites passes, if they agree.
    pub args: Vec<Arg>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Hash, Debug)]
pub struct Summary {
    pub key: InstanceKey,
    /// Its instructions, as LLVM counts them after its pre-link pipeline.
    pub size: u32,
    pub hint: Hint,
    pub calls: Vec<Call>,
    /// Functions whose address it takes: they can be called from where no summary says.
    pub addressed: Vec<InstanceKey>,
    pub params: u32,
    /// The parameters its body never reads, by place.
    pub unused: Vec<u32>,
    /// What LLVM infers of it alone, knowing nothing of what it calls.
    pub attrs: Attrs,
    /// What LLVM infers of it when every function of the build it calls (every
    /// [`Call::callee`]) is nounwind, readnone, willreturn and norecurse: what holds of its
    /// own instructions, for [`crate::imports::decide`] to meet with its callees'. Its
    /// `noreturn` is [`Summary::attrs`]'s, as a noreturn callee on one path says nothing of
    /// the others.
    pub body: Attrs,
    /// Whether code outside the build can call it: an exported symbol, a patchable function
    /// of a live build. Nothing is assumed of its callers.
    pub external: bool,
}

/// What one release compile is given, decided from the summaries of the whole build.
#[derive(Serialize, Deserialize, Clone, Default, PartialEq, Eq, Hash, Debug)]
pub struct Imports {
    /// The functions whose exports it may build in, at any depth, sorted.
    pub imported: Vec<InstanceKey>,
    /// Its parameters every caller passes the same constant for: it is compiled with them
    /// known.
    pub constants: Vec<(u32, Arg)>,
    /// What it may assume of the functions it calls, sorted by callee.
    pub callees: Vec<(InstanceKey, Attrs)>,
    /// Parameters of callees it need not compute, as no callee reads them: by callee, sorted.
    pub dead: Vec<(InstanceKey, Vec<u32>)>,
}

impl Imports {
    /// The store's key of the code compiled for `key` with these imports.
    pub fn blob_key(&self, key: InstanceKey) -> ContentId {
        let bytes = serde_json::to_vec(self).expect("imports are plain data");
        let mut hasher = Hasher::new(IMPORTS_DOMAIN);
        hasher.id(&key.0).part(&bytes);
        hasher.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u8) -> InstanceKey {
        InstanceKey(ContentId([n; 32]))
    }

    #[test]
    fn an_import_is_in_the_key() {
        let none = Imports::default();
        let one = Imports { imported: vec![key(2)], ..Imports::default() };
        let other = Imports { imported: vec![key(3)], ..Imports::default() };
        let keys = [none.blob_key(key(1)), one.blob_key(key(1)), other.blob_key(key(1))];
        assert_ne!(keys[0], keys[1]);
        assert_ne!(keys[1], keys[2]);
    }

    #[test]
    fn every_whole_program_fact_is_in_the_key() {
        let base = Imports::default().blob_key(key(1));
        let constant = Imports { constants: vec![(0, Arg::Int { bits: 32, value: 7 })], ..Imports::default() };
        let attrs = Imports {
            callees: vec![(key(2), Attrs { nounwind: true, ..Attrs::default() })],
            ..Imports::default()
        };
        let dead = Imports { dead: vec![(key(2), vec![1])], ..Imports::default() };
        for it in [constant, attrs, dead] {
            assert_ne!(it.blob_key(key(1)), base);
        }
    }
}

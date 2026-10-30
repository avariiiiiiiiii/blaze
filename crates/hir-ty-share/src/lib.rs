//! Shared code across generic instances (idea 2 of `docs/v2/plan/design/compile-time.md`,
//! `docs/v2/plan/lanes/fastcompile/share.md`): two instances of one generic body compile to one
//! body when nothing their code does tells their arguments apart beyond the layouts of those
//! arguments. The decision is made here, before codegen, as a key: [`share_key`].
//!
//! - The key of an instance is a BLAKE3 hash of what its monomorphized MIR depends on, read
//!   at each place where the generic body names a generic parameter: the fact of each type
//!   there ([`fact`]: its layout, its fields' facts, a pointee's size and alignment), each
//!   constant's value, each callee by its own key ([`walk`]), each drop by its glue's key
//!   ([`drop`]), each intrinsic by what rustc's definition of it reads ([`intrinsics`]), and
//!   the full identity of everything else that tells types apart ([`identity`]): `TypeId`,
//!   `type_name`, vtables, `dyn` calls, closures. A place of the generic body that names no
//!   parameter is the same in every instance and is not read.
//! - Equal keys mean equal code: the code generator puts the key in place of the arguments in
//!   the instance's code identity, so instances of one body with equal keys are one instance.
//!   An instance with no key ([`Unshared`], with why) keeps its arguments' hash, exactly as
//!   before: that is the per-instance path, not a guess.
//! - Nothing here reads a dense id into a hash: items by their [`blaze_items`] keys, types by
//!   content.
//!
//! Every query is salsa's, memoized by (instance, environment, levels): a hit is one lookup.
//! A cycle of instances (polymorphic recursion, a recursive type's drop glue) gives every
//! instance in it [`Unshared::Recursive`], the same whichever is asked first.

#![forbid(unsafe_code)]

mod drop;
mod fact;
mod identity;
mod intrinsics;
mod layout;
mod walk;

#[cfg(test)]
mod tests;

use std::fmt;

use blaze_ids::{ContentId, Fingerprint};
use hir_def::FunctionId;
use hir_ty::{
    db::HirDatabase, mir::MirOptLevel, next_solver::StoredGenericArgs,
    traits::StoredParamEnvAndCrate,
};

pub use intrinsics::{Class, classify};

/// The domain of a share key.
pub const SHARE_DOMAIN: &str = "blaze v2 share key 1";
/// The domain of a type's fact.
pub const FACT_DOMAIN: &str = "blaze v2 share fact 1";
/// The domain of a type's, constant's or arguments' full identity.
pub const IDENTITY_DOMAIN: &str = "blaze v2 share identity 1";
/// The domain of an ADT's identity: its key, interface, layout and the drop impls it reaches.
pub const ADT_DOMAIN: &str = "blaze v2 share adt 1";

/// What an instance runs: the code generator's `InstanceDef`, one for one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShareDef {
    Fn(FunctionId),
    /// An `InternedClosureId`, without its database lifetime.
    Closure(salsa::Id),
    /// An `InternedCoroutineId`: the `Future::poll` of an `async` block.
    Coroutine(salsa::Id),
    /// The same of the future an `async` closure gives when called by value.
    CoroutineByMove(salsa::Id),
    /// `drop_in_place` of the instance's only argument.
    DropGlue,
    CallOnceShim,
    VtableShim(FunctionId),
    FnPointerShim,
    ReifyShim { receiver: bool },
}

/// An instance: what it runs and its generic arguments (a closure's or coroutine's are its
/// parent's, as the code generator's are).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ShareInstance {
    pub def: ShareDef,
    pub args: StoredGenericArgs,
}

/// The MIR optimization level of the bodies the code generator compiles: `lang` for a body
/// of the library's crates, `other` for the rest. The key is of the body that is compiled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MirLevels {
    pub lang: MirOptLevel,
    pub other: MirOptLevel,
}

/// What an instance's code depends on beyond the layouts of its arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ShareKey(pub ContentId);

impl ShareKey {
    pub fn fingerprint(&self) -> Fingerprint {
        self.0.fingerprint()
    }
}

/// Why an instance has no share key and is compiled for its arguments alone.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Unshared {
    /// The instance is in a cycle of instances or types, which salsa gives this to all of.
    Recursive,
    /// A shim the code generator builds without MIR: kept per instance.
    Shim(&'static str),
    /// An intrinsic that is not in [`intrinsics`]' table, by its name.
    UnknownIntrinsic(String),
    /// The body has no MIR, or a part of it the code generator refuses too.
    NoMir(String),
    /// A type has no layout.
    NoLayout(String),
    /// A type or constant has no content: a parameter, an inference variable, an error, an
    /// alias that does not normalize, a constant made of other allocations.
    NoContent(String),
    /// A constant could not be evaluated.
    ConstEval(String),
    /// An item has no hash in [`blaze_items`].
    Unhashable(String),
    /// The monomorphized body is not the generic body's shape.
    Shape(&'static str),
}

impl fmt::Display for Unshared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unshared::Recursive => f.write_str("it is in a cycle of instances or types"),
            Unshared::Shim(what) => write!(f, "it is a {what}, which is kept per instance"),
            Unshared::UnknownIntrinsic(name) => write!(f, "it calls the unclassified intrinsic `{name}`"),
            Unshared::NoMir(what) => write!(f, "its MIR: {what}"),
            Unshared::NoLayout(what) => write!(f, "a layout: {what}"),
            Unshared::NoContent(what) => write!(f, "no content: {what}"),
            Unshared::ConstEval(what) => write!(f, "a constant: {what}"),
            Unshared::Unhashable(what) => f.write_str(what),
            Unshared::Shape(what) => write!(f, "its monomorphized body differs from the generic one in {what}"),
        }
    }
}

/// The share key of `instance` in `env`, with its body at `levels`: equal keys of two
/// instances of one body mean their code is the same.
pub fn share_key<'db>(
    db: &'db dyn HirDatabase,
    instance: &ShareInstance,
    env: &StoredParamEnvAndCrate,
    levels: MirLevels,
) -> &'db Result<ShareKey, Unshared> {
    walk::share(db, instance.clone(), env.clone(), levels)
}

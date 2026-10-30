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
//!   the full identity of everything else that tells types apart ([`identity`], the code
//!   generator's spelling of a type too): `TypeId`, `type_name`, vtables, `dyn` calls, closures,
//!   coroutines by their compiled code ([`code`]). A place of the generic body that names no
//!   parameter is the same in every instance and is not read.
//! - Equal keys mean equal code: the code generator puts the key in place of the arguments in
//!   the instance's code identity, so instances of one body with equal keys are one instance.
//!   An instance with no key ([`Unshared`], with why) keeps its arguments' hash, exactly as
//!   before: that is the per-instance path, not a guess.
//! - Nothing here reads a dense id into a hash: items by their [`blaze_items`] keys, types by
//!   content.
//!
//! Every query is salsa's, memoized by (instance, environment, options): a hit is one lookup.
//! A cycle of instances (polymorphic recursion, a recursive type's drop glue) gives every
//! instance in it [`Unshared::Recursive`], the same whichever is asked first.

#![forbid(unsafe_code)]

pub mod code;
mod drop;
mod fact;
pub mod identity;
mod intrinsics;
mod layout;
mod walk;

#[cfg(test)]
mod tests;

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

use std::fmt;

use base_db::Db;
use blaze_ids::{ContentId, Fingerprint};
use hir_def::FunctionId;
use hir_ty::{
    db::HirDatabase,
    mir::{MirOptLevel, MirOptions},
    next_solver::StoredGenericArgs,
    traits::StoredParamEnvAndCrate,
};

pub use fact::is_plain_data;
pub use intrinsics::{Class, classify};

/// The domain of a share key.
pub const SHARE_DOMAIN: &str = "blaze v2 share key 1";
/// The domain of a type's fact.
pub const FACT_DOMAIN: &str = "blaze v2 share fact 1";
/// The domain of the records of one walk of a body.
pub const WALK_DOMAIN: &str = "blaze v2 share walk 1";
/// The domain of what a call, or an argument list, is keyed by.
pub const CALLEE_DOMAIN: &str = "blaze v2 share callee 1";
/// The domain of an evaluated constant's bytes.
pub const VALUE_DOMAIN: &str = "blaze v2 share value 1";

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

/// A set of [`MirOptions`], one bit for each of the twelve (three levels, UB checks or not,
/// lifetime markers or not).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OptionsSet(u16);

const LEVELS: [MirOptLevel; 3] = [MirOptLevel::Off, MirOptLevel::Debug, MirOptLevel::Release];

impl OptionsSet {
    pub const EMPTY: OptionsSet = OptionsSet(0);

    pub const fn of(options: &[MirOptions]) -> OptionsSet {
        let mut bits = 0;
        let mut at = 0;
        while at < options.len() {
            bits |= 1 << OptionsSet::index(options[at]);
            at += 1;
        }
        OptionsSet(bits)
    }

    pub(crate) const fn index(options: MirOptions) -> u32 {
        let level = match options.level {
            MirOptLevel::Off => 0,
            MirOptLevel::Debug => 1,
            MirOptLevel::Release => 2,
        };
        level * 4 + (options.ub_checks as u32) * 2 + options.lifetime_markers as u32
    }

    pub const fn contains(self, options: MirOptions) -> bool {
        self.0 & (1 << OptionsSet::index(options)) != 0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The options, in [`MirOptions`]' order.
    pub fn iter(self) -> impl Iterator<Item = MirOptions> {
        LEVELS
            .into_iter()
            .flat_map(|level| {
                [false, true].into_iter().flat_map(move |ub_checks| {
                    [false, true]
                        .map(|lifetime_markers| MirOptions { level, ub_checks, lifetime_markers })
                })
            })
            .filter(move |&options| self.contains(options))
    }
}

/// The options the code generator may compile a body with (`hir-ty-codegen`'s
/// `Build::options` over the units of the build): `lang` for a body of the library's crates,
/// `other` for the rest. An instance's `Described` is one for every unit of the build, so the
/// key is over the body with each options here: two instances with one key are one under
/// each of them. A build whose units all compile alike gives one options each, and the key
/// costs one walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BodyOptions {
    pub lang: OptionsSet,
    pub other: OptionsSet,
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
    /// An intrinsic whose code is made for the one instance ([`Class::PerInstance`]).
    PerInstance(String),
    /// No options to key the body with: [`BodyOptions`] is empty for its crate.
    NoOptions,
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
    /// The body has a place whose type the key does not read.
    Shape(&'static str),
}

impl fmt::Display for Unshared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unshared::Recursive => f.write_str("it is in a cycle of instances or types"),
            Unshared::Shim(what) => write!(f, "it is a {what}, which is kept per instance"),
            Unshared::UnknownIntrinsic(name) => write!(f, "it calls the unclassified intrinsic `{name}`"),
            Unshared::PerInstance(name) => write!(f, "it calls `{name}`, whose code is per instance"),
            Unshared::NoOptions => f.write_str("no MIR options were given for its crate"),
            Unshared::NoMir(what) => write!(f, "its MIR: {what}"),
            Unshared::NoLayout(what) => write!(f, "a layout: {what}"),
            Unshared::NoContent(what) => write!(f, "no content: {what}"),
            Unshared::ConstEval(what) => write!(f, "a constant: {what}"),
            Unshared::Unhashable(what) => f.write_str(what),
            Unshared::Shape(what) => write!(f, "its body has {what}, whose type the key does not read"),
        }
    }
}

/// The share key of `instance` in `env`, with its body under each of `options`: equal keys of
/// two instances of one body mean their code is the same.
pub fn share_key<'db>(
    db: &'db Db,
    instance: &ShareInstance,
    env: &StoredParamEnvAndCrate,
    options: BodyOptions,
) -> &'db Result<ShareKey, Unshared> {
    hir_ty::attach_db(db, || walk::share(db, instance.clone(), env.clone(), options)) // clone: the query's keys, reference counts
}

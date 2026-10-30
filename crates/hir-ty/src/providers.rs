//! The queries hir-ty declares and a crate above it computes: MIR (hir-ty-mir) and printing a
//! type as its path (hir-ide).
//!
//! hir-ty, hir-ty-borrowck and blaze-items read MIR, whose code is in hir-ty-mir, above them.
//! The memos are declared here, so a hit is a static call from every crate; only a miss calls
//! through the table, as only a miss calls `execute` in rustc. The crate with the code
//! registers its table with `inventory::submit!`, which the linker keeps in every binary that
//! links that crate: nothing installs anything, and a binary with no table, or two, panics on
//! the first miss, naming what to link.

use std::sync::OnceLock;

use base_db::Db;
use hir_def::ModuleId;

use crate::{
    InferBodyId,
    db::{InternedClosureId, InternedCoroutineId, InternedOpaqueTyId},
    mir::{MirBody, MirEvalError, MirLowerError, MirOptions},
    next_solver::{Allocation, GenericArgs, StoredGenericArgs, Ty},
    traits::{ParamEnvAndCrate, StoredParamEnvAndCrate},
};

type Built<'db> = Result<MirBody<'db>, MirLowerError<'db>>;

/// hir-ty-mir's code for the MIR queries, one field per query, in its signatures.
pub struct Mir {
    pub mir_body: for<'db> fn(&'db Db, InferBodyId<'db>) -> Built<'db>,
    pub mir_body_for_closure: for<'db> fn(&'db Db, InternedClosureId<'db>) -> Built<'db>,
    pub mir_body_for_coroutine: for<'db> fn(&'db Db, InternedCoroutineId<'db>) -> Built<'db>,
    pub mir_body_for_coroutine_by_move:
        for<'db> fn(&'db Db, InternedCoroutineId<'db>) -> Built<'db>,

    pub monomorphized_mir_body: for<'db> fn(
        &'db Db,
        InferBodyId<'db>,
        StoredGenericArgs,
        StoredParamEnvAndCrate,
    ) -> Built<'db>,
    pub monomorphized_mir_body_for_closure: for<'db> fn(
        &'db Db,
        InternedClosureId<'db>,
        StoredGenericArgs,
        StoredParamEnvAndCrate,
    ) -> Built<'db>,
    pub monomorphized_mir_body_for_coroutine: for<'db> fn(
        &'db Db,
        InternedCoroutineId<'db>,
        StoredGenericArgs,
        StoredParamEnvAndCrate,
    ) -> Built<'db>,
    pub monomorphized_mir_body_for_coroutine_by_move: for<'db> fn(
        &'db Db,
        InternedCoroutineId<'db>,
        StoredGenericArgs,
        StoredParamEnvAndCrate,
    ) -> Built<'db>,

    pub runtime_mir_body: for<'db> fn(&'db Db, InferBodyId<'db>) -> Built<'db>,
    pub runtime_mir_body_for_closure: for<'db> fn(&'db Db, InternedClosureId<'db>) -> Built<'db>,
    pub runtime_mir_body_for_coroutine:
        for<'db> fn(&'db Db, InternedCoroutineId<'db>) -> Built<'db>,
    pub runtime_mir_body_for_coroutine_by_move:
        for<'db> fn(&'db Db, InternedCoroutineId<'db>) -> Built<'db>,
    pub monomorphized_runtime_mir_body: for<'db> fn(
        &'db Db,
        InferBodyId<'db>,
        StoredGenericArgs,
        StoredParamEnvAndCrate,
    ) -> Built<'db>,
    pub monomorphized_runtime_mir_body_for_closure: for<'db> fn(
        &'db Db,
        InternedClosureId<'db>,
        StoredGenericArgs,
        StoredParamEnvAndCrate,
    ) -> Built<'db>,
    pub monomorphized_runtime_mir_body_for_coroutine: for<'db> fn(
        &'db Db,
        InternedCoroutineId<'db>,
        StoredGenericArgs,
        StoredParamEnvAndCrate,
    ) -> Built<'db>,
    pub monomorphized_runtime_mir_body_for_coroutine_by_move: for<'db> fn(
        &'db Db,
        InternedCoroutineId<'db>,
        StoredGenericArgs,
        StoredParamEnvAndCrate,
    ) -> Built<'db>,

    pub optimized_mir_body: for<'db> fn(&'db Db, InferBodyId<'db>, MirOptions) -> Built<'db>,
    pub optimized_mir_body_for_closure:
        for<'db> fn(&'db Db, InternedClosureId<'db>, MirOptions) -> Built<'db>,
    pub optimized_mir_body_for_coroutine:
        for<'db> fn(&'db Db, InternedCoroutineId<'db>, MirOptions) -> Built<'db>,
    pub optimized_mir_body_for_coroutine_by_move:
        for<'db> fn(&'db Db, InternedCoroutineId<'db>, MirOptions) -> Built<'db>,
    pub monomorphized_optimized_mir_body: for<'db> fn(
        &'db Db,
        InferBodyId<'db>,
        MirOptions,
        StoredGenericArgs,
        StoredParamEnvAndCrate,
    ) -> Built<'db>,
    pub monomorphized_optimized_mir_body_for_closure: for<'db> fn(
        &'db Db,
        InternedClosureId<'db>,
        MirOptions,
        StoredGenericArgs,
        StoredParamEnvAndCrate,
    ) -> Built<'db>,
    pub monomorphized_optimized_mir_body_for_coroutine: for<'db> fn(
        &'db Db,
        InternedCoroutineId<'db>,
        MirOptions,
        StoredGenericArgs,
        StoredParamEnvAndCrate,
    ) -> Built<'db>,
    pub monomorphized_optimized_mir_body_for_coroutine_by_move: for<'db> fn(
        &'db Db,
        InternedCoroutineId<'db>,
        MirOptions,
        StoredGenericArgs,
        StoredParamEnvAndCrate,
    ) -> Built<'db>,

    /// The value running `body` returns.
    pub interpret_mir: for<'db> fn(
        &'db Db,
        &'db MirBody<'db>,
        bool,
        Option<ParamEnvAndCrate<'db>>,
    ) -> Result<Allocation<'db>, MirEvalError<'db>>,
    pub opaque_of_impl: for<'db> fn(
        &'db Db,
        ParamEnvAndCrate<'db>,
        InternedOpaqueTyId<'db>,
        GenericArgs<'db>,
    ) -> Option<Ty<'db>>,
}

/// hir-ide's code for printing a type as source.
pub struct Display {
    /// The type's path as `std::any::type_name` gives it: `None` when the type has no source
    /// path to print, which the caller makes an error.
    pub type_name: for<'db> fn(&'db Db, Ty<'db>, ModuleId) -> Option<String>,
}

inventory::collect!(Mir);
inventory::collect!(Display);

/// The linked MIR table.
pub fn mir() -> &'static Mir {
    static ONE: OnceLock<&'static Mir> = OnceLock::new();
    ONE.get_or_init(|| one::<Mir>("hir-ty-mir"))
}

/// The linked display table.
pub fn display() -> &'static Display {
    static ONE: OnceLock<&'static Display> = OnceLock::new();
    ONE.get_or_init(|| one::<Display>("hir-ide"))
}

fn one<T: inventory::Collect>(by: &str) -> &'static T {
    let mut all = inventory::iter::<T>();
    let first = all.next().unwrap_or_else(|| {
        panic!("no `{}` is linked: link {by} (`use hir as _;`)", std::any::type_name::<T>())
    });
    assert!(all.next().is_none(), "two `{}` are linked", std::any::type_name::<T>());
    first
}

/// How many of the lowered bodies salsa keeps the value of once a revision is over; 0 keeps
/// every one.
pub fn set_mir_capacity(db: &mut Db, capacity: usize) {
    mir_body_query::set_lru_capacity(db, capacity);
    mir_body_for_closure_query::set_lru_capacity(db, capacity);
    mir_body_for_coroutine_query::set_lru_capacity(db, capacity);
    mir_body_for_coroutine_by_move_query::set_lru_capacity(db, capacity);
}

/// How many of the runtime bodies (and their monomorphized copies) salsa keeps the value of once
/// a revision is over; 0 keeps every one. A body let go is lowered again when it is next asked
/// for, and what depends on it cannot be backdated then.
pub fn set_runtime_mir_capacity(db: &mut Db, capacity: usize) {
    runtime_mir_body_query::set_lru_capacity(db, capacity);
    runtime_mir_body_for_closure_query::set_lru_capacity(db, capacity);
    runtime_mir_body_for_coroutine_query::set_lru_capacity(db, capacity);
    runtime_mir_body_for_coroutine_by_move_query::set_lru_capacity(db, capacity);
    monomorphized_runtime_mir_body_query::set_lru_capacity(db, capacity);
    monomorphized_runtime_mir_body_for_closure_query::set_lru_capacity(db, capacity);
    monomorphized_runtime_mir_body_for_coroutine_query::set_lru_capacity(db, capacity);
    monomorphized_runtime_mir_body_for_coroutine_by_move_query::set_lru_capacity(db, capacity);
}

// A body that needs itself to be built is an error, not a fixpoint: one per arity of the keys.

fn mir_loop1<'db, A>(_: &'db Db, _: salsa::Id, _: A) -> Built<'db> {
    Err(MirLowerError::Loop)
}

fn mir_loop2<'db, A, B>(_: &'db Db, _: salsa::Id, _: A, _: B) -> Built<'db> {
    Err(MirLowerError::Loop)
}

fn mir_loop3<'db, A, B, C>(_: &'db Db, _: salsa::Id, _: A, _: B, _: C) -> Built<'db> {
    Err(MirLowerError::Loop)
}

fn mir_loop4<'db, A, B, C, D>(_: &'db Db, _: salsa::Id, _: A, _: B, _: C, _: D) -> Built<'db> {
    Err(MirLowerError::Loop)
}

// Built MIR.

#[salsa::tracked(returns(as_ref), cycle_result = mir_loop1, lru = 0)]
pub fn mir_body_query<'db>(db: &'db Db, def: InferBodyId<'db>) -> Built<'db> {
    (mir().mir_body)(db, def)
}

#[salsa::tracked(returns(as_ref), cycle_result = mir_loop1, lru = 0)]
pub fn mir_body_for_closure_query<'db>(db: &'db Db, closure: InternedClosureId<'db>) -> Built<'db> {
    (mir().mir_body_for_closure)(db, closure)
}

/// The body of an `async` block, as the `Future::poll` of the block's type.
#[salsa::tracked(returns(as_ref), cycle_result = mir_loop1, lru = 0)]
pub fn mir_body_for_coroutine_query<'db>(
    db: &'db Db,
    coroutine: InternedCoroutineId<'db>,
) -> Built<'db> {
    (mir().mir_body_for_coroutine)(db, coroutine)
}

/// The body of the `async` block of an `async` closure that is called by value though it could
/// be called again: the future that makes owns what the closure captured, where the block
/// itself borrows from the closure.
#[salsa::tracked(returns(as_ref), cycle_result = mir_loop1, lru = 0)]
pub fn mir_body_for_coroutine_by_move_query<'db>(
    db: &'db Db,
    coroutine: InternedCoroutineId<'db>,
) -> Built<'db> {
    (mir().mir_body_for_coroutine_by_move)(db, coroutine)
}

// Built MIR, monomorphized.

#[salsa::tracked(returns(as_ref), cycle_result = mir_loop3)]
pub fn monomorphized_mir_body_query<'db>(
    db: &'db Db,
    owner: InferBodyId<'db>,
    subst: StoredGenericArgs,
    trait_env: StoredParamEnvAndCrate,
) -> Built<'db> {
    (mir().monomorphized_mir_body)(db, owner, subst, trait_env)
}

#[salsa::tracked(returns(as_ref), cycle_result = mir_loop3)]
pub fn monomorphized_mir_body_for_closure_query<'db>(
    db: &'db Db,
    closure: InternedClosureId<'db>,
    subst: StoredGenericArgs,
    trait_env: StoredParamEnvAndCrate,
) -> Built<'db> {
    (mir().monomorphized_mir_body_for_closure)(db, closure, subst, trait_env)
}

#[salsa::tracked(returns(as_ref), cycle_result = mir_loop3)]
pub fn monomorphized_mir_body_for_coroutine_query<'db>(
    db: &'db Db,
    coroutine: InternedCoroutineId<'db>,
    subst: StoredGenericArgs,
    trait_env: StoredParamEnvAndCrate,
) -> Built<'db> {
    (mir().monomorphized_mir_body_for_coroutine)(db, coroutine, subst, trait_env)
}

#[salsa::tracked(returns(as_ref), cycle_result = mir_loop3)]
pub fn monomorphized_mir_body_for_coroutine_by_move_query<'db>(
    db: &'db Db,
    coroutine: InternedCoroutineId<'db>,
    subst: StoredGenericArgs,
    trait_env: StoredParamEnvAndCrate,
) -> Built<'db> {
    (mir().monomorphized_mir_body_for_coroutine_by_move)(db, coroutine, subst, trait_env)
}

// Runtime MIR.

#[salsa::tracked(returns(as_ref), cycle_result = mir_loop1, lru = 0)]
pub fn runtime_mir_body_query<'db>(db: &'db Db, def: InferBodyId<'db>) -> Built<'db> {
    (mir().runtime_mir_body)(db, def)
}

#[salsa::tracked(returns(as_ref), cycle_result = mir_loop1, lru = 0)]
pub fn runtime_mir_body_for_closure_query<'db>(
    db: &'db Db,
    closure: InternedClosureId<'db>,
) -> Built<'db> {
    (mir().runtime_mir_body_for_closure)(db, closure)
}

#[salsa::tracked(returns(as_ref), cycle_result = mir_loop1, lru = 0)]
pub fn runtime_mir_body_for_coroutine_query<'db>(
    db: &'db Db,
    coroutine: InternedCoroutineId<'db>,
) -> Built<'db> {
    (mir().runtime_mir_body_for_coroutine)(db, coroutine)
}

#[salsa::tracked(returns(as_ref), cycle_result = mir_loop1, lru = 0)]
pub fn runtime_mir_body_for_coroutine_by_move_query<'db>(
    db: &'db Db,
    coroutine: InternedCoroutineId<'db>,
) -> Built<'db> {
    (mir().runtime_mir_body_for_coroutine_by_move)(db, coroutine)
}

#[salsa::tracked(returns(as_ref), cycle_result = mir_loop3, lru = 0)]
pub fn monomorphized_runtime_mir_body_query<'db>(
    db: &'db Db,
    def: InferBodyId<'db>,
    subst: StoredGenericArgs,
    trait_env: StoredParamEnvAndCrate,
) -> Built<'db> {
    (mir().monomorphized_runtime_mir_body)(db, def, subst, trait_env)
}

#[salsa::tracked(returns(as_ref), cycle_result = mir_loop3, lru = 0)]
pub fn monomorphized_runtime_mir_body_for_closure_query<'db>(
    db: &'db Db,
    closure: InternedClosureId<'db>,
    subst: StoredGenericArgs,
    trait_env: StoredParamEnvAndCrate,
) -> Built<'db> {
    (mir().monomorphized_runtime_mir_body_for_closure)(db, closure, subst, trait_env)
}

#[salsa::tracked(returns(as_ref), cycle_result = mir_loop3, lru = 0)]
pub fn monomorphized_runtime_mir_body_for_coroutine_query<'db>(
    db: &'db Db,
    coroutine: InternedCoroutineId<'db>,
    subst: StoredGenericArgs,
    trait_env: StoredParamEnvAndCrate,
) -> Built<'db> {
    (mir().monomorphized_runtime_mir_body_for_coroutine)(db, coroutine, subst, trait_env)
}

#[salsa::tracked(returns(as_ref), cycle_result = mir_loop3, lru = 0)]
pub fn monomorphized_runtime_mir_body_for_coroutine_by_move_query<'db>(
    db: &'db Db,
    coroutine: InternedCoroutineId<'db>,
    subst: StoredGenericArgs,
    trait_env: StoredParamEnvAndCrate,
) -> Built<'db> {
    (mir().monomorphized_runtime_mir_body_for_coroutine_by_move)(db, coroutine, subst, trait_env)
}

// Optimized MIR.

#[salsa::tracked(returns(as_ref), cycle_result = mir_loop2)]
pub fn optimized_mir_body_query<'db>(
    db: &'db Db,
    def: InferBodyId<'db>,
    options: MirOptions,
) -> Built<'db> {
    (mir().optimized_mir_body)(db, def, options)
}

#[salsa::tracked(returns(as_ref), cycle_result = mir_loop2)]
pub fn optimized_mir_body_for_closure_query<'db>(
    db: &'db Db,
    closure: InternedClosureId<'db>,
    options: MirOptions,
) -> Built<'db> {
    (mir().optimized_mir_body_for_closure)(db, closure, options)
}

/// The body of an `async` block's `poll` with `options`: of the passes, only the UB checks run
/// on a coroutine body.
#[salsa::tracked(returns(as_ref), cycle_result = mir_loop2)]
pub fn optimized_mir_body_for_coroutine_query<'db>(
    db: &'db Db,
    coroutine: InternedCoroutineId<'db>,
    options: MirOptions,
) -> Built<'db> {
    (mir().optimized_mir_body_for_coroutine)(db, coroutine, options)
}

#[salsa::tracked(returns(as_ref), cycle_result = mir_loop2)]
pub fn optimized_mir_body_for_coroutine_by_move_query<'db>(
    db: &'db Db,
    coroutine: InternedCoroutineId<'db>,
    options: MirOptions,
) -> Built<'db> {
    (mir().optimized_mir_body_for_coroutine_by_move)(db, coroutine, options)
}

#[salsa::tracked(returns(as_ref), cycle_result = mir_loop4)]
pub fn monomorphized_optimized_mir_body_query<'db>(
    db: &'db Db,
    def: InferBodyId<'db>,
    options: MirOptions,
    subst: StoredGenericArgs,
    trait_env: StoredParamEnvAndCrate,
) -> Built<'db> {
    (mir().monomorphized_optimized_mir_body)(db, def, options, subst, trait_env)
}

#[salsa::tracked(returns(as_ref), cycle_result = mir_loop4)]
pub fn monomorphized_optimized_mir_body_for_closure_query<'db>(
    db: &'db Db,
    closure: InternedClosureId<'db>,
    options: MirOptions,
    subst: StoredGenericArgs,
    trait_env: StoredParamEnvAndCrate,
) -> Built<'db> {
    (mir().monomorphized_optimized_mir_body_for_closure)(db, closure, options, subst, trait_env)
}

#[salsa::tracked(returns(as_ref), cycle_result = mir_loop4)]
pub fn monomorphized_optimized_mir_body_for_coroutine_query<'db>(
    db: &'db Db,
    coroutine: InternedCoroutineId<'db>,
    options: MirOptions,
    subst: StoredGenericArgs,
    trait_env: StoredParamEnvAndCrate,
) -> Built<'db> {
    (mir().monomorphized_optimized_mir_body_for_coroutine)(db, coroutine, options, subst, trait_env)
}

#[salsa::tracked(returns(as_ref), cycle_result = mir_loop4)]
pub fn monomorphized_optimized_mir_body_for_coroutine_by_move_query<'db>(
    db: &'db Db,
    coroutine: InternedCoroutineId<'db>,
    options: MirOptions,
    subst: StoredGenericArgs,
    trait_env: StoredParamEnvAndCrate,
) -> Built<'db> {
    let by_move = mir().monomorphized_optimized_mir_body_for_coroutine_by_move;
    by_move(db, coroutine, options, subst, trait_env)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Unlinked;
    inventory::collect!(Unlinked);

    struct Once(u32);
    inventory::collect!(Once);
    inventory::submit! { Once(7) }

    struct Twice;
    inventory::collect!(Twice);
    inventory::submit! { Twice }
    inventory::submit! { Twice }

    #[test]
    fn a_table_nobody_registered_names_what_to_link() {
        let panic = std::panic::catch_unwind(|| one::<Unlinked>("the-crate")).unwrap_err();
        let message = panic.downcast_ref::<String>().unwrap();
        assert!(message.contains("link the-crate"), "{message}");
        assert!(message.contains("Unlinked"), "{message}");
    }

    #[test]
    fn a_table_registered_once_is_found() {
        assert_eq!(one::<Once>("the-crate").0, 7);
    }

    #[test]
    fn a_table_registered_twice_is_refused() {
        let panic = std::panic::catch_unwind(|| one::<Twice>("the-crate")).unwrap_err();
        let message = panic.downcast_ref::<String>().unwrap();
        assert!(message.contains("two"), "{message}");
    }
}

//! The code of hir-ty's MIR queries (`hir_ty::providers`), registered once for every binary
//! that links this crate.

use base_db::Db;
use hir_ty::{
    mir::{MirBody, MirEvalError},
    next_solver::Allocation,
    providers::Mir,
    traits::ParamEnvAndCrate,
};

use super::{eval, lower, monomorphization, optimize, transform};

inventory::submit! {
    Mir {
        mir_body: lower::compute_mir_body,
        mir_body_for_closure: lower::compute_mir_body_for_closure,
        mir_body_for_coroutine: lower::compute_mir_body_for_coroutine,
        mir_body_for_coroutine_by_move: lower::compute_mir_body_for_coroutine_by_move,

        monomorphized_mir_body: monomorphization::compute_monomorphized_mir_body,
        monomorphized_mir_body_for_closure:
            monomorphization::compute_monomorphized_mir_body_for_closure,
        monomorphized_mir_body_for_coroutine:
            monomorphization::compute_monomorphized_mir_body_for_coroutine,
        monomorphized_mir_body_for_coroutine_by_move:
            monomorphization::compute_monomorphized_mir_body_for_coroutine_by_move,

        runtime_mir_body: transform::compute_runtime_mir_body,
        runtime_mir_body_for_closure: transform::compute_runtime_mir_body_for_closure,
        runtime_mir_body_for_coroutine: transform::compute_runtime_mir_body_for_coroutine,
        runtime_mir_body_for_coroutine_by_move:
            transform::compute_runtime_mir_body_for_coroutine_by_move,
        monomorphized_runtime_mir_body: transform::compute_monomorphized_runtime_mir_body,
        monomorphized_runtime_mir_body_for_closure:
            transform::compute_monomorphized_runtime_mir_body_for_closure,
        monomorphized_runtime_mir_body_for_coroutine:
            transform::compute_monomorphized_runtime_mir_body_for_coroutine,
        monomorphized_runtime_mir_body_for_coroutine_by_move:
            transform::compute_monomorphized_runtime_mir_body_for_coroutine_by_move,

        optimized_mir_body: optimize::compute_optimized_mir_body,
        optimized_mir_body_for_closure: optimize::compute_optimized_mir_body_for_closure,
        optimized_mir_body_for_coroutine: optimize::compute_optimized_mir_body_for_coroutine,
        optimized_mir_body_for_coroutine_by_move:
            optimize::compute_optimized_mir_body_for_coroutine_by_move,
        monomorphized_optimized_mir_body: optimize::compute_monomorphized_optimized_mir_body,
        monomorphized_optimized_mir_body_for_closure:
            optimize::compute_monomorphized_optimized_mir_body_for_closure,
        monomorphized_optimized_mir_body_for_coroutine:
            optimize::compute_monomorphized_optimized_mir_body_for_coroutine,
        monomorphized_optimized_mir_body_for_coroutine_by_move:
            optimize::compute_monomorphized_optimized_mir_body_for_coroutine_by_move,

        interpret_mir,
        opaque_of_impl: monomorphization::opaque_of_impl,
    }
}

/// The value running `body` returns; what it printed is the interpreter's callers'.
fn interpret_mir<'db>(
    db: &'db Db,
    body: &'db MirBody<'db>,
    assert_placeholder_ty_is_unused: bool,
    trait_env: Option<ParamEnvAndCrate<'db>>,
) -> Result<Allocation<'db>, MirEvalError<'db>> {
    eval::interpret_mir(db, body, assert_placeholder_ty_is_unused, trait_env)
        .and_then(|(value, _)| value)
}

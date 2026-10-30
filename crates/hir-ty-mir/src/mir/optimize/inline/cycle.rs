//! rustc's `mir_callgraph_cyclic` and `mir_inliner_callees`
//! (`compiler/rustc_mir_transform/src/inline/cycle.rs`): the bodies of the caller's crate that
//! may call the caller back, following each call as it resolves once the generic arguments of
//! the body it is in are known. The inliner inlines none of them into the caller, so the
//! optimized body of a function never asks for itself.

use std::hash::Hash;

use hir_def::{FunctionId, HasModule, nameres::crate_def_map, signatures::FunctionSignature};
use rustc_hash::{FxHashMap, FxHashSet};
use rustc_type_ir::inherent::GenericArgs as _;
use smallvec::SmallVec;

use crate::{
    db::{HirDatabase, InternedClosureId},
    mir::{InlinedBody, MirBody, TerminatorKind, transform::Ctx},
    next_solver::{GenericArgs, StoredGenericArgs},
    traits::ParamEnvAndCrate,
};

use super::{fn_def, instantiate, resolve, spans::owner_of};

/// The functions `body` calls by name, each once with its generic arguments, in the order it
/// first calls them: rustc's `mir_inliner_callees`, of the runtime body. Of
/// `const_eval_select` it is the function called at runtime.
fn callees_of(db: &dyn HirDatabase, body: &MirBody<'_>) -> Vec<(FunctionId, StoredGenericArgs)> {
    let mut seen = FxHashSet::default();
    let mut callees = Vec::new();
    for (_, block) in body.basic_blocks.iter() {
        let Some(TerminatorKind::Call { func, args, .. }) = block.terminator.as_ref().map(|it| &it.kind)
        else {
            continue;
        };
        let Some(mut call) = fn_def(body, func) else { continue };
        if FunctionSignature::is_intrinsic(db, call.0)
            && FunctionSignature::of(db, call.0).name.as_str() == "const_eval_select"
        {
            match args.get(2).and_then(|it| fn_def(body, it)) {
                Some(runtime) => call = runtime,
                None => continue,
            }
        }
        if seen.insert(call) {
            callees.push((call.0, call.1.store()));
        }
    }
    callees
}

#[salsa::tracked(returns(ref))]
pub(super) fn inliner_callees_of_fn(
    db: &dyn HirDatabase,
    func: FunctionId,
) -> Vec<(FunctionId, StoredGenericArgs)> {
    match db.runtime_mir_body(func.into()) {
        Ok(body) => callees_of(db, body),
        Err(_) => Vec::new(),
    }
}

#[salsa::tracked(returns(ref))]
pub(super) fn inliner_callees_of_closure<'db>(
    db: &'db dyn HirDatabase,
    closure: InternedClosureId<'db>,
) -> Vec<(FunctionId, StoredGenericArgs)> {
    match db.runtime_mir_body_for_closure(closure) {
        Ok(body) => callees_of(db, body),
        Err(_) => Vec::new(),
    }
}

fn inliner_callees<'db>(
    db: &'db dyn HirDatabase,
    body: InlinedBody<'db>,
) -> &'db [(FunctionId, StoredGenericArgs)] {
    match body {
        InlinedBody::Fn(func) => inliner_callees_of_fn(db, func),
        InlinedBody::Closure(closure) => inliner_callees_of_closure(db, closure),
    }
}

#[salsa::tracked(returns(ref))]
pub(super) fn callgraph_cyclic_fn<'db>(
    db: &'db dyn HirDatabase,
    root: FunctionId,
) -> Vec<InlinedBody<'db>> {
    cyclic(db, InlinedBody::Fn(root))
}

#[salsa::tracked(returns(ref))]
pub(super) fn callgraph_cyclic_closure<'db>(
    db: &'db dyn HirDatabase,
    root: InternedClosureId<'db>,
) -> Vec<InlinedBody<'db>> {
    cyclic(db, InlinedBody::Closure(root))
}

/// The bodies of `root`'s crate that a call of `root`'s body may lead back to `root` through.
pub(super) fn callgraph_cyclic<'db>(
    db: &'db dyn HirDatabase,
    root: InlinedBody<'db>,
) -> &'db [InlinedBody<'db>] {
    match root {
        InlinedBody::Fn(func) => callgraph_cyclic_fn(db, func),
        InlinedBody::Closure(closure) => callgraph_cyclic_closure(db, closure),
    }
}

/// Whether the inliner may look into `body`: rustc's `should_recurse`.
fn has_mir(db: &dyn HirDatabase, body: InlinedBody<'_>) -> bool {
    match body {
        InlinedBody::Fn(func) => {
            FunctionSignature::of(db, func).has_body() && !FunctionSignature::is_intrinsic(db, func)
        }
        InlinedBody::Closure(_) => true,
    }
}

fn cyclic<'db>(db: &'db dyn HirDatabase, root: InlinedBody<'db>) -> Vec<InlinedBody<'db>> {
    if !has_mir(db, root) {
        return Vec::new();
    }
    let owner = owner_of(db, root);
    let krate = owner.krate(db);
    let ctx = Ctx::new(db, owner);
    let env = ParamEnvAndCrate { param_env: ctx.env, krate };
    let identity = GenericArgs::identity_for_item(ctx.infcx.interner, owner.generic_def(db).into());
    let limit = crate_def_map(db, krate).recursion_limit() as usize / 2;
    let callees = |(body, args): (InlinedBody<'db>, GenericArgs<'db>)| {
        let mut out: SmallVec<[((InlinedBody<'db>, GenericArgs<'db>), bool); 8]> = SmallVec::new();
        for (func, callee_args) in inliner_callees(db, body) {
            let Some(callee_args) = instantiate(&ctx, callee_args.as_ref(), args) else { continue };
            let Some(callee) = resolve(&ctx, env, *func, callee_args) else { continue };
            out.push((callee, has_mir(db, callee.0)));
        }
        out
    };
    let local = |body: InlinedBody<'db>| owner_of(db, body).krate(db) == krate;
    walk((root, identity), root, limit, |node| node.0, callees, local)
}

struct Frame<N> {
    node: N,
    callees: SmallVec<[(N, bool); 8]>,
    next: usize,
    found: bool,
}

/// rustc's `process` from `start`, without recursion: the bodies a path from `start` to
/// `target` goes through, of those `local` says to keep. A node is an instance, `body_of` its
/// body; `callees` gives the instances an instance calls, each with whether it has MIR to
/// follow. Each instance is followed once, and each body at most `limit` times, past which it
/// is taken to lead to `target`, as rustc takes it.
pub(super) fn walk<N, B, C>(
    start: N,
    target: B,
    limit: usize,
    body_of: impl Fn(N) -> B,
    mut callees: impl FnMut(N) -> C,
    mut local: impl FnMut(B) -> bool,
) -> Vec<B>
where
    N: Copy + Eq + Hash,
    B: Copy + Eq + Hash,
    C: Into<SmallVec<[(N, bool); 8]>>,
{
    let mut seen: FxHashSet<N> = FxHashSet::default();
    let mut followed: FxHashMap<B, usize> = FxHashMap::default();
    let mut involved: Vec<B> = Vec::new();
    let mut kept: FxHashSet<B> = FxHashSet::default();
    let mut keep = |body: B, involved: &mut Vec<B>| {
        if local(body) && kept.insert(body) {
            involved.push(body);
        }
    };
    let mut stack = vec![Frame { node: start, callees: callees(start).into(), next: 0, found: false }];
    while let Some(frame) = stack.last_mut() {
        let Some(&(callee, has_mir)) = frame.callees.get(frame.next) else {
            let Some(done) = stack.pop() else { break };
            if done.found && let Some(caller) = stack.last_mut() {
                keep(body_of(done.node), &mut involved);
                caller.found = true;
            }
            continue;
        };
        frame.next += 1;
        let body = body_of(callee);
        if body == target {
            frame.found = true;
        }
        if !has_mir || !seen.insert(callee) {
            continue;
        }
        let count = followed.entry(body).or_default();
        if *count <= limit {
            *count += 1;
            let callees = callees(callee).into();
            stack.push(Frame { node: callee, callees, next: 0, found: false });
        } else {
            frame.found = true;
            keep(body, &mut involved);
        }
    }
    involved
}

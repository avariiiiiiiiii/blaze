//! `unconditional_recursion`: a function every way through which calls itself, or a `Drop` impl
//! every way through which drops a value of its type.
//!
//! Follows `compiler/rustc_mir_transform/src/check_call_recursion.rs` (`CheckCallRecursion`
//! on the built MIR, `CheckDropRecursion` after drop elaboration, `Search` over
//! `TriColorDepthFirstSearch` of `compiler/rustc_data_structures/src/graph/iterate/mod.rs`)
//! and `compiler/rustc_mir_transform/src/diagnostics.rs` (`UnconditionalRecursion`) (MIT or
//! Apache-2.0).
//!
//! The MIR here has no cleanup edge where rustc's goes only to `UnwindResume` (lowering
//! makes none when nothing is dropped, the runtime passes remove the no-op landing pads
//! first), so a call that does not return and has no cleanup block unwinds out of the
//! function, as rustc's through its `UnwindResume`.

use either::Either;
use hir::{HasSource as _, InFile};
use hir_def::{CallableDefId, FunctionId, GenericDefId, ItemContainerId, Lookup, generics::GenericParams};
use hir_ty::{
    ParamEnvAndCrate,
    mir::{BasicBlockId, MirBody, Operand, OperandKind, StoredPlace, Terminator, TerminatorKind},
    next_solver::{
        CallableIdWrapper, DbInterner, GenericArgKind, GenericArgs, ParamEnv, Ty, TyKind, TypingMode,
        infer::{DbInternerInferExt, InferCtxt},
    },
};
use rustc_hash::FxHashSet;
use rustc_type_ir::{
    TypeVisitableExt as _,
    inherent::{GenericArgs as _, IntoKind as _, SliceLike as _},
};
use syntax::{AstNode as _, ast};

use super::BodyCx;
use crate::{
    cx::signature_range,
    lower::{Found, Phase},
    model::{Diag, Span},
};

/// `CheckCallRecursion`: the calls of the function to itself, in `mir_promoted`.
pub(super) fn calls(b: &BodyCx<'_, '_>) -> Vec<Found> {
    let Some(func) = function(b) else { return Vec::new() };
    let Some(mir) = b.mir() else { return Vec::new() };
    let types = Types::new(b, func);
    let trait_args = trait_args(b, func);
    let classifier = |mir: &MirBody<'_>, terminator: &Terminator| types.calls_itself(mir, terminator, &trait_args);
    lint(b, mir, classifier).into_iter().collect()
}

/// `CheckDropRecursion`: the drops of a value of the type a `Drop` impl's `drop` drops, after
/// drop elaboration.
pub(super) fn drops(b: &BodyCx<'_, '_>) -> Vec<Found> {
    if !b.elaborated {
        return Vec::new();
    }
    let Some(func) = function(b) else { return Vec::new() };
    let types = Types::new(b, func);
    let Some(drop_for) = types.drop_for(func) else { return Vec::new() };
    let Some(mir) = b.runtime_mir() else { return Vec::new() };
    let classifier = |mir: &MirBody<'_>, terminator: &Terminator| types.drops(mir, terminator, drop_for);
    lint(b, mir, classifier).into_iter().collect()
}

/// A function or method whose body type checked: rustc builds no MIR of a body that did not.
fn function(b: &BodyCx<'_, '_>) -> Option<FunctionId> {
    match b.owner {
        hir_def::DefWithBodyId::FunctionId(it) if !b.tainted => Some(it),
        _ => None,
    }
}

/// `check_recursion`: the lint at the function's signature with the recursive call sites, when
/// no way through the body returns.
fn lint(b: &BodyCx<'_, '_>, mir: &MirBody<'_>, recursive: impl Fn(&MirBody<'_>, &Terminator) -> bool) -> Option<Found> {
    let calls = search(mir, &recursive)?;
    if calls.is_empty() {
        return None;
    }
    let mut sites: Vec<Span> = calls.into_iter().filter_map(|it| b.mir_span(it)).collect();
    sites.sort_by(|a, b| (&a.file, a.lo, a.hi).cmp(&(&b.file, b.lo, b.hi)));
    let at = b.owner_node();
    let node = b.cx.syntax(at);
    let range = ast::Fn::cast(node.value.clone()).map_or(node.value.text_range(), |it| signature_range(&it));
    let span = b.cx.span(InFile::new(node.file_id, range));
    let mut diag = Diag::lint("unconditional_recursion", "function cannot return without recursing", span.clone());
    diag.span_label(span, "cannot return without recursing");
    for site in sites {
        diag.span_label(site, "recursive call site");
    }
    diag.help("a `loop` may express intention better if this is on purpose");
    Some(b.lint(diag, Phase::Mir, at))
}

/// rustc's `Search` from the start block: `None` when a way through returns or loops (its
/// `NonRecursive`), else the recursive terminators reached, by where they are.
fn search(mir: &MirBody<'_>, recursive: &impl Fn(&MirBody<'_>, &Terminator) -> bool) -> Option<Vec<hir_ty::mir::MirSpan>> {
    let mut visited: FxHashSet<BasicBlockId> = FxHashSet::default();
    let mut settled: FxHashSet<BasicBlockId> = FxHashSet::default();
    let mut stack = vec![(mir.start_block, false)];
    let mut calls = Vec::new();
    while let Some((block, settle)) = stack.pop() {
        let terminator = mir.basic_blocks[block].terminator.as_ref()?;
        if settle {
            settled.insert(block);
            if recursive(mir, terminator) {
                calls.push(terminator.span);
            }
            continue;
        }
        let first = visited.insert(block);
        if !first && !settled.contains(&block) {
            return None;
        }
        if returns(terminator, recursive(mir, terminator)) {
            return None;
        }
        if !first {
            continue;
        }
        stack.push((block, true));
        if !recursive(mir, terminator) {
            stack.extend(successors(&terminator.kind).into_iter().map(|it| (it, false)));
        }
    }
    Some(calls)
}

/// rustc's `node_examined` breaking with `NonRecursive`: the terminator returns control to the
/// caller, or is a call that neither returns nor recursed and unwinds straight out.
fn returns(terminator: &Terminator, recursive: bool) -> bool {
    match &terminator.kind {
        TerminatorKind::Abort
        | TerminatorKind::CoroutineDrop
        | TerminatorKind::UnwindResume
        | TerminatorKind::Return
        | TerminatorKind::Unreachable
        | TerminatorKind::Yield { .. } => true,
        TerminatorKind::Call { target: None, cleanup: None, .. } => !recursive,
        _ => false,
    }
}

/// The edges rustc's `Search` follows: every successor but an unwind edge next to another one
/// and the imaginary edge of a `FalseEdge`.
fn successors(kind: &TerminatorKind) -> Vec<BasicBlockId> {
    match kind {
        TerminatorKind::Call { target: None, cleanup: Some(cleanup), .. } => vec![*cleanup],
        _ => kind.successors().into_vec(),
    }
}

/// `CallRecursion::trait_args`: of a method of a trait, the trait's part of its identity
/// arguments, which a call must have to call the same method.
fn trait_args<'db>(b: &BodyCx<'_, 'db>, func: FunctionId) -> Vec<hir_ty::next_solver::GenericArg<'db>> {
    let db = b.db();
    let ItemContainerId::TraitId(trait_) = func.lookup(db).container else { return Vec::new() };
    let count = GenericParams::of(db, trait_.into()).len();
    let interner = DbInterner::new_with(db, b.cx.krate.into());
    GenericArgs::identity_for_item(interner, func.into()).iter().take(count).collect()
}

/// The types of the function's MIR, in its environment.
struct Types<'db> {
    db: &'db dyn hir::db::HirDatabase,
    interner: DbInterner<'db>,
    infcx: InferCtxt<'db>,
    env: ParamEnv<'db>,
    func: FunctionId,
}

impl<'db> Types<'db> {
    fn new(b: &BodyCx<'_, 'db>, func: FunctionId) -> Types<'db> {
        let db = b.db();
        let interner = DbInterner::new_with(db, b.cx.krate.into());
        let env = db.trait_environment(GenericDefId::FunctionId(func));
        let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
        Types { db, interner, infcx, env, func }
    }

    fn place_ty(&self, mir: &MirBody<'db>, place: &StoredPlace) -> Ty<'db> {
        place.as_ref().ty(mir, &self.infcx, self.env).ty
    }

    fn operand_ty(&self, mir: &MirBody<'db>, operand: &Operand) -> Option<Ty<'db>> {
        match &operand.kind {
            OperandKind::Constant { ty, .. } => Some(ty.as_ref()),
            OperandKind::Copy(place) | OperandKind::Move(place) => Some(self.place_ty(mir, place)),
            OperandKind::Allocation { .. } | OperandKind::Static(_) => None,
        }
    }

    /// `CallRecursion::is_recursive_terminator`: a call, with as many arguments as the function
    /// has, of the function itself once the method an impl gives is found, with the trait's
    /// arguments of its own. rustc's MIR has its regions erased, so a lifetime of the trait's
    /// identity arguments is never equal to the call's.
    fn calls_itself(&self, mir: &MirBody<'_>, terminator: &Terminator, trait_args: &[hir_ty::next_solver::GenericArg<'db>]) -> bool {
        let TerminatorKind::Call { func, args, .. } = &terminator.kind else { return false };
        // SAFETY of the lifetime: `mir` is the body of `self.func`, of the same database.
        let mir: &MirBody<'db> = unsafe { std::mem::transmute(mir) };
        if args.len() != mir.param_locals.len() {
            return false;
        }
        let Some(ty) = self.operand_ty(mir, func) else { return false };
        let TyKind::FnDef(CallableIdWrapper(CallableDefId::FunctionId(callee)), call_args) = ty.kind() else {
            return false;
        };
        let env = ParamEnvAndCrate { param_env: self.env, krate: self.interner.krate.expect("a crate") };
        let (callee, call_args) = match self.db.lookup_impl_method(env, callee, call_args) {
            (Either::Left(it), args) => (it, args),
            (Either::Right(_), _) => return false,
        };
        callee == self.func
            && call_args.len() >= trait_args.len()
            && trait_args.iter().zip(call_args.iter()).all(|(want, got)| {
                !matches!(want.kind(), GenericArgKind::Lifetime(_)) && *want == got
            })
    }

    /// The type `drop` of a `Drop` impl drops (`RecursiveDrop::drop_for`): what its one argument
    /// is a reference to. rustc's MIR has its regions erased, so a type with a lifetime in it is
    /// never equal to a dropped one, and is not looked for.
    fn drop_for(&self, func: FunctionId) -> Option<Ty<'db>> {
        let db = self.db;
        let ItemContainerId::ImplId(impl_) = func.lookup(db).container else { return None };
        let trait_ = hir::Impl::from(impl_).trait_(db)?;
        if Some(hir_def::TraitId::from(trait_)) != self.interner.lang_items().Drop {
            return None;
        }
        let sig = db.callable_item_signature(CallableDefId::FunctionId(func)).instantiate_identity().skip_binder();
        let [input] = sig.inputs().as_slice() else { return None };
        let TyKind::Ref(_, dropped, _) = input.kind() else { return None };
        (!dropped.has_free_regions()).then_some(dropped)
    }

    /// `RecursiveDrop::is_recursive_terminator`: a drop of a value of `drop_for`.
    fn drops(&self, mir: &MirBody<'_>, terminator: &Terminator, drop_for: Ty<'db>) -> bool {
        let TerminatorKind::Drop { place, .. } = &terminator.kind else { return false };
        // SAFETY of the lifetime: `mir` is the body of `self.func`, of the same database.
        let mir: &MirBody<'db> = unsafe { std::mem::transmute(mir) };
        self.place_ty(mir, place) == drop_for
    }
}

#[allow(dead_code)]
fn _source(b: &BodyCx<'_, '_>, func: FunctionId) -> Option<ast::Fn> {
    Some(hir::Function::from(func).source(b.db())?.value)
}

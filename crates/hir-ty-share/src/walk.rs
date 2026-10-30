//! The share key of an instance: a walk of its generic body and its monomorphized body in
//! lockstep. The two have one shape (monomorphization puts the arguments in and changes
//! nothing else), so the walk goes over the generic body and, where a type or constant there
//! names a generic parameter, reads what the monomorphized body has in its place:
//!
//! - a local's type: its fact; each type a `Deref` of the local's place reaches: its fact;
//! - a constant: its value, evaluated as the code generator's `eval_const` does; its type:
//!   its fact; a promoted constant's value, as `promoted_constant` makes it;
//! - a call: the callee by its own share key, or by what the code generator's walk asks for
//!   it (an intrinsic by [`classify`], drop glue by [`drop::key`], a `dyn` call, a builtin
//!   impl's method, `Future::poll` of a coroutine, a builtin derive by full identity);
//! - an unsizing coercion, a function or closure turned into a pointer and a `dyn*` cast:
//!   both types' identities (the vtable or shim it makes is named by them); another cast:
//!   the target's fact;
//! - a drop: its drop key; an aggregate: its type's fact; an `asm!` `sym`: its identity.
//!
//! Whether something is read depends only on the generic body, so the records of two
//! instances line up one for one, and equal records mean equal monomorphized bodies up to the
//! facts the code generator reads. Each record is a tag byte and a 32-byte content ID.
//!
//! A call of a generic instance of the body itself is recorded as such: direct recursion
//! shares. Any other cycle of instances gives [`Unshared::Recursive`] to all in it.

use blaze_ids::{ContentId, Hasher};
use blaze_items::{ItemId, closure_code, code_hash, coroutine_code, interface_hash, item_key};
use either::Either;
use hir_def::{
    CallableDefId, FunctionId, HasModule, ItemContainerId, Lookup, signatures::FunctionSignature,
};
use hir_ty::{
    InferBodyId, MemoryMap, PointerCast,
    db::{GeneralConstId, HirDatabase, InternedClosureId, InternedCoroutineId},
    intrinsic::{Intrinsic, intrinsic_of},
    method_resolution::{is_dyn_method, lookup_impl_const},
    mir::MirOptLevel,
    next_solver::{AllocationData, Const, ConstKind, GenericArgs, Ty, TyKind, UnevaluatedConst},
    traits::StoredParamEnvAndCrate,
};
use hir_ty_mir::mir::{
    AggregateKind, AsmOperand, BasicBlock, CastKind, InlineAsm, MirBody, Operand, OperandKind,
    PlaceElem, PlaceTy, ProjectionElem, Rvalue, Statement, StatementKind, StoredPlace,
    TerminatorKind, monomorphized, monomorphized_promoted_mir_body_query,
    runtime_promoted_mir_body_query,
};
use rustc_hash::FxHashSet;
use rustc_type_ir::{
    GenericArgKind, TypeFlags, TypeVisitableExt,
    inherent::{GenericArgs as _, IntoKind},
};
use salsa::plumbing::{AsId, FromId};
use smallvec::SmallVec;

use crate::{
    CALLEE_DOMAIN, Class, MirLevels, SHARE_DOMAIN, ShareDef, ShareInstance, ShareKey, Unshared,
    VALUE_DOMAIN, WALK_DOMAIN, classify, drop, fact::Cx, identity,
};

/// A generic parameter, of a type or a constant. A lifetime is erased before codegen.
const PARAMS: TypeFlags = TypeFlags::HAS_TY_PARAM.union(TypeFlags::HAS_CT_PARAM);

/// What a record of the walk is of.
mod tag {
    pub(super) const LOCAL: u8 = 1;
    pub(super) const DEREF: u8 = 2;
    pub(super) const CONST_TY: u8 = 3;
    pub(super) const CONST: u8 = 4;
    pub(super) const ALLOCATION_TY: u8 = 5;
    pub(super) const CAST: u8 = 6;
    pub(super) const COERCE_FROM: u8 = 7;
    pub(super) const COERCE_TO: u8 = 8;
    pub(super) const REPEAT: u8 = 9;
    pub(super) const AGGREGATE: u8 = 10;
    pub(super) const CALL: u8 = 11;
    pub(super) const DROP: u8 = 12;
    pub(super) const PROMOTED: u8 = 13;
    pub(super) const PROMOTED_NONE: u8 = 14;
    pub(super) const SYM: u8 = 15;

    pub(super) const DEF_FN: u8 = 1;
    pub(super) const DEF_CLOSURE: u8 = 2;
    pub(super) const DEF_COROUTINE: u8 = 3;
    pub(super) const DEF_COROUTINE_BY_MOVE: u8 = 4;

    pub(super) const CALLEE_FN: u8 = 1;
    pub(super) const CALLEE_SELF: u8 = 2;
    pub(super) const CALLEE_SHARED: u8 = 3;
    pub(super) const CALLEE_ARGS: u8 = 4;
    pub(super) const CALLEE_INTRINSIC: u8 = 5;
    pub(super) const CALLEE_DROP: u8 = 6;
    pub(super) const CALLEE_FN_TRAIT: u8 = 7;
    pub(super) const CALLEE_CLOSURE: u8 = 8;
    pub(super) const CALLEE_IDENTITY: u8 = 9;
    pub(super) const CALLEE_POINTER: u8 = 10;

    pub(super) const ARG_TY: u8 = 1;
    pub(super) const ARG_CONST: u8 = 2;
}

#[salsa::tracked(returns(ref), cycle_result = share_cycle)]
pub(crate) fn share(
    db: &dyn HirDatabase,
    instance: ShareInstance,
    env: StoredParamEnvAndCrate,
    levels: MirLevels,
) -> Result<ShareKey, Unshared> {
    let cx = Cx::new(db, &env);
    let mut key = Hasher::new(SHARE_DOMAIN);
    let bodies = bodies(db, instance.def, levels, &mut key)?;
    // The code generator reads an instance's const arguments itself (a SIMD lane).
    for konst in instance.args.as_ref().iter().filter_map(|arg| arg.konst()) {
        key.id(&value(&cx, konst)?);
    }
    for (level, generic) in bodies {
        let mono = monomorphized(db, generic, &instance.args, &env)
            .map_err(|error| Unshared::NoMir(format!("monomorphized: {error:?}")))?;
        let mut walk = Walk {
            cx: &cx,
            instance: &instance,
            levels,
            generic,
            mono: &mono,
            out: Vec::with_capacity(1024),
            derefs: FxHashSet::default(),
        };
        walk.body()?;
        key.update(&[level as u8]).id(&ContentId::of(WALK_DOMAIN, &walk.out));
    }
    Ok(ShareKey(key.finish()))
}

fn share_cycle(
    _: &dyn HirDatabase,
    _: salsa::Id,
    _: ShareInstance,
    _: StoredParamEnvAndCrate,
    _: MirLevels,
) -> Result<ShareKey, Unshared> {
    Err(Unshared::Recursive)
}

type Bodies<'db> = SmallVec<[(MirOptLevel, &'db MirBody<'db>); 3]>;

/// The generic bodies the code generator may compile `def` from, one per level, with what
/// names the body hashed into `key`.
fn bodies<'db>(
    db: &'db dyn HirDatabase,
    def: ShareDef,
    levels: MirLevels,
    key: &mut Hasher,
) -> Result<Bodies<'db>, Unshared> {
    let no_mir = |error| Unshared::NoMir(format!("{error:?}"));
    let levels_of = |owner: InferBodyId<'db>| {
        let set =
            if owner.krate(db).data(db).origin.is_lang() { levels.lang } else { levels.other };
        if set.is_empty() { Err(Unshared::NoLevels) } else { Ok(set) }
    };
    let mut bodies = Bodies::new();
    match def {
        ShareDef::Fn(func) => {
            let item = ItemId::Function(func);
            let code = code_hash(db, item).map_err(identity::unhashable(db))?.ok_or_else(|| {
                Unshared::NoMir(format!("`{}` has no body", FunctionSignature::of(db, func).name.as_str()))
            })?;
            key.update(&[tag::DEF_FN]).id(&item_key(db, item).0).id(&code.0);
            let owner = InferBodyId::DefWithBodyId(func.into());
            for level in levels_of(owner)?.iter() {
                let body = match level {
                    MirOptLevel::Off => db.runtime_mir_body(owner),
                    level => db.optimized_mir_body(owner, level),
                };
                bodies.push((level, body.map_err(no_mir)?));
            }
        }
        ShareDef::Closure(id) => {
            let closure = InternedClosureId::from_id(id);
            let code = closure_code(db, closure).map_err(identity::unhashable(db))?;
            key.update(&[tag::DEF_CLOSURE]).id(&code.0);
            for level in levels_of(closure.loc(db).owner)?.iter() {
                let body = match level {
                    MirOptLevel::Off => db.runtime_mir_body_for_closure(closure),
                    level => db.optimized_mir_body_for_closure(closure, level),
                };
                bodies.push((level, body.map_err(no_mir)?));
            }
        }
        // A coroutine's body is the runtime body at every level.
        ShareDef::Coroutine(id) => {
            let coroutine = InternedCoroutineId::from_id(id);
            let code = coroutine_code(db, coroutine).map_err(identity::unhashable(db))?;
            key.update(&[tag::DEF_COROUTINE]).id(&code.0);
            let body = db.runtime_mir_body_for_coroutine(coroutine).map_err(no_mir)?;
            bodies.push((MirOptLevel::Off, body));
        }
        ShareDef::CoroutineByMove(id) => {
            let coroutine = InternedCoroutineId::from_id(id);
            let code = coroutine_code(db, coroutine).map_err(identity::unhashable(db))?;
            key.update(&[tag::DEF_COROUTINE_BY_MOVE]).id(&code.0);
            let body = db.runtime_mir_body_for_coroutine_by_move(coroutine).map_err(no_mir)?;
            bodies.push((MirOptLevel::Off, body));
        }
        ShareDef::DropGlue => return Err(Unshared::Shim("drop glue")),
        ShareDef::CallOnceShim => return Err(Unshared::Shim("`FnOnce::call_once` shim")),
        ShareDef::VtableShim(_) => return Err(Unshared::Shim("vtable shim")),
        ShareDef::FnPointerShim => return Err(Unshared::Shim("function pointer shim")),
        ShareDef::ReifyShim { .. } => return Err(Unshared::Shim("reify shim")),
    }
    Ok(bodies)
}

fn has_params<'db>(ty: Ty<'db>) -> bool {
    ty.has_type_flags(PARAMS)
}

/// The value of a constant of the monomorphized body: a value's identity, or what an
/// unevaluated one evaluates to, as the code generator's `eval_const` evaluates it.
pub(crate) fn value<'db>(cx: &Cx<'db>, konst: Const<'db>) -> Result<ContentId, Unshared> {
    let ConstKind::Unevaluated(UnevaluatedConst { def, args }) = konst.kind() else {
        return identity::konst(cx.db, konst);
    };
    let db = cx.db;
    let allocation = match def.0 {
        GeneralConstId::ConstId(id) => {
            let (id, args) = lookup_impl_const(&cx.infcx, cx.env.param_env, id, args);
            db.const_eval(id, args, Some(cx.env))
        }
        GeneralConstId::StaticId(id) => db.const_eval_static(id),
        GeneralConstId::AnonConstId(id) => db.anon_const_eval(id, args, Some(cx.env)),
    }
    .map_err(|error| Unshared::ConstEval(format!("`{konst:?}`: {error:?}")))?;
    allocation_value(&allocation)
}

/// An evaluated constant's bytes and the one allocation it may point to.
fn allocation_value(allocation: &AllocationData<'_>) -> Result<ContentId, Unshared> {
    let mut h = Hasher::new(VALUE_DOMAIN);
    h.part(&allocation.memory);
    match &allocation.memory_map {
        MemoryMap::Empty => h.update(&[0]),
        MemoryMap::Simple(pointee) => h.update(&[1]).part(pointee),
        MemoryMap::Complex(_) => {
            return Err(Unshared::NoContent(format!(
                "a constant of `{:?}` that points to more than one allocation",
                allocation.ty
            )));
        }
    };
    Ok(h.finish())
}

struct Walk<'a, 'db> {
    cx: &'a Cx<'db>,
    instance: &'a ShareInstance,
    levels: MirLevels,
    generic: &'a MirBody<'db>,
    mono: &'a MirBody<'db>,
    out: Vec<u8>,
    /// The places of the generic body whose `Deref`s are recorded.
    derefs: FxHashSet<&'a StoredPlace>,
}

impl<'a, 'db> Walk<'a, 'db> {
    fn db(&self) -> &'db dyn HirDatabase {
        self.cx.db
    }

    fn record(&mut self, tag: u8, id: ContentId) {
        self.out.push(tag);
        self.out.extend_from_slice(&id.0);
    }

    fn body(&mut self) -> Result<(), Unshared> {
        let (generic, mono) = (self.generic, self.mono);
        if generic.basic_blocks.len() != mono.basic_blocks.len()
            || generic.locals.len() != mono.locals.len()
            || generic.promoted_locals.len() != mono.promoted_locals.len()
        {
            return Err(Unshared::Shape("its blocks or locals"));
        }
        for ((_, g), (_, m)) in generic.locals.iter().zip(mono.locals.iter()) {
            if has_params(g.ty.as_ref()) {
                let fact = self.cx.fact(m.ty.as_ref())?;
                self.record(tag::LOCAL, fact);
            }
        }
        self.promoted()?;
        for ((_, g), (_, m)) in generic.basic_blocks.iter().zip(mono.basic_blocks.iter()) {
            self.block(g, m)?;
        }
        Ok(())
    }

    /// Each promoted constant whose body names a parameter, by its value.
    fn promoted(&mut self) -> Result<(), Unshared> {
        let db = self.db();
        let owner = self.generic.owner;
        for &(_, expr) in &self.generic.promoted_locals {
            let Ok(generic) = runtime_promoted_mir_body_query(db, owner, expr) else { continue };
            if !mentions_params(generic) {
                continue;
            }
            let evaluated = monomorphized_promoted_mir_body_query(
                db,
                owner,
                expr,
                self.instance.args.clone(),
                self.cx.stored.clone(),
            )
            .ok()
            .and_then(|body| db.interpret_mir(body, false, Some(self.cx.env)).ok());
            match evaluated {
                Some(allocation) => {
                    let value = allocation_value(&allocation)?;
                    self.record(tag::PROMOTED, value);
                }
                // The code generator does not promote a constant it can't evaluate.
                None => self.record(tag::PROMOTED_NONE, ContentId::default()),
            }
        }
        Ok(())
    }

    fn block(&mut self, g: &'a BasicBlock, m: &'a BasicBlock) -> Result<(), Unshared> {
        if g.statements.len() != m.statements.len() {
            return Err(Unshared::Shape("a block's statements"));
        }
        for (g, m) in g.statements.iter().zip(&m.statements) {
            self.statement(g, m)?;
        }
        match (&g.terminator, &m.terminator) {
            (Some(g), Some(m)) => self.terminator(&g.kind, &m.kind),
            (None, None) => Ok(()),
            _ => Err(Unshared::Shape("a block's terminator")),
        }
    }

    fn statement(&mut self, g: &'a Statement, m: &'a Statement) -> Result<(), Unshared> {
        match (&g.kind, &m.kind) {
            (StatementKind::Assign(gp, gr), StatementKind::Assign(mp, mr)) => {
                self.place(gp, mp)?;
                self.rvalue(gr, mr)
            }
            (StatementKind::FakeRead(g), StatementKind::FakeRead(m))
            | (StatementKind::Deinit(g), StatementKind::Deinit(m)) => self.place(g, m),
            (StatementKind::InlineAsm(g), StatementKind::InlineAsm(m)) => self.inline_asm(g, m),
            (StatementKind::StorageLive(_), StatementKind::StorageLive(_))
            | (StatementKind::StorageDead(_), StatementKind::StorageDead(_))
            | (StatementKind::Nop, StatementKind::Nop) => Ok(()),
            _ => Err(Unshared::Shape("a statement")),
        }
    }

    fn rvalue(&mut self, g: &'a Rvalue, m: &'a Rvalue) -> Result<(), Unshared> {
        match (g, m) {
            (Rvalue::Use(g), Rvalue::Use(m)) | (Rvalue::UnaryOp(_, g), Rvalue::UnaryOp(_, m)) => {
                self.operand(g, m)
            }
            (Rvalue::Repeat(g, g_len), Rvalue::Repeat(m, m_len)) => {
                self.operand(g, m)?;
                if g_len.as_ref().has_type_flags(PARAMS) {
                    let len = value(self.cx, m_len.as_ref())?;
                    self.record(tag::REPEAT, len);
                }
                Ok(())
            }
            (Rvalue::Ref(_, g), Rvalue::Ref(_, m))
            | (Rvalue::Len(g), Rvalue::Len(m))
            | (Rvalue::Discriminant(g), Rvalue::Discriminant(m))
            | (Rvalue::CopyForDeref(g), Rvalue::CopyForDeref(m)) => self.place(g, m),
            (Rvalue::Cast(kind, g, g_to), Rvalue::Cast(_, m, m_to)) => {
                self.operand(g, m)?;
                if self.operand_has_params(g) || has_params(g_to.as_ref()) {
                    self.cast(kind, m, self.cx.normalize(m_to.as_ref()))?;
                }
                Ok(())
            }
            (Rvalue::BinaryOp(_, gl, gr), Rvalue::BinaryOp(_, ml, mr))
            | (Rvalue::CheckedBinaryOp(_, gl, gr), Rvalue::CheckedBinaryOp(_, ml, mr)) => {
                self.operand(gl, ml)?;
                self.operand(gr, mr)
            }
            (Rvalue::Aggregate(g_kind, g), Rvalue::Aggregate(m_kind, m)) => {
                if g.len() != m.len() {
                    return Err(Unshared::Shape("an aggregate"));
                }
                self.aggregate(g_kind, m_kind)?;
                for (g, m) in g.iter().zip(m.iter()) {
                    self.operand(g, m)?;
                }
                Ok(())
            }
            (Rvalue::ThreadLocalRef(never) | Rvalue::AddressOf(never) | Rvalue::NullaryOp(never), _) => {
                match *never {}
            }
            _ => Err(Unshared::Shape("an rvalue")),
        }
    }

    fn aggregate(&mut self, g: &AggregateKind, m: &AggregateKind) -> Result<(), Unshared> {
        match (g, m) {
            (AggregateKind::Array(g), AggregateKind::Array(m))
            | (AggregateKind::Tuple(g), AggregateKind::Tuple(m))
            | (AggregateKind::Closure(g), AggregateKind::Closure(m)) => {
                if has_params(g.as_ref()) {
                    let fact = self.cx.fact(m.as_ref())?;
                    self.record(tag::AGGREGATE, fact);
                }
                Ok(())
            }
            (AggregateKind::Adt(_, g), AggregateKind::Adt(_, m)) => {
                if g.as_ref().has_type_flags(PARAMS) {
                    let args = self.args_facts(m.as_ref())?;
                    self.record(tag::AGGREGATE, args);
                }
                Ok(())
            }
            (AggregateKind::Union(..), AggregateKind::Union(..)) => Ok(()),
            _ => Err(Unshared::Shape("an aggregate's kind")),
        }
    }

    /// A cast whose source or target names a parameter in the generic body.
    fn cast(&mut self, kind: &CastKind, m: &Operand, to: Ty<'db>) -> Result<(), Unshared> {
        let from = self.mono_operand_ty(m)?;
        let coerces = match kind {
            CastKind::PointerCoercion(PointerCast::Unsize) => !is_array_to_slice(from, to),
            CastKind::PointerCoercion(
                PointerCast::ReifyFnPointer | PointerCast::ClosureFnPointer(_),
            )
            | CastKind::DynStar => true,
            CastKind::PointerCoercion(
                PointerCast::UnsafeFnPointer
                | PointerCast::MutToConstPointer
                | PointerCast::ArrayToPointer,
            )
            | CastKind::PointerExposeAddress
            | CastKind::PointerFromExposedAddress
            | CastKind::PtrToPtr
            | CastKind::IntToInt
            | CastKind::FloatToInt
            | CastKind::FloatToFloat
            | CastKind::IntToFloat
            | CastKind::FnPtrToPtr => false,
        };
        if coerces {
            let (from, to) = (identity::ty(self.db(), from)?, identity::ty(self.db(), to)?);
            self.record(tag::COERCE_FROM, from);
            self.record(tag::COERCE_TO, to);
        } else {
            let (from, to) = (self.cx.fact(from)?, self.cx.fact(to)?);
            self.record(tag::CAST, from);
            self.record(tag::CAST, to);
        }
        Ok(())
    }

    fn inline_asm(&mut self, g: &'a InlineAsm, m: &'a InlineAsm) -> Result<(), Unshared> {
        if g.operands.len() != m.operands.len() {
            return Err(Unshared::Shape("an `asm!` block's operands"));
        }
        for (g, m) in g.operands.iter().zip(m.operands.iter()) {
            match (g, m) {
                (
                    AsmOperand::Reg { input: g_in, output: g_out, .. },
                    AsmOperand::Reg { input: m_in, output: m_out, .. },
                ) => {
                    match (g_in, m_in) {
                        (Some(g), Some(m)) => self.operand(g, m)?,
                        (None, None) => (),
                        _ => return Err(Unshared::Shape("an `asm!` input")),
                    }
                    match (g_out, m_out) {
                        (Some(g), Some(m)) => self.place(g, m)?,
                        (None, None) => (),
                        _ => return Err(Unshared::Shape("an `asm!` output")),
                    }
                }
                (AsmOperand::Const(g), AsmOperand::Const(m)) => self.operand(g, m)?,
                (AsmOperand::Sym(g), AsmOperand::Sym(m)) => {
                    self.operand(g, m)?;
                    if self.operand_has_params(g) {
                        let sym = identity::ty(self.db(), self.mono_operand_ty(m)?)?;
                        self.record(tag::SYM, sym);
                    }
                }
                (AsmOperand::Label, AsmOperand::Label) => (),
                _ => return Err(Unshared::Shape("an `asm!` operand")),
            }
        }
        match (&g.taken, &m.taken) {
            (Some(g), Some(m)) => self.place(g, m),
            (None, None) => Ok(()),
            _ => Err(Unshared::Shape("an `asm!` block's labels")),
        }
    }

    fn terminator(&mut self, g: &'a TerminatorKind, m: &'a TerminatorKind) -> Result<(), Unshared> {
        match (g, m) {
            (TerminatorKind::Goto { .. }, TerminatorKind::Goto { .. })
            | (TerminatorKind::UnwindResume, TerminatorKind::UnwindResume)
            | (TerminatorKind::Abort, TerminatorKind::Abort)
            | (TerminatorKind::Return, TerminatorKind::Return)
            | (TerminatorKind::Unreachable, TerminatorKind::Unreachable)
            | (TerminatorKind::CoroutineDrop, TerminatorKind::CoroutineDrop)
            | (TerminatorKind::FalseEdge { .. }, TerminatorKind::FalseEdge { .. })
            | (TerminatorKind::FalseUnwind { .. }, TerminatorKind::FalseUnwind { .. }) => Ok(()),
            (TerminatorKind::SwitchInt { discr: g, .. }, TerminatorKind::SwitchInt { discr: m, .. }) => {
                self.operand(g, m)
            }
            (
                TerminatorKind::Yield { value: g, resume_arg: g_arg, .. },
                TerminatorKind::Yield { value: m, resume_arg: m_arg, .. },
            ) => {
                self.operand(g, m)?;
                self.place(g_arg, m_arg)
            }
            (TerminatorKind::Drop { place: g, .. }, TerminatorKind::Drop { place: m, .. }) => {
                self.drop(g, m)
            }
            (
                TerminatorKind::DropAndReplace { place: g, value: g_value, .. },
                TerminatorKind::DropAndReplace { place: m, value: m_value, .. },
            ) => {
                self.drop(g, m)?;
                self.operand(g_value, m_value)
            }
            (
                TerminatorKind::Assert { cond: g_cond, msg: g_msg, .. },
                TerminatorKind::Assert { cond: m_cond, msg: m_msg, .. },
            ) => {
                self.operand(g_cond, m_cond)?;
                let mut any = false;
                g_msg.for_each_operand(&mut |it| any |= self.operand_has_params(it));
                if any {
                    let mut g_ops = SmallVec::<[Operand; 2]>::new();
                    let mut m_ops = SmallVec::<[Operand; 2]>::new();
                    g_msg.for_each_operand(&mut |it| g_ops.push(it.clone()));
                    m_msg.for_each_operand(&mut |it| m_ops.push(it.clone()));
                    if g_ops.len() != m_ops.len() {
                        return Err(Unshared::Shape("an assertion's message"));
                    }
                    for (g, m) in g_ops.iter().zip(&m_ops) {
                        self.owned_operand(g, m)?;
                    }
                }
                Ok(())
            }
            (
                TerminatorKind::Call { func: g_func, args: g_args, destination: g_dst, .. },
                TerminatorKind::Call {
                    func: m_func,
                    args: m_args,
                    destination: m_dst,
                    intrinsic,
                    ..
                },
            ) => {
                if g_args.len() != m_args.len() {
                    return Err(Unshared::Shape("a call's arguments"));
                }
                self.operand(g_func, m_func)?;
                if self.operand_has_params(g_func) {
                    let fn_ty = self.mono_operand_ty(m_func)?;
                    let callee = self.callee(fn_ty, *intrinsic)?;
                    self.record(tag::CALL, callee);
                }
                for (g, m) in g_args.iter().zip(m_args.iter()) {
                    self.operand(g, m)?;
                }
                self.place(g_dst, m_dst)
            }
            _ => Err(Unshared::Shape("a terminator")),
        }
    }

    fn drop(&mut self, g: &'a StoredPlace, m: &'a StoredPlace) -> Result<(), Unshared> {
        self.place(g, m)?;
        if has_params(self.generic.locals[g.local].ty.as_ref()) {
            let key = drop::key(self.cx, self.mono_place_ty(m)?)?;
            self.record(tag::DROP, key);
        }
        Ok(())
    }

    fn operand(&mut self, g: &'a Operand, m: &Operand) -> Result<(), Unshared> {
        match (&g.kind, &m.kind) {
            (
                OperandKind::Copy(g) | OperandKind::Move(g),
                OperandKind::Copy(m) | OperandKind::Move(m),
            ) => self.place(g, m),
            _ => self.constant(g, m),
        }
    }

    /// [`Self::operand`] of operands the walk holds copies of, whose places it does not keep.
    fn owned_operand(&mut self, g: &Operand, m: &Operand) -> Result<(), Unshared> {
        match (&g.kind, &m.kind) {
            (
                OperandKind::Copy(g) | OperandKind::Move(g),
                OperandKind::Copy(m) | OperandKind::Move(m),
            ) => self.derefs_of(g, m),
            _ => self.constant(g, m),
        }
    }

    fn constant(&mut self, g: &Operand, m: &Operand) -> Result<(), Unshared> {
        match (&g.kind, &m.kind) {
            (
                OperandKind::Constant { konst: g_konst, ty: g_ty },
                OperandKind::Constant { konst: m_konst, ty: m_ty },
            ) => {
                if has_params(g_ty.as_ref()) {
                    let fact = self.cx.fact(m_ty.as_ref())?;
                    self.record(tag::CONST_TY, fact);
                }
                if g_konst.as_ref().has_type_flags(PARAMS) {
                    let value = value(self.cx, m_konst.as_ref())?;
                    self.record(tag::CONST, value);
                }
                Ok(())
            }
            (OperandKind::Allocation { allocation: g }, OperandKind::Allocation { allocation: m }) => {
                if has_params(g.as_ref().ty) {
                    let fact = self.cx.fact(m.as_ref().ty)?;
                    self.record(tag::ALLOCATION_TY, fact);
                }
                Ok(())
            }
            (OperandKind::Static(_), OperandKind::Static(_)) => Ok(()),
            _ => Err(Unshared::Shape("an operand")),
        }
    }

    /// A place's `Deref`s, once per place of the generic body.
    fn place(&mut self, g: &'a StoredPlace, m: &StoredPlace) -> Result<(), Unshared> {
        if !g.projection.as_slice().contains(&ProjectionElem::Deref)
            || !has_params(self.generic.locals[g.local].ty.as_ref())
            || !self.derefs.insert(g)
        {
            return Ok(());
        }
        self.derefs_of(g, m)
    }

    fn derefs_of(&mut self, g: &StoredPlace, m: &StoredPlace) -> Result<(), Unshared> {
        let projection = m.projection.as_slice();
        if g.projection.as_slice().len() != projection.len() {
            return Err(Unshared::Shape("a place"));
        }
        let Some(last) = projection.iter().rposition(|it| *it == ProjectionElem::Deref) else {
            return Ok(());
        };
        if !has_params(self.generic.locals[g.local].ty.as_ref()) {
            return Ok(());
        }
        let mut ty = PlaceTy::from_ty(self.cx.normalize(self.mono.locals[m.local].ty.as_ref()));
        for elem in &projection[..=last] {
            ty = self.project(ty, elem)?;
            if let ProjectionElem::Deref = elem {
                let fact = self.cx.fact(ty.ty)?;
                self.record(tag::DEREF, fact);
            }
        }
        Ok(())
    }

    fn project(&self, ty: PlaceTy<'db>, elem: &PlaceElem) -> Result<PlaceTy<'db>, Unshared> {
        if ty.variant_id.is_some() && !matches!(ty.ty.kind(), TyKind::Adt(..)) {
            return Err(Unshared::Shape("a field of a coroutine's variant"));
        }
        let mut ty = ty.projection_ty(&self.cx.infcx, elem, self.cx.env.param_env);
        ty.ty = self.cx.normalize(ty.ty);
        Ok(ty)
    }

    fn mono_place_ty(&self, place: &StoredPlace) -> Result<Ty<'db>, Unshared> {
        let mut ty =
            PlaceTy::from_ty(self.cx.normalize(self.mono.locals[place.local].ty.as_ref()));
        for elem in place.projection.as_slice() {
            ty = self.project(ty, elem)?;
        }
        Ok(ty.ty)
    }

    fn mono_operand_ty(&self, operand: &Operand) -> Result<Ty<'db>, Unshared> {
        match &operand.kind {
            OperandKind::Copy(place) | OperandKind::Move(place) => self.mono_place_ty(place),
            OperandKind::Constant { ty, .. } => Ok(self.cx.normalize(ty.as_ref())),
            OperandKind::Allocation { allocation } => Ok(allocation.as_ref().ty),
            OperandKind::Static(_) => {
                Err(Unshared::NoContent("the type of a static's operand".to_owned()))
            }
        }
    }

    /// Whether the generic body's `operand` is of a type that names a parameter.
    fn operand_has_params(&self, operand: &Operand) -> bool {
        match &operand.kind {
            OperandKind::Copy(place) | OperandKind::Move(place) => {
                has_params(self.generic.locals[place.local].ty.as_ref())
            }
            OperandKind::Constant { konst, ty } => {
                has_params(ty.as_ref()) || konst.as_ref().has_type_flags(PARAMS)
            }
            OperandKind::Allocation { allocation } => has_params(allocation.as_ref().ty),
            OperandKind::Static(_) => false,
        }
    }

    /// The facts of a list of arguments' types and the values of its constants.
    fn args_facts(&self, args: GenericArgs<'db>) -> Result<ContentId, Unshared> {
        let mut h = Hasher::new(CALLEE_DOMAIN);
        for arg in args.iter() {
            match arg.kind() {
                GenericArgKind::Type(ty) => h.update(&[tag::ARG_TY]).id(&self.cx.fact(ty)?),
                GenericArgKind::Const(konst) => {
                    h.update(&[tag::ARG_CONST]).id(&value(self.cx, konst)?)
                }
                GenericArgKind::Lifetime(_) => continue,
            };
        }
        Ok(h.finish())
    }

    /// What the code generator's walk asks for a call of `fn_ty`.
    fn callee(&self, fn_ty: Ty<'db>, intrinsic: Option<Intrinsic>) -> Result<ContentId, Unshared> {
        let fn_ty = self.cx.normalize(fn_ty);
        match fn_ty.kind() {
            // Called through the pointer, whose type's fact its place has.
            TyKind::FnPtr(..) => Ok(ContentId::of(CALLEE_DOMAIN, &[tag::CALLEE_POINTER])),
            TyKind::FnDef(def, args) => match def.0 {
                CallableDefId::FunctionId(func) => self.call_fn(fn_ty, func, args, intrinsic),
                CallableDefId::StructId(_) | CallableDefId::EnumVariantId(_) => {
                    self.identity_callee(fn_ty)
                }
            },
            _ => self.identity_callee(fn_ty),
        }
    }

    fn identity_callee(&self, fn_ty: Ty<'db>) -> Result<ContentId, Unshared> {
        let mut h = Hasher::new(CALLEE_DOMAIN);
        h.update(&[tag::CALLEE_IDENTITY]).id(&identity::ty(self.db(), fn_ty)?);
        Ok(h.finish())
    }

    fn call_fn(
        &self,
        fn_ty: Ty<'db>,
        func: FunctionId,
        args: GenericArgs<'db>,
        intrinsic: Option<Intrinsic>,
    ) -> Result<ContentId, Unshared> {
        let db = self.db();
        let intrinsic = match intrinsic {
            Some(intrinsic) => Some(intrinsic),
            None => intrinsic_of(db, func)
                .map_err(|unknown| Unshared::UnknownIntrinsic(unknown.0.as_str().to_owned()))?,
        };
        if let Some(intrinsic) = intrinsic {
            return self.intrinsic(fn_ty, func, args, intrinsic);
        }
        let lang = self.cx.interner.lang_items();
        if [lang.DropInPlace, lang.DropGlue].contains(&Some(func)) {
            let mut h = Hasher::new(CALLEE_DOMAIN);
            h.update(&[tag::CALLEE_DROP]).id(&drop::key(self.cx, args.type_at(0))?);
            return Ok(h.finish());
        }
        if [lang.Fn_call, lang.FnMut_call_mut, lang.FnOnce_call_once].contains(&Some(func)) {
            return self.fn_trait_call(fn_ty, func, args);
        }
        if Some(func) == lang.FuturePoll
            || is_dyn_method(self.cx.interner, self.cx.env.param_env, func, args).is_some()
        {
            return self.identity_callee(fn_ty);
        }
        let (callee, callee_args) = db.lookup_impl_method(self.cx.env, func, args);
        let Either::Left(callee) = callee else { return self.identity_callee(fn_ty) };
        // A method of a builtin impl (`Clone` of a tuple, `FnPtr::addr`), which the code
        // generator makes from the type.
        if callee == func
            && let ItemContainerId::TraitId(_) = func.lookup(db).container
            && !FunctionSignature::of(db, func).has_body()
        {
            return self.identity_callee(fn_ty);
        }
        self.instance_callee(ShareDef::Fn(callee), callee_args, tag::CALLEE_FN, Some(callee))
    }

    /// A call of `def` with `args`: the code generator names it by its instance key, which is
    /// its share key, or its arguments' hash for an instance with none.
    fn instance_callee(
        &self,
        def: ShareDef,
        args: GenericArgs<'db>,
        what: u8,
        func: Option<FunctionId>,
    ) -> Result<ContentId, Unshared> {
        let db = self.db();
        let mut h = Hasher::new(CALLEE_DOMAIN);
        h.update(&[what]);
        if let Some(func) = func {
            let item = ItemId::Function(func);
            h.id(&item_key(db, item).0).id(&interface_hash(db, item).map_err(identity::unhashable(db))?.0);
        }
        let generic = args.iter().any(|arg| !matches!(arg.kind(), GenericArgKind::Lifetime(_)));
        if !generic {
            return Ok(h.finish());
        }
        let instance = ShareInstance { def, args: args.store() };
        if instance == *self.instance {
            h.update(&[tag::CALLEE_SELF]);
            return Ok(h.finish());
        }
        match share(db, instance, self.cx.stored.clone(), self.levels) {
            Ok(key) => h.update(&[tag::CALLEE_SHARED]).id(&key.0),
            Err(Unshared::Recursive) => return Err(Unshared::Recursive),
            Err(_) => h.update(&[tag::CALLEE_ARGS]).id(&identity::args(db, args)?),
        };
        Ok(h.finish())
    }

    fn intrinsic(
        &self,
        fn_ty: Ty<'db>,
        func: FunctionId,
        args: GenericArgs<'db>,
        intrinsic: Intrinsic,
    ) -> Result<ContentId, Unshared> {
        match classify(intrinsic) {
            Class::Layout => {
                let db = self.db();
                let mut h = Hasher::new(CALLEE_DOMAIN);
                h.update(&[tag::CALLEE_INTRINSIC]).update(&intrinsic.index().to_le_bytes());
                h.id(&self.args_facts(args)?);
                // An intrinsic the code generator does not make inline calls its fallback body.
                if FunctionSignature::of(db, func).has_body() {
                    h.id(&self.instance_callee(ShareDef::Fn(func), args, tag::CALLEE_FN, Some(func))?);
                }
                Ok(h.finish())
            }
            Class::Identity => self.identity_callee(fn_ty),
            Class::PerInstance => Err(Unshared::PerInstance(intrinsic.to_string())),
        }
    }

    /// `Fn::call`, `FnMut::call_mut` or `FnOnce::call_once` of a closure or a function item
    /// by its body's key; of anything else by identity.
    fn fn_trait_call(
        &self,
        fn_ty: Ty<'db>,
        method: FunctionId,
        args: GenericArgs<'db>,
    ) -> Result<ContentId, Unshared> {
        let db = self.db();
        let self_ty = self.cx.normalize(args.type_at(0));
        let mut h = Hasher::new(CALLEE_DOMAIN);
        h.update(&[tag::CALLEE_FN_TRAIT]).id(&item_key(db, ItemId::Function(method)).0);
        h.id(&self.cx.fact(args.type_at(1))?);
        match self_ty.kind() {
            TyKind::Closure(id, closure_args) => {
                let parent = GenericArgs::new_from_slice(closure_args.as_closure().parent_args());
                let def = ShareDef::Closure(id.0.as_id());
                h.id(&self.instance_callee(def, parent, tag::CALLEE_CLOSURE, None)?);
                // A closure passed by value to a body that borrows it is the caller's to drop.
                h.id(&drop::key(self.cx, self_ty)?);
            }
            TyKind::FnDef(def, fn_args) => match def.0 {
                CallableDefId::FunctionId(func) if intrinsic_of(db, func).ok().flatten().is_none() => {
                    h.id(&self.call_fn(self_ty, func, fn_args, None)?);
                }
                _ => return self.identity_callee(fn_ty),
            },
            _ => return self.identity_callee(fn_ty),
        }
        Ok(h.finish())
    }
}

/// An unsizing of a pointer to an array to one to a slice: its metadata is the length, which
/// the source's fact has.
fn is_array_to_slice<'db>(from: Ty<'db>, to: Ty<'db>) -> bool {
    match (from.builtin_deref(true), to.builtin_deref(true)) {
        (Some(from), Some(to)) => {
            matches!((from.kind(), to.kind()), (TyKind::Array(..), TyKind::Slice(_)))
        }
        _ => false,
    }
}

/// Whether a promoted constant's body names a parameter: in a local's type or a constant.
fn mentions_params(body: &MirBody<'_>) -> bool {
    if body.locals.iter().any(|(_, local)| has_params(local.ty.as_ref())) {
        return true;
    }
    let mut in_operand = false;
    let mut visit = |operand: &Operand| {
        in_operand |= match &operand.kind {
            OperandKind::Constant { konst, ty } => {
                has_params(ty.as_ref()) || konst.as_ref().has_type_flags(PARAMS)
            }
            OperandKind::Allocation { allocation } => has_params(allocation.as_ref().ty),
            OperandKind::Copy(_) | OperandKind::Move(_) | OperandKind::Static(_) => false,
        };
    };
    let mut in_length = false;
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            statement.kind.for_each_operand(&mut visit);
            if let StatementKind::Assign(_, Rvalue::Repeat(_, len)) = &statement.kind {
                in_length |= len.as_ref().has_type_flags(PARAMS);
            }
        }
        if let Some(terminator) = &block.terminator {
            terminator.kind.for_each_operand(&mut visit);
        }
    }
    in_operand || in_length
}

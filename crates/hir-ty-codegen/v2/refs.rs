//! The references of an instance's code, from a walk of its MIR (docs/v2/plan/lanes/codegen.md):
//! what [`blaze_codegen::backend::Described::refs`] lists before the instance is compiled, and
//! all that the IR of any unit of it may ask for by place ([`places::Placer::listed`]).
//!
//! The walk runs the IR builder's own resolution on a [`FunctionCx`] that builds no code: its
//! resolvers of functions, vtables, constants and statics ([`FunctionCx::fn_item`],
//! [`FunctionCx::vtable`], [`FunctionCx::allocation_blob`]) make their requests of the
//! [`Recorder`] as they do for the IR, and the walk reads them off it in the order of the
//! MIR. Where the IR builder decides with code what it calls, the walk makes the same
//! decision of the types. What it lists is the union over the settings of a unit: the blocks
//! that any of them compiles (it walks with unwinding, `ub_checks()` and debug assertions
//! on), each check overflow checks may make, each landing pad's routines.

use std::{borrow::Cow, hash::BuildHasher};

use blaze_codegen::backend::FrontRef;
use cranelift_codegen::ir::{self, ArgumentPurpose};
use hashbrown::{HashTable, hash_table::Entry};
use hir_def::{
    AdtId, CallableDefId, FunctionId, HasModule, ItemContainerId, Lookup, StaticId,
    attrs::AttrFlags, signatures::FunctionSignature,
};
use hir_ty::{
    InferBodyId, ParamEnvAndCrate, PointerCast,
    db::HirDatabase,
    drop::DropGlue,
    method_resolution::is_dyn_method,
    next_solver::{ConstKind, GenericArgs, ParamEnv, Region, Ty, TyKind},
    traits::StoredParamEnvAndCrate,
};
use hir_ty_mir::mir::{
    AssertKind, BasicBlockId, BinOp, CastKind, InlineAsm, MirBody, MirPhase, MirSpan, Operand,
    OperandKind, PlaceTy, Rvalue, StatementKind, StoredPlace, TerminatorKind, interpret_mir,
    is_by_move_coroutine,
    location::{located_concats, track_caller},
    monomorphized_promoted_mir_body_query,
};
use rustc_abi::ExternAbi;
use rustc_ast_ir::Mutability;
use rustc_hash::{FxBuildHasher, FxHashSet};
use rustc_type_ir::inherent::{GenericArgs as _, IntoKind, SliceLike as _, Ty as _};
use salsa::plumbing::AsId;
use smallvec::{SmallVec, smallvec};

use super::{
    HirInstance, Kind,
    build::Build,
    places::{self, Ref},
};
use crate::{
    Arch, CodegenError, FnAbi, FunctionCx, Instance, InstanceDef, Target, asm, coroutine_instance,
    fn_trait_methods, fncache,
    fragment::{DataRequest, FuncTarget, Recorder},
    home::Homes,
    is_integer, is_thread_local,
};

mod intrinsics;
mod shims;
#[cfg(test)]
mod tests;

/// The types of a call's operands: inline up to eight, as nearly every call has.
pub(super) type Tys<'db> = SmallVec<[Ty<'db>; 8]>;

/// Everything the IR of `instance` can request, each once, in the order of its MIR: [`walked`],
/// owned.
pub(crate) fn walk(
    db: &dyn HirDatabase,
    instance: &HirInstance,
    _: &mut Homes,
) -> Result<Vec<Ref>, String> {
    listed(db, instance).map(Cow::into_owned)
}

/// [`walk`], borrowed from the database's memo where it has one: what `prepare` places the
/// IR's requests by, with nothing copied.
pub(crate) fn listed<'db>(
    db: &'db dyn HirDatabase,
    instance: &HirInstance,
) -> Result<Cow<'db, [Ref]>, String> {
    match &instance.0 {
        Kind::Allocator(forward) => Ok(Cow::Owned(super::allocator::walk(forward))),
        _ => walked(db, instance).map(Cow::Borrowed),
    }
}

/// Everything the IR of `instance` can request, each once, in the order of its MIR, walked once
/// a revision ([`code_walk`], [`start_walk`]) as rustc's collector has `items_of_instance`: a
/// second asker (`describe`, then `prepare`) gets the list of the first and allocates nothing.
pub(crate) fn walked<'db>(
    db: &'db dyn HirDatabase,
    instance: &HirInstance,
) -> Result<&'db [Ref], String> {
    let walked = match &instance.0 {
        // Its assembly names nothing: `asm::wrapper` refuses a `sym`, and the backend an
        // assembly function with a relocation.
        Kind::Asm(_) => return Ok(&[]),
        Kind::Start(main) => start_walk(db, *main),
        Kind::Allocator(forward) => {
            return Err(format!("{forward:?}: an allocator entry, whose references are listed"));
        }
        Kind::Code(code, env) => code_walk(db, code.clone(), env.clone()),
    };
    walked.as_deref().map_err(|what| format!("the references of {instance:?}: {what}"))
}

// Nothing in its arguments is collected by salsa (no tracked struct, no interned value with
// fewer revisions), so they stay interned and are never verified.
#[salsa::tracked(returns(ref), revisions = usize::MAX)]
fn code_walk<'db>(
    db: &'db dyn HirDatabase,
    instance: Instance,
    env: StoredParamEnvAndCrate,
) -> Result<Vec<Ref>, String> {
    code_refs(db, &instance, &env)
}

#[salsa::tracked(returns(ref))]
fn start_walk<'db>(db: &'db dyn HirDatabase, main: FunctionId) -> Result<Vec<Ref>, String> {
    let env = ParamEnvAndCrate { param_env: ParamEnv::empty(), krate: main.krate(db) };
    scratch(db, env, None, false, |walk| walk.start_shim(main))
}

/// The walk of a function, closure, `async` body or shim, as `instance_code` builds each.
fn code_refs(
    db: &dyn HirDatabase,
    instance: &Instance,
    env: &StoredParamEnvAndCrate,
) -> Result<Vec<Ref>, String> {
    let ty = || instance.args.as_ref().type_at(0);
    let shim_env = env.as_ref(db);
    match instance.def {
        InstanceDef::DropGlue => {
            return scratch(db, shim_env, None, false, |walk| walk.drop_glue(ty()));
        }
        InstanceDef::CallOnceShim => {
            return scratch(db, shim_env, None, false, |walk| walk.call_once_shim(ty()));
        }
        InstanceDef::VtableShim(func) => {
            let args = instance.args.as_ref();
            return scratch(db, shim_env, None, false, |walk| walk.vtable_shim(func, args));
        }
        InstanceDef::FnPointerShim => {
            return scratch(db, shim_env, None, false, |walk| walk.fn_pointer_shim(ty()));
        }
        InstanceDef::ReifyShim { receiver } => {
            return scratch(db, shim_env, None, false, |walk| walk.reify_shim(ty(), receiver));
        }
        InstanceDef::Fn(_)
        | InstanceDef::Closure(_)
        | InstanceDef::Coroutine(_)
        | InstanceDef::CoroutineByMove(_) => (),
    }
    let body = crate::instance_body(db, instance, env.clone()).map_err(told)?;
    let body = &*body;
    let (tracked, abi, variadic) = match instance.def {
        InstanceDef::Fn(func) => {
            let signature = FunctionSignature::of(db, func);
            (track_caller(db, func), signature.abi, signature.is_varargs())
        }
        _ => (false, ExternAbi::Rust, false),
    };
    let args = instance.args.clone();
    scratch(db, env.as_ref(db), Some(body), tracked, |walk| {
        walk.fx.abi = abi;
        walk.fx.variadic = variadic;
        walk.fx.args = Some(args);
        walk.body(body)
    })
}

/// Runs `work` on a [`FunctionCx`] for `body` in `env` whose function is never built: what
/// its resolvers ask for is all that is kept of it.
fn scratch<'db>(
    db: &'db dyn HirDatabase,
    env: ParamEnvAndCrate<'db>,
    body: Option<&'db MirBody<'db>>,
    tracked: bool,
    work: impl for<'w, 'cx> FnOnce(&mut Walk<'w, 'cx, 'db>) -> Result<(), String>,
) -> Result<Vec<Ref>, String> {
    let isa = &**crate::target().map_err(told)?;
    let target = crate::selected_target().map_err(told)?;
    let mut cg = Recorder::new(db, isa, target, true);
    // The settings a walk reads are its own, never the thread's unit's: the walk is a query.
    cg.unit = Some(Build { debug_assertions: true, aborts: false });
    // About one reference a block, and the few of `outside`.
    let capacity = body.map_or(0, |it| it.basic_blocks.len()) + 8;
    let mut walked = None;
    crate::build(&mut cg, env, body, tracked, |fx| {
        // The union of what the settings of a unit compile.
        fx.aborts = false;
        fx.debug_assertions = true;
        fx.ub_checks = true;
        let mut walk = Walk {
            fx,
            homes: Homes::default(),
            env: env.store(),
            listed: Listed::with_capacity(capacity),
            called: FxHashSet::with_capacity_and_hasher(capacity, Default::default()),
            drained: (0, 0),
        };
        let done = walk.outside().and_then(|()| work(&mut walk)).and_then(|()| walk.drain());
        walked = Some(done.map(|()| {
            let mut refs = walk.listed.refs;
            refs.shrink_to_fit();
            refs
        }));
        Ok(())
    })
    .map_err(told)?;
    walked.expect("the walk ran")
}

fn told(error: CodegenError) -> String {
    error.to_string()
}

/// A walk of one instance.
pub(super) struct Walk<'w, 'cx, 'db> {
    fx: &'w mut FunctionCx<'cx, 'db>,
    homes: Homes,
    /// `fx.env`, as the requests hold it.
    env: StoredParamEnvAndCrate,
    listed: Listed,
    /// The calls listed already, by their interned definition and arguments: a call made
    /// again costs a hash of two ids and no homing (`places::call_ref`).
    called: FxHashSet<(InstanceDef, GenericArgs<'db>)>,
    /// How many of the function and data requests of `fx.cg` are in `listed`.
    drained: (usize, usize),
}

/// The references found so far, each once, in the order found, as rustc's collector keeps the
/// items of an instance in an index set. Each is hashed once (FxHash) and kept once: the
/// tables hold its hash and place (16 bytes an entry, one probe a lookup), and growing them
/// hashes nothing again. A symbol is looked up by its name, so one listed already makes no
/// `String`, nor does a routine of Cranelift's listed already.
struct Listed {
    refs: Vec<Ref>,
    /// The places in `refs` of all but the symbols.
    at: HashTable<Slot>,
    /// The places in `refs` of the symbols ([`FrontRef::Extern`]), by the hash of the name.
    symbols: HashTable<Slot>,
    libcalls: SmallVec<[ir::LibCall; 8]>,
}

#[derive(Clone, Copy)]
struct Slot {
    hash: u64,
    at: u32,
}

const _: () = assert!(size_of::<Slot>() == 16);

impl Listed {
    fn with_capacity(capacity: usize) -> Self {
        Listed {
            refs: Vec::with_capacity(capacity),
            at: HashTable::with_capacity(capacity),
            symbols: HashTable::with_capacity(8),
            libcalls: SmallVec::new(),
        }
    }

    fn next(&self) -> u32 {
        u32::try_from(self.refs.len()).expect("fewer than 2^32 references")
    }

    fn add(&mut self, it: Ref) {
        let it = match it {
            FrontRef::Extern(name) => return self.symbol(Cow::Owned(name)),
            it => it,
        };
        let hash = FxBuildHasher.hash_one(&it);
        let at = self.next();
        let refs = &self.refs;
        let entry = self.at.entry(hash, |slot| refs[slot.at as usize] == it, |slot| slot.hash);
        if let Entry::Vacant(vacant) = entry {
            vacant.insert(Slot { hash, at });
            self.refs.push(it);
        }
    }

    fn symbol(&mut self, name: Cow<'_, str>) {
        let hash = FxBuildHasher.hash_one(&*name);
        let at = self.next();
        let refs = &self.refs;
        let same =
            |slot: &Slot| matches!(&refs[slot.at as usize], FrontRef::Extern(it) if **it == *name);
        if let Entry::Vacant(vacant) = self.symbols.entry(hash, same, |slot| slot.hash) {
            vacant.insert(Slot { hash, at });
            self.refs.push(FrontRef::Extern(name.into_owned()));
        }
    }

    fn libcall(&mut self, libcall: ir::LibCall) {
        if !self.libcalls.contains(&libcall) {
            self.libcalls.push(libcall);
            self.symbol(Cow::Owned(places::libcall_symbol(libcall)));
        }
    }
}

/// A set of the blocks of a body, a bit a block.
struct Blocks(Vec<u64>);

impl Blocks {
    fn new(count: usize) -> Self {
        Blocks(vec![0; count.div_ceil(64)])
    }

    fn bit(id: BasicBlockId) -> (usize, u64) {
        let index = id.into_raw().into_u32() as usize;
        (index / 64, 1 << (index % 64))
    }

    /// Adds `id`, saying whether it was not in the set.
    fn insert(&mut self, id: BasicBlockId) -> bool {
        let (word, bit) = Self::bit(id);
        let new = self.0[word] & bit == 0;
        self.0[word] |= bit;
        new
    }

    fn contains(&self, id: BasicBlockId) -> bool {
        let (word, bit) = Self::bit(id);
        self.0[word] & bit != 0
    }
}

impl<'cx, 'db> Walk<'_, 'cx, 'db> {
    pub(super) fn db(&self) -> &'db dyn HirDatabase {
        self.fx.cg.db
    }

    /// Lists what the resolvers have asked of the recorder since last time, as the IR
    /// builder's requests are placed: a function by its target, a static as itself or its
    /// symbol, with what Cranelift calls to reach a thread-local one.
    fn drain(&mut self) -> Result<(), String> {
        let Walk { fx, homes, listed, drained, .. } = self;
        let db = fx.cg.db;
        let requests = &fx.cg.wants.requests;
        for request in &requests.funcs[drained.0..] {
            match &request.target {
                FuncTarget::Symbol(symbol) => listed.symbol(Cow::Borrowed(symbol.as_str())),
                target => listed.add(places::func_ref(db, homes, target)?),
            }
        }
        for request in &requests.datas[drained.1..] {
            if let DataRequest::Static(id) = request {
                listed.add(places::static_ref(db, *id)?);
                if is_thread_local(db, *id) {
                    listed.libcall(ir::LibCall::ElfTlsGetAddr);
                }
            }
        }
        *drained = (requests.funcs.len(), requests.datas.len());
        Ok(())
    }

    /// Runs one of the IR builder's resolvers, which builds no code, and lists what it asked
    /// for.
    pub(super) fn recorded<T>(
        &mut self,
        work: impl FnOnce(&mut FunctionCx<'cx, 'db>) -> crate::Result<T>,
    ) -> Result<T, String> {
        let out = work(&mut *self.fx).map_err(told)?;
        self.drain()?;
        Ok(out)
    }

    pub(super) fn add(&mut self, it: Ref) -> Result<(), String> {
        self.drain()?;
        self.listed.add(it);
        Ok(())
    }

    /// A call of `def` with `args`, as `Recorder::callee` asks for it.
    pub(super) fn call(&mut self, def: InstanceDef, args: GenericArgs<'db>) -> Result<(), String> {
        self.drain()?;
        if !self.called.insert((def, args)) {
            return Ok(());
        }
        let it = places::call_ref(self.fx.cg.db, &mut self.homes, def, args, &self.env)?;
        self.listed.add(it);
        Ok(())
    }

    /// A function of no crate of the build, by its symbol: `Recorder::import`.
    pub(super) fn symbol(&mut self, name: &str) -> Result<(), String> {
        self.drain()?;
        self.listed.symbol(Cow::Borrowed(name));
        Ok(())
    }

    /// A routine Cranelift calls itself, which no request names.
    pub(super) fn libcall(&mut self, libcall: ir::LibCall) -> Result<(), String> {
        self.drain()?;
        self.listed.libcall(libcall);
        Ok(())
    }

    /// The function of assembly `text`: `Recorder::assembly`.
    pub(super) fn assembly(&mut self, text: String) -> Result<(), String> {
        self.add(FrontRef::Function(HirInstance(Kind::Asm(text))))
    }

    /// What any function may name without asking: the copies Cranelift makes calls of, and
    /// what a landing pad needs.
    fn outside(&mut self) -> Result<(), String> {
        for libcall in [ir::LibCall::Memcpy, ir::LibCall::Memmove, ir::LibCall::Memset] {
            self.libcall(libcall)?;
        }
        self.symbol(places::PERSONALITY)?;
        let resume = match self.fx.cg.target.exception_in_register() {
            true => "_Unwind_Resume",
            false => crate::unwind::seh::RESUME,
        };
        self.symbol(resume)
    }

    /// The type of a place, as `place_ref` finds it.
    pub(super) fn place_ty(&self, place: &StoredPlace) -> PlaceTy<'db> {
        let fx = &*self.fx;
        let body = fx.body.expect("MIR body");
        let mut ty = PlaceTy::from_ty(body.locals[place.local].ty.as_ref());
        ty.ty = fx.normalize(ty.ty);
        for proj in place.projection.as_slice() {
            ty = ty.projection_ty(&fx.infcx, proj, fx.env.param_env);
            ty.ty = fx.normalize(ty.ty);
        }
        ty
    }

    pub(super) fn operand_ty(&self, operand: &Operand) -> Ty<'db> {
        match &operand.kind {
            OperandKind::Copy(place) | OperandKind::Move(place) => self.place_ty(place).ty,
            OperandKind::Constant { ty, .. } => ty.as_ref(),
            OperandKind::Allocation { allocation } => allocation.as_ref().ty,
            OperandKind::Static(id) => self.fx.static_ty(*id),
        }
    }

    /// What reading `operand` asks for, as `FunctionCx::operand` reads it: a constant's data
    /// and what it points to, a static. Its type.
    pub(super) fn operand(&mut self, operand: &Operand) -> Result<Ty<'db>, String> {
        match &operand.kind {
            OperandKind::Copy(place) | OperandKind::Move(place) => Ok(self.place_ty(place).ty),
            OperandKind::Constant { konst, ty } => {
                let ty = ty.as_ref();
                if self.located(operand) {
                    return Ok(ty);
                }
                match konst.as_ref().kind() {
                    ConstKind::Value(_) if is_integer(ty) => (),
                    ConstKind::Value(value) => {
                        self.recorded(|fx| fx.valtree_blob(ty, value.value))?;
                    }
                    ConstKind::Unevaluated(unevaluated) => {
                        self.recorded(|fx| {
                            let allocation = fx.eval_const(unevaluated)?;
                            fx.allocation_blob(allocation, false)
                        })?;
                    }
                    kind => return Err(format!("constant {kind:?}")),
                }
                Ok(ty)
            }
            OperandKind::Allocation { allocation } => {
                let allocation = allocation.as_ref();
                let ty = allocation.ty;
                if !self.located(operand) {
                    self.recorded(|fx| fx.allocation_blob(allocation, false))?;
                }
                Ok(ty)
            }
            OperandKind::Static(id) => {
                self.static_(*id)?;
                Ok(self.fx.static_ty(*id))
            }
        }
    }

    pub(super) fn operands(&mut self, operands: &[Operand]) -> Result<Tys<'db>, String> {
        operands.iter().map(|it| self.operand(it)).collect()
    }

    /// Whether `operand` is what `line!()`, `column!()`, `file!()` or a `concat!` of them
    /// expanded to, which `builtin_location` makes of where it is: data that points nowhere
    /// else, a literal's when it does not.
    fn located(&self, operand: &Operand) -> bool {
        let (Some(MirSpan::ExprId(expr)), Some(body)) = (operand.span(), self.fx.body) else {
            return false;
        };
        let InferBodyId::DefWithBodyId(owner) = body.owner else { return false };
        self.fx.location_macro(body, expr).is_some()
            || located_concats(self.db(), owner).contains_key(&expr)
    }

    pub(super) fn static_(&mut self, id: StaticId) -> Result<(), String> {
        self.recorded(|fx| {
            fx.cg.static_data(id);
            Ok(())
        })
    }

    /// The panic function of core at `path`, as `call_panic` calls it.
    pub(super) fn panic(&mut self, path: &[&str]) -> Result<(), String> {
        match self.fx.core_function(path) {
            Some(func) => self.call(InstanceDef::Fn(func), GenericArgs::new_from_slice(&[])),
            None => Err(format!(
                "a call of rustc's `core::{}`, which a build without rustc has none of",
                path.join("::")
            )),
        }
    }

    /// `drop_value`: the drop glue of `ty`, when dropping it does anything.
    pub(super) fn drop_value(&mut self, ty: Ty<'db>) -> Result<(), String> {
        let ty = self.fx.normalize(ty);
        if self.fx.glue_of(ty) == DropGlue::None {
            return Ok(());
        }
        self.call(InstanceDef::DropGlue, GenericArgs::new_from_slice(&[ty.into()]))
    }

    /// `call_direct` of `func` with arguments of `arg_tys`.
    pub(super) fn call_direct(
        &mut self,
        func: FunctionId,
        generic_args: GenericArgs<'db>,
        arg_tys: &[Ty<'db>],
        dst_ty: Ty<'db>,
    ) -> Result<(), String> {
        let db = self.db();
        let lang_items = self.fx.interner.lang_items();
        if Some(func) == lang_items.FuturePoll
            && let Some((def, parent_args)) =
                coroutine_instance(db, self.fx.normalize(generic_args.type_at(0)))
        {
            return self.emit_call(def, parent_args, arg_tys, dst_ty);
        }
        // Through the object's vtable, which the coercion to it listed.
        if is_dyn_method(self.fx.interner, self.fx.env.param_env, func, generic_args).is_some() {
            return Ok(());
        }
        let (callee, callee_args) = db.lookup_impl_method(self.fx.env, func, generic_args);
        let either::Either::Left(callee) = callee else {
            return Err("call to builtin derive method".to_owned());
        };
        if callee == func
            && let ItemContainerId::TraitId(trait_) = func.lookup(db).container
            && let [_] = arg_tys
        {
            if Some(trait_) == lang_items.Clone
                && FunctionSignature::of(db, func).name.as_str() == "clone"
            {
                return self.clone_builtin(func, dst_ty);
            }
            if Some(trait_) == lang_items.FnPtrTrait {
                return Ok(());
            }
        }
        self.emit_call(InstanceDef::Fn(callee), callee_args, arg_tys, dst_ty)
    }

    /// `clone_builtin`: each part cloned, and the ones cloned before dropped when one unwinds.
    fn clone_builtin(&mut self, clone: FunctionId, ty: Ty<'db>) -> Result<(), String> {
        let interner = self.fx.interner;
        let part = |this: &mut Self, part: Ty<'db>| {
            let reference =
                Ty::new_ref(interner, Region::new_erased(interner), part, Mutability::Not);
            let args = GenericArgs::new_from_slice(&[part.into()]);
            this.call_direct(clone, args, &[reference], part)
        };
        let fields = match ty.kind() {
            TyKind::Tuple(fields) => Some(fields),
            TyKind::Closure(..) | TyKind::CoroutineClosure(..) => {
                match ty.closure_upvars_ty().map(|it| it.kind()) {
                    Some(TyKind::Tuple(fields)) => Some(fields),
                    _ => return Err("builtin `Clone` of a closure without tupled upvars".to_owned()),
                }
            }
            _ => None,
        };
        if let Some(fields) = fields {
            for field in fields.iter() {
                part(self, field)?;
                self.drop_value(field)?;
            }
            return Ok(());
        }
        match ty.kind() {
            TyKind::Array(element, _) => {
                part(self, element)?;
                self.drop_value(element)
            }
            TyKind::FnDef(..)
            | TyKind::FnPtr(..)
            | TyKind::Never
            | TyKind::RawPtr(..)
            | TyKind::Ref(_, _, Mutability::Not) => Ok(()),
            _ => Err(format!("builtin `Clone` of {ty:?}")),
        }
    }

    /// `emit_call` of an instance: the instance, and for one that takes `...`, the function
    /// the call goes through.
    pub(super) fn emit_call(
        &mut self,
        def: InstanceDef,
        args: GenericArgs<'db>,
        arg_tys: &[Ty<'db>],
        dst_ty: Ty<'db>,
    ) -> Result<(), String> {
        self.call(def, args)?;
        let InstanceDef::Fn(func) = def else { return Ok(()) };
        let db = self.db();
        let signature = FunctionSignature::of(db, func);
        if !signature.is_varargs() {
            return Ok(());
        }
        self.variadic_call(signature.abi, arg_tys, dst_ty, track_caller(db, func))
    }

    /// `emit_call` through a pointer: `named` is how many of the arguments are for named
    /// parameters, when the rest are for `...`.
    pub(super) fn emit_pointer_call(
        &mut self,
        abi: ExternAbi,
        named: Option<usize>,
        arg_tys: &[Ty<'db>],
        dst_ty: Ty<'db>,
    ) -> Result<(), String> {
        match named {
            Some(_) => self.variadic_call(abi, arg_tys, dst_ty, false),
            None => Ok(()),
        }
    }

    /// The function `counted_variadic` sends a call of a variadic function through, where the
    /// System V convention of x86-64 says how many vector registers hold arguments.
    fn variadic_call(
        &mut self,
        abi: ExternAbi,
        arg_tys: &[Ty<'db>],
        dst_ty: Ty<'db>,
        tracked: bool,
    ) -> Result<(), String> {
        let target = self.fx.cg.target;
        if Target::current().is_wasm()
            || target.call_conv != cranelift_codegen::isa::CallConv::SystemV
            || target.arch != Arch::X86_64
        {
            return Ok(());
        }
        let FnAbi { ret, args: modes } =
            self.fx.fn_abi(abi, dst_ty, arg_tys.iter().copied()).map_err(told)?;
        let mut sig = self.fx.signature(&modes, ret);
        if tracked {
            sig.params.push(ir::AbiParam::new(crate::ptr()));
        }
        if !sig.params.iter().any(|param| param.value_type.is_float()) {
            return Ok(());
        }
        let (mut ints, mut floats, mut stack) = (0, 0, 0);
        for param in &sig.params {
            match param.purpose {
                ArgumentPurpose::Normal | ArgumentPurpose::StructReturn => (),
                _ => return Err("an argument in memory with a float to a variadic function".into()),
            }
            match param.value_type {
                ty if ty.is_float() && floats < 8 => floats += 1,
                ty if ty.is_float() => stack += 1,
                ty if ty.is_int() && ty.bytes() <= 8 && ints < 6 => ints += 1,
                ty if ty.is_int() && ty.bytes() <= 8 => stack += 1,
                _ => return Err("a float and a wide argument to a variadic function".into()),
            }
        }
        self.assembly(asm::counted_variadic(ints, stack))
    }

    /// The body of a function, closure or `async` block, as `FunctionCx::codegen` builds it.
    fn body(&mut self, body: &'db MirBody<'db>) -> Result<(), String> {
        if body.phase != MirPhase::Runtime {
            return Err("a body whose drops are not elaborated".to_owned());
        }
        self.bfloat(body)?;
        self.promoted(body)?;
        if body.coroutine.is_none() && self.neon_fused()? {
            return Ok(());
        }
        let (reachable, decided) = self.reachable(body);
        if body.coroutine.is_some() {
            self.enter_coroutine(body)?;
        }
        for (id, block) in body.basic_blocks.iter() {
            if !reachable.contains(id) {
                continue;
            }
            for statement in &block.statements {
                self.fx.span = statement.span;
                match &statement.kind {
                    StatementKind::Assign(place, rvalue) => self.rvalue(place, rvalue)?,
                    StatementKind::InlineAsm(asm) => self.inline_asm(asm)?,
                    StatementKind::Deinit(_) => return Err("deinit".to_owned()),
                    StatementKind::StorageLive(_)
                    | StatementKind::StorageDead(_)
                    | StatementKind::FakeRead(_)
                    | StatementKind::Nop => (),
                }
            }
            let Some(terminator) = &block.terminator else {
                return Err("block without terminator".to_owned());
            };
            self.fx.span = terminator.span;
            if !decided.contains(id) {
                self.terminator(&terminator.kind)?;
            }
        }
        Ok(())
    }

    /// The blocks `codegen` compiles with every check on and unwinding, which are all that it
    /// compiles with any setting: a check that is off only decides a switch, and `panic =
    /// "abort"` only leaves out cleanup blocks. With the blocks whose switch is decided, which
    /// only jump.
    fn reachable(&mut self, body: &'db MirBody<'db>) -> (Blocks, Blocks) {
        let off = self.fx.checks_off(body);
        let resumes_only = match body.coroutine {
            Some(_) => FxHashSet::default(),
            None => FunctionCx::resumes_only(body),
        };
        let count = body.basic_blocks.len();
        let mut reachable = Blocks::new(count);
        let mut decided = Blocks::new(count);
        let mut worklist = Vec::with_capacity(count);
        worklist.push(body.start_block);
        while let Some(id) = worklist.pop() {
            if !reachable.insert(id) {
                continue;
            }
            match body.basic_blocks[id].terminator.as_ref().map(|it| &it.kind) {
                Some(TerminatorKind::SwitchInt { discr, targets }) => {
                    match self.fx.switch_bits(&body.basic_blocks[id], discr, &off) {
                        Some(bits) => {
                            decided.insert(id);
                            worklist.push(targets.target_for_value(bits))
                        }
                        None => worklist.extend_from_slice(targets.all_targets()),
                    }
                }
                Some(
                    TerminatorKind::Goto { target }
                    | TerminatorKind::Drop { target, .. }
                    | TerminatorKind::DropAndReplace { target, .. }
                    | TerminatorKind::Assert { target, .. }
                    | TerminatorKind::FalseEdge { real_target: target, .. }
                    | TerminatorKind::FalseUnwind { real_target: target, .. },
                ) => worklist.push(*target),
                Some(TerminatorKind::Call { target, .. }) => worklist.extend(*target),
                Some(TerminatorKind::Yield { resume, drop, .. }) => {
                    worklist.push(*resume);
                    worklist.extend(*drop);
                }
                _ => (),
            }
            let block = &body.basic_blocks[id];
            let unwind = block.terminator.as_ref().and_then(|it| it.kind.unwind());
            worklist.extend(unwind.filter(|it| !resumes_only.contains(it)));
        }
        (reachable, decided)
    }

    /// `enter_coroutine`: an `async` body dropped before it ran drops what it captured.
    fn enter_coroutine(&mut self, body: &'db MirBody<'db>) -> Result<(), String> {
        let receiver = body.param_locals[0];
        let Some(ty) = body.locals[receiver].ty.as_ref().builtin_deref(true) else {
            return Err("async body without a reference to its future".to_owned());
        };
        let TyKind::Coroutine(_, args) = ty.kind() else {
            return Err(format!("async body of {ty:?}"));
        };
        for field in args.as_coroutine().tupled_upvars_ty().tuple_fields().iter() {
            self.drop_value(field)?;
        }
        Ok(())
    }

    /// The read-only data of each promoted local, as `promoted_constant` makes it. Its
    /// requests are made whether or not the data is then kept: a local the data is not kept
    /// for, or that is kept in a register, is assigned by its statements, which the walk of
    /// the blocks lists too.
    fn promoted(&mut self, body: &'db MirBody<'db>) -> Result<(), String> {
        let db = self.db();
        for &(local, expr) in &body.promoted_locals {
            let Ok(layout) = self.fx.layout(body.locals[local].ty.as_ref()) else { continue };
            if layout.size.bytes() == 0 || self.fx.mentions_location(body, expr) {
                continue;
            }
            let env = self.fx.env;
            let allocation = (|| {
                let constant = monomorphized_promoted_mir_body_query(
                    db,
                    body.owner,
                    expr,
                    self.fx.args.clone()?,
                    env.store(),
                )
                .ok()?;
                interpret_mir(db, constant, false, Some(env)).ok()?.0.ok()
            })();
            if let Some(allocation) = allocation {
                // A constant whose data can't be made is not promoted, and leaves what it
                // asked for before then.
                _ = self.fx.allocation_blob(allocation, false);
                self.drain()?;
            }
        }
        Ok(())
    }

    /// `assign`: what the value assigned asks for.
    fn rvalue(&mut self, place: &StoredPlace, rvalue: &Rvalue) -> Result<(), String> {
        match rvalue {
            Rvalue::Use(operand) | Rvalue::Repeat(operand, _) => {
                self.operand(operand)?;
            }
            Rvalue::CopyForDeref(_)
            | Rvalue::Ref(..)
            | Rvalue::Len(_)
            | Rvalue::Discriminant(_) => (),
            Rvalue::BinaryOp(op, lhs, rhs) => {
                self.operand(lhs)?;
                self.operand(rhs)?;
                self.binary_op(op, lhs, rhs)?;
            }
            Rvalue::UnaryOp(op, operand) => {
                self.operand(operand)?;
                self.unary_op(op, operand)?;
            }
            Rvalue::Cast(kind, operand, target) => {
                let from = self.operand(operand)?;
                self.cast(kind, from, target.as_ref())?;
            }
            Rvalue::Aggregate(_, operands) => {
                self.operands(operands)?;
            }
            Rvalue::CheckedBinaryOp(..) => {
                let dst = self.place_ty(place).ty;
                return Err(format!("rvalue {rvalue:?} of {dst:?}"));
            }
            Rvalue::ThreadLocalRef(never) | Rvalue::AddressOf(never) | Rvalue::NullaryOp(never) => {
                match *never {}
            }
        }
        Ok(())
    }

    /// `cast`: an unsizing coercion's vtables, a function pointer's function, a conversion's
    /// routines.
    fn cast(&mut self, kind: &CastKind, from: Ty<'db>, to: Ty<'db>) -> Result<(), String> {
        match kind {
            CastKind::PointerCoercion(PointerCast::Unsize) => self.unsize(from, to),
            CastKind::PointerCoercion(
                PointerCast::ReifyFnPointer | PointerCast::ClosureFnPointer(_),
            ) => self.recorded(|fx| fx.fn_item(from)).map(drop),
            CastKind::PointerCoercion(
                PointerCast::MutToConstPointer
                | PointerCast::ArrayToPointer
                | PointerCast::UnsafeFnPointer,
            )
            | CastKind::IntToInt
            | CastKind::PtrToPtr
            | CastKind::PointerExposeAddress
            | CastKind::PointerFromExposedAddress
            | CastKind::FnPtrToPtr => Ok(()),
            CastKind::IntToFloat | CastKind::FloatToInt | CastKind::FloatToFloat => {
                self.float_cast(kind, from, to)
            }
            _ => Err(format!("cast {kind:?}")),
        }
    }

    /// `unsize`: the vtable of each pointer that becomes one to a `dyn`.
    pub(super) fn unsize(&mut self, from_ty: Ty<'db>, to_ty: Ty<'db>) -> Result<(), String> {
        if from_ty == to_ty {
            return Ok(());
        }
        match (from_ty.kind(), to_ty.kind()) {
            (
                TyKind::RawPtr(from, _) | TyKind::Ref(_, from, _),
                TyKind::RawPtr(to, _) | TyKind::Ref(_, to, _),
            ) => self.unsize_metadata(from, to),
            (TyKind::Pat(from, _), TyKind::Pat(to, _)) => self.unsize(from, to),
            (TyKind::Adt(def, args), TyKind::Adt(to_def, to_args))
                if def.def_id() == to_def.def_id() =>
            {
                let AdtId::StructId(id) = def.def_id() else {
                    return Err(format!("unsizing {from_ty:?} to {to_ty:?}"));
                };
                let interner = self.fx.interner;
                let field_types = self.db().field_types(id.into());
                for (_, field) in field_types.iter() {
                    let from = field.ty().instantiate(interner, args).skip_norm_wip();
                    let to = field.ty().instantiate(interner, to_args).skip_norm_wip();
                    let (from, to) = (self.fx.normalize(from), self.fx.normalize(to));
                    if self.fx.layout(to).map_err(told)?.size.bytes() == 0 {
                        continue;
                    }
                    self.unsize(from, to)?;
                }
                Ok(())
            }
            _ => Err(format!("unsizing {from_ty:?} to {to_ty:?}")),
        }
    }

    /// `unsize_metadata`: a pointee that becomes a `dyn` has the vtable of its type, made of
    /// its methods and drop glue.
    fn unsize_metadata(&mut self, from: Ty<'db>, to: Ty<'db>) -> Result<(), String> {
        match (from.kind(), to.kind()) {
            (TyKind::Array(..), TyKind::Slice(_)) => Ok(()),
            // The vtable it is upcast to is in the one it has.
            (TyKind::Dynamic(..), TyKind::Dynamic(..)) => Ok(()),
            (_, TyKind::Dynamic(..)) => {
                if self.fx.is_unsized(from) {
                    return Err(format!("unsizing {from:?} to {to:?}"));
                }
                self.recorded(|fx| {
                    let trait_ref = fx.principal(to, from);
                    fx.vtable(from, trait_ref)
                })
                .map(drop)
            }
            (TyKind::Adt(def, args), TyKind::Adt(to_def, to_args))
                if def.def_id() == to_def.def_id() =>
            {
                let AdtId::StructId(id) = def.def_id() else {
                    return Err(format!("unsizing {from:?}"));
                };
                let field_types = self.db().field_types(id.into());
                let Some((_, last)) = field_types.iter().last() else {
                    return Err("unsizing a struct without fields".to_owned());
                };
                let interner = self.fx.interner;
                let from = last.ty().instantiate(interner, args).skip_norm_wip();
                let to = last.ty().instantiate(interner, to_args).skip_norm_wip();
                let (from, to) = (self.fx.normalize(from), self.fx.normalize(to));
                self.unsize_metadata(from, to)
            }
            _ => Err(format!("unsizing {from:?} to {to:?}")),
        }
    }

    /// `terminator`, of a block whose switch is not decided.
    fn terminator(&mut self, kind: &TerminatorKind) -> Result<(), String> {
        match kind {
            TerminatorKind::Goto { .. }
            | TerminatorKind::FalseEdge { .. }
            | TerminatorKind::FalseUnwind { .. }
            | TerminatorKind::Return
            | TerminatorKind::Yield { .. }
            | TerminatorKind::Unreachable
            | TerminatorKind::CoroutineDrop
            // `outside` has what it resumes with.
            | TerminatorKind::UnwindResume => Ok(()),
            TerminatorKind::SwitchInt { discr, .. } => {
                if self.fx.const_bits(discr).is_none() {
                    self.operand(discr)?;
                }
                Ok(())
            }
            TerminatorKind::Drop { place, .. } => self.drop_value(self.place_ty(place).ty),
            TerminatorKind::Assert { msg, .. } => self.assert(msg),
            TerminatorKind::Call { func, args, destination, target, .. } => {
                // A promoted call already holds its value.
                if target.is_some() && self.fx.constant_locals.contains(&destination.local) {
                    return Ok(());
                }
                self.call_terminator(func, args, destination)
            }
            TerminatorKind::DropAndReplace { .. } | TerminatorKind::Abort => {
                Err(format!("terminator {kind:?}"))
            }
        }
    }

    /// The panic an `Assert` makes when it fails, which a unit makes where it checks: an
    /// overflow check is made by a unit with overflow checks only, and listed all the same.
    fn assert(&mut self, msg: &AssertKind) -> Result<(), String> {
        let name = match msg {
            AssertKind::BoundsCheck { .. } => {
                return self.panic(&["panicking", "panic_bounds_check"]);
            }
            AssertKind::Overflow(BinOp::Add, ..) => "panic_const_add_overflow",
            AssertKind::Overflow(BinOp::Sub, ..) => "panic_const_sub_overflow",
            AssertKind::Overflow(BinOp::Mul, ..) => "panic_const_mul_overflow",
            AssertKind::Overflow(BinOp::Div, ..) => "panic_const_div_overflow",
            AssertKind::Overflow(BinOp::Rem, ..) => "panic_const_rem_overflow",
            AssertKind::Overflow(BinOp::Shl, ..) => "panic_const_shl_overflow",
            AssertKind::Overflow(BinOp::Shr, ..) => "panic_const_shr_overflow",
            // A unit that makes this check refuses it, naming it.
            AssertKind::Overflow(..) if msg.is_optional_overflow_check() => return Ok(()),
            AssertKind::Overflow(op, ..) => return Err(format!("overflow check of {op:?}")),
            AssertKind::OverflowNeg(_) => "panic_const_neg_overflow",
            AssertKind::DivisionByZero(_) => "panic_const_div_by_zero",
            AssertKind::RemainderByZero(_) => "panic_const_rem_by_zero",
        };
        self.panic(&["panicking", "panic_const", name])
    }

    /// `call_with`: what a call terminator calls, as the IR builder sorts it out.
    fn call_terminator(
        &mut self,
        func: &Operand,
        args: &[Operand],
        destination: &StoredPlace,
    ) -> Result<(), String> {
        let dst_ty = self.place_ty(destination).ty;
        let fn_ty = self.operand_ty(func);
        if let TyKind::FnPtr(..) = fn_ty.kind() {
            self.operand(func)?;
            let arg_tys = self.operands(args)?;
            let sig = fn_ty.fn_sig(self.fx.interner);
            let named = sig.c_variadic().then(|| sig.inputs().skip_binder().len());
            return self.emit_pointer_call(sig.abi(), named, &arg_tys, dst_ty);
        }
        let TyKind::FnDef(def, generic_args) = fn_ty.kind() else {
            return Err(format!("call of {fn_ty:?}"));
        };
        let func = match def.0 {
            CallableDefId::FunctionId(func) => func,
            CallableDefId::StructId(_) | CallableDefId::EnumVariantId(_) => {
                self.operands(args)?;
                return Ok(());
            }
        };
        let db = self.db();
        if fncache::is_code(db, func) {
            return Ok(());
        }
        let lang_items = self.fx.interner.lang_items();
        if fn_trait_methods(lang_items).iter().any(|family| family.contains(&Some(func))) {
            return self.call_fn_trait(func, generic_args, args, dst_ty);
        }
        if [lang_items.DropInPlace, lang_items.DropGlue].contains(&Some(func)) {
            let [pointer] = args else {
                return Err(format!("drop glue with {} arguments", args.len()));
            };
            self.operand(pointer)?;
            return self.drop_value(generic_args.type_at(0));
        }
        let signature = FunctionSignature::of(db, func);
        let intrinsic = FunctionSignature::is_intrinsic(db, func);
        if intrinsic && signature.name.as_str() == "const_eval_select" {
            let [tuple, _, runtime] = args else {
                return Err(format!("const_eval_select with {} arguments", args.len()));
            };
            let TyKind::FnDef(runtime, runtime_args) = self.operand_ty(runtime).kind() else {
                return Err("const_eval_select with a non-fn-item runtime function".to_owned());
            };
            let CallableDefId::FunctionId(runtime) = runtime.0 else {
                return Err("const_eval_select with a constructor".to_owned());
            };
            let tuple_ty = self.operand(tuple)?;
            let TyKind::Tuple(fields) = tuple_ty.kind() else {
                return Err("const_eval_select with non-tuple arguments".to_owned());
            };
            let fields: Tys<'db> = fields.iter().collect();
            return self.call_direct(runtime, runtime_args, &fields, dst_ty);
        }
        if func.krate(db).data(db).origin.is_lang()
            && matches!(signature.name.as_str(), "check_language_ub" | "check_library_ub")
        {
            return Ok(());
        }
        if intrinsic {
            if self.intrinsic(func, generic_args, args, dst_ty)? {
                return Ok(());
            }
            let overridden = AttrFlags::query(db, func.into())
                .contains(AttrFlags::RUSTC_INTRINSIC_MUST_BE_OVERRIDDEN);
            if overridden || crate::compile_time_only(db, func) || !signature.has_body() {
                return Err(format!("intrinsic `{}`", signature.name.as_str()));
            }
        }
        if let Some(name) = crate::link_name(db, func)
            && let Some(name) = name.strip_prefix("llvm.")
        {
            return self.llvm_intrinsic(name, args, dst_ty);
        }
        self.inlined_fma(func, dst_ty)?;
        let arg_tys = self.operands(args)?;
        self.call_direct(func, generic_args, &arg_tys, dst_ty)
    }

    /// `call_fn_trait`: `Fn::call`, `FnMut::call_mut` or `FnOnce::call_once` of what MIR
    /// passes first, with the arguments one by one or as their tuple.
    fn call_fn_trait(
        &mut self,
        method: FunctionId,
        method_args: GenericArgs<'db>,
        args: &[Operand],
        dst_ty: Ty<'db>,
    ) -> Result<(), String> {
        let Some((callee, rest)) = args.split_first() else {
            return Err("Fn-trait call without a callee".to_owned());
        };
        // A `dyn FnOnce` called by value: through its vtable's `call_once` slot.
        if let OperandKind::Copy(place) | OperandKind::Move(place) = &callee.kind
            && let TyKind::Dynamic(..) = self.place_ty(place).ty.kind()
        {
            self.operands(rest)?;
            return Ok(());
        }
        let db = self.db();
        let interner = self.fx.interner;
        let mut ty = self.operand(callee)?;
        let mut arg_tys = self.operands(rest)?;
        let tuple_ty = method_args.type_at(1);
        let spread = match (&arg_tys[..], tuple_ty.kind()) {
            (&[only], TyKind::Tuple(fields)) if only == tuple_ty => Some(fields),
            _ => None,
        };
        if let Some(fields) = spread {
            arg_tys = fields.iter().collect();
        }
        let mut by_ref = None;
        while let TyKind::Ref(_, inner, mutability) = ty.kind() {
            if self.fx.implements_fn_itself(method, ty, tuple_ty) {
                break;
            }
            by_ref = Some((ty, mutability));
            if let TyKind::Dynamic(..) = inner.kind() {
                if self.fx.fn_method_of_object(inner).is_none() {
                    return Err(format!("Fn-trait call on {inner:?}"));
                }
                // Through the object's vtable.
                return Ok(());
            }
            ty = inner;
        }
        match ty.kind() {
            TyKind::FnDef(def, generic_args) => match def.0 {
                CallableDefId::FunctionId(func) => {
                    if FunctionSignature::is_intrinsic(db, func) {
                        return Err("intrinsic called through an Fn trait".to_owned());
                    }
                    self.call_direct(func, generic_args, &arg_tys, dst_ty)
                }
                CallableDefId::StructId(_) | CallableDefId::EnumVariantId(_) => Ok(()),
            },
            // Called with no `...`, through the pointer.
            TyKind::FnPtr(..) => Ok(()),
            TyKind::Closure(..) | TyKind::CoroutineClosure(..) => {
                let (id, parent_args) = match ty.kind() {
                    TyKind::Closure(id, generic_args) => {
                        (id.0, generic_args.as_closure().parent_args())
                    }
                    TyKind::CoroutineClosure(id, generic_args) => {
                        let closure = generic_args.as_coroutine_closure();
                        if let TyKind::Coroutine(future, future_args) = dst_ty.kind()
                            && is_by_move_coroutine(db, future.0, future_args.as_coroutine())
                        {
                            // `future_by_move`: the future is made in place.
                            return Ok(());
                        }
                        (id.0.as_closure(db), closure.parent_args())
                    }
                    _ => unreachable!(),
                };
                let parent_args = GenericArgs::new_from_slice(parent_args);
                let def = InstanceDef::Closure(id.as_id());
                let body = self.fx.body_types(def, parent_args).map_err(told)?;
                if body.params.len() != arg_tys.len() + 1 {
                    return Err("closure called with a tuple of arguments".to_owned());
                }
                let self_ty = body.params[0].as_ref();
                let borrows = self_ty.as_reference().is_some();
                let mut values: Tys<'db> = smallvec![if borrows { self_ty } else { ty }];
                values.extend(arg_tys);
                self.emit_call(def, parent_args, &values, dst_ty)?;
                // A closure passed by value to a body that borrows it is the caller's to drop.
                if by_ref.is_none() && borrows {
                    self.drop_value(ty)?;
                }
                Ok(())
            }
            TyKind::Adt(..) | TyKind::Ref(..) => {
                let [by_shared, by_mut, _] = fn_trait_methods(interner.lang_items())
                    .into_iter()
                    .find(|family| family.contains(&Some(method)))
                    .expect("an Fn-trait method");
                let (method, self_ty) = match by_ref {
                    None => (Some(method), ty),
                    Some((ref_ty, Mutability::Not)) => (by_shared, ref_ty),
                    Some((ref_ty, Mutability::Mut)) => (by_mut, ref_ty),
                };
                let Some(method) = method else {
                    return Err(format!("Fn-trait call on {ty:?}"));
                };
                let args = GenericArgs::new_from_slice(&[ty.into(), tuple_ty.into()]);
                self.call_direct(method, args, &[self_ty, tuple_ty], dst_ty)
            }
            _ => Err(format!("Fn-trait call on {ty:?}")),
        }
    }

    /// `inline_asm`: the function an `asm!` block is assembled as, by its text, and what a
    /// `#[naked]` function's or `throw_exception`'s `sym` operands name.
    fn inline_asm(&mut self, block: &InlineAsm) -> Result<(), String> {
        use hir_ty_mir::mir::AsmOperand;
        let db = self.db();
        let seh = self.fx.is_seh_throw_info(block);
        let mut operands = Vec::with_capacity(block.operands.len());
        for operand in &block.operands {
            match operand {
                AsmOperand::Sym(_) if !block.naked && !seh => {
                    return Err("`asm!` with a `sym`".to_owned());
                }
                AsmOperand::Sym(Operand { kind: OperandKind::Static(id), .. }) => {
                    if is_thread_local(db, *id) {
                        return Err("inline assembly with a `sym` that is thread local".to_owned());
                    }
                    self.static_(*id)?;
                }
                AsmOperand::Sym(function) => {
                    let ty = self.operand_ty(function);
                    self.recorded(|fx| fx.fn_item(ty))?;
                }
                AsmOperand::Reg { .. } | AsmOperand::Const(_) | AsmOperand::Label => (),
            }
            operands.push(match operand {
                AsmOperand::Sym(_) => asm::Operand::Sym,
                AsmOperand::Reg { reg, input, output, written } => {
                    let mut size = None;
                    let tys = [
                        input.as_ref().map(|it| self.operand_ty(it)),
                        output.as_ref().map(|it| self.place_ty(it).ty),
                    ];
                    for ty in tys.into_iter().flatten() {
                        let bytes = self.fx.layout(ty).map_err(told)?.size.bytes() as u32;
                        if size.is_some_and(|it| it != bytes) {
                            return Err(
                                "an `asm!` register that goes in and comes out in two sizes"
                                    .to_owned(),
                            );
                        }
                        size = Some(bytes);
                    }
                    asm::Operand::Reg {
                        reg,
                        size,
                        input: input.is_some(),
                        output: output.is_some(),
                        written: *written,
                    }
                }
                AsmOperand::Const(value) => {
                    let ty = self.operand_ty(value);
                    let bits = match &value.kind {
                        OperandKind::Allocation { allocation }
                            if matches!(ty.kind(), TyKind::Int(_) | TyKind::Uint(_)) =>
                        {
                            let mut bytes = [0; 16];
                            let memory = &allocation.as_ref().memory;
                            bytes[..memory.len()].copy_from_slice(memory);
                            Some(u128::from_le_bytes(bytes))
                        }
                        OperandKind::Constant { konst, .. }
                            if matches!(ty.kind(), TyKind::Int(_) | TyKind::Uint(_))
                                && let ConstKind::Unevaluated(named) = konst.as_ref().kind() =>
                        {
                            let mut bytes = [0; 16];
                            let memory = &self.fx.eval_const(named).map_err(told)?.memory;
                            bytes[..memory.len()].copy_from_slice(memory);
                            Some(u128::from_le_bytes(bytes))
                        }
                        _ => self.fx.const_bits(value),
                    };
                    let Some(bits) = bits else {
                        return Err("an `asm!` constant that is not a literal".to_owned());
                    };
                    let unused = 128 - self.fx.layout(ty).map_err(told)?.size.bits() as u32;
                    asm::Operand::Const(match ty.kind() {
                        TyKind::Int(_) => (bits << unused) as i128 >> unused,
                        _ => bits as i128,
                    })
                }
                AsmOperand::Label => asm::Operand::Label,
            });
        }
        if seh {
            // `seh_throw_info`: the image's base, which the offsets it writes are from.
            return self.symbol("GetModuleHandleW");
        }
        // A `#[naked]` function is its assembly, which names its `sym`s.
        if block.naked {
            return Ok(());
        }
        let target = self.fx.cg.target;
        let features = self.fx.body.and_then(|it| it.owner.as_function());
        let features = features.map(|func| AttrFlags::target_features(db, func));
        let extensions =
            asm::extensions(target.arch, features.into_iter().flatten().map(|it| it.as_str()));
        let mut wrapper = asm::wrapper(target, &block.template, block.options, &operands)?;
        wrapper.text.insert_str(0, &extensions);
        for (operand, offset) in block.operands.iter().zip(&wrapper.offsets) {
            if let (AsmOperand::Reg { input: Some(input), .. }, Some(_)) = (operand, offset) {
                self.operand(input)?;
            }
        }
        self.assembly(wrapper.text)
    }

    /// `start_shim`: the C `main` hands `main` to `lang_start`.
    fn start_shim(&mut self, main: FunctionId) -> Result<(), String> {
        let interner = self.fx.interner;
        let Some(start) = interner.lang_items().Start else {
            return Err("no `lang_start`".to_owned());
        };
        let fn_ty = |func: FunctionId, args| {
            Ty::new_fn_def(interner, CallableDefId::FunctionId(func).into(), args)
        };
        let sig_of =
            |ty: Ty<'db>| interner.instantiate_bound_regions_with_erased(ty.fn_sig(interner));
        let main_ty = fn_ty(main, GenericArgs::new_from_slice(&[]));
        let returned = self.fx.normalize(sig_of(main_ty).output());
        let start_args = GenericArgs::new_from_slice(&[returned.into()]);
        let start_sig = sig_of(fn_ty(start, start_args));
        self.recorded(|fx| fx.fn_item(main_ty))?;
        let inputs: Tys<'db> =
            start_sig.inputs().as_slice().iter().map(|&ty| self.fx.normalize(ty)).collect();
        self.call_direct(start, start_args, &inputs, start_sig.output())
    }
}

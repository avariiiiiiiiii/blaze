//! The backends of a v2 build ([`Prepare`]): the IR builder's IR of an instance against its
//! references by place, compiled with Cranelift, or handed to LLVM ([`super::release`]).

use std::marker::PhantomData;

use blaze_codegen::{
    Arch, Backend, CodegenError, CompiledFunction, Compile, Static, Unit,
    backend::{Emitted, FrontRef, Import, Prepare, Shape},
    fold::{Group, Masked, Merged},
    settings::{DebugInfo, Relocation, Spec},
    summary::{Imports, Summary},
    template::Refs,
};
use blaze_format::SpanRef;
use blaze_ids::ContentId;
use blaze_items::ItemKey;
use cranelift_codegen::{ir, isa::TargetIsa};
use hir_def::{
    DefWithBodyId, HasModule,
    attrs::AttrFlags,
    expr_store::{
        Body,
        scope::{ExprScopes, ScopeId},
    },
    hir::BindingId,
    signatures::FunctionSignature,
};
use hir_ty::{ParamEnvAndCrate, db::HirDatabase, next_solver::ParamEnv};
use hir_ty_mir::mir::MirSpan;
use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;

use super::{
    Built, DataPlace, Function, Hashes, HirFrontend, HirInstance, Kind, Places, Prepared,
    build::{self, Build},
    failed,
    places::{Placer, Ref, place_requests, resolve},
    refs, shape, side,
};
use crate::{
    Detach, Instance, InstanceDef, Produced, compile_as,
    debuginfo::SlotPlaces,
    fragment::{self, Hint, Requests},
    home::Homes,
    instance_code,
};

/// Cranelift and LLVM over the IR builder.
#[derive(Default)]
pub struct Backends<'db>(PhantomData<&'db ()>);

/// Variables of the environment that change the code the IR builder makes, which a unit's
/// key has no part of: set, they would give two builds of one key different code.
const CHANGES_CODE: &[&str] = &[
    "BLAZE_INLINE",
    "BLAZE_PROMOTE",
    "BLAZE_MIR_OPT",
    "BLAZE_REGALLOC",
    "BLAZE_LEAN",
    "BLAZE_BACKEND",
    "BLAZE_OPT",
    "BLAZE_NO_HOME",
    "BLAZE_BUILT_IN",
];

/// Whether the process's code generator is the one `spec` asks for: the IR builder and
/// Cranelift take the target, its features and frame pointers from the process.
fn check_spec(unit: &Unit, spec: &Spec) -> Result<(), CodegenError> {
    let fail = |what: String| failed(unit, what);
    if unit.backend != spec.backend {
        return Err(fail(format!("a unit for {:?} with a spec for {:?}", unit.backend, spec.backend)));
    }
    let toolchain = &spec.toolchain;
    if unit.arch != toolchain.arch {
        return Err(fail(format!("a unit for {:?} with a spec for {:?}", unit.arch, toolchain.arch)));
    }
    if toolchain.arch == Arch::Wasm32 {
        return Err(fail("wasm: v2 builds in the bodies wasm-bindgen needs built in, and has no \
                         relocations of wasm's, neither of which it has yet"
            .to_owned()));
    }
    let target = crate::Target::selected().map_err(fail)?;
    let arch = match target.arch {
        crate::Arch::X86_64 => Arch::X86_64,
        crate::Arch::Aarch64 => Arch::Aarch64,
        crate::Arch::Wasm32 => Arch::Wasm32,
    };
    if target.triple() != toolchain.target || arch != toolchain.arch {
        return Err(fail(format!(
            "the process compiles for {}, the unit is for {}",
            target.triple(),
            toolchain.target
        )));
    }
    if target.cpu() != toolchain.cpu {
        return Err(fail(format!(
            "the process compiles for the CPU {}, the unit for {}",
            target.cpu(),
            toolchain.cpu
        )));
    }
    if spec.backend == Backend::Cranelift && !toolchain.features.is_empty() {
        return Err(fail(format!(
            "target features {:?}: the IR builder's Cranelift has the target's baseline only",
            toolchain.features
        )));
    }
    if toolchain.frame_pointers != crate::backend::frame_pointers() {
        return Err(fail(format!(
            "the unit keeps frame pointers: {}, the process: {}",
            toolchain.frame_pointers,
            crate::backend::frame_pointers()
        )));
    }
    let isa = isa(unit)?;
    let pic = isa.flags().is_pic();
    if pic != (toolchain.relocation == Relocation::Pic) {
        return Err(fail(format!(
            "the unit's code is {:?}, the process's Cranelift {}",
            toolchain.relocation,
            if pic { "position-independent" } else { "static" }
        )));
    }
    if let Some(knob) = set_knob() {
        return Err(fail(format!("`{knob}` is set, which changes code a unit's key has no part of")));
    }
    Ok(())
}

/// The first of [`CHANGES_CODE`] that is set, read once: the environment is the process's,
/// and reading it takes the environment's lock.
fn set_knob() -> Option<&'static str> {
    static SET: std::sync::OnceLock<Option<&'static str>> = std::sync::OnceLock::new();
    *SET.get_or_init(|| CHANGES_CODE.iter().copied().find(|it| std::env::var_os(it).is_some()))
}

fn isa(unit: &Unit) -> Result<&'static dyn TargetIsa, CodegenError> {
    crate::target().map(|it| &**it).map_err(|e| failed(unit, e))
}

impl Backends<'_> {
    pub(super) fn prepared(
        &self,
        db: &dyn HirDatabase,
        homes: &mut Homes,
        hashes: &dyn Hashes,
        unit: &Unit,
        spec: &Spec,
        instance: &HirInstance,
        refs: &Refs,
    ) -> Result<(Shape, Prepared), CodegenError> {
        let fail = |what: String| failed(unit, format!("{instance:?}: {what}"));
        let walked = refs::listed(db, instance).map_err(fail)?;
        if walked.len() != refs.len() {
            return Err(fail(format!(
                "{} references given for an instance of {}",
                refs.len(),
                walked.len()
            )));
        }
        let mut placer = Placer::listed(walked);
        let mut code = build::with(Build::of(spec), || built(db, instance, spec.overflow_checks))
            .map_err(fail)?
            .map_err(fail)?;
        let (places, source, variables) = match &mut code {
            Built::Ir(pending) => {
                let requests = &mut pending.wants.requests;
                let places =
                    place_requests(db, homes, hashes, &mut placer, requests).map_err(fail)?;
                (places, pending.source.take(), pending.variables.take())
            }
            Built::Assembled(_, requests) => {
                let places =
                    place_requests(db, homes, hashes, &mut placer, requests).map_err(fail)?;
                (places, None, None)
            }
        };
        let lines = spec.debuginfo != DebugInfo::None;
        let spans = match (&source, lines) {
            (Some((owner, spans)), true) => spans
                .iter()
                .map(|span| hashes.span(db, *owner, *span))
                .collect::<Result<_, _>>()
                .map_err(fail)?,
            _ => Vec::new(),
        };
        let told = matches!(spec.debuginfo, DebugInfo::Limited | DebugInfo::Full);
        let (variables, scopes) = match (&source, variables) {
            (Some((owner, spans)), Some((_, variables))) if told => {
                named(db, hashes, *owner, &variables, spans).map_err(fail)?
            }
            _ => Default::default(),
        };
        if let Built::Ir(pending) = &mut code {
            if !lines {
                pending.func.stencil.srclocs.clear();
            }
            if unit.backend == Backend::Cranelift {
                colocate(&mut pending.func, &places, placer.refs());
            }
        }
        let inlined = super::frontend::inlined(db, hashes, instance).map_err(fail)?;
        let function = Function {
            code,
            places,
            spans,
            variables,
            scopes,
            inlined,
            hint: hint(db, instance),
            release: Default::default(),
            foldable: foldable(db, instance),
        };
        let shape = shape::of(&function).map_err(fail)?;
        let prepared = match unit.backend {
            Backend::Cranelift => Prepared::Cranelift(Box::new(function)),
            Backend::Llvm { .. } => Prepared::Llvm(Box::new(function)),
        };
        Ok((shape, prepared))
    }
}

/// What a function's `#[inline]` asks of LLVM, as `imports::build` reads it.
fn hint(db: &dyn HirDatabase, instance: &HirInstance) -> Hint {
    let Kind::Code(Instance { def: InstanceDef::Fn(func), .. }, _) = &instance.0 else {
        return Hint::None;
    };
    let flags = AttrFlags::query(db, (*func).into());
    if flags.contains(AttrFlags::INLINE_NEVER) {
        Hint::Never
    } else if flags.contains(AttrFlags::INLINE_ALWAYS) {
        Hint::Always
    } else if flags.contains(AttrFlags::IS_INLINE) {
        Hint::Inline
    } else {
        Hint::None
    }
}

/// Whether a fold may give the instance another's code: its address is no symbol's, and it
/// takes no C variadic arguments, whose `va_list` a thunk could not pass on.
fn foldable(db: &dyn HirDatabase, instance: &HirInstance) -> bool {
    if super::frontend::symbol_of(db, &instance.0).is_some() {
        return false;
    }
    match &instance.0 {
        Kind::Code(Instance { def: InstanceDef::Fn(func), .. }, _) => {
            !FunctionSignature::of(db, *func).is_varargs()
        }
        Kind::Code(..) => true,
        Kind::Asm(_) | Kind::Start(_) | Kind::Allocator(_) => false,
    }
}

/// The IR of an instance, built as a unit with overflow checks `checked` asks.
pub(super) fn built(db: &dyn HirDatabase, instance: &HirInstance, checked: bool) -> Result<Built, String> {
    let produced = match &instance.0 {
        Kind::Code(instance, env) => {
            let instance = Instance { checked, ..instance.clone() };
            instance_code(db, instance, env.clone(), Detach::Unit)
        }
        Kind::Start(main) => {
            let env = ParamEnvAndCrate { param_env: ParamEnv::empty(), krate: main.krate(db) };
            compile_as(db, env, None, false, checked, true, |fx| fx.start_shim(*main))
        }
        Kind::Asm(text) => return assembled(text),
        Kind::Allocator(forward) => super::allocator::built(db, forward, checked),
    }
    .map_err(|e| e.to_string())?;
    Ok(match produced {
        Produced::Pending(pending) => Built::Ir(pending),
        Produced::Code(compiled) => Built::Assembled(compiled.code, compiled.requests),
    })
}

/// The function of an `asm!` block: its assembly, which names nothing.
fn assembled(text: &str) -> Result<Built, String> {
    let isa = crate::target().map_err(|e| e.to_string())?;
    let assembled = crate::asm::assemble(&isa.triple().to_string(), text)?;
    if !assembled.relocs.is_empty() {
        return Err("an `asm!` function that names a symbol".to_owned());
    }
    let code = fragment::Code {
        signature: ir::Signature::new(isa.default_call_conv()),
        bytes: assembled.code.clone(),
        align: 16,
        relocs: fragment::Relocs::default(),
        unwind: None,
        lines: Vec::new(),
        slots: SlotPlaces::default(),
    };
    Ok(Built::Assembled(code, Requests::default()))
}

/// The named locals a debugger is told of, and the scopes of the body they are bound in: a
/// name a macro of the compiler's made up is no name of the source. `spans` are the
/// statements the function's line rows are of.
fn named(
    db: &dyn HirDatabase,
    hashes: &dyn Hashes,
    owner: DefWithBodyId,
    variables: &[fragment::Variable],
    spans: &[MirSpan],
) -> Result<(Vec<side::Variable>, side::Scopes), String> {
    let body = Body::of(db, owner);
    let variables: Vec<_> =
        variables.iter().filter(|it| !body[it.binding].name.as_str().starts_with('<')).collect();
    let (scopes, of_binding) = scoped(db, owner, &variables, spans);
    let mut named = Vec::with_capacity(variables.len());
    for variable in variables {
        named.push(side::Variable {
            name: *body[variable.binding].name.symbol(),
            param: variable.param,
            ty: hashes.ty(db, variable.ty.as_ref())?,
            slot: variable.slot,
            pointer: variable.pointer,
            scope: of_binding.get(&variable.binding).copied(),
        });
    }
    Ok((named, scopes))
}

/// The scopes of the body of `owner` that `variables` are bound in, outer ones first, which
/// of them each binding is of, and which each of `spans` is of. A binding of the outermost
/// scope, which a parameter is, is known in all of the function and is of none. The code that
/// tries the pattern of an arm is of the scope its `match` is in; other code of no expression
/// is of the scope of the code before it.
fn scoped(
    db: &dyn HirDatabase,
    owner: DefWithBodyId,
    variables: &[&fragment::Variable],
    spans: &[MirSpan],
) -> (side::Scopes, FxHashMap<BindingId, u32>) {
    let scopes = ExprScopes::of(db, owner);
    let body = Body::of(db, owner);
    let outermost = scopes.scope_chain(scopes.scope_for(body.root_expr())).last();
    // The scope of each binding told of, and of no other: the walk stops once all are found.
    let mut wanted: SmallVec<[BindingId; 16]> = variables.iter().map(|it| it.binding).collect();
    blaze_ids::radix::sort_copy_by_key(&mut wanted, |it| it.into_raw().into_u32());
    wanted.dedup();
    let mut bound = FxHashMap::with_capacity_and_hasher(wanted.len(), Default::default());
    let mut seen = FxHashSet::default();
    'scopes: for (_, &scope) in scopes.scope_by_expr().iter() {
        for scope in scopes.scope_chain(Some(scope)).take_while(|&it| seen.insert(it)) {
            for entry in scopes.entries(scope) {
                if wanted.binary_search(&entry.binding()).is_ok() {
                    bound.insert(entry.binding(), scope);
                    if bound.len() == wanted.len() {
                        break 'scopes;
                    }
                }
            }
        }
    }
    let mut told: FxHashMap<ScopeId, u32> = FxHashMap::default();
    let mut parents = Vec::new();
    let mut of_binding = FxHashMap::default();
    for variable in variables {
        let Some(&scope) = bound.get(&variable.binding).filter(|&&it| Some(it) != outermost) else {
            continue;
        };
        let mut chain: Vec<ScopeId> = scopes
            .scope_chain(Some(scope))
            .take_while(|it| Some(*it) != outermost && !told.contains_key(it))
            .filter(|&it| it == scope || !scopes.entries(it).is_empty())
            .collect();
        chain.reverse();
        for scope in chain {
            let parent =
                scopes.scope_chain(Some(scope)).skip(1).find_map(|it| told.get(&it).copied());
            told.insert(scope, parents.len() as u32);
            parents.push(parent);
        }
        of_binding.insert(variable.binding, told[&scope]);
    }
    if told.is_empty() {
        return (side::Scopes::default(), of_binding);
    }
    let mut tried = FxHashMap::default();
    if spans.iter().any(|it| matches!(it, MirSpan::PatId(_))) {
        for (expr, it) in body.exprs() {
            if let hir_def::hir::Expr::Match { arms, .. } = it {
                for arm in arms.iter() {
                    body.walk_pats(arm.pat, &mut |pat| drop(tried.insert(pat, expr)));
                }
            }
        }
    }
    let innermost = |scope: Option<ScopeId>| {
        match scopes.scope_chain(scope).find_map(|it| told.get(&it)) {
            Some(&index) => side::SpanScope::In(index),
            None => side::SpanScope::Outside,
        }
    };
    let mut of_span = Vec::with_capacity(spans.len());
    of_span.extend(spans.iter().map(|span| match *span {
        MirSpan::ExprId(expr) | MirSpan::ExprEnd(expr) => innermost(scopes.scope_for(expr)),
        MirSpan::PatId(pat) => match tried.get(&pat) {
            Some(&of) => innermost(scopes.scope_for(of)),
            None => side::SpanScope::Before,
        },
        _ => side::SpanScope::Before,
    }));
    (side::Scopes { parents, of_span }, of_binding)
}

/// Makes every call and every address of blaze's own code and data direct: in v2 the linker
/// places them all, so nothing of them goes through the GOT. A symbol of outside code is
/// called directly too (through the PLT where it is in another object), but its address is
/// left as the IR builder takes it.
fn colocate(func: &mut ir::Function, places: &Places, refs: &[Ref]) {
    let outside = |place: u32| {
        matches!(refs.get(place as usize), Some(FrontRef::Extern(_) | FrontRef::Weak(_)) | None)
    };
    let ir::Function { params, stencil, .. } = func;
    let names = params.user_named_funcs();
    let own = |name: &ir::ExternalName| -> Option<bool> {
        let ir::ExternalName::User(at) = name else { return None };
        let name = &names[*at];
        match name.namespace {
            0 => Some(!outside(*places.funcs.get(name.index as usize)?)),
            1 => Some(match places.datas.get(name.index as usize)? {
                DataPlace::Place(place) => !outside(*place),
                DataPlace::Own | DataPlace::Position(_) | DataPlace::Text(_) | DataPlace::Nowhere => {
                    true
                }
            }),
            _ => None,
        }
    };
    // One bit a callee: no allocation for the usual few hundred.
    let mut addressed: SmallVec<[u64; 4]> = SmallVec::new();
    addressed.resize(stencil.dfg.ext_funcs.len().div_ceil(64), 0);
    for block in stencil.layout.blocks() {
        for inst in stencil.layout.block_insts(block) {
            if let ir::InstructionData::FuncAddr { func_ref, .. } = stencil.dfg.insts[inst] {
                let at = func_ref.as_u32() as usize;
                addressed[at / 64] |= 1 << (at % 64);
            }
        }
    }
    for (func_ref, ext) in stencil.dfg.ext_funcs.iter_mut() {
        let at = func_ref.as_u32() as usize;
        match own(&ext.name) {
            Some(true) => ext.colocated = true,
            Some(false) if addressed[at / 64] & (1 << (at % 64)) == 0 => ext.colocated = true,
            _ => {}
        }
    }
    for (_, global) in stencil.global_values.iter_mut() {
        if let ir::GlobalValueData::Symbol { name, colocated, tls: false, .. } = global
            && own(name) == Some(true)
        {
            *colocated = true;
        }
    }
}

/// Compiles a prepared function with Cranelift, against `refs`.
pub(crate) fn emit_cranelift(
    unit: &Unit,
    function: Box<Function>,
    refs: &Refs,
) -> Result<Emitted, CodegenError> {
    if function.places.refs as usize != refs.len() {
        return Err(failed(
            unit,
            format!(
                "{} references given for a function prepared against {}",
                refs.len(),
                function.places.refs
            ),
        ));
    }
    let Function { code, places, spans, variables, scopes, .. } = *function;
    let named = Named { spans: &spans, variables: &variables, scopes: &scopes };
    match code {
        Built::Ir(pending) => {
            let code = compiled(unit, pending.func, &pending.named)?;
            told(unit, &places, &pending.wants.requests, code, refs, named)
        }
        Built::Assembled(code, requests) => told(unit, &places, &requests, code, refs, named),
    }
}

/// What a debugger is told of a function: [`Function::spans`], `variables` and `scopes`.
#[derive(Clone, Copy)]
struct Named<'a> {
    spans: &'a [SpanRef],
    variables: &'a [side::Variable],
    scopes: &'a side::Scopes,
}

impl Function {
    fn named(&self) -> Named<'_> {
        Named { spans: &self.spans, variables: &self.variables, scopes: &self.scopes }
    }
}

/// What `emit` gives of a function's code, Cranelift's or LLVM's: its relocations and data
/// through the function's places at `refs`, its lines and the side table of its named locals.
pub(crate) fn emitted(
    unit: &Unit,
    function: &Function,
    code: fragment::Code,
    refs: &Refs,
) -> Result<Emitted, CodegenError> {
    told(unit, &function.places, function.requests(), code, refs, function.named())
}

fn told(
    unit: &Unit,
    places: &Places,
    requests: &Requests,
    code: fragment::Code,
    refs: &Refs,
    Named { spans, variables, scopes }: Named<'_>,
) -> Result<Emitted, CodegenError> {
    let fail = |what: String| failed(unit, what);
    if refs.len() < places.refs as usize {
        return Err(fail(format!(
            "{} references given for a function prepared against {}",
            refs.len(),
            places.refs
        )));
    }
    let isa = isa(unit)?;
    let side = side::of(variables, scopes, &code).map_err(fail)?;
    let compiled = resolve(places, refs, |resolve| {
        super::convert::function(
            unit.instance.key(),
            unit.arch,
            isa,
            code,
            requests,
            resolve,
            spans,
            side.as_ref().map(|it| it.1),
        )
    })
    .map_err(fail)?;
    Ok(Emitted { function: compiled, side: side.map(|it| it.0), extra: Vec::new() })
}

/// Compiles `ir` in place of a prepared function's IR, its names being the function's: a
/// fold's body, emitted as its first member, or one of its thunks, which names the body
/// last of `refs` and has none of its member's lines or named locals.
fn emit_ir(
    unit: &Unit,
    function: &Function,
    ir: ir::Function,
    refs: &Refs,
) -> Result<Emitted, CodegenError> {
    let own = function.places.refs as usize;
    let thunk = refs.len() == own + 1;
    if !thunk && refs.len() != own {
        return Err(failed(
            unit,
            format!("{} references given for a fold of a function of {own}", refs.len()),
        ));
    }
    let Built::Ir(pending) = &function.code else {
        return Err(failed(unit, "a fold of an assembly function"));
    };
    if thunk {
        let code = compiled(unit, ir, &[])?;
        let none = side::Scopes::default();
        let named = Named { spans: &[], variables: &[], scopes: &none };
        told(unit, &function.places, function.requests(), code, refs, named)
    } else {
        let code = compiled(unit, ir, &pending.named)?;
        emitted(unit, function, code, refs)
    }
}

/// Cranelift's code of the IR, `named` being the stack slots of its named locals.
fn compiled(
    unit: &Unit,
    func: ir::Function,
    named: &[ir::StackSlot],
) -> Result<fragment::Code, CodegenError> {
    let isa = isa(unit)?;
    crate::with_context(func, |context| {
        let pairs = crate::float_pair::lower(&mut context.func);
        let mut code = crate::emit(context).map_err(|e| failed(unit, e))?;
        if let Some(frame) = context.compiled_code().and_then(|it| it.buffer.frame_layout()) {
            code.slots = SlotPlaces::of_frame(isa, frame, &context.func, named.iter().copied());
        }
        if let Some(pairs) = pairs {
            pairs.hold(context, &mut code).map_err(|e| failed(unit, e))?;
        }
        Ok(code)
    })
}

impl<'db> Prepare for Backends<'db> {
    type Frontend = HirFrontend<'db>;
    type Prepared = Prepared;
    type Export = super::release::Export;

    fn prepare(
        &self,
        frontend: &HirFrontend<'db>,
        unit: &Unit,
        spec: &Spec,
        instance: &HirInstance,
        refs: &Refs,
    ) -> Result<(Shape, Prepared), CodegenError> {
        check_spec(unit, spec)?;
        frontend.with(|db, homes| {
            self.prepared(db, homes, &*frontend.hashes, unit, spec, instance, refs)
        })
    }

    fn summarize(
        &self,
        unit: &Unit,
        spec: &Spec,
        prepared: &mut Prepared,
        refs: &Refs,
    ) -> Result<(Summary, Self::Export), CodegenError> {
        match prepared {
            Prepared::Llvm(function) => super::release::summarize(unit, spec, function, refs),
            Prepared::Cranelift(_) => {
                Err(failed(unit, "a summary of a Cranelift unit: summaries are LLVM's"))
            }
        }
    }

    fn emit(
        &self,
        unit: &Unit,
        spec: &Spec,
        prepared: Prepared,
        refs: &Refs,
        imports: Option<(&Imports, &[Import<'_, Self::Export>])>,
    ) -> Result<Emitted, CodegenError> {
        match prepared {
            Prepared::Cranelift(function) => {
                if imports.is_some() {
                    return Err(failed(unit, "imports for a Cranelift unit: imports are LLVM's"));
                }
                emit_cranelift(unit, function, refs)
            }
            Prepared::Llvm(function) => super::release::emit(unit, spec, *function, refs, imports),
        }
    }

    fn masked(&self, _: &Unit, prepared: &Prepared, refs: &Refs) -> Option<Masked> {
        match prepared {
            Prepared::Cranelift(function) if function.foldable => super::fold::masked(function, refs),
            Prepared::Cranelift(_) | Prepared::Llvm(_) => None,
        }
    }

    fn merge(
        &self,
        group: &Group,
        members: Vec<(&Unit, &Spec, Prepared, &Refs)>,
    ) -> Result<Merged, CodegenError> {
        let mut functions = Vec::with_capacity(members.len());
        for (unit, spec, prepared, refs) in members {
            match prepared {
                Prepared::Cranelift(function) => functions.push((unit, spec, function, refs)),
                Prepared::Llvm(_) => {
                    return Err(failed(unit, "a fold of an LLVM unit: folds are Cranelift's"));
                }
            }
        }
        let members: Vec<(&Unit, &Spec, &Function, &Refs)> = functions
            .iter()
            .map(|(unit, spec, function, refs)| (*unit, *spec, &**function, *refs))
            .collect();
        let Some(&(first, ..)) = members.first() else {
            return Err(CodegenError {
                unit: group.key(),
                item: ItemKey(ContentId::default()),
                what: "a fold of no functions".to_owned(),
            });
        };
        let pointer = isa(first)?.pointer_type();
        super::fold::merge(group, &members, pointer, |unit, _, function, ir, refs| {
            emit_ir(unit, function, ir, refs)
        })
    }

    fn compile_static(
        &self,
        frontend: &HirFrontend<'db>,
        spec: &Spec,
        value: &super::HirStatic,
        refs: &Refs,
    ) -> Result<Static, CodegenError> {
        frontend.with(|db, homes| {
            let hashes = &*frontend.hashes;
            super::statics::compile(db, homes, hashes, spec.toolchain.arch, value.0, refs)
                .map_err(|what| super::undescribed(value, None, what))
        })
    }
}

impl Compile for Backends<'_> {
    type Mir<'a> = (&'a HirFrontend<'a>, &'a Spec, &'a HirInstance, &'a Refs);

    fn compile(
        &self,
        unit: &Unit,
        (frontend, spec, instance, refs): Self::Mir<'_>,
    ) -> Result<CompiledFunction, CodegenError> {
        let (_, prepared) = self.prepare(frontend, unit, spec, instance, refs)?;
        Ok(self.emit(unit, spec, prepared, refs, None)?.function)
    }
}

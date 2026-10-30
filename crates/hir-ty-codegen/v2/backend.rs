//! The backends of a v2 build ([`Prepare`]): the IR builder's IR of an instance against its
//! references by place, compiled with Cranelift, or handed to LLVM ([`super::release`]).

use std::marker::PhantomData;

use base_db::{Db, Crate};
use blaze_codegen::{
    Arch, Backend, CodegenError, CompiledFunction, Compile, Static, Unit,
    backend::{Emitted, FrontRef, Import, Prepare, Shape},
    fold::{Group, Masked, Merged},
    settings::{DebugInfo, Relocation, Sanitizers, Spec},
    summary::{Imports, Summary},
    template::Refs,
};
use blaze_format::SpanRef;
use blaze_ids::{ContentId, bits::Bits};
use blaze_items::{CodeHash, ItemId, ItemKey};
use blaze_mem::Frame;
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

use super::{
    Built, DataPlace, Function, HashSource, Hashes, HirFrontend, HirInstance, Kind, Places, Prepared,
    build::{self, Build},
    failed,
    places::{Placer, Ref, evaluated, place_requests, resolve},
    refs, shape, side,
};
use crate::{
    Detach, Instance, InstanceDef, Produced, compile_as,
    debuginfo::SlotPlaces,
    fragment::{self, Hint, Requests},
    frame_map::{self, FrameHashMap},
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
    let cranelift = spec.backend.is_cranelift();
    if cranelift && target.features() != toolchain.features.as_slice() {
        return Err(fail(format!(
            "the process compiles with the target features {:?}, the unit with {:?}",
            target.features(),
            toolchain.features
        )));
    }
    if cranelift && !spec.sanitize.is_empty() {
        return Err(fail(
            "-Zsanitizer and SanitizerCoverage are instrumented by the LLVM back end \
             (Backend::Llvm), which this unit is not compiled by"
                .to_owned(),
        ));
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

// dyn: cranelift's API takes `&dyn TargetIsa`; its backends are private, one made per process
fn isa(unit: &Unit) -> Result<&'static dyn TargetIsa, CodegenError> {
    crate::target().map(|it| &**it).map_err(|e| failed(unit, e))
}

impl Backends<'_> {
    /// `instance` and whether its references were listed for a unit that MIR-inlines
    /// ([`HirFrontend::inlines`]), as describing it listed them.
    pub(super) fn prepared(
        &self,
        db: &Db,
        homes: &mut Homes,
        hashes: &HashSource,
        unit: &Unit,
        spec: &Spec,
        (instance, inlined): (&HirInstance, bool),
        refs: &Refs,
    ) -> Result<(Shape, Prepared), CodegenError> {
        // What preparing one instance makes and lets go of (the placer's index, the scope
        // tables, the shape's tables) is in a frame of the worker's, reset when it returns;
        // what it keeps (the IR, its places, spans, variables and scopes) is the heap's.
        blaze_mem::frame::task(blaze_mem::Tag::Codegen, |frame| {
            let fail = |what: String| failed(unit, format!("{instance:?}: {what}"));
            let build = Build::of(spec);
            if build.inlines && !inlined {
                return Err(fail(format!(
                    "a unit whose MIR is inlined ({:?}) of references listed for units that \
                     inline none: what the inlined code calls is not among them",
                    unit.backend
                )));
            }
            let walked = refs::listed(db, instance, inlined).map_err(fail)?;
            if walked.len() != refs.len() {
                return Err(fail(format!(
                    "{} references given for an instance of {}",
                    refs.len(),
                    walked.len()
                )));
            }
            let mut placer = Placer::listed_in(walked, frame);
            let mut code = build::with(build, || built(db, instance, spec.overflow_checks))
                .map_err(fail)?
                .map_err(fail)?;
            let krate = compiled_in(db, instance);
            let (places, source, variables) = match &mut code {
                Built::Ir(pending) => {
                    let requests = &mut pending.wants.requests;
                    let places = place_requests(db, homes, hashes, krate, &mut placer, requests)
                        .map_err(fail)?;
                    (places, pending.source.take(), pending.variables.take())
                }
                Built::Assembled(_, requests) => {
                    let places = place_requests(db, homes, hashes, krate, &mut placer, requests)
                        .map_err(fail)?;
                    (places, None, None)
                }
            };
            let lines = spec.debuginfo != DebugInfo::None;
            let spans = match (&source, lines) {
                (Some(source), true) => written_spans(db, hashes, source).map_err(fail)?,
                _ => Vec::new(),
            };
            let told = matches!(spec.debuginfo, DebugInfo::Limited | DebugInfo::Full);
            let (variables, mut scopes) = match (&source, variables) {
                (Some(source), Some((env, variables))) if told => {
                    let krate = env.as_ref(db).krate;
                    named(db, hashes, krate, source.owner, &variables, &source.spans, frame)
                        .map_err(fail)?
                }
                _ => Default::default(),
            };
            if let (Some(source), true) = (&source, lines) {
                inlined_scopes(db, hashes, source, &mut scopes).map_err(fail)?;
            }
            if let Built::Ir(pending) = &mut code {
                if !lines {
                    pending.func.stencil.srclocs.clear();
                }
                if unit.backend.is_cranelift() {
                    colocate(&mut pending.func, &places, placer.refs(), frame);
                }
            }
            let inlined =
                super::frontend::inlined(db, hashes, (instance, inlined)).map_err(fail)?;
            let function = Function {
                code,
                places,
                spans,
                variables,
                scopes,
                inlined,
                hint: hint(db, instance),
                unsanitized: unsanitized(db, spec, instance).map_err(fail)?,
                release: Default::default(),
                foldable: foldable(db, instance),
            };
            let shape = shape::of(unit, &function, frame).map_err(fail)?;
            // alloc: the prepared function, kept by the driver until its emit
            let prepared = match unit.backend {
                Backend::Cranelift { .. } => Prepared::Cranelift(Box::new(function)),
                Backend::Llvm { .. } => Prepared::Llvm(Box::new(function)),
            };
            Ok((shape, prepared))
        })
    }
}

/// The crate an instance's code is compiled in; `None` for assembly, which is no crate's.
pub(super) fn compiled_in(db: &Db, instance: &HirInstance) -> Option<Crate> {
    match &instance.0 {
        Kind::Code(_, env) => Some(env.as_ref(db).krate),
        Kind::Start(main) => Some(main.krate(db)),
        Kind::Allocator(forward) => Some(forward.krate),
        Kind::Asm(_) => None,
    }
}

/// What a function's `#[inline]` asks of LLVM, as `imports::build` reads it.
fn hint(db: &Db, instance: &HirInstance) -> Hint {
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

/// Which of `spec`'s sanitizers are off for the instance. As rustc's `codegen_instance_attrs`,
/// a shim has the `#[sanitize]` of the item it is of ([`super::frontend::item_of`]), a
/// closure its function's; the C `main` and the allocator shim are no item and have none
/// off. SanitizerCoverage is off for a `__sanitizer_*` symbol, which its pass skips by name
/// in rustc's build and which here it would otherwise trace.
fn unsanitized(
    db: &Db,
    spec: &Spec,
    instance: &HirInstance,
) -> Result<Sanitizers, String> {
    let sanitize = &spec.sanitize;
    if sanitize.is_empty() {
        return Ok(Sanitizers::NONE);
    }
    let off = match &instance.0 {
        Kind::Code(..) => match super::frontend::item_of(db, &instance.0)? {
            ItemId::Function(func) => hir_def::attrs::disabled_sanitizers(db, func),
            other => {
                return Err(format!(
                    "the `#[sanitize]` of {other:?}, which is no function: db's \
                     `disabled_sanitizers` takes a function"
                ));
            }
        },
        Kind::Start(_) | Kind::Allocator(_) | Kind::Asm(_) => 0,
    };
    let mut unsanitized = Sanitizers(off & sanitize.sanitizers.0);
    let callback = || {
        super::frontend::symbol_of(db, &instance.0).is_some_and(|it| it.starts_with("__sanitizer_"))
    };
    if sanitize.coverage.is_on() && callback() {
        unsanitized = unsanitized.union(Sanitizers::COVERAGE);
    }
    Ok(unsanitized)
}

/// Whether a fold may give the instance another's code: its address is no symbol's, and it
/// takes no C variadic arguments, whose `va_list` a thunk could not pass on.
fn foldable(db: &Db, instance: &HirInstance) -> bool {
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
pub(super) fn built(db: &Db, instance: &HirInstance, checked: bool) -> Result<Built, String> {
    let produced = match &instance.0 {
        Kind::Code(instance, env) => {
            let instance = Instance { checked, ..instance.clone() };
            instance_code(db, instance, env.clone(), Detach::Unit, super::build::building())
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

/// Where each statement the function's line rows are of is written, inlined code's in the
/// body it was inlined from.
fn written_spans(
    db: &Db,
    hashes: &HashSource,
    source: &fragment::Source,
) -> Result<Vec<SpanRef>, String> {
    let mut spans = Vec::with_capacity(source.spans.len());
    for at in 0..source.spans.len() as u32 {
        let Some(written) = source.written(at, None) else {
            return Err(format!("statement {at} of the function, which has no place in the source"));
        };
        spans.push(hashes.span(db, written.owner, written.span)?);
    }
    Ok(spans)
}

/// Tells `scopes` of the bodies the MIR optimizer inlined into the function: each one's call
/// site, and which of them each statement is of.
fn inlined_scopes(
    db: &Db,
    hashes: &HashSource,
    source: &fragment::Source,
    scopes: &mut side::Scopes,
) -> Result<(), String> {
    if source.inlined.is_empty() {
        return Ok(());
    }
    scopes.inlined.reserve(source.scopes.len());
    for (index, scope) in source.scopes.iter().enumerate() {
        let Some(at) = source.call_site(index as u32, None) else {
            return Err(format!(
                "the call inlined scope {index} is inlined at, of {:?} within inlined callee \
                 {:?}, which is no item's body",
                scope.call_site, scope.parent
            ));
        };
        let call_site = hashes.span(db, at.owner, at.span)?;
        scopes.inlined.push(side::InlinedScope { parent: scope.parent, call_site });
    }
    scopes.inlined_of_span =
        (0..source.spans.len() as u32).map(|at| source.scope_of(at)).collect();
    Ok(())
}

/// The named locals a debugger is told of, and the scopes of the body they are bound in: a
/// name a macro of the compiler's made up is no name of the source. `spans` are the
/// statements the function's line rows are of. Their types are of a body monomorphized in
/// `krate`.
fn named(
    db: &Db,
    hashes: &HashSource,
    krate: Crate,
    owner: DefWithBodyId,
    variables: &[fragment::Variable],
    spans: &[MirSpan],
    frame: &Frame,
) -> Result<(Vec<side::Variable>, side::Scopes), String> {
    let body = Body::of(db, owner);
    let mut told: Vec<&fragment::Variable, &Frame> = Vec::with_capacity_in(variables.len(), frame);
    told.extend(variables.iter().filter(|it| !body[it.binding].name.as_str().starts_with('<')));
    let (scopes, of_binding) = scoped(db, owner, &told, spans, frame);
    // alloc: the named locals, kept in the prepared function
    let mut named = Vec::with_capacity(told.len());
    for variable in told.iter().copied() {
        named.push(side::Variable {
            name: *body[variable.binding].name.symbol(),
            param: variable.param,
            ty: hashes.ty(db, evaluated(db, Some(krate), variable.ty.as_ref())?)?,
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
fn scoped<'f>(
    db: &Db,
    owner: DefWithBodyId,
    variables: &[&fragment::Variable],
    spans: &[MirSpan],
    frame: &'f Frame,
) -> (side::Scopes, FrameHashMap<'f, BindingId, u32>) {
    let scopes = ExprScopes::of(db, owner);
    let body = Body::of(db, owner);
    let outermost = scopes.scope_chain(scopes.scope_for(body.root_expr())).last();
    // The scope of each binding told of, and of no other: the walk stops once all are found.
    // By their raw index, which is what the sort keyed them by before: the radix sort's
    // buffer is the thread's.
    let mut wanted: Vec<u32, &Frame> = Vec::with_capacity_in(variables.len(), frame);
    wanted.extend(variables.iter().map(|it| it.binding.into_raw().into_u32()));
    blaze_mem::scratch::with_u32s(|scratch| {
        blaze_ids::radix::sort_copy_by_key_in(&mut wanted, |&it| it, scratch);
    });
    wanted.dedup();
    let mut bound = frame_map::hash_map_with_capacity(wanted.len(), frame);
    let mut seen = frame_map::hash_set(frame);
    'scopes: for (_, &scope) in scopes.scope_by_expr().iter() {
        for scope in scopes.scope_chain(Some(scope)).take_while(|&it| seen.insert(it)) {
            for entry in scopes.entries(scope) {
                if wanted.binary_search(&entry.binding().into_raw().into_u32()).is_ok() {
                    bound.insert(entry.binding(), scope);
                    if bound.len() == wanted.len() {
                        break 'scopes;
                    }
                }
            }
        }
    }
    let mut told: FrameHashMap<'_, ScopeId, u32> = frame_map::hash_map(frame);
    // alloc: the scopes' parents, kept in the prepared function
    let mut parents = Vec::new();
    let mut of_binding = frame_map::hash_map(frame);
    // A binding's scopes not told of yet, outermost first: one list for the unit, refilled
    // for each binding.
    let mut chain: Vec<ScopeId, &Frame> = Vec::new_in(frame);
    for variable in variables {
        let Some(&scope) = bound.get(&variable.binding).filter(|&&it| Some(it) != outermost) else {
            continue;
        };
        chain.clear();
        chain.extend(
            scopes
                .scope_chain(Some(scope))
                .take_while(|it| Some(*it) != outermost && !told.contains_key(it))
                .filter(|&it| it == scope || !scopes.entries(it).is_empty()),
        );
        chain.reverse();
        for &scope in chain.iter() {
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
    let mut tried = frame_map::hash_map(frame);
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
    (side::Scopes { parents, of_span, ..Default::default() }, of_binding)
}

/// Makes every call and every address of blaze's own code and data direct: in v2 the linker
/// places them all, so nothing of them goes through the GOT. A symbol of outside code is
/// called directly too (through the PLT where it is in another object), but its address is
/// left as the IR builder takes it.
fn colocate(func: &mut ir::Function, places: &Places, refs: &[Ref], frame: &Frame) {
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
    // One bit a callee, in the frame.
    let words = frame.alloc_slice_fill_with(stencil.dfg.ext_funcs.len().div_ceil(64), |_| 0);
    let mut addressed: Bits<&mut [u64]> = Bits::from_words(words);
    for block in stencil.layout.blocks() {
        for inst in stencil.layout.block_insts(block) {
            if let ir::InstructionData::FuncAddr { func_ref, .. } = stencil.dfg.insts[inst] {
                addressed.insert(func_ref.as_u32() as usize);
            }
        }
    }
    for (func_ref, ext) in stencil.dfg.ext_funcs.iter_mut() {
        let at = func_ref.as_u32() as usize;
        match own(&ext.name) {
            Some(true) => ext.colocated = true,
            Some(false) if !addressed.contains(at) => ext.colocated = true,
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
    let Function { code, places, spans, variables, scopes, inlined, .. } = *function;
    let named = Named { spans: &spans, variables: &variables, scopes: &scopes, inlined };
    match code {
        Built::Ir(pending) => {
            let code = compiled(unit, pending.func, &pending.named)?;
            told(unit, &places, &pending.wants.requests, code, refs, named)
        }
        Built::Assembled(code, requests) => told(unit, &places, &requests, code, refs, named),
    }
}

/// What a debugger is told of a function: [`Function::spans`], `variables` and `scopes`; and
/// what its code has built in, [`Function::inlined`].
struct Named<'a> {
    spans: &'a [SpanRef],
    variables: &'a [side::Variable],
    scopes: &'a side::Scopes,
    inlined: Vec<CodeHash>,
}

impl Function {
    fn named(&self) -> Named<'_> {
        Named {
            spans: &self.spans,
            variables: &self.variables,
            scopes: &self.scopes,
            inlined: self.inlined.clone(),
        }
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
    Named { spans, variables, scopes, inlined }: Named<'_>,
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
    let (compiled, side) = resolve(places, refs, |resolve| {
        let side = side::of(variables, scopes, &code, resolve.frame())?;
        let compiled = super::convert::function(
            resolve.frame(),
            unit.instance.key(),
            unit.arch,
            isa,
            code,
            requests,
            resolve,
            spans,
            side.as_ref().map(|it| it.1),
        )?;
        Ok::<_, String>((compiled, side))
    })
    .map_err(fail)?;
    let function = CompiledFunction { inlined, ..compiled };
    Ok(Emitted { function, side: side.map(|it| it.0), extra: Vec::new() })
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
        let named = Named { spans: &[], variables: &[], scopes: &none, inlined: Vec::new() };
        told(unit, &function.places, function.requests(), code, refs, named)
    } else {
        let code = compiled(unit, ir, &pending.named)?;
        emitted(unit, function, code, refs)
    }
}

/// Cranelift's code of the IR at the unit's level, `named` being the stack slots of its
/// named locals.
fn compiled(
    unit: &Unit,
    func: ir::Function,
    named: &[ir::StackSlot],
) -> Result<fragment::Code, CodegenError> {
    let Backend::Cranelift { speed } = unit.backend else {
        return Err(failed(unit, format!("Cranelift's code of a unit for {:?}", unit.backend)));
    };
    let isa = isa(unit)?;
    crate::with_context(func, |context| {
        let pairs = crate::float_pair::lower(&mut context.func);
        let mut code = crate::emit_lent(context, speed).map_err(|e| failed(unit, e))?;
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
            let inlined = frontend.inlines(db, instance).map_err(|e| failed(unit, e))?;
            self.prepared(db, homes, &frontend.hashes, unit, spec, (instance, inlined), refs)
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
            let hashes = &frontend.hashes;
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

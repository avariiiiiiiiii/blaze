//! The backends of a v2 build ([`Prepare`]): the IR builder's IR of an instance against its
//! references by place, compiled with Cranelift, or handed to LLVM ([`super::release`]).

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
    DefWithBodyId, HasModule, attrs::AttrFlags, expr_store::Body, signatures::FunctionSignature,
};
use hir_ty::{ParamEnvAndCrate, db::HirDatabase, next_solver::ParamEnv};

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
pub struct Backends;

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
    if let Some(knob) = CHANGES_CODE.iter().find(|it| std::env::var_os(it).is_some()) {
        return Err(fail(format!("`{knob}` is set, which changes code a unit's key has no part of")));
    }
    Ok(())
}

fn isa(unit: &Unit) -> Result<&'static dyn TargetIsa, CodegenError> {
    crate::target().map(|it| &**it).map_err(|e| failed(unit, e))
}

impl Backends {
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
        let walked = refs::walk(db, instance, homes).map_err(fail)?;
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
        let variables = match (&source, variables) {
            (Some((owner, _)), Some((_, variables))) if told => {
                named(db, hashes, *owner, &variables).map_err(fail)?
            }
            _ => Vec::new(),
        };
        if let Built::Ir(pending) = &mut code {
            if !lines {
                pending.func.stencil.srclocs.clear();
            }
            if unit.backend == Backend::Cranelift {
                colocate(&mut pending.func, &places, placer.refs());
            }
        }
        let cranelift = unit.backend == Backend::Cranelift;
        let function = Function {
            code,
            places,
            spans,
            variables,
            hint: hint(db, instance),
            release: Default::default(),
            foldable: foldable(db, instance),
            refs: if cranelift { refs.to_vec() } else { Vec::new() },
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
        Kind::Asm(_) | Kind::Start(_) => false,
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
        relocs: Vec::new(),
        elf_types: Vec::new(),
        unwind: None,
        lines: Vec::new(),
        slots: SlotPlaces::default(),
    };
    Ok(Built::Assembled(code, Requests::default()))
}

/// The named locals a debugger is told of: a name a macro of the compiler's made up is no
/// name of the source.
fn named(
    db: &dyn HirDatabase,
    hashes: &dyn Hashes,
    owner: DefWithBodyId,
    variables: &[fragment::Variable],
) -> Result<Vec<side::Variable>, String> {
    let body = Body::of(db, owner);
    let mut named = Vec::with_capacity(variables.len());
    for variable in variables {
        let name = body[variable.binding].name.as_str();
        if name.starts_with('<') {
            continue;
        }
        named.push(side::Variable {
            name: name.to_owned(),
            param: variable.param,
            ty: hashes.ty(db, variable.ty.as_ref())?,
            slot: variable.slot,
            pointer: variable.pointer,
        });
    }
    Ok(named)
}

/// Makes every call and every address of blaze's own code and data direct: in v2 the linker
/// places them all, so nothing of them goes through the GOT. A symbol of outside code is
/// called directly too (through the PLT where it is in another object), but its address is
/// left as the IR builder takes it.
fn colocate(func: &mut ir::Function, places: &Places, refs: &[Ref]) {
    let outside = |place: u32| {
        matches!(refs.get(place as usize), Some(FrontRef::Extern(_) | FrontRef::Weak(_)) | None)
    };
    let names = func.params.user_named_funcs().clone();
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
    let addressed: rustc_hash::FxHashSet<ir::FuncRef> = func
        .layout
        .blocks()
        .flat_map(|block| func.layout.block_insts(block))
        .filter_map(|inst| match func.dfg.insts[inst] {
            ir::InstructionData::FuncAddr { func_ref, .. } => Some(func_ref),
            _ => None,
        })
        .collect();
    for (func_ref, ext) in func.dfg.ext_funcs.iter_mut() {
        match own(&ext.name) {
            Some(true) => ext.colocated = true,
            Some(false) if !addressed.contains(&func_ref) => ext.colocated = true,
            _ => {}
        }
    }
    for (_, global) in func.global_values.iter_mut() {
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
    mut function: Box<Function>,
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
    let Function { code, places, spans, variables, .. } = &mut *function;
    match code {
        Built::Ir(pending) => {
            let func = std::mem::replace(&mut pending.func, ir::Function::new());
            let code = compiled(unit, func, &pending.named)?;
            told(unit, places, &pending.wants.requests, &code, refs, spans, variables)
        }
        Built::Assembled(code, requests) => {
            told(unit, places, requests, code, refs, spans, variables)
        }
    }
}

/// What `emit` gives of a function's code, Cranelift's or LLVM's: its relocations and data
/// through the function's places at `refs`, its lines and the side table of its named locals.
pub(crate) fn emitted(
    unit: &Unit,
    function: &Function,
    code: &fragment::Code,
    refs: &Refs,
) -> Result<Emitted, CodegenError> {
    let requests = function.requests();
    told(unit, &function.places, requests, code, refs, &function.spans, &function.variables)
}

fn told(
    unit: &Unit,
    places: &Places,
    requests: &Requests,
    code: &fragment::Code,
    refs: &Refs,
    spans: &[SpanRef],
    variables: &[side::Variable],
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
    let side = side::encode(variables, &code.slots).map_err(fail)?;
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
    Ok(Emitted { function: compiled, side: side.map(|it| it.0) })
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
        told(unit, &function.places, function.requests(), &code, refs, &[], &[])
    } else {
        let code = compiled(unit, ir, &pending.named)?;
        emitted(unit, function, &code, refs)
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

impl Prepare for Backends {
    type Frontend = HirFrontend;
    type Prepared = Prepared;
    type Export = super::release::Export;

    fn prepare(
        &self,
        frontend: &HirFrontend,
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
        prepared: &Prepared,
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

    fn masked(&self, _: &Unit, prepared: &Prepared) -> Option<Masked> {
        match prepared {
            Prepared::Cranelift(function) if function.foldable => {
                super::fold::masked(function, &function.refs)
            }
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
        frontend: &HirFrontend,
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

impl Compile for Backends {
    type Mir<'a> = (&'a HirFrontend, &'a Spec, &'a HirInstance, &'a Refs);

    fn compile(
        &self,
        unit: &Unit,
        (frontend, spec, instance, refs): Self::Mir<'_>,
    ) -> Result<CompiledFunction, CodegenError> {
        let (_, prepared) = self.prepare(frontend, unit, spec, instance, refs)?;
        Ok(self.emit(unit, spec, prepared, refs, None)?.function)
    }
}

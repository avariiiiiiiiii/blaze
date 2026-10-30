//! LLVM for a release build of blaze v2 (docs/v2-codegen.md, "Release"): each function is
//! lowered and simplified by ThinLTO's pre-link pipeline once ([`prelink`]), which gives what
//! its summary says of it ([`Facts`]) and its export; then compiled from that module with the
//! exports it imports linked in and what the whole program says of it and its callees
//! applied ([`compile`]), through ThinLTO's post-link pipeline.
//!
//! A compile starts from the module its summary was read from, never from a second lowering:
//! a parameter the summary calls unused is passed `poison` by the callers, and is unused in
//! the code only because it is the same IR.

use blaze_codegen::summary::{Arg, Attrs};
use cranelift_codegen::ir;
use inkwell::{
    attributes::AttributeLoc,
    context::Context,
    llvm_sys::{core, prelude::LLVMValueRef},
    memory_buffer::MemoryBuffer,
    module::{Linkage, Module},
    passes::PassBuilderOptions,
    targets::{FileType, TargetMachine},
    types::BasicTypeEnum,
    values::{AsValueRef as _, BasicValueEnum, FunctionValue, InstructionOpcode},
};
use rustc_hash::{FxHashMap, FxHashSet};

use super::{
    INNER, Machine, OWN, REBASED, Symbol, Unhandled, agree_calls, attribute_kind, emitted, link,
    lower::{Debug, Defined, Lowering},
    mark_hint, rebase, retold_bytes, told_constants, wrap,
};
use crate::{
    backend::{Optimization, Passes, Phase, Tally},
    fragment::{Code, Hint, Wants},
    imports::Import,
};

/// A function after LLVM's pre-link pipeline.
pub(crate) struct Prelinked {
    /// Its module, the function under [`OWN`], with its line table if one was asked for:
    /// what its own compile starts from.
    pub(crate) own: Vec<u8>,
    /// The same without a line table: what importers link in.
    pub(crate) export: Vec<u8>,
    /// Its instructions, as LLVM counts them.
    pub(crate) size: usize,
    pub(crate) facts: Facts,
}

/// What the pre-linked function says of itself for its summary, its callees by the
/// requests they are of.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Facts {
    pub(crate) params: u32,
    /// The parameters it never reads, by place, leaving out those passed in memory the call
    /// makes (`byval`, `sret`...), whose argument is read by the call itself.
    pub(crate) unused: Vec<u32>,
    /// Its calls of the functions of `instances`, in the order of each callee's first call.
    pub(crate) calls: Vec<Called>,
    /// The functions of `instances` whose address it takes rather than calls, sorted.
    pub(crate) addressed: Vec<u32>,
    pub(crate) attrs: Attrs,
    pub(crate) body: Attrs,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Called {
    pub(crate) request: u32,
    pub(crate) count: u32,
    /// Per argument: what every call of it passes, where they agree.
    pub(crate) args: Vec<Arg>,
}

/// What a compile is given of the whole program, the functions by the requests they are of
/// in the compile's [`Wants`].
#[derive(Clone, Default, Debug)]
pub(crate) struct Given {
    /// Its parameters known to be these constants, by place.
    pub(crate) constants: Vec<(u32, Arg)>,
    /// What each function it calls may be assumed to be.
    pub(crate) callees: Vec<(u32, Attrs)>,
    /// The parameters of each function it calls that no caller need compute.
    pub(crate) dead: Vec<(u32, Vec<u32>)>,
}

/// The passes before the summary is read: ThinLTO's pre-link pipeline, after the slots are
/// taken apart as for a compile ([`Optimization::pipeline`]), so that the pre-link and the
/// post-link pipelines ([`postlink`]) are together what a compile at the level runs.
fn prelink_passes(optimization: Optimization) -> Option<&'static str> {
    match optimization.passes {
        Passes::None => None,
        Passes::Less => Some("function(sroa,instsimplify,sroa)"),
        Passes::Default | Passes::Aggressive => Some(super::exported(optimization)),
    }
}

/// The passes of a compile once its imports are linked in.
fn postlink(optimization: Optimization) -> Option<&'static str> {
    match optimization.passes {
        Passes::None => None,
        Passes::Less => Some("default<O1>"),
        Passes::Default => Some("thinlto<O2>"),
        Passes::Aggressive => Some(
            "cgscc(inline,function(sroa,instsimplify,sroa,jump-threading,early-cse<memssa>)),\
             thinlto<O3>,\
             function(gvn,sccp,instcombine<no-verify-fixpoint>,simplifycfg)",
        ),
    }
}

fn llvm(e: inkwell::support::LLVMString) -> Unhandled {
    Unhandled::Llvm(e.to_string_lossy().into_owned())
}

fn run(
    module: &Module<'_>,
    passes: &str,
    machine: &TargetMachine,
    vectorize: bool,
) -> Result<(), Unhandled> {
    let options = PassBuilderOptions::create();
    // rustc vectorizes straight-line code at `-O2` and `-O3`.
    options.set_loop_slp_vectorization(vectorize);
    Tally::global().time(Phase::Optimize, || module.run_passes(passes, machine, options)).map_err(llvm)
}

/// Lowers `func` alone into a module, runs the pre-link pipeline of `optimization` on it,
/// and reads what its summary says. `instances` says which of the function requests of
/// `wants` are functions of the build (the others are outside symbols). `debug` asks for its
/// line table and variables.
pub(crate) fn prelink(
    wants: &Wants,
    func: &ir::Function,
    named: &[ir::StackSlot],
    hint: Hint,
    debug: bool,
    optimization: Optimization,
    instances: &dyn Fn(u32) -> bool,
) -> Result<Prelinked, Unhandled> {
    let tally = Tally::global();
    Machine::with(optimization, |context, machine| {
        let module = context.create_module("blaze");
        module.set_triple(&machine.get_triple());
        module.set_data_layout(&machine.get_target_data().get_data_layout());
        let constants: FxHashMap<u32, &[u8]> = wants.constants().collect();
        let statics: FxHashSet<u32> = wants.statics().collect();
        let readonly: FxHashMap<u32, &crate::ConstBlob> = wants.readonly().collect();
        let pointer = machine.get_target_data().get_pointer_byte_size(None) as usize;
        told_constants(context, &module, &readonly, pointer);
        let lines = if debug { Debug::of(&module)? } else { None };
        let own = Defined {
            name: OWN,
            func,
            pointees: &wants.pointees,
            named: if debug { named } else { &[] },
            weights: None,
            marked: debug,
        };
        let blocks_asm = wants.asm_blocks();
        let shown = FxHashMap::default();
        let function = tally.time(Phase::Lower, || {
            let function = Lowering::add(context, &module, &own)?;
            mark_hint(context, function, hint);
            Lowering::define(
                context,
                &module,
                function,
                &own,
                &constants,
                &statics,
                wants.resume(),
                wants.memcmp(),
                &blocks_asm,
                &shown,
                lines.as_ref(),
            )?;
            Ok::<_, Unhandled>(function)
        })?;
        if let Some(lines) = &lines {
            lines.finish();
        }
        tally.time(Phase::Verify, || module.verify()).map_err(llvm)?;
        if let Some(passes) = prelink_passes(optimization) {
            run(&module, passes, machine, true)?;
        }
        let size = function.get_basic_blocks().iter().map(|it| it.get_instructions().count()).sum();
        let facts = facts(context, &module, function, instances, machine)?;
        let own = module.write_bitcode_to_memory().as_slice().to_vec();
        let export = if lines.is_some() {
            let stripped = module.clone();
            stripped.strip_debug_info();
            stripped.write_bitcode_to_memory().as_slice().to_vec()
        } else {
            own.clone()
        };
        Ok(Prelinked { own, export, size, facts })
    })
}

/// LLVM's attributes `memory(read)` stands for nothing of an object's state but this: a
/// location's `Mod` bit is the high one of its two.
const MOD: u64 = 0xAAAA_AAAA_AAAA_AAAA;

/// What LLVM says of `function` in its attributes.
fn attrs_of(function: FunctionValue<'_>) -> Attrs {
    let has = |name| function.get_enum_attribute(AttributeLoc::Function, attribute_kind(name)).is_some();
    let memory = function
        .get_enum_attribute(AttributeLoc::Function, attribute_kind("memory"))
        .map(|it| it.get_enum_value());
    Attrs {
        nounwind: has("nounwind"),
        readonly: memory.is_some_and(|it| it & MOD == 0),
        readnone: memory == Some(0),
        norecurse: has("norecurse"),
        willreturn: has("willreturn"),
        noreturn: has("noreturn"),
    }
}

/// `memory(read)` as the linked LLVM encodes it, which depends on how many kinds of location
/// it has.
fn read_only(context: &Context) -> Result<u64, Unhandled> {
    let text = "declare void @read() memory(read)\n\0";
    let source = MemoryBuffer::create_from_memory_range_copy(text.as_bytes(), "memory");
    let parsed = context.create_module_from_ir(source).map_err(llvm)?;
    let function = parsed.get_function("read").ok_or(Unhandled::Shape("no `memory(read)`"))?;
    let memory = function.get_enum_attribute(AttributeLoc::Function, attribute_kind("memory"));
    memory.map(|it| it.get_enum_value()).ok_or(Unhandled::Shape("no `memory(read)`"))
}

/// The attributes of a parameter whose argument the call itself reads or writes, in memory
/// it makes: such an argument is never passed `poison`.
const IN_MEMORY: [&str; 5] = ["byval", "byref", "inalloca", "preallocated", "sret"];

/// The attributes that make a call with a `poison` argument undefined, which LLVM's
/// dead-argument elimination takes off an argument it replaces.
const UB_IF_POISON: [&str; 5] =
    ["noundef", "nonnull", "dereferenceable", "dereferenceable_or_null", "align"];

fn in_memory(function: FunctionValue<'_>, at: u32) -> bool {
    IN_MEMORY.iter().any(|name| {
        function.get_enum_attribute(AttributeLoc::Param(at), attribute_kind(name)).is_some()
    })
}

/// The request a function of the module is of, by its name.
fn request_of(function: FunctionValue<'_>) -> Option<u32> {
    match Symbol::parse(function.get_name().to_str().ok()?)? {
        Symbol::Func(at) => Some(at),
        Symbol::Data(_) => None,
    }
}

/// Whether `user` is a call or invoke that calls `function` and passes it nothing of itself.
///
/// # Safety
///
/// Both are values of one live module.
unsafe fn only_calls(user: LLVMValueRef, function: LLVMValueRef) -> bool {
    unsafe {
        let call = !core::LLVMIsACallInst(user).is_null() || !core::LLVMIsAInvokeInst(user).is_null();
        call && core::LLVMGetCalledValue(user) == function
            && (0..core::LLVMGetNumArgOperands(user)).all(|at| core::LLVMGetOperand(user, at) != function)
    }
}

/// The calls and invokes of `function` in the module, each once.
fn calls_of(function: FunctionValue<'_>) -> Vec<LLVMValueRef> {
    let mut calls = Vec::new();
    let target = function.as_value_ref();
    // SAFETY: the uses of a live function are of its module.
    unsafe {
        let mut used = core::LLVMGetFirstUse(target);
        while !used.is_null() {
            let user = core::LLVMGetUser(used);
            if only_calls(user, target) && !calls.contains(&user) {
                calls.push(user);
            }
            used = core::LLVMGetNextUse(used);
        }
    }
    calls
}

/// What a constant argument is, where it is an integer of at most 64 bits.
fn arg_of(value: LLVMValueRef) -> Arg {
    // SAFETY: `value` is an operand of a live instruction.
    unsafe {
        if core::LLVMIsAConstantInt(value).is_null() {
            return Arg::Unknown;
        }
        let bits = core::LLVMGetIntTypeWidth(core::LLVMTypeOf(value));
        match u8::try_from(bits) {
            Ok(bits) if bits <= 64 => {
                Arg::Int { bits, value: u128::from(core::LLVMConstIntGetZExtValue(value)) }
            }
            _ => Arg::Unknown,
        }
    }
}

/// What `function`, alone in `module` after the pre-link pipeline, says of itself.
fn facts<'ctx>(
    context: &'ctx Context,
    module: &Module<'ctx>,
    function: FunctionValue<'ctx>,
    instances: &dyn Fn(u32) -> bool,
    machine: &TargetMachine,
) -> Result<Facts, Unhandled> {
    let params = function.count_params();
    let unused = function
        .get_param_iter()
        .enumerate()
        .filter(|(at, param)| {
            // SAFETY: a parameter of a live function.
            let used = unsafe { !core::LLVMGetFirstUse(param.as_value_ref()).is_null() };
            !used && !in_memory(function, *at as u32)
        })
        .map(|(at, _)| at as u32)
        .collect();
    let mut calls: Vec<Called> = Vec::new();
    for block in function.get_basic_block_iter() {
        for instruction in block.get_instructions() {
            if !matches!(instruction.get_opcode(), InstructionOpcode::Call | InstructionOpcode::Invoke) {
                continue;
            }
            let at = instruction.as_value_ref();
            // SAFETY: a call or invoke of the module; what it calls may be any value.
            let callee = unsafe { core::LLVMGetCalledValue(at) };
            if unsafe { core::LLVMIsAFunction(callee) }.is_null() {
                continue;
            }
            // SAFETY: it is a function.
            let Some(callee) = (unsafe { FunctionValue::new(callee) }) else { continue };
            let Some(request) = request_of(callee).filter(|it| instances(*it)) else { continue };
            // SAFETY: the operands of a call are its arguments, then what it calls.
            let args: Vec<Arg> = unsafe {
                (0..core::LLVMGetNumArgOperands(at)).map(|it| arg_of(core::LLVMGetOperand(at, it))).collect()
            };
            match calls.iter_mut().find(|it| it.request == request) {
                Some(called) => {
                    called.count += 1;
                    if called.args.len() != args.len() {
                        let len = called.args.len().min(args.len());
                        called.args = vec![Arg::Unknown; len];
                    }
                    for (known, arg) in called.args.iter_mut().zip(args) {
                        if *known != arg {
                            *known = Arg::Unknown;
                        }
                    }
                }
                None => calls.push(Called { request, count: 1, args }),
            }
        }
    }
    let mut addressed = Vec::new();
    for other in module.get_functions() {
        let Some(request) = request_of(other).filter(|it| instances(*it)) else { continue };
        let target = other.as_value_ref();
        // SAFETY: the uses of a live function are of its module.
        let taken = unsafe {
            let mut used = core::LLVMGetFirstUse(target);
            let mut taken = false;
            while !used.is_null() && !taken {
                taken = !only_calls(core::LLVMGetUser(used), target);
                used = core::LLVMGetNextUse(used);
            }
            taken
        };
        if taken {
            addressed.push(request);
        }
    }
    addressed.sort_unstable();
    addressed.dedup();
    let attrs = attrs_of(function);
    let body = optimistic(context, module, instances, machine)?;
    Ok(Facts {
        params,
        unused,
        calls,
        addressed,
        attrs,
        body: Attrs { noreturn: attrs.noreturn, ..body },
    })
}

/// What LLVM's attribute inference says of the function of `module` when every function of
/// `instances` it calls is nounwind, readnone, willreturn and norecurse: run on a copy, as
/// what it assumes need not be true.
fn optimistic<'ctx>(
    context: &'ctx Context,
    module: &Module<'ctx>,
    instances: &dyn Fn(u32) -> bool,
    machine: &TargetMachine,
) -> Result<Attrs, Unhandled> {
    let copy = module.clone();
    let memory = attribute_kind("memory");
    for function in copy.get_functions() {
        if request_of(function).is_none_or(|it| !instances(it)) {
            continue;
        }
        function.remove_enum_attribute(AttributeLoc::Function, memory);
        function.add_attribute(AttributeLoc::Function, context.create_enum_attribute(memory, 0));
        for name in ["nounwind", "willreturn", "norecurse"] {
            let kind = attribute_kind(name);
            function.add_attribute(AttributeLoc::Function, context.create_enum_attribute(kind, 0));
        }
    }
    run(&copy, "cgscc(function-attrs)", machine, false)?;
    let function = copy.get_function(OWN).ok_or(Unhandled::Shape("a copy without the function"))?;
    Ok(attrs_of(function))
}

/// Compiles the function of `own`, a module [`prelink`] made, with `imports` linked in as
/// bodies only available externally and what `given` says applied, through the post-link
/// pipeline of `optimization`. `wants` has the function's requests and, after them, those
/// of the imports (`Import::funcs`, `Import::datas`); `signature` is the function's.
pub(crate) fn compile(
    wants: &mut Wants,
    own: &[u8],
    signature: &ir::Signature,
    imports: &[Import],
    given: &Given,
    optimization: Optimization,
) -> Result<Code, Unhandled> {
    let tally = Tally::global();
    // Where a parameter is the only way to its memory, as `compiled` wraps it.
    let wrapped = optimization.pipeline().is_some()
        && wants.pointees.iter().any(|(_, it)| it.unique)
        && crate::Target::selected().is_ok_and(|it| it.object_format == crate::ObjectFormat::MachO);
    Machine::with(optimization, |context, machine| {
        let module = context.create_module("blaze");
        module.set_triple(&machine.get_triple());
        module.set_data_layout(&machine.get_target_data().get_data_layout());
        let readonly: FxHashMap<u32, &crate::ConstBlob> = wants.readonly().collect();
        let pointer = machine.get_target_data().get_pointer_byte_size(None) as usize;
        told_constants(context, &module, &readonly, pointer);
        tally.time(Phase::Lower, || {
            let source = MemoryBuffer::create_from_memory_range(own, "own");
            let parsed = context.create_module_from_ir(source).map_err(llvm)?;
            module.link_in_module(parsed).map_err(llvm)?;
            link(context, &module, imports, Linkage::AvailableExternally)?;
            if !imports.is_empty() {
                agree_calls(context, &module)?;
            }
            Ok::<_, Unhandled>(())
        })?;
        let function = module.get_function(OWN).ok_or(Unhandled::Shape("a module without its function"))?;
        apply(context, &module, function, given, pointer as u32 * 8)?;
        if wrapped {
            function.as_global_value().set_name(INNER);
            wrap(context, &module, function, None)?;
        }
        tally.time(Phase::Verify, || module.verify()).map_err(llvm)?;
        if let Some(passes) = postlink(optimization) {
            run(&module, passes, machine, true)?;
            if rebase::rebase(&module) {
                run(&module, REBASED, machine, false)?;
            }
        }
        if !imports.is_empty() {
            run(&module, "elim-avail-extern", machine, false)?;
        }
        retold_bytes(context, &module, &readonly);
        if module.get_function(INNER).is_some() {
            return Err(Unhandled::Shape("a body not built into the function that calls it"));
        }
        let object = tally
            .time(Phase::Emit, || machine.write_to_memory_buffer(&module, FileType::Object))
            .map_err(llvm)?;
        let read = [(OWN, signature.clone())];
        let mut codes = tally.time(Phase::Read, || emitted::read(object.as_slice(), &read, wants))?;
        codes.pop().ok_or(Unhandled::Shape("an object without the function"))?
    })
}

/// Gives `function` and the functions of `module` it calls what `given` says of them.
/// `pointer` is the width of an address, in bits.
fn apply<'ctx>(
    context: &'ctx Context,
    module: &Module<'ctx>,
    function: FunctionValue<'ctx>,
    given: &Given,
    pointer: u32,
) -> Result<(), Unhandled> {
    for &(at, arg) in &given.constants {
        let param = function
            .get_nth_param(at)
            .ok_or(Unhandled::Shape("a constant for a parameter the function lacks"))?;
        let Arg::Int { bits, value } = arg else {
            return Err(Unhandled::Shape("a constant parameter that is not a constant"));
        };
        let value = u64::try_from(value).map_err(|_| Unhandled::Shape("a constant of more than 64 bits"))?;
        let constant: BasicValueEnum<'ctx> = match param.get_type() {
            BasicTypeEnum::IntType(ty) if ty.get_bit_width() == u32::from(bits) => {
                ty.const_int(value, false).into()
            }
            BasicTypeEnum::PointerType(ty) if u32::from(bits) == pointer => {
                context.custom_width_int_type(u32::from(bits)).const_int(value, false).const_to_pointer(ty).into()
            }
            _ => return Err(Unhandled::Shape("a constant of another type than its parameter")),
        };
        // SAFETY: both are values of the module, of one type.
        unsafe { core::LLVMReplaceAllUsesWith(param.as_value_ref(), constant.as_value_ref()) };
    }
    let memory = attribute_kind("memory");
    let read = if given.callees.iter().any(|(_, it)| it.readonly && !it.readnone) {
        read_only(context)?
    } else {
        0
    };
    for &(request, attrs) in &given.callees {
        let Some(callee) = module.get_function(&Symbol::Func(request).to_string()) else { continue };
        let flags = [
            ("nounwind", attrs.nounwind),
            ("willreturn", attrs.willreturn),
            ("norecurse", attrs.norecurse),
            ("noreturn", attrs.noreturn),
        ];
        for (name, _) in flags.iter().filter(|it| it.1) {
            let kind = attribute_kind(name);
            callee.add_attribute(AttributeLoc::Function, context.create_enum_attribute(kind, 0));
        }
        let assumed = match (attrs.readnone, attrs.readonly) {
            (true, _) => Some(0),
            (false, true) => Some(read),
            (false, false) => None,
        };
        if let Some(assumed) = assumed {
            // What the declaration says already holds too: the two are met.
            let had = callee.get_enum_attribute(AttributeLoc::Function, memory).map(|it| it.get_enum_value());
            let value = had.map_or(assumed, |had| had & assumed);
            callee.remove_enum_attribute(AttributeLoc::Function, memory);
            callee.add_attribute(AttributeLoc::Function, context.create_enum_attribute(memory, value));
        }
    }
    for (request, params) in &given.dead {
        let Some(callee) = module.get_function(&Symbol::Func(*request).to_string()) else { continue };
        let params: Vec<u32> = params
            .iter()
            .copied()
            .filter(|&at| at < callee.count_params() && !in_memory(callee, at))
            .collect();
        for &at in &params {
            for name in UB_IF_POISON {
                callee.remove_enum_attribute(AttributeLoc::Param(at), attribute_kind(name));
            }
        }
        for call in calls_of(callee) {
            // SAFETY: a call of the module, its arguments of the types its callee is called
            // through.
            unsafe {
                let args = core::LLVMGetNumArgOperands(call);
                for &at in params.iter().filter(|&&at| at < args) {
                    let arg = core::LLVMGetOperand(call, at);
                    core::LLVMSetOperand(call, at, core::LLVMGetPoison(core::LLVMTypeOf(arg)));
                    for name in UB_IF_POISON {
                        core::LLVMRemoveCallSiteEnumAttribute(call, at + 1, attribute_kind(name));
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed<'ctx>(context: &'ctx Context, text: &str) -> Module<'ctx> {
        let text = format!("{text}\0");
        let source = MemoryBuffer::create_from_memory_range_copy(text.as_bytes(), "test");
        context.create_module_from_ir(source).unwrap()
    }

    fn machine() -> TargetMachine {
        let optimization = Optimization {
            passes: Passes::Aggressive,
            selection: crate::backend::Selection::Optimized,
        };
        Machine::create(optimization).unwrap()
    }

    /// Calls its request 1 twice with one constant and its request 2 with two, takes the
    /// address of its request 3, calls the symbol 4 and never reads its second parameter.
    const CALLER: &str = r#"
define i32 @"blaze.own"(i32 %n, i64 %unused, ptr byval(i64) %copied) {
  %a = call i32 @"blaze.f.1"(i32 7, i32 %n)
  %b = call i32 @"blaze.f.1"(i32 7, i32 3)
  %c = call i32 @"blaze.f.2"(i32 1)
  %d = call i32 @"blaze.f.2"(i32 2)
  %e = call i32 @"blaze.f.4"(ptr @"blaze.f.3")
  %ab = add i32 %a, %b
  %cd = add i32 %c, %d
  %all = add i32 %ab, %cd
  %more = add i32 %all, %e
  ret i32 %more
}

declare i32 @"blaze.f.1"(i32, i32)
declare i32 @"blaze.f.2"(i32)
declare i32 @"blaze.f.3"(i32)
declare i32 @"blaze.f.4"(ptr)
"#;

    #[test]
    fn the_facts_of_a_function_alone() {
        let context = Context::create();
        let module = parsed(&context, CALLER);
        let function = module.get_function(OWN).unwrap();
        let instances = |it: u32| it != 4;
        let facts = facts(&context, &module, function, &instances, &machine()).unwrap();
        assert_eq!(facts.params, 3);
        // The `byval` one is read by the call.
        assert_eq!(facts.unused, vec![1]);
        let int = |value| Arg::Int { bits: 32, value };
        assert_eq!(
            facts.calls,
            vec![
                Called { request: 1, count: 2, args: vec![int(7), Arg::Unknown] },
                Called { request: 2, count: 2, args: vec![Arg::Unknown] },
            ]
        );
        assert_eq!(facts.addressed, vec![3]);
    }

    const PURE: &str = r#"
define i32 @"blaze.own"(i32 %n) {
  %a = call i32 @"blaze.f.1"(i32 %n)
  %b = add i32 %a, 1
  ret i32 %b
}

declare i32 @"blaze.f.1"(i32)
"#;

    #[test]
    fn the_body_assumes_the_best_of_the_builds_callees_and_the_function_nothing() {
        let context = Context::create();
        let module = parsed(&context, PURE);
        let machine = machine();
        let alone = facts(&context, &module, module.get_function(OWN).unwrap(), &|_| true, &machine).unwrap();
        assert!(!alone.attrs.nounwind && !alone.attrs.readnone, "{:?}", alone.attrs);
        assert!(alone.body.nounwind && alone.body.readnone && alone.body.readonly, "{:?}", alone.body);
        assert!(alone.body.willreturn && alone.body.norecurse, "{:?}", alone.body);
        // The module is as it was: the callee is assumed nothing of.
        let callee = module.get_function("blaze.f.1").unwrap();
        assert!(callee.get_enum_attribute(AttributeLoc::Function, attribute_kind("nounwind")).is_none());
        // An outside symbol is assumed nothing of.
        let outside = facts(&context, &module, module.get_function(OWN).unwrap(), &|_| false, &machine).unwrap();
        assert!(!outside.body.nounwind && !outside.body.readnone, "{:?}", outside.body);
        assert!(outside.calls.is_empty());
    }

    #[test]
    fn given_facts_are_applied() {
        let context = Context::create();
        let module = parsed(&context, CALLER);
        let function = module.get_function(OWN).unwrap();
        let given = Given {
            constants: vec![(0, Arg::Int { bits: 32, value: 5 })],
            callees: vec![
                (1, Attrs { nounwind: true, readonly: true, ..Attrs::default() }),
                (2, Attrs { readnone: true, readonly: true, willreturn: true, ..Attrs::default() }),
            ],
            dead: vec![(1, vec![0])],
        };
        apply(&context, &module, function, &given, 64).unwrap();
        module.verify().unwrap();
        let text = function.print_to_string().to_string_lossy().into_owned();
        assert!(text.contains("@blaze.f.1(i32 poison, i32 5)"), "{text}");
        assert!(text.contains("@blaze.f.1(i32 poison, i32 3)"), "{text}");
        let all = module.print_to_string().to_string_lossy().into_owned();
        let one = attrs_of(module.get_function("blaze.f.1").unwrap());
        assert!(one.nounwind && one.readonly && !one.readnone, "{all}");
        let two = attrs_of(module.get_function("blaze.f.2").unwrap());
        assert!(two.readnone && two.willreturn && !two.nounwind, "{all}");
    }

    #[test]
    fn a_dead_argument_loses_what_makes_poison_undefined() {
        let context = Context::create();
        let module = parsed(
            &context,
            r#"
define void @"blaze.own"(ptr %p) {
  call void @"blaze.f.1"(ptr noundef nonnull %p)
  ret void
}

declare void @"blaze.f.1"(ptr noundef nonnull)
"#,
        );
        let function = module.get_function(OWN).unwrap();
        let given = Given { dead: vec![(1, vec![0])], ..Given::default() };
        apply(&context, &module, function, &given, 64).unwrap();
        module.verify().unwrap();
        let all = module.print_to_string().to_string_lossy().into_owned();
        assert!(all.contains("@blaze.f.1(ptr poison)"), "{all}");
        assert!(!all.contains("noundef") && !all.contains("nonnull"), "{all}");
    }

    #[test]
    fn a_pre_linked_function_compiles_with_its_import_built_in() {
        let context = Context::create();
        let optimization = Optimization {
            passes: Passes::Aggressive,
            selection: crate::backend::Selection::Optimized,
        };
        let machine = machine();
        let module = parsed(&context, PURE);
        run(&module, prelink_passes(optimization).unwrap(), &machine, true).unwrap();
        let callee = parsed(
            &context,
            r#"
define i32 @"blaze.own"(i32 %n) {
  %m = mul i32 %n, 3
  ret i32 %m
}
"#,
        );
        let export = crate::imports::Export {
            bitcode: callee.write_bitcode_to_memory().as_slice().to_vec(),
            requests: Default::default(),
            signature: ir::Signature::new(cranelift_codegen::isa::CallConv::SystemV),
            size: 2,
            hint: Hint::None,
        };
        let import = Import { export: std::sync::Arc::new(export), id: 1, funcs: vec![1], datas: Vec::new() };
        let into = context.create_module("blaze");
        into.link_in_module(module).unwrap();
        link(&context, &into, std::slice::from_ref(&import), Linkage::AvailableExternally).unwrap();
        run(&into, postlink(optimization).unwrap(), &machine, true).unwrap();
        run(&into, "elim-avail-extern", &machine, false).unwrap();
        let own = into.get_function(OWN).unwrap();
        let text = own.print_to_string().to_string_lossy().into_owned();
        assert!(!text.contains("blaze.f.1") && text.contains("mul i32"), "{text}");
    }
}

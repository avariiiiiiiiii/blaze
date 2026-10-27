//! An LLVM intrinsic the front half has no lowering of, as a function LLVM compiles: a
//! *helper*. rustc hands LLVM a call of the intrinsic, and LLVM selects the instruction; the
//! front half makes Cranelift IR, which has no such call, so it calls a function of LLVM's
//! making instead. The helper loads each operand from where the caller points (a vector
//! the way ra-jit keeps one, in memory), calls the intrinsic with its real signature, and
//! stores the result where the caller points. An operand LLVM takes as an immediate
//! (`immarg`) is a constant of the helper, so the constant is part of what the helper is.
//!
//! The helper is compiled for the target features of the function that calls the
//! intrinsic and no more, as rustc compiles that function: a helper of a function of SSE
//! does not use VEX's encodings, which a processor without AVX does not run. Its machine
//! code goes into the object as the text of a function of bytes, which the assembler of
//! `asm!` blocks takes ([`super::super::asm::machine_code`]); that text is the code's
//! identity in the store.

use std::sync::{Arc, LazyLock, Mutex};

use inkwell::{
    AddressSpace,
    attributes::AttributeLoc,
    context::Context,
    intrinsics::Intrinsic,
    memory_buffer::MemoryBuffer,
    module::{Linkage, Module},
    targets::{FileType, TargetMachine},
    types::{BasicMetadataTypeEnum, BasicType, BasicTypeEnum},
    values::{BasicMetadataValueEnum, BasicValue, BasicValueEnum, FunctionValue},
};
use object::{Object, ObjectSection, ObjectSymbol, RelocationTarget};
use rustc_hash::FxHashMap;

use super::{Machine, Unhandled};
use crate::{
    Arch, Target,
    backend::{Optimization, Passes, Selection},
};

/// The name of the helper, in the module and in the object LLVM writes of it.
const NAME: &str = "ra_jit_intrinsic";

/// A value of the call as Rust lays it out. An overloaded intrinsic's types are those of
/// its operands, as rustc declares it with them.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Shape {
    Scalar(Scalar),
    Vector { lane: Scalar, count: u32 },
    /// A tuple, each field at its offset and of its size, which an intrinsic that answers
    /// several values (`{ i8, i32 }`) answers, field by field.
    Fields(Vec<(u64, u64)>),
    /// Nothing: the call answers no value.
    Unit,
    /// Something else, which only a non-overloaded intrinsic takes, by its size.
    Bytes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Scalar {
    Int(u32),
    Float(u32),
    Pointer,
}

/// An operand of the call.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Operand {
    pub(crate) shape: Shape,
    pub(crate) size: u64,
    /// Its bits, when they are known while compiling.
    pub(crate) constant: Option<u128>,
}

/// A call of `llvm.{name}`, in a function of `features`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Call {
    pub(crate) name: String,
    pub(crate) operands: Vec<Operand>,
    pub(crate) result: Shape,
    pub(crate) result_size: u64,
    /// The target features the calling function enables, as Rust names them, sorted.
    pub(crate) features: Vec<String>,
}

/// The helper of a call: the text of its function, and how it is called. It takes the
/// address of each operand it is passed ([`Self::passed`]) in order, then that of the
/// result when there is one, and answers nothing.
#[derive(Debug)]
pub(crate) struct Helper {
    pub(crate) text: String,
    /// For each operand, whether the helper is passed its address: the others are
    /// constants of the helper.
    pub(crate) passed: Vec<bool>,
    pub(crate) returns: bool,
}

/// The helper of `call`, made once per process: the store keeps what the text assembles to.
/// Why there is none is the error.
pub(crate) fn helper(call: &Call) -> Result<Arc<Helper>, String> {
    static MADE: LazyLock<Mutex<FxHashMap<Call, Result<Arc<Helper>, String>>>> =
        LazyLock::new(Mutex::default);
    let known = |call: &Call| MADE.lock().unwrap_or_else(|e| e.into_inner()).get(call).cloned();
    if let Some(made) = known(call) {
        return made;
    }
    // Optimized selection, without passes: the helper is one call between loads and stores.
    let optimization = Optimization { passes: Passes::Less, selection: Selection::Optimized };
    let made = Machine::with(optimization, |context, machine| build(context, machine, call))
        .map_err(|e| match e {
            Unhandled::Llvm(why) => why,
            other => other.to_string(),
        })
        .map(Arc::new);
    MADE.lock().unwrap_or_else(|e| e.into_inner()).insert(call.clone(), made.clone());
    made
}

fn build(context: &Context, machine: &TargetMachine, call: &Call) -> Result<Helper, Unhandled> {
    let refuse = |why: String| Unhandled::Llvm(why);
    let module = context.create_module("ra_jit_intrinsic");
    module.set_triple(&machine.get_triple());
    let data = machine.get_target_data();
    module.set_data_layout(&data.get_data_layout());
    let full = format!("llvm.{}", call.name);
    let found = Intrinsic::find(&full);
    // A non-overloaded intrinsic has one signature, which rustc's declaration meets by
    // bitcasting operands of the same size (bf16 lanes as integers, masks of bits as
    // integers). An overloaded one is declared with the types of the operands, as rustc
    // declares it; so is one this LLVM does not have, which reading the module back
    // upgrades to what replaced it, as LLVM upgrades the IR of an older version. LLVM finds
    // an overloaded intrinsic by the start of its name, so a name it upgrades can look like
    // one it has (`x86.avx512.mask.compress.w.128` like `x86.avx512.mask.compress`): only
    // a declaration that reading back keeps is of the intrinsic it found.
    let (found, declared) = match found {
        Some(intrinsic) if !intrinsic.is_overloaded() => {
            let declared = intrinsic
                .get_declaration(&module, &[])
                .ok_or_else(|| refuse(format!("LLVM gave no declaration of `{full}`")))?;
            (found, declared)
        }
        _ => {
            let params: Vec<BasicMetadataTypeEnum> = call
                .operands
                .iter()
                .map(|it| llvm_type(context, &it.shape).map(Into::into))
                .collect::<Result<_, _>>()
                .map_err(refuse)?;
            let ty = match &call.result {
                Shape::Unit => context.void_type().fn_type(&params, false),
                shape => llvm_type(context, shape).map_err(refuse)?.fn_type(&params, false),
            };
            let found = match found {
                Some(_) if !kept(context, &full, ty)? => None,
                found => found,
            };
            (found, module.add_function(&full, ty, Some(Linkage::External)))
        }
    };
    let ty = declared.get_type();
    plain_types(ty).map_err(|why| refuse(format!("`{full}` {why}")))?;
    let params = ty.get_param_types();
    if params.len() != call.operands.len() {
        return Err(refuse(format!(
            "`{full}` takes {} operands where it is passed {}",
            params.len(),
            call.operands.len()
        )));
    }
    let immarg = super::attribute_kind("immarg");
    let mut passed = Vec::with_capacity(params.len());
    for (index, operand) in call.operands.iter().enumerate() {
        let immediate =
            declared.get_enum_attribute(AttributeLoc::Param(index as u32), immarg).is_some();
        if immediate && operand.constant.is_none() {
            return Err(refuse(format!(
                "`{full}` with operand {index} not a constant, which LLVM takes as an immediate"
            )));
        }
        // Where LLVM does not know the intrinsic it does not say what is an immediate: a
        // constant is passed as one, which what it is upgraded to may need.
        passed.push(!(immediate || found.is_none() && operand.constant.is_some()));
    }
    let returns = ty.get_return_type().is_some();
    let ptr = context.ptr_type(AddressSpace::default());
    let helper_params = vec![ptr.into(); passed.iter().filter(|it| **it).count() + returns as usize];
    let function =
        module.add_function(NAME, context.void_type().fn_type(&helper_params, false), None);
    mark(context, function, call);
    let builder = context.create_builder();
    builder.position_at_end(context.append_basic_block(function, "entry"));
    let llvm = |e: inkwell::builder::BuilderError| Unhandled::Llvm(e.to_string());
    let mut addresses = function.get_param_iter().map(|it| it.into_pointer_value());
    let mut values: Vec<BasicMetadataValueEnum> = Vec::with_capacity(params.len());
    for (index, (operand, param)) in call.operands.iter().zip(&params).enumerate() {
        let param = BasicTypeEnum::try_from(*param)
            .map_err(|()| refuse(format!("operand {index} of `{full}` is metadata")))?;
        let value: BasicValueEnum = if passed[index] {
            let address = addresses.next().expect("a parameter for each operand passed");
            match bits_vector(param) {
                // A vector of `i1`s is a mask the caller keeps in an integer of its bits.
                Some(count) => {
                    let whole = int(context, operand.size as u32 * 8).map_err(refuse)?;
                    let bits = builder.build_load(whole, address, "mask").map_err(llvm)?;
                    let bits = bits.into_int_value();
                    let bits = if count < operand.size as u32 * 8 {
                        let narrow = int(context, count).map_err(refuse)?;
                        builder.build_int_truncate(bits, narrow, "mask").map_err(llvm)?
                    } else {
                        bits
                    };
                    builder.build_bit_cast(bits, param, "mask").map_err(llvm)?
                }
                None => {
                    same_size(&data, param, operand.size)
                        .map_err(|why| refuse(format!("operand {index} of `{full}` {why}")))?;
                    let value = builder.build_load(param, address, "operand").map_err(llvm)?;
                    set_unaligned(value);
                    value
                }
            }
        } else {
            let bits = operand.constant.expect("an operand not passed is a constant");
            let BasicTypeEnum::IntType(int) = param else {
                return Err(refuse(format!(
                    "`{full}` takes operand {index} as an immediate that is not an integer"
                )));
            };
            let words = [bits as u64, (bits >> 64) as u64];
            int.const_int_arbitrary_precision(&words).into()
        };
        values.push(value.into());
    }
    let answer = builder.build_call(declared, &values, "").map_err(llvm)?;
    if returns {
        let to = addresses.next().expect("a parameter for the result");
        let value = answer.try_as_basic_value().basic().expect("a value of a function that answers");
        store(context, &builder, &data, value, to, &call.result, call.result_size)
            .map_err(|why| refuse(format!("the result of `{full}` {why}")))?;
    }
    builder.build_return(None).map_err(llvm)?;
    // A name LLVM upgrades is checked as what it is upgraded to.
    let module = match found {
        Some(_) => module,
        None => upgraded(context, &module, &full)?,
    };
    module.verify().map_err(|e| refuse(format!("`{full}`: {}", e.to_string_lossy())))?;
    let object = machine
        .write_to_memory_buffer(&module, FileType::Object)
        .map_err(|e| refuse(e.to_string_lossy().into_owned()))?;
    let code = code(object.as_slice()).map_err(|why| refuse(format!("`{full}`: {why}")))?;
    Ok(Helper { text: crate::asm::machine_code(&code), passed, returns })
}

/// Whether LLVM keeps a declaration of `full` of type `ty` when it reads it back, rather
/// than upgrading it to something else.
fn kept<'ctx>(
    context: &'ctx Context,
    full: &str,
    ty: inkwell::types::FunctionType<'ctx>,
) -> Result<bool, Unhandled> {
    let probe = context.create_module("ra_jit_intrinsic_probe");
    probe.add_function(full, ty, Some(Linkage::External));
    let text = probe.print_to_string();
    let buffer = MemoryBuffer::create_from_memory_range_copy(text.to_bytes(), "probe");
    let read = context
        .create_module_from_ir(buffer)
        .map_err(|e| Unhandled::Llvm(format!("`{full}`: {}", e.to_string_lossy())))?;
    Ok(read.get_function(full).is_some())
}

/// `module` read back from its text, which upgrades the calls of an intrinsic this LLVM no
/// longer has to what replaced it. One no version had stays an error.
fn upgraded<'ctx>(
    context: &'ctx Context,
    module: &Module<'ctx>,
    full: &str,
) -> Result<Module<'ctx>, Unhandled> {
    let text = module.print_to_string();
    let buffer = MemoryBuffer::create_from_memory_range_copy(text.to_bytes(), "ra_jit_intrinsic");
    let module = context
        .create_module_from_ir(buffer)
        .map_err(|e| Unhandled::Llvm(format!("`{full}`: {}", e.to_string_lossy())))?;
    let used = module.get_function(full).and_then(|it| {
        it.as_global_value().as_pointer_value().get_first_use().map(|_| ())
    });
    if used.is_some() {
        return Err(Unhandled::Llvm(format!("LLVM has no intrinsic `{full}`")));
    }
    Ok(module)
}

/// The helper is compiled for the calling function's target features, on the target's
/// baseline processor, and does not unwind: LLVM writes no unwind table of it.
fn mark<'ctx>(context: &'ctx Context, function: FunctionValue<'ctx>, call: &Call) {
    let target = Target::selected().expect("a target LLVM compiles for");
    let features: Vec<String> = call
        .features
        .iter()
        .flat_map(|it| llvm_features(target.arch, it))
        .map(|it| format!("+{it}"))
        .collect();
    let at = AttributeLoc::Function;
    function.add_attribute(at, context.create_string_attribute("target-cpu", target.cpu()));
    if !features.is_empty() {
        let features = features.join(",");
        function.add_attribute(at, context.create_string_attribute("target-features", &features));
    }
    let nounwind = context.create_enum_attribute(super::attribute_kind("nounwind"), 0);
    function.add_attribute(at, nounwind);
}

/// What LLVM calls a target feature Rust names `feature`, as rustc tells LLVM
/// (`rustc_codegen_llvm::llvm_util::to_llvm_features`).
fn llvm_features(arch: Arch, feature: &str) -> Vec<&str> {
    let one = |it| vec![it];
    match (arch, feature) {
        // LLVM has CRC-32 apart from the rest of SSE 4.2, which Rust's feature includes.
        (Arch::X86_64, "sse4.2") => vec!["sse4.2", "crc32"],
        (Arch::X86_64, "pclmulqdq") => one("pclmul"),
        (Arch::X86_64, "rdrand") => one("rdrnd"),
        (Arch::X86_64, "bmi1") => one("bmi"),
        (Arch::X86_64, "cmpxchg16b") => one("cx16"),
        (Arch::X86_64, "lahfsahf") => one("sahf"),
        (Arch::X86_64, "apxf") => vec!["egpr", "push2pop2", "ppx", "ndd", "ccmp", "cf", "nf", "zu"],
        (Arch::Aarch64, "rcpc2") => one("rcpc-immo"),
        (Arch::Aarch64, "dpb") => one("ccpp"),
        (Arch::Aarch64, "dpb2") => one("ccdp"),
        (Arch::Aarch64, "frintts") => one("fptoint"),
        (Arch::Aarch64, "fcma") => one("complxnum"),
        (Arch::Aarch64, "pmuv3") => one("perfmon"),
        (Arch::Aarch64, "paca" | "pacg") => one("pauth"),
        (Arch::Aarch64, "flagm2") => one("altnzcv"),
        (Arch::Aarch64, "fhm") => one("fp16fml"),
        (Arch::Aarch64, "fp16") => one("fullfp16"),
        (Arch::Aarch64, "sve-b16b16") => one("b16b16"),
        (Arch::Aarch64, "sve2-bitperm") => one("sve-bitperm"),
        _ => one(feature),
    }
}

/// The LLVM type of a value of `shape`, for an intrinsic declared with its operands' types.
fn llvm_type<'ctx>(context: &'ctx Context, shape: &Shape) -> Result<BasicTypeEnum<'ctx>, String> {
    let scalar = |scalar: Scalar| -> Result<BasicTypeEnum<'ctx>, String> {
        Ok(match scalar {
            Scalar::Int(bits) => int(context, bits)?.into(),
            Scalar::Float(16) => context.f16_type().into(),
            Scalar::Float(32) => context.f32_type().into(),
            Scalar::Float(64) => context.f64_type().into(),
            Scalar::Float(128) => context.f128_type().into(),
            Scalar::Float(bits) => return Err(format!("takes a float of {bits} bits")),
            Scalar::Pointer => context.ptr_type(AddressSpace::default()).into(),
        })
    };
    Ok(match shape {
        Shape::Scalar(it) => scalar(*it)?,
        Shape::Vector { lane, count } => match scalar(*lane)? {
            BasicTypeEnum::IntType(it) => it.vec_type(*count).into(),
            BasicTypeEnum::FloatType(it) => it.vec_type(*count).into(),
            BasicTypeEnum::PointerType(it) => it.vec_type(*count).into(),
            _ => unreachable!("a scalar is an integer, a float or a pointer"),
        },
        Shape::Fields(_) | Shape::Unit | Shape::Bytes => {
            return Err(format!("is overloaded on a value Rust lays out as {shape:?}"));
        }
    })
}

/// Whether every type of `ty` is one a Rust value is loaded as: not AMX's tiles, which live
/// in registers of their own and which inkwell has no type of.
fn plain_types(ty: inkwell::types::FunctionType<'_>) -> Result<(), String> {
    use inkwell::{
        llvm_sys::{LLVMTypeKind, core as llvm},
        types::AsTypeRef,
    };
    let raw = ty.as_type_ref();
    // SAFETY: `raw` is a function type, which has as many parameter types as it counts.
    let types = unsafe {
        let mut params = vec![std::ptr::null_mut(); llvm::LLVMCountParamTypes(raw) as usize];
        llvm::LLVMGetParamTypes(raw, params.as_mut_ptr());
        params.push(llvm::LLVMGetReturnType(raw));
        params
    };
    for ty in types {
        // SAFETY: each is a type of the function type.
        match unsafe { llvm::LLVMGetTypeKind(ty) } {
            LLVMTypeKind::LLVMX86_AMXTypeKind => return Err("takes or answers an AMX tile".into()),
            LLVMTypeKind::LLVMTokenTypeKind | LLVMTypeKind::LLVMLabelTypeKind => {
                return Err("takes or answers a token or a label".into());
            }
            _ => {}
        }
    }
    Ok(())
}

/// The integer type of `bits` bits.
fn int(context: &Context, bits: u32) -> Result<inkwell::types::IntType<'_>, String> {
    let width = std::num::NonZero::new(bits).ok_or("an integer of no bits")?;
    context.custom_width_int_type(width).map_err(|e| format!("an integer of {bits} bits: {e}"))
}

/// The lanes of a vector of `i1`s.
fn bits_vector(ty: BasicTypeEnum<'_>) -> Option<u32> {
    match ty {
        BasicTypeEnum::VectorType(vector) => match vector.get_element_type() {
            BasicTypeEnum::IntType(lane) if lane.get_bit_width() == 1 => Some(vector.get_size()),
            _ => None,
        },
        _ => None,
    }
}

fn same_size(
    data: &inkwell::targets::TargetData,
    ty: BasicTypeEnum<'_>,
    size: u64,
) -> Result<(), String> {
    let stored = data.get_store_size(&ty);
    if stored == size {
        Ok(())
    } else {
        Err(format!("is {size} bytes in Rust where LLVM's `{ty}` is {stored}"))
    }
}

/// Loads and stores of the helper assume no alignment: the caller's memory is a Rust value
/// of the operand's own type, whose alignment need not be the LLVM type's.
fn set_unaligned(value: BasicValueEnum<'_>) {
    if let Some(load) = value.as_instruction_value() {
        _ = load.set_alignment(1);
    }
}

/// Stores the intrinsic's `value` at `to`, where Rust keeps a value of `shape`.
fn store<'ctx>(
    context: &'ctx Context,
    builder: &inkwell::builder::Builder<'ctx>,
    data: &inkwell::targets::TargetData,
    value: BasicValueEnum<'ctx>,
    to: inkwell::values::PointerValue<'ctx>,
    shape: &Shape,
    size: u64,
) -> Result<(), String> {
    let error = |e: inkwell::builder::BuilderError| e.to_string();
    let put = |value: BasicValueEnum<'ctx>, to| -> Result<(), String> {
        let store = builder.build_store(to, value).map_err(error)?;
        _ = store.set_alignment(1);
        Ok(())
    };
    let ty = value.get_type();
    if let Some(count) = bits_vector(ty) {
        let bits = builder
            .build_bit_cast(value, int(context, count)?, "mask")
            .map_err(error)?
            .into_int_value();
        let whole = int(context, size as u32 * 8)?;
        let bits = if count < size as u32 * 8 {
            builder.build_int_z_extend(bits, whole, "mask").map_err(error)?
        } else {
            bits
        };
        return put(bits.into(), to);
    }
    match (ty, shape) {
        (BasicTypeEnum::StructType(fields), Shape::Fields(rust)) => {
            if fields.count_fields() as usize != rust.len() {
                return Err(format!("is `{ty}`, which Rust has {} fields of", rust.len()));
            }
            for (index, &(offset, size)) in rust.iter().enumerate() {
                let field = builder
                    .build_extract_value(value.into_struct_value(), index as u32, "field")
                    .map_err(error)?;
                same_size(data, field.get_type(), size)
                    .map_err(|why| format!("field {index} {why}"))?;
                let i8 = context.i8_type();
                // SAFETY: the offset is within the value Rust lays out at `to`.
                let at = unsafe {
                    builder.build_in_bounds_gep(i8, to, &[i8.const_int(offset, false)], "field")
                }
                .map_err(error)?;
                put(field, at)?;
            }
            Ok(())
        }
        (BasicTypeEnum::StructType(_), _) => Err(format!("is `{ty}`, which Rust has no fields of")),
        _ => {
            same_size(data, ty, size)?;
            put(value, to)
        }
    }
}

/// The machine code of the helper in the object LLVM wrote. Code that refers to anything,
/// a constant LLVM put in a section of its own or a function it calls, is an error: the
/// text of a function of bytes cannot say where that is.
fn code(object: &[u8]) -> Result<Vec<u8>, String> {
    let file = object::File::parse(object).map_err(|e| e.to_string())?;
    // The function, not the file symbol ELF names after the module.
    let symbol = file
        .symbols()
        .find(|it| {
            it.kind() == object::SymbolKind::Text
                && it.name().is_ok_and(|name| name.trim_start_matches('_') == NAME)
        })
        .ok_or("the object LLVM wrote lacks the helper")?;
    let section = symbol
        .section_index()
        .and_then(|it| file.section_by_index(it).ok())
        .ok_or("the helper is in no section")?;
    let bytes = section.data().map_err(|e| e.to_string())?;
    let start = (symbol.address() - section.address()) as usize;
    let end = match symbol.size() {
        0 => bytes.len(),
        size => start + size as usize,
    };
    let within = |at: u64| (start..end).contains(&(at as usize));
    if let Some((_, relocation)) = section.relocations().find(|(at, _)| within(*at)) {
        let target = match relocation.target() {
            RelocationTarget::Symbol(index) => file.symbol_by_index(index).ok().and_then(|it| {
                let name = it.name().ok().filter(|it| !it.is_empty()).map(str::to_owned);
                name.or_else(|| {
                    let section = file.section_by_index(it.section_index()?).ok()?;
                    section.name().ok().map(str::to_owned)
                })
            }),
            RelocationTarget::Section(index) => {
                file.section_by_index(index).ok().and_then(|it| it.name().ok().map(str::to_owned))
            }
            _ => None,
        };
        let target = target.unwrap_or_else(|| "something".to_owned());
        return Err(format!("LLVM's code of it refers to `{target}`, which a helper cannot"));
    }
    bytes.get(start..end).map(<[u8]>::to_vec).ok_or_else(|| "the helper is past its section".into())
}

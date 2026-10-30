//! LLVM through its C API: parse a module's text, compile it to an ELF object for one
//! variant, and disassemble stencils for the generator's checks. The one module of the
//! generator with `unsafe`; each call's invariant is said where it is made.

use std::{
    ffi::{CStr, CString, c_char},
    ptr,
    sync::Once,
};

use llvm_sys::{
    analysis::{LLVMVerifierFailureAction, LLVMVerifyModule},
    core::{
        LLVMContextCreate, LLVMContextDispose, LLVMCreateMemoryBufferWithMemoryRangeCopy,
        LLVMDisposeMemoryBuffer, LLVMDisposeMessage, LLVMDisposeModule, LLVMGetBufferSize,
        LLVMGetBufferStart, LLVMSetDataLayout, LLVMSetTarget,
    },
    disassembler::{
        LLVMCreateDisasmCPUFeatures, LLVMDisasmContextRef, LLVMDisasmDispose,
        LLVMDisasmInstruction,
    },
    ir_reader::LLVMParseIRInContext2,
    prelude::{LLVMMemoryBufferRef, LLVMModuleRef},
    target::{
        LLVMCopyStringRepOfTargetData, LLVMDisposeTargetData, LLVMInitializeAArch64AsmParser,
        LLVMInitializeAArch64AsmPrinter, LLVMInitializeAArch64Disassembler,
        LLVMInitializeAArch64Target, LLVMInitializeAArch64TargetInfo,
        LLVMInitializeAArch64TargetMC, LLVMInitializeX86AsmParser, LLVMInitializeX86AsmPrinter,
        LLVMInitializeX86Disassembler, LLVMInitializeX86Target, LLVMInitializeX86TargetInfo,
        LLVMInitializeX86TargetMC,
    },
    target_machine::{
        LLVMCodeGenFileType, LLVMCodeGenOptLevel, LLVMCodeModel, LLVMCreateTargetDataLayout,
        LLVMCreateTargetMachine, LLVMDisposeTargetMachine, LLVMGetTargetFromTriple,
        LLVMRelocMode, LLVMTargetMachineEmitToMemoryBuffer, LLVMTargetRef,
    },
};

use crate::format::Variant;

/// How LLVM is asked to compile one variant's stencils (stencil.md, "Why a build script").
#[derive(Clone, Copy, Debug)]
pub struct Machine {
    pub triple: &'static str,
    pub cpu: &'static str,
    pub features: &'static str,
    reloc: LLVMRelocMode,
    model: LLVMCodeModel,
}

impl Machine {
    /// x86-64: position-independent, so a place (the next stencil, a constant) is always
    /// RIP-relative, while the holes declared `!absolute_symbol` are absolute fields.
    /// AArch64: the tiny code model, so constants are `adr`/`ldr` literal (±1 MB) and never a
    /// page-relative `adrp`; `x18` reserved; atomics inline, never outlined into calls.
    pub const fn of(variant: Variant) -> Machine {
        match variant {
            Variant::X86_64 => Machine {
                triple: "x86_64-unknown-linux-gnu",
                cpu: "x86-64",
                features: "",
                reloc: LLVMRelocMode::LLVMRelocPIC,
                model: LLVMCodeModel::LLVMCodeModelSmall,
            },
            Variant::Aarch64 => Machine {
                triple: "aarch64-unknown-linux-gnu",
                cpu: "generic",
                features: "+neon,+reserve-x18,-outline-atomics,-lse",
                reloc: LLVMRelocMode::LLVMRelocStatic,
                model: LLVMCodeModel::LLVMCodeModelTiny,
            },
            Variant::Aarch64Lse => Machine {
                triple: "aarch64-unknown-linux-gnu",
                cpu: "generic",
                features: "+neon,+reserve-x18,-outline-atomics,+lse",
                reloc: LLVMRelocMode::LLVMRelocStatic,
                model: LLVMCodeModel::LLVMCodeModelTiny,
            },
        }
    }
}

/// Registers the two targets with LLVM, once per process.
pub fn init() {
    static INIT: Once = Once::new();
    // SAFETY: LLVM's target registration functions take no arguments and only fill LLVM's
    // global target registry; `Once` makes them run once, before any other call here.
    INIT.call_once(|| unsafe {
        LLVMInitializeX86TargetInfo();
        LLVMInitializeX86Target();
        LLVMInitializeX86TargetMC();
        LLVMInitializeX86AsmPrinter();
        LLVMInitializeX86AsmParser();
        LLVMInitializeX86Disassembler();
        LLVMInitializeAArch64TargetInfo();
        LLVMInitializeAArch64Target();
        LLVMInitializeAArch64TargetMC();
        LLVMInitializeAArch64AsmPrinter();
        LLVMInitializeAArch64AsmParser();
        LLVMInitializeAArch64Disassembler();
    });
}

fn cstring(text: &str) -> CString {
    CString::new(text).expect("LLVM strings hold no NUL")
}

/// Takes an LLVM-allocated message, freeing it.
///
/// # Safety
/// `message` is null or a string LLVM allocated for its caller to free.
unsafe fn message(message: *mut c_char) -> String {
    if message.is_null() {
        return String::from("(no message)");
    }
    // SAFETY: the caller's contract: a NUL-terminated string from LLVM, freed once here.
    unsafe {
        let text = CStr::from_ptr(message).to_string_lossy().into_owned();
        LLVMDisposeMessage(message);
        text
    }
}

/// Compiles one module of LLVM IR text for `machine` at `-O2`; the ELF object's bytes.
pub fn compile(machine: Machine, name: &str, ir: &str) -> Result<Vec<u8>, String> {
    init();
    let triple = cstring(machine.triple);
    let cpu = cstring(machine.cpu);
    let features = cstring(machine.features);
    let buffer_name = cstring(name);
    // SAFETY: every pointer handed to LLVM is a live `CString` or the text's own bytes with
    // its length; every object LLVM returns is disposed exactly once below, on every path,
    // and none is used after its disposal. The context is used by this thread alone.
    unsafe {
        let context = LLVMContextCreate();
        let buffer = LLVMCreateMemoryBufferWithMemoryRangeCopy(
            ir.as_ptr().cast(),
            ir.len(),
            buffer_name.as_ptr(),
        );
        let mut module: LLVMModuleRef = ptr::null_mut();
        let mut error: *mut c_char = ptr::null_mut();
        let parsed = LLVMParseIRInContext2(context, buffer, &mut module, &mut error);
        LLVMDisposeMemoryBuffer(buffer);
        if parsed != 0 {
            LLVMContextDispose(context);
            return Err(format!("LLVM refused the stencils' IR of {name}: {}", message(error)));
        }
        let result = emit(machine, &triple, &cpu, &features, module, name);
        LLVMDisposeModule(module);
        LLVMContextDispose(context);
        result
    }
}

/// # Safety
/// `module` is a live module; the strings are live.
unsafe fn emit(
    machine: Machine,
    triple: &CString,
    cpu: &CString,
    features: &CString,
    module: LLVMModuleRef,
    name: &str,
) -> Result<Vec<u8>, String> {
    // SAFETY: as `compile` says; the target machine and data layout are disposed here.
    unsafe {
        let mut target: LLVMTargetRef = ptr::null_mut();
        let mut error: *mut c_char = ptr::null_mut();
        if LLVMGetTargetFromTriple(triple.as_ptr(), &mut target, &mut error) != 0 {
            return Err(format!("LLVM has no target {}: {}", machine.triple, message(error)));
        }
        let tm = LLVMCreateTargetMachine(
            target,
            triple.as_ptr(),
            cpu.as_ptr(),
            features.as_ptr(),
            LLVMCodeGenOptLevel::LLVMCodeGenLevelDefault,
            machine.reloc,
            machine.model,
        );
        if tm.is_null() {
            return Err(format!("LLVM made no target machine for {}", machine.triple));
        }
        LLVMSetTarget(module, triple.as_ptr());
        let layout = LLVMCreateTargetDataLayout(tm);
        let layout_text = LLVMCopyStringRepOfTargetData(layout);
        LLVMSetDataLayout(module, layout_text);
        LLVMDisposeMessage(layout_text);
        LLVMDisposeTargetData(layout);
        let mut error: *mut c_char = ptr::null_mut();
        if LLVMVerifyModule(module, LLVMVerifierFailureAction::LLVMReturnStatusAction, &mut error)
            != 0
        {
            LLVMDisposeTargetMachine(tm);
            return Err(format!("the stencils' IR of {name} is not valid: {}", message(error)));
        }
        if !error.is_null() {
            LLVMDisposeMessage(error);
        }
        let mut out: LLVMMemoryBufferRef = ptr::null_mut();
        let mut error: *mut c_char = ptr::null_mut();
        let failed = LLVMTargetMachineEmitToMemoryBuffer(
            tm,
            module,
            LLVMCodeGenFileType::LLVMObjectFile,
            &mut error,
            &mut out,
        );
        LLVMDisposeTargetMachine(tm);
        if failed != 0 {
            return Err(format!("LLVM did not compile the stencils of {name}: {}", message(error)));
        }
        let start = LLVMGetBufferStart(out).cast::<u8>();
        let bytes = std::slice::from_raw_parts(start, LLVMGetBufferSize(out)).to_vec();
        LLVMDisposeMemoryBuffer(out);
        Ok(bytes)
    }
}

/// LLVM's disassembler for one variant, for the checks: each instruction's length and text.
pub struct Disasm {
    context: LLVMDisasmContextRef,
}

impl Disasm {
    pub fn new(machine: Machine) -> Result<Disasm, String> {
        init();
        let triple = cstring(machine.triple);
        let cpu = cstring(machine.cpu);
        let features = cstring(machine.features);
        // SAFETY: live strings; no callbacks and no user data, which LLVM accepts as null.
        let context = unsafe {
            LLVMCreateDisasmCPUFeatures(
                triple.as_ptr(),
                cpu.as_ptr(),
                features.as_ptr(),
                ptr::null_mut(),
                0,
                None,
                None,
            )
        };
        if context.is_null() {
            return Err(format!("LLVM has no disassembler for {}", machine.triple));
        }
        Ok(Disasm { context })
    }

    /// The instruction at the start of `code` (at `pc`): its length and text, or `None` when
    /// the bytes are no instruction.
    pub fn inst(&self, code: &[u8], pc: u64) -> Option<(usize, String)> {
        let mut text = [0 as c_char; 256];
        // SAFETY: LLVM reads at most `code.len()` bytes (it takes a `*mut` but does not write
        // through it) and writes at most `text.len()` bytes, NUL-terminated, into `text`.
        let len = unsafe {
            LLVMDisasmInstruction(
                self.context,
                code.as_ptr().cast_mut(),
                code.len() as u64,
                pc,
                text.as_mut_ptr(),
                text.len(),
            )
        };
        if len == 0 {
            return None;
        }
        // SAFETY: LLVM NUL-terminated what it wrote, within `text`.
        let text = unsafe { CStr::from_ptr(text.as_ptr()) }.to_string_lossy().trim().to_owned();
        Some((len, text))
    }
}

impl Drop for Disasm {
    fn drop(&mut self) {
        // SAFETY: the context was made by `new` and is disposed once, here.
        unsafe { LLVMDisasmDispose(self.context) }
    }
}

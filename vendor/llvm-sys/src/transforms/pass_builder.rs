#![allow(non_snake_case)]

use crate::error::LLVMErrorRef;
use crate::prelude::*;
use crate::target_machine::LLVMTargetMachineRef;

#[derive(Debug)]
pub enum LLVMOpaquePassBuilderOptions {}
pub type LLVMPassBuilderOptionsRef = *mut LLVMOpaquePassBuilderOptions;

extern "C" {
    pub fn LLVMRunPasses(
        M: LLVMModuleRef,
        Passes: *const ::libc::c_char,
        TM: LLVMTargetMachineRef,
        Options: LLVMPassBuilderOptionsRef,
    ) -> LLVMErrorRef;
    /// Construct and run a set of passes over a function.
    ///
    /// This function behaves the same as LLVMRunPasses, but operates on a single
    /// function instead of an entire module.
    pub fn LLVMRunPassesOnFunction(
        F: LLVMValueRef,
        Passes: *const ::libc::c_char,
        TM: LLVMTargetMachineRef,
        Options: LLVMPassBuilderOptionsRef,
    ) -> LLVMErrorRef;
    pub fn LLVMCreatePassBuilderOptions() -> LLVMPassBuilderOptionsRef;
    pub fn LLVMPassBuilderOptionsSetVerifyEach(
        Options: LLVMPassBuilderOptionsRef,
        VerifyEach: LLVMBool,
    );
    pub fn LLVMPassBuilderOptionsSetDebugLogging(
        Options: LLVMPassBuilderOptionsRef,
        DebugLogging: LLVMBool,
    );
    pub fn LLVMPassBuilderOptionsSetAAPipeline(
        Options: LLVMPassBuilderOptionsRef,
        AAPipeline: *const ::libc::c_char,
    );
    pub fn LLVMPassBuilderOptionsSetLoopInterleaving(
        Options: LLVMPassBuilderOptionsRef,
        LoopInterleaving: LLVMBool,
    );
    pub fn LLVMPassBuilderOptionsSetLoopVectorization(
        Options: LLVMPassBuilderOptionsRef,
        LoopVectorization: LLVMBool,
    );
    pub fn LLVMPassBuilderOptionsSetSLPVectorization(
        Options: LLVMPassBuilderOptionsRef,
        SLPVectorization: LLVMBool,
    );
    pub fn LLVMPassBuilderOptionsSetLoopUnrolling(
        Options: LLVMPassBuilderOptionsRef,
        LoopUnrolling: LLVMBool,
    );
    pub fn LLVMPassBuilderOptionsSetForgetAllSCEVInLoopUnroll(
        Options: LLVMPassBuilderOptionsRef,
        ForgetAllSCEVInLoopUnroll: LLVMBool,
    );
    pub fn LLVMPassBuilderOptionsSetLicmMssaOptCap(
        Options: LLVMPassBuilderOptionsRef,
        LicmMssaOptCap: ::libc::c_uint,
    );
    pub fn LLVMPassBuilderOptionsSetLicmMssaNoAccForPromotionCap(
        Options: LLVMPassBuilderOptionsRef,
        LicmMssaNoAccForPromotionCap: ::libc::c_uint,
    );
    pub fn LLVMPassBuilderOptionsSetCallGraphProfile(
        Options: LLVMPassBuilderOptionsRef,
        CallGraphProfile: LLVMBool,
    );
    pub fn LLVMPassBuilderOptionsSetMergeFunctions(
        Options: LLVMPassBuilderOptionsRef,
        MergeFunctions: LLVMBool,
    );
    pub fn LLVMPassBuilderOptionsSetInlinerThreshold(
        Options: LLVMPassBuilderOptionsRef,
        Threshold: ::libc::c_int,
    );
    pub fn LLVMDisposePassBuilderOptions(Options: LLVMPassBuilderOptionsRef);
}

/// blaze: LLVM's pass builder, analysis managers, instrumentation and parsed pipelines kept for
/// one target machine (wrappers/passes.cpp). Used by one thread at a time, with modules of one
/// context, and disposed of before the target machine and the context.
#[derive(Debug)]
pub enum LLVMBlazeOpaquePasses {}
pub type LLVMBlazePassesRef = *mut LLVMBlazeOpaquePasses;

extern "C" {
    pub fn LLVMBlazeCreatePasses(TM: LLVMTargetMachineRef) -> LLVMBlazePassesRef;
    pub fn LLVMBlazeDisposePasses(Passes: LLVMBlazePassesRef);
    /// Runs the pipeline `Pipeline` (`Length` bytes, no NUL needed) over `M`, as
    /// [`LLVMRunPasses`] does with default options but `SLPVectorization`.
    pub fn LLVMBlazeRunPasses(
        Passes: LLVMBlazePassesRef,
        M: LLVMModuleRef,
        Pipeline: *const ::libc::c_char,
        Length: usize,
        SLPVectorization: LLVMBool,
    ) -> LLVMErrorRef;
    /// Instruments `M` for `Wanted`, which is read during the call only: SanitizerCoverage,
    /// then MemorySanitizer, ThreadSanitizer and AddressSanitizer, as clang orders them and with
    /// rustc's options for each. `M` is of the context the passes were first run with.
    pub fn LLVMBlazeRunSanitizers(
        Passes: LLVMBlazePassesRef,
        M: LLVMModuleRef,
        Wanted: *const LLVMBlazeSanitizers,
    ) -> LLVMErrorRef;
}

/// blaze: what [`LLVMBlazeRunSanitizers`] instruments for (wrappers/passes.cpp's struct of
/// the same name, field for field). A 0 is false.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct LLVMBlazeSanitizers {
    pub Address: u8,
    pub AddressRecover: u8,
    pub Thread: u8,
    pub Memory: u8,
    pub MemoryRecover: u8,
    /// `-sanitizer-coverage-level`, 1 to 4; any other is no SanitizerCoverage.
    pub CoverageLevel: u8,
    /// The `LLVMBlazeCoverage*` bits.
    pub CoverageFlags: u16,
    /// 0, 1 or 2.
    pub MemoryTrackOrigins: i32,
}

/// The `-sanitizer-coverage-*` switches of [`LLVMBlazeSanitizers::CoverageFlags`].
pub const LLVMBlazeCoverageTracePC: u16 = 1 << 0;
pub const LLVMBlazeCoverageTracePCGuard: u16 = 1 << 1;
pub const LLVMBlazeCoverageInline8bitCounters: u16 = 1 << 2;
pub const LLVMBlazeCoverageInlineBoolFlag: u16 = 1 << 3;
pub const LLVMBlazeCoveragePCTable: u16 = 1 << 4;
pub const LLVMBlazeCoverageTraceCmp: u16 = 1 << 5;
pub const LLVMBlazeCoverageTraceDiv: u16 = 1 << 6;
pub const LLVMBlazeCoverageTraceGep: u16 = 1 << 7;
pub const LLVMBlazeCoverageTraceLoads: u16 = 1 << 8;
pub const LLVMBlazeCoverageTraceStores: u16 = 1 << 9;
pub const LLVMBlazeCoverageNoPrune: u16 = 1 << 10;
pub const LLVMBlazeCoverageStackDepth: u16 = 1 << 11;
pub const LLVMBlazeCoverageTracePCEntryExit: u16 = 1 << 12;
pub const LLVMBlazeCoverageControlFlow: u16 = 1 << 13;

const _: () = assert!(std::mem::size_of::<LLVMBlazeSanitizers>() == 12);

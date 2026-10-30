/* blaze: LLVM's new pass manager kept per thread and target machine.
 *
 * LLVMRunPasses builds, on every call, a PassBuilder, four analysis managers with every
 * analysis registered in them, the standard instrumentation (whose timer groups are looked up
 * under a process-wide lock) and the pass pipeline parsed from its text. blaze runs passes
 * over one small module per function, so that was paid once per function, on every core at
 * once. Here it is built once per target machine and context, and a module's run costs what
 * its passes do. It runs the passes LLVMRunPasses would, with the same tuning options and
 * instrumentation.
 *
 * An object is used by one thread at a time, and outlives no target machine or context it was
 * used with: blaze keeps one beside each target machine of a thread, and drops it first.
 *
 * LLVMBlazeRunSanitizers runs the sanitizers' instrumentation with the same analysis
 * managers, as rustc runs it at the end of its pipeline.
 */
#include <llvm-c/Error.h>
#include <llvm-c/TargetMachine.h>
#include <llvm-c/Types.h>
#include <llvm/ADT/StringMap.h>
#include <llvm/ADT/StringRef.h>
#include <llvm/IR/LLVMContext.h>
#include <llvm/IR/Module.h>
#include <llvm/IR/PassManager.h>
#include <llvm/Passes/PassBuilder.h>
#include <llvm/Passes/StandardInstrumentations.h>
#include <llvm/Support/Error.h>
#include <llvm/Target/TargetMachine.h>
#include <llvm/Transforms/Instrumentation/AddressSanitizer.h>
#include <llvm/Transforms/Instrumentation/MemorySanitizer.h>
#include <llvm/Transforms/Instrumentation/SanitizerCoverage.h>
#include <llvm/Transforms/Instrumentation/ThreadSanitizer.h>
#include <llvm/Transforms/Utils/Instrumentation.h>

#include <cstdint>
#include <memory>
#include <optional>

using namespace llvm;

namespace {

PipelineTuningOptions tuning(bool SLPVectorization) {
  PipelineTuningOptions Options;
  Options.SLPVectorization = SLPVectorization;
  return Options;
}

/* What LLVMRunPasses makes for one call, made once for one context. Members are built in this
 * order and destroyed in the reverse, as LLVMRunPasses's locals are. */
struct Pipelines {
  Pipelines(TargetMachine *Machine, bool SLPVectorization, LLVMContext &Context)
      : Context(Context), Tuning(tuning(SLPVectorization)),
        Builder(Machine, Tuning, std::nullopt, &Callbacks),
        Instrumentation(Context, /*DebugLogging=*/false, /*VerifyEach=*/false) {
    Builder.registerLoopAnalyses(Loops);
    Builder.registerFunctionAnalyses(Functions);
    Builder.registerCGSCCAnalyses(Components);
    Builder.registerModuleAnalyses(Modules);
    Builder.crossRegisterProxies(Loops, Functions, Components, Modules);
    Instrumentation.registerCallbacks(Callbacks, &Modules);
  }

  /* Forgets every analysis of the module that ran: the next is another module. The analyses
   * registered stay. */
  void forget() {
    Modules.clear();
    Components.clear();
    Functions.clear();
    Loops.clear();
  }

  LLVMContext &Context;
  PipelineTuningOptions Tuning;
  PassInstrumentationCallbacks Callbacks;
  PassBuilder Builder;
  LoopAnalysisManager Loops;
  FunctionAnalysisManager Functions;
  CGSCCAnalysisManager Components;
  ModuleAnalysisManager Modules;
  StandardInstrumentations Instrumentation;
  /* Each pipeline text as parsed, for the pipelines a run leaves as it found them. */
  StringMap<std::unique_ptr<ModulePassManager>> Parsed;
};

/* The CGSCC inliner run on its own, not inside the module inliner the default pipelines wrap
 * it in, makes an inline advisor of the first module it runs on and keeps it
 * (InlinerPass::OwnedAdvisor): a pipeline that names it is parsed again for each module, so
 * that no module is optimized by what another left. */
bool reusable(StringRef Text) { return !Text.contains("inline"); }

} // namespace

struct LLVMBlazeOpaquePasses {
  TargetMachine *Machine;
  std::unique_ptr<Pipelines> Vectorizing;
  std::unique_ptr<Pipelines> Plain;
};

/* What LLVMBlazeRunSanitizers instruments for: rustc's -Zsanitizer, -Zsanitizer-recover and
 * -Zsanitizer-memory-track-origins, and SanitizerCoverage as -Cllvm-args=-sanitizer-coverage-*
 * sets it. Declared field for field in src/transforms/pass_builder.rs; a 0 is false. */
struct LLVMBlazeSanitizers {
  uint8_t Address;
  uint8_t AddressRecover;
  uint8_t Thread;
  uint8_t Memory;
  uint8_t MemoryRecover;
  /* -sanitizer-coverage-level: 1 the entry block, 2 every block, 3 and critical edges, 4 and
   * indirect calls. Any other: no SanitizerCoverage, as LLVM takes it. */
  uint8_t CoverageLevel;
  /* The LLVMBlazeCoverage* bits. */
  uint16_t CoverageFlags;
  /* 0, 1 or 2. */
  int32_t MemoryTrackOrigins;
};

/* SanitizerCoverage's -sanitizer-coverage-* switches, one bit each. The same values are
 * blaze_codegen::settings::Coverage's. */
enum : uint16_t {
  LLVMBlazeCoverageTracePC = 1 << 0,
  LLVMBlazeCoverageTracePCGuard = 1 << 1,
  LLVMBlazeCoverageInline8bitCounters = 1 << 2,
  LLVMBlazeCoverageInlineBoolFlag = 1 << 3,
  LLVMBlazeCoveragePCTable = 1 << 4,
  LLVMBlazeCoverageTraceCmp = 1 << 5,
  LLVMBlazeCoverageTraceDiv = 1 << 6,
  LLVMBlazeCoverageTraceGep = 1 << 7,
  LLVMBlazeCoverageTraceLoads = 1 << 8,
  LLVMBlazeCoverageTraceStores = 1 << 9,
  LLVMBlazeCoverageNoPrune = 1 << 10,
  LLVMBlazeCoverageStackDepth = 1 << 11,
  LLVMBlazeCoverageTracePCEntryExit = 1 << 12,
  LLVMBlazeCoverageControlFlow = 1 << 13,
};

namespace {

/* SanitizerCoverage's options as its -sanitizer-coverage-* switches give them
 * (SanitizerCoverage.cpp, getOptions and OverrideFromCL): here per module rather than per
 * process, which a build of units with other settings needs. The pass still ORs in the
 * process's switches, which blaze leaves unset. */
SanitizerCoverageOptions coverage(const LLVMBlazeSanitizers &Wanted) {
  SanitizerCoverageOptions Options;
  switch (Wanted.CoverageLevel) {
  case 1:
    Options.CoverageType = SanitizerCoverageOptions::SCK_Function;
    break;
  case 2:
    Options.CoverageType = SanitizerCoverageOptions::SCK_BB;
    break;
  case 3:
    Options.CoverageType = SanitizerCoverageOptions::SCK_Edge;
    break;
  case 4:
    Options.CoverageType = SanitizerCoverageOptions::SCK_Edge;
    Options.IndirectCalls = true;
    break;
  default:
    break;
  }
  uint16_t Flags = Wanted.CoverageFlags;
  Options.TracePC = Flags & LLVMBlazeCoverageTracePC;
  Options.TracePCGuard = Flags & LLVMBlazeCoverageTracePCGuard;
  Options.Inline8bitCounters = Flags & LLVMBlazeCoverageInline8bitCounters;
  Options.InlineBoolFlag = Flags & LLVMBlazeCoverageInlineBoolFlag;
  Options.PCTable = Flags & LLVMBlazeCoveragePCTable;
  Options.TraceCmp = Flags & LLVMBlazeCoverageTraceCmp;
  Options.TraceDiv = Flags & LLVMBlazeCoverageTraceDiv;
  Options.TraceGep = Flags & LLVMBlazeCoverageTraceGep;
  Options.TraceLoads = Flags & LLVMBlazeCoverageTraceLoads;
  Options.TraceStores = Flags & LLVMBlazeCoverageTraceStores;
  Options.NoPrune = Flags & LLVMBlazeCoverageNoPrune;
  Options.StackDepth = Flags & LLVMBlazeCoverageStackDepth;
  Options.TracePCEntryExit = Flags & LLVMBlazeCoverageTracePCEntryExit;
  Options.CollectControlFlow = Flags & LLVMBlazeCoverageControlFlow;
  return Options;
}

bool covers(const LLVMBlazeSanitizers &Wanted) {
  return Wanted.CoverageLevel >= 1 && Wanted.CoverageLevel <= 4;
}

} // namespace

extern "C" {

LLVMBlazeOpaquePasses *LLVMBlazeCreatePasses(LLVMTargetMachineRef Machine) {
  return new LLVMBlazeOpaquePasses{reinterpret_cast<TargetMachine *>(Machine), nullptr,
                                   nullptr};
}

void LLVMBlazeDisposePasses(LLVMBlazeOpaquePasses *Passes) { delete Passes; }

/* Runs the pipeline `Text` (`Length` bytes) over `M`, as LLVMRunPasses does with options whose
 * only change is SLP vectorization. Every module given to one object is of one context. */
LLVMErrorRef LLVMBlazeRunPasses(LLVMBlazeOpaquePasses *Passes, LLVMModuleRef M,
                                const char *Text, size_t Length,
                                LLVMBool SLPVectorization) {
  Module &Mod = *unwrap(M);
  std::unique_ptr<Pipelines> &Slot = SLPVectorization ? Passes->Vectorizing : Passes->Plain;
  if (!Slot) {
    Slot = std::make_unique<Pipelines>(Passes->Machine, SLPVectorization != 0,
                                       Mod.getContext());
  } else if (&Slot->Context != &Mod.getContext()) {
    return wrap(make_error<StringError>(
        "blaze: passes kept for one LLVM context were given a module of another",
        inconvertibleErrorCode()));
  }
  Pipelines &Kept = *Slot;
  StringRef Pipeline(Text, Length);
  std::unique_ptr<ModulePassManager> Once;
  ModulePassManager *Manager = nullptr;
  if (reusable(Pipeline)) {
    auto Found = Kept.Parsed.find(Pipeline);
    if (Found != Kept.Parsed.end())
      Manager = Found->second.get();
  }
  if (!Manager) {
    auto Parsed = std::make_unique<ModulePassManager>();
    if (Error Err = Kept.Builder.parsePassPipeline(*Parsed, Pipeline))
      return wrap(std::move(Err));
    Manager = Parsed.get();
    if (reusable(Pipeline))
      Kept.Parsed.try_emplace(Pipeline, std::move(Parsed));
    else
      Once = std::move(Parsed);
  }
  Manager->run(Mod, Kept.Modules);
  Kept.forget();
  return nullptr;
}

/* Instruments `M` for `Wanted` (not null): SanitizerCoverage, then MemorySanitizer,
 * ThreadSanitizer and AddressSanitizer, in clang's order (BackendUtil.cpp, addSanitizers), so
 * that coverage traces none of the sanitizers' own checks; each with the options rustc's
 * LLVMRustOptimize gives it. Only functions with the attribute `sanitize_address`,
 * `sanitize_thread` or `sanitize_memory` get that sanitizer's checks. Every module given to
 * one object is of one context, as for LLVMBlazeRunPasses. */
LLVMErrorRef LLVMBlazeRunSanitizers(LLVMBlazeOpaquePasses *Passes, LLVMModuleRef M,
                                    const LLVMBlazeSanitizers *Wanted) {
  Module &Mod = *unwrap(M);
  std::unique_ptr<Pipelines> &Slot = Passes->Plain;
  if (!Slot) {
    Slot = std::make_unique<Pipelines>(Passes->Machine, false, Mod.getContext());
  } else if (&Slot->Context != &Mod.getContext()) {
    return wrap(make_error<StringError>(
        "blaze: passes kept for one LLVM context were given a module of another",
        inconvertibleErrorCode()));
  }
  ModulePassManager Manager;
  if (covers(*Wanted)) {
    Manager.addPass(SanitizerCoveragePass(coverage(*Wanted)));
  }
  if (Wanted->Memory) {
    MemorySanitizerOptions Options(Wanted->MemoryTrackOrigins, Wanted->MemoryRecover != 0,
                                   /*Kernel=*/false, /*EagerChecks=*/true);
    Manager.addPass(MemorySanitizerPass(Options));
  }
  if (Wanted->Thread) {
    Manager.addPass(ModuleThreadSanitizerPass());
    Manager.addPass(createModuleToFunctionPassAdaptor(ThreadSanitizerPass()));
  }
  if (Wanted->Address) {
    AddressSanitizerOptions Options;
    Options.CompileKernel = false;
    Options.Recover = Wanted->AddressRecover != 0;
    Options.UseAfterScope = true;
    Options.UseAfterReturn = AsanDetectStackUseAfterReturnMode::Runtime;
    /* Windows has no ODR indicators (https://reviews.llvm.org/D137227), as rustc says. */
    bool Odr = !Passes->Machine->getTargetTriple().isOSWindows();
    Manager.addPass(AddressSanitizerPass(Options, /*UseGlobalGC=*/true, Odr));
  }
  Manager.run(Mod, Slot->Modules);
  Slot->forget();
  return nullptr;
}

} // extern "C"

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

} // extern "C"

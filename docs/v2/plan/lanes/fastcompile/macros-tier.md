---
status: plan
summary: blaze v2 lane fastcompile, change 20's second tier (crates/blaze-macro-eval/src/tier.rs and src/tier/): a proc-macro crate compiled by copy-and-patch from hir-ty-codegen's Cranelift IR and run in blaze's process, isolated as a wasm engine isolates a module. The memory model (4 GiB-aligned region, offsets as addresses, guard pages, shadow stack), the call table, the confining IR pass, externals bound to shims or refused at compile time, how the bridge's buffers cross, unwinding and faults, the policy, the cost per access, the speed of light, and the requests.
date: 2026-10-29
---
# fastcompile: the compiled tier of proc macros

Change 20 of [compile-time.md](../../design/compile-time.md) runs proc macros on blaze's own
compiler. The first tier is the MIR evaluator (`crates/blaze-macro-eval`, lane
`fastcompile-macros`). This is the second: the macro crate's code compiled by copy-and-patch
([stencil.md](stencil.md)) from the same MIR, through the same Cranelift IR the IR builder
makes for every dev build, and run in blaze's process. Isolation comes from the code: memory
is bounded, indirect calls are checked, and every external symbol is a shim or a refusal at
compile time.

Owned here: `crates/blaze-macro-eval/src/tier.rs`, `src/tier/`, this file.

## Memory

**One region per running expansion.** It is a reservation of 4 GiB aligned to 4 GiB, followed
by a 64 KiB guard (`tier/region.rs`). A guest address is an offset into it. Every load and
store of the compiled code goes to

```text
base + ((address + offset) mod 2^32)
```

where `base` is the region's address and `offset` the instruction's immediate. Whatever 64-bit
value the macro computes as an address, the access lands inside the reservation. The widest
access is 16 bytes, so one at offset `2^32 - 1` ends in the tail guard. The parts the macro
may use are mapped; everything else is `PROT_NONE` and faults:

| offsets | part | protection |
|---|---|---|
| `[0, 64K)` | null guard | none |
| `[64K, 128K)` | header: shadow stack pointer and limit, `errno`, a zero cell | rw |
| `[128K, ..)` | image: statics and thread-locals, then constants | rw, then r |
| 64 KiB | guard below the shadow stack | none |
| stack bound | shadow stack, growing down | rw |
| heap bound | heap, mapped as it grows | rw |
| up to `2^32` | never mapped | none |
| `[2^32, 2^32 + 64K)` | tail guard | none |

64 KiB is a multiple of every page size blaze runs on (4 KiB, Apple's 16 KiB, 64 KiB arm64
kernels) and Windows' allocation granularity, so every part has its own protection.

**Why offsets with wrap, not bounds checks.** This is the wasm32 engines' model: Wasmtime and
V8 reserve 4 GiB plus guards and never check a 32-bit index. Here pointers are 64-bit, since
the macro is compiled for the host target so that layouts are the evaluator's and rustc's, so
the one extra operation per access is the truncation to 32 bits. On x86-64 a 32-bit `lea`
computes `address + offset`, wraps it and zero-extends it in one instruction. On AArch64 a
32-bit `add` then `ldr x, [base, w, uxtw]` does the same, and with a zero offset the
`uxtw` addressing mode alone suffices. An explicit check would cost a compare and a branch per
access, plus the size of the access.

Offsets rather than host addresses also give three properties for free:

- **Shared code.** Every region holds the same image at the same offsets, so the compiled
  code, its relocations and its constants are the same for every region. One copy per macro
  crate serves every thread.
- **Deterministic addresses.** The same expansion sees the same addresses in every run, so a
  macro that hashes or prints a pointer is as reproducible as one that doesn't. The evaluator
  has the same property.
- **No base in the data.** A pointer stored in memory is an offset, so the image is copied
  in, never relocated.

**`base` is a parameter.** Each function of the macro takes `vmctx` (the region's base) as a
last parameter with `ArgumentPurpose::VMContext`, and every call it makes to a function of the
macro passes it on. This is wasm's `vmctx`. There is no pinned register (stencil code clobbers
every register) and no thread-local load per function, and stencil code keeps the base in one
slot.

**The shadow stack.** The machine's stack holds spill slots, saved registers and return
addresses. It is never in the region, so no guest address names it, and a macro can't
overwrite a return address. That is the reason the native stack lives outside wasm's linear
memory too. The locals whose address Rust takes are Cranelift stack slots in the IR builder's
output. The confining pass moves every one into a shadow frame in the region:

- at entry: `sp = load [vmctx + SHADOW_SP]`, `frame = (sp - size) & -align`; trap
  (`stack_overflow`) if `frame < load [vmctx + SHADOW_LIMIT]`; then `store frame`;
- `stack_addr slot, k` becomes `frame + slot_offset + k`;
- `sp` is stored back before each return and at the head of each landing pad.

LLVM's wasm target and Emscripten do the same with `__stack_pointer`. The cells are
guest-writable: a macro that corrupts them only breaks itself, and every access stays
confined. Overflow of the machine stack hits the guard page of the expansion's own machine
stack (see "Running").

**The heap** (`tier/heap.rs`) backs `__rust_alloc` and the rest of Rust's allocator API, and
C's `malloc` family. Its bookkeeping is host memory, never the region's. Small blocks are
powers of two from 16 bytes to 32 KiB, carved from 64 KiB chunks per class, each class with a
LIFO free list. Larger blocks are whole granules, reused by count. A call costs a shift, a pop
and a compare. The heap's bound is `Bounds::heap`, and a request past it stops the expansion
with an error naming the bound.

**Between expansions** the writable image is copied back (statics are a few KiB) and the
cells are reset. The heap forgets its blocks but keeps up to 64 MiB mapped and warm. Bytes
the heap hands out again are dirty; `alloc_zeroed` and `calloc` zero them. A macro without UB
never reads memory it didn't write, so this is unobservable. The statics a run changed are a
`memcmp` of the writable image against the installed one. That gives the parent's state
verdict (stateless, settles, counts, holds) as `changed_statics` does on the evaluator. A word
that is an address falls in the region's stack or heap range, which is how "holds addresses"
is told.

**What isolation is and isn't.** The evaluator checks every access and stops at UB. This tier
confines every access: a macro with UB stays inside its region and gets a wrong result or a
fault, as it would natively, but it can never reach blaze's memory. That is the wasm
guarantee. The policy runs each crate's first expansion in the evaluator, where the access
checks still apply. The differential corpus compares both tiers on the same expansions.

## Calls

**Direct calls** to the macro's own functions keep their relocation to the callee's code. The
target is fixed when the code is compiled, so it is safe as it is.

**Indirect calls** go through the image's table (`tier/table.rs`). A function pointer is the
byte offset of its entry, `index * 16`, and each 16-byte entry is `{ code: u64, sig: u32 }`.
Index 0 is null and matches no signature. The pass rewrites each `call_indirect sig, p, args`
as:

```text
if p & 15 != 0 || p >= TABLE_SIZE            trap bad_fn_ptr       (and + cmp + branch)
e = TABLE + p
if load.i32 [e + 8] != SIG(sig)              trap bad_signature    (load + cmp + branch)
call_indirect sig', load.i64 [e], args, vmctx
```

`TABLE`, `TABLE_SIZE` and `SIG(sig)` are relocations the loader fills (absolute, non-PIC).
`func_addr f` becomes the absolute relocation of `f`'s pointer, and a vtable's or a static's
relocation to a function becomes the pointer's value in the image. The table is the image's,
outside every region: the macro can hold its pointers but can't read or write the table.
Shim stubs whose address the macro takes get entries too. The server checks a pointer the same
way (`Table::target`) before calling into the macro: the bridge buffer's `reserve` and `drop`,
and a client's `run`.

Signatures are interned per image from the confined signature's encoding (parameter and return
types, purposes and extensions, calling convention), so a call through a pointer of another
signature traps and is never called with the wrong arguments.

## What hir-ty-codegen must emit, and the confining pass

Codegen emits nothing new for a sandboxed unit. It gives the tier the same
`ir::Function` the stencil backend would compile, after ABI lowering and `float_pair::lower`,
with its references (request `fastcompile-macros-tier 1`). The tier rewrites it (the pass
`confine`, `src/tier/confine.rs`), compiles it with `blaze_stencil::compile`, and loads it.
This keeps the security-critical rewrite in one place, owned and tested here, and costs
codegen nothing when unused. The rewrite, per instruction:

| IR | becomes |
|---|---|
| every function's signature | `vmctx` appended (`ArgumentPurpose::VMContext`); each call to a function of the macro passes it |
| `load`/`store`/`uload*`/`sload*`/`istore*`, vector loads, `atomic_load`/`atomic_store`/`atomic_rmw`/`atomic_cas` at `p, off` | at `iadd vmctx, uextend.i64 (ireduce.i32 (iadd_imm p, off))`, offset 0 |
| `stack_addr`, `stack_load`, `stack_store`, stack slots | the shadow frame (see Memory) |
| `symbol_value`/`global_value` of a static, `tls_value` | the guest offset of the static or of its place in the image's thread-local block, an absolute relocation (non-colocated, non-PIC) |
| `func_addr` | the function's table pointer, an absolute relocation |
| `call_indirect`, `return_call_indirect`, `try_call_indirect` | the checked sequence above |
| `call` of an external | its binding (below): a direct call of a shim stub, or a refusal |
| `get_frame_pointer`, `get_stack_pointer`, `get_return_address`, `stack_switch`, `dynamic_stack_*`, a `StructArgument` parameter or argument | refused, naming the function: each one gives the macro an address of the machine's stack |
| an assembly unit (`Built::Asm`), `asm!`, `global_asm!`, `naked_asm!` | refused, naming the function |

The units are compiled with `pic = false` and `probe_stack = true`. Overflow checks, debug
assertions and `cfg` are the macro crate's own settings (`Spec`), the same ones the
evaluator's MIR was built with. Panics, overflow checks and drops are therefore the same MIR
in both tiers, lowered once by the IR builder's rules.

## Externals

`tier/bind.rs` binds each name outside the macro's code when the crate is compiled, in this
order:

1. **The tier's own shims:** Rust's allocator, C's allocator, `memcpy`/`memmove`/`memset`/
   `memcmp`/`bcmp`/`memchr`/`memrchr`/`strlen` (each range checked against the region, then
   the host's SIMD routine), `__errno_location`/`__error`/`_errno` (the header's cell), the
   panic runtime (`__rust_start_panic`, `__rust_panic_cleanup`, `_Unwind_Resume`), and
   `abort`. The allocator's data symbols read the zero cell.
2. **Pure scalar functions** of libm and compiler-rt (registers in, registers out, no memory),
   which are the host's own. Those that write through a pointer (`frexp`, `modf`, `lgamma_r`,
   `sincos`) or a global (`lgamma`) are not on the list.
3. **The run's shims:** the same claims as the evaluator's run. These are `os.rs`'s file,
   environment, clock and random shims and the libc `syscall` they trace, by symbol, and the
   bridge's `Closure::call`, by `FunctionId`.
4. Anything else is a `Refusal` naming the symbol, raised when the crate is compiled and never
   at run time: an unknown function or datum, threads, `dlopen`, and raw `syscall`
   instructions (which only come from assembly).

## The bridge's buffers

The client's `Buffer` (`data`, `len`, `capacity`, `reserve`, `drop`) lives in the region, made
by the client's own `Buffer::new` in the macro's heap, as on the evaluator. A call of the
claimed `Closure::call` reaches the bridge shim with the closure's environment and the buffer
by value (offsets and table pointers). The server reads the request in place (`Region::read`:
one range check, then a borrowed `&[u8]`, zero-copy) and writes its reply in place. When the
reply doesn't fit, it calls the buffer's own `reserve` through `Table::target` and a nested
entry into the macro, exactly as the evaluator's server does through `call_pointer`. Token
streams, spans and symbols stay handles on the server's side. No byte is copied twice, and
nothing crosses but the buffer's bytes.

## Running

These parts wait on the compiled code (requests 1 and 2); none is written yet:

- **The machine stack**: each expansion thread has its own machine stack (8 MiB, guard page
  below), outside every region. The entry trampoline saves the host's callee-saved registers
  and stack pointer, switches to it, and calls the client's `run` with `vmctx`.
- **Shim stubs**: a call from the macro to a shim switches back to the host stack and calls
  the Rust shim with `&mut Memory`. The macro's stack pointer is kept for a nested entry,
  which continues below it.
- **Unwinding**: `__rust_start_panic` (claimed) walks the macro's frames by the frame-pointer
  chain. Every stencil function keeps one. Each frame pointer read is checked to lie inside the
  machine stack and to rise. The walk finds each return address's call site and handler in
  the image's call-site table (stencil's `Code::calls`/`handlers`) and resumes at the landing
  pad with the payload, through a resume trampoline. Cleanup pads end in `_Unwind_Resume`
  (claimed), which goes on. `catch_unwind`'s landing pad gets the payload through
  `__rust_panic_cleanup`, as on the evaluator. No libunwind, no DWARF, no personality routine
  runs. The panic message, the hook and the payload are std's own code, compiled.
- **Faults**: a fault (SIGSEGV/SIGBUS, an EXCEPTION_ACCESS_VIOLATION, or a trap instruction)
  whose PC is in an image's code is turned into the expansion's error. The error carries the
  trap code (stencil's `Code::traps`) and the macro's frame by its line table. The machine
  stack is unwound to the entry trampoline. A fault anywhere else is blaze's, and goes to the
  default handler.

## Which tier

`choose(code, runs)` returns `Tier::Compiled` once the crate has run `Policy::warm` times, in
a blaze that runs the compiled tier (`Policy::compiled`). Until the confining pass, loader and
runner are in, `compiled` is false and every expansion is evaluated; `tier::run`, which
`run.rs` calls for `Tier::Compiled`, stops naming what it lacks. The default `warm` is 1. This is the ski-rental rule stencil's `Tiering` uses: compile when
the evaluator's extra cost has reached the compile's. One serde-sized expansion on the
evaluator (50 to 500 ms) already costs more than compiling the crate (about 8,000 functions
at 15 µs each, 120 ms of CPU across the workers, once per macro version, kept in the store
and shared through the global cache). A small macro's compile is a few milliseconds, so
compiling it after one run wastes at most that. The decision depends on the run count only,
never on a failure. One expansion runs wholly in one tier. A crate the policy picks that
can't be compiled is the expansion's error, naming the macro and what it met (a `Refusal`),
and it never quietly stays in the evaluator.

## Cost per access

| | evaluator | compiled, unfused | compiled, fused (request 2) | native |
|---|---|---|---|---|
| load/store | 20 to 50 ns: decode the address's tag, find its allocation, bounds check, copy through an interval | 4 extra ops (`iadd_imm`, `ireduce`, `uextend`, `iadd`), about 1 to 1.5 ns on top of the access, chained through the accumulator | 1 extra instruction (`lea r32` or 32-bit `add`), about 0.25 ns | 0.25 ns (L1 hit) |
| indirect call | 100 to 300 ns: a `vtable_map` lookup, a new frame | 2 compares, 2 loads, 1 call: about 2 ns | same | 1 ns |
| call into a shim | the evaluator's claimed call: about 200 ns | stack switch, about 5 to 10 ns | same | 1 ns |

## Speed of light

**The work.** `#[derive(Serialize)]` on a struct of 20 named fields. syn parses about 100
input tokens into a `DeriveInput`; serde_derive builds the impl; quote emits about 800 tokens
(20 `serialize_field` calls of about 15 tokens each, plus the impl's frame). Through
proc_macro2, every token built or iterated is a bridge RPC: about 2,500 per expansion.

**Natively** (serde_derive built in release, run by rustc): about 150 µs of the macro's own
code and about 2,500 RPCs at 100 to 150 ns of server work each. That is 0.4 to 0.55 ms.

**At the tier's bound** on one core:

- the macro's own code at stencil quality, which is about Cranelift `opt_level=none`: 5 to 8
  times release for syn's iterator-heavy code, so 0.75 to 1.2 ms;
- confinement at one instruction per access, fused: memory operations are about 35% of
  unoptimized code, so 5 to 10% more, 0.8 to 1.3 ms;
- the RPCs: the same server work plus a 5 to 10 ns crossing each, 0.27 to 0.4 ms.

That is **1.1 to 1.7 ms per expansion**. The tier must come within 10% of that. Unfused, the
confinement costs 4 operations per access, 25 to 40% more, which is why request 2 exists.

**On the evaluator today** the same work is about 1.5 to 2 million MIR statements, at 100 to
300 ns each (place projection, interval copies, the memory map), plus about 100,000 calls at 1
to 2 µs each (body fetch, locals). That is **0.3 to 0.8 s per expansion**, 200 to 500 times
the bound. These figures are estimates from the work's size; `fastcompile-macros-corpus`
measures both tiers on the same expansions, and this section takes its numbers when it has
them.

**Compile cost.** About 8,000 reached functions, each about 10 µs to build IR, 3 µs to copy
and patch, 1 µs to confine and 0.5 µs to load: 120 ms of CPU, 15 ms on 8 workers. It is paid
once per macro version: the image is stored by code hash and shared through the global cache.

## State of the art

- **Wasmtime and V8 (wasm32)**: a 4 GiB reservation plus guards, no bounds checks, `vmctx`
  as a parameter. Matched here, with the native stack outside the region as theirs is.
- **Segue (Narayan et al., "Segue & ColorGuard")**: on x86-64, a segment register holds the base and a 32-bit address
  size wraps the offset, which costs nothing per access. On x86-64 Linux it is the next step
  past the fused stencil (`wrgsbase`, `gs:` loads). macOS gives no free segment register.
- **NaCl and RLBox/wasm2c**: masking with bundles, or SFI through wasm. Here the masking is
  the 32-bit wrap, which is free on AArch64 (`uxtw`) and one `lea` on x86-64.
- **Lucet and wasm2c's shadow stack**: the same split between the native stack and the
  address-taken locals.

## Tests

- `tier/region/tests.rs`: the layout's order, alignment and guards. A real reservation is
  aligned to 4 GiB. Null, the guards, unmapped space and wrapping ranges are refused;
  constants are read-only. Reset restores statics and cells, and the heap maps as it grows and
  reads zeros after a shrink. A reference model (one vector per part) is checked against the
  region over random writes, fills and moves.
- `tier/heap/tests.rs`: blocks are aligned, inside the heap and disjoint over random traffic.
  Freed blocks are reused LIFO, the same calls give the same addresses, zeroed blocks are zero
  after reuse, `realloc` keeps bytes, the bound is held, and what was never given is refused.
  `malloc` sizes are kept, and a warm expansion allocates no host memory (`blaze-alloc-count`).
- `tier/memory.rs`: the parent's `Guest` over the compiled tier's memory, and reset.
- `tier/table.rs`: pointers are entry offsets. Null, misaligned, out-of-range and
  wrong-signature calls are refused, and the entry's layout is what the confined code reads.
- `tier/bind.rs`: the binding order, scalar math against pointer math, and a refusal naming
  the symbol.
- `tier/tests.rs`: the policy.
- Once requests 1 and 2 land: the confining pass against a reference interpreter
  (cranelift-interpreter over the original IR, with a bounds-checked memory, on the same
  inputs), and the differential corpus of `fastcompile-macros-corpus` run in both tiers, which
  must give equal token streams, diagnostics and panics.

## Requests

In `v2/requests/fastcompile.md`:

1. `fastcompile-macros-tier 1` (codegen): the IR of an instance for a sandboxed unit.
2. `fastcompile-macros-tier 2` (fastcompile-stencil): the fused confined access, and the
   frame facts the unwinder needs.
3. `fastcompile-macros-tier 3` (fastcompile-macros): the crate's dependencies and its module
   declarations.

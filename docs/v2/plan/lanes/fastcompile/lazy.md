---
status: plan
summary: blaze v2 lane fastcompile, idea 1 (lazy compilation, per function): crates/blaze-lazy, its stubs and resolver per arch, the daemon's single-flight fetcher, what is compiled ahead and why each case is exact, the release refusal, the hooks in inproc, link, codegen and cargo, and the state of the art it matches or beats.
date: 2026-10-29
---
# fastcompile: lazy compilation, per function (`crates/blaze-lazy`)

Idea 1 of [compile-time.md](../../design/compile-time.md). A dev run or test compiles only
the functions that execute. Every other function is a stub that asks the daemon for its code
on its first call, gets it patched in, and jumps. Release builds and `blaze build` outputs
are compiled whole.

## Pieces

| module | side | what |
|---|---|---|
| `mode` | daemon | `allow(&Purpose)`: `Refused` for a release profile (by name, `release` or `bench`, or by opt-level 2, 3, `s` or `z`), for `Mode::Output`, and for a build that writes a file. |
| `plan` | daemon | `Plan`: what is compiled ahead (`Ahead::Entry`, `Ahead::Start`), the lazy functions by dense `LazyId`, lookup by code hash for history, and a function's likely callees. |
| `stub` | daemon | One position-independent region per session: the hook's cell, one shared resolver, and 16-byte entries. Generated as bytes for x86-64 (SysV, Win64) and AArch64 (with or without SVE). |
| `fetch` | daemon | `Fetcher`: a deterministic state machine with single flight, four priorities and failure handling. |
| `runner` | daemon | `Runner`: the fetcher on worker threads, against `Compiler`, `Placer` and `Answers`. |
| `session` | daemon | `Session`: refusal, plan and stub region together, plus what to fetch first. |
| `wire` | both | The frames `Ask`, `Ready` and `Failed`, carried in blaze-inproc's `kinds::LAZY` range. |
| `hook` | run process | `blaze_lazy_resolve(id) -> address`, which the resolver calls. |

The run process links the crate without its `daemon` feature, so it gets only `hook` and
`wire`.

## The stub ABI

An entry is `endbr64; push id; jmp resolver` on x86-64, and `bti c; movz/movk w16, id;
b resolver` on AArch64. The resolver saves every register a call can carry a value in:

- **x86-64**: `rax` (with `al`, the vector count of a variadic call), `rcx`, `rdx`, `rsi`,
  `rdi`, `r8` to `r10`. The vector state goes through `XSAVEC`, `XSAVE` or `FXSAVE`, with the
  mask and size read from `XCR0` and `CPUID` leaf 0xD (`xmm`, `ymm`, `zmm`, AVX-512 masks,
  APX `r16` to `r31`).
- **AArch64**: `x0` to `x15` (with `x8`, the indirect result register), and `q0` to `q31`.
  With SVE it saves `z0` to `z31`, `p0` to `p15` and `FFR` instead.

The resolver then calls the hook on a stack aligned for the C ABI, probing every 2 KiB.
It restores everything and tail-jumps with the caller's stack exactly as it was. A hook that
answers 0 traps (`ud2`, `brk #1`). Only `r11`, `x16` and `x17` are not kept: blaze-link's
slot stubs clobber them already.

## Single flight and failures

`Fetcher` keeps one state byte, one priority and one target per function, and waiters only
for functions someone is asking for. An ask of a published function is answered at once,
with no allocation. An ask of a function being compiled joins its waiters. A resent ask is
deduplicated. The priorities, most urgent first:

1. Asked.
2. A compiled function's callees (from its relocations, calls and tail calls only, not
   addresses taken).
3. History: usercode's `Reached` set.
4. Idle: everything else, when the runner's `idle` is on.

Background compiles leave one worker free for asks.

A compile that fails or panics is caught and answered `Failed`, naming the instance and the
item. The run is stopped once, through `Answers::stop`. A failure nobody called stops nothing,
and `Runner::close` reports it. In the run process, a `Failed` answer, a broken channel or an
answer to another ask writes the reason to stderr and aborts. The resolver never jumps to
anything the daemon did not publish.

## What is compiled ahead, and why each case is exact

Every function, lazy or not, is reached through its jump-table slot. Its address, the one a
program can observe, is its slot stub's, which is fixed when the image is laid out. That is
the same address a whole live build gives it, so laziness only changes where the stub jumps:

- **Function pointers and their comparisons, callbacks handed to C, signal handlers.** The
  pointer is the slot stub, one per function, so two pointers to one function compare equal
  whether it was compiled or not.
- **Vtable entries and statics holding function pointers.** These are relocations to the
  slot stub, written when the data is placed, before `main`.
- **TLS destructors and `atexit` handlers.** They are registered as slot stub addresses. A
  first call at thread or process exit resolves like any other, because the hook keeps no
  thread-local of its own.
- **The entry and what runs before or after `main`.** This covers the `main` shim and
  everything a static in `.init_array`, `.preinit_array`, `.fini_array`, `.ctors`, `.dtors`,
  `__mod_init_func`, `__mod_term_func` or `.CRT$X*` names. These are compiled ahead
  (`Plan::ahead`), so starting a run never waits on a fetch and never depends on when the
  channel is up.
- **Statics.** Statics are data, placed whole before the run.
- **Errors.** The front end's walk checks every instance a whole build would compile,
  including post-monomorphization const errors and layout errors, so a program rustc rejects
  is rejected here too (fastcompile-lazy 4).

One case is not yet exact: a lazy signal handler whose first call interrupts its own thread
inside the hook's ask. That waits on inproc's signal-safe asks (fastcompile-lazy 1).

## State of the art

- **glibc's `_dl_runtime_resolve_xsavec`** (lazy PLT binding): the same shape. One shared
  trampoline, the index pushed by a per-entry stub, and the vector state saved with
  `XSAVEC` sized from CPUID. blaze matches it, and adds AArch64 SVE state and Win64. It also
  resolves to compiled code, not to an existing symbol.
- **LLVM ORC's lazy reexports and `LazyCallThroughManager`**: per-function stubs that compile
  on first call. ORC's x86-64 resolver saves only the argument registers that fixed calling
  conventions use. blaze's saves the whole enabled vector state and `al`, so variadic and
  vector calls are exact. ORC has no single flight across processes. blaze's fetcher compiles
  once per function however many threads ask.
- **V8's lazy compilation and Julia's JIT**: compile on first call, then tier up. blaze does
  the same, with the copy-and-patch backend for the first call (`blaze-stencil`) and hot
  functions tiered up later. Its prefetch of callees and history keeps first-call waits off
  the critical path.
- **Allocation**: `Ready` and `Ask` frames are fixed-size stack arrays. Per-function state is
  three columns indexed by `LazyId`, and lookups by code hash binary-search a sorted column.

## Tests (`crates/blaze-lazy/src/tests/`)

- `stubs`: region layout, the entries, the resolver's cell load and trap, Win64 shadow space,
  the AArch64 encoders checked against `llvm-mc`, and the size limits.
- `abi` (on the host): calls through an entry, whose hook overwrites every argument
  register, match direct calls. It covers integers and floats past the registers, mixed
  arguments, a large aggregate with its indirect result, register pairs, homogeneous float
  aggregates, whole vector registers, a variadic `snprintf` call, and the trap on 0.
- `flight`: 32 threads asking 8 functions at once compile each once. Failures and panics
  answer every asker and stop the run once. Idle compiles report unasked failures. On the
  host, 16 threads call through real entries into a runner.
- `sim`: a deterministic simulation of the protocol, with dropped, duplicated and reordered
  frames, compiles finishing out of order, failures and resends, over 400 seeds. It also
  checks the fetcher's priorities.
- `wire`: round trips, malformed frames, and random bytes, which never panic and re-encode
  identically.
- `plan` and `refuse`: what is compiled ahead, ids, history, callees, and the release and
  output refusals.

## Hooks (v2/requests/fastcompile.md)

- fastcompile-lazy 1 (inproc): concurrent, allocation-free, signal-safe asks.
- fastcompile-lazy 2 (inproc): a run channel split into a reader and a shared sender.
- fastcompile-lazy 3 (link): slots that start at entries, the stub region, the `Placer`,
  and the runtime linked into the image.
- fastcompile-lazy 4 (codegen): a "compile later" walk and `compile_one`.
- fastcompile-lazy 5 (cargo): dev `run` and `test` take the lazy path. An explicit lazy
  release is an error.

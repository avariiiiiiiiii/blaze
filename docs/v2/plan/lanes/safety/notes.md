---
status: plan
summary: lane safety's design: hir-ty-ub (our own Miri over hir-ty-mir's evaluator), the evaluator's Machine hooks, fuzz targets, the model-checking scheduler, and the unsafe audit.
date: 2026-10-29
---
# Lane safety: notes

The plan is [unsafe.md](../../design/unsafe.md). This file says how lane `safety` builds what
proves it. Miri's source cited here is `rust-lang/rust` at `ad3d0bc141a0`
(`src/tools/miri/`, `compiler/rustc_const_eval/src/interpret/`); paths below are relative to it.

## Who does what

| piece | paths | agent |
|---|---|---|
| crate root, shared ids, `Ub` | `crates/hir-ty-ub/{Cargo.toml,src/lib.rs,src/ids.rs,src/ub.rs}`, this folder | safety |
| memory: allocations, init, provenance, bounds, frees, leaks | `crates/hir-ty-ub/src/memory.rs`, `src/memory/` | safety-mem |
| Tree Borrows | `crates/hir-ty-ub/src/borrows.rs`, `src/borrows/` | safety-tb |
| data races, weak memory, scheduler, sync | `crates/hir-ty-ub/src/race.rs`, `src/race/` | safety-race |
| the `Machine` impl, validity and retag visitor, `run_test`, program tests | `crates/hir-ty-ub/src/check.rs`, `src/check/`, `crates/hir-ty-ub/tests/` | safety-check |
| fuzz targets and the coverage-guided engine | `fuzz/` | safety-fuzz |
| model-checking scheduler, model tests | `crates/blaze-graph/src/check/` (and tests in owners' crates by request) | safety-model |
| `// SAFETY:` and `forbid(unsafe_code)` audit | files of finished owners only | safety-audit |

## hir-ty-ub

### Shape

The evaluator owns the bytes; the checker owns everything else about them. The evaluator is
generic over a `Machine` (below); `Plain` is the one const eval and every run without `--ub`
use, a zero-sized type whose hooks are empty `#[inline(always)]` functions and whose
provenance and shadow types are `()`, so a build without the checker carries none of it.

`check::Checker` is the `Machine` of `--ub`. It composes:

- `memory::Memory`: allocations by address range (a sorted column of starts, binary search),
  each with its id, kind, size, align, liveness, an init mask (Miri's `InitMask`: run-length
  blocks) and a provenance map (Miri's `ProvenanceMap`: sorted `(offset, Prov)` per pointer, and
  bytewise fragments for partial pointers). It checks bounds, use after free, alignment,
  null and dangling pointers, frees (kind, size, align, double free, interior pointer),
  exposure for int-to-pointer casts, and leaks (heap memory not freed and not reachable from a
  global: Miri's leak check, `src/tools/miri/src/machine.rs` `MiriMachine::collect_leaks` and
  `src/tools/miri/src/diagnostics.rs` `report_leaks`).
- `borrows::Borrows`: Tree Borrows per allocation, a port of
  `src/tools/miri/src/borrow_tracker/tree_borrows/` (`perms.rs`, `tree.rs`, `unimap.rs`,
  `foreign_access_skipping.rs`, `wildcard.rs`, `diagnostics.rs` for the history in reports) and
  of the protector bookkeeping of `src/tools/miri/src/borrow_tracker/mod.rs`.
- `race::Races`: `src/tools/miri/src/concurrency/` (`vector_clock.rs`, `data_race.rs`,
  `weak_memory.rs`, the sync objects of `sync.rs` reduced to what the evaluator's shims need,
  and `scheduler.rs`'s seeded choice of the next thread with Miri's preemption rate).
- `check::valid`: `compiler/rustc_const_eval/src/interpret/validity.rs` over hir-ty's types and
  layouts: run on every typed copy, it checks the value and retags each reference and `Box`
  in it with the copy's retag mode (as `ValidityVisitor::check_safe_pointer` calls
  `M::retag_ptr_value`), and resets padding to uninitialized and provenance on non-pointer
  bytes (`reset_provenance_and_padding`).

### Retags without Retag statements

blaze's MIR has no `Retag` statements, and needs none: current Miri retags inside the typed
copy, not at statements (`interpret/step.rs` `eval_rvalue_into_place`, `interpret/call.rs`
argument passing, `interpret/validity.rs` line ~623). The evaluator says the mode of each typed
copy and the checker does the rest:

| the copy | mode |
|---|---|
| `Assign(_, Ref(Mut { kind: TwoPhasedBorrow }, _))` | `TwoPhase` |
| any other `Assign(_, Ref(..))` to a reference type | `Default` |
| `Assign(_, Ref(..))` to a raw pointer type (`&raw`) | `Raw` (Tree Borrows: no retag) |
| every other `Assign` (`Use`, `Cast`, `Aggregate`, ...) | `Default` |
| a call's arguments into the callee's locals | `FnEntry` (protectors) |
| a call's return value into its destination | `Default` |

rustc's `WithRetag::No` marks only copies its own MIR passes add (deref temporaries, match
branch simplification), which blaze's MIR doesn't have.

### Errors

A hook that finds UB records it in the checker and returns `Err(Stop)`; the evaluator returns
`MirEvalError::MachineStop` (requested of db-ty, who owns `hir-ty`'s `MirEvalError`) wrapped in
its `InFunction` stack, and `check::run_test` turns that and the checker's record into a
report: the UB with Miri's wording, the thread, the statement, the call stack, and for an
aliasing error the history of the tags involved (Tree Borrows' `diagnostics.rs`).

## The evaluator's `Machine` (requested of codegen, owner of `crates/hir-ty-mir/`)

The narrowest set of calls that lets the checker see every memory event, retag and thread
switch. It goes in `crates/hir-ty-mir/src/mir/eval/machine.rs`, exported as
`hir_ty_mir::mir::{Machine, Plain, Ptr, MemKind, Access, RetagMode, Shadow, Stop, TypedCx,
Site, EvalThread}` (`EvalThread` is the evaluator's thread id, a `u32`). `check::Checker`
implements exactly this:

```rust
use std::ops::Range;

/// The evaluator's error for a stop the machine asked for; what it found the machine keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Stop;

pub type MResult<T> = Result<T, Stop>;

/// A pointer: its address, and the provenance the machine gave it (`None`: an integer).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Ptr<P> {
    pub addr: u64,
    pub prov: Option<P>,
}

/// What made an allocation, which says how it may be freed and whether it may leak.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MemKind {
    /// A local, live from its `StorageLive` (or the frame's start, for arguments, the return
    /// place and locals without storage statements) to its `StorageDead` or the frame's end.
    Stack,
    /// `__rust_alloc` and `__rust_alloc_zeroed`, freed by `__rust_dealloc`.
    Rust,
    /// `malloc`, `calloc`, `posix_memalign`, freed by `free`.
    C,
    /// A static, a constant's memory, a promoted temporary, a vtable, a string literal: never
    /// freed, and roots of the leak check.
    Global,
    /// `const_allocate`.
    ConstHeap,
    /// The evaluator's own: program arguments, the environment, thread-local storage. Roots of
    /// the leak check, never freed by the program.
    Machine,
}

/// How bytes are accessed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Access {
    Plain,
    /// An atomic load, store, or read-modify-write (the read and the write of one RMW are one
    /// `read` then one `write` with this, with nothing between them).
    Atomic { ord: AtomicOrd, rmw: bool },
}

/// `core::sync::atomic::Ordering`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AtomicOrd { Relaxed, Acquire, Release, AcqRel, SeqCst }

/// The retag a typed copy does (rustc_const_eval's `RetagMode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RetagMode { Default, TwoPhase, FnEntry, Raw, None }

/// Which bytes of a value the evaluator holds outside memory are initialized and which carry
/// a pointer's provenance. `()` for `Plain`.
pub trait Shadow<P>: Clone + Default + std::fmt::Debug {
    /// `len` initialized bytes with no provenance: a value the evaluator computed.
    fn init(len: usize) -> Self;
    /// A pointer of `size` bytes with provenance `prov`, initialized.
    fn pointer(size: usize, prov: Option<P>) -> Self;
    /// Bytes `range` of these.
    fn slice(&self, range: Range<usize>) -> Self;
    /// `other` put over these at `at`, growing them when it ends past them.
    fn put(&mut self, at: usize, other: &Self);
    /// The provenance of the pointer that starts at `at`, if a whole one does.
    fn prov_at(&self, at: usize, size: usize) -> Option<P>;
    /// Fails unless every byte of `range` is initialized: the bytes are about to be used as a
    /// scalar (arithmetic, a comparison, a `SwitchInt`, a cast, an address).
    fn require_init(&self, range: Range<usize>) -> MResult<()>;
}

/// Where the evaluator is, for reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Site<'db> {
    pub owner: InferBodyId<'db>,
    pub span: MirSpan,
}

/// What `typed` needs of the evaluator to read a value at its type.
pub struct TypedCx<'a, 'db> {
    pub db: &'db dyn HirDatabase,
    pub env: ParamEnvAndCrate<'db>,
    pub target: &'db TargetDataLayout,
    /// The type a vtable id (a `dyn` pointer's metadata) stands for.
    pub vtables: &'a VTableMap<'db>,
}

pub type EvalThread = u32;

pub trait Machine<'db> {
    type Prov: Copy + Eq + std::hash::Hash + std::fmt::Debug;
    type Shadow: Shadow<Self::Prov>;

    // Where the evaluator is: called before each statement and each terminator.
    fn step(&mut self, at: Site<'db>);

    // Memory. `alloc` returns the root provenance of the new allocation, which the evaluator
    // puts on every pointer it makes to it itself (a local's place, a constant's address).
    fn alloc(&mut self, addr: u64, size: u64, align: u64, kind: MemKind, zeroed: bool)
        -> MResult<Self::Prov>;
    fn free(&mut self, ptr: Ptr<Self::Prov>, size: Option<u64>, align: Option<u64>, kind: MemKind)
        -> MResult<()>;
    /// The root provenance of the allocation `addr` is in (for pointers inside a constant's
    /// memory the evaluator lays out itself).
    fn root(&mut self, addr: u64) -> Option<Self::Prov>;
    fn read(&mut self, ptr: Ptr<Self::Prov>, size: u64, align: u64, access: Access)
        -> MResult<Self::Shadow>;
    fn write(&mut self, ptr: Ptr<Self::Prov>, size: u64, align: u64, access: Access,
        shadow: &Self::Shadow) -> MResult<()>;
    /// `size` bytes from `src` to `dst`: `copy_nonoverlapping` (`overlapping: false`, which
    /// fails if they overlap), `copy`, and the evaluator's own moves between places.
    fn copy(&mut self, dst: Ptr<Self::Prov>, src: Ptr<Self::Prov>, size: u64, align: u64,
        overlapping: bool) -> MResult<()>;
    /// An atomic load's value under weak memory: the bytes it reads, when the store it reads
    /// from is not the latest (`None`: the bytes in memory). Called after `read`.
    fn atomic_load(&mut self, ptr: Ptr<Self::Prov>, size: u64, ord: AtomicOrd,
        latest: &[u8]) -> MResult<Option<Vec<u8>>>;
    /// An atomic store's bytes, for later loads under weak memory. Called after `write`.
    fn atomic_store(&mut self, ptr: Ptr<Self::Prov>, ord: AtomicOrd, bytes: &[u8])
        -> MResult<()>;
    fn fence(&mut self, ord: AtomicOrd, compiler_only: bool) -> MResult<()>;

    // Values.
    /// After every typed copy into `place` (the `Assign`s, arguments and return values of the
    /// table above): checks the value at `ty` and retags what it holds by `mode`. `bytes` are
    /// the value's bytes, already written.
    fn typed(&mut self, cx: &TypedCx<'_, 'db>, place: Ptr<Self::Prov>, ty: Ty<'db>,
        bytes: &[u8], mode: RetagMode) -> MResult<()>;
    /// A place projected through a dereference (`(*p).f`, `(*p)[i]`) or `ptr::offset`:
    /// in bounds of its allocation, as an inbounds GEP requires. `wrapping_offset` and
    /// `arith_offset` don't call this.
    fn offset(&mut self, ptr: Ptr<Self::Prov>, by: i64) -> MResult<()>;
    /// A pointer cast to an integer (`as usize`, `expose_provenance`).
    fn expose(&mut self, ptr: Ptr<Self::Prov>) -> MResult<()>;
    /// An integer cast to a pointer (`as *const T`, `with_exposed_provenance`).
    fn from_exposed(&mut self, addr: u64) -> Option<Self::Prov>;

    // Frames. `ret` ends the protectors of the frame's `FnEntry` retags.
    fn call(&mut self) -> MResult<()>;
    fn ret(&mut self) -> MResult<()>;

    // Threads. The evaluator keeps one code stack per thread and runs the one `schedule`
    // picks: at every terminator, when the running thread blocks or ends, and at
    // `sched_yield`/`thread::yield_now`.
    fn spawn(&mut self) -> MResult<EvalThread>;
    fn schedule(&mut self, running: EvalThread, runnable: &[EvalThread]) -> MResult<EvalThread>;
    fn join(&mut self, joined: EvalThread) -> MResult<()>;
    fn exit(&mut self) -> MResult<()>;
    /// A synchronization: `release` at a futex wake, an unlock, a condvar notify; `acquire`
    /// when the woken or locking thread goes on. `object` is the address of the futex word or
    /// lock.
    fn release(&mut self, object: u64) -> MResult<()>;
    fn acquire(&mut self, object: u64) -> MResult<()>;

    /// The main thread returned: the leak check, and threads still running.
    fn finish(&mut self) -> MResult<()>;
}
```

The evaluator's side, in order of what it touches:

1. `Evaluator<'db, M: Machine<'db> = Plain>`; `Address` becomes `Ptr<M::Prov>` on every path
   from a place to an access; `IntervalOrOwned::Owned` carries an `M::Shadow` beside its bytes
   (`Plain`: `()`), filled from `read`, from `Shadow::init` for computed scalars, from
   `Shadow::pointer` for `Ref` and pointer casts, and joined by `put` for aggregates.
2. Every byte read and write of program memory goes through `read`/`write`/`copy`, with the
   type's alignment (1 for untyped copies). Deref reads the pointer's provenance with
   `prov_at`; scalar uses call `require_init`.
3. `alloc`/`free` at each place the evaluator allocates: frame locals (`Stack`, per local, at
   `StorageLive`, and `free` at `StorageDead` and frame end), heap shims by their kind,
   statics, constants, promoted temporaries (`Global`), `const_allocate` (`ConstHeap`).
4. `typed` after each typed copy, by the mode table above; `call` and `ret` around frames.
5. Threads: `pthread_create`/`pthread_join`/`pthread_detach`/`pthread_self`, futex (`futex`
   on Linux, `__ulock_wait`/`__ulock_wake` and `os_sync_wait_on_address` on macOS),
   `sched_yield`, each thread its own code stack and TLS; a thread blocked on a futex is not
   runnable until woken. Atomics call `read`/`write` with `Access::Atomic`, `atomic_load` and
   `atomic_store`, and `fence`; `cxchg` failure is a load with the failure ordering.
6. `hir_ty_mir::mir::run_function::<M>(db, func: FunctionId, machine: M) -> (Result<(),
   MirEvalError<'db>>, MirOutput, M)`: runs a `fn()` (a `#[test]`) with the machine, `finish`
   after the main thread returns.

## Fuzzing (`fuzz/`)

One target per parser of untrusted bytes, written against the parser's public API, each with
a seed corpus and the invariants it checks (`fuzz/README.md` lists them). The engine is ours:
SanitizerCoverage's inline 8-bit counters and PC table (`__sanitizer_cov_8bit_counters_init`,
`__sanitizer_cov_pcs_init`) collected by callbacks in `fuzz/src/`, a corpus kept by new
coverage, and libFuzzer-style mutations. A target built without coverage stops with an error
naming the flag it needs (no-fallback); the seed corpora also run as plain tests.

## Model checking (`crates/blaze-graph/src/check/`)

A loom-style explorer under the simulation driver: shim atomics and threads, every
interleaving to a preemption bound by dynamic partial-order reduction, and the C11 orderings
by a store history per atomic (a relaxed or acquire load may read any store not
happens-before-hidden). Each lock-free structure in `blaze-ids`, `intern`, `blaze-query`,
`blaze-cas` and `blaze-graph` gets a model test against a sequential model.

## Benchmarks this matches

- Miri itself for the checker's rules (the port is rule for rule), and Miri's speed as the bar
  to beat: Miri pays a `RefCell` and a `BTreeMap` lookup per access; ours is a flat column per
  allocation and one binary search per access.
- libFuzzer and AFL++ for the engine (8-bit counters, value profile, corpus minimization).
- loom and shuttle for the explorer (loom's bounded DPOR, shuttle's randomized PCT).

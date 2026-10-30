---
status: plan
summary: lane safety's model checker for lock-free code (crate blaze-check): shim atomics, locks and threads as coroutines on one OS thread, loom's bounded DPOR over interleavings, every value a load may read under C11, and a model test per lock-free structure against a sequential model.
date: 2026-10-29
---
# Model checking lock-free code

`blaze-check` runs a closure under every interleaving of its threads up to a preemption bound,
and under every value each atomic load may read under the C11 model Rust uses. It finds data
races on tracked cells, deadlocks, panics and assertion failures in any of them, and prints the
seed and the schedule, which replay the failing execution exactly.

## Where it lives

`crates/blaze-check/` (new; agent `safety-model`), a leaf: its only dependencies are
`blaze-core` (the simulation driver's seeded `Rng`), `libc` on Unix (coroutine stacks) and
`windows-sys` on Windows (fibers). It cannot be a module of `blaze-graph`, as first planned:
blaze-graph will depend on `blaze-reclaim` (its pool enters a task per job), so a
`blaze-reclaim` → `blaze-graph` dependency, even under a `cfg`, is a cycle cargo refuses
(resolution is platform-independent). `blaze-ids` and `intern` depend on nothing of ours;
every crate with lock-free code can depend on a leaf. `blaze-core` must stay below every crate
with model tests (it has none).

## The switch

As loom's `cfg(loom)`. Each crate with lock-free code has one module that names its
synchronization types:

```rust
// src/sync.rs
#[cfg(not(blaze_check))]
pub(crate) use std::{
    cell::UnsafeCell, // only where a crate needs one: see "Cells" below
    hint, sync::{atomic, Mutex, OnceLock}, thread, thread_local,
};
#[cfg(blaze_check)]
pub(crate) use blaze_check::{hint, sync::{atomic, Mutex, OnceLock}, thread, thread_local};
```

and its manifest has

```toml
[target.'cfg(blaze_check)'.dependencies]
blaze-check.workspace = true
```

A normal build doesn't see the crate: nothing in it changes, not a type, a size or a
dependency. A model build is `RUSTFLAGS="--cfg blaze_check" blaze test -p <crate> --release
model_`; its model tests are `#[cfg(blaze_check)]` and its other tests `#[cfg(not(blaze_check))]`
where they spawn many threads or loop long (they still pass under the shims, only slower).
The root manifest adds `cfg(blaze_check)` to `unexpected_cfgs`' `check-cfg`.

## The API

The `std` API, path for path, so the switch is a `use`:

- `blaze_check::model(|| { ... })`: runs the closure under every execution;
  `blaze_check::Builder` sets the bounds (`max_preemptions`, from `CHECK_MAX_PREEMPTIONS`,
  default 3, `none` for no bound; `max_branches`, from `CHECK_MAX_BRANCHES`, default 10 000 per
  execution; `max_threads` up to 8; `stack_size`, from `CHECK_STACK_SIZE`, default 1 MiB per
  model thread), the random mode (`CHECK_RANDOM=<runs>` with `CHECK_SEED` and
  `CHECK_PCT_DEPTH`, default 3: shuttle's PCT over a seeded `Rng`, for models too big to
  exhaust) and the replay (`CHECK_REPLAY=<schedule>`).
- `sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU32, AtomicU64, AtomicUsize,
  AtomicI8, AtomicI16, AtomicI32, AtomicI64, AtomicIsize, AtomicPtr, Ordering, fence,
  compiler_fence}`: `const fn new`, `load`, `store`, `swap`, `compare_exchange`,
  `compare_exchange_weak` (no spurious failure is explored, as loom), `fetch_*` (`fetch_not` on
  `AtomicBool`), `fetch_update`, `get_mut`, `into_inner`, `Default`, `From`, `Debug`. `as_ptr`
  is not offered (a raw access the checker cannot see). Acquire, release and AcqRel fences act
  on the thread's own clocks and are no schedule points; an SC fence is.
- `sync::{Arc, Weak}` (std's), `sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard,
  RwLockWriteGuard, Condvar, WaitTimeoutResult, OnceLock, LockResult, TryLockError,
  TryLockResult, PoisonError}`. Taking a lock is a schedule point, releasing one is not (as
  loom: the releaser's next operation is). A condvar wait is an access of the condvar (so DPOR
  orders it with each notify) that releases the mutex and blocks in one step; a timed wait may
  time out at once (a branch); untimed waits get no spurious wakeup (as loom). `OnceLock`
  keeps its value in `std`'s cell: a `static` cell an earlier execution set stays set, as one
  set before the model (its initializer must do no operation the model sees).
- `thread::{spawn, Builder, JoinHandle, Thread, ThreadId, current, yield_now, sleep (a yield),
  park, park_timeout (may return at once: a branch), available_parallelism (2 in a model),
  panicking, Result, LocalKey, AccessError}`; `hint::spin_loop` (a yield);
  `thread_local!` with std's syntax (`const { }` too) making `LocalKey`s (`with`, `try_with`,
  and `set`, `get`, `take`, `replace`, `with_borrow`, `with_borrow_mut` on cells), one value per
  model thread, dropped when it ends. `thread::scope` is not offered: the crates under test use
  it only in tests, through `std::thread::scope`.
- `cell::UnsafeCell` with loom's `with(|ptr: *const T| ..)` and `with_mut(|ptr: *mut T| ..)`,
  and `get_mut`: each access is a tracked non-atomic read or write, and one not ordered by
  happens-before with another thread's write (or a write with a read) is a finding. Accesses
  are no schedule points: whether two race depends only on the clocks.
- `linear::{Spec, History}`: `History::record(op, |op| ..)` runs and records one operation of
  the calling model thread; `History::check(init)` panics (a finding, with the schedule) unless
  some linearization of the run gives every result on the `Spec`.

Outside a model the shims are std's types: a model build runs its other tests too.

## Cells

A crate whose lock-free code keeps values in `UnsafeCell` writes, in its `sync.rs`, a
zero-cost std twin with the same `with`/`with_mut` (an `#[inline(always)]` call of the
closure on `get()`), so the tracked accesses in model builds are the same lines as the plain
ones in normal builds. Values a structure owns through a raw pointer (`Box::into_raw` behind an
`AtomicPtr`) are checked through the atomics that publish them and the sequential model; a
structure that wants each field access checked keeps the field in an `UnsafeCell`.

## How it works

- **Threads are coroutines.** Each model thread runs on a stack of its own (`mmap` with a guard
  page on Unix, a fiber on Windows), switched by a few instructions of assembly (callee-saved
  registers and the stack pointer) on x86-64 and aarch64. A model runs on the OS thread that
  calls `model`, with no lock and no OS scheduling: a switch costs nanoseconds where loom's
  generator crate pays about the same and shuttle the same, and an execution allocates nothing
  once its stacks and tables are warm (they are reused across executions).
- **Schedule points** are every atomic operation (but non-SC fences), lock, wait, notify,
  once-cell access, join, park, unpark, yield and thread end. Before each, the explorer picks the thread that runs: the current one unless
  this execution's path says to preempt. The path is loom's (`rt/path.rs`): a list of branches
  (a schedule, the store a load reads, a spurious failure), replayed up to its end and extended
  past it; after an execution the last branch with an alternative left takes it.
- **DPOR** as loom's (`rt/execution.rs` `schedule`): each object keeps its last access and, for
  atomics, its last store; when another thread is about to do a dependent operation that does
  not happen after it (by the DPOR vector clock), a backtrack point is set at that access's
  schedule so the other order is explored. With a preemption bound, as loom, a second backtrack
  point goes at the last schedule where the active thread changed, to cover what the bound cuts.
- **Weak memory** as Miri's (`concurrency/weak_memory.rs`, which ports tsan11 and "Dynamic Race
  Detection for C++11"): each atomic keeps its stores in modification order, each with the
  storing thread's timestamp, the clock a load acquires by reading it, whether it is SC, and the
  first timestamp each thread loaded it at. A load may read any store not hidden by coherence
  (write-read: a store that happens before the load hides everything earlier; read-read: a store
  some load that happens before this one read hides everything earlier) and by the SC rules
  (SC fences restrict reads, SC loads ignore all but the last SC store). Where Miri picks one
  candidate at random, the explorer makes each one a branch: every value is explored. RMWs and
  failed compare-exchanges read the latest store, as C11 requires of an RMW. Release sequences
  as C++20 (an RMW joins the clock it replaces; a plain store breaks the sequence). SC fences are
  an AcqRel RMW on one global clock (Miri's rule). A thread that yields (`yield_now`,
  `spin_loop`) may not reread a store it saw before its last yield (loom's
  `is_seen_before_yield`), so spin loops end.
- **Races** on `UnsafeCell` and on atomics accessed through `get_mut`/`into_inner`: per cell the
  last write's (thread, epoch) and each thread's last read epoch, checked against the accessing
  thread's clock (FastTrack's rule with a full read vector).
- **Findings** stop the model: a panic in any thread, a data race, a deadlock (no thread can
  run and some has not ended), a thread left running when the closure returns and never ending,
  or an execution longer than `max_branches` (a spin without a yield). The report names the
  finding, each thread's last operation, the seed and the schedule as a string of branch
  choices; `CHECK_REPLAY=<that string>` runs just that execution again.

## Compared with the state of the art

| | loom 0.7 | shuttle 0.8 | blaze-check |
|---|---|---|---|
| search | bounded DPOR (CDSChecker) | random, PCT | bounded DPOR (loom's), and PCT under a seed |
| weak memory | 7 stores per atomic, mo as vector clocks | sequentially consistent only | every store, Miri's C11 rules, each candidate a branch |
| SC fences, SC accesses | joins a global SC clock | n/a | Miri's (C++20 coherence-ordered-before, fences as an RMW on one clock) |
| threads | `generator` crate | `generator` crate | own coroutines, one OS thread |
| `static` atomics | `lazy_static!` only | yes | yes: `const fn new`, reset per execution |
| `thread_local!` | `loom::thread_local!` | yes | yes, std syntax incl. `const { }` |
| cells | `UnsafeCell::with`/`with_mut` | none | same as loom |
| replay | checkpoint file (feature) | seed and schedule | seed and schedule string |
| linearizability | none (asserts only) | none | `linear::History` against a sequential `Spec` |
| spurious failures | none | none | none for `compare_exchange_weak` and untimed waits; timed waits and `park_timeout` branch |

What none of them (and this) covers: load buffering (a load reading a store later in execution
order) and modification orders that contradict execution order (2+2W); those need a promising
semantics, which no implementable model has with SC accesses (Miri's notes say the same). A
freshly allocated atomic's initial value happens before every access here (as a `static`'s
does), where loom, whose `new` isn't `const`, checks a first read racing with it.

## Model tests

Each lock-free structure gets a test that runs its operations from 2 or 3 threads under
`model`, records each operation's call and result, and checks the outcome against a sequential
model of the structure: some order of the operations that keeps every call-before-return order
of the run (a linearization) gives the same results on the sequential model. The checker is
`blaze_check::linear` (Wing and Gong's search with Lowe's memo of (operations done, state)
pairs, as porcupine and lincheck keep) so each test is the structure, its sequential model and
a few calls. safety-model's children (`safety-model-{format,db,store,graph}`) write them: in
the crate where its owners are done, as a request to them where not.

| crate | structure | owner |
|---|---|---|
| `blaze-ids` | `Sharded` interner (shard tables, chunk slots) | format |
| `intern` | `arena.rs`'s sharded table | format |
| `blaze-query` | the revision `generation` counter and the shards | db |
| `blaze-cas` | `table::Table` (levels, slots, flag bits), `segment.rs` chunks, `flags::Bits` | concurrency (store) |
| `blaze-graph` | `pool.rs` (job lists, idle bits, sleepers), `pump.rs` (published waits, flights), `arena.rs` totals | core |
| `blaze-reclaim` | slots, `enter`/`Guard`, `Grace`, `Limbo` | concurrency |

The tests and switches for crates whose owners are still at work are requests in
`v2/requests/<owner>.md`, ready to paste.

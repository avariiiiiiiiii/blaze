---
status: plan
summary: lane safety's speed of light: the bound each hir-ty-ub component's work and the hardware set, the arithmetic, and how the design reaches 90% of it.
date: 2026-10-29
---
# Lane safety: speed of light

The bar is "Within 10% of the theoretical maximum, everywhere" in
[principles.md](../../goals/principles.md).
Each component of `hir-ty-ub` has a section here, written by the agent that owns it. Cycle
counts are for one core of the target machines (Graviton 5, EPYC Turin): about 3.5 GHz, an
L1 hit in 4 to 5 cycles, L2 in about 14, and branchy pointer code at about 2 instructions per
cycle. Figures in ns use 3.5 GHz.

## The checker: `check.rs`, `check/` (safety-check)

The checker does no work of its own beyond its hooks: it is called by the evaluator once per
event and hands each event to memory, Tree Borrows and the race detector. So its bound is
per hook, and its own part of each hook should be a few instructions on lines already in L1.
The whole run is then bound by the evaluator's events times these costs. The benchmark is Miri
on the same programs, which does the same checks; `proof` compares the two once things
compile.

### `step`, once per MIR statement and terminator

Work: turn the evaluator's `Site` (body and MIR span) into the dense `SiteId` that the race
detector and Tree Borrows store with each access.

- Bound: compare the body with the current one, pick the span kind's column, check the
  index against the column's length, and load the id. That is about 16 instructions on two
  lines in L1: 8 cycles, 2.3 ns.
- Design: `Sites` keeps one table per body, found through an `FxHashMap` only when the body
  changes (a call or return), and one dense `Vec<SiteId>` per span kind indexed by the span's
  arena index. A statement is interned with no hashing. The first version hashed every
  statement's site, which costs about 30 to 40 cycles from hashing a 16- to 24-byte key,
  probing and comparing it.
- What stands in the way of the last factor: the evaluator passes the site by value. If
  `Site` carried a body-local statement index, `step` would be one indexed load, about 3
  cycles. That is a change to the `Machine` contract (codegen-mir) and is worth making only
  if `proof` measures `step` above 10% of a statement's total cost.
- Tree Borrows' garbage collection runs from `step` every 10,000 retags (Miri's default).
  It visits every live tag once, so it costs a few cycles per tag per 10,000 retags,
  amortized.

### An access (`read`, `write`, `copy`)

Work: memory's bounds, liveness and alignment check; then the race check; then Tree Borrows'
permission check.

- Memory: one allocation record (base, size, alignment, live) by `AllocId` index, one line
  and about 12 instructions.
- Race detector, one thread: `race.rs` skips everything until a second thread is spawned
  (Miri's `multi_threaded`), about 2 instructions. Several threads: one epoch compare per
  location in the common case, about 10 instructions and a line.
- Tree Borrows: the tag's node and the location's permission, one or two lines, about 20
  instructions when the access changes nothing (Miri's fast path).
- The checker's glue: about 10 instructions, no allocation, no hashing.
- Bound: about 45 instructions on 4 lines in L1 or L2, about 25 cycles (7 ns) per access,
  plus about 5 cycles with several threads.
- Design: `place`, `plain_access` and `atomic_access` call each part once, in Miri's order,
  and return at the first error. The error path is `#[cold]`. The read's shadow is a
  `memory::Bytes`, which holds values of up to 64 bytes with one provenance run inline, so
  a scalar or pointer read allocates nothing.
- What stands in the way: a read of more than 64 bytes builds a heap `Bytes` (memory's
  representation and the `Machine` trait's owned `Shadow`). A borrowed view of memory's
  shadow would remove that allocation. It is proportional to the bytes moved, so it matters
  only for large copies, and `copy` (the common way large values move) already works on
  memory without building a shadow.

### `typed`, once per typed copy (validity and retags)

Work: check that the N bytes copied are a valid value of their type, retag each of the P
references in them, and reset padding and provenance as Miri's `validity.rs` does.

- Bound: one lookup of the type's plan, then about 5 cycles per plan op (K ops), one cycle
  per 64 bytes for the init check, and one Tree Borrows retag per reference (about 50
  cycles plus the pointee's locations). That is about 10 + 5K + N/64 + 50P cycles.
- Design: a type's plan is built once per run (`Plans`, reused across schedules) as a flat
  list of ops, with enum, variant, pointee and `UnsafeCell` tables as columns. The plan is
  found with one `FxHashMap` probe on the interned `Ty`, which is a pointer. The walk reuses
  scratch vectors, so a copy allocates nothing. Strings are built only for an error.
- What stands in the way: none known in the checker. Validating every typed copy is what
  Miri does, and the checks are what make it Miri's semantics.

### `run_test`, a test under several schedules

Work: S schedules of a test, each a full run of time T.

- Bound: with C cores and many tests, total work over cores, Σ S·T / C. The critical path is
  the longest single schedule, max T, because schedules are independent.
- Design: `run_test` runs the schedules of one test in a loop, reusing the type plans, and
  stops at the first that fails. A runner reaches the critical-path bound by giving each
  seed its own job: `run_test(db, test, &Config { seed: s + i, schedules: 1, .. })`. A job
  then rebuilds the plans of the types it uses, a few microseconds each.
- What stands in the way: within one `run_test` call, schedules run one after another,
  because `&dyn HirDatabase` can't be shared across threads. With one test and many
  schedules, splitting the seeds is the runner's job (`blaze test --ub`, lane cargo), and
  the call above is how it does it.

### What `proof` should measure

- ns per `step`, per access, and per typed copy against the bounds above;
- the checker's run time over the plain evaluator's (`Plain`) on the same program;
- the checker against Miri on the same programs.

A component more than 10% off its bound is a bug.

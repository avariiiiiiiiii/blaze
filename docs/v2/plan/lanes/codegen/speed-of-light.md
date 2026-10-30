---
status: plan
summary: lane codegen's speed of light: for each component, the bound its work and the hardware set, the arithmetic, how the design reaches 90% of it, and what stands in the way where it does not yet.
date: 2026-10-29
---
# Lane codegen: speed of light

The bar is "Within 10% of the theoretical maximum, everywhere" in
[principles.md](../../goals/principles.md). Each part of lane codegen has a section here,
written by the agent that owns it; a sibling adds its own section below rather than rewriting
the file. The thread shims have theirs in [speed-mir-threads.md](speed-mir-threads.md).

Figures are for one core of the target machines (EPYC Turin, Graviton 5), from
[hardware.md](../../design/hardware.md), with the same constants as
[frontend's](../frontend/speed-of-light.md):
- about 3.5 GHz;
- an L1 hit in 4 to 5 cycles, L2 in about 14;
- streaming from L2 at about 32 bytes a cycle, DRAM at 20 to 30 GB/s;
- an IPC of about 2 for branchy code and about 4 for straight-line integer code;
- a mispredicted branch costs about 15 cycles;
- a hash-map hit (FxHash, one probe, `RefCell` borrow flag) about 20 to 30 cycles;
- a small `malloc` with its `free` about 40 to 80 cycles.

Nothing has been measured: nothing may compile until every lane is done. `proof`'s budgets
measure each bound once it does. Instruction counts are estimates from the code as written.

## codegen-mir: `crates/hir-ty-mir` (MIR building, passes, monomorphization, the evaluator)

Owner: codegen-mir. The inliner (`optimize/inline*`) is codegen-inline's and not counted here.

### 1. The evaluator, per statement (`eval.rs`, `Machine = Plain`)

What it runs: const eval, `#[test]`s and build-time evaluation, so this is the bound of every
program blaze interprets.

**Work.** Take `_3 = Add(_1, _2)` on `u64`, the common case: read 16 bytes, add, write 8.
- **Bytes.** 24 bytes of frame, plus the statement's encoding (16 bytes when decoded), all in
  L1 in a running loop: under 1 cycle.
- **Instructions, the minimum.** Fetch the opcode and dispatch (2, plus an indirect jump), two
  frame-relative loads (base plus an immediate offset: 2), the add (1), the store (1), advance
  (1): about 8. The indirect jump predicts well under threaded dispatch, about 1 in 10 missed
  over mixed code: 1.5 cycles. At an IPC near 2, **the bound is about 5 to 6 cycles, 1.6 ns a
  statement.** A `SwitchInt` or `Goto` costs the same; an overflow-checked add is two
  statements (`CheckedBinaryOp` and `Assert`), about 11 cycles.

**Where the code is.** Per such statement, estimated:

| Step | Cycles |
| --- | --- |
| `place_addr` of the destination (arena index, projection loop) | 15 |
| two `eval_operand` → `eval_place` → `place_addr_and_ty_and_metadata` | 30 |
| `operand_ty` (`place_ty`, a `projected_ty_cache` probe per projection) | 10 to 30 |
| `size_align_of_sized` of the operand type (`layout_cache` probe) | 25 |
| two `read_scalar` → `peek_memory` (address-space match, thread-region math, bounds) | 30 |
| the arithmetic (`pad16`, match on op and type, widening through `u128`) | 40 |
| the result as `IntervalOrOwned::Owned(Vec<u8>)`: `malloc` and `free` | 40 to 80 |
| `copy_from_interval_or_owned` into the destination | 20 |
| `steps_left`, `exec_statement`'s match, `Result` plumbing | 15 |
| **total** | **about 230 to 290** |

That is **about 2% of the bound**. A terminator adds `pick_at_terminator` (a branch while one
thread runs). A call adds the callee lookup (`mir_or_dyn_index_cache`, one probe), the frame's
layout (`create_locals_for_body` sizes every local through `size_align_of`, one probe each),
and copying the arguments: hundreds of cycles plus 25 per local, against a bound of about 20
cycles plus the bytes of the arguments.

**What stands in the way.** The evaluator walks typed MIR and works out on every execution
what is fixed once the body is monomorphized: each place's type, size and offset, each
operand's scalar kind, the callee of each direct call, and the frame's layout. Values in flight
are heap `Vec`s. None of that depends on the values, so it is all removable.

**The design that reaches 90%: decoded bodies.**
- **Decode once per instance.** The first time a `(body, generic args)` runs, it is decoded to
  an `EvalBody`: a frame layout (every local's offset and size, computed once), and a flat
  `Vec<Inst>` of 16-byte instructions with everything resolved: frame offsets, sizes, the
  scalar kind (width, signedness, float), constant bytes already placed in the heap, and each
  direct call's callee as an index into the evaluator's table of decoded bodies. A place with
  projections decodes to a short chain of steps (add an immediate, load a pointer, scale an
  index), each fixed at decode time. Decoding costs about one walk of the body, paid once per
  instance per evaluator (each `const` evaluates once, so this is the bound of the decode).
- **No values on the heap.** Every instruction reads its operands where they live and writes
  its result straight to its destination; `IntervalOrOwned::Owned` goes from the hot path.
- **Frame-relative fast paths.** A frame operand is `stack[base + offset]` in the running
  thread's region, with no address-space decoding; pointers keep the packed `Address` and its
  decoding only behind a dereference.
- **Dispatch.** A dense `match` on the opcode in one loop (a jump table) first. If dispatch
  mispredicts dominate when measured, threaded dispatch with explicit tail calls (`become`,
  available on blaze's own nightly) and superinstructions for the common pairs
  (`CheckedBinaryOp` + `Assert`, `Discriminant` + `SwitchInt`).
- **Hooks.** `M::CHECKS` already compiles every hook out for `Plain`. Under a checker, an
  instruction carries the type ids the hooks need (`typed` still takes the `Ty`), so the
  checker's cost is its own work and the evaluator adds under 10% to it.
- **One path.** The decoded form replaces the typed walk; there is no second, slower
  interpreter kept beside it (rule no-fallback). The generic-placeholder case
  (`assert_placeholder_ty_is_unused`, which takes `(0, 1)` as the size) decodes with that
  size.

Estimated after it: about 8 instructions plus dispatch, **7 cycles a simple statement, about
80% of the bound**; with superinstructions and threaded dispatch, **6 cycles, about 90%**.
Calls: the frame is a bump of the thread's stack by the decoded frame size and a copy of the
arguments, about 25 cycles plus the bytes.

**State of the art.** The fastest interpreters of a typed bytecode run a simple operation in a
few cycles: wasm3's threaded dispatch and LuaJIT's interpreter. The comparable checker of Rust
is Miri, which walks MIR and re-derives types and layouts as this evaluator does today, at
hundreds of cycles a statement or more. The decoded design is wasm3's design on MIR; it beats Miri for the same
reason. Where it does not match wasm3 is the checker's hooks, which are the checker's work.

**Done now.** An `Assert` no longer asks the database whether the body checks overflow unless
the assertion has failed (`checks_overflow` was two database lookups on every checked
operation, an estimated 100 to 200 cycles each time). The decoded-body rewrite is the next change in this
component; it replaces `eval_place`, `eval_operand`, `eval_rvalue` and `create_locals_for_body`
as a whole, so it lands in one piece once the crate compiles, with the differential tests
against rustc (`tests/mir`) and the shim tests as its proof.

### 2. Unwinding and threads in the evaluator (`eval/unwind.rs`, `eval/threads.rs`)

- **Unwinding.** The work is the cleanup blocks that run, plus one step per frame left. Each
  frame is popped once and its cleanup block is a block like any other; the payload is moved,
  not copied. There are no unwind tables to search. **At the bound** (the cleanup code itself),
  once the cleanup blocks run decoded as in section 1.
- **Thread switch.** The work: park the running thread's stack region and unpark the next
  one's, and the machine's `schedule`. Regions are separate `Vec`s that are swapped, not
  copied, so a switch is a few pointer moves and the scheduler call: about 20 cycles, **at the
  bound**. What is off it: `pick_at_terminator` builds the list of runnable threads (a `Vec`)
  at every terminator while more than one thread runs, about 50 cycles a block. A runnable set
  kept up to date on block and wake (a bitset of at most 240 threads, four words) removes it.
- **Shims.** In [speed-mir-threads.md](speed-mir-threads.md).

### 3. MIR building (`lower.rs`, `lower/`)

**Work.** Per HIR expression, about 1.5 MIR statements and 0.7 locals come out.
- **Bytes.** Read the expression's view (about 16 bytes), its type and adjustments from
  inference (8 to 16), write about 1.5 × 40 bytes of statements and a 16-byte local: about 100
  bytes, 3 cycles from L2.
- **Instructions, the minimum.** Dispatch on the expression (3, plus a mispredict 1 in 4:
  4 cycles), fetch the type and adjustments (4), build and push the statements (about 20 each,
  30), the local (10): about 50 at an IPC of 2. **Bound: about 30 cycles, 9 ns an expression.**

**Where the code is**, estimated per expression: the type fetch is an arena index (cheap);
`expr_adjustments.get` is a hash probe (25); every temporary's type is interned again with
`.store()` (a probe of the type interner, 50 to 100); an operand list is a boxed slice (a
`malloc`, 50); drop-scope bookkeeping (20); and a path expression resolves its name again
through the resolver (`update_to_inner_scope` and `resolve_path_in_value_ns_fully`, several
def-map probes, 200 to 500), though inference resolved it already. **About 200 to 400 cycles:
8 to 15% of the bound.**

**What stands in the way, and what removes it.**
- Name resolution done twice: inference records what each path resolves to (it has
  `assoc_resolutions` and `variant_resolution` already); recording the value-namespace result
  for every path expression in `InferenceResult` lets lowering read one column. That is
  `hir-ty`'s; I will ask db-ty for it in `v2/requests/codegen-mir.md`.
- Types interned again: locals keep the stored type inference already holds (`StoredTy` is an
  interned id; a `Ty` → `StoredTy` map per body, filled once, makes `.store()` an index).
- Boxed operand lists: one arena per body for operands, freed with the body.
- Hash probes for adjustments: an adjustments column indexed by `ExprId`, as the HIR columns
  are (hir-ty's, same request).

With those, about 60 to 70 cycles: **about 45% of the bound**; the remaining gap is the
statement representation (an enum of up to 48 bytes with boxed parts), which the decoded form
of section 1 and codegen's lowering both read. Making MIR a column store like the HIR's is the
step to 90%; it is lane-wide (every reader of MIR changes), so I propose it in the codegen
plan rather than doing it alone.

**The edit bound.** MIR building is a salsa query per body: an edit rebuilds only the bodies
whose HIR or inference changed. That is at the bound for invalidation at body granularity.

### 4. MIR passes (`optimize.rs`)

**Work.** 25 passes (`run!` in `optimize`), each a walk over every statement and terminator;
rustc's order, which is semantics (rule superset-of-rust: what a pass leaves decides what
codegen and the lints see). Per statement per pass, about 20 to 40 cycles: **about 500 to 1000
cycles a statement** at release, against about 4 passes' worth (simplify-cfg, the dataflow
passes, and one cleanup walk) if independent local passes were fused: **about 100 to 150
cycles, so 15 to 25% of the bound.**

**What stands in the way.** Fusing passes changes nothing only where they commute; each pair
has to be shown to (the differential tests against rustc's MIR at each level are that proof).
The local, statement-at-a-time passes (`RemoveStorageMarkers`, `RemoveZsts`,
`RemoveNoopLandingPads`, the copy-prop cleanups) fuse into one walk; the dataflow passes stay
separate. Until that is measured, the passes run once per body per edit (they are a salsa
query), so their cost is paid on changed bodies only.

### 5. Monomorphization (`monomorphization.rs`)

**Work.** Copy the body (memory-bound: body bytes over L2 bandwidth, about 0.1 cycle a byte)
and substitute the generic arguments in every type, constant and argument list.
- **Bound.** Each distinct type of the body folded once: a body has far fewer distinct types
  than type occurrences (often 10 to 20 against hundreds), and a fold is an interner probe
  (50 to 100 cycles). So about 20 × 75 = 1500 cycles plus the copy for a body of a few hundred
  occurrences.
- **Where the code is.** `fill_ty`, `fill_const` and `fill_args` fold every occurrence
  (`try_fold_with` then `.store()`): hundreds × 75 to 150 cycles, **about 10 to 20% of the
  bound** on a typical body.
- **What removes it.** A per-instance memo from the stored type to its substitution (an
  `FxHashMap<StoredTy, StoredTy>`, 25 cycles a hit), so each distinct type folds once: **about
  80 to 90%**, the rest being the copy. This one is small and local; it is next after the
  decoded evaluator.

STATUS of this section: the bounds are written; one gap closed (the `Assert` overflow query);
the evaluator's decoded bodies, monomorphization's memo and the runnable bitset are the next
changes, in that order; MIR lowering's two gaps are requests to hir-ty.

---
status: plan
summary: lane db's speed of light: the bound each component's work and the hardware set, the arithmetic, how the design reaches 90% of it, and what stands in the way where it doesn't yet.
date: 2026-10-29
---
# Lane db: speed of light

The bar is "Within 10% of the theoretical maximum, everywhere" in
[principles.md](../../goals/principles.md). Each component of lane db has a section here,
written by the agent that owns it. Add yours as a new `##` section; don't rewrite another's.

The machine figures, for one core of the target machines (EPYC Turin, Graviton 5) unless noted:
about 3.5 GHz, an L1 hit in 4 to 5 cycles, L2 in about 14, L3 in about 50 (14 ns), DRAM in
about 110 ns, and branchy pointer code at about 2 instructions per cycle. A core keeps about 20
cache misses in flight, so independent misses cost about 110 / 20 = 5.5 ns each in throughput.
DRAM bandwidth: Turin's 12 channels of DDR5-6400 give 614 GB/s peak per socket, about 480
achievable; one core streams about 30 GB/s. Turin has up to 192 cores and 32 MB of L3 per CCD.
Figures in ns use 3.5 GHz.

## vendor/salsa: the front end's query database (speed-salsa)

Costs are from the code as of 10-29 19:30: S1–S4 done, S5's edges and claim-free verify done,
and `speed-salsa-next3` writing the memo rows (`MemoRows`). Benchmarks and the before/after are
in [speed-salsa.md](speed-salsa.md). "Off" is today's cost over the bound.

### Memo hit (`fetch`: value present, verified this revision)

The most frequent operation in a warm build.

- **Work:** find the memo of (function, key), check `verified_at` against the current
  revision, take the value, and record the read.
- **Bound:** the memo's row address is computable from the key: a page base by the slot's page,
  plus slot times row size. Then 1 dependent load for the page base (an L1 hit from a per-task
  cache, since pages never move), 1 for the row, and about 20 instructions: address arithmetic,
  the revision compare, a value-present test, and the read's record (a compare with the last
  edge and a 4-byte store into the task's frame). Hot: 2 × 4.5 + 20 / 2 ≈ 19 cycles, **5.4 ns**.
  Cold (rows not cached, at a warm build's start): 1 DRAM miss per hit, but hits from
  independent callers overlap, so **≈ 5.5 ns** throughput per core and 110 ns latency.
- **Today (S1–S4, before MemoRows):** 7 dependent lines (ingredient cache, `boxcar` bucket,
  `Page`, slot validation, the slot's `MemoTable` array, the `MemoEntry` pointer, the boxed
  `Memo`) and about 60 instructions. Hot: 7 × 4.5 + 30 ≈ 62 cycles, 18 ns, **3.3× off**. Cold:
  7 dependent misses, 770 ns latency, **7× off**.
- **What stands in the way, and the design that removes it:**
  1. The `MemoTable` array, the `MemoEntry` pointer and the `Box<Memo>`: three lines.
     `MemoRows` (speed-salsa-next3) puts the memo in a row of a column per function, which
     removes all three. That leaves 4 lines.
  2. The `boxcar` bucket and the `Page` header: two lines found again on every hit, though
     neither moves once published. Keep `(ingredient, page) → row base` in the task's cache,
     beside its node cache, so a hit on a page the task has seen loads only the row. That
     leaves 2 lines, plus validation.
  3. Slot validation (a tracked struct's `updated_at`, an interned value's
     `last_interned_at`): one more line, needed only for slots that can be reused. Immortal
     interned keys (hir-def's IDs, `revisions = usize::MAX`) skip it already in `memos()`; the
     row fetch must not reload the slot for them.

  With all three: 2 dependent lines, which is the bound. Hot ≈ 20 cycles, cold ≈ one miss.

### Recording a read

- **Work:** append the dependency to the task's frame once per distinct key.
- **Bound:** a compare with the last edge (S4), else an L1-resident probe of the frame's set
  (about 10 cycles), and a 4-byte store of the dependency's node. **≈ 3 ns** for a new key, 1 ns
  for a repeat.
- **Today (S5):** the frame holds 12-byte `QueryEdge`s. When the query finishes, each edge is
  turned into a node by a probe of the task's 1024-entry node cache (L1 on a repeat key) or the
  shared node table (a lock-free probe: 1 to 2 lines, plus the key columns on a tag match). That
  is about 15 ns per distinct edge on a cache miss, **≈ 5× off** for edges to keys the task
  hasn't seen.
- **Design to reach it:** the read already loaded the dependency's memo row, so carry the
  node there: a 4-byte `node` field in the row, set when the memo is first published. Inputs,
  interned and tracked struct fields get a node column beside their slots, set on first read.
  Then the frame records the node directly, and finishing a query copies words with no probe.
  The node table remains only for keys being recorded for the first time.

### Edges at rest

- **Work:** remember, for every memo, the ordered list of what it read.
- **Bound:** an edge must name one of N nodes plus its kind: ⌈log2 N⌉ + 1 bits. Nushell
  (#734) has about 100M edges; at N ≈ 10M nodes that is 25 bits, **≈ 312 MB**.
- **Today (S5):** 4-byte words in one CSR array, 400 MB, **28% off**. Before S5 it was 8 bytes
  an edge plus one allocation per memo, about 0.8 GB.
- **Design to reach it:** compaction (`Graph::compact`, run with `&mut` at a new revision once
  the array is twice what is live) already copies every live range. It should write the copy
  bit-packed at the width the largest node needs, frame-of-reference style (data.md, "warm"),
  read in place with a shift and a mask and unpacked 8 at a time with SIMD. Freshly recorded
  chunks stay at 4 bytes until the next compaction. Settled edges are then within 1 bit of the
  bound (4% at 25 bits). Outputs are rare, so a per-memo output list in the row's extra data
  would also free the kind bit.

### Memo rows at rest

- **Work:** what a memo must keep: `verified_at`, `changed_at`, durability and flags, its value
  (or a reference to it), its edge range and its node.
- **Bound:** 4 + 4 + 1 + 4 + 4 + 2 + 4 ≈ **24 bytes** hot (revisions fit in 32 bits for the
  daemon's lifetime; the edge length in 16 bits, with longer lists marked in the extra data).
  The 16-byte value fingerprint goes in a side column, only for memos that have one.
- **Today:** 40-byte `MemoHeader` plus the value, boxed (48 bytes for an 8-byte value), plus an
  8-byte `MemoEntry` for every function keyed on the slot's struct type. **≈ 2.3× off**.
- **Design:** `MemoRows` at 24 bytes: `Revision` stored as `u32` in the row (salsa's
  `Revision` is a `usize` today), durability and flags in the top bits of `changed_at`, and
  values of at most 8 bytes that are `Copy` held in the row. That is at the bound.

### Deep verify (a warm build after an edit)

- **Work:** for each memo that may have changed, walk its edges in read order and check each
  dependency's color; stop at the first changed one.
- **Bound:** per edge, one sequential 4-byte edge load and one random load of the dependency's
  color. The colors of an edge list are independent loads, so the walk can issue them all
  before it checks them in order. Re-verifying all of nushell's 100M edges touches 100M random
  lines: 6.4 GB / 480 GB/s = **13 ms** on all of Turin's cores, if the colors are in DRAM. With
  a dense color column (below) of 8 bytes per node, 10M nodes are 80 MB, much of which stays in
  L3; the bound is then about 100M × 6 instructions / (2 IPC × 192 × 3.5 GHz) ≈ **0.5 ms**. An
  edit's real walk is the part reachable from what changed, far less than all of it.
- **Today (S5, claim-free verify):** per edge, the node's key (a page load and two column loads:
  3 lines), then the hit path of the dependency's memo (7 lines today, 4 after MemoRows), all
  dependent. That is ≈ 7 × 110 ns = 770 ns per edge when cold. All 100M edges on 192 cores take
  ≈ 400 ms, **≈ 30× off** the DRAM bound.
- **What stands in the way, and the design that removes it:** the color lives in the memo, which
  is found through the key. rustc keeps it in `DepNodeColorMap`, a dense array indexed by node.
  Give salsa the same: a node-indexed column of `verified_at: AtomicU32` and
  `changed_at_and_durability: AtomicU32` (8 bytes per node), written where the memo's are today
  (publish, `mark_as_verified`, an input's setter, interning). Deep verify then reads edge
  words and colors only, and goes to the memo row only for a node whose color says "not yet
  verified this revision". It prefetches the colors of the next 8 edges as it checks one. That
  is 1 random load per edge, the bound.

### Interning a hit (`intern_id`, every multi-argument query call)

- **Work:** hash the key and find its value.
- **Bound:** FxHash of a 2- to 3-word key (≈ 3 cycles), then 1 line for the group (control word
  and pointers together) and 1 line for the value, to compare the key. **≈ 2 lines, ≈ 15 cycles
  hot.**
- **Today (S2):** a table pointer (L1), one 64-byte group line holding the control word and
  seven pointers, and the value line. **At the bound.** Before S2 it was a shard `Mutex` (two
  atomic read-modify-writes on a shared line, about 40 to 100 cycles uncontended and far worse
  contended) plus a `hashbrown` probe.

### LRU eviction (S1, once per revision per `lru` function)

- **Work:** rank the memos that hold values by `verified_at`, and drop the oldest beyond the
  cap.
- **Bound:** read 4 bytes of `verified_at` per row. 1M rows are 4 MB: 0.13 ms on one core, or
  **8 µs** at all-core bandwidth.
- **Today:** a walk of the slot pages and each boxed memo: about 2 lines (128 bytes) per row,
  4 ms per 1M rows on one core. **32× off**, but it runs once per revision with `&mut`, so no
  reader waits.
- **Design:** with `MemoRows` the scan reads 24-byte rows, 6× the bound in bytes. That is
  24 MB, 0.8 ms on one core, 50 µs if the scan is split across pages and run as plane tasks.
  Splitting `verified_at` into its own column would reach the bound but would cost the hit path
  a second line. The hit path runs millions of times per revision and the scan once, so the
  row stays whole. 50 µs is well under 10% of an edit's budget.

### Claims (single flight, once per execution)

- **Work:** make sure one task runs a key at a time.
- **Bound:** one CAS on the key's own word to claim, and one store to release (plus a wake only
  if someone waits). About 2 × 20 cycles uncontended, **≈ 12 ns**.
- **Today (S3):** a shard `Mutex` at 4 times the cores, plus a `hashbrown` insert and remove:
  about 4 atomic operations and two probes, ≈ 40 to 60 ns. For a query that runs for
  microseconds that is under 3%. For an accessor-sized query (about 100 ns of work) it is
  **≈ 4× off** the bound and a third of the query.
- **Design:** a claim word in the memo row (free, claimed by a task, or claimed with waiters),
  claimed by a CAS. The shard table is used only to park waiters when the CAS finds the key
  claimed. That is the bound. It is the plane's single flight (`blaze_graph::pump::Flights`)
  for blaze-query, and db-salsa's claims by task for salsa, so it goes to that code when it next
  changes.

### Summary

| component | bound | today | off | removes the gap |
|---|---|---|---|---|
| memo hit | 2 lines, 5 ns hot | 7 lines, 18 ns | 3.3× | MemoRows (in progress), row base in the task cache, no slot reload for immortal keys |
| record a read | 4-byte store, 3 ns | node probe at finish, ≈ 15 ns | 5× | node in the memo row and in field/interned columns |
| edges at rest | 25 bits, 312 MB (nushell) | 4 bytes, 400 MB | 1.28× | bit-packed compaction |
| memo row | 24 bytes | 48 + 8 per sibling function | 2.3× | MemoRows, `u32` revisions, values of at most 8 bytes in the row |
| deep verify | 1 random load per edge; 13 ms DRAM-bound for all of nushell | ≈ 7 dependent loads, 400 ms | 30× | node-indexed color column and prefetch |
| intern hit | 2 lines | 2 lines | 1× | — |
| LRU scan | 8 µs per 1M rows | 4 ms | 32× (once per revision) | MemoRows plus a parallel scan: 50 µs |
| claim | 12 ns | 40 to 60 ns | 4× for tiny queries | claim word in the row |

None of these bounds is reached yet except interning. Every gap has a named change. The memo
row changes are `speed-salsa-next3`'s current work. The color column, the node in the row, the
task's row-base cache and bit-packed compaction were sent to it as the next changes, and the
claim word to the owners of claims.

## hir-ty's trait selection and trait-error leaf search (db-ty-port-infer-errors)

Files: `crates/hir-ty/src/next_solver/infer/{select,errors,traits}.rs`. Nothing was run: the
costs below are counted from the code and from `vendor/rustc_type_ir/src/search_graph/mod.rs`,
not measured. Unit costs used: a global solver-cache hit (canonicalize a small goal, about 60
instructions, plus one table probe of 2 lines) is about 30 + 9 = **≈ 40 cycles hot**. Unifying
an impl header with a goal is about 200 instructions, **≈ 100 cycles**. A small heap
allocation plus its free is **≈ 20 ns**.

### Obligations (`traits.rs`)

- **Work:** carry a goal plus its span and depth through fulfillment, one per nested goal.
- **Bound:** the information is a `Span` (3-bit tag + `u32` index), a `ParamEnv` and a
  `Predicate` (one 4-byte interned handle each), and a depth no larger than the crate's `u32`
  `recursion_limit`. Kept as separately addressable 4-byte fields: 8 + 4 + 4 + 4 = **20 bytes**.
  Packing the tag into the depth word would reach 17 but caps `recursion_limit` at 2^29, which
  rustc doesn't. So 20 bytes is the bound.
- **Today:** was 24 (`recursion_depth: usize`, 8 bytes plus alignment), **20% off**. Now `u32`
  and `const`-asserted at 20: **at the bound**. fulfill.rs's SoA depth column goes from 8 to 4
  bytes per pending obligation. This needs the fulfill.rs edit requested in
  `v2/requests/db-ty.md`.

### `select` (`select.rs`)

`find_matching_impl` calls it behind the `lookup_impl_method` salsa query, so it runs once per
distinct (method, args) pair. It also runs from method-probe ambiguity reporting, which is cold.

- **Work:** find which candidate proves `T: Trait` and the impl's args.
- **Bound:** the solver's global cache keeps a goal's result, not its candidates. So the root
  goal's candidate assembly must run once: one simplified-type impl lookup (2 lines). Then, for
  each of k candidates, one header unification plus n nested goals that hit the global cache.
  The proof-tree builder is noop below the root (`eval_ctxt/mod.rs`, `evaluate_goal` passes
  `ProofTreeBuilder::new_noop()`), so these do hit it. The chosen impl's args are rematched
  once. With typical k = 2, n = 3: 2 × 100 + 6 × 40 + 100 ≈ 540 cycles, **≈ 0.15 µs**.
- **Today:** exactly that evaluation (`visit_proof_tree` makes one uncached root evaluation),
  plus the proof-tree recording. That is a `Vec` of probe steps per probe, a `ThinVec` of
  original values, and in `inspect.rs`'s `candidates_recur` a `steps.clone()` per candidate plus
  the candidates `Vec`. That makes about 6 to 8 allocations, ≈ 0.15 µs, so **≈ 2× off**.
- **What stands in the way:** allocation in proof-tree recording, not the search. To remove it:
  - `InspectCandidate` keeps a range into one shared step buffer instead of a cloned `Vec<&ProbeStep>`.
  - The builder's step vectors come from a per-evaluation bump arena.

  These belong to inspect.rs (db-ty-port-solver) and the vendored builder (solver-sync). The
  first is requested. With both: ≈ 2 allocations, ≈ 0.19 µs total, **within 10%**. A fast path that
  first asks the cached `evaluate_root_goal` was considered and rejected: `select`'s caller is
  success-dominated, and on success it would evaluate the root twice.

### Trait-error leaf search (`errors.rs`, `BestObligation`)

This is cold: it runs only for an obligation that is reported as an error. A build with no
errors never runs it.

- **Work:** walk from the failing root goal to the leaf obligation to blame, at most
  `max_depth` = 10 levels.
- **Bound:** a level's nested trees are not recorded, so each level costs one proof-tree root
  evaluation of the goal (≈ 0.15 µs as above). Picking among the c candidates needs the results
  of only the nested goals whose source is `Normalization` (rustc's rule). Following the chosen
  candidate needs its n nested results. So per level: (1 + n + #normalization) evaluations. For
  c = 3, n = 3 and 1 normalization goal per candidate, that is 7 × 0.15 ≈ 1 µs a level, **≤ 10 µs
  per error**.
- **Today:** two gaps.
  - `non_trivial_candidates` builds the proof tree of every nested goal of every candidate when
    c > 1: (1 + c × n + n) = 13 evaluations a level, **≈ 1.9× off**.
  - `InferenceTable::has_trait_errors` (infer/unify.rs) runs the whole search for each pending
    error and `emit_trait_errors` runs it again, at least **2× more**.
- **What removes it:** both were requested in `v2/requests/db-ty.md`.
  - `InspectCandidate::instantiate_nested_goals_from(span, keep)` (inspect.rs) still advances
    every step's canonical state but builds trees only where `keep(source)`. errors.rs then
    passes `|s| s == GoalSource::Normalization`.
  - unify.rs converts each error once.

  With both, the search is at the bound.

| component | bound | today | off | removes the gap |
|---|---|---|---|---|
| obligation | 20 bytes | 20 bytes (was 24) | 1× | done; fulfill.rs column to `u32` requested |
| `select` | ≈ 0.15 µs (one root evaluation) | ≈ 0.3 µs | 2× | step ranges in inspect.rs, arena for the proof-tree builder |
| leaf search | ≤ 10 µs per error | ≈ 2 × 1.9× that | ≈ 3.8× (cold path) | source-filtered nested instantiation, one conversion per error |

State of the art: rustc's new solver (`rustc_trait_selection::solve::select`,
`fulfill/derive_errors.rs`) does the same work with the same per-candidate `Vec`s. The changes
above beat it on allocation and on the error path.

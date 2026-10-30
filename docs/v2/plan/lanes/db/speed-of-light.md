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

Costs are counted from the code as of 10-29 19:55, not measured: S1–S4, S5's edges and
claim-free verify, the 24-byte memo header and memos as columns are in the tree, none of it built
yet. The before/after and benchmarks are in [speed-salsa.md](speed-salsa.md). "Off" is today's
cost over the bound. A "line" is a dependent cache-line load: about 4.5 cycles from L1, 110 ns
from DRAM.

### Memo hit (`fetch`: value present, verified this revision)

The most frequent operation in a warm build.

- **Work:** find the memo of (function, key), compare `verified_at` with the current
  revision, take the value, and record the read.
- **Bound:** the memo's address is computable from the key, so the work is 1 line for the row
  (its base cached per task, since rows never move), 1 for the memo, and about 20 instructions
  (address arithmetic, the revision compare, a value-present test, the read's record: a compare
  with the last edge and a 4-byte store). Hot: 2 × 4.5 + 20 / 2 ≈ 19 cycles, **5.4 ns**. Cold:
  independent callers' misses overlap, about 20 in flight, so **≈ 5.5 ns** per hit per core in
  throughput, 220 ns latency.
- **Today (memos as columns, 19:55):** 6 lines, 5 deep:
  1. the `boxcar` bucket;
  2. the `Page`, which now keeps its ingredient's column slice beside its `Arc`;
  3. the slot (validation and the 4-byte row number), and the `Column` (the type check and
     the row page pointer share its first line: `repr(C, align(64))`, arrays inline, held by a
     `const` assertion), loaded in parallel since both depend only on the page;
  4. the row page entry;
  5. the memo.

  About 50 instructions. Hot: 5 × 4.5 + 25 ≈ 48 cycles, 14 ns, **2.5× off**. Cold: 5 dependent
  misses, 550 ns latency, **2.5× off**. Before columns: 7 lines, 7 deep (ingredient cache,
  bucket, `Page`, slot, the slot's memo table array, the `MemoEntry` pointer, the boxed memo),
  18 ns hot.
- **What stands in the way, and the change that removes it:**
  1. The bucket and the `Page`: two lines found again on every hit though neither moves once
     published.
  2. The row number, which lives in the slot, so the row page waits for the slot.

  The change: index a column's rows by the `Id`'s page and slot (a directory by table page of
  128-entry row blocks, made on the function's first memo in that page) instead of a number
  stored in the slot. `MemoTable` then holds nothing. Add a per-task cache of
  `(column, table page) → row block`, beside the node cache. A hit on a page the task has seen
  is then the row entry and the memo: 2 lines, with slot validation in parallel, which is the
  bound. Immortal interned keys (hir-def's IDs) already skip validation. It changes every caller
  of `memo_table_for` to pass the task's cache. Owner: speed-salsa, the first change after the
  first compile of S5, so that change is measured against a build that works.

### Recording a read

- **Work:** append the dependency to the task's frame, once per distinct key.
- **Bound:** a compare with the last edge (S4), else an L1-resident probe of the frame's set
  (about 10 cycles), and a 4-byte store of the dependency's node. **≈ 3 ns** for a new key, 1 ns
  for a repeat.
- **Today (S5):** the frame holds 12-byte `QueryEdge`s. When the query finishes, each edge
  becomes a node through a probe of the task's 1024-entry node cache (L1 on a repeat key) or of
  the shared node table (a lock-free probe: 1 to 2 lines, plus the key columns on a tag match).
  That is about 15 ns per distinct edge on a cache miss, **≈ 5× off** for keys the task hasn't
  seen.
- **What stands in the way:** the read doesn't know the dependency's node. The 24-byte header
  has no room for a 4-byte node (the origin word's span uses 61 bits). The change: a node
  column beside each column's row blocks (4 bytes per row, in the row block's line once rows are
  by page, above), set when the row's first memo is published, plus a node column beside input,
  interned and tracked struct slots. Then the frame records the node directly and finishing a
  query copies words with no probe. Owner: speed-salsa, with the row-by-page change.

### Edges at rest

- **Work:** remember, for every memo, the ordered list of what it read.
- **Bound:** an edge names one of N nodes plus its kind: ⌈log2 N⌉ + 1 bits. Nushell (#734) has
  about 100M edges; at N ≈ 10M nodes that is 25 bits, **≈ 312 MB**.
- **Today (S5):** 4-byte words in one paged array, 400 MB, **1.28× off**. Before S5 it was 8
  bytes an edge plus one allocation per memo, about 0.8 GB. With the node table (0.2 to 0.27 GB
  at 10M keys) the graph is 0.6 to 0.67 GB.
- **The change:** compaction (`Graph::compact`, with `&mut` at a new revision once the array is
  twice what is live) already copies every live range. Write the copy bit-packed at the width
  the largest node needs, read in place with a shift and a mask, unpacked 8 at a time with SIMD
  in deep verify. Fresh chunks stay 4 bytes until the next compaction. Settled edges are then
  within 1 bit of the bound (4% at 25 bits). It changes every edge reader (`EdgeSpan` decoding),
  so it comes after the first compile. Owner: speed-salsa.

### Memo at rest

- **Work:** what a memo must keep: `verified_at`, `changed_at`, durability and flags, its value,
  its edge range.
- **Bound:** with revisions as `u32` (enough for the daemon's lifetime) and the edge length in
  16 bits: 4 + 4 + 1 + 4 + 2 ≈ **16 bytes** plus the value. The 16-byte value fingerprint
  belongs in a side table, only for memos that have one.
- **Today (19:55):** the 24-byte header and the value in its column's slab, no box; an 8-byte
  row pointer in its own column only; a 4-byte row number per slot, which fits in padding in all
  three struct kinds. For an 8-byte value that is 40 bytes against 24, **1.67× off**. Before:
  48 bytes boxed, 8 per sibling function and a 16-byte table per slot, 2.3× and more.
- **What stands in the way:**
  1. `Revision` is a `usize`, 8 bytes in `verified_at` and in the stamp. Narrowing it to `u32`
     touches every revision compare in salsa; it saves 8 bytes per memo.
  2. The row pointer. A replaced memo can't be overwritten in place because `&'db` references
     to it live for the whole revision, so the new memo takes a new slot and the row points at
     it. Removing the pointer needs memos written in place, which needs readers to copy small
     values out instead of borrowing them: a change to every `returns(ref)` caller in the front
     end. Not worth it while the pointer is 8 bytes against values that are mostly larger.

  Owner of 1: speed-salsa, after the first compile.

### Deep verify (a warm build after an edit)

- **Work:** for each memo that may have changed, walk its edges in read order and check each
  dependency's color; stop at the first that changed.
- **Bound:** per edge, one sequential 4-byte edge load and one random load of the dependency's
  color. An edge list's colors are independent loads, so all can be issued before they are
  checked in order. All of nushell's 100M edges touch 100M random lines: 6.4 GB / 480 GB/s =
  **13 ms** on all of Turin's cores if the colors are in DRAM. With a dense color column of 8
  bytes per node, 10M nodes are 80 MB, much of it in L3; the bound is then about 100M × 6
  instructions / (2 IPC × 192 × 3.5 GHz) ≈ **0.5 ms**. An edit's real walk is only what is
  reachable from the change.
- **Today (S5, claim-free verify, columns):** per edge, the node's key (a page load and two
  column loads: 3 lines), then the dependency's hit path (5 deep), all dependent: ≈ 8 × 110 ns
  = 880 ns per edge cold. All 100M edges on 192 cores take ≈ 460 ms, **≈ 35× off** the DRAM
  bound.
- **What stands in the way:** the color lives in the memo, which is found through the key.
  rustc keeps it in `DepNodeColorMap`, a dense array by node. The change: a node-indexed column
  of `verified_at: AtomicU32` and `changed_at_and_durability: AtomicU32`, written wherever the
  memo's are (publish, `mark_as_verified`, an input's setter, interning, backdating). Deep
  verify then reads edge words and colors only, prefetching the next 8 edges' colors as it checks
  one, and goes to a memo only for a node not verified this revision. That is 1 random load per
  edge, the bound. A color written late or missed is a wrong incremental result, not a slow
  one, so it is tested differentially against the memo path (every verify run both ways in
  debug builds) before the memo path is dropped. Owner: speed-salsa, after the first compile.

### Interning a hit (`intern_id`, every multi-argument query call)

- **Work:** hash the key and find its value.
- **Bound:** FxHash of a 2- to 3-word key (≈ 3 cycles), then 1 line for the group (control word
  and pointers together) and 1 line for the value, to compare the key. **≈ 2 lines, ≈ 15 cycles
  hot.**
- **Today (S2):** a table pointer (L1), one 64-byte group line with the control word and seven
  pointers, and the value line. **At the bound.**

### LRU eviction (S1, once per revision per `lru` function)

- **Work:** rank the memos that hold values by `verified_at`, and drop the oldest beyond the
  cap.
- **Bound:** 4 to 8 bytes of `verified_at` per row: 1M rows are 8 MB, 0.27 ms on one core, or
  **17 µs** at all-core bandwidth.
- **Today:** a walk of the struct's slots in page and slot order (the row number, sequential),
  the row entry (8 bytes, mostly sequential) and the memo's line (64 bytes, in slab order):
  about 100 bytes per row, 100 MB per 1M rows, 3.3 ms on one core, **12× off**. It runs once per
  revision with `&mut`, so no reader waits.
- **What stands in the way:** reading the slab directly would cut it to the header's 8 bytes,
  but the slab's order depends on scheduling, and the LRU's ties must break the same way in every
  run (determinism). The change: split the walk by table page across the plane's workers and
  merge the candidates in page order: 100 MB / 480 GB/s ≈ 0.2 ms on all cores. That is still 12×
  the bound in bytes, which stays: reaching it needs `verified_at` in a column of its own, a
  second line on every hit, and the hit runs millions of times per revision against this scan's
  once. Owner: speed-salsa.

### Claims (single flight, once per execution)

- **Work:** make sure one task runs a key at a time.
- **Bound:** one CAS on the key's own word to claim, one store to release, plus a wake only if
  someone waits: about 2 × 20 cycles uncontended, **≈ 12 ns**.
- **Today (S3):** a shard `Mutex` at 4 times the cores plus a `hashbrown` insert and remove:
  about 4 atomic operations and two probes, ≈ 40 to 60 ns. Under 3% of a query that runs for
  microseconds; for an accessor-sized query (about 100 ns of work) **≈ 4× off** and a third of
  the query.
- **The change:** a claim word per row (free, claimed by a task, claimed with waiters) beside
  the row's entry in the row block, claimed by a CAS; the shard table only parks waiters. That is
  the bound. It sits with the row-by-page change, since that is where the word lives. Owner:
  speed-salsa; db-salsa owns cycle handling, which reads claims.

### Summary

| component | bound | today | off | removes the gap |
|---|---|---|---|---|
| memo hit | 2 lines, 5 ns hot | 6 lines (5 deep), 14 ns | 2.5× | rows by page plus a per-task row-block cache |
| record a read | 4-byte store, 3 ns | node probe at finish, ≈ 15 ns | 5× | a node column beside the rows and the struct slots |
| edges at rest | 25 bits, 312 MB (nushell) | 4 bytes, 400 MB | 1.28× | bit-packed compaction |
| memo at rest | 16 + value | 24 + value + 8 | 1.67× (8-byte value) | `u32` revisions |
| deep verify | 1 random load per edge; 13 ms for all of nushell | ≈ 8 dependent loads, 460 ms | 35× | a node-indexed color column with prefetch |
| intern hit | 2 lines | 2 lines | 1× | — |
| LRU scan | 17 µs per 1M rows (all cores) | 3.3 ms (one core) | 12× in bytes, once per revision | a parallel scan by page, merged in page order: ≈ 0.2 ms |
| claim | 12 ns | 40 to 60 ns | 4× for tiny queries | a claim word in the row block |

Only interning is at its bound. Every other gap has a named change, all owned by speed-salsa,
and all wait for one thing: the first compile of S5. Each rewrites a path S5 already rewrote
(the hit, the edge readers, every revision compare), and stacking a second unbuilt rewrite on the
first would leave the first compile's errors with two possible causes. Order: rows by page with
the task cache, node column and claim word (one change, the hit path and recording); the color
column (deep verify); bit-packed compaction; `u32` revisions; the parallel LRU scan.

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

## hir-def signatures: `signatures.rs` (speed-hir-defs-sigs)

What this file does:
- lowers every item's signature (`StructSignature` through `TypeAliasSignature`),
  `VariantFields` and `EnumVariants` from the item's own outline tree (`SignatureSyntax`);
- moves the resulting source map back to the file on request;
- answers every read of a signature.

Costs below are estimates from the code as of 10-29 20:00, not measurements. `proof`'s budgets
measure them once things compile. The store and generic params it lowers into are
speed-hir-store's, and the per-query costs are salsa's (section above). Only what this file adds
on top of those is counted against it.

The sizes used, for a typical item:
- **A fn's outline** (signature, body left out): 40 elements and 120 bytes of text. Its
  flat-tree words are 4 bytes each, so 160 B of words plus 120 B of text: **280 B**.
- **A 20-method impl:** about 820 elements and 4 KB of text, of which the impl's own outline is
  about 15 elements.
- **A file:** 150 items.

### Reading a signature (`X::of`, a memo hit)

- **Work:** return the memo's value.
- **Bound:** one salsa memo hit, 5.4 ns hot (above). This file adds nothing to it.
- **Today:** `returns(deref)` hands out `&X` with no `Arc` clone, so this file's part is at the
  bound. The hit itself is 18 ns, all of it salsa's (MemoRows and the row-base cache take it to
  the bound).

### `signature_syntax`: once per item per edit of its file

- **Work:** decide whether the item's outline changed, and keep a copy of it when it did.
- **Bound:**
  - read the outline from the file's tree (280 B) and write a copy (280 B);
  - compare it with the old copy when salsa compares the values (read 2 × 280 B);
  - that is 1.1 KB through L1/L2 at about 50 GB/s: **22 ns**;
  - one allocation for the copy and its maps, about 10 ns;
  - the outline's few ast ids and span anchors, about 10 ns;
  - salsa's share of one execution at its bound: claim 12 ns, 4 reads × 3 ns, publish about 10
    ns, so about 35 ns.

  Total **≈ 80 ns per fn**.
- **Before this pass, ≈ 1.0 µs, 13× off:**
  - three memo hits (span map, ast id map, source) plus a walk to the node: 85 ns;
  - finding the anchor: 20 ns;
  - `clone_subtree`, which allocates about 10 columns and copies the whole item: 110 ns, plus
    the whole parsed body when the fn's body was lowered (`subtree` copies parsed lazy parts),
    and an impl's or trait's whole item list;
  - `replace_subtree` repacking the copy, with its own 10 columns: 185 ns;
  - the empty node's tree and the text copy: 65 ns;
  - **two SipHash-1-3 passes over the tree** (`tree_hash`): 2 × (300 B × 1.5 cycles + 50) ≈
    1,000 cycles, **285 ns**;
  - `relative_without` walk: 77 ns;
  - `within`: 30 ns;
  - salsa's execution today: 140 ns.
- **Applied now:**
  - The hash is gone. `SignatureSyntax` compares its tree by content (`FlatTree: PartialEq`),
    which reads each byte once, about 15 ns. It runs only when salsa compares a re-run's value,
    and it is exact where a 128-bit hash could collide and reuse a stale signature.
  - Result: ≈ 0.75 µs, **9.5× off**.
- **Requested** (frontend-text, `v2/requests/frontend-text.md`, "clone_subtree_without, one
  pass"):
  - make the outline copy in one pass over the elements outside the elided node, never
    reading or copying the elided part;
  - that replaces the clone, the repack, the empty tree and the text copy (360 ns) with about
    120 ns, giving ≈ 0.5 µs, **6.5× off**;
  - for a fn whose body was lowered, or a large impl, it also removes the cost that grows with
    the body or the item list (about 1.5 µs for a 20-method impl).
- **What stands in the way after that:**
  - **About 25 allocations per `SignatureSyntax`, ≈ 250 ns:** the tree's 10 columns, the ast
    id map's and span map's vectors, and the `Arc`. Reaching the bound's one allocation means
    building all three into one block. That is a layout change in `FlatTree`, `AstIdMap` and
    `RealSpanMap` (frontend-text, span), or a per-revision bump arena for memo values (salsa).
  - **Salsa's execution cost, 140 ns → 35 ns:** speed-salsa's claim word and node-in-row
    changes.
  - **The structural gap, below:** every item of an edited file runs at all.

### An edit of a file: what signatures re-run

- **Work:** a body edit changes no signature, and an outline edit changes one item.
- **Bound:** for a body edit, establish that no outline changed. That is one pass comparing the
  file's outline with the old one: 150 × 280 B = 42 KB read twice, 84 KB / 50 GB/s =
  **1.7 µs**, plus one salsa execution, so **≈ 2 µs per file**. For an outline edit, add one
  item's `relative` lowering.
- **Before the relative job:** every signature, `VariantFields` and `EnumVariants` of the file
  lowered again: 150 × about 2 to 3 µs of lowering, the signature, its fields and its
  variants each re-run and backdated, ≈ **300 to 450 µs**, about 200× off.
- **Today:** each item's `signature_syntax` runs and backdates, and nothing lowers. 150 ×
  0.75 µs ≈ **110 µs**, 55× off. After `clone_subtree_without`: 75 µs, 37× off.
- **What stands in the way:** each item's syntax query reads the file's parse, so salsa runs all
  150 of them on any edit of the file, even when every result is unchanged.
- **The design that removes it:**
  - A `file_outline(db, HirFileId)` query in this file: the file's tree with the body of every
    fn, const and static that is its own anchor left out, and its ast id map and span map.
  - It is made in one pass by the list form of `clone_subtree_without`,
    `AstIdMap::relative_without` and `RealSpanMap::without`, all requested from frontend-text.
  - It is compared by content, so it backdates on every body edit. Positions in it don't move
    when a body's length changes, because bodies aren't in it.
  - `signature_syntax` then reads `file_outline` instead of the parse. On a body edit, one query
    runs (≈ 2 µs, the bound) and each item's memo costs a deep verify of one edge (≈ 5 ns at
    salsa's bound, 0.75 µs for the file).
  - Bodies of items anchored at an enclosing item stay in the outline. Cutting them would move
    their neighbours' anchor-relative offsets.
  - This is the next change once the list APIs land.

### `X::relative`: lowering a signature (cold, or when its outline changed)

- **Work:** one pass over the outline's elements, writing the signature, its store and its
  source map.
- **Bound:**
  - about 10 instructions per element to dispatch and record: 40 × 10 = 400 instructions;
  - writing about 400 B of output;
  - 1 to 2 allocations;
  - so about 250 cycles, **≈ 70 ns per fn**, plus salsa's 35 ns.
- **Today, ≈ 1 to 2 µs, ≈ 15× off:**
  - this file's part, the flags and keywords, is at its bound: branch-free shifts and masks,
    and one pass over the kind column;
  - the rest is speed-hir-store's: the collector's setup (two memo hits for the def map, a
    pooled builder), the store's tables and their roughly 10 allocations;
  - plus salsa's 140 ns, and the `AttrFlags` and lang-item memo hits (2 to 3 × 18 ns).
- **What stands in the way:**
  - the store's allocation count and setup (speed-hir-store's sorted tables);
  - salsa's execution cost.

  Neither is in this file.

### `X::with_source_map`: moving a map to the file (per edit, for maps someone asked for)

- **Work:** rewrite each pointer of the map by a shift.
- **Bound:** a map of N entries of 16 B, read and written: for 30 entries, 1 KB, about 20 ns,
  plus 1 allocation.
- **Before:** `loc.source(db)` walked the file's tree to the item's node to read its range,
  about 30 ns plus a memo hit. Then `Move` called a `dyn Fn` once per pointer and built about 6
  new vectors: ≈ 150 to 200 ns, about 8× off.
- **Applied now:** the item's file range is read off its ast id pointer (`file_range`), with no
  walk to the node.
- **What stands in the way:**
  - `Move`'s per-pointer `dyn Fn` and per-table allocations (speed-hir-store's type). A shift
    is the same for every pointer outside the elided node, so `moved` could take
    `(at, removed, base)` and rewrite each table with one SIMD add over its ranges. That is a
    change to `Move`, to be asked of speed-hir-store if `proof` measures this above 10% of an
    edit's signature cost.
  - This query runs only for maps someone holds (`lru = 1024`), so it is not on the cold path.

### Summary

| component | bound | before | today | off today | removes the rest |
|---|---|---|---|---|---|
| read a signature | 1 memo hit | 1 memo hit | 1 memo hit | 1× (this file's part) | salsa's hit path |
| `signature_syntax` per item | ≈ 80 ns | ≈ 1.0 µs | ≈ 0.75 µs | 9.5× | `clone_subtree_without` (6.5×), one-block layout, salsa execution cost |
| signatures re-run per body edit, 150-item file | ≈ 2 µs | ≈ 300–450 µs (all lowered) | ≈ 110 µs (none lowered) | 55× | `file_outline` over the list APIs |
| lowering one fn signature | ≈ 70 ns | ≈ 1–2 µs | same | ≈ 15× | store allocations (speed-hir-store), salsa |
| moving a source map | ≈ 20 ns | ≈ 200 ns | ≈ 150 ns | 7× | `Move` as one shift (speed-hir-store), only if measured |

The benchmark for the whole is rustc's `collect` queries plus its red-green re-verification.
rustc re-verifies an unchanged item's `fn_sig` by one color check per dependency. It needs no
per-item re-run because its HIR is hashed per owner in one pass, which is what `file_outline`
is.

### Memory ([memory.md](../../design/memory.md)) for these files

`blaze-mem` does not exist yet, so nothing here is written against it. Where each remaining
general-purpose allocation goes once it does:

- `select`'s nested obligations (`to_selection`) and `ImplSource::map`'s buffers never escape the
  task: `find_matching_impl` registers them in an `ObligationCtxt` and keeps only the impl and its
  args. They go to the **frame allocator** (`ImplSource<'db, N>` takes a frame-backed vector).
- The leaf search's `parent_trait_obligations` and `FulfillmentError` live until
  `SolverDiagnostic::from_fulfillment_error` converts them within the same inference task. They
  go to the frame allocator too. An edit that keeps an error re-runs this search, so it is part of
  the warm loop, not exempt as an error path.
- Proof-tree step vectors and per-candidate step lists (inspect.rs and the vendored builder) are
  the bulk of `select`'s allocations; with the step-range change above they become one frame
  buffer per evaluation.
- `Obligation` itself is a 20-byte `Copy` value with no heap part.

## blaze-query: the v2 query database (db-query)

Files: `crates/blaze-query/src/{lib,memo,table,segment,image}.rs`. Nothing was run. Costs are
counted from the code as of 10-29 against the machine figures at the top of this file, not
measured; `blaze-query`'s benchmarks check them once the tree builds. The design and the
benchmark each piece is held to (rustc's red-green graph, salsa, papaya, folly's
`ConcurrentHashMap`, Click's `NonBlockingHashMap`, bump arenas, mimalloc) are in
[query-database.md](query-database.md). Rows below are 64-byte lines; "dependent" means a load
whose address comes from the one before it.

### Warm hit (`Cx::get` or `Pinned::get` of a key the view resolved)

The most frequent operation: every read of a query inside a query, on every warm build.

- **Work:** from a `QueryKey` (a content hash, so it is never hashed again), find the memo the
  view resolved and the address of its value's bytes.
- **Bound:** a value is shared by every view and moved by eviction, refill and cold writes, so
  where it lives must be in one place (the memo), not copied into each view's result. Readers
  hold `&[u8]` for their whole task, so rows can't move, and a result must be found through a
  table of places. That leaves 3 dependent lines: the key's place, the view's result row, and
  the memo's line. About 35 instructions (tag, probe, a 34-byte key compare, the value check).
  - Hot: 3 × 4.5 + 35 / 2 ≈ 31 cycles, **≈ 9 ns**.
  - Cold: 3 × 110 = **330 ns** latency, 3 × 5.5 = 16.5 ns throughput when callers' hits overlap.
- **Before this pass:** 4 dependent lines (plus the value's row, read for its address), each row
  load after a dependent load of its chunk's base, and the shard's table header. Cold:
  4 × 110 + 14 ≈ 455 ns, **1.4× off**. Hot: 8 dependent L1 loads + ~45 instructions ≈ 58
  cycles, 17 ns, **1.8× off**.
- **Applied now:**
  - The memo's line (`MemoHot`, 64 bytes, line-aligned) holds a seqlocked copy of the value's
    address and length, rewritten after every swap of its value word. A hit reads the memo's
    line and never the value's row. That is 3 lines, the bound.
  - A shard's table pointer carries log2 of the table's length in its top 6 bits, so a probe
    goes from the pointer straight to the place, with no header line.
  - Rows of a multiple of 64 bytes are line-aligned, so a result or memo is one line, never two.
  - Cold: 3 × 110 + 14 (the shard's pointer, in L2) ≈ 344 ns, **1.04× off: within 10%**.
  - Hot: 6 dependent L1 loads (shard pointer, place, the result chunk's base, result, the memo
    chunk's base, memo) + ~45 instructions ≈ 50 cycles, 14 ns, **1.6× off**.
- **What stands in the way (hot only):** the two chunk-base loads (each `Rows` keeps 27
  chunks, found by the row's index) and the kind's `FxMap` probe (`kind_of`, about 15 cycles).
  - A reserved region per column (`blaze_mem::Region`: address space taken once, committed as
    it grows, never moved) makes a row's address base + index × size. The base is a field,
    loaded off the critical path, so the chain becomes the bound's 3 lines plus the shard
    pointer. `blaze-mem` is not a workspace dependency yet: requested
    (`v2/requests/db-query.md`, 3).
  - `kind_of` checks the kind on every hit only to name a query asked under another's kind; a
    `[u32; 65536]` index by `QueryKind` would make it one L1 load. That is about 10% of a hot
    hit, so it is done with the region change.
  - With both: ≈ 4 × 4.5 + 40 / 2 ≈ 38 cycles, **1.2× off** hot. The remaining shard pointer
    can be folded in only by giving up sharded writers.
- `Q::input(key)` (hashing a typed key into its `QueryKey`) is the query's own cost and is
  counted with it. A query that asks with a key it already has as a `QueryKey` pays nothing.

### Recording a read (`Cx::record`)

- **Work:** append a 56-byte edge (the key and the fingerprint it saw) to the running task's
  edge stack.
- **Bound:** one 56-byte store into L1, ≈ 4 cycles.
- **Today:** a thread-local access, a `RefCell` flag set and cleared, and a `Vec` push: about
  12 instructions, ≈ 8 cycles. The `RefCell` flag is about 3 of them. A record always comes
  with a hit or a run, so against the whole read (a 31-cycle hit plus the record) that is
  **≈ 8% over**, within the bar.

### Making a memo (a miss's bookkeeping, apart from the query's own work)

- **Work:** copy the edges (n × 56 bytes) and the value (v bytes) once, write the memo's rows
  (64 hot, 48 cold, a 64-byte value row and a 4-byte generation), and publish into the slot
  map, the view's results and `born`.
- **Bound:** for n = 8 and v = 256, 884 bytes written to L1 at 64 bytes a cycle ≈ 14 cycles;
  3 publishes, each at least one atomic read-modify-write to install lock-free (≈ 20 cycles
  uncontended); ≈ 100 instructions at 2 IPC. Total ≈ 14 + 60 + 50 ≈ **125 cycles, 36 ns**.
- **Before this pass:** a 4 KiB `Writer` allocation per run (≈ 20 ns, plus first-touch page
  faults), the store's writer lock (a CAS and a release), a `fetch_add` on each segment's hold
  count (2), and a `fetch_add` on the row's generation. That is ≈ 330 cycles, **2.6× off**.
- **Applied now:**
  - Each thread reuses up to 8 value buffers (`Writer::with_buffer`), so a run allocates
    nothing once they have grown.
  - Each lane's free lists are its own: no lock. Retired value rows are reclaimed by the lane
    that retired them, as it writes; `&mut` reclaims every lane's.
  - An open segment's hold count carries a bias (`OPEN`); its lane counts the spans it hands out
    in a plain field and settles the count once, at seal. A copy does no atomic operation.
  - A row's generation is a load and a store by its one claimer.
  - So `Store::make` does no atomic read-modify-write. The 3 publishes are each a shard lock
    (a CAS) and a release store. Today ≈ 14 + 4 (the value copied once more, from the
    buffer) + 3 × 25 + 150 / 2 ≈ 170 cycles, 48 ns, **1.35× off**.
- **What stands in the way:**
  - `born`, a whole map published per memo only so `collect` can tell a key added while it
    walked. Stamping the generation in the memo's cold row instead (48 bytes with 6 free) drops
    a publish and its ~40 instructions. A key that resolves to an equal memo it already had
    must restamp that memo. With that: ≈ 130 cycles, **1.04×**. This is the next change to this
    crate.
  - The value is written into a buffer and then copied into the segment. Writing straight into
    the segment would need `blaze_format::Writer` over a borrowed `&mut [u8]` that can grow in
    place. That is 4 cycles of 170, under 3%, so it is not worth a new `Writer`.
  - `add_memo` compares the new memo with the key's existing ones (`Store::same`). That is work
    the bound needs (dedup), proportional to the memos kept for the key.

### Verifying a memo (a warm build after an edit)

- **Work:** walk the memo's edges in order; for each, find the dependency's current fingerprint
  in the view and compare; stop at the first that differs.
- **Bound:** per edge, the record streams in sequentially (56 bytes at 30 GB/s ≈ 1.9 ns, and
  prefetched by the hardware). The lookup is 2 dependent lines (place, result row): a
  dependency is named by its content key, so it must be found in a table. Edges are
  independent, so lookups overlap up to the core's 20 misses in flight: 2 × 5.5 ≈ **11 ns per
  edge** from DRAM. Hot: ≈ 30 instructions, **≈ 15 cycles** an edge.
- **Before this pass:** 3 dependent lines per edge (the result's memo ID, then the memo's line
  for its fingerprint). Out-of-order execution overlaps only as many edges as the reorder buffer
  holds, about 8 at ~55 instructions an edge: 3 × 110 / 8 ≈ 41 ns, **3.7× off**.
- **Applied now:**
  - A view's result (`Resolved`) holds the memo's fingerprint beside its ID: the row is
    exactly one line (34-byte key, 4-byte ID, 16-byte fingerprint), and an edge's check reads
    only it.
  - In a database past `PREFETCH_FROM` memos (2^16, about 8 MB of rows: past L2), verification
    prefetches the place of the edge 8 ahead and the result row of the edge 4 ahead, so about
    16 misses are in flight instead of 8.
  - Today: 2 × 110 / 16 ≈ 14 ns per edge, **1.25× off**; hot (a small database) ≈ 15 cycles,
    at the bound.
- **What stands in the way:**
  - The window (8 and 4) is set from the figures above, not measured. With 20 misses in flight
    and 2 per edge, a window of 10 reaches the bound; `proof`'s budget sets it.
  - The stronger bound needs dense key IDs: a view's results become a column indexed by a
    `u32` key ID, so an edge is 1 line (≈ 5.5 ns) and 24 bytes. That is the redesign in
    query-database.md ("To match: intern query keys"). It is 2× today's bound and changes the
    image's layout, so it is a change of its own, after the tree builds and this one is
    measured.

### Collection (`collect`, between edits, concurrent with readers)

- **Work:** mark every memo reachable from the open views and the kept keys (each reached memo's
  edges, and each edge's slot), sweep every slot, and free what is past its grace.
- **Bound:** memory-bound over what it touches. For nushell's scale (M = 10M reached memos,
  E = 100M edges): 64 M + 56 E for the memos and edges plus 2 random lines per edge for its
  slot. Random lines at all of Turin's cores: 200M × 110 ns / (20 × 192) ≈ **6 ms**. On one
  core: 200M × 5.5 ns ≈ 1.1 s.
- **Before this pass:** single-threaded, with `reached` and `hot` as hash sets of 10M entries
  (≈ 160 MB each, so every insert and test a DRAM miss): ≈ 3 random lines per edge-step and 2
  more per memo. ≈ 1.8 s, **300× off** the all-core bound.
- **Applied now:** the marks are bitsets indexed by memo ID (`Marks`, 1 bit a memo: 1.25 MB
  for 10M memos, which stays in L2/L3), so marking costs no miss. ≈ 1.1 s on one core, at the
  one-core bound but **180× off** the all-core one.
- **What stands in the way:** it runs on one thread. Marking parallelizes as rustc's and every
  tracing collector's does: split the roots, and have each worker set bits with `fetch_or` on
  the shared bitset, so a memo is taken once. The sweep over slots splits by shard. The
  workers must be the plane's (no thread of the database's own), and `collect` is called from
  the daemon outside any task. Asked of the owner of `blaze_graph`'s plane: a way to run a batch
  of closures as plane tasks and wait for them from outside a task
  (`v2/requests/db-query.md`, 4). `collect` never blocks a reader (reads take no lock) and runs
  between edits, so it is off an edit's critical path while it is slow.

### Cold pack (`write_cold`, then the mapped completion)

- **Work:** copy V bytes of cold values into one file, write it, map it back, check what was
  mapped, and swap each value's word to the mapping.
- **Bound:** the write, V / 7 GB/s on NVMe. One write and one map, each a single I/O op through
  the core. Each value's content ID (`Stored::Disk` names it) is one hash pass: V at about
  4 GB/s a core. The mapped check is a `memcmp` against the copy still in memory, at 30 GB/s.
- **Before this pass:** the check hashed every value again: a second pass over V at 4 GB/s,
  **≈ 2× off** in CPU.
- **Applied now:** a value whose word still names the copy written is compared byte for byte.
  Only one swapped out since (evicted, or moved by another write) is hashed again.
- **What stands in the way:** the check reads the whole mapping back, page by page, faulting in
  4 KiB pages: at ~1 µs a fault, V / 4 KiB µs (≈ 260 ms per GB) against 33 ms for the
  `memcmp` itself. A map that populates its pages (`MAP_POPULATE`, or huge pages) would remove
  it. That is the core's `Effect::Map`, and it is to be measured before it is asked for.

### Writing the image (`write_image`, once per cold build, before `settle`)

- **Work:** every key's memos, sorted by key and by memo, with their edges and values, written
  as one file.
- **Bound:** the bytes, M × (192 + 56 × edges + value) at 7 GB/s; for 10M memos of 10 edges and
  256-byte values, 10 GB, ≈ 1.4 s. Before it, the sort of the keys: as a radix sort on the
  key's leading 8 bytes (uniform, since keys are content hashes), ≈ 0.3 s on one core. Total
  **≈ 1.7 s**.
- **Before this pass:** a `FxSet` of every key (10M random inserts, ≈ 0.3 s), a comparison sort
  of the keys (≈ 10M × 23 × 3 ns ≈ 0.7 s), and a lock of the store's code table per memo for its
  schema and producer (≈ 0.2 s). ≈ 2.6 s, **1.5× off**.
- **Applied now:** the keys are pushed into a `Vec`, sorted and deduplicated (no set), and the
  code table is snapshotted once (taken again only when reading a key in from the image adds a
  code). ≈ 0.7 + 1.4 ≈ 2.1 s, **1.24× off**.
- **What stands in the way:** the comparison sort. A radix sort on the keys' leading bytes
  takes it to the bound. It runs once per cold build, off the warm loop, so it comes after the
  hit, verify and miss paths are measured.

### Summary

| component | bound | before | today | off today | removes the rest |
|---|---|---|---|---|---|
| warm hit, cold | 3 lines, 330 ns | 4 lines + headers, 455 ns | 344 ns | 1.04× | — |
| warm hit, hot | ≈ 31 cycles | ≈ 58 cycles | ≈ 50 cycles | 1.6× | reserved regions for rows (requested), kind index |
| record a read | 4 cycles | 8 | 8 | 8% of the read | — |
| make a memo | ≈ 125 cycles | ≈ 330 | ≈ 170 | 1.35× | `born` stamped in the cold row |
| verify, per edge | 11 ns (2 lines, overlapped) | 41 ns | 14 ns | 1.25× | prefetch window tuned; dense key IDs for the 1-line bound |
| collect | 6 ms all-core (nushell) | 1.8 s | 1.1 s | 180× (off the edit path) | parallel marking on the plane (requested) |
| cold pack check | `memcmp`, 30 GB/s | a second hash | `memcmp` | ≈ 1× in CPU; faults dominate | populated maps, once measured |
| image write | ≈ 1.7 s (10 GB) | ≈ 2.6 s | ≈ 2.1 s | 1.24× (once per cold build) | radix sort of the keys |

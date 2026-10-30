---
status: plan
summary: frontend-fast-trait — trait goals answered before canonicalization (a per-revision global-goal table keyed by packed IDs), a shape table for Copy, Clone and auto traits over a per-ADT verdict, and the persistent goal cache by content; the bound, today's cost, the design, and why each keeps rustc's answers.
date: 2026-10-29
---
# frontend-fast-trait: trait goals

Section 2 of [fast.md](fast.md). Code: `crates/hir-ty/src/next_solver/global_goals.rs` and
`global_goals/` (the global-goal table, the shape table), `crates/hir-ty/src/next_solver/goal_store.rs`
and `goal_store/` (the persistent cache, child `frontend-fast-trait-store`), cases in
`harness/v2/frontend/cases/trait/` (child `frontend-fast-trait-cases`). The hooks in files of lane
db (`solver.rs`, `fulfill.rs`, `interner.rs`, `interner/tls_cache.rs`, `next_solver.rs`,
`infer/mod.rs`) are requests in `v2/requests/db-ty.md` under `## frontend-fast-trait: ...`.

Nothing here was run. Costs are counted from the code with the machine of
[../db/speed-of-light.md](../db/speed-of-light.md): about 3.5 GHz, L1 in 4 to 5 cycles, L2 in 14,
L3 in 50, branchy code at 2 instructions a cycle.

## The work and its bound

A body's inference asks a few hundred goals. Most are `Sized`, `Copy`, `Clone`, auto traits and
impl lookups whose self type has no inference variables, and the same goals recur across bodies
(`T: Sized`, `String: Display`, `Vec<u8>: Clone`).

- **A goal answered already in this revision** must still be found and its reads recorded on the
  asking query. Finding it: the goal's identity is already two hash-consed rows (the predicate,
  the param-env's clause list) and the typing mode's tag, so the key is a register op: two row
  flag loads (whether the goal is global, both rows hot in L1/L2), a shift and an or. One probe
  of an open-addressed table: a multiply and shift to the slot (4 cycles), one line holding the
  key and the answer (4 to 14 cycles). **Bound: about 20 cycles, 6 ns**, plus recording the reads.
  Recording the reads is salsa's `replay_reads`: a capture's edges are appended to the asking
  query, about 5 cycles an edge. A goal reads tens of edges, so replay is the larger part of a
  hit; its own bound (one anonymous node, as rustc's `read_index` of a dep node) is salsa's, not
  this file's (lane db, [../db/speed-salsa.md](../db/speed-salsa.md)).
- **A `Copy`/`Clone`/auto-trait goal on a type whose structure decides it** costs a `match` on
  the type's kind tag and, for an ADT, one memoized verdict: a salsa memo hit, about 19 cycles
  ([../db/speed-of-light.md](../db/speed-of-light.md), "Memo hit"). **Bound: about 25 cycles,
  7 ns.**
- **A goal answered in an earlier run or another crate's build** costs one store probe and the
  check that every fact it read still has its hash: a few fact lookups (each a memo hit) and one
  read of a mapped record. **Bound: about 1 µs**, against the microseconds to milliseconds of a
  solve.

## What the code does today (2026-10-29)

Counted from `fulfill.rs`, `solver.rs`, `interner/tls_cache.rs` and the vendored solver.

- `FulfillmentCtxt::register_predicate_obligation` tries `compute_goal_fast_path`
  (`solver.rs`: trivially sized types, trivially pure `Copy`/`Clone`, a few region and WF
  shapes) and otherwise pushes the obligation.
- Each round, `try_evaluate_obligations` calls `evaluate_root_goal` for every pending
  obligation not stalled. That builds a root `EvalCtxt` and a `SearchGraph`
  (`EvalCtxt::enter_root`), canonicalizes the goal (a fold over the predicate and the param
  env with the canonicalizer's state reused, then interning the canonical input: about 300 to
  600 cycles for a goal of five to ten type nodes), probes the search graph's `GlobalCache`
  through `tls_cache::shared` (the scope lookup, about 45 cycles, then FxHash of the canonical
  input and a probe, about 30), replays the reads, and instantiates the canonical response back
  into the inference context (about 200 cycles). **A hit costs about 600 to 1,000 cycles,
  30 to 50 times the bound.**
- Within a goal, each nested goal goes the same way: `add_goal` tries the delegate's fast path,
  then canonicalizes and probes the search graph.
- `Send`, `Sync`, `Unpin`, `Freeze` and `Copy`/`Clone` of an ADT have no fast path: each is a
  full evaluation of the auto-trait candidate (constituent types, one nested goal each) or of
  the impl candidate.
- Nothing outlives the revision: the next run, or the next crate in a fresh process, solves
  `String: Display` again.

## The design

### 1. The global-goal table, before canonicalization (`global_goals.rs`, `global_goals/table.rs`)

**Which goals.** A goal is *global* when its predicate and param env have no inference
variables (`HAS_INFER`), no placeholders (`HAS_PLACEHOLDER`), no bound variables escaping, no
free local regions (`HAS_FREE_LOCAL_REGIONS`: region parameters, region inference variables
and placeholders; `'static` and erased regions are allowed) and no error (`HAS_ERROR`).
Parameters are allowed: `T: Sized` under a param env is a goal like any other, and its
canonical form keeps `T`. Two flag tests, no fold.

**The key** is one `u64`:

| bits | what |
|---|---|
| 0..30 | the predicate's row (`Predicate::row`, at most `table::LAST_ROW` = 2^30 - 1) |
| 30..60 | the param env's clause list (`Clauses::index`; a list past 2^30 has no key) |
| 60..63 | the typing mode's tag (0 to 6, below) |
| 63 | 0 in a key; set in a slot being written |

**Which typing modes.** The key stands for the canonical input `(goal, param env, typing mode,
predefined opaques)` only when that input is a function of the key. So:

- `Coherence`, `Reflection`, `PostAnalysis`, `Codegen`: no data, keyed by tag.
- `Typeck`, `PostTypeckThroughBorrowck`, `PostBorrowck`: keyed only when their list of opaque
  types (and coroutines) is empty. A non-empty list is the defining scope of a body, and a goal
  may reach an opaque in it through a field type or a projection even when the goal names none,
  so the answer depends on the list; those goals keep the canonical path.
- `ErasedNotCoherence`: never keyed. Its answers may be refused and rerun in the non-erased
  mode (`RerunNonErased`), so the answer is not a function of the goal alone.
- And in every mode, only when the inference context's opaque-type storage is empty
  (`opaque_types_storage_num_entries` is zero): otherwise the canonical input carries the
  predefined opaques, which the key does not.

So in every keyed case, key equal means canonical input equal (regions: only `'static` and
erased, which the canonicalizer maps the same way every time), and the vendored solver's answer
to a canonical input in a revision is the answer the table holds. rustc's old solver kept global
evaluation and selection caches keyed by non-canonical global predicates on the same argument.

**Which answers.** Only root goals are inserted, and only when:

1. `salsa::capture_reads` around the root evaluation gave `Some`: the evaluation read no
   provisional value of a salsa cycle, made no tracked struct, read no query-local cache
   (`unshareable_reads`) and ran under a query. These are exactly the shared search-graph
   cache's conditions (`interner.rs`, `cache_scope`).
2. The answer is `Ok` with `Certainty::Yes` and `HasChanged::No`, or `Err(NoSolution)`.
   Ambiguity is never inserted: its `MaybeInfo` and stall data are what the caller acts on.
3. The evaluation left the inference context as it found it: the same `state_changes`, region
   variables, region constraints (both the legacy and the solver's data), registered region
   obligations, and overflow warnings (rustc's `recursion_depth_exceeding_limit` rerun, which
   pushes one). Then a hit, which changes nothing, is the evaluation exactly.

A root result is final: the root's search graph is empty when it returns, so no answer in the
table is a cycle head's provisional one. The table is probed only for root goals (fulfillment
rounds and registration), never for nested ones: a nested goal inside a cycle must see the
search graph's provisional answers, which only the search graph's own cache (with its
`NestedGoals` applicability check) knows how to keep. Nested goals keep the canonical path, with
the shape table in front of it.

**A hit** replays the stored `Reads` into the asking query (`salsa::replay_reads`), exactly as
`tls_cache.rs`'s shared cache does through `get_tracked`, then returns the answer. It allocates
nothing.

**Where it lives.** One table per scope and revision, like the shared search-graph cache: the
shared generation in `tls_cache.rs` holds both (request: `SharedCaches { search, goals }`), so it
is per database, crate and `regions` flag, replaced on the first use in a later revision, and
freed through the same limbo after its grace.

**The table** (`global_goals/table.rs`): open addressing with linear probing over 64-byte slots
(the key word, the answer, the 32-byte `Reads`), one slot to a half line. It starts empty and
allocates its first level (2^12 slots, 256 KB, zeroed pages) on the first insert, so making a
generation allocates nothing for it. A full level (half its slots used) is never rehashed: a new
level four times larger is added (docs/v2/plan/design/concurrency.md: "A full table grows by
adding a level"), and inserts go to the newest. Up to five levels (about 1.4 million goals); past
that inserts stop, which costs solves, never an answer. Lookups probe levels oldest first, since
the goals asked first are asked most. A reader does plain acquire loads: no lock and no write on
the read path. A writer claims an empty slot with a compare-and-swap to the key with bit 63 set,
writes the answer and reads, then publishes the key with a release store; a reader that finds the
pending key stops with a miss. Two writers of one key: the second sees the first's key and
returns.

### 2. The shape table (`global_goals/shape.rs`)

In `compute_goal_fast_path`, for a positive trait goal whose self type has no inference
variables, placeholders, free local regions or errors, outside `Coherence`:

- **`Sized`, `MetaSized`**: rustc's own fast path (`sizedness_fast_path`, `has_trivial_sizedness`
  with the per-ADT `sizedness_constraint`) already is the shape table for sizedness; kept as is.
- **`Copy`, `Clone`**: rustc's `is_trivially_pure_clone_copy` for scalars, shared references,
  raw pointers, `!`, and tuples and arrays of those, kept; extended to a non-generic ADT whose
  verdict holds: exactly one relevant impl of the trait for it, positive, not `default`, with no
  where clauses (a `#[derive(Copy, Clone)]` or a plain manual impl). Its fields are not looked
  at: an impl candidate's nested goals are its where clauses only.
- **Auto traits** (`trait_is_auto`: `Send`, `Sync`, `Unpin`, `Freeze`, `UnsafeUnpin` and user
  auto traits alike; `Send` is not a hir-def lang item, so no fixed list): a walk that is rustc's
  auto-trait rule on a closed fragment, answering only "holds":
  - at every node, the relevant impls (`for_each_relevant_impl`, the solver's own lookup) come
    first. Any impl disqualifies the structural candidate, as
    `disqualify_auto_trait_candidate_due_to_possible_impl` does for every rigid type. Exactly one
    positive, non-`default` impl whose self type is the node's head over distinct parameters
    (`&'a T`, `*const T`, `Unique<T>`), whose where clauses are each an auto trait on a parameter
    (`T: Sync`) or a sizedness bound the sizedness fast path proves, is followed into those
    clauses. A negative impl, two impls, or any other clause ends the walk with no answer;
  - with no impl: rustc's `instantiate_constituent_tys_for_auto_trait` for scalars, `!`, fn
    definitions and pointers (none), `str` (`[u8]`), pointers, references, arrays, slices,
    patterns (the element), tuples (the elements), `PhantomData<T>` (`T`) and other ADTs (every
    field type, instantiated). Unsafe traits on types with unsafe fields, closures, coroutines,
    `dyn`, parameters, aliases, foreign types and errors end the walk with no answer;
  - a (trait, type) pair already on the walk's stack holds: auto traits are coinductive
    (`TraitFlags::AUTO | COINDUCTIVE`, `CurrentGoalKind::CoinductiveTrait`), and the solver's
    coinductive cycles hold unless a leaf fails, which is what the walk computes;
  - the walk stops with no answer past 16 levels or 256 nodes.

  For an ADT the verdict is a query, `adt_auto_trait_holds(adt, trait)`, computed once per
  revision from its fields with identity arguments, run with an interner of the ADT's own crate
  (impls of an auto trait for an ADT are in its crate or, for core's types, in core: E0321). A
  verdict that holds with identity arguments holds for every instantiation: the walk reached no
  parameter whose answer it used, which is the brief's "no generic dependence". A goal on an ADT
  instance whose verdict does not hold is walked inline with its arguments.

**Allocation.** A hit allocates nothing: the per-ADT verdict is a `Copy` memo; the inline walk
keeps its stack in a fixed array of 16.

**What it must keep of rustc, and the one edge.** Every answer it gives is `Certainty::Yes`
with no constraints, which the solver gives on the same goal: same candidates (impl or auto),
same nested goals, coinduction for cycles. Param-env candidates would also hold for the same goal
(the type has no free local regions or inference variables, so they constrain nothing). The edge
is the recursion limit: rustc's search graph charges each nested goal one level, and a nested
goal answered by this table charges none. rustc's own sizedness fast path does the same. A
program can tell only if a goal sits within 16 levels of the recursion limit (default 128) when
it reaches the shape: rustc then overflows, reruns with twice the limit and warns
(`recursion_depth_exceeding_limit`), and blaze holds without the warning; at twice the limit,
rustc's error comes up to 16 levels later in blaze. No real program sits there.

### 3. The persistent cache by content (`goal_store.rs`, child `frontend-fast-trait-store`)

compile-time.md change 17. A root goal inserted into the global-goal table is offered to the goal
store; a root goal the table misses asks the store before solving.

- **Record**: the goal in stable form (types by `ContentId`, never dense IDs), the answer, and
  the facts its evaluation read (each `Fact::key` with the fact's hash), under domain
  `"blaze v2 goal <n>"`, plus the solver version (`harness/toolchain`'s nightly and the hash of
  the vendored solver's patch series).
- **Reuse** only when the solver version matches and every fact hashes the same in the current
  view: a constructive trace, as Shake's and Bazel's action caches. A reused answer is inserted
  into the global-goal table with the reads of the fact checks, so its query reruns when a fact
  changes.
- **Never stored**: cycle results (not root, or `capture_reads` gave `None`), goals with
  inference variables (not global), and every goal whose read set cannot be expressed as facts.

**What stands in the way today.** `blaze-items` depends on `hir-ty`, so `hir-ty` cannot name
`Fact`, the items' stable type hashes or the fact queries; and salsa's `Reads::edges` is
`pub(crate)`, so a capture's reads cannot be listed at all. The solver's reads today are salsa
queries over hir-def and hir-ty (the crate's trait impls, impl headers and predicates, field
types, signatures, lang items) that have no fact. The child's notes list each read and the fact
requested of lane items, and the store holds no goal whose reads it cannot express.

## What proves it

- `global_goals/tests.rs`:
  - the key's packing round-trips, and each non-keyed shape (inference variables, placeholders,
    local regions, erased mode, a defining scope, opaques in storage) has no key;
  - the table against a `Mutex<HashMap>` reference under eight threads of inserts and lookups,
    level growth included;
  - a table hit and a shape hit allocate nothing (`blaze-alloc-count`);
  - the differential test: a generated crate (impls with and without where clauses, auto traits,
    negative impls, `PhantomData`, `PhantomPinned`, `UnsafeCell`, generic and recursive ADTs) and
    random global goals over it, each answered with the tables and through the plain solver
    (tables switched off, caches cleared between), which must agree on every goal; and the shape
    walker against the solver on every (auto trait, ADT) pair of the crate.
- `goal_store`'s tests: the record read back from the store equals the computed one, and a
  changed fact refuses it.
- `harness/v2/frontend/cases/trait/`: programs through rustc and blaze, every shape the tables
  take and each edge where they hand over.

## Prior art

- rustc's old solver's global evaluation and selection caches, keyed by global predicates
  without canonicalizing; the new solver's `GlobalCache` behind one lock.
- chalk's finding that canonicalization dominates small goals.
- Swift's requirement machine caching conformance lookups per (protocol, nominal type), the
  model for the per-ADT verdict.
- Shake's and Bazel's constructive traces for the persistent cache.

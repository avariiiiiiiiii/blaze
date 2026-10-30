---
status: plan
summary: mem-db-def-collect's plan to move def-map collection (hir-def's nameres.rs and nameres/ except path resolution) onto blaze-mem; the frame each collecting query opens, which temporaries move, how each memoized result is made once at exact size, what still grows on the global heap and why, the budget tests, salsa cycles and the speed-of-light bound.
date: 2026-10-29
---
# mem-db-def-collect: def-map collection onto blaze-mem

Part of `mem-db-def` under `mem-db` (`docs/v2/plan/lanes/mem/db.md`, whose contract this follows).
Paths: `crates/hir-def/src/nameres.rs` and, in `nameres/`, `collector.rs`, `facts.rs`, `facts/`,
`demand.rs`, `mod_resolution.rs`, `proc_macro.rs`, `attr_resolution.rs`, `assoc.rs`,
`diagnostics.rs`, `frozen.rs`, `uses.rs`, plus the new `tests/budget.rs`. Path resolution
(`path_resolution.rs`, `item_scope.rs`, `resolver.rs` and the rest) is mem-db-def-resolve's.
Every change is a request: `v2/requests/mem-db-def-collect.md`, which has the exact changes
(Parts A to G). This note gives the design and the reasons.

## The design

A collecting query (`crate_local_def_map`, `block_def_map`, every facts query, `TraitItems`,
`ImplItems`, `used_imports`, the demand queries) opens `blaze_mem::frame::current` as its first
line. It passes `&'f Frame` to whatever does the work: `DefCollector<'db, 'f>`,
`CollectCx<'db, 'f>`, `AssocItemCollector<'db, 'f>`, `frozen::Dec<'a, &'f Frame>`. Three kinds of
data are then told apart.

1. **Temporaries go to the frame.** These are the import worklist (`Imports`: directives,
   generations, the two binary heaps, the wait table and its edges, woken), the glob edges, the
   macro directive lists and per-round vectors, `mod_dirs`, `skip_attrs`, `prev_active_attrs`,
   the deferred derives, the name-record lists, the staged diagnostics and derive helpers, the
   import graph and its SCC solver, and the decode tables of a frozen map. They are `FrameVec`,
   `FrameMap` (hashbrown with `&'f Frame` as its allocator) and `BinaryHeap<_, &'f Frame>`, so
   dropping them is free and the next task reuses the region.
2. **Results are made once, at size.** Each owned box is made from a frame vector in one
   allocation (`Box::from(&v[..])` for `Copy` rows, `collect` from a `TrustedLen` drain otherwise),
   and maps are made with `with_capacity(len)` then `extend`. This replaces every
   `shrink_to_fit` and every `into_boxed_slice` of a vector with slack. The
   `DefMap.diagnostics` and `derive_helpers_in_scope` fields become boxed slices. A result
   that is a filter of a scope (`ModuleScope::traits`, `legacy_exports`, proc-macro helper names)
   counts first and then fills.
3. **What is read while it is built stays where it is read.** The `ModulesMap`, each module's
   `ItemScope` and children, and `macro_use_prelude` are part of the `DefMap` that
   `resolve_path_fp` reads during collection. The result cannot borrow the frame (it outlives the
   task), so these grow on the global heap during a miss. At the end they are rebuilt once at
   exact size (`settle`) instead of shrunk. Under the facts engine the crate's `DefMap` is
   assembled from final rows with `ModulesMap` reserved to the module count, so this residual is
   left in blocks and in the old collector (kept as the differential reference).

Values carried from one facts round to the next (`SccState.state: Arc<UnitState>`, `MacroStep`,
`SideTables`) are memo values, so they are owned. The per-round queues inside `UnitState` (heaps,
deferred lists, wait table, woken) are rebuilt each round from the carried state. They move into a
frame `ImportQueues<'f>`, so the carried part holds only what the next round reads.

Code outside query runs keeps its signatures and uses `blaze_mem::scratch`: `demand::walk_bodies`
and its kin, which drivers call, and the `scratch::with_string` buffer of module-file resolution.

## Salsa cycles

No nameres query is in a salsa cycle. There is no `cycle_fn`, `cycle_initial` or `cycle_result` in
the slice. The collector's fixpoint is a loop inside one run, and the facts rounds are distinct
keys, each reading only the round before. What a round keeps for the next is owned (above).

## Residual: the building `DefMap`

The best long-term design stages the building tables in the frame too. The building `DefMap` would
be a view the resolver reads through (a `ModuleTable` trait, or `DefMap<A: Allocator>` while
building), published once into the owned `DefMap`. That touches `path_resolution.rs` and
`item_scope.rs` (mem-db-def-resolve) and every `DefMap` reader. It is the follow-up once the facts
driver serves crates: then only blocks build a `DefMap` in place, and their scopes are small.
Until it lands, a cold `crate_local_def_map` is held from the warm side, by budget test (a). The
exact-count miss tests (b) use queries whose results are made at size end to end
(`module_impls`, `ModuleScope::traits`).

## Budget tests

`nameres/tests/budget.rs`, written in full in Part F of the request file:

- **(a)** Collect a crate cold, then edit whitespace in a function body twice. The first
  re-collection warms what revalidation keeps. The second allocates nothing
  (`crate_local_def_map` revalidates, its item-tree input backdates). Reading a collected
  module's names and traits also allocates nothing.
- **(b)** A miss of `module_impls` (and of `ModuleScope::traits`) allocates salsa's run cost plus
  one allocation per non-empty box of the result. The count is computed from the result. Salsa's
  run cost is measured on the same query for a module whose result owns nothing, after a warm-up
  run takes the ingredient's one-time setup. When mem-db-salsa pools memos, that baseline goes to
  0 and the tests hold a miss to exactly its result's boxes, with no edit.

## Speed of light

What has to happen for collection: each import directive, macro directive and module row written
once, read by the fixpoint, and the result's rows copied once into exact boxes. Today every
temporary is a global allocation that also grows by doubling: each `Vec` field of `Imports`,
`DefCollector`, `ImportGraph` and `Solver` takes about \( \log_2 n \) reallocations, each copying
the vector (\( 2n \) element writes in total), at 15 to 25 ns per mimalloc call, then a shrink
realloc on the result. On the frame an allocation is a bump of about 2 ns, growth past the end is
an in-place extend, and dropping is free. So the bound for the temporaries is their bytes written
once, about \( 16 \) bytes per import (`WaitEdge`) plus 40 to 80 bytes per directive. That is
memory bandwidth: for a 10 000-import crate, about 1 MB, or tens of microseconds. The result adds
one allocation per non-empty box, at its exact size, plus the residual tables above. On a
revalidation and a hit the bound is 0 allocations, which budget test (a) holds.

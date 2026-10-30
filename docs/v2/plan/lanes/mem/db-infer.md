---
status: plan
summary: mem-db-infer's plan to move hir-ty (all but the trait solver) onto blaze-mem; how each query's run reaches its frame, which temporaries move, how memoized results are built once at their exact size, the tags, per-call scratch, budget tests and the speed-of-light bound.
date: 2026-10-29
---
# mem-db-infer: hir-ty onto blaze-mem

Part of `mem-db` (`docs/v2/plan/lanes/mem/db.md`, whose **contract** this programs against).
Paths: `crates/hir-ty/` except `src/next_solver/` (`mem-db-solver`'s). The owners are lane db
(and its children) and, for `method_resolution.rs`, `method_resolution/`, `traits.rs`,
`specialization.rs`, `opaques.rs`, `layout.rs`, `layout/` and `drop.rs`, lane items; none is
done, so every change is a request. The index is `v2/requests/mem-db-infer.md`; the changes, file
by file, are in its part files:

| part | files of `crates/hir-ty/src/` | request file |
|---|---|---|
| `mem-db-infer` | `db.rs`, `lib.rs`, `Cargo.toml`, the budget tests | `v2/requests/mem-db-infer.md` |
| `mem-db-infer-ctx` | `infer.rs`, `infer/{unify,columns,persist,fallback,diagnostics,ambiguity,at_error,mutability,opaques,path,autoderef}.rs` | `v2/requests/mem-db-infer-ctx.md` |
| `mem-db-infer-expr` | `infer/{expr,pat,coerce,callee,cast,op,place_op}.rs` | `v2/requests/mem-db-infer-expr.md` |
| `mem-db-infer-closure` | `infer/closure.rs`, `infer/closure/` | `v2/requests/mem-db-infer-closure.md` |
| `mem-db-infer-lower` | `lower.rs`, `lower/`, `generics.rs`, `utils.rs`, `variance.rs`, `variance/`, `dyn_compatibility.rs`, `builtin_derive.rs`, `effects.rs` | `v2/requests/mem-db-infer-lower.md` |
| `mem-db-infer-method` | `method_resolution.rs`, `method_resolution/`, `autoderef.rs`, `traits.rs`, `specialization.rs`, `opaques.rs`, `drop.rs`, `inhabitedness.rs`, `representability.rs` | `v2/requests/mem-db-infer-method.md` |
| `mem-db-infer-eval` | `layout.rs`, `layout/`, `consteval.rs`, `mir.rs`, `mir/`, `intrinsic.rs`, `target_feature.rs`, `target_feature/`, `lang_items.rs`, `primitive.rs`, `solver_errors.rs`, `evict.rs` | `v2/requests/mem-db-infer-eval.md` |

## Rules every part follows

These fix the shape so the parts fit; a part that finds one can't work for its file says so in
its request file and to `mem-db-infer`, and does not invent another.

### R1. Where a run's frame comes from

- A query function (a `#[salsa::tracked]` fn, or a fn only such a fn calls) opens
  `blaze_mem::frame::current(|frame| ..)` once, at the top of its body, and passes `frame` down.
  The executor runs the query inside `frame::task(tag, ..)` (contract 1), so this is the run's
  frame. `current` stops naming the caller when the thread is in no task: never a second path.
- A `pub` entry point that `crates/hir`, `crates/hir-ty-*` or the IDE call outside any query
  (`could_unify`, `could_coerce`, `could_unify_deeply`, `InferenceTable::new` from outside
  inference, method resolution's `iterate_method_candidates` and the like) opens
  `blaze_mem::frame::task(blaze_mem::Tag::Infer, |frame| ..)` itself instead: a task inside a
  query's task is one level deeper, so its reset frees only its own memory. Each part lists
  which of its functions are entry points of this kind, by the callers it found
  (`rg -n '<fn>' crates/ -g '!crates/hir-ty/**'`).
- `current` and `task` are not opened again below that: deep code takes the `&'f Frame` of the
  context it is in.

### R2. How `&'f Frame` enters the contexts

- `InferenceContext<'db>` becomes `InferenceContext<'db, 'f>` with a field
  `pub(crate) frame: &'f blaze_mem::Frame`. Every `impl<'db> InferenceContext<'db>` header (in
  every file that has one) becomes `impl<'db, 'f> InferenceContext<'db, 'f>`, and every
  `&mut InferenceContext<'db>` in a signature `&mut InferenceContext<'db, 'f>`. `InferenceContext::new`
  takes `frame: &'f Frame` as its last parameter. A struct that holds an
  `&'a mut InferenceContext<'db>` (`InferenceContextAutoderefCtx`, `ConfirmContext`, the probe's
  context, the expression-use visitor, `CastCheck::check`'s callers) gains the `'f` with it.
- `InferenceTable<'db>` stays without `'f`: it lives inside `InferenceContext`, and the few
  buffers of its own (`trait_errors`) take the frame by a method argument or move to
  `InferenceContext` (ctx decides, writes it).
- `TyLoweringContext<'db, 'a>` takes `frame: &'a Frame` as a field (no new lifetime: `'a` is
  already the borrow of the creator's resolver and generics, which the frame outlives) and as
  the last parameter of `TyLoweringContext::new`. `InferenceTyLoweringContext<'db, 'a>` passes
  its context's frame. The other lowering contexts (`lower.rs`'s other `new`s,
  `lower/path.rs`'s `PathLoweringContext`) take it the same way.
- The method probe's context (`method_resolution/probe.rs`), `ConfirmContext`, the autoderef
  steps, `CoerceMany`, the closure analysis' visitor and pattern lowering buffers all reach the
  frame through the `InferenceContext` they hold, or a `&'f Frame` field when they don't hold one.

### R3. Which types temporaries take

- A list: `Vec<T, &'f Frame>` (`Vec::new_in(frame)`, `Vec::with_capacity_in(n, frame)`). A list
  whose length is known when it is made: `&'f mut [T]` from `frame.alloc_slice_copy`,
  `alloc_slice_fill_with` or `alloc_slice_from_iter`.
- A map or set: `hashbrown::HashMap<K, V, rustc_hash::FxBuildHasher, &'f Frame>` /
  `hashbrown::HashSet<K, FxBuildHasher, &'f Frame>` (`with_hasher_in(FxBuildHasher, frame)`).
  `indexmap` takes no allocator: an `IndexSet`/`IndexMap` temporary becomes a frame `Vec` of the
  values plus a frame `hashbrown::HashMap<K, u32, ..>` of their places.
- An `la_arena::ArenaMap<Idx<K>, V>` temporary (la-arena takes no allocator) becomes
  `columns::DenseMap<'f, K, V>` (rule R5), a `Vec<Option<V>, &'f Frame>` indexed by the arena
  index, which is what `ArenaMap` is inside.
- A `SmallVec<[T; N]>` that never spills in practice stays (it allocates nothing); one that
  spills on real code (more than `N` in the corpus: say why you think so) becomes a frame `Vec`.
- `ThinVec`/`Box`/`Rc`/`Arc` made per node or per goal and dropped in the run: frame `Vec`/slice;
  a `Box<T>` temporary: `Box<T, &'f Frame>` or `frame.alloc(value)`.
- Recursion that builds a list per level: one frame `Vec` for the whole run with a start index
  per level (truncate on return), or a frame `scope` per level when nothing escapes it.

### R4. What a memoized result is

Contract 4: a memo's value outlives the run, so it is owned, from the general-purpose allocator,
made once at its exact size at the end of the run from the frame temporaries:
`Box<[T]>` by `frame_vec.drain(..).collect()` (a `TrustedLen` drain, one allocation of exactly
`len`), `Box<str>`, or the result's own column types. Never a global `Vec` grown by doubling and
shrunk, never `shrink_to_fit`, never `ThinVec` grown by push. A result that is empty allocates
nothing (`Box::default()`). For each query each part writes: the result's type, every
allocation it makes (count them: that count is the cold budget), and which frame temporaries it
is built from.

### R5. `InferenceResult`: frame-backed while inferring, owned once

`InferenceTables<'db>` (`infer.rs`) becomes `InferenceTables<'db, 'f>`: every table it has is a
frame value (`DenseMap<'f, ExprId, StoredTy>` for `ArenaMap<ExprId, StoredTy>`,
`CsrBuilder<'f, K, T>` with frame vectors, frame `hashbrown` maps for `FxHashMap`, frame bit
vectors for `IdxSet` and `ExprOrPatSet` while they grow). `InferenceTables::freeze` makes the
`InferenceResult`'s columns (`TyColumn`, `IndexColumn`, `Csr`, `IdxSet`, `SortedMap`, ...) each
once at exact size; their builders' own temporaries (`dense`, `by_ty`, `words`, `ranks`, the
sort buffers) are frame values too, so `from_entries` takes a `&Frame`. `columns.rs` gains
`DenseMap<'f, K, V>` and the frame forms of the builders; the owned column types do not change
(their layout and `Eq`/`Hash` are the result's, and `infer/persist.rs` encodes them).

### R6. Tags

Contract 3. `db.rs` names them once:

```rust
/// What hir-ty's inference, lowering, method resolution, layout, const eval and trait
/// solving allocate (contract 3 of docs/v2/plan/lanes/mem/db.md).
pub(crate) const INFER_TAG: blaze_mem::Tag = blaze_mem::Tag::Infer;
/// What hir-ty's MIR queries allocate.
pub(crate) const MIR_TAG: blaze_mem::Tag = blaze_mem::Tag::Mir;
```

and every `#[salsa::tracked(..)]` fn of hir-ty adds `mem_tag = crate::db::INFER_TAG` (MIR
queries `crate::db::MIR_TAG`), the option `mem-db-salsa` adds to the tracked attribute
(`v2/requests/mem-db-salsa.md`; asked to take a path to a `Tag` constant, not only a variant
name). Each part lists its tracked fns with their line and tag.

### R7. Per-call scratch on hit paths

Helpers called from many queries on paths that run on a memo hit or on a re-check that changes
nothing: inside a query they take the caller's `&'f Frame` (rule R1: they never open `current`
themselves); a helper that is also an entry point outside any query (R1) and needs a buffer of
ids uses `blaze_mem::scratch::{with_u32s, with_u64s}` or a `scratch::Scratch<T>` in a
`thread_local!` (only for `'static` element types: a `Ty<'db>` list is a frame value).

### R8. Tests

Each part writes, beside its code, the unit tests that keep its semantics (existing tests kept
and extended). The budget tests are `mem-db-infer`'s (`tests/budget.rs`, below), with the
counter `solver-sync-glue`'s request 6 installs in `lib.rs`.

## What each part found

Filled in from the parts' request files as they finish: see `v2/requests/mem-db-infer.md`.

## Budget tests

In `crates/hir-ty/src/tests/budget.rs` (a test module `mem-db-infer` requests), with
`#[global_allocator] static ALLOC: blaze_alloc_count::Counting` in `lib.rs` (solver-sync-glue's
request 6) and `test-fixture` as a dev-dependency:

1. **Warm re-check, zero**: a fixture crate, `InferenceResult::of` of each body, then
   `db.synthetic_write(Durability::LOW)` and `of` again (deep verification, nothing changed),
   twice; the second re-check is held to zero allocations (`assert_at_most(0, ..)`).
2. **Cold infer, its owned result alone**: after (1) warmed every dependency and interned every
   type, `infer_query` is called directly (not through salsa, so it runs) inside
   `frame::task(Tag::Infer, ..)`; it is held to `result.owned_allocations()`, a test-only count of
   the result's own heap blocks (each non-empty boxed column, each closure's capture lists).
3. **Frame use**: the same cold run's `frame.peak()` is reported and bounded by the body's size
   (bytes per expression times expressions), so a temporary that grows per node and is never
   reset shows up.

## Speed of light

Written below with the parts' numbers.

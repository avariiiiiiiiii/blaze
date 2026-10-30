---
status: plan
summary: lane mem's child mem-format: moving the front end's interners (intern, blaze-ids, span), syntax and item-tree columns, the lexer's columns and the per-call scratch of parse, lowering, token conversion and mbe onto crates/blaze-mem, with the allocation-budget tests that hold each to zero.
date: 2026-10-29
---
# mem-format: the front end's format crates onto blaze-mem

Parent: lane mem (docs/v2/plan/design/memory.md). Children: `mem-format-tokens` (parser, tt,
mbe, syntax-bridge), `mem-format-tree` (syntax, syntax-tree, hir-def's item tree),
`mem-format-hir` (hir-def's expression store and HIR). Each writes its own section below.

## interners and span (mem-format)

None of these crates was free, so each change is written out, code and budget tests, in
`v2/requests/mem-format.md` for its owner. Speed of light: `speed-of-light-format.md`.

- **`blaze-mem` (to mem, section 1).** `Column::claim(count) -> Block` and `Column::fill(&mut
  Block, value)`: a writer claims a run of indices with one `fetch_add` and fills them in
  order. That lets an interner that assigns IDs in per-shard blocks keep its exact assignment
  on a `Column`.
- **`intern` (to dedup-intern, section 2).** The store is type-erased, so its values go on one
  shared `Region` tagged `Tag::Intern` (`space.rs`, 64 GiB reserved), not a `Column<T>` per type.
  Tables, grown table headers and bump chunks are claimed from it; the model build
  (`blaze_check`) keeps boxes, as only std atomics can be cast from region memory. The `Symbol`
  ID column becomes one `Region` of `AtomicPtr` slots. It is not a `Column`, because a freed ID
  is reused at a chosen index. Tests: a hit and read and 100k misses allocate nothing; each slot
  is published once.
- **`blaze-ids` (to dedup-intern and the codec children, section 3).** `Sharded`'s values move
  onto a `Column<(ContentId, T)>` with per-shard `Block`s of 64. The probe tables stay boxed (see
  the speed-of-light doc). `ContentId::of_many_into` keeps its buffers in `scratch`. Tests: hits
  and reads allocate nothing; misses allocate only when a table doubles; a reserved interner
  stops at its size.
- **`span` (to db-expand-macros-next1, section 4).** `AstIdMap`'s build, body walk and relative
  maps put their buffers on the task's `Frame` and their radix sorts on kept `Scratch`. The
  index counters are one hash map per thread, cleared per build. Results are copied out once
  at exact length. Tests: a warm build allocates only its 3 (or 8 with fn bodies) result
  blocks; a warm body walk allocates only its ids. `map.rs` and `lines.rs` already build exact
  results, and their remaining sorts are lazy or of macro fragments only.
- **`const-ast`** allocates nothing outside its tests: nothing to move.

Memo values (`AstIdMap`, `SpanMap`, `ItemTree`, expression stores) stay global-allocator blocks
per lane mem's answer, until memos live in the image.

## tokens (mem-format-tokens)

All four crates are frontend-tokens', so each change is written out, code and budget tests, in
`v2/requests/mem-format-tokens.md`. A memoized `tt::TopSubtree` stays global-allocator blocks
(lane mem's answer); only per-call scratch moves.

- **Lexer (`parser/src/lexed_str.rs`).** The token and checkpoint columns are `lend_u32s`,
  declared checkpoints-first so they drop back in the order they were lent and each keeps its
  role. Test: a warm lex allocates nothing (was 2). A lex dropped on another thread warms that
  thread. `syntax::reparsing::lexes_alone` holds four `LexedStr`s (eight columns against a
  shelf of 4), so its last four allocate, as today.
- **Parse.** The event list, forward parents and output words were already on
  `per_thread_buffer!` shelves (requests-parser, frontend-tokens), with the warm-parse budgets
  at 0. Nothing to add.
- **Parser input (`parser/src/input.rs`).** The words go on their own `per_thread_buffer!`
  kind, so a warm `to_input` is free and the hand-rolled `Cell<Option<Input>>` caches in
  syntax-bridge and mbe can go. `into_words` copies out exactly: same count as today for
  `tree_words`, and the shelf stays warm.
- **mbe.** The matcher arena, both matches' bindings and the transcriber's nesting stack are
  `Vec<_, &Frame>` in one `frame::task(Tag::Expand)` per `expand_rules`. Nothing outlives the
  call, and no new lifetime is needed (the frame's and the input's shorten to one `'t`).
  `${concat}` text is `lend_string`. Test: a warm expansion of `$($e:tt)*` allocates the same
  for 10 trees as for 1000.
- **tt.** The walk stack is `Scratch<(u32, u32)>` (same count). The builder's five-column
  `SCRATCH` stays hand-rolled: it is a struct under one size cap, not a `Vec<T>`.
- **syntax-bridge.** `TtTreeSink`'s text is `lend_string` and its fragment starts are
  `lend_u32s`. `FlatTokens`' stack is a `SmallVec<[_; 4]>`.
- **Cost.** A lend and its return are two thread-local accesses; a task is two more. No hot
  loop takes one per token: one per lex, input, parse, expansion or conversion.

## tree (mem-format-tree)

Syntax side (`crates/syntax`, `crates/syntax-tree`). Not free when written, so every change is
in `v2/requests/mem-format-tree.md`; the item tree is `## tree items (mem-format-tree-items)`.

- **An unchanged file's parse.** `SourceFile::parse_file` of the same text is a kept parse
  handed out again: two `Parse` clones and two drops, all counts of a tree the kept slot holds.
  The allocation was in the drops: `Drop for Parse` sent every count to the drop thread over
  `std::sync::mpsc`, whose list channel allocates a block every 31 sends. Only a tree's last
  count frees anything, so only it is sent (`Arc::into_inner`, atomic); the others are a
  decrement in place. `reparse_to` compares the texts' addresses before their bytes. Budget:
  0, a hundred times over (`an_unchanged_file_parses_again_allocating_nothing`).
- **Lifetimes.** A `FlatTree` is a memo value (in the parse query's `Parse`): by lane mem's
  answer, one block, from the global allocator. Every column of one build, copy or edit is a
  temporary of that call: a per-thread set of buffers (`Buffers`, a `Cell` in a thread-local of
  `syntax-tree`'s, the builder's own as the item tree's `TreeBuilder` is), kept at the largest
  tree the thread built. Nothing in a tree is a process-long table, so no `RegionVec`/`Column`.
- **One block.** `flat/block.rs`: `Block` (one raw allocation, 8-aligned, columns placed past
  each other) and `Col<T>` (derefs to `[T]`), so the tree's ~150 column reads are unchanged and
  only its five constructors move to `FlatTree::pack`. Soundness: a column is a field of the
  tree whose block it is in; `FlatTree` and `Kept` implement `Drop`, so none can be moved out.
  The 8-aligned columns go first: the block has no padding and is exactly the bytes
  `heap_size` counted before. 12 boxes and `exact`'s second copy of each become one block and
  one copy.
- **Budgets.** Warm build 1 (the block), copy 2 (its text, its block), splice 1, token
  replacement 1 (`a_warm_build_copy_or_edit_allocates_its_block_alone`); an outline adds its
  `Lazy`, the parse its `Arc`. The lexer and parser's buffers are `crates/parser`'s
  (mem-format-tokens).
- **Speed of light.** The block's cost is one copy of the columns, about 6.5 bytes an element
  at memory bandwidth (about 0.3 ns an element, under 1% of a parse at about 40 ns an element),
  and it replaces `exact`'s copy, so the work is no more than before; what goes is about 20
  allocator calls a parse and 12 to 15 an edit. Prior art: rust-analyzer's rowan allocates a
  green node per node; Roslyn pools its builders' buffers per thread, as this does.

## hir (mem-format-hir)

hir-def's expression store and HIR (`expr_store.rs`, `expr_store/`, `hir.rs`, `hir/`). The
paths are speed-hir-store's, which is not done, so every change is an exact request in
`v2/requests/mem-format-hir.md`. This section is the design those requests implement.

### Lifetimes, and the allocator for each

| what | lives | allocator |
|---|---|---|
| a builder's columns, list stacks, forward and back pushes, arenas, hash maps and side vectors | one lowering | the per-thread pool of builder scratch (`expr_store.rs` `Scratch`, at most 4 sets, a set over 1 Mi elements freed): warm after a thread's first body |
| `finish`'s sort buffers and gathered side columns | one `finish` | the same pool (`FreezeScratch::sort`, `Packing`) |
| a finished store's `Copy` columns | the memo value | one slab for the HIR columns, now with the match arms, record fields, field patterns and names; one for the type columns; one for the five side columns (`hir/col.rs` `Col`) |
| the store's arenas (bindings, labels, lifetimes) and its cold tables (paths, literals, asm, offset_of, generic args) | the memo value | one exact allocation each, none when empty |
| encode's words, names and gathered columns | one encode | per-thread spares (`Words`, `EncodeScratch`) |

A store, a body and a source map are memo values. Lane mem's ruling (`v2/requests/mem.md`) is
that a memo value has no revision-arena lifetime: salsa drops each one on its own when it is
recomputed. So they are owned, made once at their exact size, and their `Copy` columns are
packed into one block. Nothing here uses a revision arena or a task frame:
- A frame gains nothing over the pool for about 30 interleaved growing columns. A bump
  allocator wastes each column's old space when it grows, where the pool keeps each at its
  high-water mark.
- The pool is a type of hir-def's own in its own thread-local, which is lane mem's ruling
  for crate-owned scratch.
- So hir-def needs no `blaze-mem` dependency for this.

### Budgets, each held by a test

- Warm lowering: 0 (`a_warm_lowering_allocates_nothing`). Before this change, each of the 12
  per-store tables grew from empty for every body.
- `finish`: no buffer that grows with the body (`finishing_allocates_no_scratch`). Std's stable
  sort takes a heap buffer once a table outgrows 4 KB, about 150 entries. `finish` now uses two
  stable radix passes into a pooled buffer, low key then file. That gives the same permutation.
- A finished store without a source map is exactly its blocks
  (`a_warm_store_is_its_blocks_and_nothing_else`). For the fixture that is 8: the expression
  box, the column slab, the side slab, three arenas, the type slab and the type hygiene.
- Reading a store, a memo hit and a scope lookup: 0 (`expr_store/tests/alloc.rs`,
  speed-hir-store's).
- Warm encode: 0 into an output with room (`a_warm_encode_allocates_nothing`), and names cost
  0 on warm `Words` (`names_are_one_index_per_text_on_warm_words`).

### Speed of light

Lowering a body is a walk of its syntax tree that writes 5 to 12 bytes per node into warm,
pooled columns, so its bound is that walk. `finish` adds one copy of every column (memory
bandwidth: about 1 µs for a 100 KB store at 100 GB/s). A small allocation costs about 20 ns,
so the 8 to 12 allocations of a store are under 0.3 µs, against a lowering of tens of
microseconds for any body that has them, which is under 1%. The forward-map sorts are linear:
the radix survey skips the digits all keys share, the whole file key within one body, so for
range keys they are about 4 to 6 passes over 28-byte pairs.

What is left, and why:
- The five cold tables of `HirColumns` and the store's arenas are one allocation each.
  Packing them into one block needs offsets in place of pointers, and `Binding`/`Path` as
  plain records.
- Decode still copies once per column. Making it borrow the mapped bytes is speed-hir-store's
  queued "borrowed decode columns" (it needs frontend-deps-sigs' `Arc` owner of the image).
- The source maps' encode is next, once srcmap's schema 4 is settled.

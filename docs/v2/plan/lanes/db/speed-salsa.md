---
status: plan
summary: speed-salsa's audit of vendor/salsa's hot paths (memo hit, edge recording, deep verify, interning, LRU, claims) against rustc's red-green graph, Unison, Adapton, SwissTable and papaya; costs today, the changes filed with db-salsa, and what the memo and edge columns would take.
date: 2026-10-29
---
# speed-salsa: vendor/salsa against the state of the art

The changes were filed in `v2/requests/db-salsa.md` under "From speed-salsa" while `db-salsa`
held `vendor/salsa*`, then applied on 10-29: S1, S3 and S4 by speed-salsa, S2 by
speed-salsa-interned, and S5 (the edges, claim-free verify and the 24-byte memo header) by
speed-salsa. "How salsa
compares today" is the state before them; "Before and after" is what changed. Costs come from
reading the code, not from measurements: nothing was built or run.

## Benchmarks

| path | state of the art | what it costs there |
|---|---|---|
| memo hit | rustc's `VecCache` (DefId-keyed queries) and its `DepNodeColorMap` | 1 to 2 loads into a dense array by index, no lock; the color check is one `AtomicU32` load |
| recording a read | rustc's `read_index` | a 4-byte `DepNodeIndex` pushed to the task's `EdgesVec`, deduplicated by a linear scan up to 8 reads and an `FxHashSet` after |
| edges at rest | rustc's `SerializedDepGraph` | one edge array for the whole graph, each edge 1 to 4 bytes (the width the largest index needs), a node's edges a range into it |
| deep verify | rustc's `try_mark_green` | per edge, one color load; red or unknown recurses; no per-node lock (colors are set with a CAS) |
| interning | SwissTable probing (8 or 16 tag bytes per group, one SIMD compare), papaya (wait-free reads, incremental resize); in this workspace `crates/intern/src/arena.rs` | lookups take no lock; inserts lock a shard only to look again and insert; growth never makes a reader wait |
| invalidation | Unison | definitions named by content hash, so a cached result is never verified, only looked up; blaze-query takes this model (keys by input hash); salsa keeps revisions for the front end |
| invalidation | Adapton | demanded computation graphs, dirtying and change propagation; per-edge heap nodes, so slower than rustc and salsa per edge, and only of interest for its finer-grained cutoff |

## How salsa compares today

**Memo hit** (`function/fetch.rs` `fetch`, value present, verified this revision): no lock and
no shared write for a function without `lru`. About seven dependent cache lines: the
ingredient, the `boxcar` bucket, the `Page` (type id, `allocated`, `data`, `memo_types`), the
slot (a tracked struct's `updated_at` or an interned value's `last_interned_at`), the slot's
`MemoTable` array, the `MemoEntry` pointer, the boxed `Memo` (`verified_at`,
`verified_final`), and the runtime's current revision. rustc's is one or two.

Two things put locks on the hit path:

- **A function with `lru` (29 in the front end)**: `Lru::record_use` takes the ingredient's one
  `Mutex` and inserts into a linked hash set on **every hit**. On 100 cores every reader of
  `parse`, `ast_id_map`, `parse_macro_expansion` and the MIR queries writes the same cache line.
- **A function with more than one argument** (roughly a hundred in hir-ty, hir-def,
  hir-expand and hir-ty-mir by a rough `rg`): each call interns the argument tuple first
  (`setup_tracked_fn.rs:111`). `interned::IngredientImpl::intern_id` hashes, **locks a shard
  `Mutex`** and probes a `hashbrown` table, hit or miss. On a hit inside a query it also
  stores `durability`, every time, even when unchanged. On the first hit in a revision it stores
  `last_interned_at` and relinks an intrusive LRU list. The argument interners default to
  `REVISIONS = 3`, garbage-collected, so they all take this path.

**Recording a read** (`ActiveQuery::add_read`): the frame and its buffers are reused (no
allocation), but every read hashes the edge (FxHash) and probes the frame's `FxIndexSet`, even
when the same key was the last one read.

**Memo at rest**, a derived memo with `n` input edges, all packed (`zalsa_local.rs`):

| part | bytes |
|---|---|
| `MemoEntry` pointer in the slot's `MemoTable` | 8 per function keyed on that struct type, allocated for all of them once one memo exists |
| boxed `Memo` (`MemoHeader` 40 + an 8-byte value) | 48, one heap allocation |
| edges (`PackedQueryEdge`) | 8 per edge, a second heap allocation, rounded to its size class |
| edge fingerprints (`ThinVec`) | 0 unless it read an `lru` function: then 16 plus 20 per fingerprinted edge |

So about 48 + 8n bytes plus 8 per sibling function and two allocations per memo, against
data.md's budget of 24 + 4n and none. An edge is 8 bytes because it names an ingredient (12
bits), an index (32) and a generation (20); rustc's names a dense node index.

**Edges for nushell (#734).** The 0.8 GB of edges is about 100M edges at today's 8 bytes. In
one CSR array per memo table at 4 bytes they would cost 0.4 GB, and about 0.3 GB at rustc's
3-byte width (fewer than 16M nodes). The per-memo allocation and size-class rounding that
#734 counts under the memo would go too.

**Deep verify** (`deep_verify_edges`): per edge, a `dyn Ingredient` call, then the hit path
above for the dependency. Before verifying, the memo takes a claim: a shard `Mutex` and a
`hashbrown` insert and remove (`function/sync.rs`), and the shard count is the core count
rounded up to a power of two (concurrency.md asks for at least 4 times the cores). rustc
locks nothing per node.

**Tracked struct field reads**: one CAS on the struct's own `updated_at` per revision, then
plain loads. **Interned field reads** (`data`) and **interned `maybe_changed_after`** take no
lock in the common case. All three meet concurrency.md.

**Tables**: `Table` is a `boxcar::Vec` of 128-slot pages. It is append-only and grows by a
level, so readers never wait: this meets concurrency.md.

## Before and after (10-29)

| operation | before | after | rustc |
|---|---|---|---|
| memo hit | the seven lines above; a function with `lru` takes the LRU `Mutex` and inserts on every hit; a multi-argument function interns its tuple under a shard `Mutex` | five lines, four deep (bucket, page, then slot and column in parallel, row pointer at a computed address, memo; the page keeps its column slice, and the columns are blaze-mem regions), with no lock and no shared write: `lru` marks nothing on a hit (S1) and interning a tuple is a lock-free probe (S2). The bound is 2 lines; [speed-of-light.md](speed-of-light.md) names the change | 1 to 2 loads |
| recording a read | FxHash and an `FxIndexSet` probe per read | a read of the same key as the last one is skipped before hashing (S4); the rest as before, in the task's own frame | a push, deduplicated by a scan |
| edge record (query finishes) | one heap allocation per memo, 8 bytes an edge copied in | per edge, a probe of the task's 1024-entry node cache (L1 on a repeat key, the shared node table's lock-free probe otherwise) and one `Relaxed` store into the task's reserved chunk; one CAS per 256 edges; no allocation; published by the memo's own `Release` store | an `EdgesVec` moved into the graph |
| deep verify | a claim per memo walked (a shard `Mutex` plus a `hashbrown` insert and remove), then a `dyn Ingredient` call per edge | no claim for a memo that ran no cycle, has no outputs and isn't provisional: an iterative walk, per edge one decode (the node's key: one page load and two column loads) then the hit check; a claim only for a memo that must run | one color load per edge, no lock |
| memo at rest | header 40 + value, boxed; + 8 an edge in a second allocation (rounded to its size class); + 8 per sibling function; a 16-byte memo table in every slot | header 24 + value in its function's slab, no box; + 4 an edge in the shared array, no second allocation; + 8 for its row pointer in its own column only; a 4-byte row number in the slot. A memo with cycle, tracked-struct, accumulator or fingerprint data, or an assigned one, has them in one 72-byte box | 1 to 4 bytes an edge, a range per node |

`tests/memory-usage.rs` moves with it. The memos (header and value, the row pointer not
counted): an interned argument's 144 to 96, each `String` memo 40 to 24, a tracked function's
248 and the tuple's 148 (their tracked-struct lists sit in the 72-byte box). The structs lose
the 16-byte memo table per slot (the 4-byte row fits in padding): `MyInput` 96 to 48,
`MyInterned` 168 to 120, `MyTracked` 128 to 64, each `interned_arguments` 56 to 40.

**What the edges cost against #734's 0.8 GB.** nushell's 0.8 GB is about 100M edges at 8
bytes. The edge array holds them in 0.4 GB. Dead ranges of replaced memos stay until the array
is twice what was live, so between compactions it can reach 0.8 GB of words, but a one-shot
`check` replaces nothing and stays at 0.4 GB; pages come from zeroed allocations, so the unused
end of the last page is never resident. Each distinct key read costs the node table 12 bytes of
key columns plus 7 to 15 bytes of table (4-byte slots in 40-byte groups of seven, filled to three
quarters before doubling; outgrown tables are freed at each new revision), and each thread 16 KB
of node cache. At one key per ten edges (10M keys) that is 0.2 to 0.27 GB, so edges and nodes
together are about 0.6 to 0.67 GB against 0.8 GB, and the per-memo edge allocations' headers and
size-class rounding, which #734 counts under memos, are gone. Matching rustc's 0.3 GB needs its
variable width (3 bytes under 16M nodes), which an atomic word array can't express; it would
come with the frozen, persisted graph, not the live one.

**The 24-byte memo header.** `MemoHeader` is `verified_at` (8) and `QueryRevisions` (16), held
by a `const` assertion:

- one `AtomicU64` stamp: changed-at in the low 56 bits, durability in 2, and the
  `verified_final` and accumulated-inputs flags. Changed-at and durability are written only
  through `&mut`, before the memo is published; the flags change on a shared memo with one
  `fetch_or` or `fetch_and`, which leaves the other bits alone.
- one 8-byte origin word with a 3-bit tag: a derived memo with no extra data holds its edge span
  inline (start 32 bits, length 29); anything else is a box aligned to 8, its address in the
  rest. The box holds the extra data, the input and value fingerprints (moved out of the header:
  they exist only after an eviction), an assigned memo's 12-byte key, or a span longer than 2^29.

**Memos as columns** (`table/memo.rs`; callers in `function.rs`, `function/memo.rs`, the three
struct kinds, `zalsa.rs`, `ingredient.rs`). No box per memo, no pointer per sibling function, and
a slot's memo table is one 4-byte row number:

| piece | where | bytes | written by, when |
|---|---|---|---|
| row number (`MemoTable { row: u32 }`) | in the slot; handed out by the struct ingredient's `MemoTableTypes::next_row` when the slot is made, kept when the slot is reused | 4 per slot, folded into padding in all three struct kinds | once, at slot creation |
| row pointer | the function's `Column`: a blaze-mem `Region` (`Tag::Query`) reserving 8 bytes for each of 2^28 rows, row `r` at `8r`, committed (kernel-zeroed) as far as the highest row with a memo, at least 64 KB at a time | 8 per row up to the highest with a memo; untouched committed pages are not resident | the publisher, one `AcqRel` swap per publish; clearing a slot swaps in null |
| memo: `Memo<C>`, the 24-byte header and the value | the column's slab, a second `Region`: slot `i` at `size * i`, never moved, up to 2^24 memos (or 16 GB of address space) per column, a panic naming the type past it | 24 + the value | written once by the thread that took the slot, before the swap publishes it; after that only the header's atomics change, and the value only through `&mut` (eviction) |

- **Publish** (`MemoTableWithTypes::insert`): take a slab slot (the free list by one
  `fetch_add`, else the bump counter), write the memo there, swap the row pointer to it. The
  release half publishes the memo (and its edges, recorded before); the old memo, if any, goes on
  the column's retired list. A value is never written in place on a shared memo.
- **Read** (`get`): the column (a type check against `TypeId` and the rows' committed length,
  on one line), the row pointer at a computed address (acquire), the memo. Loads only, no write
  to shared memory.
- **Reclaim** (`Zalsa::reclaim_memos`, in `new_revision` and at the end of `evict_values`, both
  `&mut Zalsa`): `&'db` references into memos last the whole revision, so the revision boundary
  is the quiescent point. Each column drops its retired memos in place and puts their slots on the
  free list: O(retired), not O(live). The free list is written only here and read through the
  `fetch_add` index, so a slot is never handed out twice (no ABA). Slot addresses depend on
  scheduling but are never observed, hashed or persisted, so results stay deterministic.
- **Clearing a slot** (tracked struct deleted, interned value reused, a page dropped): `take_memos`
  or `drop` swaps each column's row to null and retires the memo; `take_memos` hands each to the
  caller as `&dyn Memo` for `DidDiscard` and `remove_outputs`, which is sound because retired
  memos live until the next reclaim.
- **Eviction, LRU, compaction, persistence, memory usage** walk the struct pages in page and slot
  order as before (`for_each_memo_table_mut`, `map_memo`), so the LRU picks the same values.
- **Invariant, checked in debug builds at every reclaim and at drop** (`Column::check_accounting`):
  every slab slot ever handed out is exactly one of a row's current memo, retired, or free and not
  yet taken. Also debug-checked: a retired memo lies in its own column's slab, slab slots are
  aligned and inside their page, a table is used only with rows its types handed out. Release
  checks: the memo type of a column (`type_assert_failed`), more than 2^32 - 1 slots, a column
  outgrowing its last slab page.
- **Tests** (`table/memo.rs`): `pages_tile_rows_and_slots`, `rows_keep_their_own_memos_per_column`,
  `replaced_memos_live_until_reclaim_and_their_slots_are_reused` (with drop counts),
  `taking_memos_retires_them`, `map_memo_changes_the_memo_in_place`, `the_wrong_memo_type_panics`,
  `racing_inserts_keep_every_row` (threads on disjoint rows, then reclaim and drop counts).
- **`const` assertions:** `MemoHeader` 24, `MemoTable` 4, an interned value 24 plus its fields.

Memo at rest: 24 + value + 4n bytes in the slab, plus 8 for its row pointer in its own column
only, against 40 + value + 8n, a box, 8 per sibling function, and the 16-byte memo table every
slot paid.

## What is filed (v2/requests/db-salsa.md, "From speed-salsa")

1. **LRU with no write on a hit.** `Lru` keeps only its capacity. At the revision boundary
   (`reset_for_new_revision`, which already has `&mut Table`), the memos that hold values are
   ranked by `verified_at` (set the first time a memo is used in a revision), and the oldest
   beyond the capacity are evicted, in slot order within a revision. Hit: one mutex and a set
   insert become nothing. Eviction: O(slots of the key's struct type) once per revision instead
   of O(evicted). The values evicted differ, but results don't: eviction keeps fingerprints and
   edges.
2. **Lock-free interning on a hit.** The key map becomes `crates/intern/src/arena.rs`'s table
   (eight tag bytes per group, release-published slots, growth into a new table while old ones
   stay readable). A hit is a probe with no lock. For reusable values, the per-revision
   validation is one CAS on the value's own `last_interned_at`, and reuse claims a slot by a
   CAS too, so a reader and a reuser can't both win. The intrusive LRU list becomes a CLOCK
   sweep over the shard's values. `durability` is stored only when it rises. Hit: a shard
   mutex (two RMWs on a shared line) and a store become nothing shared.
   It is the only new unsafe code here. It goes in one module with a safe API
   (`interned/table.rs`) with a `// SAFETY:` on every block. Today's locked map stays in the
   tests as the safe reference, and the CAS protocol is model-checked under shuttle
   ([unsafe.md](../../design/unsafe.md)).
3. **Small fixes**, each a few lines: skip the `last_interned_at` store for immortal values
   (nothing reads it); store `durability` only when it rises; in `add_read`, skip the hash
   when the edge equals the last one recorded; `SyncTable` shards at
   `(max_parallelism() * 4).next_power_of_two()`.

## S5: memos and edges as columns (the design, applied by speed-salsa 10-29)

### How it maps onto blaze-query

`crates/blaze-query` (lane db, read only here) keeps per key a list of `MemoData`: the key
(`QueryKey`, a kind and a content ID), a 128-bit value `fingerprint`, `edges: Edges` (an owned
`Box<[Edge]>` or a range of 56-byte records in the image, each `Edge { read: QueryKey, saw:
Fingerprint }`), and the value (`Value::Memory`, `Mapped` or `Evicted`). salsa's front-end memos
are the same four things in revision form, so the columns below line up one to one with them:

| blaze-query | salsa after S5 | bytes |
|---|---|---|
| `MemoData::key` (`QueryKey`, hashed to a `ContentId`) | the slot the memo hangs off (ingredient, `Id`); a key read through an edge gets a dense `u32` **node** in the database's node table, which is what an edge names | 0 in the memo; 12 per node, once per key ever read |
| `MemoData::fingerprint` | the value fingerprint, in the memo's cold box only once a value was evicted with one; a flag bit says it is there, so a hit never loads it | 0 hot; 16 cold |
| (view: verified in this view) | `verified_at` | 8 |
| `MemoData::value` (`Stored`) | `value: Option<Output>` beside the header in the memo's row; `None` once evicted, like `Value::Evicted` | the value |
| `MemoData::edges` (`Edges`, in read order) | `(start, len)` into the database's one CSR edge array; each edge one `u32` word: node (31 bits) and an output bit; the fingerprint it saw, when it had one, in the cold box by position (`Edge::saw`) | 8, plus 4 per edge |
| changed-at, durability (no counterpart: blaze-query has no revisions) | one `AtomicU64`: changed-at in the low 56 bits, durability, origin kind, `verified_final`, accumulated-inputs, cold and has-fingerprint bits in the top byte | 8 |

So a memo header is 24 bytes (`verified_at`, the packed word, the edge span or the cold box), plus
4 per edge in the shared array; data.md's "a memo 24 plus 4 per edge". The node table is the
piece blaze-query has no need for (its edges name content IDs), and the reason a salsa edge fits
in 4 bytes: rustc's `DepNodeIndex`. When the front end moves onto blaze-query, a salsa memo
converts to a `MemoData` by mapping each edge's node through the node table to its key, and its
cold fingerprints to `Edge::saw`.

### The pieces

- **Node table** (`src/graph/nodes.rs`, one per database in `Zalsa`). A `DatabaseKeyIndex` gets a node the
  first time a finished query's edges name it, and keeps it for the life of the database (an edge
  must never come to name another key, so nodes are not reused). Two columns, `id` (`u64`) and
  `ingredient` (`u32`), in pages that grow by doubling and never move. The map from key to node
  is the SwissTable of `crates/intern/src/arena.rs`, sharded at four times the cores: groups of
  seven 4-byte node slots and a control word of tag bytes in 40 bytes, found with no lock and no
  write, a tag match comparing the key through the node's columns; a miss takes the shard's lock
  to look again and insert. A table fills to three quarters, then doubles; the tables it outgrew
  are freed at the next new revision. Every task keeps a
  1024-entry direct-mapped cache of key to node, so the keys a task reads again and again (the
  crate graph, a file's text, the def map) cost one cache line in L1, not a probe of the shared
  table.
- **Edge array** (`src/graph/edges.rs`, one per database in `Zalsa`). `AtomicU32` words in pages that
  double and never move; a range never crosses a page, so a memo's edges are one slice. A task
  reserves 256 words at a time (one CAS on the array's length per 256 edges, never one per edge
  or per memo) and writes its query's edges there when the query finishes (`ActiveQuery` pop):
  the edges were recorded in the task's own frame and are published with the memo, which is the
  release store that makes them visible (concurrency.md, "Dependencies are recorded per task").
  Replaced memos leave dead ranges; at a new revision (with `&mut`), when the array is more than
  twice what was live at the last compaction (and over 1M words), every function ingredient
  copies its memos' ranges into a new array in slot order and the old one is dropped. That is
  O(live edges), paid at most once per doubling.

  One array for the database rather than one per memo table: rustc's `SerializedDepGraph` keeps
  one edge list for the whole graph; one array means one reservation per task (not one per
  function it ran), and one compaction pass.
- **Origin** (`zalsa_local.rs`, applied): one 8-byte word, a kind tag in its low bits and either
  the edge span inline or the address of a box (`WithExtra`) of the extra data and
  fingerprints with the span or an assigning query's key beside them. The extra is what most memos don't have: cycle heads, iteration
  and convergence, tracked struct IDs, accumulated values and the edge fingerprints.
- **Memo header** (`function/memo.rs`, applied): `verified_at`, the packed changed-at and
  flags word, and the 8-byte origin, at 24 bytes; see "The 24-byte memo header".
- **Deep verify without a claim** (`function/maybe_changed_after.rs`, `fetch.rs`): a memo that
  isn't provisional, took no part in a cycle and has no outputs is verified before any claim is
  taken: its edges are checked in order, each dependency by the same claim-free check, which
  only reads (and stores `verified_at` once a memo is found unchanged, which is idempotent). If
  every edge is unchanged, the memo is marked verified with no claim. Only a memo that must run
  takes a claim, and the claimed path is the existing one, so cycle handling, backdating and
  provisional memos are untouched. rustc's `try_mark_green` works this way. The walk is
  iterative (an explicit stack, eight frames inline), so a chain of any depth is verified in one
  pass with no recursion. `tests/unclaimed_verify.rs` covers a 300-deep chain, backdating, and
  eight threads verifying one chain at once.

- **Tracked functions over a concrete database** (salsa's step of frontend/db.md's migration):
  `#[salsa::tracked]` takes `db: &Db` for a `#[salsa::db]` struct `Db` as well as
  `db: &dyn Trait`. The macro hands `setup_tracked_fn!` the database type (`DbView: dyn Trait`
  or `DbView: Db`) and the function that registers its downcaster
  (`Trait::zalsa_register_downcaster`, or `ConcreteView::zalsa_register_concrete_downcaster`,
  which `#[salsa::db]` on the struct implements as the identity view, and `DatabaseImpl` by
  hand). `Database::task_depth(&self) -> u32` is a provided method (0: one task per thread)
  that `attach` and `zalsa_local` read, so a concrete database that overrides it (base-db's
  `Db`) has no indirect call on a hit. It is the only source of the depth: each `ZalsaLocal`
  records its `TaskId` when `TaskLocals` makes it for a depth, and claims, blocking and lock
  transfers take the task from it (`zalsa_local.task()`), so the `set_task_depth` hook is gone.
  `tests/task_sync.rs` pumps tasks through the override. `tests/concrete_db.rs` covers
  memoizing, interned arguments, LRU, `evict_values`, `&Db` and `&dyn Database` functions on
  one storage, `DatabaseImpl`, and the hit reading `task_depth`.

### Kept

Every public API `crates/` calls (`Storage`, `Database`, `#[salsa::tracked]`, `evict_values`,
`evict_values_of`, `trigger_lru_eviction`, `set_lru_capacity`, `memory_usage`), durability and
backdating (#350: a fingerprint backdates only when durability didn't drop), cycle handling,
eviction keeping edges and fingerprints, and determinism: node numbers depend on scheduling, but
nothing observable does (edges are compared by key, compaction copies in slot order, and no node
number is ever persisted or hashed).

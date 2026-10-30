---
status: plan
summary: speed-salsa's audit of vendor/salsa's hot paths (memo hit, edge recording, deep verify, interning, LRU, claims) against rustc's red-green graph, Unison, Adapton, SwissTable and papaya; costs today, the changes filed with db-salsa, and what the memo and edge columns would take.
date: 2026-10-29
---
# speed-salsa: vendor/salsa against the state of the art

`vendor/salsa*` was not free when this was written: `db-salsa` was running its second brief
(`db-salsa-sync.md`, the `Sync` database with claims by task). So nothing here is applied
yet. The exact changes are in `v2/requests/db-salsa.md` under "From speed-salsa". Costs
below come from reading the code, not from measurements: nothing was run.

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

- **Node table** (`src/node.rs`, one per database in `Zalsa`). A `DatabaseKeyIndex` gets a node the
  first time a finished query's edges name it, and keeps it for the life of the database (an edge
  must never come to name another key, so nodes are not reused). Two columns, `id` (`u64`) and
  `ingredient` (`u32`), in pages that grow by doubling and never move. The map from key to node
  is the SwissTable of `crates/intern/src/arena.rs`, sharded at four times the cores: groups of
  seven slots and a control word of tag bytes on one 128-byte pair of lines, found with no lock
  and no write; a miss takes the shard's lock to look again and insert. Every task keeps a
  1024-entry direct-mapped cache of key to node, so the keys a task reads again and again (the
  crate graph, a file's text, the def map) cost one cache line in L1, not a probe of the shared
  table.
- **Edge array** (`src/edges.rs`, one per database in `Zalsa`). `AtomicU32` words in pages that
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
- **Memo header** (`function/memo.rs`, `zalsa_local.rs`): `verified_at`, the packed
  changed-at and flags word, and a union of the edge span and `Box<Cold>`. The cold box holds
  what most memos don't have: cycle heads, iteration and convergence, tracked struct IDs,
  accumulated values, an assigning query (`specify`), and the edge and value fingerprints.
  `const` assertions hold the header at 24 bytes.
- **Deep verify without a claim** (`function/maybe_changed_after.rs`, `fetch.rs`): a memo that
  isn't provisional, took no part in a cycle and has no outputs is verified before any claim is
  taken: its edges are checked in order, each dependency by the same claim-free check, which
  only reads (and stores `verified_at` once a memo is found unchanged, which is idempotent). If
  every edge is unchanged, the memo is marked verified with no claim. Only a memo that must run
  takes a claim, and the claimed path is the existing one, so cycle handling, backdating and
  provisional memos are untouched. rustc's `try_mark_green` works this way. The recursion is
  bounded (256 levels); deeper chains continue under claims.

### Kept

Every public API `crates/` calls (`Storage`, `Database`, `#[salsa::tracked]`, `evict_values`,
`evict_values_of`, `trigger_lru_eviction`, `set_lru_capacity`, `memory_usage`), durability and
backdating (#350: a fingerprint backdates only when durability didn't drop), cycle handling,
eviction keeping edges and fingerprints, and determinism: node numbers depend on scheduling, but
nothing observable does (edges are compared by key, compaction copies in slot order, and no node
number is ever persisted or hashed).

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

## What it would take to meet the memo budget (not filed: a redesign of the memo layer)

- **Memos as columns per function ingredient**, rustc's `DepNodeIndex` model: each memo gets a
  `u32` node index at creation. Columns indexed by it hold the key, `verified_at` (4 bytes), the
  changed-at revision and durability (4 + 1), the fingerprint (16, only for memos that have
  one, in a side column), the value's offset into a per-ingredient value arena (4), and the
  edge range (4 + 4). That gives 24 bytes hot with the fingerprint cold. The slot's `MemoTable`
  holds a `u32` node index per function (4 bytes instead of an 8-byte pointer to a box).
  Replacing a memo appends a new node; old nodes are reclaimed at the revision boundary, which
  already frees `deleted_entries`.
- **Edges as one CSR array per function ingredient**, each a `u32` node index into a global
  node column that maps node to `DatabaseKeyIndex`. Input fields, interned and tracked struct
  reads need node indices too: one column per ingredient beside its slots. The task's edges stay
  in its own frame and are appended to the ingredient's array when the memo is published.
  That is one append per memo instead of one allocation (concurrency.md: per-worker segments,
  since many tasks publish into one ingredient).
- **Deep verify without a claim** while it only reads: verifying is idempotent (it stores
  `verified_at` and marks outputs validated, both idempotent), so it can run unclaimed and
  take the claim only when an input changed and the memo must run. rustc's `try_mark_green`
  works this way.

Together these touch every ingredient kind, persistence, cycle handling and the macros. That is
the db lane's "remade from vendor/salsa" work (docs/v2/plan/lanes/db.md, section 7), and
blaze-query already has its own memo layout. It belongs to whoever owns salsa once db-salsa's
`Sync` work lands, since both rewrite `sync.rs`, `zalsa_local.rs` and `function/*`.

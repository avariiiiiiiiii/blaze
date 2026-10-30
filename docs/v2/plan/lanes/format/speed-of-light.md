---
status: plan
summary: lane format's speed of light: the bound each component's work and the hardware set, the arithmetic, and how the design reaches 90% of it.
date: 2026-10-29
---
# Lane format: speed of light

The bar is "Within 10% of the theoretical maximum, everywhere" in
[principles.md](../../goals/principles.md). Each component of lane format has a section here,
written by the agent that owns it. Unless a section says otherwise, figures are for one core of
the target machines (EPYC Turin, Graviton 5), from [hardware.md](../../design/hardware.md):
- about 3.5 GHz, so figures in ns use 3.5 GHz;
- an L1 hit in 4 to 5 cycles and L2 in about 14;
- streaming reads from L2 at about 32 bytes a cycle, and from DRAM at about 20 to 30 GB/s;
- about 2 instructions per cycle for branchy pointer code and about 4 for straight-line
  integer code.

None of this has been measured yet: nothing compiles until every lane is done. `proof`'s budgets
measure each bound once it does.

## The HIR expression store: `hir-def` `hir/`, `expr_store/` (speed-hir-store)

Layout and history: [speed-hir-store.md](speed-hir-store.md). Below, \(n\) is the number of HIR
nodes in a body (expressions, patterns, statements and type refs) and \(S\) the store's bytes.

### Lowering a body, `Body::of` (no source map)

Work: walk the body's syntax once and write one row per HIR node.
- **Bytes.** Each HIR node reads about 6 tree elements (4-byte words) and about 6 bytes of text,
  so about 30 bytes, and writes a 5-byte row, about 4 bytes of `extra` and list entries, and 4
  bytes on the list stack: about 45 bytes in all. The anchor's tree was just parsed or is kept
  per thread, so it is in L2. That is about 1.5 cycles a node at L2 bandwidth, and bytes are not
  the bound.
- **Instructions.** The minimum per node is about 5 to dispatch on the syntax kind, about 10 to
  find its children in the flat tree, about 5 to write the row, and about 3 for the list stack.
  Add a name interned for about one node in three (hash about 8 bytes and probe one table:
  about 25, so about 8 a node), and a hygiene lookup in the body's span map, which almost always
  has one context (about 5). That is about 40 instructions, at IPC 2 about **20 cycles, 5.7 ns
  a node**.
- **Fixed cost per body.** One salsa query and one exact allocation per non-empty column at
  `finish` (8 to 20 of them, about 30 ns each): about **1 µs**.
- **The bound** is \(5.7n\) ns + 1 µs per body. A build lowers bodies in parallel, one query
  each, so a crate graph's lowering is bound by total nodes over cores.
- **Where the design is.** The store side is at the bound: rows and CSR lists, per-thread
  pooled scratch with no allocation per node, and no hashing per node. Without a reader of the
  map, no source map is built, so no `AstPtr` and no table push either.
- **What stands in the way, in the walk** (estimated, to be measured first):
  1. Each `ast::*` step goes through a `SyntaxNode` handle. If a handle costs more than about
     10 instructions (a refcount, a parent pointer), it is the largest term. The fix is to
     lower from `(FlatTree, element index)` directly, with typed views over indices.
     Owners: speed-syntax (the views) and this lane (lowering).
  2. `check_cfg` runs for every expression, statement, field and parameter. It must be one
     test (does the node have an attribute child?) when there is no attribute. That is in
     `lower.rs` (speed-hir-store).
  3. Interning a name hashes the token text again. The lexer could carry each identifier's
     hash, or a per-thread front cache could answer repeated names in a body without touching
     the shared table. Owner: the interner (format).

  Once it compiles, the first step is `proof` measuring cycles per HIR node on the corpus.
  Anything above 22 cycles a node is a bug against this bound.

### Reading the store: `store.expr(e)`, `pat(p)`, `type_ref(t)`

- **Bound:** the bytes of the node, a 1-byte kind and a 4-byte data word in two columns, plus
  one `extra` read for a node of several words. That is 2 or 3 loads, L1 or L2 hits for a
  body being inferred: about 5 to 15 cycles.
- **Design:** exactly those loads and a `match` on the kind, so it is at the bound by
  construction. A whole-store scan reads \(5n\) bytes plus `extra`. The id-only scans
  (`expr_ids()`) read nothing, and borrowck's seven scans per body are filed to become one
  (`v2/requests/db.md`).

### `finish`: scratch to the store's columns

- **Bound:** write \(S\) once. Scratch is hot in L1 and L2, so the reads are nearly free, plus
  the allocations.
- **Design:** one copy per column (`take_exact`), at the bound for \(S\). For a body of a few
  dozen nodes, the 8 to 20 allocations (about 0.5 µs) are more than the copy.
- **What stands in the way:** an allocation per column. The fix is one allocation per store,
  with every column a slice of it, which brings 8 to 20 allocations down to 1. That is the
  next change to `HirColumns` (speed-hir-store, after `speed-hir-store-stmts`, which holds the
  file).

### Encoding and decoding a store (`expr_store/persist.rs`)

- **Encode bound:** write \(S\) once, and hash it if its content id is taken. BLAKE3 costs
  about 0.6 cycles a byte with AVX-512 or NEON over large inputs.
- **Encode design:** each column is written as a `Slice`, which is a copy on little-endian
  targets. It is at the bound.
- **Decode bound:** unsafe.md requires bytes from outside the process to be hash-verified
  (0.6 cycles a byte) and bounds-checked before any view of them. The check (kinds below their
  count, list ranges inside their arrays) is about 1 cycle a 5-byte row, so about 0.2 cycles a
  byte. With views that borrow the buffer, the bound is about **0.8 cycles a byte**.
- **Decode design:** hash, then check in place, then copy each column out once. The copy adds
  about 0.1 cycles a byte plus one allocation per column, which is about 15% over the bound for
  a large store and more for a small one.
- **What stands in the way:** the columns are owned `Box<[T]>`. Columns that are either owned
  or borrowed from the verified, aligned buffer (an `Arc` of the mapped pack) would remove the
  copy. That needs `blaze_format::Slice` to guarantee the alignment of its elements (format),
  and the store's column type to hold either (speed-hir-store). Check also fuses into the hash
  pass, a 16 KiB block at a time, so the bytes are read from memory once.

### Expression scopes (`expr_store/scope.rs`)

- **Bound:** one pass over the expressions: read each kind (5 bytes), write its scope (4
  bytes), and push one entry per binding. About 10 instructions an expression, so 5 cycles.
- **Design:** one walk over the views into per-thread scratch columns, copied out exactly.
  Entries are grouped by scope, with a stable radix sort that is skipped when they are
  already grouped, which is every body without a `const` block inside a pattern. It is at the
  bound, estimated.

### An edit: what runs again

- **Bound:** for an edit inside body \(B\) of file \(F\), reparse what changed (speed-syntax),
  lower \(B\) again, and do \(O(1)\) for every other item of \(F\): compare a hash the parse
  carries.
- **Design:** a body is lowered from its own anchor's tree and is equal after any edit outside
  it, so nothing downstream reruns. But `body_syntax` runs for every body of \(F\), and each
  anchor is hashed whole (`tree_hash`, once per anchor per thread). So an edit costs
  \(O(|F|)\) hashing: \(|F| \times\) cycles a byte.
  - It was two SipHash-1-3 passes, about 3 cycles a byte: 1.2 M cycles, **340 µs**, for a
    400 KB file.
  - Since 2026-10-29 it is one BLAKE3 pass (`body.rs` `subtree_hash`): about 0.6 cycles a byte
    over a large anchor, so about **70 µs** for the same file. A single-function anchor under
    1 KiB gains little (one chunk, about 1.5 cycles a byte).
  - Signatures hash their trees the same way, filed in `v2/requests/speed-hir.md`.
- **What stands in the way:** every unchanged anchor is still hashed on every edit. The parse
  could carry each node's subtree hash, computed bottom-up while the tree is built and kept for
  unchanged subtrees by an incremental reparse. Then `tree_hash` is one load per anchor, and an
  edit's cost is the reparse plus one body. Owner: speed-syntax (the flat tree), filed in
  `v2/requests/format.md`. Until then an edit is \(O(|F|)\) and over its bound for large files.
- **Source maps** are built only when something reads them. An IDE body read with its map is
  lowered twice, once by `Body::of` and once with the map. That is IDE-only, and within 2× of
  the bound for that body.

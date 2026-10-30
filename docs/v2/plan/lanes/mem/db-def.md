---
status: plan
summary: mem-db-def's plan to move base-db, hir-expand, hir and hir-def (but format's files) onto crates/blaze-mem; the shared test harness, base-db's setters, tables and batch runs, where each other part is filed, and the speed-of-light bounds for base-db's temporaries.
date: 2026-10-29
---
# mem-db-def: base-db, hir-expand, hir and hir-def onto blaze-mem

Part of mem-db's plan (`docs/v2/plan/lanes/mem/db.md`, whose contract every change here
programs against). The paths are lane db's and its children's, not done, so every change is a
request:

| part | paths | request file | notes |
|---|---|---|---|
| `mem-db-def` | shared harness; `crates/base-db/` | `v2/requests/mem-db-def.md` | this file |
| `mem-db-def-collect` | `hir-def/src/nameres.rs`, `nameres/` but `path_resolution.rs` | `v2/requests/mem-db-def-collect.md` | `db-def-collect.md` |
| `mem-db-def-resolve` | `nameres/path_resolution.rs`, `item_scope.rs`, `resolver.rs`, `per_ns.rs`, `visibility.rs`, `find_path.rs`, `dyn_map.rs` | `v2/requests/mem-db-def-resolve.md` | `db-def-resolve.md` |
| `mem-db-def-items` | the rest of `hir-def` | `v2/requests/mem-db-def-items.md` | `db-def-items.md` |
| `mem-db-def-expand` | `crates/hir-expand/` | `v2/requests/mem-db-def-expand.md` | `db-def-expand.md` |
| `mem-db-def-hir` | `crates/hir/` | `v2/requests/mem-db-def-hir.md` | `db-def-hir.md` |

## Rules the parts share

1. **`frame::current` only in a query function's body.** Salsa runs each body inside
   `frame::task(tag, ..)` (`vendor/salsa/src/function/execute.rs:340`, wrapped by
   mem-db-salsa), and base-db's `run_all` runs each batch task inside one too (request B5). Code
   also reached from outside a run (IDE requests through `hir::Semantics`, `Resolver` walks,
   path resolution on a warm map, attribute lookups) keeps its per-call buffers in the worker's
   scratch (`blaze_mem::scratch`, or a `thread_local!` `Scratch<T>`), since `current` stops in no
   task (no fallback). Setters run outside any query and open their own task.
2. **Tags**: base-db's setters `Parse`; hir-expand `Expand`; hir-def's name resolution, scopes,
   signatures, attrs, visibility, `lang_item`, import map and `macro_reach` `Nameres`; batch
   tasks the caller's (`Expand` for an expansion round, `Nameres` for preludes, `Infer` for a
   check); base-db's tables `Query`.
3. **Memoized results and inputs are owned at their exact size** (contract 4): a text, its
   reparse slot, a def map, a memo's boxed slice. Built from frame vectors once at the end,
   never grown by doubling and shrunk.
4. **One counting allocator per test binary** (request H3): hir-def's hand-rolled one in
   `nameres/tests/paths.rs` gives way to `blaze_alloc_count::Counting` in `lib.rs`, so every
   part's budget tests share one.

## base-db

The database's read side was already off the allocator: `Files`' columns and tables are read
with acquire loads and no lock. What was left:

- **The setters' temporaries.** A keystroke (`Files::set_text`) made about fifteen
  allocations of temporaries: the `changed` list, `RootContent::refresh`'s three buffers and two
  result vectors, `hash_ranges`' input list and `ContentId::of_many`'s three buffers, twice. A
  view set that writes nothing still made its base's owned `VfsPath` key and the `built` list.
  After: each setter is a `frame::task(Tag::Parse, ..)`, its temporaries frame vectors and
  slices, the hasher's buffers the worker's (`of_many_into`, of mem-format), the view index found
  by a scan of the few bases. A keystroke allocates its new text and its reparse slot (2), a
  view set that changes nothing allocates nothing, a view edit allocates its text, its slot and
  the returned change list (3).
- **Tables on regions.** `Column` (a salsa input per file ID) becomes an
  `AppendVec<AtomicU64>`: the word's address is the base plus the ID, so the length and the word
  are two independent loads instead of a directory load the word's load waits on. `Log`
  becomes `blaze_mem::Column` (claim with `fetch_add`, publish with a ready bit, the design it
  had in chunks). A `Table`'s shard levels become exact reservations. All tagged `Query`.
- **Batch tasks framed.** `run_all` runs each task in a `frame::task` of the caller's tag:
  `hir`'s check runs non-query work in its tasks, and a task the pump runs on a waiting thread
  must not allocate into the waiter's frame. Its `round`/`again` lists are the worker's.
- **Crate deps memoized.** `transitive_deps` and `transitive_rev_deps` rebuilt their walk
  (and, for reverse deps, the whole inverted graph in a hash map of vectors) on every call from
  queries and IDE requests. They become tracked queries; the reverse walk runs over a CSR of the
  reverse edges in the frame. Reverse deps come out in walk order, deterministic, instead of a
  hash set's.

The executor check (request B8): base-db's query bodies run only through salsa's
`fetch` → `fetch_cold` → `execute` → `execute_query` → `C::execute`, the one call mem-db-salsa
frames; nothing in base-db calls a body another way.

## Speed of light (base-db)

On the M-series and Zen 4 machines blaze targets: a frame allocation is an align, an add and
a compare, about 2 ns (`crates/blaze-mem/src/frame.rs`); mimalloc's small allocation and free
is 15 to 25 ns; BLAKE3 over short inputs batched by `of_many` runs at 1 to 2 GB/s a core
(eight inputs a lane group); `memcpy` runs at 20 GB/s or more from L2.

- **A keystroke** on a 10 KB file in a 1000-file root: the work is hashing the text once
  (10 KB at 1.5 GB/s, about 7 µs), the edit's prefix and suffix compare (32-byte blocks, under
  1 µs), copying the text into its `Arc<str>` (0.5 µs), and hashing one bucket (about 16 files
  of 80 bytes, 1.3 KB, about 1 µs) and the root (2 KB, about 1.5 µs). About 10 µs, all of it
  hashing and copying. Before, the temporaries cost about 20 allocations at 20 ns, 0.4 µs (4%);
  after, about 15 frame bumps at 2 ns, 30 ns (0.3%), plus the 2 owned allocations (40 ns). The
  bound is the bytes hashed at BLAKE3's rate, and the setter is within 1% of it.
- **A view set that changes nothing**, N files: per file a path check, one `IndexMap` lookup
  of the relative path (hash of about 40 bytes plus a compare) and two lookups by file ID,
  three cache lines; for 10 000 files about 30 000 L2/L3 hits at 10 to 30 ns, 0.3 to 0.9 ms.
  The per-call allocations it made were a constant few (under 0.1 µs), so the change makes it
  0 rather than faster; the bound is the three lookups per file, which it is at.
- **A table read** (`Files::file_text`, every query that reads a file): before, a page-pointer
  load and then the word's load, dependent; after, the length and the word are independent
  loads from known addresses, one load's latency on the critical path (about 1 ns from L1).
- **A batch task**: `frame::task` is two thread-local accesses (about 2 ns) against a task that
  runs at least one query (microseconds), under 0.1%.
- **Reverse deps**: before, every call built a hash map of vectors over every edge (for 500
  crates and 5000 edges, about 5000 inserts and 500 allocations, 100 µs or more); after, a hit
  is a memo read (about 20 ns), and a miss is O(V + E) over a CSR in the frame (about 10 µs for
  the same graph).

The state of the art these follow: game engines' per-frame bump allocators (one reset per
task, nothing freed piecemeal), rustc's per-query arenas for its temporaries, and compressed
sparse rows for graph walks.

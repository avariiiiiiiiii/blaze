---
status: plan
summary: lane mem's speed of light: for each allocator in crates/blaze-mem, the least work the operation needs on the hardware, the arithmetic, how the design comes within 10% of it, and the state of the art it is measured against.
date: 2026-10-29
---
# Lane mem: speed of light

The bar is "Within 10% of the theoretical maximum" in `docs/v2/plan/goals/principles.md`. The
machine is a current x86-64 or arm64 core at about 4 GHz: an L1 hit is 4 to 5 cycles, a store
forwarded to a load is about 5, an uncontended atomic read-modify-write on a line the core owns
is about 18 cycles on x86-64 (a `lock` prefix) and 10 to 20 on arm64 with LSE, a system call is
about 1 µs, and the kernel zeroes a fresh page at memory bandwidth on first touch (10 to 20 GB/s
per core). All numbers are to be checked by the coordinator's benchmarks once everything builds.
Each section names the bound, the arithmetic, how the code reaches it, and the fastest known
design it is held against.

## Frame allocation (`Frame::alloc_layout`, `frame::task`)

- **Bound**: a bump allocation has to read the cursor, align it, add the size, compare with the
  end and store the cursor. That is five dependent operations. With the cursor in memory (a
  `Cell` behind `&self`), the critical path between two allocations is the store-forward of the
  cursor plus the add and the mask: about 7 cycles, under 2 ns. A task needs at least its own
  frame state (cursor, end, a link to the enclosing frame) written at start and read back at
  end, and memory that is already warm: about ten stores and loads, a few ns.
- **Design**: exactly that path. The frame is a chain of blocks (2 MB chunks, and for an
  allocation over 1 MB a span: a region of its own reserved four times past it), and the bump's
  end is cached in `end` (the chunk's end, a span's committed end, or the frame's limit), so the
  common case reads no atomic and touches no line another core writes. Moving to the next
  block is a cold call. The peak update (a compare and a store) is off the cursor's dependency
  chain. Freeing or growing the top allocation moves the cursor in place, so a
  `Vec<T, &Frame>` pushed in a loop never copies within a chunk, and in a span never copies at
  all: growth there commits more of the reservation, one system call per step of a quarter of
  what is committed (at least 2 MB), so 0.5 µs per 2 MB or less.
- **A task**: its frame is a header on `frame::task`'s stack. An inner task starts in the rest
  of its enclosing task's chunk (four stores to the outer frame and the inner one's own), so
  nested small tasks take no block at all: 300 nested tasks of 1 KB each use 300 × (1 KB + at
  most 64 bytes of alignment) ≈ 320 KB of one chunk. A task that needs a block takes it from
  the thread's shelf (`SHELF` = 8 chunks, 64 spans; a thread-local array, no atomic) and only
  past that from the process's chunk cache (one compare-and-swap, about 20 cycles). Its end
  puts two thread-local words back and gives its blocks back the same way. Warm, a task makes
  no system call and no global allocation at any depth: about 10 ns plus
  `tag::task_ended`'s two relaxed atomics on the tag's line (shared by every worker; about
  20 cycles each uncontended, more when many workers end tasks of one tag at once), against a
  task of at least microseconds.
- **Measure** (`frame::measure`): counted at task and measure boundaries only (a few loads,
  adds and stores each), never per allocation. An enclosing frame that grows while an inner
  task runs counts on top of the inner peak, so it errs high by at most that growth.
- **The cap** (`frame::Cap`): a thread's first `SHELF` chunks are outside it, so a warm task
  reserves nothing. Past that, a chunk or a span's commit step is one compare-and-swap on the
  cap's total for a thread already holding bytes (about 20 cycles per 2 MB); starting or ending
  a holding, a wait and a give-back while someone waits take its lock.
- **State of the art**: bumpalo (bumps downward to save one instruction; chunk-chained, a new
  chunk from the global allocator when one fills, chunks doubling), rustc's `DroplessArena`
  (the same, per arena rather than per task), LLVM's `BumpPtrAllocator` (slabs from the
  global allocator, one custom-sized slab per big allocation, like a span) and game-engine
  frame allocators (one reserved range per frame, reset per frame). blaze's frame takes the
  chain of the first three without their global allocator (blocks come from a thread's shelf
  and a lock-free chunk cache), the big-allocation slab of LLVM's grown in place by commit,
  and a stack allocator's nesting: an inner task borrows the rest of its outer task's chunk
  and gives it back when it ends, so nesting depth costs neither memory nor system calls.

## Request arena (`ArenaLocal`, `Arena::blocks`, `chunks`)

- **Bound**: the same bump as a frame per allocation, plus whatever sharing costs. Sharing
  needs one atomic per refill at least.
- **Design**: each task bumps in a 64 KB block of its own. A block refill is one compare-and-swap
  on the arena's `current` word, about 20 cycles per 64 KB, so 0.005 cycles a byte. A chunk
  refill (2 MB) is one compare-and-swap on the chunk cache. The largest waste is a block's unused
  tail, at most `SMALL` (8 KB) of 64 KB (allocations bigger than that take blocks of their own).
- **State of the art**: mimalloc's thread-local free-list sharding, which keeps allocation off
  shared lines in the same way; jemalloc's thread caches. Here a request frees all at once by
  pushing its chunks back (one compare-and-swap per 2 MB), where those allocators free per
  object.

## Pools (`Cache::alloc`, `Cache::free`, batches of 64)

- **Bound**: an allocation from a free list is a pop (load the length, decrement, load the
  index) and the write of the value; a free is a push. About 3 cycles plus the value's store.
- **Design**: the cache is an array of 128 indices with no atomic. Moving a batch of 64 to or
  from the shared stacks is one compare-and-swap, 18 cycles for 64 objects, about 0.3 cycles
  each. The tagged head (32-bit tag beside the index) makes the lock-free stack ABA-safe with
  no hazard pointers. A batch descriptor is a 256-byte record that moves whole.
- **State of the art**: tcmalloc's per-CPU caches with transfer caches of batches, and
  mimalloc's free lists per page. A pool matches tcmalloc's batch transfer and has no size class
  lookup (one type per pool).
- **Shared values** (`pool::Shared`): a clone is one relaxed atomic add, about 1 cycle
  uncontended (an owner on another core's line costs the line's transfer, as for `Arc`). The
  last drop is an acquire-release subtract and a slot push. `make_mut` with one owner is one
  acquire load; while shared it is a cache allocation and the value's copy. Against `Arc`, the
  same atomics, but no call into the global allocator for the copy or the free.

## Generational handles (`Slab::get`, `insert`, `remove`)

- **Bound**: one load of the entry for a lookup: bounds compare, generation compare, address.
  The generation and the value share a line for values up to 56 bytes, so a lookup is one cache
  access.
- **Design**: entries are `{generation, next, value}` in a `RegionVec`; insert pops the
  intrusive free list (newest first, warm), remove pushes it. O(1), no allocation once grown.
- **State of the art**: slotmap's `SlotMap` and the `(index, generation)` handles of game
  engines (Bevy's entities). Same layout and cost; blaze's grows in place without a realloc and
  retires a slot whose generation would wrap instead of reusing it.

## Reserved regions (`Region::ensure`, `RegionVec::push`, `AppendVec`, `Column`)

- **Bound for growth**: the kernel has to zero every new page on first touch, at 10 to 20 GB/s.
  Nothing faster is possible for fresh memory. Commit system calls add 1 µs each.
- **Design**: commits are geometric (a quarter of what is committed, at least a granule, at most
  64 MB). Growing to `n` bytes takes `O(log n)` calls before 64 MB and one per 64 MB after:
  1 µs per 64 MB is under 0.1% of the 3 to 6 ms of zeroing that 64 MB costs. Huge pages
  (Linux, `MADV_HUGEPAGE`) cut page faults 512-fold. Growth never copies, where a `Vec`
  doubling copies every byte about once more (a 2x bound on bandwidth).
- **Bound for a push**: one bounds compare and one store. `RegionVec::push` is that.
  `AppendVec::append` adds the writer flag (an uncontended swap and a store, about 20 cycles)
  so that a safe API cannot race, and one release store of the length (a plain store on x86-64,
  `stlr` on arm64).
- **Bound for a read**: one load of the element. `AppendVec` adds one acquire load of the
  length (a plain load on x86-64, `ldar` on arm64), independent of the element's address, so
  the two issue together. `Column::get` loads the ready word and the value in parallel, both
  addresses computed from the index.
- **State of the art**: virtual-memory-reserved arrays as in game engines and in V8's and
  .NET's heaps; `boxcar` and append-only vectors (chunked, with an index computation per read).
  A reserved region needs no chunk index per read.

## Revisions (`Revisions::read`, `publish`)

- **Bound for a read**: one acquire load of a pointer, then the reader's own loads from what it
  points at.
- **Design**: exactly that. The published pointer is to a record `{number, value}` placed in the
  revision's arena, so the number and the value share a line with no second load. No shared
  write on a read: the task guard is the reader's own thread slot (`blaze_reclaim`).
- **Publish**: one arena reset (its chunks back to the cache, one compare-and-swap each), the
  build, two release stores and one `Grace::start` (a fence and a `fetch_add`). It never waits
  on a reader: `PublishError::Readers` instead.
- **State of the art**: RCU and epoch-based reclamation (crossbeam-epoch), whose read side is
  the same one load. Here reclamation is per revision arena, not per object, so a publish frees
  a whole revision with one reset.

## Counters (`tag`)

- **Bound**: counting cannot be free, but it need not be per allocation.
- **Design**: counters move only at coarse grain: a commit, a chunk taken or given back, a
  batch of 64, a task's end. Each tag sits on its own 128-byte line, so subsystems never share a
  counter line. A snapshot is 20 × 5 relaxed loads, read by admission per decision and by the
  agent API, never on an allocation path.

## Scratch buffers (`scratch`)

- **Bound**: lending a warm buffer is a thread-local access, a pop and a `clear` (a store of
  the length).
- **Design**: that, with a shelf of 4 per kind and thread. A `Lent` dropped on another thread
  goes to that thread's shelf, keeping the largest buffers.

## Where the bar is proved

Each allocator has an allocation-budget test in `crates/blaze-mem/src/tests.rs`: a warm frame
task, a request arena, a pool round, a slab round, a limbo round, `RegionVec` and `AppendVec`
pushes, `Column` reads and scratch lends make zero calls to the global allocator. The cycle
counts above are for the coordinator's benchmarks to confirm, with any gap over 10% treated as a
bug.
